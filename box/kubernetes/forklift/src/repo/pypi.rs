//! Serves the PyPI simple repository protocol (PEP 503/691/700). Paths under
//! `/pypi/{repo}/`:
//!
//! ```text
//! simple/<project>/          version index (metadata)
//! packages/<ref>/<filename>  distribution file (artifact)
//! POST to the repo root      twine legacy upload (local repos)
//! ```
//!
//! For proxy repos the simple index is fetched from upstream as PEP 691 JSON,
//! files newer than the age-policy cooldown are removed (PEP 700 upload-time),
//! and file URLs are rewritten to point back at forklift; the original upstream
//! URL travels base64url-encoded in `<ref>` because PyPI serves files from a
//! different host than the index.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{FromRequest, Multipart, Request};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use http::header::{CONTENT_TYPE, VARY};
use http::request::Parts;
use http::{HeaderValue, Method, StatusCode};
use serde_json::value::RawValue;
use tokio::io::AsyncReadExt;
use url::Url;

use crate::meta::{self, Artifact};
use crate::repoconfig::{ACTION_BLOCK, AgePolicyConfig};
use crate::server::http_error;

use super::maven::last_modified;
use super::router::{Resolved, action_for_method, join_upstream};
use super::{
    FetchSpec, Kind, MAX_METADATA_BYTES, Manager, not_found, path_base, username_from_context,
};

/// The PEP 691 simple-index media type.
pub(crate) const PYPI_JSON_TYPE: &str = "application/vnd.pypi.simple.v1+json";

/// Caps a single uploaded distribution file (parity with the previous in-memory
/// multipart limit).
const MAX_PYPI_UPLOAD_BYTES: i64 = 256 << 20;

/// Serves the PyPI simple repository protocol.
pub(crate) async fn handle_pypi(m: Arc<Manager>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let res = match m.resolve_repo(&parts, meta::FORMAT_PYPI).await {
        Ok(res) => res,
        Err(resp) => return resp,
    };
    if let Err(resp) = m.authorize(
        &parts,
        &res.repo.name,
        action_for_method(&parts.method),
        res.cfg.public,
    ) {
        return resp;
    }

    if res.path.is_empty() {
        if parts.method != Method::POST {
            return http_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
        if res.repo.r#type != meta::TYPE_HOSTED {
            return http_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "uploads are only allowed on local repositories",
            );
        }
        return m.pypi_upload(Arc::new(parts), res, body).await;
    }
    if res.path.starts_with("simple/") {
        if parts.method != Method::GET && parts.method != Method::HEAD {
            return http_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
        return m.pypi_simple(Arc::new(parts), res).await;
    }
    if res.path.starts_with("packages/") {
        if parts.method != Method::GET && parts.method != Method::HEAD {
            return http_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
        return m.pypi_file(Arc::new(parts), res).await;
    }
    not_found()
}

impl Manager {
    /// Serves a project's version index.
    async fn pypi_simple(self: &Arc<Self>, parts: Arc<Parts>, res: Resolved) -> Response {
        let project = normalize_pypi(
            res.path
                .strip_prefix("simple/")
                .unwrap_or(&res.path)
                .trim_matches('/'),
        );
        if project.is_empty() {
            if res.repo.r#type == meta::TYPE_HOSTED {
                return self.pypi_local_root_simple(&parts, &res).await;
            }
            return not_found();
        }
        if let Some(resp) = self
            .policy_gates(Arc::clone(&parts), Arc::new(res.clone()), &project, "")
            .await
        {
            return resp;
        }
        if res.repo.r#type == meta::TYPE_HOSTED {
            return self.pypi_local_simple(&parts, &res, &project).await;
        }

        let e = Arc::clone(&self.engine);
        let key = format!("simple/{project}");

        match e.store.get_artifact(res.repo.id, &key).await {
            Ok(art) if e.fresh(&art, &res.cfg, Kind::Metadata) => {
                // HEAD needs no body, so skip the decode/rewrite work entirely.
                if parts.method == Method::HEAD {
                    let gate = self.final_policy_gate(res.clone(), &project, "");
                    if let Some(resp) = gate(Arc::clone(&parts)).await {
                        return resp;
                    }
                    e.touch(&art, &username_from_context(&parts)).await;
                    return write_simple(&parts, &Bytes::new(), &project);
                }
                if let Some(resp) = self
                    .pypi_serve_cached_simple(&parts, &res, &project, &art)
                    .await
                {
                    return resp;
                }
            }
            Ok(_) | Err(meta::Error::NotFound) => {}
            Err(_) => return http_error(StatusCode::INTERNAL_SERVER_ERROR, "metadata error"),
        }

        let neg_key = format!("{}/{}", res.repo.name, key);
        if e.neg.has(&neg_key) {
            return not_found();
        }
        e.cache_miss.with_label_values(&[&res.repo.name]).inc();

        let spec = FetchSpec {
            repo: res.repo.clone(),
            cfg: res.cfg.clone(),
            path: key.clone(),
            upstream_url: format!("{}/", join_upstream(&res.repo.upstream_url, &project)),
            kind: Kind::Metadata,
            accept: PYPI_JSON_TYPE.to_string(),
            ..FetchSpec::blank()
        };
        let resp = match e.upstream_get(&spec).await {
            Ok(resp) => resp,
            Err(_) => {
                e.upstream_err.with_label_values(&[&res.repo.name]).inc();
                return http_error(StatusCode::BAD_GATEWAY, "upstream unreachable");
            }
        };
        if resp.status() == StatusCode::NOT_FOUND {
            e.neg.set(&neg_key, res.cfg.cache.negative_ttl.d());
            return not_found();
        }
        if !resp.status().is_success() {
            e.upstream_err.with_label_values(&[&res.repo.name]).inc();
            return http_error(StatusCode::BAD_GATEWAY, "upstream error");
        }

        // A cold-cache install burst is all misses, so this is the path that
        // historically OOMs the pod. When caching is on, stream the upstream
        // body into a blob and rewrite off its reader, so peak memory is the
        // parsed fragments alone — never the raw index plus the parsed document
        // held at once. The rewrite gate bounds how many decodes run at once,
        // and is taken only around the decode itself: a slot held across the
        // upstream stream would stall every other index, cache hits included.
        let reader = tokio_util::io::StreamReader::new(futures_util::TryStreamExt::map_err(
            resp.bytes_stream(),
            std::io::Error::other,
        ));
        if !res.cfg.cache.enabled {
            // No blob to decode off; buffer once (the gate bounds how many
            // exist).
            let mut body = Vec::new();
            if tokio::io::AsyncReadExt::take(reader, MAX_METADATA_BYTES as u64)
                .read_to_end(&mut body)
                .await
                .is_err()
            {
                return http_error(StatusCode::BAD_GATEWAY, "read upstream");
            }
            let Some(slot) = e.acquire_rewrite(&res.repo.name).await else {
                return http_error(StatusCode::SERVICE_UNAVAILABLE, "shutting down");
            };
            let rewritten = rewrite_simple_index(
                &body,
                &self.external_base(&parts),
                &res.repo.name,
                &res.cfg.age_policy,
                e.now(),
            );
            drop(slot);
            let Some((out, removed)) = rewritten else {
                return http_error(
                    StatusCode::BAD_GATEWAY,
                    "upstream simple index is not PEP 691 JSON",
                );
            };
            self.report_simple_age_blocks(&res, &project, removed);
            let gate = self.final_policy_gate(res.clone(), &project, "");
            if let Some(resp) = gate(Arc::clone(&parts)).await {
                return resp;
            }
            return write_simple(&parts, &out, &project);
        }

        // Stream the body into a content-addressed blob first, then decode the
        // cached copy off its reader. The artifact is recorded only after it
        // parses as PEP 691 JSON: a malformed upstream response is a 502 and
        // must not poison the cache, so an unparseable blob is abandoned for the
        // sweeper to reclaim. Hold the GC read lock from the byte write through
        // the reference insert (and the intervening open of the cached copy) so
        // the sweeper cannot reclaim these bytes mid-flight (see
        // `Engine::gc_mu`).
        let _gc = e.gc_mu.read().await;
        let limited = tokio::io::AsyncReadExt::take(reader, MAX_METADATA_BYTES as u64);
        let (digest, size) = match e.blobs.put(Box::pin(limited)).await {
            Ok(v) => v,
            Err(_) => return http_error(StatusCode::INTERNAL_SERVER_ERROR, "cache write failed"),
        };
        // The bytes are written but not yet referenced. Giving up here (the
        // client is gone) leaves them for the sweeper to reclaim, which is why
        // the wait happens after the stream rather than before it.
        let Some(slot) = e.acquire_rewrite(&res.repo.name).await else {
            return http_error(StatusCode::SERVICE_UNAVAILABLE, "shutting down");
        };
        let Ok((rc, _)) = e.blobs.open(&digest).await else {
            drop(slot);
            e.abandon_blob(&digest, size).await;
            return http_error(StatusCode::INTERNAL_SERVER_ERROR, "cache read failed");
        };
        let top = super::npm::decode_json_stream::<RawObject, _>(rc, 64 << 20).await;
        let rewritten = top.and_then(|top| {
            rewrite_simple_index_top(
                top,
                &self.external_base(&parts),
                &res.repo.name,
                &res.cfg.age_policy,
                e.now(),
            )
        });
        drop(slot);
        let Some((out, removed)) = rewritten else {
            e.abandon_blob(&digest, size).await;
            return http_error(
                StatusCode::BAD_GATEWAY,
                "upstream simple index is not PEP 691 JSON",
            );
        };
        let now = e.now();
        if e.store
            .put_artifact(Artifact {
                repo_id: res.repo.id,
                path: key.clone(),
                blob_sha256: digest,
                size,
                content_type: PYPI_JSON_TYPE.to_string(),
                cached_at: now,
                last_accessed_at: now,
                cached_by: username_from_context(&parts),
                ..Default::default()
            })
            .await
            .is_err()
        {
            return http_error(StatusCode::INTERNAL_SERVER_ERROR, "cache write failed");
        }
        self.report_simple_age_blocks(&res, &project, removed);
        let gate = self.final_policy_gate(res.clone(), &project, "");
        if let Some(resp) = gate(Arc::clone(&parts)).await {
            return resp;
        }
        write_simple(&parts, &out, &project)
    }

    /// Records the age-policy quarantine metric and log for a proxy simple-index
    /// fetch when the rewrite removed one or more files.
    fn report_simple_age_blocks(&self, res: &Resolved, project: &str, removed: i64) {
        if removed <= 0 {
            return;
        }
        let e = &self.engine;
        e.age_blocks
            .with_label_values(&[&res.repo.name, &res.cfg.age_policy.action])
            .inc_by(removed as f64);
        tracing::warn!(
            repo = %res.repo.name, project, removed,
            min_age = %humantime::format_duration(res.cfg.age_policy.min_age.d()),
            action = "block",
            "age policy quarantined package files"
        );
    }

    /// Serves a fresh cached simple index. `None` falls the caller through to an
    /// upstream re-fetch. The cache holds the upstream's original index; URLs are
    /// rewritten per request so cached bodies stay host-agnostic (a client cannot
    /// poison the cache for others via Host/X-Forwarded-* headers). Decoding
    /// straight off the blob reader avoids holding the raw JSON and the parsed
    /// document in memory at once, and the rewrite gate bounds how many of these
    /// decodes run concurrently.
    async fn pypi_serve_cached_simple(
        self: &Arc<Self>,
        parts: &Arc<Parts>,
        res: &Resolved,
        project: &str,
        art: &Artifact,
    ) -> Option<Response> {
        let e = &self.engine;
        let Some(slot) = e.acquire_rewrite(&res.repo.name).await else {
            // Client gone; nothing left to serve.
            return Some(http_error(StatusCode::SERVICE_UNAVAILABLE, "shutting down"));
        };
        let Ok((rc, _)) = e.blobs.open(&art.blob_sha256).await else {
            drop(slot);
            return None;
        };
        let top = super::npm::decode_json_stream::<RawObject, _>(rc, 64 << 20).await;
        let rewritten = top.and_then(|top| {
            rewrite_simple_index_top(
                top,
                &self.external_base(parts),
                &res.repo.name,
                &res.cfg.age_policy,
                e.now(),
            )
        });
        drop(slot);
        let (out, _) = rewritten?;
        let gate = self.final_policy_gate(res.clone(), project, "");
        if let Some(resp) = gate(Arc::clone(parts)).await {
            return Some(resp);
        }
        e.touch(art, &username_from_context(parts)).await;
        Some(write_simple(parts, &out, project))
    }

    /// Builds a PEP 691 index for a hosted repository from its stored
    /// distribution files.
    async fn pypi_local_simple(
        self: &Arc<Self>,
        parts: &Arc<Parts>,
        res: &Resolved,
        project: &str,
    ) -> Response {
        let arts = match self
            .engine
            .store
            .list_artifacts(res.repo.id, &format!("packages/{project}/"))
            .await
        {
            Ok(arts) => arts,
            Err(_) => return http_error(StatusCode::INTERNAL_SERVER_ERROR, "metadata error"),
        };
        if arts.is_empty() {
            return not_found();
        }
        let base = self.external_base(parts);
        let mut files = Vec::with_capacity(arts.len());
        let mut seen = std::collections::HashSet::new();
        let mut versions: Vec<String> = Vec::new();
        for a in &arts {
            let name = path_base(&a.path).to_string();
            let mut file = serde_json::Map::new();
            file.insert(
                "filename".to_string(),
                serde_json::Value::String(name.clone()),
            );
            file.insert(
                "url".to_string(),
                serde_json::Value::String(format!(
                    "{base}/pypi/{}/packages/{project}/{}",
                    res.repo.name,
                    percent_encoding::utf8_percent_encode(&name, PATH_SEGMENT),
                )),
            );
            let mut hashes = serde_json::Map::new();
            hashes.insert(
                "sha256".to_string(),
                serde_json::Value::String(a.blob_sha256.clone()),
            );
            file.insert("hashes".to_string(), serde_json::Value::Object(hashes));
            file.insert("size".to_string(), serde_json::Value::from(a.size));
            if let Some(requires_python) = requires_python(&a.metadata_json) {
                file.insert(
                    "requires-python".to_string(),
                    serde_json::Value::String(requires_python),
                );
            }
            if let Some(published) = a.published_at {
                file.insert(
                    "upload-time".to_string(),
                    serde_json::Value::String(format_upload_time(published)),
                );
            }
            files.push(serde_json::Value::Object(file));
            if !a.version.is_empty() && seen.insert(a.version.clone()) {
                versions.push(a.version.clone());
            }
        }
        versions.sort();
        let mut meta_obj = serde_json::Map::new();
        meta_obj.insert(
            "api-version".to_string(),
            serde_json::Value::String("1.1".to_string()),
        );
        let mut doc = serde_json::Map::new();
        doc.insert("files".to_string(), serde_json::Value::Array(files));
        doc.insert("meta".to_string(), serde_json::Value::Object(meta_obj));
        doc.insert(
            "name".to_string(),
            serde_json::Value::String(project.to_string()),
        );
        doc.insert(
            "versions".to_string(),
            serde_json::Value::Array(
                versions
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
        let Ok(out) = serde_json::to_vec(&serde_json::Value::Object(doc)) else {
            return http_error(StatusCode::INTERNAL_SERVER_ERROR, "encode index");
        };
        write_simple(parts, &Bytes::from(out), project)
    }

    async fn pypi_local_root_simple(
        self: &Arc<Self>,
        parts: &Arc<Parts>,
        res: &Resolved,
    ) -> Response {
        let artifacts = match self
            .engine
            .store
            .list_artifacts(res.repo.id, "packages/")
            .await
        {
            Ok(artifacts) => artifacts,
            Err(_) => return http_error(StatusCode::INTERNAL_SERVER_ERROR, "metadata error"),
        };
        let mut seen = std::collections::HashSet::new();
        let mut projects: Vec<String> = Vec::new();
        for artifact in &artifacts {
            let rest = artifact
                .path
                .strip_prefix("packages/")
                .unwrap_or(&artifact.path);
            if let Some((project, _)) = rest.split_once('/')
                && !project.is_empty()
                && seen.insert(project.to_string())
            {
                projects.push(project.to_string());
            }
        }
        projects.sort();

        let wants_json = super::header_str(&parts.headers, "Accept").contains(PYPI_JSON_TYPE);
        let head = parts.method == Method::HEAD;
        let mut resp = if wants_json {
            if head {
                StatusCode::OK.into_response()
            } else {
                let items: Vec<serde_json::Value> = projects
                    .iter()
                    .map(|project| {
                        let mut item = serde_json::Map::new();
                        item.insert(
                            "name".to_string(),
                            serde_json::Value::String(project.clone()),
                        );
                        serde_json::Value::Object(item)
                    })
                    .collect();
                let mut meta_obj = serde_json::Map::new();
                meta_obj.insert(
                    "api-version".to_string(),
                    serde_json::Value::String("1.1".to_string()),
                );
                let mut doc = serde_json::Map::new();
                doc.insert("meta".to_string(), serde_json::Value::Object(meta_obj));
                doc.insert("projects".to_string(), serde_json::Value::Array(items));
                let mut body =
                    serde_json::to_string(&serde_json::Value::Object(doc)).unwrap_or_default();
                // `json.Encoder` appends a newline.
                body.push('\n');
                body.into_response()
            }
        } else if head {
            StatusCode::OK.into_response()
        } else {
            let mut body = String::from(
                "<!DOCTYPE html>\n<html><head><meta name=\"pypi:repository-version\" content=\"1.0\"><title>Simple index</title></head><body>\n",
            );
            for project in &projects {
                let escaped = escape_html(project);
                body.push_str(&format!("<a href=\"{escaped}/\">{escaped}</a><br/>\n"));
            }
            body.push_str("</body></html>\n");
            body.into_response()
        };
        let headers = resp.headers_mut();
        headers.insert(VARY, HeaderValue::from_static("Accept"));
        headers.insert(
            CONTENT_TYPE,
            if wants_json {
                HeaderValue::from_static(PYPI_JSON_TYPE)
            } else {
                HeaderValue::from_static("text/html; charset=utf-8")
            },
        );
        resp
    }

    /// Serves a distribution file. For proxy repos the upstream URL is recovered
    /// from the base64url-encoded `<ref>` path segment written by
    /// [`rewrite_simple_index`].
    async fn pypi_file(self: &Arc<Self>, parts: Arc<Parts>, res: Resolved) -> Response {
        // The .metadata suffix (PEP 658) is stripped so a denied version's core
        // metadata is blocked along with the distribution file itself.
        let filename = path_base(&res.path).to_string();
        let pypi_pkg = pypi_package_from_filename(&filename);
        let pypi_ver = pypi_version(filename.strip_suffix(".metadata").unwrap_or(&filename));
        if let Some(resp) = self
            .policy_gates(
                Arc::clone(&parts),
                Arc::new(res.clone()),
                &pypi_pkg,
                &pypi_ver,
            )
            .await
        {
            return resp;
        }
        let mut spec = FetchSpec {
            repo: res.repo.clone(),
            cfg: res.cfg.clone(),
            path: res.path.clone(),
            kind: Kind::Artifact,
            version: pypi_version(&filename),
            content_type: "application/octet-stream".to_string(),
            extract_published: Some(Arc::new(last_modified)),
            final_gate: Some(self.final_policy_gate(res.clone(), &pypi_pkg, &pypi_ver)),
            ..FetchSpec::blank()
        };
        if res.repo.r#type == meta::TYPE_PROXY {
            let rest = res.path.strip_prefix("packages/").unwrap_or(&res.path);
            let (encoded_ref, part_filename) = match rest.split_once('/') {
                Some((r, f)) => (r, f),
                None => (rest, ""),
            };
            let Ok(raw) = BASE64URL.decode(encoded_ref) else {
                return http_error(StatusCode::BAD_REQUEST, "invalid package reference");
            };
            let Ok(text) = String::from_utf8(raw) else {
                return http_error(StatusCode::BAD_REQUEST, "invalid package reference");
            };
            let Ok(file_url) = Url::parse(&text) else {
                return http_error(StatusCode::BAD_REQUEST, "invalid package reference");
            };
            if (file_url.scheme() != "http" && file_url.scheme() != "https")
                || file_url.host_str().unwrap_or("").is_empty()
            {
                return http_error(StatusCode::BAD_REQUEST, "invalid package reference");
            }
            let mut u = file_url.to_string();
            // PEP 658: clients fetch core metadata at `<file-url>.metadata`, so
            // the rewritten ref still points at the distribution file itself.
            if part_filename.ends_with(".metadata") && !u.ends_with(".metadata") {
                u.push_str(".metadata");
            }
            spec.upstream_url = u;
            // The ref is client-controlled. Only the admin-configured upstream
            // host is trusted; any other host (PyPI legitimately serves files
            // from a different host than the index) is fetched via the
            // SSRF-guarded client that refuses private/loopback destinations.
            spec.untrusted_url = !same_host(&file_url, &res.repo.upstream_url);
        }
        self.engine.serve(parts, spec).await
    }

    /// Handles the twine legacy upload API: a multipart form with name, version
    /// and content fields POSTed to the repository root. Parts are consumed as a
    /// stream — the distribution file is hashed straight into the blob store the
    /// moment its part arrives and the artifact record is attached once the
    /// remaining fields are known (temp-blob-then-attach, as in Nexus), so an
    /// upload never holds the file in memory.
    pub(crate) async fn pypi_upload(
        self: &Arc<Self>,
        parts: Arc<Parts>,
        res: Resolved,
        body: Body,
    ) -> Response {
        if self.uploader.read().is_some() {
            return self.pypi_upload_atomic(&parts, &res, body).await;
        }
        let request = Request::from_parts((*parts).clone(), body);
        let mut multipart = match Multipart::from_request(request, &()).await {
            Ok(multipart) => multipart,
            Err(_) => return http_error(StatusCode::BAD_REQUEST, "invalid multipart form"),
        };
        let e = &self.engine;
        // Hold the GC read lock across the whole upload: the distribution bytes
        // are written (`blobs.put`) before the remaining fields are read, and
        // the artifact that references them is recorded (`record_upload`) only
        // afterward. Holding the read lock from before the write until the
        // reference exists stops the sweeper from reclaiming a matching
        // content-addressed digest in that gap (see `Engine::gc_mu`).
        // `abandon_blob` paths intentionally leave bytes for the sweeper and are
        // unaffected.
        let _gc = e.gc_mu.read().await;
        let mut fields: BTreeMap<String, String> = BTreeMap::new();
        let mut filename = String::new();
        let mut digest = String::new();
        let mut size = 0i64;
        let mut have_content = false;
        loop {
            let field = match multipart.next_field().await {
                Ok(Some(field)) => field,
                Ok(None) => break,
                Err(_) => return http_error(StatusCode::BAD_REQUEST, "invalid multipart form"),
            };
            let form_name = field.name().unwrap_or("").to_string();
            if form_name == "content" {
                if have_content {
                    return http_error(StatusCode::BAD_REQUEST, "duplicate content field");
                }
                filename = path_base(field.file_name().unwrap_or("")).to_string();
                // `BlobStore::put` takes an owned `'static` reader, but a
                // multipart field borrows the request. Pumping the field into
                // one half of an in-memory pipe while the store drains the
                // other keeps the upload streaming without buffering the file.
                let mut field = field;
                let (mut sink, source) = tokio::io::duplex(64 * 1024);
                let limited =
                    tokio::io::AsyncReadExt::take(source, (MAX_PYPI_UPLOAD_BYTES + 1) as u64);
                let store_blob = e.blobs.put(Box::pin(limited));
                let pump = async {
                    use tokio::io::AsyncWriteExt as _;
                    while let Ok(Some(chunk)) = field.chunk().await {
                        if sink.write_all(&chunk).await.is_err() {
                            break;
                        }
                    }
                    let _ = sink.shutdown().await;
                };
                let (stored, ()) = tokio::join!(store_blob, pump);
                match stored {
                    Ok((d, s)) => {
                        digest = d;
                        size = s;
                    }
                    Err(_) => return http_error(StatusCode::INTERNAL_SERVER_ERROR, "store failed"),
                }
                have_content = true;
                if size > MAX_PYPI_UPLOAD_BYTES {
                    e.abandon_blob(&digest, size).await;
                    return http_error(StatusCode::PAYLOAD_TOO_LARGE, "file too large");
                }
                continue;
            }
            // Metadata fields are small; twine repeats some (classifiers), where the first
            // value wins to match `FormValue`'s behavior.
            let mut field = field;
            let mut val: Vec<u8> = Vec::new();
            loop {
                match field.chunk().await {
                    Ok(Some(chunk)) => {
                        let room = (1 << 20) - val.len();
                        if room == 0 {
                            break;
                        }
                        let take = room.min(chunk.len());
                        val.extend_from_slice(&chunk[..take]);
                    }
                    Ok(None) => break,
                    Err(_) => {
                        return http_error(StatusCode::BAD_REQUEST, "invalid multipart form");
                    }
                }
            }
            fields
                .entry(form_name)
                .or_insert_with(|| String::from_utf8_lossy(&val).to_string());
        }
        let name = normalize_pypi(fields.get("name").map(String::as_str).unwrap_or(""));
        if name.is_empty() || !have_content {
            if have_content {
                e.abandon_blob(&digest, size).await;
            }
            return http_error(StatusCode::BAD_REQUEST, "missing name or content field");
        }
        if filename.is_empty() || filename == "." || filename == "/" {
            e.abandon_blob(&digest, size).await;
            return http_error(StatusCode::BAD_REQUEST, "invalid filename");
        }
        let p = format!("packages/{name}/{filename}");
        if e.record_upload(
            &res.repo,
            &p,
            fields.get("version").map(String::as_str).unwrap_or(""),
            "application/octet-stream",
            &digest,
            size,
            &username_from_context(&parts),
        )
        .await
        .is_err()
        {
            return http_error(StatusCode::INTERNAL_SERVER_ERROR, "store failed");
        }
        self.scan_stored(&res.repo, &p);
        self.resolve_stored(&res.repo, &p);
        StatusCode::CREATED.into_response()
    }
}

const PATH_SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~')
    .remove(b'$')
    .remove(b'&')
    .remove(b'+')
    .remove(b',')
    .remove(b':')
    .remove(b';')
    .remove(b'=')
    .remove(b'@');

/// A JSON object whose members are kept as raw fragments.
type RawObject = BTreeMap<String, Box<RawValue>>;

/// Extracts `format_metadata.requires_python` from an artifact's metadata
/// envelope.
fn requires_python(metadata_json: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Envelope {
        #[serde(default)]
        format_metadata: FormatMetadata,
    }
    #[derive(serde::Deserialize, Default)]
    struct FormatMetadata {
        #[serde(default)]
        requires_python: String,
    }
    let envelope: Envelope = serde_json::from_str(metadata_json).ok()?;
    if envelope.format_metadata.requires_python.is_empty() {
        return None;
    }
    Some(envelope.format_metadata.requires_python)
}

fn format_upload_time(t: DateTime<Utc>) -> String {
    let micros = t.timestamp_subsec_micros();
    if micros == 0 {
        return t.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    }
    let frac = format!("{micros:06}");
    let frac = frac.trim_end_matches('0');
    format!("{}.{frac}Z", t.format("%Y-%m-%dT%H:%M:%S"))
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#39;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            _ => out.push(c),
        }
    }
    out
}

/// Rewrites a raw PEP 691 index (see [`rewrite_simple_index_top`]). `None` means
/// the body is not PEP 691 JSON.
pub(crate) fn rewrite_simple_index(
    body: &[u8],
    base: &str,
    repo_name: &str,
    age: &AgePolicyConfig,
    now: DateTime<Utc>,
) -> Option<(Bytes, i64)> {
    let top = serde_json::from_slice::<RawObject>(body).ok()?;
    rewrite_simple_index_top(top, base, repo_name, age, now)
}

/// Rewrites a PEP 691 index: file URLs are pointed back at forklift and files
/// whose PEP 700 upload-time violates a blocking age policy are removed. It
/// returns the transformed JSON and the number of files removed.
///
/// The index is taken as top-level raw fragments and each file entry stays as
/// raw bytes; only the small map of the file being rewritten is decoded at a
/// time. A project with thousands of release files would otherwise expand into a
/// large array of maps all at once, which under a concurrent install burst is a
/// needless memory multiple over the document itself.
pub(crate) fn rewrite_simple_index_top(
    mut top: RawObject,
    base: &str,
    repo_name: &str,
    age: &AgePolicyConfig,
    now: DateTime<Utc>,
) -> Option<(Bytes, i64)> {
    let mut files: Vec<Box<RawValue>> = Vec::new();
    if let Some(raw) = top.get("files")
        && !raw.get().is_empty()
    {
        files = serde_json::from_str(raw.get()).ok()?;
    }
    let min_age = chrono::TimeDelta::from_std(age.min_age.d()).unwrap_or(chrono::TimeDelta::MAX);
    let mut kept: Vec<Box<RawValue>> = Vec::with_capacity(files.len());
    let mut removed = 0i64;
    for raw in files {
        let Ok(mut fm) = serde_json::from_str::<RawObject>(raw.get()) else {
            kept.push(raw);
            continue;
        };
        if age.enabled && age.action == ACTION_BLOCK {
            let ts: Option<String> = fm
                .get("upload-time")
                .and_then(|v| serde_json::from_str(v.get()).ok());
            if let Some(ts) = ts
                && let Ok(pub_at) = DateTime::parse_from_rfc3339(&ts)
                && now - pub_at.with_timezone(&Utc) < min_age
            {
                removed += 1;
                continue;
            }
        }
        let u: Option<String> = fm
            .get("url")
            .and_then(|v| serde_json::from_str(v.get()).ok());
        let Some(u) = u else {
            kept.push(raw);
            continue;
        };
        let name = fm
            .get("filename")
            .and_then(|v| serde_json::from_str::<String>(v.get()).ok())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| path_base(&u).to_string());
        let new_url = format!(
            "{base}/pypi/{repo_name}/packages/{}/{name}",
            BASE64URL.encode(u.as_bytes())
        );
        if let Ok(nb) = serde_json::to_string(&new_url)
            && let Ok(parsed) = RawValue::from_string(nb)
        {
            fm.insert("url".to_string(), parsed);
        }
        let nf = serde_json::to_string(&fm).ok()?;
        kept.push(RawValue::from_string(nf).ok()?);
    }
    let nb = serde_json::to_string(&kept).ok()?;
    top.insert("files".to_string(), RawValue::from_string(nb).ok()?);
    let out = serde_json::to_vec(&top).ok()?;
    Some((Bytes::from(out), removed))
}

/// Writes a project index, negotiating between PEP 691 JSON and a PEP 503 HTML
/// rendering of the same document.
fn write_simple(parts: &Parts, json_body: &Bytes, project: &str) -> Response {
    let wants_json = super::header_str(&parts.headers, "Accept").contains(PYPI_JSON_TYPE);
    let head = parts.method == Method::HEAD;
    let mut resp = if head {
        StatusCode::OK.into_response()
    } else if wants_json {
        json_body.clone().into_response()
    } else {
        simple_html(json_body, project).into_response()
    };
    let headers = resp.headers_mut();
    headers.insert(VARY, HeaderValue::from_static("Accept"));
    headers.insert(
        CONTENT_TYPE,
        if wants_json {
            HeaderValue::from_static(PYPI_JSON_TYPE)
        } else {
            HeaderValue::from_static("text/html; charset=utf-8")
        },
    );
    resp
}

/// Renders a PEP 691 JSON index as a PEP 503 HTML page for clients that do not
/// accept the JSON media type.
fn simple_html(json_body: &[u8], project: &str) -> String {
    #[derive(serde::Deserialize, Default)]
    struct Doc {
        #[serde(default)]
        files: Vec<File>,
    }
    #[derive(serde::Deserialize)]
    struct File {
        #[serde(default)]
        filename: String,
        #[serde(default)]
        url: String,
        #[serde(default)]
        hashes: BTreeMap<String, String>,
        #[serde(default, rename = "requires-python")]
        requires_python: String,
    }
    let doc: Doc = serde_json::from_slice(json_body).unwrap_or_default();
    let title = escape_html(&format!("Links for {project}"));
    let mut b = String::new();
    b.push_str("<!DOCTYPE html>\n<html>\n<head><meta name=\"pypi:repository-version\" content=\"1.0\"><title>");
    b.push_str(&title);
    b.push_str("</title></head>\n<body>\n<h1>");
    b.push_str(&title);
    b.push_str("</h1>\n");
    for f in &doc.files {
        let mut href = f.url.clone();
        if let Some(sha) = f.hashes.get("sha256")
            && !sha.is_empty()
        {
            href.push_str("#sha256=");
            href.push_str(sha);
        }
        b.push_str(&format!("<a href=\"{}\"", escape_html(&href)));
        if !f.requires_python.is_empty() {
            b.push_str(&format!(
                " data-requires-python=\"{}\"",
                escape_html(&f.requires_python)
            ));
        }
        b.push_str(&format!(">{}</a><br/>\n", escape_html(&f.filename)));
    }
    b.push_str("</body>\n</html>\n");
    b
}

/// Reports whether `u` points at the same host as the repository's configured
/// upstream URL.
pub(crate) fn same_host(u: &Url, upstream: &str) -> bool {
    let Ok(up) = Url::parse(upstream) else {
        return false;
    };
    u.host_str()
        .unwrap_or("")
        .eq_ignore_ascii_case(up.host_str().unwrap_or(""))
}

/// Applies PEP 503 project-name normalization.
pub(crate) fn normalize_pypi(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut in_run = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !in_run {
                out.push('-');
                in_run = true;
            }
            continue;
        }
        in_run = false;
        out.extend(c.to_lowercase());
    }
    out
}

/// Best-effort extracts the normalized project name from a distribution
/// filename: wheels are `name-version(-build)-python-abi-platform.whl` (name
/// uses underscores per the wheel spec), sdists end the stem with `-version`.
/// PEP 658 `.metadata` files map to their distribution file. Returns `""` when
/// the name cannot be derived (the approval gate never blocks on unknown names).
pub(crate) fn pypi_package_from_filename(f: &str) -> String {
    if let Some(before) = f.strip_suffix(".metadata") {
        return pypi_package_from_filename(before);
    }
    if let Some(stem) = f.strip_suffix(".whl") {
        let parts: Vec<&str> = stem.splitn(3, '-').collect();
        if parts.len() >= 2 {
            return normalize_pypi(parts[0]);
        }
    } else if let Some(stem) = f.strip_suffix(".tar.gz").or_else(|| f.strip_suffix(".zip"))
        && let Some(i) = stem.rfind('-')
        && i > 0
    {
        return normalize_pypi(&stem[..i]);
    }
    String::new()
}

/// Best-effort extracts the version from a distribution filename: wheels are
/// `name-version(-build)-python-abi-platform.whl`, sdists end the stem with
/// `-version`.
pub(crate) fn pypi_version(f: &str) -> String {
    if let Some(stem) = f.strip_suffix(".whl") {
        let parts: Vec<&str> = stem.split('-').collect();
        if parts.len() >= 2 {
            return parts[1].to_string();
        }
    } else if let Some(stem) = f.strip_suffix(".tar.gz").or_else(|| f.strip_suffix(".zip"))
        && let Some(i) = stem.rfind('-')
    {
        return stem[i + 1..].to_string();
    }
    String::new()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::response::IntoResponse;
    use axum::routing::any;
    use base64::Engine as _;
    use chrono::{TimeZone, Utc};
    use http::{Method, Request, StatusCode, Uri};

    use crate::meta;
    use crate::repoconfig::{self, ACTION_BLOCK, AgePolicyConfig, Duration};

    use crate::repo::pypi::{PYPI_JSON_TYPE, normalize_pypi, pypi_version};
    use crate::repo::uiupload::tests::{Part, multipart_body};
    use crate::testing::repo::{TestResponse, call, mk_format_repo, mux, new_test_manager, send};

    /// The PyPI upstream every proxy test runs against: a PEP 691 index with one old
    /// and one fresh file, plus the wheel and its PEP 658 metadata.
    async fn pypi_upstream() -> String {
        // The index embeds absolute file URLs, so the listener is bound first and
        // its address folded into the handler.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream");
        let base = format!("http://{}", listener.local_addr().expect("upstream addr"));
        let index_base = base.clone();
        let app = Router::new().fallback(any(move |uri: Uri| {
            let base = index_base.clone();
            async move {
                let last_modified =
                    [(http::header::LAST_MODIFIED, "Mon, 01 Jan 2024 00:00:00 GMT")];
                if uri.path().ends_with(".metadata") {
                    return (last_modified, "METADATA").into_response();
                }
                if uri.path().starts_with("/packages/") {
                    return (last_modified, "WHEEL").into_response();
                }
                // PEP 691 simple index with one old and one fresh file.
                (
                    [(http::header::CONTENT_TYPE, PYPI_JSON_TYPE)],
                    format!(
                        r#"{{
				"meta": {{"api-version": "1.1"}},
				"name": "demo",
				"versions": ["1.0.0", "2.0.0"],
				"files": [
					{{"filename": "demo-1.0.0-py3-none-any.whl",
					 "url": "{base}/packages/aa/demo-1.0.0-py3-none-any.whl",
					 "hashes": {{"sha256": "abc123"}},
					 "requires-python": ">=3.8",
					 "upload-time": "2024-01-01T00:00:00Z"}},
					{{"filename": "demo-2.0.0-py3-none-any.whl",
					 "url": "{base}/packages/bb/demo-2.0.0-py3-none-any.whl",
					 "hashes": {{"sha256": "def456"}},
					 "upload-time": "2025-06-09T00:00:00Z"}}
				]
			}}"#
                    ),
                )
                    .into_response()
            }
        }));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        base
    }

    /// Issues a GET with an `Accept` header.
    async fn get_accept(app: &Router, uri: &str, accept: &str) -> TestResponse {
        let request = Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header(http::header::ACCEPT, accept)
            .body(axum::body::Body::empty())
            .expect("build request");
        send(app, request).await
    }

    #[tokio::test]
    async fn pypi_proxy_index_rewrite_and_age_filter() {
        let upstream = pypi_upstream().await;

        let mut cfg = repoconfig::default();
        cfg.age_policy = AgePolicyConfig {
            enabled: true,
            min_age: Duration::from_std(std::time::Duration::from_secs(30 * 24 * 60 * 60)),
            action: ACTION_BLOCK.to_string(),
            ..Default::default()
        };
        let tm = new_test_manager().await;
        mk_format_repo(
            &tm.store,
            "p",
            meta::FORMAT_PYPI,
            meta::TYPE_PROXY,
            &format!("{upstream}/simple"),
            cfg,
        )
        .await;
        let at = Utc.with_ymd_and_hms(2025, 6, 10, 0, 0, 0).unwrap();
        tm.engine.set_now(Arc::new(move || at));
        let app = mux(&tm.manager);

        let resp = get_accept(&app, "/pypi/p/simple/demo/", PYPI_JSON_TYPE).await;
        assert_eq!(resp.status, StatusCode::OK, "index: {}", resp.text());
        assert_eq!(resp.header("Content-Type"), PYPI_JSON_TYPE, "content type");
        let doc: serde_json::Value = serde_json::from_slice(&resp.body).expect("index json");
        let files = doc["files"].as_array().expect("files");
        assert_eq!(
            files.len(),
            1,
            "fresh file should be filtered by 30d cooldown, got {files:?}"
        );
        assert_eq!(files[0]["filename"], "demo-1.0.0-py3-none-any.whl");
        let want_ref = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(
            "{upstream}/packages/aa/demo-1.0.0-py3-none-any.whl"
        ));
        let url = files[0]["url"].as_str().unwrap_or_default();
        assert!(
            url.contains(&format!(
                "/pypi/p/packages/{want_ref}/demo-1.0.0-py3-none-any.whl"
            )),
            "file url not rewritten: {url:?}"
        );

        // Download through the rewritten path.
        let resp = call(
            &app,
            Method::GET,
            &format!("/pypi/p/packages/{want_ref}/demo-1.0.0-py3-none-any.whl"),
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "file");
        assert_eq!(resp.text(), "WHEEL");

        // PEP 658: <file-url>.metadata resolves to the upstream metadata file.
        let resp = call(
            &app,
            Method::GET,
            &format!("/pypi/p/packages/{want_ref}/demo-1.0.0-py3-none-any.whl.metadata"),
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "metadata");
        assert_eq!(resp.text(), "METADATA");

        // A bogus package reference is rejected.
        let resp = call(&app, Method::GET, "/pypi/p/packages/!!!/x.whl", "").await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "bogus ref");
    }

    #[tokio::test]
    async fn pypi_proxy_index_html() {
        let upstream = pypi_upstream().await;
        let tm = new_test_manager().await;
        mk_format_repo(
            &tm.store,
            "p",
            meta::FORMAT_PYPI,
            meta::TYPE_PROXY,
            &format!("{upstream}/simple"),
            repoconfig::default(),
        )
        .await;
        let app = mux(&tm.manager);

        // No PEP 691 accept header: a browser or old pip gets PEP 503 HTML.
        let resp = call(&app, Method::GET, "/pypi/p/simple/demo/", "").await;
        assert_eq!(resp.status, StatusCode::OK, "index");
        assert!(
            resp.header("Content-Type").starts_with("text/html"),
            "content type = {:?}",
            resp.header("Content-Type")
        );
        let body = resp.text();
        assert!(
            body.contains(">demo-1.0.0-py3-none-any.whl</a>")
                && body.contains("#sha256=abc123")
                && body.contains(r#"data-requires-python="&gt;=3.8""#),
            "html missing expected anchors:\n{body}"
        );

        // Second request is served from the metadata cache.
        let resp = get_accept(&app, "/pypi/p/simple/demo/", PYPI_JSON_TYPE).await;
        assert_eq!(resp.status, StatusCode::OK, "cached index");
        assert_eq!(resp.header("Content-Type"), PYPI_JSON_TYPE, "cached index");
    }

    /// Issues a twine-style upload with the given multipart parts.
    async fn pypi_post(app: &Router, uri: &str, parts: Vec<Part>) -> TestResponse {
        let (content_type, body) = multipart_body(parts);
        let request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(http::header::CONTENT_TYPE, content_type)
            .body(axum::body::Body::from(body))
            .expect("build request");
        send(app, request).await
    }

    #[tokio::test]
    async fn pypi_local_upload_and_index() {
        let tm = new_test_manager().await;
        mk_format_repo(
            &tm.store,
            "internal",
            meta::FORMAT_PYPI,
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        let app = mux(&tm.manager);

        let resp = pypi_post(
            &app,
            "/pypi/internal",
            vec![
                Part::Field("name", "My_Package".to_string()),
                Part::Field("version", "1.2.3".to_string()),
                Part::File(
                    "content",
                    "my_package-1.2.3-py3-none-any.whl".to_string(),
                    b"LOCAL-WHEEL".to_vec(),
                ),
            ],
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "upload: {}", resp.text());

        // Index uses the PEP 503 normalized project name.
        let resp = get_accept(&app, "/pypi/internal/simple/my-package/", PYPI_JSON_TYPE).await;
        assert_eq!(resp.status, StatusCode::OK, "index");
        let doc: serde_json::Value = serde_json::from_slice(&resp.body).expect("index json");
        let files = doc["files"].as_array().expect("files");
        assert_eq!(files.len(), 1, "files = {files:?}");
        assert_eq!(files[0]["filename"], "my_package-1.2.3-py3-none-any.whl");
        assert!(
            files[0]["hashes"]["sha256"]
                .as_str()
                .is_some_and(|v| !v.is_empty()),
            "file should carry its sha256"
        );
        let versions = doc["versions"].as_array().expect("versions");
        assert_eq!(versions.len(), 1, "versions = {versions:?}");
        assert_eq!(versions[0], "1.2.3");
        let url = files[0]["url"].as_str().unwrap_or_default();
        assert!(
            url.contains("/pypi/internal/packages/my-package/my_package-1.2.3-py3-none-any.whl"),
            "file url = {url:?}"
        );

        // Download the stored file.
        let resp = call(
            &app,
            Method::GET,
            "/pypi/internal/packages/my-package/my_package-1.2.3-py3-none-any.whl",
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "download");
        assert_eq!(resp.text(), "LOCAL-WHEEL");

        // Unknown project 404s; upload to a proxy repo is rejected.
        let resp = call(&app, Method::GET, "/pypi/internal/simple/nope/", "").await;
        assert_eq!(resp.status, StatusCode::NOT_FOUND, "unknown project");
        mk_format_repo(
            &tm.store,
            "proxyrepo",
            meta::FORMAT_PYPI,
            meta::TYPE_PROXY,
            "https://pypi.org/simple",
            repoconfig::default(),
        )
        .await;
        let resp = call(&app, Method::POST, "/pypi/proxyrepo", "x").await;
        assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED, "proxy upload");
    }

    /// twine is usually configured with a trailing slash on the repository URL
    /// (`--repository-url https://host/pypi/internal/`); the root route must match
    /// that form too instead of falling through to the console fallback.
    #[tokio::test]
    async fn pypi_upload_repository_root_with_trailing_slash() {
        let tm = new_test_manager().await;
        mk_format_repo(
            &tm.store,
            "internal",
            meta::FORMAT_PYPI,
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        let app = mux(&tm.manager);

        let resp = pypi_post(
            &app,
            "/pypi/internal/",
            vec![
                Part::Field("name", "My_Package".to_string()),
                Part::Field("version", "1.2.3".to_string()),
                Part::File(
                    "content",
                    "my_package-1.2.3-py3-none-any.whl".to_string(),
                    b"LOCAL-WHEEL".to_vec(),
                ),
            ],
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "upload: {}", resp.text());

        let resp = get_accept(&app, "/pypi/internal/simple/my-package/", PYPI_JSON_TYPE).await;
        assert_eq!(resp.status, StatusCode::OK, "index");
    }

    /// The streaming upload parser must not depend on field order: some clients put
    /// the content part before the metadata fields.
    #[tokio::test]
    async fn pypi_upload_content_field_first() {
        let tm = new_test_manager().await;
        mk_format_repo(
            &tm.store,
            "internal",
            meta::FORMAT_PYPI,
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        let app = mux(&tm.manager);

        let resp = pypi_post(
            &app,
            "/pypi/internal",
            vec![
                Part::File(
                    "content",
                    "pkg-0.1.0.tar.gz".to_string(),
                    b"SDIST-BYTES".to_vec(),
                ),
                Part::Field("name", "pkg".to_string()),
                Part::Field("version", "0.1.0".to_string()),
            ],
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "upload: {}", resp.text());

        let resp = call(
            &app,
            Method::GET,
            "/pypi/internal/packages/pkg/pkg-0.1.0.tar.gz",
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "download");
        assert_eq!(resp.text(), "SDIST-BYTES");
    }

    /// An upload that streamed its file but then fails validation must leave the
    /// stored blob sweepable (zero-reference record) rather than leaking it.
    #[tokio::test]
    async fn pypi_upload_missing_name_abandons_blob() {
        let tm = new_test_manager().await;
        mk_format_repo(
            &tm.store,
            "internal",
            meta::FORMAT_PYPI,
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        let app = mux(&tm.manager);

        let resp = pypi_post(
            &app,
            "/pypi/internal",
            vec![Part::File(
                "content",
                "orphan-1.0.0.tar.gz".to_string(),
                b"ORPHAN-BYTES".to_vec(),
            )],
        )
        .await;
        assert_eq!(
            resp.status,
            StatusCode::BAD_REQUEST,
            "upload without name: {}",
            resp.text()
        );

        let shas = tm
            .store
            .list_unreferenced_blobs(16, Utc::now())
            .await
            .expect("list unreferenced blobs");
        assert_eq!(
            shas.len(),
            1,
            "unreferenced blobs, want 1 (abandoned upload must be sweepable)"
        );
    }

    #[test]
    fn pypi_helpers() {
        for (input, want) in [
            ("My_Package", "my-package"),
            ("foo.bar--baz", "foo-bar-baz"),
            ("requests", "requests"),
            ("Django_REST.types", "django-rest-types"),
        ] {
            assert_eq!(normalize_pypi(input), want, "normalize_pypi({input:?})");
        }
        for (input, want) in [
            ("demo-1.0.0-py3-none-any.whl", "1.0.0"),
            ("demo-2.31.0.tar.gz", "2.31.0"),
            ("demo-0.1.zip", "0.1"),
            ("plain.txt", ""),
        ] {
            assert_eq!(pypi_version(input), want, "pypi_version({input:?})");
        }
    }
}
