//! The push side of the distribution API: blob upload sessions and the
//! manifest commit.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use http::request::Parts;
use http::{HeaderValue, Method, StatusCode};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use tokio::io::AsyncWriteExt as _;

use crate::meta::{self, OCIUploadSession};

use super::oci::{
    OCI_DIGEST_RE, OCI_NAME_RE, OciRequest, oci_blob_path, oci_index_media_type,
    oci_manifest_media_type, oci_manifest_path, query_param, write_oci_error,
};
use super::router::Resolved;
use super::{Manager, header_str, itoa, request_body, username_from_context};

/// Caps a pushed or proxied manifest document. Real manifests are a few KB; the
/// cap keeps a hostile client or upstream from filling the heap, since
/// manifests are buffered whole for digesting and reference verification.
pub(crate) const DEFAULT_OCI_MAX_MANIFEST_BYTES: i64 = 4 << 20;

/// Validates a session id before it is used as a file name, so a crafted id
/// cannot traverse out of the upload directory.
static OCI_SESSION_ID_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[a-f0-9]{32}$").expect("valid regex"));

/// The subset of an OCI content descriptor needed for reference verification.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct OciDescriptor {
    #[serde(default, rename = "mediaType")]
    pub(crate) media_type: String,
    #[serde(default)]
    pub(crate) digest: String,
    #[serde(default)]
    pub(crate) size: i64,
    /// Set on index children; the console's image view surfaces it.
    #[serde(default)]
    pub(crate) platform: Option<OciPlatform>,
}

/// The descriptor platform selector (index children).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct OciPlatform {
    #[serde(default)]
    pub(crate) os: String,
    #[serde(default)]
    pub(crate) architecture: String,
}

/// The subset of a manifest or index document needed to verify references and
/// classify the document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct OciManifestDoc {
    #[serde(default, rename = "mediaType")]
    pub(crate) media_type: String,
    #[serde(default)]
    pub(crate) config: Option<OciDescriptor>,
    #[serde(default)]
    pub(crate) layers: Vec<OciDescriptor>,
    #[serde(default)]
    pub(crate) manifests: Vec<OciDescriptor>,
    /// Links this manifest to another one (OCI 1.1 referrers): SBOMs and
    /// signatures push with subject set and are discovered via end-12.
    #[serde(default)]
    pub(crate) subject: Option<OciDescriptor>,
    #[serde(default, rename = "artifactType")]
    pub(crate) artifact_type: String,
    #[serde(default)]
    pub(crate) annotations: std::collections::BTreeMap<String, String>,
}

impl Manager {
    /// Sets the directory where in-progress OCI blob upload sessions accumulate
    /// their bytes. On an RWX volume or with per-pod storage plus sticky routing
    /// this lets the requests of one push land on different replicas. Push
    /// endpoints reject uploads until it is set.
    pub fn oci_upload_dir_value(&self) -> String {
        self.oci_upload_dir.read().clone()
    }

    pub(crate) fn oci_manifest_cap(&self) -> i64 {
        let n = self.oci_max_manifest_bytes.load(Ordering::Relaxed);
        if n > 0 {
            n
        } else {
            DEFAULT_OCI_MAX_MANIFEST_BYTES
        }
    }

    fn oci_max_blob_bytes_value(&self) -> i64 {
        self.oci_max_blob_bytes.load(Ordering::Relaxed)
    }

    /// Dispatches the blob upload session endpoints:
    ///
    /// ```text
    /// POST   …/blobs/uploads/             open a session (or monolithic ?digest=, or cross-repo ?mount=&from=)
    /// PATCH  …/blobs/uploads/{id}         append a chunk
    /// PUT    …/blobs/uploads/{id}?digest= finalize
    /// GET    …/blobs/uploads/{id}         progress
    /// DELETE …/blobs/uploads/{id}         cancel
    /// ```
    pub(crate) async fn oci_uploads(
        self: &Arc<Self>,
        parts: &Arc<Parts>,
        res: &Resolved,
        req: &OciRequest,
        body: Body,
    ) -> Response {
        if res.repo.r#type != meta::TYPE_HOSTED {
            return write_oci_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "UNSUPPORTED",
                "pushes are only allowed on hosted repositories",
            );
        }
        let empty_ref = req.reference.is_empty();
        match (&parts.method, empty_ref) {
            (&Method::POST, true) => self.oci_open_upload(parts, res, req, body).await,
            (&Method::PATCH, false) => self.oci_patch_upload(parts, res, req, body).await,
            (&Method::PUT, false) => self.oci_finalize_upload(parts, res, req, body).await,
            (&Method::GET, false) => self.oci_upload_status(res, req).await,
            (&Method::DELETE, false) => self.oci_cancel_upload(res, req).await,
            _ => write_oci_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "UNSUPPORTED",
                "method not allowed",
            ),
        }
    }

    /// Handles POST `…/blobs/uploads/`: a monolithic push when `?digest=` is
    /// present, a cross-repository mount when `?mount=&from=` is present, and a
    /// new chunked session otherwise.
    async fn oci_open_upload(
        self: &Arc<Self>,
        parts: &Arc<Parts>,
        res: &Resolved,
        req: &OciRequest,
        body: Body,
    ) -> Response {
        let (mount, from) = (query_param(parts, "mount"), query_param(parts, "from"));
        if !mount.is_empty() && !from.is_empty() {
            // Cross-repository mount (end-11): re-reference an already-stored
            // blob from another image name in the same Forklift repository, so
            // shared layers are never re-uploaded. A failed mount falls through
            // to a new session, per spec.
            if OCI_DIGEST_RE.is_match(&mount)
                && OCI_NAME_RE.is_match(&from)
                && let Ok(src) = self
                    .store
                    .get_artifact(res.repo.id, &oci_blob_path(&from, &mount))
                    .await
                && self
                    .engine
                    .record_upload(
                        &res.repo,
                        &oci_blob_path(&req.name, &mount),
                        "",
                        &src.content_type,
                        &src.blob_sha256,
                        src.size,
                        &username_from_context(parts),
                    )
                    .await
                    .is_ok()
            {
                let mut resp = StatusCode::CREATED.into_response();
                set_location(
                    &mut resp,
                    &oci_blob_url(&res.repo.name, &req.name, &mount),
                    &mount,
                );
                return resp;
            }
        }

        let digest = query_param(parts, "digest");
        if !digest.is_empty() {
            // Monolithic push: the whole blob in the POST body, no session
            // state.
            if !OCI_DIGEST_RE.is_match(&digest) {
                return write_oci_error(
                    StatusCode::BAD_REQUEST,
                    "DIGEST_INVALID",
                    "invalid digest",
                );
            }
            return self
                .oci_store_blob(parts, res, req, &digest, request_body(body))
                .await;
        }

        let dir = self.oci_upload_dir_value();
        if dir.is_empty() {
            return write_oci_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BLOB_UPLOAD_INVALID",
                "upload directory not configured",
            );
        }
        if tokio::fs::create_dir_all(&dir).await.is_err() {
            return write_oci_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BLOB_UPLOAD_INVALID",
                "upload directory unavailable",
            );
        }
        let mut raw = [0u8; 16];
        rand::fill(&mut raw);
        let id = hex::encode(raw);
        if tokio::fs::write(self.oci_session_file(&id), b"")
            .await
            .is_err()
        {
            return write_oci_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BLOB_UPLOAD_INVALID",
                "session file create failed",
            );
        }
        if self
            .store
            .create_oci_upload_session(&id, res.repo.id, &req.name)
            .await
            .is_err()
        {
            let _ = tokio::fs::remove_file(self.oci_session_file(&id)).await;
            return write_oci_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BLOB_UPLOAD_INVALID",
                "session create failed",
            );
        }
        let mut resp = StatusCode::ACCEPTED.into_response();
        let headers = resp.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&oci_upload_url(&res.repo.name, &req.name, &id)) {
            headers.insert(http::header::LOCATION, v);
        }
        if let Ok(v) = HeaderValue::from_str(&id) {
            headers.insert(http::HeaderName::from_static("docker-upload-uuid"), v);
        }
        headers.insert(http::header::RANGE, HeaderValue::from_static("0-0"));
        resp
    }

    fn oci_session_file(&self, id: &str) -> PathBuf {
        PathBuf::from(self.oci_upload_dir_value()).join(id)
    }

    /// Validates and loads the session addressed by the request, returning the
    /// error response on failure.
    // Boxing it would push an extra indirection through every push endpoint for no benefit on a
    // path that allocates a response body anyway.
    #[allow(clippy::result_large_err)]
    async fn oci_load_session(
        &self,
        res: &Resolved,
        req: &OciRequest,
    ) -> Result<OCIUploadSession, Response> {
        let unknown = || {
            Err(write_oci_error(
                StatusCode::NOT_FOUND,
                "BLOB_UPLOAD_UNKNOWN",
                "upload session unknown",
            ))
        };
        if !OCI_SESSION_ID_RE.is_match(&req.reference) || self.oci_upload_dir_value().is_empty() {
            return unknown();
        }
        match self.store.get_oci_upload_session(&req.reference).await {
            Ok(sess) if sess.repo_id == res.repo.id && sess.name == req.name => Ok(sess),
            _ => unknown(),
        }
    }

    /// Appends a chunk. A `Content-Range` whose start does not match the
    /// recorded offset is a 416 per spec, which is how a client recovers from a
    /// lost chunk. The offset row update is guarded by the previous value so two
    /// concurrent appends cannot interleave silently.
    async fn oci_patch_upload(
        self: &Arc<Self>,
        parts: &Arc<Parts>,
        res: &Resolved,
        req: &OciRequest,
        body: Body,
    ) -> Response {
        let sess = match self.oci_load_session(res, req).await {
            Ok(sess) => sess,
            Err(resp) => return resp,
        };
        let content_range = header_str(&parts.headers, "Content-Range").to_string();
        if !content_range.is_empty() {
            let start = content_range
                .split_once('-')
                .map(|(s, _)| s.trim().parse::<i64>().ok())
                .unwrap_or(None);
            if start != Some(sess.offset) {
                let mut resp = write_oci_error(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "BLOB_UPLOAD_INVALID",
                    "chunk out of order",
                );
                if let Ok(v) = HeaderValue::from_str(&oci_range_header(sess.offset)) {
                    resp.headers_mut().insert(http::header::RANGE, v);
                }
                return resp;
            }
        }
        let n = match self.oci_append_session(&sess, body).await {
            Ok(n) => n,
            Err(_) => {
                return write_oci_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "BLOB_UPLOAD_INVALID",
                    "append failed",
                );
            }
        };
        if self
            .store
            .set_oci_upload_session_offset(&sess.id, sess.offset, sess.offset + n)
            .await
            .is_err()
        {
            // Another replica appended concurrently; the client must re-check
            // Range.
            return write_oci_error(
                StatusCode::CONFLICT,
                "BLOB_UPLOAD_INVALID",
                "concurrent append",
            );
        }
        let mut resp = StatusCode::ACCEPTED.into_response();
        let headers = resp.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&oci_upload_url(&res.repo.name, &req.name, &sess.id)) {
            headers.insert(http::header::LOCATION, v);
        }
        if let Ok(v) = HeaderValue::from_str(&sess.id) {
            headers.insert(http::HeaderName::from_static("docker-upload-uuid"), v);
        }
        if let Ok(v) = HeaderValue::from_str(&oci_range_header(sess.offset + n)) {
            headers.insert(http::header::RANGE, v);
        }
        resp
    }

    /// Appends the request body to the session file, enforcing the per-blob size
    /// cap across the accumulated total.
    async fn oci_append_session(
        &self,
        sess: &OCIUploadSession,
        body: Body,
    ) -> Result<i64, std::io::Error> {
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(self.oci_session_file(&sess.id))
            .await?;
        let cap = self.oci_max_blob_bytes_value();
        let mut reader = request_body(body);
        if cap > 0 {
            let remaining = cap - sess.offset;
            if remaining <= 0 {
                return Err(std::io::Error::other("blob size cap exceeded"));
            }
            let mut limited = tokio::io::AsyncReadExt::take(reader, remaining as u64);
            let n = tokio::io::copy(&mut limited, &mut file).await? as i64;
            // Anything left in body means the cap was hit mid-chunk.
            let mut reader = limited.into_inner();
            let mut probe = [0u8; 1];
            if tokio::io::AsyncReadExt::read(&mut reader, &mut probe).await? > 0 {
                return Err(std::io::Error::other("blob size cap exceeded"));
            }
            file.flush().await?;
            return Ok(n);
        }
        let n = tokio::io::copy(&mut reader, &mut file).await? as i64;
        file.flush().await?;
        Ok(n)
    }

    /// Handles PUT `…/blobs/uploads/{id}?digest=`: append any final body, stream
    /// the assembled file into the blob store, and verify the client's digest
    /// against what was actually stored — never against a digest the client
    /// merely asserts.
    async fn oci_finalize_upload(
        self: &Arc<Self>,
        parts: &Arc<Parts>,
        res: &Resolved,
        req: &OciRequest,
        body: Body,
    ) -> Response {
        let sess = match self.oci_load_session(res, req).await {
            Ok(sess) => sess,
            Err(resp) => return resp,
        };
        let digest = query_param(parts, "digest");
        if !OCI_DIGEST_RE.is_match(&digest) {
            return write_oci_error(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "invalid digest");
        }
        match self.oci_append_session(&sess, body).await {
            Err(_) => {
                return write_oci_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "BLOB_UPLOAD_INVALID",
                    "append failed",
                );
            }
            Ok(n) if n > 0 => {
                if self
                    .store
                    .set_oci_upload_session_offset(&sess.id, sess.offset, sess.offset + n)
                    .await
                    .is_err()
                {
                    return write_oci_error(
                        StatusCode::CONFLICT,
                        "BLOB_UPLOAD_INVALID",
                        "concurrent append",
                    );
                }
            }
            Ok(_) => {}
        }
        let file = match tokio::fs::File::open(self.oci_session_file(&sess.id)).await {
            Ok(file) => file,
            Err(_) => {
                return write_oci_error(
                    StatusCode::NOT_FOUND,
                    "BLOB_UPLOAD_UNKNOWN",
                    "upload session unknown",
                );
            }
        };
        let resp = self
            .oci_store_blob(parts, res, req, &digest, Box::pin(file))
            .await;
        // Session cleanup regardless of outcome: a digest mismatch discards the
        // upload (the client restarts the push), it does not resume.
        let _ = self.store.delete_oci_upload_session(&sess.id).await;
        let _ = tokio::fs::remove_file(self.oci_session_file(&sess.id)).await;
        resp
    }

    /// Streams `body` into the blob store, verifies the stored digest against
    /// the client's, and records the artifact row on match. The GC read lock is
    /// held from the byte write through the reference insert (see
    /// `Engine::gc_mu`); a mismatched upload is handed to the sweeper.
    async fn oci_store_blob(
        self: &Arc<Self>,
        parts: &Arc<Parts>,
        res: &Resolved,
        req: &OciRequest,
        digest: &str,
        body: super::StoreBody,
    ) -> Response {
        let cap = self.oci_max_blob_bytes_value();
        let body: super::StoreBody = if cap > 0 {
            Box::pin(tokio::io::AsyncReadExt::take(body, (cap + 1) as u64))
        } else {
            body
        };
        let _gc = self.engine.gc_mu.read().await;
        let (stored, size) = match self.engine.blobs.put(body).await {
            Ok(v) => v,
            Err(_) => {
                return write_oci_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "BLOB_UPLOAD_INVALID",
                    "store failed",
                );
            }
        };
        if cap > 0 && size > cap {
            self.engine.abandon_blob(&stored, size).await;
            return write_oci_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "SIZE_INVALID",
                "blob exceeds the configured size cap",
            );
        }
        if format!("sha256:{stored}") != digest {
            self.engine.abandon_blob(&stored, size).await;
            return write_oci_error(
                StatusCode::BAD_REQUEST,
                "DIGEST_INVALID",
                "digest does not match uploaded content",
            );
        }
        if let Err(err) = self
            .engine
            .record_upload(
                &res.repo,
                &oci_blob_path(&req.name, digest),
                "",
                "application/octet-stream",
                &stored,
                size,
                &username_from_context(parts),
            )
            .await
        {
            tracing::error!(
                repo = %res.repo.name, name = %req.name, digest, err = %err,
                "oci blob record failed"
            );
            return write_oci_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BLOB_UPLOAD_INVALID",
                "record failed",
            );
        }
        let mut resp = StatusCode::CREATED.into_response();
        set_location(
            &mut resp,
            &oci_blob_url(&res.repo.name, &req.name, digest),
            digest,
        );
        resp
    }

    /// Reports session progress (end-13 GET form).
    async fn oci_upload_status(&self, res: &Resolved, req: &OciRequest) -> Response {
        let sess = match self.oci_load_session(res, req).await {
            Ok(sess) => sess,
            Err(resp) => return resp,
        };
        let mut resp = StatusCode::NO_CONTENT.into_response();
        let headers = resp.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&oci_upload_url(&res.repo.name, &req.name, &sess.id)) {
            headers.insert(http::header::LOCATION, v);
        }
        if let Ok(v) = HeaderValue::from_str(&sess.id) {
            headers.insert(http::HeaderName::from_static("docker-upload-uuid"), v);
        }
        if let Ok(v) = HeaderValue::from_str(&oci_range_header(sess.offset)) {
            headers.insert(http::header::RANGE, v);
        }
        resp
    }

    /// Discards a session and its bytes.
    async fn oci_cancel_upload(&self, res: &Resolved, req: &OciRequest) -> Response {
        let sess = match self.oci_load_session(res, req).await {
            Ok(sess) => sess,
            Err(resp) => return resp,
        };
        let _ = self.store.delete_oci_upload_session(&sess.id).await;
        let _ = tokio::fs::remove_file(self.oci_session_file(&sess.id)).await;
        StatusCode::NO_CONTENT.into_response()
    }

    /// The commit point of a push. The manifest is verified, not merely stored:
    /// every blob (or child manifest, for an index) it references must already
    /// exist for this (repo, name), which is what makes "the tag exists" imply
    /// "the image is pullable". The manifest artifact and the tag row are
    /// written manifest-first, so a failure in between leaves an untagged
    /// manifest for the prune pass, never a dangling tag.
    pub(crate) async fn oci_put_manifest(
        self: &Arc<Self>,
        parts: &Arc<Parts>,
        res: &Resolved,
        req: &OciRequest,
        is_digest: bool,
        body: Body,
    ) -> Response {
        let cap = self.oci_manifest_cap();
        let Ok(body) = axum::body::to_bytes(body, (cap + 1) as usize).await else {
            return write_oci_error(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", "read failed");
        };
        if body.len() as i64 > cap {
            return write_oci_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "MANIFEST_INVALID",
                "manifest exceeds the size cap",
            );
        }

        let mut media_type = header_str(&parts.headers, "Content-Type").to_string();
        let Ok(doc) = serde_json::from_slice::<OciManifestDoc>(&body) else {
            return write_oci_error(
                StatusCode::BAD_REQUEST,
                "MANIFEST_INVALID",
                "invalid manifest JSON",
            );
        };
        if media_type.is_empty() || media_type == "application/json" {
            media_type = doc.media_type.clone();
        }
        if !oci_manifest_media_type(&media_type) {
            return write_oci_error(
                StatusCode::BAD_REQUEST,
                "MANIFEST_INVALID",
                "unsupported manifest media type",
            );
        }

        // Reference verification.
        if oci_index_media_type(&media_type) {
            for child in &doc.manifests {
                if !OCI_DIGEST_RE.is_match(&child.digest) {
                    return write_oci_error(
                        StatusCode::BAD_REQUEST,
                        "MANIFEST_INVALID",
                        "invalid child manifest digest",
                    );
                }
                if self
                    .store
                    .get_artifact(res.repo.id, &oci_manifest_path(&req.name, &child.digest))
                    .await
                    .is_err()
                {
                    return write_oci_error(
                        StatusCode::BAD_REQUEST,
                        "MANIFEST_BLOB_UNKNOWN",
                        &format!("referenced manifest not present: {}", child.digest),
                    );
                }
            }
        } else {
            let mut refs: Vec<&OciDescriptor> = Vec::with_capacity(doc.layers.len() + 1);
            if let Some(config) = &doc.config {
                refs.push(config);
            }
            refs.extend(doc.layers.iter());
            for reference in refs {
                if !OCI_DIGEST_RE.is_match(&reference.digest) {
                    return write_oci_error(
                        StatusCode::BAD_REQUEST,
                        "MANIFEST_INVALID",
                        "invalid descriptor digest",
                    );
                }
                if self
                    .store
                    .get_artifact(res.repo.id, &oci_blob_path(&req.name, &reference.digest))
                    .await
                    .is_err()
                {
                    return write_oci_error(
                        StatusCode::BAD_REQUEST,
                        "MANIFEST_BLOB_UNKNOWN",
                        &format!("referenced blob not present: {}", reference.digest),
                    );
                }
            }
        }

        let digest = format!("sha256:{}", hex::encode(sha2::Sha256::digest(&body)));
        if is_digest && digest != req.reference {
            return write_oci_error(
                StatusCode::BAD_REQUEST,
                "DIGEST_INVALID",
                "manifest digest does not match reference",
            );
        }

        let version = if is_digest { "" } else { &req.reference };
        if self
            .engine
            .put(
                &res.repo,
                &oci_manifest_path(&req.name, &digest),
                version,
                &media_type,
                None,
                Box::pin(std::io::Cursor::new(body.to_vec())),
                &username_from_context(parts),
            )
            .await
            .is_err()
        {
            return write_oci_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "MANIFEST_INVALID",
                "store failed",
            );
        }
        if !is_digest
            && self
                .store
                .upsert_oci_tag(res.repo.id, &req.name, &req.reference, &digest)
                .await
                .is_err()
        {
            return write_oci_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "MANIFEST_INVALID",
                "tag write failed",
            );
        }
        let mut resp = StatusCode::CREATED.into_response();
        set_location(
            &mut resp,
            &oci_manifest_url(&res.repo.name, &req.name, &digest),
            &digest,
        );
        // A manifest pushed with a subject participates in the referrers graph;
        // the spec requires acknowledging that with the OCI-Subject header.
        if let Some(subject) = &doc.subject
            && OCI_DIGEST_RE.is_match(&subject.digest)
            && let Ok(v) = HeaderValue::from_str(&subject.digest)
        {
            resp.headers_mut()
                .insert(http::HeaderName::from_static("oci-subject"), v);
        }
        resp
    }

    /// Implements the spec's split semantics: deleting by tag removes only the
    /// tag row; deleting by digest removes every tag pointing at the manifest
    /// and the manifest row itself. Blob rows are never deleted here — the prune
    /// pass collects what becomes unreachable.
    pub(crate) async fn oci_delete_manifest(
        &self,
        res: &Resolved,
        req: &OciRequest,
        is_digest: bool,
    ) -> Response {
        if !is_digest {
            if self
                .store
                .delete_oci_tag(res.repo.id, &req.name, &req.reference)
                .await
                .is_err()
            {
                return write_oci_error(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "tag unknown");
            }
            return StatusCode::ACCEPTED.into_response();
        }
        if self
            .store
            .delete_oci_tags_by_digest(res.repo.id, &req.name, &req.reference)
            .await
            .is_err()
        {
            return write_oci_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "UNSUPPORTED",
                "metadata error",
            );
        }
        if self
            .store
            .delete_artifact(res.repo.id, &oci_manifest_path(&req.name, &req.reference))
            .await
            .is_err()
        {
            return write_oci_error(
                StatusCode::NOT_FOUND,
                "MANIFEST_UNKNOWN",
                "manifest unknown",
            );
        }
        StatusCode::ACCEPTED.into_response()
    }
}

fn oci_range_header(offset: i64) -> String {
    if offset <= 0 {
        "0-0".to_string()
    } else {
        format!("0-{}", itoa(offset - 1))
    }
}

/// Sets the `Location` and `Docker-Content-Digest` headers both blob and
/// manifest commits answer with.
fn set_location(resp: &mut Response, location: &str, digest: &str) {
    let headers = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(location) {
        headers.insert(http::header::LOCATION, v);
    }
    if let Ok(v) = HeaderValue::from_str(digest) {
        headers.insert(http::HeaderName::from_static("docker-content-digest"), v);
    }
}

// URL builders for Location headers. Relative URLs are valid here per spec and
// sidestep external-URL derivation entirely.
pub(crate) fn oci_blob_url(repo: &str, name: &str, digest: &str) -> String {
    format!("/v2/{repo}/{name}/blobs/{digest}")
}

pub(crate) fn oci_manifest_url(repo: &str, name: &str, digest: &str) -> String {
    format!("/v2/{repo}/{name}/manifests/{digest}")
}

pub(crate) fn oci_upload_url(repo: &str, name: &str, id: &str) -> String {
    format!("/v2/{repo}/{name}/blobs/uploads/{id}")
}
