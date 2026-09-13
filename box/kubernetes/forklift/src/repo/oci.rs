//! The OCI format serves the OCI Distribution Specification v1.1 (end-1
//! through end-11) under `/v2/{repo}/…`. The docker client derives its request
//! paths from the image reference and always addresses `/v2` at the host root,
//! so unlike the package formats the Forklift repository name is embedded in
//! the image name:
//!
//! ```text
//! docker pull forklift.example.com/oci-public/library/nginx:1.27
//! → GET /v2/oci-public/library/nginx/manifests/1.27
//! ```
//!
//! Blobs and manifests are immutable and digest-addressed, stored as ordinary
//! artifact rows (`{name}/blobs/{digest}`, `{name}/manifests/{digest}`) so blob
//! reference counting, replication and the sweeper work unchanged. Tags are the
//! only mutable object and live in the `oci_tags` table. The referrers API
//! (end-12) is served from the stored manifests.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use http::header::{CONTENT_TYPE, ETAG};
use http::request::Parts;
use http::{HeaderValue, Method, StatusCode};
use once_cell::sync::Lazy;
use regex::Regex;

use crate::meta::{self, Artifact, OCITag};
use crate::{auth, storage};

use super::router::{Resolved, action_for_method};
use super::{FetchSpec, Manager, header_str, itoa, username_from_context};

/// The spec grammar for an OCI repository name (the image name inside a
/// Forklift repository).
pub(crate) static OCI_NAME_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^[a-z0-9]+(?:[._-][a-z0-9]+)*(?:/[a-z0-9]+(?:[._-][a-z0-9]+)*)*$")
        .expect("valid regex")
});

/// Accepts the only algorithm Forklift stores by: the blob store is keyed by
/// SHA-256, so a digest in another algorithm is unresolvable.
pub(crate) static OCI_DIGEST_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^sha256:[a-f0-9]{64}$").expect("valid regex"));

/// The spec grammar for a tag reference.
pub(crate) static OCI_TAG_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}$").expect("valid regex"));

// Manifest media types accepted on push and requested on proxy pull. Docker
// schema 1 is deliberately absent: it is signed, path-dependent and long
// deprecated, and storing it byte-identically would still not make it safe to
// serve.
pub(crate) const OCI_MEDIA_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub(crate) const OCI_MEDIA_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub(crate) const DOCKER_MEDIA_MANIFEST: &str =
    "application/vnd.docker.distribution.manifest.v2+json";
pub(crate) const DOCKER_MEDIA_LIST: &str =
    "application/vnd.docker.distribution.manifest.list.v2+json";

/// The `Accept` header for proxy manifest fetches, every type Forklift can
/// store, so the upstream never falls back to schema 1.
pub(crate) const OCI_MANIFEST_ACCEPT: &str = concat!(
    "application/vnd.oci.image.index.v1+json",
    ", ",
    "application/vnd.oci.image.manifest.v1+json",
    ", ",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    ", ",
    "application/vnd.docker.distribution.manifest.v2+json"
);

pub(crate) fn oci_manifest_media_type(mt: &str) -> bool {
    matches!(
        mt,
        OCI_MEDIA_MANIFEST | OCI_MEDIA_INDEX | DOCKER_MEDIA_MANIFEST | DOCKER_MEDIA_LIST
    )
}

pub(crate) fn oci_index_media_type(mt: &str) -> bool {
    mt == OCI_MEDIA_INDEX || mt == DOCKER_MEDIA_LIST
}

/// The artifact-row path for an OCI blob.
///
/// Name-scoping is deliberate even though blob bytes deduplicate globally: one
/// row per (name, digest) keeps per-image listing, deletion, audit and blob
/// reference counting correct when two images share a layer.
pub(crate) fn oci_blob_path(name: &str, digest: &str) -> String {
    format!("{name}/blobs/{digest}")
}

/// The artifact-row path for an OCI manifest.
pub(crate) fn oci_manifest_path(name: &str, digest: &str) -> String {
    format!("{name}/manifests/{digest}")
}

/// [`oci_manifest_path`] for callers outside the module: the management API
/// addresses an OCI artifact by the identity every other format uses
/// (repository plus stored path), which for an image or chart is its manifest
/// path. Artifact labels key on it.
pub fn oci_manifest_path_public(name: &str, digest: &str) -> String {
    oci_manifest_path(name, digest)
}

/// Writes the spec error envelope `{"errors":[{"code":…,"message":…}]}` that
/// OCI clients parse for diagnostics.
pub(crate) fn write_oci_error(status: StatusCode, code: &str, message: &str) -> Response {
    let mut envelope = serde_json::Map::new();
    envelope.insert(
        "errors".to_string(),
        serde_json::json!([{ "code": code, "message": message }]),
    );
    let mut body = serde_json::to_string(&envelope).unwrap_or_else(|_| "{}".to_string());
    body.push('\n');
    let mut resp = (status, body).into_response();
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

/// Serves `GET /v2/`, the endpoint docker clients probe to discover the API
/// version and the authentication scheme.
///
/// An unauthenticated probe is always answered 401 with the Basic challenge —
/// even when anonymous read is enabled — because the client uses this response
/// to decide HOW to authenticate, not WHETHER access is allowed: a 200 here
/// makes podman/docker drop their credentials entirely and then fail every
/// push. Anonymous readers ignore the challenge and proceed; per-request
/// authorization still governs access.
pub(crate) async fn handle_oci_base(m: Arc<Manager>, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    if parts.method != Method::GET && parts.method != Method::HEAD {
        return write_oci_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "UNSUPPORTED",
            "method not allowed",
        );
    }
    let mut resp = if m.authz.is_some() && auth::from_request_parts(&parts).is_none() {
        auth::unauthorized_basic()
    } else {
        let mut resp = "{}".into_response();
        resp.headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        resp
    };
    resp.headers_mut().insert(
        http::HeaderName::from_static("docker-distribution-api-version"),
        HeaderValue::from_static("registry/2.0"),
    );
    resp
}

/// One parsed distribution-API request below `/v2/{repo}/`.
#[derive(Debug, Clone, Default)]
pub(crate) struct OciRequest {
    /// OCI repository name (the image name).
    pub(crate) name: String,
    /// manifests | blobs | tags | uploads | referrers
    pub(crate) endpoint: &'static str,
    /// tag or digest (manifests), digest (blobs), session id (uploads)
    pub(crate) reference: String,
}

/// Splits the repo-relative path into (name, endpoint, reference). The name
/// grammar permits segments like "blobs", so the split anchors on the LAST
/// occurrence of a recognized endpoint suffix, the way registries parse this
/// path in practice.
pub(crate) fn parse_oci_path(wildcard: &str) -> Option<OciRequest> {
    let wildcard = wildcard.strip_suffix('/').unwrap_or(wildcard);
    if let Some(name) = wildcard.strip_suffix("/tags/list") {
        return OCI_NAME_RE.is_match(name).then(|| OciRequest {
            name: name.to_string(),
            endpoint: "tags",
            reference: String::new(),
        });
    }
    if let Some(name) = wildcard.strip_suffix("/blobs/uploads") {
        return OCI_NAME_RE.is_match(name).then(|| OciRequest {
            name: name.to_string(),
            endpoint: "uploads",
            reference: String::new(),
        });
    }
    for (marker, endpoint) in [
        ("/blobs/uploads/", "uploads"),
        ("/referrers/", "referrers"),
        ("/manifests/", "manifests"),
        ("/blobs/", "blobs"),
    ] {
        if let Some(i) = wildcard.rfind(marker) {
            let (name, reference) = (&wildcard[..i], &wildcard[i + marker.len()..]);
            let ok =
                OCI_NAME_RE.is_match(name) && !reference.is_empty() && !reference.contains('/');
            return ok.then(|| OciRequest {
                name: name.to_string(),
                endpoint,
                reference: reference.to_string(),
            });
        }
    }
    None
}

/// Dispatches every request under `/v2/{repo}/`. RBAC runs through the shared
/// authorize path; PATCH maps to write via [`action_for_method`] like the other
/// mutating methods.
pub(crate) async fn handle_oci(m: Arc<Manager>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let res = match m.resolve(&parts, meta::FORMAT_OCI).await {
        Ok(res) => res,
        Err(resp) => return resp,
    };
    let Some(request) = parse_oci_path(&res.path) else {
        return write_oci_error(
            StatusCode::NOT_FOUND,
            "NAME_UNKNOWN",
            "unrecognized repository path",
        );
    };
    if let Err(resp) = m.authorize(
        &parts,
        &res.repo.name,
        action_for_method(&parts.method),
        res.cfg.public,
    ) {
        return resp;
    }

    let parts = Arc::new(parts);
    match request.endpoint {
        "manifests" => m.oci_manifests(parts, res, request, body).await,
        "blobs" => m.oci_blobs(parts, res, request).await,
        "tags" => m.oci_tags_list(&parts, &res, &request).await,
        "uploads" => m.oci_uploads(&parts, &res, &request, body).await,
        "referrers" => m.oci_referrers(&parts, &res, &request).await,
        _ => write_oci_error(StatusCode::NOT_FOUND, "UNSUPPORTED", "unsupported endpoint"),
    }
}

impl Manager {
    /// Serves GET/HEAD/PUT/DELETE `…/manifests/{reference}`.
    async fn oci_manifests(
        self: &Arc<Self>,
        parts: Arc<Parts>,
        res: Resolved,
        req: OciRequest,
        body: Body,
    ) -> Response {
        let is_digest = OCI_DIGEST_RE.is_match(&req.reference);
        if !is_digest && !OCI_TAG_RE.is_match(&req.reference) {
            // A push to a malformed reference is a client error; a pull of one
            // is indistinguishable from a manifest that does not exist (the
            // spec's conformance suite pulls ".INVALID_MANIFEST_NAME" and
            // expects 404).
            return if parts.method == Method::PUT {
                write_oci_error(
                    StatusCode::BAD_REQUEST,
                    "MANIFEST_INVALID",
                    "invalid reference",
                )
            } else {
                write_oci_error(
                    StatusCode::NOT_FOUND,
                    "MANIFEST_UNKNOWN",
                    "manifest unknown",
                )
            };
        }

        match parts.method {
            Method::GET | Method::HEAD => {
                // The policy coordinate is (image name, tag-or-digest):
                // approval and version denies review "which image, which
                // version", matching how a reviewer thinks about an image.
                if let Some(resp) = self
                    .policy_gates(
                        Arc::clone(&parts),
                        Arc::new(res.clone()),
                        &req.name,
                        &req.reference,
                    )
                    .await
                {
                    return resp;
                }
                self.oci_serve_manifest(&parts, &res, &req, is_digest).await
            }
            Method::PUT => {
                if res.repo.r#type != meta::TYPE_HOSTED {
                    return write_oci_error(
                        StatusCode::METHOD_NOT_ALLOWED,
                        "UNSUPPORTED",
                        "pushes are only allowed on hosted repositories",
                    );
                }
                self.oci_put_manifest(&parts, &res, &req, is_digest, body)
                    .await
            }
            Method::DELETE => {
                if res.repo.r#type != meta::TYPE_HOSTED {
                    return write_oci_error(
                        StatusCode::METHOD_NOT_ALLOWED,
                        "UNSUPPORTED",
                        "deletes are only allowed on hosted repositories",
                    );
                }
                self.oci_delete_manifest(&res, &req, is_digest).await
            }
            _ => write_oci_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "UNSUPPORTED",
                "method not allowed",
            ),
        }
    }

    /// Resolves the reference to a digest (via `oci_tags` for tag references)
    /// and serves the stored manifest, falling through to the proxy fetch on a
    /// miss. `Docker-Content-Digest` is set before the body is written: the
    /// client verifies it against the bytes.
    async fn oci_serve_manifest(
        self: &Arc<Self>,
        parts: &Arc<Parts>,
        res: &Resolved,
        req: &OciRequest,
        is_digest: bool,
    ) -> Response {
        let mut digest = req.reference.clone();
        if !is_digest {
            match self
                .store
                .get_oci_tag(res.repo.id, &req.name, &req.reference)
                .await
            {
                Ok(tag)
                    if res.repo.r#type == meta::TYPE_HOSTED || self.oci_tag_fresh(res, &tag) =>
                {
                    digest = tag.manifest_digest;
                }
                // Unknown or stale tag: ask upstream (which re-caches the tag
                // row).
                _ if res.repo.r#type == meta::TYPE_PROXY => {
                    return self.oci_proxy_manifest(parts, res, req).await;
                }
                Ok(tag) => digest = tag.manifest_digest,
                Err(_) => {
                    return write_oci_error(
                        StatusCode::NOT_FOUND,
                        "MANIFEST_UNKNOWN",
                        "manifest unknown",
                    );
                }
            }
        }

        let art = match self
            .store
            .get_artifact(res.repo.id, &oci_manifest_path(&req.name, &digest))
            .await
        {
            Ok(art) => art,
            Err(_) => {
                if res.repo.r#type == meta::TYPE_PROXY {
                    return self.oci_proxy_manifest(parts, res, req).await;
                }
                return write_oci_error(
                    StatusCode::NOT_FOUND,
                    "MANIFEST_UNKNOWN",
                    "manifest unknown",
                );
            }
        };
        let spec = FetchSpec {
            repo: res.repo.clone(),
            cfg: res.cfg.clone(),
            path: art.path.clone(),
            ..FetchSpec::blank()
        };
        if let Some(resp) = self.engine.age_gate(&spec, art.published_at) {
            return resp;
        }
        // The pipeline's post-age policies (human approval last) run before the
        // stored manifest is served, mirroring the final gate the package
        // formats pass into the engine. Every image pull begins with a manifest
        // request, so gating here gates the image.
        let gate = self.final_policy_gate(res.clone(), &req.name, &req.reference);
        if let Some(resp) = gate(Arc::clone(parts)).await {
            return resp;
        }
        if res.repo.r#type == meta::TYPE_PROXY {
            self.engine
                .cache_hits
                .with_label_values(&[&res.repo.name])
                .inc();
        }
        self.engine.touch(&art, &username_from_context(parts)).await;
        let (mut resp, n) = self.engine.serve_artifact(parts, &art).await;
        self.engine
            .bytes
            .with_label_values(&["egress", &res.repo.format])
            .inc_by(n as f64);
        if let Ok(v) = HeaderValue::from_str(&digest) {
            resp.headers_mut()
                .insert(http::HeaderName::from_static("docker-content-digest"), v);
        }
        resp
    }

    /// Serves GET/HEAD/DELETE `…/blobs/{digest}`.
    async fn oci_blobs(
        self: &Arc<Self>,
        parts: Arc<Parts>,
        res: Resolved,
        req: OciRequest,
    ) -> Response {
        if !OCI_DIGEST_RE.is_match(&req.reference) {
            return write_oci_error(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "invalid digest");
        }
        match parts.method {
            Method::GET | Method::HEAD => {
                // Blob pulls follow a manifest pull that already passed the
                // policy gates for (name, reference); the blob request carries
                // no version, so gates that need one no-op here exactly like
                // versionless package paths.
                let art = match self
                    .store
                    .get_artifact(res.repo.id, &oci_blob_path(&req.name, &req.reference))
                    .await
                {
                    Ok(art) => art,
                    Err(_) => {
                        if res.repo.r#type == meta::TYPE_PROXY {
                            return self.oci_proxy_blob(&parts, &res, &req).await;
                        }
                        return write_oci_error(
                            StatusCode::NOT_FOUND,
                            "BLOB_UNKNOWN",
                            "blob unknown",
                        );
                    }
                };
                if res.repo.r#type == meta::TYPE_PROXY {
                    self.engine
                        .cache_hits
                        .with_label_values(&[&res.repo.name])
                        .inc();
                }
                self.engine
                    .touch(&art, &username_from_context(&parts))
                    .await;
                self.oci_serve_blob_artifact(&parts, &res, &art, &req.reference)
                    .await
            }
            Method::DELETE => {
                if res.repo.r#type != meta::TYPE_HOSTED {
                    return write_oci_error(
                        StatusCode::METHOD_NOT_ALLOWED,
                        "UNSUPPORTED",
                        "deletes are only allowed on hosted repositories",
                    );
                }
                if self
                    .store
                    .delete_artifact(res.repo.id, &oci_blob_path(&req.name, &req.reference))
                    .await
                    .is_err()
                {
                    return write_oci_error(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "blob unknown");
                }
                StatusCode::ACCEPTED.into_response()
            }
            _ => write_oci_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "UNSUPPORTED",
                "method not allowed",
            ),
        }
    }

    /// Serves GET `…/tags/list` with the spec's `n`/`last` pagination. Hosted
    /// lists come from `oci_tags`; proxy lists pass through to the upstream (a
    /// tag list is a moving index and is not worth caching separately from the
    /// tag rows pulls create); group lists are merged upstream of this handler
    /// (see `group_metadata.rs`).
    async fn oci_tags_list(
        self: &Arc<Self>,
        parts: &Arc<Parts>,
        res: &Resolved,
        req: &OciRequest,
    ) -> Response {
        if parts.method != Method::GET && parts.method != Method::HEAD {
            return write_oci_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "UNSUPPORTED",
                "method not allowed",
            );
        }
        if res.repo.r#type == meta::TYPE_PROXY {
            return self.oci_proxy_tags_list(parts, res, req).await;
        }
        let Ok(tags) = self.store.list_oci_tags(res.repo.id, &req.name).await else {
            return write_oci_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "UNSUPPORTED",
                "metadata error",
            );
        };
        write_oci_tags_list(parts, &req.name, tags)
    }

    /// Serves GET `…/referrers/{digest}` (end-12): an OCI image index listing
    /// every stored manifest whose subject points at the digest, with the
    /// optional `artifactType` filter. The subject association is discovered by
    /// parsing the stored manifests for the name; per-image manifest counts are
    /// small and the documents are size-capped, so a scan beats a schema change.
    /// A digest nothing refers to (or that does not exist) is an empty index
    /// with status 200, per spec.
    async fn oci_referrers(
        self: &Arc<Self>,
        parts: &Arc<Parts>,
        res: &Resolved,
        req: &OciRequest,
    ) -> Response {
        if parts.method != Method::GET && parts.method != Method::HEAD {
            return write_oci_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "UNSUPPORTED",
                "method not allowed",
            );
        }
        if !OCI_DIGEST_RE.is_match(&req.reference) {
            return write_oci_error(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "invalid digest");
        }
        let Ok(arts) = self
            .store
            .list_artifacts(res.repo.id, &format!("{}/manifests/", req.name))
            .await
        else {
            return write_oci_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "UNSUPPORTED",
                "metadata error",
            );
        };
        let filter = query_param(parts, "artifactType");
        let mut descriptors: Vec<serde_json::Value> = Vec::new();
        for art in &arts {
            let Some((_, digest)) = super::oci_prune::split_oci_path(&art.path, "/manifests/")
            else {
                continue;
            };
            let Ok(doc) = self.read_oci_manifest(art).await else {
                continue;
            };
            let Some(subject) = &doc.subject else {
                continue;
            };
            if subject.digest != req.reference {
                continue;
            }
            let mut artifact_type = doc.artifact_type.clone();
            if artifact_type.is_empty()
                && let Some(config) = &doc.config
            {
                artifact_type = config.media_type.clone();
            }
            if !filter.is_empty() && artifact_type != filter {
                continue;
            }
            let mut descriptor = serde_json::Map::new();
            descriptor.insert(
                "digest".to_string(),
                serde_json::Value::String(digest.clone()),
            );
            descriptor.insert(
                "mediaType".to_string(),
                serde_json::Value::String(art.content_type.clone()),
            );
            descriptor.insert("size".to_string(), serde_json::Value::from(art.size));
            if !artifact_type.is_empty() {
                descriptor.insert(
                    "artifactType".to_string(),
                    serde_json::Value::String(artifact_type),
                );
            }
            if !doc.annotations.is_empty() {
                descriptor.insert(
                    "annotations".to_string(),
                    serde_json::to_value(&doc.annotations).unwrap_or(serde_json::Value::Null),
                );
            }
            descriptors.push(serde_json::Value::Object(descriptor));
        }
        let mut document = std::collections::BTreeMap::new();
        document.insert(
            "manifests".to_string(),
            serde_json::Value::Array(descriptors),
        );
        document.insert(
            "mediaType".to_string(),
            serde_json::Value::String(OCI_MEDIA_INDEX.to_string()),
        );
        document.insert("schemaVersion".to_string(), serde_json::Value::from(2));
        let mut body = serde_json::to_string(&document).unwrap_or_else(|_| "{}".to_string());
        body.push('\n');
        let mut resp = body.into_response();
        let headers = resp.headers_mut();
        if !filter.is_empty() {
            headers.insert(
                http::HeaderName::from_static("oci-filters-applied"),
                HeaderValue::from_static("artifactType"),
            );
        }
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(OCI_MEDIA_INDEX));
        resp
    }

    /// Writes a stored blob. Range requests are honoured when the blob store
    /// can expose the bytes as a random-access file: containerd and podman
    /// resume interrupted layer pulls with a Range header, and a registry that
    /// answers 200-with-full-body forces the whole layer to restart.
    async fn oci_serve_blob_artifact(
        self: &Arc<Self>,
        parts: &Arc<Parts>,
        res: &Resolved,
        art: &Artifact,
        digest: &str,
    ) -> Response {
        let range = header_str(&parts.headers, "Range").to_string();
        if !range.is_empty()
            && let Some(seekable) = self.engine.blobs.as_seekable()
            && let Ok((file, size)) = seekable.open_seekable(&art.blob_sha256).await
            && let Some(mut resp) = serve_range(parts, file, size, art, &range).await
        {
            let headers = resp.headers_mut();
            if let Ok(v) = HeaderValue::from_str(digest) {
                headers.insert(http::HeaderName::from_static("docker-content-digest"), v);
            }
            if let Ok(v) = HeaderValue::from_str(&format!("\"{}\"", art.blob_sha256)) {
                headers.insert(ETAG, v);
            }
            let n = resp
                .headers()
                .get(http::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            self.engine
                .bytes
                .with_label_values(&["egress", &res.repo.format])
                .inc_by(n as f64);
            return resp;
        }
        let (mut resp, n) = self.engine.serve_artifact(parts, art).await;
        self.engine
            .bytes
            .with_label_values(&["egress", &res.repo.format])
            .inc_by(n as f64);
        if let Ok(v) = HeaderValue::from_str(digest) {
            resp.headers_mut()
                .insert(http::HeaderName::from_static("docker-content-digest"), v);
        }
        resp
    }

    /// Reports whether a cached proxy tag row is still fresh. Tags are mutable
    /// upstream pointers, so they revalidate on the metadata TTL like every
    /// other mutable index; digest-addressed objects never do.
    pub(crate) fn oci_tag_fresh(&self, res: &Resolved, tag: &OCITag) -> bool {
        if !res.cfg.cache.enabled {
            return false;
        }
        let ttl = res.cfg.cache.metadata_ttl.d();
        if ttl.is_zero() {
            return false;
        }
        (self.engine.now() - tag.updated_at)
            .to_std()
            .is_ok_and(|elapsed| elapsed < ttl)
    }
}

/// Renders a tags/list response, applying `n`/`last` pagination and emitting
/// the RFC 5988 Link header when the page is full.
pub(crate) fn write_oci_tags_list(parts: &Parts, name: &str, mut tags: Vec<String>) -> Response {
    tags.sort();
    let last = query_param(parts, "last");
    if !last.is_empty() {
        let i = match tags.binary_search(&last) {
            Ok(i) => i + 1,
            Err(i) => i,
        };
        tags.drain(..i);
    }
    let n_str = query_param(parts, "n");
    let mut truncated = false;
    if !n_str.is_empty()
        && let Ok(n) = n_str.parse::<usize>()
        && n < tags.len()
    {
        tags.truncate(n);
        truncated = true;
    }
    let link = (truncated && !tags.is_empty()).then(|| {
        format!(
            "<{}?n={n_str}&last={}>; rel=\"next\"",
            parts.uri.path(),
            tags[tags.len() - 1]
        )
    });
    let mut document = std::collections::BTreeMap::new();
    document.insert(
        "name".to_string(),
        serde_json::Value::String(name.to_string()),
    );
    document.insert(
        "tags".to_string(),
        serde_json::Value::Array(tags.into_iter().map(serde_json::Value::String).collect()),
    );
    let mut body = serde_json::to_string(&document).unwrap_or_else(|_| "{}".to_string());
    body.push('\n');
    let mut resp = body.into_response();
    let headers = resp.headers_mut();
    if let Some(link) = link
        && let Ok(v) = HeaderValue::from_str(&link)
    {
        headers.insert(http::header::LINK, v);
    }
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

/// The first value of a query parameter, or `""`.
pub(crate) fn query_param(parts: &Parts, key: &str) -> String {
    let query = parts.uri.query().unwrap_or("");
    form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

async fn serve_range(
    parts: &Parts,
    file: Box<dyn storage::ReadSeekCloser>,
    size: i64,
    art: &Artifact,
    range: &str,
) -> Option<Response> {
    let (start, end) = parse_single_range(range, size)?;
    if start >= size {
        let mut resp = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
        if let Ok(v) = HeaderValue::from_str(&format!("bytes */{size}")) {
            resp.headers_mut().insert(http::header::CONTENT_RANGE, v);
        }
        return Some(resp);
    }
    let length = (end - start + 1) as usize;
    let body = tokio::task::spawn_blocking(move || {
        let mut buf = vec![0u8; length];
        let mut read = 0usize;
        while read < length {
            match file.read_at(&mut buf[read..], (start as u64) + read as u64) {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(_) => return None,
            }
        }
        buf.truncate(read);
        Some(buf)
    })
    .await
    .ok()??;

    let mut resp = if parts.method == Method::HEAD {
        StatusCode::PARTIAL_CONTENT.into_response()
    } else {
        (StatusCode::PARTIAL_CONTENT, body).into_response()
    };
    let headers = resp.headers_mut();
    if !art.content_type.is_empty()
        && let Ok(v) = HeaderValue::from_str(&art.content_type)
    {
        headers.insert(CONTENT_TYPE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{end}/{size}")) {
        headers.insert(http::header::CONTENT_RANGE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&itoa(end - start + 1)) {
        headers.insert(http::header::CONTENT_LENGTH, v);
    }
    Some(resp)
}

/// Parses a single `bytes=` range against a known size.
fn parse_single_range(header: &str, size: i64) -> Option<(i64, i64)> {
    let spec = header.strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let (start, end) = (start.trim(), end.trim());
    if start.is_empty() {
        // Suffix range: the last N bytes.
        let n: i64 = end.parse().ok()?;
        if n <= 0 {
            return None;
        }
        return Some(((size - n).max(0), size - 1));
    }
    let start: i64 = start.parse().ok()?;
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<i64>().ok()?.min(size - 1)
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::time::Duration as StdDuration;

    use axum::Router;
    use axum::body::Body;
    use chrono::{DateTime, Utc};
    use hex;
    use http::{Method, Request, StatusCode};
    use parking_lot::Mutex;
    use serde_json::json;
    use sha2::Digest as _;

    use crate::meta::{self, Repository, Store};
    use crate::repoconfig::{self, Config, Duration};

    use crate::repo::oci::{
        DOCKER_MEDIA_LIST, OCI_MEDIA_INDEX, OCI_MEDIA_MANIFEST, oci_blob_path, oci_manifest_path,
        parse_oci_path,
    };
    use crate::repo::oci_prune::OCI_PRUNE_GRACE;
    use crate::repo::{FetchSpec, Manager};
    use crate::testing::repo::{
        TestManager, TestResponse, call, mk_format_repo, mux, new_test_manager, send,
        spawn_upstream,
    };

    /// The `tags/list` response document, as the assertions read it.
    #[derive(serde::Deserialize, Default)]
    struct OciTagsDoc {
        #[serde(default)]
        name: String,
        #[serde(default)]
        tags: Vec<String>,
    }

    async fn mk_oci_repo(
        store: &Arc<Store>,
        name: &str,
        typ: &str,
        upstream: &str,
        cfg: Config,
    ) -> Repository {
        mk_format_repo(store, name, meta::FORMAT_OCI, typ, upstream, cfg).await
    }

    fn oci_digest_of(b: &[u8]) -> String {
        format!("sha256:{}", hex::encode(sha2::Sha256::digest(b)))
    }

    /// Pushes one blob monolithically and fails the test on error.
    async fn oci_push_blob(h: &Router, repo: &str, name: &str, body: &[u8]) -> String {
        let digest = oci_digest_of(body);
        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("/v2/{repo}/{name}/blobs/uploads/?digest={digest}"))
            .body(Body::from(body.to_vec()))
            .expect("build request");
        let resp = send(h, request).await;
        assert_eq!(
            resp.status,
            StatusCode::CREATED,
            "monolithic blob push body={}",
            resp.text()
        );
        digest
    }

    /// Builds a minimal valid image (config blob, one layer, manifest) and returns
    /// the manifest bytes with the digests of its parts.
    fn oci_image(config: &[u8], layer: &[u8]) -> (Vec<u8>, String, String) {
        let manifest = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": OCI_MEDIA_MANIFEST,
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": oci_digest_of(config),
                "size": config.len(),
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": oci_digest_of(layer),
                "size": layer.len(),
            }],
        }))
        .expect("encode manifest");
        (manifest, oci_digest_of(config), oci_digest_of(layer))
    }

    /// PUTs a manifest with its media type.
    async fn put_manifest(h: &Router, uri: &str, body: &[u8], media_type: &str) -> TestResponse {
        let request = Request::builder()
            .method(Method::PUT)
            .uri(uri)
            .header("Content-Type", media_type)
            .body(Body::from(body.to_vec()))
            .expect("build request");
        send(h, request).await
    }

    /// Pins the engine clock so a test can step past the prune grace window.
    fn pin_clock(tm: &TestManager, at: DateTime<Utc>) -> Arc<Mutex<DateTime<Utc>>> {
        let clock = Arc::new(Mutex::new(at));
        let handle = Arc::clone(&clock);
        tm.engine.set_now(Arc::new(move || *handle.lock()));
        clock
    }

    fn temp_upload_dir(m: &Arc<Manager>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        m.set_oci_upload_dir(&dir.path().to_string_lossy());
        dir
    }

    #[tokio::test]
    async fn oci_base_endpoint() {
        let tm = new_test_manager().await;
        let h = mux(&tm.manager);
        for path in ["/v2", "/v2/"] {
            let resp = call(&h, Method::GET, path, "").await;
            assert_eq!(resp.status, StatusCode::OK, "GET {path}");
            assert_eq!(
                resp.header("Docker-Distribution-API-Version"),
                "registry/2.0"
            );
        }
    }

    #[test]
    fn parse_oci_path_cases() {
        let sha = format!("sha256:{}", "a".repeat(64));
        let cases: Vec<(String, bool, &str, &str, String)> = vec![
            (
                "library/nginx/manifests/1.27".into(),
                true,
                "library/nginx",
                "manifests",
                "1.27".into(),
            ),
            (
                format!("library/nginx/blobs/{sha}"),
                true,
                "library/nginx",
                "blobs",
                sha.clone(),
            ),
            ("a/b/c/tags/list".into(), true, "a/b/c", "tags", "".into()),
            (
                "nginx/blobs/uploads/".into(),
                true,
                "nginx",
                "uploads",
                "".into(),
            ),
            (
                "nginx/blobs/uploads".into(),
                true,
                "nginx",
                "uploads",
                "".into(),
            ),
            (
                "nginx/blobs/uploads/0123456789abcdef0123456789abcdef".into(),
                true,
                "nginx",
                "uploads",
                "0123456789abcdef0123456789abcdef".into(),
            ),
            // A name may itself contain "blobs": the split anchors on the last
            // marker.
            (
                "blobs/manifests/manifests/v1".into(),
                true,
                "blobs/manifests",
                "manifests",
                "v1".into(),
            ),
            ("UPPER/manifests/v1".into(), false, "", "", "".into()),
            (
                format!("nginx/referrers/{sha}"),
                true,
                "nginx",
                "referrers",
                sha.clone(),
            ),
            ("nginx".into(), false, "", "", "".into()),
        ];
        for (wildcard, ok, name, endpoint, reference) in cases {
            let got = parse_oci_path(&wildcard);
            assert_eq!(got.is_some(), ok, "{wildcard:?}");
            if let Some(got) = got {
                assert_eq!(
                    (got.name.as_str(), got.endpoint, got.reference.as_str()),
                    (name, endpoint, reference.as_str()),
                    "{wildcard:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn oci_hosted_push_pull_round_trip() {
        let tm = new_test_manager().await;
        let _dir = temp_upload_dir(&tm.manager);
        mk_oci_repo(
            &tm.store,
            "oci-local",
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        let h = mux(&tm.manager);

        let (config, layer) = (br#"{"os":"linux"}"#.to_vec(), b"LAYERBYTES".to_vec());
        let (manifest, _, layer_digest) = oci_image(&config, &layer);

        // Manifest before its blobs must be refused.
        let resp = put_manifest(
            &h,
            "/v2/oci-local/app/manifests/v1",
            &manifest,
            OCI_MEDIA_MANIFEST,
        )
        .await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "early manifest");
        assert!(
            resp.text().contains("MANIFEST_BLOB_UNKNOWN"),
            "body={}",
            resp.text()
        );

        oci_push_blob(&h, "oci-local", "app", &config).await;

        // Layer via a chunked session split across two PATCHes.
        let resp = call(&h, Method::POST, "/v2/oci-local/app/blobs/uploads/", "").await;
        assert_eq!(resp.status, StatusCode::ACCEPTED, "open session");
        let loc = resp.header("Location");
        for (i, chunk) in ["LAYER", "BYTES"].iter().enumerate() {
            let request = Request::builder()
                .method(Method::PATCH)
                .uri(&loc)
                .header("Content-Range", format!("{}-{}", i * 5, i * 5 + 4))
                .body(Body::from(chunk.to_string()))
                .expect("build request");
            let resp = send(&h, request).await;
            assert_eq!(resp.status, StatusCode::ACCEPTED, "patch {i}");
        }
        // Out-of-order chunk is a 416.
        let request = Request::builder()
            .method(Method::PATCH)
            .uri(&loc)
            .header("Content-Range", "3-3")
            .body(Body::from("X"))
            .expect("build request");
        let resp = send(&h, request).await;
        assert_eq!(
            resp.status,
            StatusCode::RANGE_NOT_SATISFIABLE,
            "out-of-order patch"
        );
        let resp = call(&h, Method::PUT, &format!("{loc}?digest={layer_digest}"), "").await;
        assert_eq!(resp.status, StatusCode::CREATED, "finalize");

        // Manifest commit by tag.
        let resp = put_manifest(
            &h,
            "/v2/oci-local/app/manifests/v1",
            &manifest,
            OCI_MEDIA_MANIFEST,
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "manifest put");
        let manifest_digest = resp.header("Docker-Content-Digest");
        assert_eq!(manifest_digest, oci_digest_of(&manifest));

        // Pull by tag: byte-identical body, correct headers.
        let resp = call(&h, Method::GET, "/v2/oci-local/app/manifests/v1", "").await;
        assert_eq!(resp.status, StatusCode::OK, "manifest get");
        assert_eq!(resp.body.as_ref(), manifest.as_slice());
        assert_eq!(resp.header("Content-Type"), OCI_MEDIA_MANIFEST);
        assert_eq!(resp.header("Docker-Content-Digest"), manifest_digest);

        // HEAD by digest mirrors GET headers with no body.
        let resp = call(
            &h,
            Method::HEAD,
            &format!("/v2/oci-local/app/manifests/{manifest_digest}"),
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "manifest head");
        assert_eq!(resp.body.len(), 0);
        assert_eq!(resp.header("Docker-Content-Digest"), manifest_digest);

        // Blob pull, full and ranged.
        let resp = call(
            &h,
            Method::GET,
            &format!("/v2/oci-local/app/blobs/{layer_digest}"),
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "blob get");
        assert_eq!(resp.text(), "LAYERBYTES");
        let request = Request::builder()
            .method(Method::GET)
            .uri(format!("/v2/oci-local/app/blobs/{layer_digest}"))
            .header("Range", "bytes=5-9")
            .body(Body::empty())
            .expect("build request");
        let resp = send(&h, request).await;
        assert_eq!(resp.status, StatusCode::PARTIAL_CONTENT, "ranged blob get");
        assert_eq!(resp.text(), "BYTES");

        // Tags list.
        let resp = call(&h, Method::GET, "/v2/oci-local/app/tags/list", "").await;
        let tags: OciTagsDoc = serde_json::from_slice(&resp.body).expect("tags list");
        assert_eq!(tags.name, "app");
        assert_eq!(tags.tags, vec!["v1".to_string()]);

        // Cross-repository (cross-name) blob mount.
        let resp = call(
            &h,
            Method::POST,
            &format!("/v2/oci-local/app2/blobs/uploads/?mount={layer_digest}&from=app"),
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "mount");
        let resp = call(
            &h,
            Method::GET,
            &format!("/v2/oci-local/app2/blobs/{layer_digest}"),
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "mounted blob get");
        assert_eq!(resp.text(), "LAYERBYTES");

        // DELETE by tag removes only the tag; the manifest stays pullable by digest.
        let resp = call(&h, Method::DELETE, "/v2/oci-local/app/manifests/v1", "").await;
        assert_eq!(resp.status, StatusCode::ACCEPTED, "delete tag");
        let resp = call(
            &h,
            Method::GET,
            &format!("/v2/oci-local/app/manifests/{manifest_digest}"),
            "",
        )
        .await;
        assert_eq!(
            resp.status,
            StatusCode::OK,
            "manifest by digest after tag delete"
        );
        let resp = call(&h, Method::GET, "/v2/oci-local/app/manifests/v1", "").await;
        assert_eq!(
            resp.status,
            StatusCode::NOT_FOUND,
            "manifest by deleted tag"
        );

        // DELETE by digest removes the manifest row.
        let resp = call(
            &h,
            Method::DELETE,
            &format!("/v2/oci-local/app/manifests/{manifest_digest}"),
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::ACCEPTED, "delete manifest");
        let resp = call(
            &h,
            Method::GET,
            &format!("/v2/oci-local/app/manifests/{manifest_digest}"),
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::NOT_FOUND, "deleted manifest get");
    }

    #[tokio::test]
    async fn oci_finalize_digest_mismatch_stores_nothing() {
        let tm = new_test_manager().await;
        let _dir = temp_upload_dir(&tm.manager);
        let repo = mk_oci_repo(
            &tm.store,
            "oci-local",
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        let h = mux(&tm.manager);

        let wrong = oci_digest_of(b"other bytes");
        let resp = call(
            &h,
            Method::POST,
            &format!("/v2/oci-local/app/blobs/uploads/?digest={wrong}"),
            "REAL",
        )
        .await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "mismatched push");
        assert!(
            resp.text().contains("DIGEST_INVALID"),
            "body={}",
            resp.text()
        );
        assert!(
            tm.store
                .get_artifact(repo.id, &oci_blob_path("app", &wrong))
                .await
                .is_err(),
            "mismatched blob was recorded"
        );
    }

    #[tokio::test]
    async fn oci_index_push_requires_children() {
        let tm = new_test_manager().await;
        let _dir = temp_upload_dir(&tm.manager);
        mk_oci_repo(
            &tm.store,
            "oci-local",
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        let h = mux(&tm.manager);

        let (config, layer) = (b"{}".to_vec(), b"L".to_vec());
        let (manifest, _, _) = oci_image(&config, &layer);
        oci_push_blob(&h, "oci-local", "app", &config).await;
        oci_push_blob(&h, "oci-local", "app", &layer).await;
        let resp = put_manifest(
            &h,
            &format!("/v2/oci-local/app/manifests/{}", oci_digest_of(&manifest)),
            &manifest,
            OCI_MEDIA_MANIFEST,
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "child manifest put");

        let index = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": OCI_MEDIA_INDEX,
            "manifests": [{
                "mediaType": OCI_MEDIA_MANIFEST,
                "digest": oci_digest_of(&manifest),
                "size": manifest.len(),
                "platform": {"os": "linux", "architecture": "amd64"},
            }],
        }))
        .expect("encode index");
        let resp = put_manifest(
            &h,
            "/v2/oci-local/app/manifests/multi",
            &index,
            OCI_MEDIA_INDEX,
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "index put");

        // An index referencing an absent child is refused.
        let missing = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": OCI_MEDIA_INDEX,
            "manifests": [{
                "mediaType": OCI_MEDIA_MANIFEST,
                "digest": oci_digest_of(b"nope"),
                "size": 4,
            }],
        }))
        .expect("encode index");
        let resp = put_manifest(
            &h,
            "/v2/oci-local/app/manifests/broken",
            &missing,
            OCI_MEDIA_INDEX,
        )
        .await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "broken index put");
        assert!(
            resp.text().contains("MANIFEST_BLOB_UNKNOWN"),
            "body={}",
            resp.text()
        );
        // `DOCKER_MEDIA_LIST` is the other index media type the push accepts.
        assert!(crate::repo::oci::oci_index_media_type(DOCKER_MEDIA_LIST));
    }

    #[tokio::test]
    async fn oci_group_resolves_members_and_merges_tags() {
        let tm = new_test_manager().await;
        let _dir = temp_upload_dir(&tm.manager);
        mk_oci_repo(
            &tm.store,
            "oci-hosted",
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        let h = mux(&tm.manager);

        let (config, layer) = (b"{}".to_vec(), b"GROUPLAYER".to_vec());
        let (manifest, _, layer_digest) = oci_image(&config, &layer);
        oci_push_blob(&h, "oci-hosted", "app", &config).await;
        oci_push_blob(&h, "oci-hosted", "app", &layer).await;
        let resp = put_manifest(
            &h,
            "/v2/oci-hosted/app/manifests/hosted-tag",
            &manifest,
            OCI_MEDIA_MANIFEST,
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "hosted manifest put");

        let mut cfg = repoconfig::default();
        cfg.group.members = vec!["oci-hosted".to_string()];
        mk_oci_repo(&tm.store, "oci-public", meta::TYPE_GROUP, "", cfg).await;

        // Manifest and blob resolve through the group.
        let resp = call(
            &h,
            Method::GET,
            "/v2/oci-public/app/manifests/hosted-tag",
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "group manifest");
        assert_eq!(resp.body.as_ref(), manifest.as_slice());
        let resp = call(
            &h,
            Method::GET,
            &format!("/v2/oci-public/app/blobs/{layer_digest}"),
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "group blob");
        assert_eq!(resp.text(), "GROUPLAYER");

        // Merged tags list.
        let resp = call(&h, Method::GET, "/v2/oci-public/app/tags/list", "").await;
        let tags: OciTagsDoc = serde_json::from_slice(&resp.body).expect("group tags");
        assert_eq!(tags.tags, vec!["hosted-tag".to_string()]);

        // Writes through a group are refused.
        let resp = call(&h, Method::POST, "/v2/oci-public/app/blobs/uploads/", "").await;
        assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED, "group push");
    }

    #[tokio::test]
    async fn oci_prune_collects_unreachable_keeps_tagged() {
        let tm = new_test_manager().await;
        let _dir = temp_upload_dir(&tm.manager);
        let repo = mk_oci_repo(
            &tm.store,
            "oci-local",
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        let h = mux(&tm.manager);

        let (keep_config, keep_layer) = (br#"{"keep":1}"#.to_vec(), b"KEEPLAYER".to_vec());
        let (keep_manifest, keep_config_digest, keep_layer_digest) =
            oci_image(&keep_config, &keep_layer);
        oci_push_blob(&h, "oci-local", "app", &keep_config).await;
        oci_push_blob(&h, "oci-local", "app", &keep_layer).await;
        let resp = put_manifest(
            &h,
            "/v2/oci-local/app/manifests/keep",
            &keep_manifest,
            OCI_MEDIA_MANIFEST,
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "keep manifest");

        // An orphaned blob (its manifest never arrived) and an untagged manifest.
        let orphan = b"ORPHANBYTES".to_vec();
        oci_push_blob(&h, "oci-local", "app", &orphan).await;
        let (gone_config, gone_layer) = (br#"{"gone":1}"#.to_vec(), b"GONELAYER".to_vec());
        let (gone_manifest, _, _) = oci_image(&gone_config, &gone_layer);
        oci_push_blob(&h, "oci-local", "app", &gone_config).await;
        oci_push_blob(&h, "oci-local", "app", &gone_layer).await;
        let resp = put_manifest(
            &h,
            "/v2/oci-local/app/manifests/gone",
            &gone_manifest,
            OCI_MEDIA_MANIFEST,
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "gone manifest");
        let resp = call(&h, Method::DELETE, "/v2/oci-local/app/manifests/gone", "").await;
        assert_eq!(resp.status, StatusCode::ACCEPTED, "untag");

        // Inside the grace window nothing may be deleted.
        let n = tm
            .manager
            .prune_oci_once(StdDuration::from_secs(3600))
            .await
            .expect("prune within grace");
        assert_eq!(n, 0, "prune within grace");

        pin_clock(
            &tm,
            Utc::now()
                + chrono::TimeDelta::from_std(OCI_PRUNE_GRACE).expect("grace")
                + chrono::TimeDelta::hours(1),
        );
        tm.manager
            .prune_oci_once(StdDuration::from_secs(3600))
            .await
            .expect("prune");
        for path in [
            oci_manifest_path("app", &oci_digest_of(&keep_manifest)),
            oci_blob_path("app", &keep_config_digest),
            oci_blob_path("app", &keep_layer_digest),
        ] {
            tm.store
                .get_artifact(repo.id, &path)
                .await
                .unwrap_or_else(|e| panic!("tagged image lost {path}: {e}"));
        }
        for path in [
            oci_manifest_path("app", &oci_digest_of(&gone_manifest)),
            oci_blob_path("app", &oci_digest_of(&orphan)),
            oci_blob_path("app", &oci_digest_of(&gone_config)),
            oci_blob_path("app", &oci_digest_of(&gone_layer)),
        ] {
            assert!(
                tm.store.get_artifact(repo.id, &path).await.is_err(),
                "unreachable object survived: {path}"
            );
        }

        // The tagged image is still pullable end to end.
        let resp = call(&h, Method::GET, "/v2/oci-local/app/manifests/keep", "").await;
        assert_eq!(resp.status, StatusCode::OK, "keep pull after prune");
    }

    #[tokio::test]
    async fn oci_reaper_and_eviction_skip() {
        let tm = new_test_manager().await;
        let _dir = temp_upload_dir(&tm.manager);
        let mut cfg = repoconfig::default();
        cfg.retention.idle_ttl = Duration(60 * 1_000_000_000);
        // Absurdly small: eviction would delete everything.
        cfg.cache.max_size_bytes = 1;
        let repo = mk_oci_repo(&tm.store, "oci-local", meta::TYPE_HOSTED, "", cfg.clone()).await;
        let h = mux(&tm.manager);

        let (config, layer) = (b"{}".to_vec(), b"EVICTLAYER".to_vec());
        let (manifest, _, layer_digest) = oci_image(&config, &layer);
        oci_push_blob(&h, "oci-local", "app", &config).await;
        oci_push_blob(&h, "oci-local", "app", &layer).await;
        let resp = put_manifest(
            &h,
            "/v2/oci-local/app/manifests/v1",
            &manifest,
            OCI_MEDIA_MANIFEST,
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "manifest put");

        // Idle far past the TTL; the reaper must skip the OCI repository entirely.
        pin_clock(&tm, Utc::now() + chrono::TimeDelta::hours(48));
        tm.manager.reap_once().await.expect("reap");
        tm.engine
            .maybe_evict(&FetchSpec {
                repo: repo.clone(),
                cfg,
                ..FetchSpec::blank()
            })
            .await;
        for path in [
            oci_manifest_path("app", &oci_digest_of(&manifest)),
            oci_blob_path("app", &layer_digest),
        ] {
            tm.store
                .get_artifact(repo.id, &path)
                .await
                .unwrap_or_else(|e| {
                    panic!("OCI artifact deleted by reaper/eviction: {path} ({e})")
                });
        }
    }

    #[tokio::test]
    async fn oci_approval_gate_blocks_manifest_pull() {
        let tm = new_test_manager().await;
        let _dir = temp_upload_dir(&tm.manager);
        let mut cfg = repoconfig::default();
        cfg.approval.enabled = true;
        cfg.approval.mode = "enforce".to_string();
        mk_oci_repo(&tm.store, "oci-local", meta::TYPE_HOSTED, "", cfg).await;
        let h = mux(&tm.manager);

        let (config, layer) = (b"{}".to_vec(), b"GATED".to_vec());
        let (manifest, _, _) = oci_image(&config, &layer);
        oci_push_blob(&h, "oci-local", "app", &config).await;
        oci_push_blob(&h, "oci-local", "app", &layer).await;
        let resp = put_manifest(
            &h,
            "/v2/oci-local/app/manifests/v1",
            &manifest,
            OCI_MEDIA_MANIFEST,
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "manifest put");

        // First pull is quarantined pending approval (the approval gate runs after
        // age, immediately before the stored manifest would be served).
        let resp = call(&h, Method::GET, "/v2/oci-local/app/manifests/v1", "").await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "gated pull");

        // Approve, then the pull succeeds.
        tm.store
            .upsert_approval_decision("oci-local", "app", meta::APPROVAL_APPROVED, "tester", "")
            .await
            .expect("approve");
        let resp = call(&h, Method::GET, "/v2/oci-local/app/manifests/v1", "").await;
        assert_eq!(resp.status, StatusCode::OK, "approved pull");
    }

    #[tokio::test]
    async fn oci_version_deny_gate_blocks_tag() {
        let tm = new_test_manager().await;
        let _dir = temp_upload_dir(&tm.manager);
        mk_oci_repo(
            &tm.store,
            "oci-local",
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        let h = mux(&tm.manager);

        let (config, layer) = (b"{}".to_vec(), b"DENIED".to_vec());
        let (manifest, _, _) = oci_image(&config, &layer);
        oci_push_blob(&h, "oci-local", "app", &config).await;
        oci_push_blob(&h, "oci-local", "app", &layer).await;
        let resp = put_manifest(
            &h,
            "/v2/oci-local/app/manifests/bad-tag",
            &manifest,
            OCI_MEDIA_MANIFEST,
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "manifest put");
        tm.store
            .upsert_version_deny("oci-local", "app", "bad-tag", "CVE", "tester")
            .await
            .expect("deny");
        let resp = call(&h, Method::GET, "/v2/oci-local/app/manifests/bad-tag", "").await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "denied tag pull");
    }

    #[tokio::test]
    async fn oci_invalid_pull_reference_is_404() {
        let tm = new_test_manager().await;
        mk_oci_repo(
            &tm.store,
            "oci-local",
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        let h = mux(&tm.manager);
        // The conformance suite pulls ".INVALID_MANIFEST_NAME": a malformed
        // reference on pull is an unknown manifest, not a client error.
        let resp = call(
            &h,
            Method::GET,
            "/v2/oci-local/app/manifests/.INVALID_MANIFEST_NAME",
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::NOT_FOUND, "invalid pull reference");
        // The same reference on push stays a client error.
        let resp = call(
            &h,
            Method::PUT,
            "/v2/oci-local/app/manifests/.INVALID_MANIFEST_NAME",
            "{}",
        )
        .await;
        assert_eq!(
            resp.status,
            StatusCode::BAD_REQUEST,
            "invalid push reference"
        );
    }

    #[tokio::test]
    async fn oci_referrers_list_and_prune_liveness() {
        let tm = new_test_manager().await;
        let _dir = temp_upload_dir(&tm.manager);
        let repo = mk_oci_repo(
            &tm.store,
            "oci-local",
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        let h = mux(&tm.manager);

        // Tagged image.
        let (config, layer) = (b"{}".to_vec(), b"SUBJECTLAYER".to_vec());
        let (manifest, _, _) = oci_image(&config, &layer);
        oci_push_blob(&h, "oci-local", "app", &config).await;
        oci_push_blob(&h, "oci-local", "app", &layer).await;
        let resp = put_manifest(
            &h,
            "/v2/oci-local/app/manifests/v1",
            &manifest,
            OCI_MEDIA_MANIFEST,
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "image put");
        let subject_digest = oci_digest_of(&manifest);

        // Referrer (e.g. an SBOM) pushed by digest with subject set.
        let sbom_blob = b"SBOMDATA".to_vec();
        oci_push_blob(&h, "oci-local", "app", &sbom_blob).await;
        let referrer = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": OCI_MEDIA_MANIFEST,
            "artifactType": "application/spdx+json",
            "config": {
                "mediaType": "application/vnd.oci.empty.v1+json",
                "digest": oci_digest_of(&sbom_blob),
                "size": sbom_blob.len(),
            },
            "layers": [{
                "mediaType": "application/spdx+json",
                "digest": oci_digest_of(&sbom_blob),
                "size": sbom_blob.len(),
            }],
            "subject": {
                "mediaType": OCI_MEDIA_MANIFEST,
                "digest": subject_digest,
                "size": manifest.len(),
            },
        }))
        .expect("encode referrer");
        let referrer_digest = oci_digest_of(&referrer);
        let resp = put_manifest(
            &h,
            &format!("/v2/oci-local/app/manifests/{referrer_digest}"),
            &referrer,
            OCI_MEDIA_MANIFEST,
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "referrer put");
        assert_eq!(resp.header("OCI-Subject"), subject_digest);

        // Referrers listing includes the SBOM; artifactType filter applies.
        let resp = call(
            &h,
            Method::GET,
            &format!("/v2/oci-local/app/referrers/{subject_digest}"),
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "referrers");
        assert!(
            resp.text().contains(&referrer_digest),
            "body={}",
            resp.text()
        );
        let resp = call(
            &h,
            Method::GET,
            &format!("/v2/oci-local/app/referrers/{subject_digest}?artifactType=application/other"),
            "",
        )
        .await;
        assert!(
            !resp.text().contains(&referrer_digest),
            "filter leaked referrer: {}",
            resp.text()
        );
        // Unreferenced digest: empty index, still 200.
        let resp = call(
            &h,
            Method::GET,
            &format!("/v2/oci-local/app/referrers/{}", oci_digest_of(b"nothing")),
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "empty referrers");
        assert!(
            resp.text().contains(r#""manifests":[]"#),
            "body={}",
            resp.text()
        );

        // Prune keeps the untagged referrer while its subject is tagged.
        pin_clock(
            &tm,
            Utc::now()
                + chrono::TimeDelta::from_std(OCI_PRUNE_GRACE).expect("grace")
                + chrono::TimeDelta::hours(1),
        );
        tm.manager
            .prune_oci_once(StdDuration::from_secs(3600))
            .await
            .expect("prune");
        tm.store
            .get_artifact(repo.id, &oci_manifest_path("app", &referrer_digest))
            .await
            .expect("referrer pruned while subject tagged");

        // Untag the image: subject and referrer both become collectable.
        let resp = call(&h, Method::DELETE, "/v2/oci-local/app/manifests/v1", "").await;
        assert_eq!(resp.status, StatusCode::ACCEPTED, "untag");
        tm.manager
            .prune_oci_once(StdDuration::from_secs(3600))
            .await
            .expect("prune 2");
        assert!(
            tm.store
                .get_artifact(repo.id, &oci_manifest_path("app", &referrer_digest))
                .await
                .is_err(),
            "referrer survived after subject untagged"
        );
    }

    #[tokio::test]
    async fn oci_list_tags_and_detail() {
        let tm = new_test_manager().await;
        let _dir = temp_upload_dir(&tm.manager);
        let repo = mk_oci_repo(
            &tm.store,
            "oci-local",
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        let h = mux(&tm.manager);

        let config = br#"{"os":"linux","architecture":"arm64","created":"2026-01-02T03:04:05Z","config":{"Entrypoint":["/app"],"Cmd":["serve"],"Env":["A=1"]}}"#.to_vec();
        let layer = b"DETAILLAYER".to_vec();
        let (manifest, _, _) = oci_image(&config, &layer);
        oci_push_blob(&h, "oci-local", "app", &config).await;
        oci_push_blob(&h, "oci-local", "app", &layer).await;
        let resp = put_manifest(
            &h,
            "/v2/oci-local/app/manifests/v1",
            &manifest,
            OCI_MEDIA_MANIFEST,
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "manifest put");

        let tags = tm.manager.list_oci_tags(repo.id).await.expect("tags");
        assert_eq!(tags.len(), 1);
        let row = &tags[0];
        assert_eq!(row.name, "app");
        assert_eq!(row.tag, "v1");
        assert_eq!(row.kind, "image");
        assert!(row.size > 0, "row = {row:?}");
        // The list view derives platforms only for indexes; single images report
        // none.
        assert!(row.platforms.is_empty(), "platforms = {:?}", row.platforms);

        let detail = tm
            .manager
            .oci_artifact_detail(repo.id, "app", "v1")
            .await
            .expect("detail");
        assert_eq!(detail.info.kind, "image");
        let image = detail.image.as_ref().expect("image summary");
        assert_eq!(image.os, "linux");
        assert_eq!(image.architecture, "arm64");
        assert_eq!(image.entrypoint.len(), 1);
        assert_eq!(image.layers, 1);
        assert!(!detail.manifest_json.get().is_empty(), "raw manifest");
        assert!(
            detail
                .config_json
                .as_ref()
                .is_some_and(|c| !c.get().is_empty()),
            "raw config"
        );

        // Digest reference resolves too.
        let by_digest = tm
            .manager
            .oci_artifact_detail(repo.id, "app", &detail.info.digest)
            .await
            .expect("by digest");
        assert_eq!(by_digest.info.digest, detail.info.digest);

        // Unknown tag is not found.
        assert!(
            tm.manager
                .oci_artifact_detail(repo.id, "app", "missing")
                .await
                .is_err(),
            "missing tag resolved"
        );
    }

    #[tokio::test]
    async fn oci_chart_additions_extraction() {
        let tm = new_test_manager().await;
        let _dir = temp_upload_dir(&tm.manager);
        let repo = mk_oci_repo(
            &tm.store,
            "oci-local",
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        let h = mux(&tm.manager);

        // Build a minimal chart archive: <name>/values.yaml + README.md + a subchart
        // file that must NOT shadow the root ones.
        let chart_bytes = {
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            {
                let mut tw = tar::Builder::new(&mut gz);
                for (name, body) in [
                    ("mychart/values.yaml", "replicaCount: 2\n"),
                    ("mychart/README.md", "# mychart\n"),
                    ("mychart/charts/sub/values.yaml", "shadow: true\n"),
                ] {
                    let mut header = tar::Header::new_gnu();
                    header.set_size(body.len() as u64);
                    header.set_mode(0o644);
                    header.set_cksum();
                    tw.append_data(&mut header, name, body.as_bytes())
                        .expect("tar entry");
                }
                tw.finish().expect("tar finish");
            }
            gz.finish().expect("gzip finish")
        };

        let chart_config = br#"{"name":"mychart","version":"0.1.0"}"#.to_vec();
        oci_push_blob(&h, "oci-local", "mychart", &chart_config).await;
        oci_push_blob(&h, "oci-local", "mychart", &chart_bytes).await;
        let manifest = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": OCI_MEDIA_MANIFEST,
            "config": {
                "mediaType": "application/vnd.cncf.helm.config.v1+json",
                "digest": oci_digest_of(&chart_config),
                "size": chart_config.len(),
            },
            "layers": [{
                "mediaType": "application/vnd.cncf.helm.chart.content.v1.tar+gzip",
                "digest": oci_digest_of(&chart_bytes),
                "size": chart_bytes.len(),
            }],
        }))
        .expect("encode manifest");
        let resp = put_manifest(
            &h,
            "/v2/oci-local/mychart/manifests/0.1.0",
            &manifest,
            OCI_MEDIA_MANIFEST,
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "chart manifest put");

        let detail = tm
            .manager
            .oci_artifact_detail(repo.id, "mychart", "0.1.0")
            .await
            .expect("detail");
        assert_eq!(detail.info.kind, "chart");
        let chart = detail.chart.as_ref().expect("chart additions");
        assert_eq!(chart.values_yaml, "replicaCount: 2\n");
        assert_eq!(chart.readme_md, "# mychart\n");
    }

    #[tokio::test]
    async fn oci_endpoint_branches() {
        let tm = new_test_manager().await;
        let _dir = temp_upload_dir(&tm.manager);
        mk_oci_repo(
            &tm.store,
            "oci-local",
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        let h = mux(&tm.manager);

        // Tags list pagination: n + last + Link header.
        let blob = b"B".to_vec();
        oci_push_blob(&h, "oci-local", "app", &blob).await;
        let man = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": OCI_MEDIA_MANIFEST,
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": oci_digest_of(&blob),
                "size": 1,
            },
            "layers": [],
        }))
        .expect("encode manifest");
        for tag in ["a", "b", "c"] {
            let resp = put_manifest(
                &h,
                &format!("/v2/oci-local/app/manifests/{tag}"),
                &man,
                OCI_MEDIA_MANIFEST,
            )
            .await;
            assert_eq!(resp.status, StatusCode::CREATED, "tag {tag}");
        }
        let resp = call(&h, Method::GET, "/v2/oci-local/app/tags/list?n=2", "").await;
        let page: OciTagsDoc = serde_json::from_slice(&resp.body).expect("page1");
        assert_eq!(page.tags.len(), 2, "page1 = {:?}", page.tags);
        assert!(!resp.header("Link").is_empty(), "link header");
        let resp = call(
            &h,
            Method::GET,
            "/v2/oci-local/app/tags/list?n=2&last=b",
            "",
        )
        .await;
        let page: OciTagsDoc = serde_json::from_slice(&resp.body).expect("page2");
        assert_eq!(page.tags, vec!["c".to_string()], "page2");

        // Blob endpoints: invalid digest, DELETE, then 404 on the deleted blob.
        let resp = call(&h, Method::GET, "/v2/oci-local/app/blobs/sha256:zz", "").await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "bad digest");
        let blob_digest = oci_digest_of(&blob);
        let resp = call(
            &h,
            Method::DELETE,
            &format!("/v2/oci-local/app/blobs/{blob_digest}"),
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::ACCEPTED, "blob delete");
        let resp = call(
            &h,
            Method::DELETE,
            &format!("/v2/oci-local/app/blobs/{blob_digest}"),
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::NOT_FOUND, "deleted blob delete");

        // Upload session: open, status, cancel, then unknown.
        let resp = call(&h, Method::POST, "/v2/oci-local/app/blobs/uploads/", "").await;
        let loc = resp.header("Location");
        let resp = call(&h, Method::GET, &loc, "").await;
        assert_eq!(resp.status, StatusCode::NO_CONTENT, "status");
        assert_eq!(resp.header("Range"), "0-0");
        let resp = call(&h, Method::DELETE, &loc, "").await;
        assert_eq!(resp.status, StatusCode::NO_CONTENT, "cancel");
        let resp = call(&h, Method::GET, &loc, "").await;
        assert_eq!(resp.status, StatusCode::NOT_FOUND, "cancelled status");

        // Session expiry through the prune pass.
        call(&h, Method::POST, "/v2/oci-local/app/blobs/uploads/", "").await;
        pin_clock(&tm, Utc::now() + chrono::TimeDelta::hours(48));
        tm.manager
            .expire_oci_sessions(StdDuration::from_secs(3600))
            .await
            .expect("expire");
        assert_eq!(
            tm.store
                .count_oci_upload_sessions()
                .await
                .expect("count sessions"),
            0,
            "sessions after expiry"
        );
    }

    /// Fakes a token-protected registry: every request without the expected bearer
    /// token gets a 401 challenge pointing at its own `/token` endpoint, mirroring
    /// Docker Hub/GHCR behavior.
    async fn new_oci_upstream(objects: Vec<(String, Vec<u8>, String)>) -> String {
        use axum::extract::Request as AxumRequest;
        use axum::response::IntoResponse;

        let base: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let handle = Arc::clone(&base);
        let table: Arc<Vec<(String, Vec<u8>, String)>> = Arc::new(objects);
        let url = spawn_upstream(Router::new().fallback(move |req: AxumRequest| {
        let handle = Arc::clone(&handle);
        let table = Arc::clone(&table);
        async move {
            let path = req.uri().path().to_string();
            if path == "/token" {
                return (
                    [("Content-Type", "application/json")],
                    r#"{"token":"TESTTOKEN","expires_in":300}"#,
                )
                    .into_response();
            }
            let authorized = req
                .headers()
                .get("Authorization")
                .and_then(|v| v.to_str().ok())
                == Some("Bearer TESTTOKEN");
            if !authorized {
                let realm = handle.lock().clone();
                return (
                    StatusCode::UNAUTHORIZED,
                    [(
                        "WWW-Authenticate",
                        format!(
                            "Bearer realm=\"{realm}/token\",service=\"test\",scope=\"repository:x:pull\""
                        ),
                    )],
                )
                    .into_response();
            }
            match table.iter().find(|(p, _, _)| *p == path) {
                Some((_, body, ct)) => (
                    [
                        ("Content-Type", ct.clone()),
                        ("Docker-Content-Digest", oci_digest_of(body)),
                    ],
                    body.clone(),
                )
                    .into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }
    }))
    .await;
        *base.lock() = url.clone();
        url
    }

    #[tokio::test]
    async fn oci_proxy_pull_with_token_handshake() {
        let (config, layer) = (br#"{"os":"linux"}"#.to_vec(), b"PROXYLAYER".to_vec());
        let (manifest, config_digest, layer_digest) = oci_image(&config, &layer);
        // Single-segment name: the proxy must apply the `library/` rewrite only for
        // docker.io hosts, so this fake (non-docker.io) uses the name as-is.
        let upstream = new_oci_upstream(vec![
            (
                "/v2/app/manifests/v1".to_string(),
                manifest.clone(),
                OCI_MEDIA_MANIFEST.to_string(),
            ),
            (
                format!("/v2/app/manifests/{}", oci_digest_of(&manifest)),
                manifest.clone(),
                OCI_MEDIA_MANIFEST.to_string(),
            ),
            (
                format!("/v2/app/blobs/{config_digest}"),
                config.clone(),
                "application/octet-stream".to_string(),
            ),
            (
                format!("/v2/app/blobs/{layer_digest}"),
                layer.clone(),
                "application/octet-stream".to_string(),
            ),
            (
                "/v2/app/tags/list".to_string(),
                br#"{"name":"app","tags":["v1","v2"]}"#.to_vec(),
                "application/json".to_string(),
            ),
        ])
        .await;

        let tm = new_test_manager().await;
        let repo = mk_oci_repo(
            &tm.store,
            "oci-proxy",
            meta::TYPE_PROXY,
            &upstream,
            repoconfig::default(),
        )
        .await;
        let h = mux(&tm.manager);

        // Manifest by tag: fetched, digest-verified, cached, tag row recorded.
        let resp = call(&h, Method::GET, "/v2/oci-proxy/app/manifests/v1", "").await;
        assert_eq!(resp.status, StatusCode::OK, "proxy manifest");
        assert_eq!(resp.body.as_ref(), manifest.as_slice());
        let tag = tm
            .store
            .get_oci_tag(repo.id, "app", "v1")
            .await
            .expect("tag row");
        assert_eq!(tag.manifest_digest, oci_digest_of(&manifest));

        // Blob: fetched and cached; second pull is a cache hit (upstream not
        // needed).
        for _ in 0..2 {
            let resp = call(
                &h,
                Method::GET,
                &format!("/v2/oci-proxy/app/blobs/{layer_digest}"),
                "",
            )
            .await;
            assert_eq!(resp.status, StatusCode::OK, "proxy blob");
            assert_eq!(resp.text(), "PROXYLAYER");
        }
        tm.store
            .get_artifact(repo.id, &oci_blob_path("app", &layer_digest))
            .await
            .expect("blob not cached");

        // Tag list passthrough.
        let resp = call(&h, Method::GET, "/v2/oci-proxy/app/tags/list", "").await;
        assert_eq!(resp.status, StatusCode::OK, "proxy tags");
        assert!(resp.text().contains("\"v2\""), "body={}", resp.text());
    }

    #[tokio::test]
    async fn oci_upstream_name_docker_hub_library_prefix() {
        let tm = new_test_manager().await;
        let mut res = crate::repo::router::Resolved {
            repo: Repository {
                upstream_url: "https://registry-1.docker.io".to_string(),
                ..Default::default()
            },
            cfg: repoconfig::default(),
            path: String::new(),
        };
        assert_eq!(
            crate::repo::oci_proxy::oci_upstream_name(&res, "nginx"),
            "library/nginx",
            "docker hub single segment"
        );
        assert_eq!(
            crate::repo::oci_proxy::oci_upstream_name(&res, "grafana/grafana"),
            "grafana/grafana",
            "docker hub two segments"
        );
        res.repo.upstream_url = "https://ghcr.io".to_string();
        assert_eq!(
            crate::repo::oci_proxy::oci_upstream_name(&res, "nginx"),
            "nginx",
            "ghcr single segment"
        );
        drop(tm);
    }
}
