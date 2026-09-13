//! Public OCI registries (Docker Hub, GHCR, Quay) answer anonymous or basic
//! requests with a 401 carrying a Bearer challenge; the client is expected to
//! exchange it at the named realm for a short-lived scoped token. This file
//! implements that handshake for the proxy path, with a per-(repo, scope) token
//! cache so a layer-by-layer pull performs one exchange, not one per request.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use http::header::CONTENT_TYPE;
use http::request::Parts;
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use sha2::Digest as _;
use url::Url;

use crate::meta::{self, Artifact};
use crate::repoconfig::{Config, UPSTREAM_AUTH_BASIC};

use super::maven::last_modified;
use super::oci::{
    OCI_DIGEST_RE, OCI_MANIFEST_ACCEPT, OciRequest, oci_blob_path, oci_manifest_media_type,
    oci_manifest_path, write_oci_error,
};
use super::router::Resolved;
use super::{
    FetchKind, FetchOutcome, FetchSpec, MAX_METADATA_BYTES, Manager, header_str, itoa,
    parse_retry_after, retry_after_seconds, username_from_context,
};

/// The upstream request could not be completed (transport error, or a token exchange that never
/// produced a usable token).
pub(crate) struct UpstreamFailed;

/// One cached upstream bearer token.
#[derive(Clone)]
struct OciToken {
    token: String,
    expires: DateTime<Utc>,
}

/// Caches upstream registry tokens keyed by repo id + scope.
pub(crate) struct OciTokenCache {
    tokens: parking_lot::Mutex<HashMap<String, OciToken>>,
}

impl OciTokenCache {
    pub(crate) fn new() -> OciTokenCache {
        OciTokenCache {
            tokens: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    fn get(&self, key: &str, now: DateTime<Utc>) -> Option<String> {
        let tokens = self.tokens.lock();
        let t = tokens.get(key)?;
        (now <= t.expires).then(|| t.token.clone())
    }

    fn set(&self, key: &str, token: String, expires: DateTime<Utc>) {
        self.tokens
            .lock()
            .insert(key.to_string(), OciToken { token, expires });
    }
}

/// Maps a Forklift-side OCI name to the upstream one. Docker Hub resolves
/// single-segment names under `library/` — a normalization the docker client
/// applies only for the docker.io hostname, so the proxy must reproduce it for
/// its own upstream requests.
pub(crate) fn oci_upstream_name(res: &Resolved, name: &str) -> String {
    let host = Url::parse(&res.repo.upstream_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    if (host == "registry-1.docker.io" || host == "docker.io" || host.ends_with(".docker.io"))
        && !name.contains('/')
    {
        return format!("library/{name}");
    }
    name.to_string()
}

/// Builds the upstream distribution-API URL for a path suffix. The configured
/// upstream is the registry base (e.g. `https://registry-1.docker.io`); `/v2`
/// is appended here, so admins configure the registry address the way every
/// other tool asks for it.
fn oci_upstream_url(res: &Resolved, name: &str, suffix: &str) -> String {
    let mut base = res.repo.upstream_url.trim_end_matches('/').to_string();
    if !base.ends_with("/v2") {
        base.push_str("/v2");
    }
    format!("{base}/{}/{suffix}", oci_upstream_name(res, name))
}

impl Manager {
    /// Performs one upstream request with the bearer handshake: use a cached
    /// token when present, and on a 401 Bearer challenge exchange the
    /// repository's static credentials (or nothing) at the realm and retry once.
    /// Static basic/bearer/header credentials configured on the repository are
    /// presented when no token flow is in play, preserving plain-registry
    /// behavior.
    async fn oci_upstream_do(
        &self,
        res: &Resolved,
        method: Method,
        raw_url: &str,
        accept: &str,
    ) -> Result<reqwest::Response, UpstreamFailed> {
        let build = |token: &str| -> HeaderMap {
            let mut headers = HeaderMap::new();
            if let Ok(v) = HeaderValue::from_str(&self.engine.user_agent) {
                headers.insert(http::header::USER_AGENT, v);
            }
            if !accept.is_empty()
                && let Ok(v) = HeaderValue::from_str(accept)
            {
                headers.insert(http::header::ACCEPT, v);
            }
            if !token.is_empty() {
                if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
                    headers.insert(http::header::AUTHORIZATION, v);
                }
            } else {
                let (auth_headers, _) = self.engine.new_upstream_request(&res.cfg.upstream_auth);
                headers.extend(auth_headers);
            }
            headers
        };

        let cache_key = oci_token_key(res.repo.id, raw_url);
        let token = self
            .oci_tokens
            .get(&cache_key, self.engine.now())
            .unwrap_or_default();
        // Observed like every other upstream fetch: time to response headers,
        // both outcomes, so OCI proxies appear in the shared latency histogram.
        let start = Instant::now();
        let resp = self
            .engine
            .client
            .request(method.clone(), raw_url)
            .headers(build(&token))
            .send()
            .await;
        self.engine
            .upstream_dur
            .with_label_values(&[&res.repo.name])
            .observe(start.elapsed().as_secs_f64());
        let resp = resp.map_err(|_| UpstreamFailed)?;
        if resp.status() != StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }
        let challenge = header_str(resp.headers(), "WWW-Authenticate").to_string();
        drop(resp);

        let Some((realm, params)) = parse_bearer_challenge(&challenge) else {
            // Not a token registry (or bad credentials); surface the 401 as-is
            // by re-issuing without retry.
            return self
                .engine
                .client
                .request(method, raw_url)
                .headers(build(&token))
                .send()
                .await
                .map_err(|_| UpstreamFailed);
        };
        let (fresh, expires) = self.oci_fetch_token(&res.cfg, &realm, &params).await?;
        self.oci_tokens
            .set(&oci_token_key(res.repo.id, raw_url), fresh.clone(), expires);
        let start = Instant::now();
        let resp = self
            .engine
            .client
            .request(method, raw_url)
            .headers(build(&fresh))
            .send()
            .await;
        self.engine
            .upstream_dur
            .with_label_values(&[&res.repo.name])
            .observe(start.elapsed().as_secs_f64());
        resp.map_err(|_| UpstreamFailed)
    }

    /// Exchanges the challenge at the realm for a scoped token, using the
    /// repository's basic credentials when configured (an authenticated exchange
    /// gets private-image scopes and Docker Hub's higher rate limits).
    async fn oci_fetch_token(
        &self,
        cfg: &Config,
        realm: &str,
        params: &[(String, String)],
    ) -> Result<(String, DateTime<Utc>), UpstreamFailed> {
        #[derive(serde::Deserialize, Default)]
        struct TokenResponse {
            #[serde(default)]
            token: String,
            #[serde(default)]
            access_token: String,
            #[serde(default)]
            expires_in: i64,
        }

        let mut url = Url::parse(realm).map_err(|_| UpstreamFailed)?;
        for (key, value) in params {
            url.query_pairs_mut().append_pair(key, value);
        }
        let mut request = self.engine.client.get(url);
        if let Ok(v) = HeaderValue::from_str(&self.engine.user_agent) {
            request = request.header(http::header::USER_AGENT, v);
        }
        if cfg.upstream_auth.type_ == UPSTREAM_AUTH_BASIC {
            request = request.basic_auth(
                &cfg.upstream_auth.username,
                Some(&cfg.upstream_auth.password),
            );
        }
        let resp = request.send().await.map_err(|_| UpstreamFailed)?;
        let tr: TokenResponse = resp.json().await.unwrap_or_default();
        let token = if tr.token.is_empty() {
            tr.access_token
        } else {
            tr.token
        };
        let ttl = if tr.expires_in > 0 { tr.expires_in } else { 60 };
        // Renew slightly early so a token never expires mid-layer.
        Ok((
            token,
            self.engine.now() + chrono::TimeDelta::seconds(ttl - 10),
        ))
    }

    /// Fetches a manifest from the upstream (by tag or digest), verifies its
    /// digest, stores it and the tag row, and serves the bytes. The body is
    /// buffered whole — manifests are small and capped — because the digest must
    /// be verified before anything is stored or served.
    pub(crate) async fn oci_proxy_manifest(
        self: &Arc<Self>,
        parts: &Arc<Parts>,
        res: &Resolved,
        req: &OciRequest,
    ) -> Response {
        let key = format!(
            "{}/{}",
            res.repo.name,
            oci_manifest_path(&req.name, &req.reference)
        );
        if self.engine.neg.has(&key) {
            return write_oci_error(
                StatusCode::NOT_FOUND,
                "MANIFEST_UNKNOWN",
                "manifest unknown",
            );
        }
        if let Some(d) = self.engine.cool.remaining(&key) {
            return self
                .engine
                .write_retry(StatusCode::SERVICE_UNAVAILABLE, &retry_after_seconds(d));
        }
        self.engine
            .cache_miss
            .with_label_values(&[&res.repo.name])
            .inc();
        let url = oci_upstream_url(res, &req.name, &format!("manifests/{}", req.reference));
        let resp = match self
            .oci_upstream_do(res, Method::GET, &url, OCI_MANIFEST_ACCEPT)
            .await
        {
            Ok(resp) => resp,
            Err(_) => {
                self.engine
                    .upstream_err
                    .with_label_values(&[&res.repo.name])
                    .inc();
                return write_oci_error(
                    StatusCode::BAD_GATEWAY,
                    "UNSUPPORTED",
                    "upstream unreachable",
                );
            }
        };
        let media_type = header_str(resp.headers(), "Content-Type").to_string();
        let published = last_modified(&resp);
        if let Err(response) = self
            .oci_relay_upstream_status(res, &key, &resp, "MANIFEST_UNKNOWN")
            .await
        {
            return response;
        }

        let cap = self.oci_manifest_cap();
        let Ok(body) = resp.bytes().await else {
            return write_oci_error(
                StatusCode::BAD_GATEWAY,
                "MANIFEST_INVALID",
                "upstream manifest unreadable or oversized",
            );
        };
        if body.len() as i64 > cap {
            return write_oci_error(
                StatusCode::BAD_GATEWAY,
                "MANIFEST_INVALID",
                "upstream manifest unreadable or oversized",
            );
        }
        let digest = format!("sha256:{}", hex::encode(sha2::Sha256::digest(&body)));
        if OCI_DIGEST_RE.is_match(&req.reference) && digest != req.reference {
            return write_oci_error(
                StatusCode::BAD_GATEWAY,
                "DIGEST_INVALID",
                "upstream manifest does not match requested digest",
            );
        }
        if !oci_manifest_media_type(&media_type) {
            return write_oci_error(
                StatusCode::BAD_GATEWAY,
                "MANIFEST_INVALID",
                "upstream manifest media type unsupported",
            );
        }

        let path = oci_manifest_path(&req.name, &digest);
        let spec = FetchSpec {
            repo: res.repo.clone(),
            cfg: res.cfg.clone(),
            path: path.clone(),
            ..FetchSpec::blank()
        };
        if self.engine.eval_age(&spec, published) {
            return write_oci_error(
                StatusCode::NOT_FOUND,
                "MANIFEST_UNKNOWN",
                "blocked by age policy",
            );
        }
        if res.cfg.cache.enabled {
            let version = if OCI_DIGEST_RE.is_match(&req.reference) {
                String::new()
            } else {
                req.reference.clone()
            };
            let store_spec = FetchSpec {
                repo: res.repo.clone(),
                cfg: res.cfg.clone(),
                path: path.clone(),
                version,
                content_type: media_type.clone(),
                ..FetchSpec::blank()
            };
            match self
                .engine
                .store_artifact(
                    &store_spec,
                    Box::pin(std::io::Cursor::new(body.to_vec())),
                    &media_type,
                    published,
                    &username_from_context(parts),
                )
                .await
            {
                Err(err) => tracing::error!(
                    repo = %res.repo.name, path = %path, err = %err,
                    "oci manifest cache write failed"
                ),
                Ok(_) if !OCI_DIGEST_RE.is_match(&req.reference) => {
                    let _ = self
                        .store
                        .upsert_oci_tag(res.repo.id, &req.name, &req.reference, &digest)
                        .await;
                }
                Ok(_) => {}
            }
        }

        let gate = self.final_policy_gate(res.clone(), &req.name, &req.reference);
        if let Some(resp) = gate(Arc::clone(parts)).await {
            return resp;
        }
        let length = body.len() as i64;
        let mut resp = if parts.method == Method::HEAD {
            StatusCode::OK.into_response()
        } else {
            self.engine
                .bytes
                .with_label_values(&["egress", &res.repo.format])
                .inc_by(length as f64);
            body.into_response()
        };
        let headers = resp.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&media_type) {
            headers.insert(CONTENT_TYPE, v);
        }
        if let Ok(v) = HeaderValue::from_str(&digest) {
            headers.insert(http::HeaderName::from_static("docker-content-digest"), v);
        }
        if let Ok(v) = HeaderValue::from_str(&itoa(length)) {
            headers.insert(http::header::CONTENT_LENGTH, v);
        }
        resp
    }

    /// Fetches a blob from the upstream by digest, stores it (verifying the
    /// stored digest against the requested one) and serves it from the store.
    pub(crate) async fn oci_proxy_blob(
        self: &Arc<Self>,
        parts: &Arc<Parts>,
        res: &Resolved,
        req: &OciRequest,
    ) -> Response {
        let path = oci_blob_path(&req.name, &req.reference);
        let key = format!("{}/{path}", res.repo.name);
        if self.engine.neg.has(&key) {
            return write_oci_error(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "blob unknown");
        }
        if let Some(d) = self.engine.cool.remaining(&key) {
            return self
                .engine
                .write_retry(StatusCode::SERVICE_UNAVAILABLE, &retry_after_seconds(d));
        }
        self.engine
            .cache_miss
            .with_label_values(&[&res.repo.name])
            .inc();

        // Coalesce concurrent pulls of the same layer into one upstream fetch,
        // like the package-format proxy path.
        let outcome = {
            let manager = Arc::clone(self);
            let res = res.clone();
            let req = req.clone();
            let key_owned = key.clone();
            self.engine
                .flight
                .do_call(&key, move || async move {
                    manager.oci_fetch_blob(&res, &req, &key_owned).await
                })
                .await
        };
        match outcome.kind {
            FetchKind::Stored => {
                let Ok(art) = self.store.get_artifact(res.repo.id, &path).await else {
                    return write_oci_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "BLOB_UNKNOWN",
                        "cache read failed",
                    );
                };
                let (mut resp, n) = self.engine.serve_artifact(parts, &art).await;
                self.engine
                    .bytes
                    .with_label_values(&["egress", &res.repo.format])
                    .inc_by(n as f64);
                if let Ok(v) = HeaderValue::from_str(&req.reference) {
                    resp.headers_mut()
                        .insert(http::HeaderName::from_static("docker-content-digest"), v);
                }
                resp
            }
            FetchKind::NotFound => {
                write_oci_error(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "blob unknown")
            }
            FetchKind::Retry => self.engine.write_retry(
                StatusCode::from_u16(outcome.status as u16)
                    .unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
                &outcome.retry_after,
            ),
            _ => write_oci_error(StatusCode::BAD_GATEWAY, "UNSUPPORTED", "upstream error"),
        }
    }

    /// Performs the coalesced upstream blob fetch and store. It writes no HTTP
    /// response.
    async fn oci_fetch_blob(&self, res: &Resolved, req: &OciRequest, key: &str) -> FetchOutcome {
        let path = oci_blob_path(&req.name, &req.reference);
        if self.store.get_artifact(res.repo.id, &path).await.is_ok() {
            return FetchOutcome::stored();
        }
        let url = oci_upstream_url(res, &req.name, &format!("blobs/{}", req.reference));
        let resp = match self.oci_upstream_do(res, Method::GET, &url, "").await {
            Ok(resp) => resp,
            Err(_) => {
                self.engine
                    .upstream_err
                    .with_label_values(&[&res.repo.name])
                    .inc();
                return FetchOutcome::error();
            }
        };
        let status = resp.status();
        if status == StatusCode::NOT_FOUND
            || status == StatusCode::UNAUTHORIZED
            || status == StatusCode::FORBIDDEN
        {
            // See `oci_relay_upstream_status`: public registries use 401/403 for
            // absent images, so they negative-cache like a 404.
            self.engine.neg.set(key, res.cfg.cache.negative_ttl.d());
            return FetchOutcome {
                kind: FetchKind::NotFound,
                ..Default::default()
            };
        }
        if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
            let d = parse_retry_after(header_str(resp.headers(), "Retry-After"), self.engine.now());
            self.engine.cool.set(key, d);
            self.engine
                .upstream_err
                .with_label_values(&[&res.repo.name])
                .inc();
            return FetchOutcome {
                kind: FetchKind::Retry,
                status: status.as_u16() as i64,
                retry_after: retry_after_seconds(d),
            };
        }
        if !status.is_success() {
            self.engine
                .upstream_err
                .with_label_values(&[&res.repo.name])
                .inc();
            return FetchOutcome::error();
        }

        let content_type = {
            let ct = header_str(resp.headers(), "Content-Type").to_string();
            if ct.is_empty() {
                "application/octet-stream".to_string()
            } else {
                ct
            }
        };
        let reader = tokio_util::io::StreamReader::new(futures_util::TryStreamExt::map_err(
            resp.bytes_stream(),
            std::io::Error::other,
        ));
        let _gc = self.engine.gc_mu.read().await;
        let Ok((stored, size)) = self.engine.blobs.put(Box::pin(reader)).await else {
            return FetchOutcome::error();
        };
        if format!("sha256:{stored}") != req.reference {
            // Upstream served bytes that do not match the digest-addressed
            // request: never record them under this path.
            self.engine.abandon_blob(&stored, size).await;
            tracing::error!(
                repo = %res.repo.name, path = %path, got = %stored,
                "oci upstream blob digest mismatch"
            );
            return FetchOutcome::error();
        }
        let now = self.engine.now();
        if self
            .store
            .put_artifact(Artifact {
                repo_id: res.repo.id,
                path: path.clone(),
                blob_sha256: stored,
                size,
                content_type,
                cached_at: now,
                last_accessed_at: now,
                ..Default::default()
            })
            .await
            .is_err()
        {
            return FetchOutcome::error();
        }
        let hook = self.engine.on_store.read().clone();
        if let Some(on_store) = hook {
            on_store(res.repo.clone(), path);
        }
        FetchOutcome::stored()
    }

    /// Passes a tag list through to the upstream. Tag lists are a moving index
    /// consulted interactively (not on the pull path), so they are served live
    /// rather than cached.
    pub(crate) async fn oci_proxy_tags_list(
        self: &Arc<Self>,
        parts: &Arc<Parts>,
        res: &Resolved,
        req: &OciRequest,
    ) -> Response {
        let mut suffix = "tags/list".to_string();
        if let Some(query) = parts.uri.query()
            && !query.is_empty()
        {
            suffix.push('?');
            suffix.push_str(query);
        }
        let url = oci_upstream_url(res, &req.name, &suffix);
        let resp = match self.oci_upstream_do(res, Method::GET, &url, "").await {
            Ok(resp) => resp,
            Err(_) => {
                self.engine
                    .upstream_err
                    .with_label_values(&[&res.repo.name])
                    .inc();
                return write_oci_error(
                    StatusCode::BAD_GATEWAY,
                    "UNSUPPORTED",
                    "upstream unreachable",
                );
            }
        };
        let key = format!("{}/{}/tags/list", res.repo.name, req.name);
        if let Err(response) = self
            .oci_relay_upstream_status(res, &key, &resp, "NAME_UNKNOWN")
            .await
        {
            return response;
        }
        let mut out = if parts.method == Method::HEAD {
            StatusCode::OK.into_response()
        } else {
            let reader = tokio_util::io::StreamReader::new(futures_util::TryStreamExt::map_err(
                resp.bytes_stream(),
                std::io::Error::other,
            ));
            let limited = tokio::io::AsyncReadExt::take(reader, MAX_METADATA_BYTES as u64);
            Body::from_stream(tokio_util::io::ReaderStream::new(limited)).into_response()
        };
        out.headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        out
    }

    /// Handles the shared upstream error statuses (404, 429, 503, other
    /// non-2xx). `Err(response)` means the caller must not proceed with the
    /// body.
    #[allow(clippy::result_large_err)]
    async fn oci_relay_upstream_status(
        &self,
        res: &Resolved,
        key: &str,
        resp: &reqwest::Response,
        not_found_code: &str,
    ) -> Result<(), Response> {
        let status = resp.status();
        if status == StatusCode::NOT_FOUND
            || status == StatusCode::UNAUTHORIZED
            || status == StatusCode::FORBIDDEN
        {
            // Public registries answer 401/403 for images that do not exist (or
            // are private), deliberately indistinguishable from absent.
            // Relaying them as 404 lets a group fall through to its next member;
            // a credential misconfiguration would also land here, so it is
            // logged.
            if status != StatusCode::NOT_FOUND {
                tracing::warn!(
                    repo = %res.repo.name, status = status.as_u16(), url = %resp.url(),
                    "oci upstream denied; treating as not found"
                );
            }
            self.engine.neg.set(key, res.cfg.cache.negative_ttl.d());
            return Err(write_oci_error(
                StatusCode::NOT_FOUND,
                not_found_code,
                "not found upstream",
            ));
        }
        if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
            let d = parse_retry_after(header_str(resp.headers(), "Retry-After"), self.engine.now());
            self.engine.cool.set(key, d);
            self.engine
                .upstream_err
                .with_label_values(&[&res.repo.name])
                .inc();
            return Err(self.engine.write_retry(status, &retry_after_seconds(d)));
        }
        if !status.is_success() {
            self.engine
                .upstream_err
                .with_label_values(&[&res.repo.name])
                .inc();
            return Err(write_oci_error(
                StatusCode::BAD_GATEWAY,
                "UNSUPPORTED",
                "upstream error",
            ));
        }
        Ok(())
    }
}

/// Scopes cached tokens to one image name: registry token scopes are per
/// repository (image), so the URL up to the endpoint segment identifies the
/// scope without parsing it back out of the challenge.
fn oci_token_key(repo_id: i64, raw_url: &str) -> String {
    let mut url = raw_url;
    for marker in ["/manifests/", "/blobs/", "/tags/"] {
        if let Some(i) = url.rfind(marker) {
            url = &url[..i];
            break;
        }
    }
    format!("{}\u{0}{url}", itoa(repo_id))
}

/// Extracts the realm and the remaining parameters from a
/// `WWW-Authenticate: Bearer` challenge.
fn parse_bearer_challenge(h: &str) -> Option<(String, Vec<(String, String)>)> {
    let rest = h.trim().strip_prefix("Bearer ")?;
    let mut params = Vec::new();
    let mut realm = String::new();
    for part in rest.split(',') {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        let value = value.trim_matches('"').to_string();
        if key == "realm" {
            realm = value;
        } else {
            params.push((key.to_string(), value));
        }
    }
    (!realm.is_empty()).then_some((realm, params))
}

// Keeps the format constant referenced even when the OCI proxy is the only
// consumer of the meta module here.
const _: &str = meta::FORMAT_OCI;
