//! Serves the npm registry protocol. Paths under `/npm/{repo}/`:
//!
//! ```text
//! <package>            packument (version index)  (metadata)
//! <package>/-/<file>   tarball                    (artifact)
//! ```
//!
//! Scoped packages (`@scope/name`) reach us with the scope separator
//! percent-encoded, and the exact form varies by client and method (npm publish
//! sends `%2f`, pnpm `%2F`, others a literal `/`). [`handle_npm`] decodes the
//! path once into a single canonical identity so a PUT and a later GET address
//! the same artifact, and the decoded form is what we store, look up and fetch
//! upstream (npmjs accepts the literal-slash form for scoped names).
//!
//! For proxy repos the packument's `dist.tarball` URLs are rewritten to point
//! back at forklift so tarball fetches are cached and age-gated here, and
//! versions newer than the age-policy cooldown are filtered out of the packument
//! entirely.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use http::header::CONTENT_TYPE;
use http::request::Parts;
use http::{HeaderValue, Method, StatusCode};
use serde_json::value::RawValue;
use tokio::io::AsyncReadExt;

use crate::meta::{self, Artifact};
use crate::repoconfig::{ACTION_BLOCK, AgePolicyConfig};
use crate::server::http_error;

use super::group::serving_repo_name;
use super::maven::last_modified;
use super::rendercache::render_cache_key;
use super::router::{Resolved, action_for_method, join_upstream};
use super::{
    FetchKind, FetchSpec, Kind, MAX_METADATA_BYTES, Manager, not_found, path_base,
    retry_after_seconds, username_from_context,
};

/// A JSON object whose members are kept as raw fragments.
pub(super) type RawObject = BTreeMap<String, Box<RawValue>>;

/// Serves the npm registry protocol.
pub(crate) async fn handle_npm(m: Arc<Manager>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let mut res = match m.resolve(&parts, meta::FORMAT_NPM).await {
        Ok(res) => res,
        Err(resp) => return resp,
    };
    // Percent-decode into the canonical package identity used for every storage
    // and lookup key. Re-check for traversal on the decoded form so a client
    // cannot smuggle ".." past resolve's raw-path check via %2e%2e.
    res.path = decode_npm_path(&res.path);
    if res.path.contains("..") {
        return http_error(StatusCode::BAD_REQUEST, "invalid path");
    }
    if let Err(resp) = m.authorize(
        &parts,
        &res.repo.name,
        action_for_method(&parts.method),
        res.cfg.public,
    ) {
        return resp;
    }
    let (pkg, version) = (npm_package(&res.path), npm_version(&res.path));
    let parts = Arc::new(parts);
    if let Some(resp) = m
        .policy_gates(Arc::clone(&parts), Arc::new(res.clone()), &pkg, &version)
        .await
    {
        return resp;
    }

    if res.path.contains("/-/") {
        return m.npm_tarball(parts, res).await;
    }

    match parts.method {
        Method::GET | Method::HEAD => m.npm_packument(parts, res).await,
        Method::PUT => {
            if res.repo.r#type != meta::TYPE_HOSTED {
                return http_error(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "uploads are only allowed on local repositories",
                );
            }
            m.npm_publish(parts, res, body).await
        }
        _ => http_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
    }
}

impl Manager {
    async fn npm_tarball(self: &Arc<Self>, parts: Arc<Parts>, res: Resolved) -> Response {
        if parts.method != Method::GET && parts.method != Method::HEAD {
            return http_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
        let version = npm_version(&res.path);
        // Derive the release time from the packument `time` map so the tarball
        // age gate agrees with the packument index filter
        // (`rewrite_packument`). The npm CDN's Last-Modified header can drift
        // from the publish time (a re-uploaded tarball bumps mtime), which would
        // block tarballs the index still advertises. Fall back to Last-Modified
        // when the packument has no timestamp for this version.
        //
        // That lookup re-reads and re-scans the whole packument, once per
        // tarball cache miss, so a cold install pays for the document twice per
        // package. It is worth it only where the disagreement it prevents can
        // actually block a download: with the age policy off, `published_at` is
        // metadata, and Last-Modified is already this path's documented source
        // for it.
        //
        // The Rust hook is synchronous too, so the lookup is done up front and the resolved
        // instant is captured by the closure; the engine only consults it, which is the same
        // decision on the same inputs.
        let packument_published = if res.cfg.age_policy.enabled {
            self.npm_tarball_published(&res, &version).await
        } else {
            None
        };
        let spec = FetchSpec {
            repo: res.repo.clone(),
            cfg: res.cfg.clone(),
            path: res.path.clone(),
            version: version.clone(),
            upstream_url: join_upstream(&res.repo.upstream_url, &res.path),
            kind: Kind::Artifact,
            content_type: "application/octet-stream".to_string(),
            final_gate: Some(self.final_policy_gate(
                res.clone(),
                &npm_package(&res.path),
                &version,
            )),
            extract_published: Some(Arc::new(move |resp: &reqwest::Response| {
                packument_published.or_else(|| last_modified(resp))
            })),
            ..FetchSpec::blank()
        };
        self.engine.serve(parts, spec).await
    }

    /// Resolves a tarball version's upstream publish time from the package's
    /// packument `time` map, the same source the packument age filter uses.
    /// Returns `None` when the version, packument, or timestamp is unavailable
    /// so the caller can fall back to the HTTP `Last-Modified` header.
    async fn npm_tarball_published(&self, res: &Resolved, version: &str) -> Option<DateTime<Utc>> {
        if version.is_empty() {
            return None;
        }
        let pkg_path = match res.path.find("/-/") {
            Some(i) => &res.path[..i],
            None => res.path.as_str(),
        };
        let times = self.npm_packument_times(res, pkg_path).await?;
        let ts = times.get(version)?;
        DateTime::parse_from_rfc3339(ts)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    }

    /// Returns the packument `time` map for `pkg_path`, preferring the cached
    /// copy and otherwise fetching it from upstream (best-effort, no caching).
    /// The document is decoded as a stream so only the `time` map is
    /// materialized; a multi-megabyte packument is never buffered here. This
    /// runs once per tarball fetch, so under an install burst of hundreds of
    /// packages buffering whole packuments would multiply into an OOM.
    async fn npm_packument_times(
        &self,
        res: &Resolved,
        pkg_path: &str,
    ) -> Option<BTreeMap<String, String>> {
        #[derive(serde::Deserialize)]
        struct TimeDoc {
            #[serde(default)]
            time: BTreeMap<String, String>,
        }

        let e = &self.engine;
        if let Ok(art) = e.store.get_artifact(res.repo.id, pkg_path).await
            && let Ok((reader, _)) = e.blobs.open(&art.blob_sha256).await
            && let Some(doc) = decode_json_stream::<TimeDoc, _>(reader, 64 << 20).await
        {
            return Some(doc.time);
        }
        let spec = FetchSpec {
            repo: res.repo.clone(),
            cfg: res.cfg.clone(),
            path: pkg_path.to_string(),
            upstream_url: join_upstream(&res.repo.upstream_url, pkg_path),
            kind: Kind::Metadata,
            ..FetchSpec::blank()
        };
        let resp = e.upstream_get(&spec).await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let reader = tokio_util::io::StreamReader::new(futures_util::TryStreamExt::map_err(
            resp.bytes_stream(),
            std::io::Error::other,
        ));
        let doc = decode_json_stream::<TimeDoc, _>(reader, 64 << 20).await?;
        Some(doc.time)
    }

    async fn npm_packument(self: &Arc<Self>, parts: Arc<Parts>, res: Resolved) -> Response {
        let e = Arc::clone(&self.engine);

        let art = match e.store.get_artifact(res.repo.id, &res.path).await {
            Ok(art) => Some(art),
            Err(meta::Error::NotFound) => None,
            Err(_) => return http_error(StatusCode::INTERNAL_SERVER_ERROR, "metadata error"),
        };
        if let Some(art) = art
            && (res.repo.r#type == meta::TYPE_HOSTED || e.fresh(&art, &res.cfg, Kind::Metadata))
        {
            e.touch(&art, &username_from_context(&parts)).await;
            // Both hosted and proxy packuments are rewritten per request so
            // tarball URLs point at the repo the client is talking to (the group
            // during fan-out) with a filename derived from the stored bytes.
            // This keeps cached proxy bodies host-agnostic (a client cannot
            // poison the cache for others via Host/X-Forwarded-* headers), and
            // stops a hosted publish's original dist.tarball — which encodes the
            // publish registry and may carry a malformed scoped filename — from
            // leaking to installers.
            //
            // HEAD needs no body, so skip the decode/rewrite work entirely.
            if parts.method == Method::HEAD {
                let gate = self.final_policy_gate(res.clone(), &res.path, "");
                if let Some(resp) = gate(Arc::clone(&parts)).await {
                    return resp;
                }
                return write_packument(&parts, Bytes::new());
            }
            let (resp, removed) = self.npm_serve_cached_packument(&parts, &res, &art).await;
            self.report_packument_age_blocks(&res, removed);
            return resp;
        }
        if res.repo.r#type == meta::TYPE_HOSTED {
            return not_found();
        }

        let key = format!("{}/{}", res.repo.name, res.path);
        if e.neg.has(&key) {
            return not_found();
        }
        // Recently rate-limited upstream: answer locally instead of re-entering
        // the storm, relaying Retry-After so the installer backs off.
        if let Some(d) = e.cool.remaining(&key) {
            return e.write_retry(StatusCode::SERVICE_UNAVAILABLE, &retry_after_seconds(d));
        }
        e.cache_miss.with_label_values(&[&res.repo.name]).inc();

        if !res.cfg.cache.enabled {
            return self.npm_packument_passthrough(&parts, &res, &key).await;
        }

        // Coalesce concurrent fetches of the same packument into one upstream
        // round-trip. An install burst resolves the same package from every
        // parallel job, and the duplicate GETs are what turn a slow upstream
        // into a rate-limit storm. The shared fetch is detached from any single
        // client's cancellation so a waiter timing out cannot abort the fetch
        // the others depend on.
        //
        // No `extract_published` is supplied: a packument has no single publish
        // time, and the age policy filters individual versions per request
        // during the rewrite below. Only the upstream's original body is cached,
        // never the rewritten one, so a cached body never embeds a
        // request-derived host.
        let spec = FetchSpec {
            repo: res.repo.clone(),
            cfg: res.cfg.clone(),
            path: res.path.clone(),
            upstream_url: join_upstream(&res.repo.upstream_url, &res.path),
            kind: Kind::Metadata,
            content_type: "application/json".to_string(),
            max_store_bytes: MAX_METADATA_BYTES,
            ..FetchSpec::blank()
        };
        let outcome = {
            let engine = Arc::clone(&e);
            let key_owned = key.clone();
            let username = username_from_context(&parts);
            e.flight
                .do_call(&format!("npm-packument:{key}"), move || async move {
                    engine.fetch_and_store(spec, &key_owned, &username).await
                })
                .await
        };
        match outcome.kind {
            FetchKind::Stored => {}
            FetchKind::NotFound => return not_found(),
            FetchKind::Retry => {
                return e.write_retry(
                    StatusCode::from_u16(outcome.status as u16)
                        .unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
                    &outcome.retry_after,
                );
            }
            _ => return http_error(StatusCode::BAD_GATEWAY, "upstream error"),
        }

        let Ok(art) = e.store.get_artifact(res.repo.id, &res.path).await else {
            return http_error(StatusCode::INTERNAL_SERVER_ERROR, "cache read failed");
        };
        let (resp, removed) = self.npm_serve_cached_packument(&parts, &res, &art).await;
        self.report_packument_age_blocks(&res, removed);
        resp
    }

    /// Serves a packument for a cache-disabled proxy repository: there is no
    /// blob to decode off, so the body is buffered once and rewritten in memory.
    /// The rewrite gate bounds how many such buffers exist, and is taken only
    /// once the body is in hand so a slow upstream cannot occupy a slot.
    async fn npm_packument_passthrough(
        self: &Arc<Self>,
        parts: &Arc<Parts>,
        res: &Resolved,
        key: &str,
    ) -> Response {
        let e = &self.engine;
        let upstream_url = join_upstream(&res.repo.upstream_url, &res.path);
        let spec = FetchSpec {
            repo: res.repo.clone(),
            cfg: res.cfg.clone(),
            path: res.path.clone(),
            upstream_url: upstream_url.clone(),
            kind: Kind::Metadata,
            ..FetchSpec::blank()
        };
        let resp = match e.upstream_get(&spec).await {
            Ok(resp) => resp,
            Err(err) => {
                e.upstream_err.with_label_values(&[&res.repo.name]).inc();
                tracing::error!(
                    repo = %res.repo.name, url = %upstream_url, err = %err,
                    "upstream fetch failed"
                );
                return http_error(StatusCode::BAD_GATEWAY, "upstream unreachable");
            }
        };
        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            e.neg.set(key, res.cfg.cache.negative_ttl.d());
            return not_found();
        }
        if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
            let d =
                super::parse_retry_after(super::header_str(resp.headers(), "Retry-After"), e.now());
            e.cool.set(key, d);
            e.upstream_err.with_label_values(&[&res.repo.name]).inc();
            tracing::warn!(
                repo = %res.repo.name, path = %res.path, status = status.as_u16(),
                cooldown = %humantime::format_duration(d),
                "upstream rate-limited; cooling down"
            );
            return e.write_retry(status, &retry_after_seconds(d));
        }
        if !status.is_success() {
            e.upstream_err.with_label_values(&[&res.repo.name]).inc();
            return http_error(StatusCode::BAD_GATEWAY, "upstream error");
        }

        let reader = tokio_util::io::StreamReader::new(futures_util::TryStreamExt::map_err(
            resp.bytes_stream(),
            std::io::Error::other,
        ));
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
        let (transformed, removed) = rewrite_packument(
            &body,
            &self.external_base(parts),
            &serving_repo_name(parts, &res.repo.name),
            &res.path,
            &res.cfg.age_policy,
            e.now(),
        );
        drop(slot);
        self.report_packument_age_blocks(res, removed);
        self.write_rendered_packument(parts, res, transformed).await
    }

    /// Serves the cached packument for `art`: tarball URLs are rewritten and the
    /// age policy applied per request, then the result is written. It also
    /// returns the number of versions the age policy removed (0 on the
    /// pass-through paths).
    ///
    /// A rewrite slot is held for the decode only. The write happens after the
    /// slot is released, because a client that reads slowly would otherwise cap
    /// metadata serving at the gate width: an installer resolving hundreds of
    /// packages holds every slot on socket writes while the rest of its own
    /// requests time out client-side, with no status code for forklift to
    /// report.
    ///
    /// A rendered document is reused until its inputs change or, under a
    /// blocking age policy, until the next version leaves the cooldown (see
    /// `RenderCache::put`), so a retry storm or a second job resolving the same
    /// lockfile costs a map lookup instead of a decode, and never queues for a
    /// slot at all.
    async fn npm_serve_cached_packument(
        self: &Arc<Self>,
        parts: &Arc<Parts>,
        res: &Resolved,
        art: &Artifact,
    ) -> (Response, i64) {
        let e = &self.engine;
        let base = self.external_base(parts);
        // The repository row is touched whenever its config changes, so its
        // `updated_at` stands in for a config revision: a policy edit
        // invalidates every document rendered under the previous configuration.
        let key = render_cache_key(
            &res.repo.name,
            &res.path,
            &art.blob_sha256,
            &base,
            &serving_repo_name(parts, &res.repo.name),
            &crate::meta::time::format_time(res.repo.updated_at),
        );
        if let Some((body, removed)) = e.renders.get(&key) {
            e.render_cache_ops.with_label_values(&["hit"]).inc();
            return (
                self.write_rendered_packument(parts, res, body).await,
                removed,
            );
        }
        e.render_cache_ops.with_label_values(&["miss"]).inc();

        let Some(slot) = e.acquire_rewrite(&res.repo.name).await else {
            return (
                http_error(StatusCode::SERVICE_UNAVAILABLE, "shutting down"),
                0,
            );
        };
        let rendered = self
            .npm_render_cached_packument(parts, res, art, &base)
            .await;
        drop(slot);

        match rendered {
            Err(()) => (
                http_error(StatusCode::INTERNAL_SERVER_ERROR, "blob missing"),
                0,
            ),
            // Not a packument: serve the stored bytes untouched, matching the
            // fetch-time fallback.
            Ok(None) => {
                let gate = self.final_policy_gate(res.clone(), &res.path, "");
                if let Some(resp) = gate(Arc::clone(parts)).await {
                    return (resp, 0);
                }
                let (resp, _) = e.serve_artifact(parts, art).await;
                (resp, 0)
            }
            Ok(Some((body, removed, until))) => {
                e.renders.put(&key, body.clone(), removed, until);
                (
                    self.write_rendered_packument(parts, res, body).await,
                    removed,
                )
            }
        }
    }

    /// Decodes the cached packument for `art` off its blob reader and rewrites
    /// it. Decoding straight off the reader holds only the parsed per-version
    /// fragments in memory — never the raw packument and the parsed document at
    /// once (see [`rewrite_packument_top`]). `Ok(None)` means the stored
    /// document is not a packument at all, which the caller serves verbatim.
    ///
    /// The caller must hold a rewrite slot: this is the whole of the work the
    /// gate exists to bound.
    #[allow(clippy::result_unit_err)]
    async fn npm_render_cached_packument(
        &self,
        parts: &Arc<Parts>,
        res: &Resolved,
        art: &Artifact,
        base: &str,
    ) -> Result<Option<(Bytes, i64, Option<DateTime<Utc>>)>, ()> {
        let e = &self.engine;
        let reader = match e.blobs.open(&art.blob_sha256).await {
            Ok((reader, _)) => reader,
            Err(err) => {
                e.note_blob_missing(
                    res.repo.id,
                    &res.repo.name,
                    &res.path,
                    &art.blob_sha256,
                    "index",
                    StatusCode::INTERNAL_SERVER_ERROR.as_u16() as i64,
                    &err.to_string(),
                );
                return Err(());
            }
        };
        let Some(top) = decode_json_stream::<RawObject, _>(reader, 64 << 20).await else {
            return Ok(None);
        };
        Ok(rewrite_packument_top(
            top,
            base,
            &serving_repo_name(parts, &res.repo.name),
            &res.path,
            &res.cfg.age_policy,
            e.now(),
        )
        .map(|(body, removed, until)| (Bytes::from(body), removed, until)))
    }

    /// Runs the final policy gate and writes an already rendered document. The
    /// in-flight render budget is reserved across the write: the rewrite gate no
    /// longer bounds this memory, since the slot is released before the client
    /// is written to.
    async fn write_rendered_packument(
        self: &Arc<Self>,
        parts: &Arc<Parts>,
        res: &Resolved,
        body: Bytes,
    ) -> Response {
        let gate = self.final_policy_gate(res.clone(), &res.path, "");
        if let Some(resp) = gate(Arc::clone(parts)).await {
            return resp;
        }
        let Some(permit) = self.engine.acquire_render_bytes(body.len() as i64).await else {
            return http_error(StatusCode::SERVICE_UNAVAILABLE, "shutting down");
        };
        let resp = write_packument(parts, body);
        drop(permit);
        resp
    }

    /// Records the age-policy quarantine metric and log for a proxy packument
    /// fetch when the rewrite removed one or more versions.
    fn report_packument_age_blocks(&self, res: &Resolved, removed: i64) {
        if removed <= 0 {
            return;
        }
        let e = &self.engine;
        e.age_blocks
            .with_label_values(&[&res.repo.name, &res.cfg.age_policy.action])
            .inc_by(removed as f64);
        tracing::warn!(
            repo = %res.repo.name, package = %res.path, removed,
            min_age = %humantime::format_duration(res.cfg.age_policy.min_age.d()),
            action = "block",
            "age policy quarantined package versions"
        );
    }

    /// Handles `npm publish` to a hosted repository. The request body is a
    /// packument document with base64 `_attachments`; each attachment is stored
    /// as a tarball blob and the packument (minus attachments) is stored as the
    /// index.
    pub(crate) async fn npm_publish(
        self: &Arc<Self>,
        parts: Arc<Parts>,
        res: Resolved,
        body: Body,
    ) -> Response {
        // A publish document embeds whole tarballs as base64, so parsing it is
        // the most memory-expensive request the npm handler serves; the rewrite
        // gate bounds how many are in flight.
        let Some(slot) = self.engine.acquire_rewrite(&res.repo.name).await else {
            return http_error(StatusCode::SERVICE_UNAVAILABLE, "shutting down");
        };
        let reader = super::request_body(body);
        let Some(mut doc) =
            decode_json_stream::<serde_json::Map<String, serde_json::Value>, _>(reader, 256 << 20)
                .await
        else {
            drop(slot);
            return http_error(StatusCode::BAD_REQUEST, "invalid publish document");
        };
        let uploader = self.uploader.read().clone();
        if uploader.is_some() {
            let resp = self.npm_publish_atomic(&parts, &res, doc).await;
            drop(slot);
            return resp;
        }
        let username = username_from_context(&parts);
        if let Some(serde_json::Value::Object(atts)) = doc.get("_attachments") {
            for (name, v) in atts.clone() {
                let serde_json::Value::Object(att) = v else {
                    continue;
                };
                let data = att.get("data").and_then(|d| d.as_str()).unwrap_or("");
                let tarball_path = format!("{}/-/{}", res.path, path_base(&name));
                // Decode the tarball as a stream into the blob store instead of
                // materializing a second full copy of the attachment.
                let decoded = match base64_decode_stream(data) {
                    Ok(bytes) => bytes,
                    Err(()) => {
                        drop(slot);
                        return http_error(StatusCode::BAD_REQUEST, "invalid attachment encoding");
                    }
                };
                if self
                    .engine
                    .put(
                        &res.repo,
                        &tarball_path,
                        &npm_version(&tarball_path),
                        "application/octet-stream",
                        None,
                        Box::pin(std::io::Cursor::new(decoded)),
                        &username,
                    )
                    .await
                    .is_err()
                {
                    drop(slot);
                    return http_error(StatusCode::INTERNAL_SERVER_ERROR, "store tarball failed");
                }
                self.scan_stored(&res.repo, &tarball_path);
                self.resolve_stored(&res.repo, &tarball_path);
            }
        }
        doc.remove("_attachments");
        let index_body = serde_json::to_vec(&doc).unwrap_or_default();
        if self
            .engine
            .put(
                &res.repo,
                &res.path,
                "",
                "application/json",
                None,
                Box::pin(std::io::Cursor::new(index_body)),
                &username,
            )
            .await
            .is_err()
        {
            drop(slot);
            return http_error(StatusCode::INTERNAL_SERVER_ERROR, "store packument failed");
        }
        drop(slot);
        StatusCode::CREATED.into_response()
    }
}

/// Writes a (rewritten) packument response.
fn write_packument(parts: &Parts, body: Bytes) -> Response {
    let mut resp = if parts.method == Method::HEAD {
        StatusCode::OK.into_response()
    } else {
        body.into_response()
    };
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

pub(crate) fn base64_decode_stream(data: &str) -> Result<Vec<u8>, ()> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| ())
}

/// Returns `None` when the bytes are not the expected shape, which every caller treats as "not
/// a packument" rather than an error.
pub(crate) async fn decode_json_stream<T, R>(reader: R, limit: u64) -> Option<T>
where
    T: serde::de::DeserializeOwned + Send + 'static,
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
{
    let bridged = tokio_util::io::SyncIoBridge::new(tokio::io::AsyncReadExt::take(reader, limit));
    tokio::task::spawn_blocking(move || serde_json::from_reader::<_, T>(bridged).ok())
        .await
        .ok()
        .flatten()
}

/// Returns the highest plain `x.y.z` version key (pre-release and
/// build-metadata versions are skipped), or `""` when none parse.
pub(super) fn highest_stable_version<V>(versions: &BTreeMap<String, V>) -> String {
    let mut best = String::new();
    let mut best_n = [0i64; 3];
    for ver in versions.keys() {
        let Some(n) = parse_semver(ver) else {
            continue;
        };
        if best.is_empty()
            || n[0] > best_n[0]
            || (n[0] == best_n[0] && (n[1] > best_n[1] || (n[1] == best_n[1] && n[2] > best_n[2])))
        {
            best = ver.clone();
            best_n = n;
        }
    }
    best
}

/// Parses a stable `x.y.z` version, rejecting pre-release/build forms.
fn parse_semver(v: &str) -> Option<[i64; 3]> {
    let v = v.strip_prefix('v').unwrap_or(v);
    if v.contains('-') || v.contains('+') {
        return None;
    }
    let segs: Vec<&str> = v.split('.').collect();
    if segs.len() != 3 {
        return None;
    }
    let mut out = [0i64; 3];
    for (i, s) in segs.iter().enumerate() {
        let n: i64 = s.parse().ok()?;
        if n < 0 {
            return None;
        }
        out[i] = n;
    }
    Some(out)
}

/// Percent-decodes the repo-relative npm request path so a scoped package's
/// identity is canonical regardless of how the client encoded the scope
/// separator (npm publish sends `%2f`, pnpm `%2F`, others a literal `/`). The
/// decoded form is the single key used for storage and lookup, so a PUT and a
/// later GET address the same artifact. Falls back to the raw path when it is
/// not valid percent-encoding rather than rejecting the request.
pub(crate) fn decode_npm_path(p: &str) -> String {
    match percent_encoding::percent_decode_str(p).decode_utf8() {
        Ok(decoded) => decoded.into_owned(),
        Err(_) => p.to_string(),
    }
}

/// Extracts the package name from an npm protocol path: the whole path for
/// packuments, the part before `/-/` for tarballs. [`handle_npm`] decodes the
/// path before use, but this also decodes so it is correct when called
/// standalone (e.g. from tests or gates) on a still-encoded scope separator.
pub(crate) fn npm_package(p: &str) -> String {
    let p = match p.find("/-/") {
        Some(i) => &p[..i],
        None => p,
    };
    decode_npm_path(p).trim_matches('/').to_lowercase()
}

/// Extracts the version from an npm tarball path
/// (`<pkg>/-/<basename>-<version>.tgz`). Versions may themselves contain dashes
/// (`1.0.0-beta.1`), so the basename prefix is stripped by name rather than
/// split on dashes. Returns `""` for packument paths and unrecognized layouts
/// (never block on unknown).
pub(crate) fn npm_version(p: &str) -> String {
    if !p.contains("/-/") {
        return String::new();
    }
    let pkg = npm_package(p);
    let file = path_base(p).to_lowercase();
    // Scoped tarballs are named after the unscoped part.
    let base = path_base(&pkg).to_string();
    let stem = file.strip_suffix(".tgz").unwrap_or(&file);
    let Some(v) = stem.strip_prefix(&format!("{base}-")) else {
        return String::new();
    };
    if v.is_empty() || !file.ends_with(".tgz") {
        return String::new();
    }
    v.to_string()
}

/// Rewrites a raw packument (see [`rewrite_packument_top`]). An unparseable body
/// passes through unchanged (never block on unknown).
pub(crate) fn rewrite_packument(
    body: &[u8],
    base: &str,
    repo_name: &str,
    pkg: &str,
    age: &AgePolicyConfig,
    now: DateTime<Utc>,
) -> (Bytes, i64) {
    let Ok(top) = serde_json::from_slice::<RawObject>(body) else {
        return (Bytes::copy_from_slice(body), 0);
    };
    match rewrite_packument_top(top, base, repo_name, pkg, age, now) {
        Some((out, removed, _)) => (Bytes::from(out), removed),
        None => (Bytes::copy_from_slice(body), 0),
    }
}

/// Rewrites `dist.tarball` URLs to forklift and removes versions whose upstream
/// publish time violates a blocking age policy. It returns the transformed JSON,
/// the number of versions removed, and the earliest instant at which a removed
/// version leaves the cooldown (`None` when nothing was removed: the output then
/// depends on nothing but its inputs). `None` overall means the caller should
/// serve the original bytes.
///
/// The packument is taken as top-level raw fragments rather than a fully
/// expanded value tree. A popular package's packument is several megabytes and
/// expanding every version manifest into nested maps costs an order of magnitude
/// more memory than the JSON itself; multiplied across a concurrent install
/// burst that is what OOMs the pod. Here each version stays as raw bytes and
/// only the small `dist` object of a version being rewritten is materialized, so
/// peak memory tracks the document size instead of a multiple of it.
pub(crate) fn rewrite_packument_top(
    mut top: RawObject,
    base: &str,
    repo_name: &str,
    pkg: &str,
    age: &AgePolicyConfig,
    now: DateTime<Utc>,
) -> Option<(Vec<u8>, i64, Option<DateTime<Utc>>)> {
    let mut versions: RawObject = BTreeMap::new();
    if let Some(raw) = top.get("versions")
        && !raw.get().is_empty()
    {
        versions = serde_json::from_str(raw.get()).ok()?;
    }

    let mut removed = 0i64;
    let mut until: Option<DateTime<Utc>> = None;
    let mut blocked: std::collections::HashSet<String> = std::collections::HashSet::new();
    if age.enabled && age.action == ACTION_BLOCK {
        // Best-effort; a missing or odd `time` map means no block.
        let times: BTreeMap<String, String> = top
            .get("time")
            .and_then(|raw| serde_json::from_str(raw.get()).ok())
            .unwrap_or_default();
        let min_age =
            chrono::TimeDelta::from_std(age.min_age.d()).unwrap_or(chrono::TimeDelta::MAX);
        for ver in versions.keys() {
            let Some(ts) = times.get(ver) else { continue };
            let Ok(pub_at) = DateTime::parse_from_rfc3339(ts) else {
                continue;
            };
            let pub_at = pub_at.with_timezone(&Utc);
            if now - pub_at < min_age {
                blocked.insert(ver.clone());
                // A version never becomes blocked later (age only grows and
                // max_age is not applied here), so the first unblock is the only
                // future instant at which this render changes.
                let release = pub_at + min_age;
                if until.is_none_or(|u| release < u) {
                    until = Some(release);
                }
            }
        }
    }

    let keys: Vec<String> = versions.keys().cloned().collect();
    for ver in keys {
        if blocked.contains(&ver) {
            versions.remove(&ver);
            removed += 1;
            continue;
        }
        if let Some(nv) =
            rewrite_version_tarball(versions[&ver].get().as_bytes(), base, repo_name, pkg)
            && let Ok(parsed) = RawValue::from_string(nv)
        {
            versions.insert(ver, parsed);
        }
    }

    // Prune dist-tags pointing at removed versions. `latest` is remapped to the
    // highest remaining stable version so a bare `npm install <pkg>` resolves to
    // the newest policy-compliant release instead of failing.
    if removed > 0
        && let Some(raw) = top.get("dist-tags")
        && let Ok(mut tags) = serde_json::from_str::<BTreeMap<String, String>>(raw.get())
    {
        let mut remap_latest = false;
        tags.retain(|tag, ver| {
            if blocked.contains(ver) {
                remap_latest = remap_latest || tag == "latest";
                return false;
            }
            true
        });
        if remap_latest {
            let best = highest_stable_version(&versions);
            if !best.is_empty() {
                tags.insert("latest".to_string(), best);
            }
        }
        if let Ok(nb) = serde_json::to_string(&tags)
            && let Ok(parsed) = RawValue::from_string(nb)
        {
            top.insert("dist-tags".to_string(), parsed);
        }
    }

    if top.contains_key("versions") {
        let nb = marshal_raw_object(&versions)?;
        top.insert(
            "versions".to_string(),
            RawValue::from_string(String::from_utf8(nb).ok()?).ok()?,
        );
    }
    let out = marshal_raw_object(&top)?;
    Some((out, removed, until))
}

/// Rewrites the `dist.tarball` URL of a single raw version manifest to point
/// back at forklift. Returns `None` when nothing was rewritten (the caller
/// leaves the original fragment untouched).
///
/// The rewrite is a splice of the tarball string literal, not a decode of the
/// manifest into a map and back. This runs once per version, so an installer
/// resolving a few hundred packages runs it tens of thousands of times: the
/// round trip through a fragment map was half the cost of serving a packument,
/// because the re-encode had to walk, validate and compact every fragment it had
/// just been handed. Splicing also leaves every other byte of the manifest
/// exactly as upstream published it, where the round trip reordered the fields
/// alphabetically for no reason.
pub(super) fn rewrite_version_tarball(
    raw: &[u8],
    base: &str,
    repo_name: &str,
    pkg: &str,
) -> Option<String> {
    let (dist_start, dist_end) = json_field_span(raw, "dist")?;
    let (tb_start, tb_end) = json_field_span(&raw[dist_start..dist_end], "tarball")?;
    let (tb_start, tb_end) = (dist_start + tb_start, dist_start + tb_end);
    let tb: String = serde_json::from_slice(&raw[tb_start..tb_end]).ok()?;
    let nb = serde_json::to_string(&format!(
        "{base}/npm/{repo_name}/{pkg}/-/{}",
        path_base(&tb)
    ))
    .ok()?;
    let mut out = Vec::with_capacity(raw.len() - (tb_end - tb_start) + nb.len());
    out.extend_from_slice(&raw[..tb_start]);
    out.extend_from_slice(nb.as_bytes());
    out.extend_from_slice(&raw[tb_end..]);
    String::from_utf8(out).ok()
}

/// Locates the value of a top-level key inside the JSON object `obj` and returns
/// the byte range that value occupies. The object is walked structurally rather
/// than searched textually, so the same key appearing inside a nested value
/// cannot be mistaken for the field itself. Returns `None` when `obj` is not an
/// object or has no such key.
pub(super) fn json_field_span(obj: &[u8], key: &str) -> Option<(usize, usize)> {
    let mut i = skip_ws(obj, 0);
    if *obj.get(i)? != b'{' {
        return None;
    }
    i = skip_ws(obj, i + 1);
    if *obj.get(i)? == b'}' {
        return None;
    }
    loop {
        i = skip_ws(obj, i);
        if *obj.get(i)? != b'"' {
            return None;
        }
        let name_end = skip_string(obj, i)?;
        let name: String = serde_json::from_slice(&obj[i..name_end]).ok()?;
        i = skip_ws(obj, name_end);
        if *obj.get(i)? != b':' {
            return None;
        }
        // Only the colon and whitespace can separate a name from its value, so
        // the value starts at the first byte that is neither.
        let start = skip_ws(obj, i + 1);
        let end = skip_value(obj, start)?;
        if name == key {
            return Some((start, end));
        }
        i = skip_ws(obj, end);
        match *obj.get(i)? {
            b',' => i += 1,
            b'}' => return None,
            _ => return None,
        }
    }
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// Returns the index one past the closing quote of the string starting at `i`.
fn skip_string(b: &[u8], i: usize) -> Option<usize> {
    debug_assert_eq!(b.get(i), Some(&b'"'));
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// Returns the index one past the JSON value starting at `i`.
fn skip_value(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => skip_string(b, i),
        b'{' | b'[' => {
            let mut depth = 0i32;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = skip_string(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            None
        }
        _ => {
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
            {
                j += 1;
            }
            if j == i { None } else { Some(j) }
        }
    }
}

/// Encodes an object whose members are already encoded. It exists because re-serializing a
/// fragment map re-walks, validates and compacts every fragment, which on a packument means a
/// second full pass over a document the caller just produced. Returns `None` when a key cannot
/// be encoded.
pub(super) fn marshal_raw_object(members: &RawObject) -> Option<Vec<u8>> {
    let size: usize = members
        .iter()
        .map(|(k, v)| k.len() + v.get().len() + 4)
        .sum::<usize>()
        + 2;
    let mut buf = Vec::with_capacity(size);
    buf.push(b'{');
    for (i, (k, v)) in members.iter().enumerate() {
        if i > 0 {
            buf.push(b',');
        }
        let name = serde_json::to_string(k).ok()?;
        buf.extend_from_slice(name.as_bytes());
        buf.push(b':');
        buf.extend_from_slice(v.get().as_bytes());
    }
    buf.push(b'}');
    Some(buf)
}

#[cfg(test)]
pub(crate) mod tests {
    mod npm_gatehold {
        use std::time::Duration;

        use axum::Router;
        use axum::extract::Path;
        use axum::routing::any;
        use http::{Method, Request, StatusCode};
        use tower::ServiceExt as _;

        use crate::meta;
        use crate::repoconfig;

        use crate::repo::MAX_CONCURRENT_REWRITES;
        use crate::testing::repo::send;
        use crate::testing::repo::{mk_format_repo, mux, new_test_manager, spawn_upstream};

        /// This is the regression test for the stall that made a slow client look like a
        /// forklift outage. The rewrite gate has [`MAX_CONCURRENT_REWRITES`] slots;
        /// while the slot was held across the response write, clients that read slowly
        /// held every slot and every other packument queued behind them. Installers saw
        /// no status code at all, just their own fetch timeout, which is the same
        /// signature as a slow upstream and just as total.
        ///
        /// An axum handler returns a response and the server streams its body afterwards, so the Rust
        /// spelling of "the client has stopped reading" is a response whose body is never consumed: the
        /// requests below stay outstanding with their bodies unread, which still fails if a future
        /// change parks a slot inside the response.
        #[tokio::test]
        async fn npm_packument_write_does_not_hold_a_rewrite_slot() {
            let upstream = spawn_upstream(Router::new().route(
                "/{pkg}",
                any(|Path(pkg): Path<String>| async move {
                    format!(
                        r#"{{"name":"{pkg}","dist-tags":{{"latest":"1.0.0"}},"versions":{{"1.0.0":{{"dist":{{"tarball":"http://up/{pkg}/-/{pkg}-1.0.0.tgz"}}}}}},"time":{{"1.0.0":"2020-01-01T00:00:00Z"}}}}"#
                    )
                }),
            ))
            .await;

            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "npmjs",
                meta::FORMAT_NPM,
                meta::TYPE_PROXY,
                &upstream,
                repoconfig::default(),
            )
            .await;
            let app = mux(&tm.manager);

            // Park more unread responses than the gate has slots, each on its own
            // package so none of them can be answered from the render cache.
            let mut parked = Vec::with_capacity(MAX_CONCURRENT_REWRITES + 1);
            for index in 0..=MAX_CONCURRENT_REWRITES {
                let request = Request::builder()
                    .method(Method::GET)
                    .uri(format!("/npm/npmjs/stalled{index}"))
                    .body(axum::body::Body::empty())
                    .expect("build request");
                let response = app
                    .clone()
                    .oneshot(request)
                    .await
                    .expect("router is infallible");
                assert_eq!(response.status(), StatusCode::OK, "stalled{index}");
                parked.push(response);
            }

            // Every parked request is now holding an unread response body. If the write
            // held a slot, the gate would be full and this request could not be served.
            let fresh = tokio::time::timeout(
                Duration::from_secs(5),
                send(
                    &app,
                    Request::builder()
                        .method(Method::GET)
                        .uri("/npm/npmjs/fresh")
                        .body(axum::body::Body::empty())
                        .expect("build request"),
                ),
            )
            .await
            .expect("packument queued behind clients that are stalled in their response write");
            assert_eq!(
                fresh.status,
                StatusCode::OK,
                "packument served while clients stall in Write"
            );
            drop(parked);
        }
    }

    mod npm_rewrite {
        //!
        //! The `metadata_rewrite_*` metrics it refers to are covered by `rewritegate_test.rs`.

        use std::collections::BTreeMap;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicI64, Ordering};

        use axum::Router;
        use axum::routing::any;
        use http::{Method, StatusCode, Uri};
        use serde_json::value::RawValue;

        use crate::meta;
        use crate::repoconfig;

        use crate::repo::npm::{json_field_span, marshal_raw_object, rewrite_version_tarball};
        use crate::testing::repo::{call, mk_format_repo, mux, new_test_manager, spawn_upstream};

        #[test]
        fn json_field_span_locates_values() {
            let cases = [
                (
                    "string value",
                    r#"{"a":"x","tarball":"http://u/p.tgz"}"#,
                    "tarball",
                    r#""http://u/p.tgz""#,
                ),
                (
                    "whitespace around colon",
                    "{\"tarball\" \t: \n \"u\"}",
                    "tarball",
                    r#""u""#,
                ),
                (
                    "object value",
                    r#"{"dist":{"tarball":"u"},"z":1}"#,
                    "dist",
                    r#"{"tarball":"u"}"#,
                ),
                (
                    "array value",
                    r#"{"k":[1,2,{"k":3}]}"#,
                    "k",
                    r#"[1,2,{"k":3}]"#,
                ),
                ("first of duplicates", r#"{"k":1,"k":2}"#, "k", "1"),
                ("escaped key", r#"{"a\"b":7}"#, "a\"b", "7"),
            ];
            for (name, obj, key, want) in cases {
                let (start, end) = json_field_span(obj.as_bytes(), key)
                    .unwrap_or_else(|| panic!("{name}: key not found"));
                assert_eq!(&obj[start..end], want, "{name}: span");
            }
        }

        /// The key must be matched as a field of this object, never as text that happens
        /// to appear inside one of its values: a splice guided by a textual search would
        /// corrupt the manifest.
        #[test]
        fn json_field_span_ignores_nested_and_missing_keys() {
            for (name, obj) in [
                ("nested only", r#"{"dist":{"tarball":"u"}}"#),
                ("inside string", r#"{"description":"the tarball is here"}"#),
                ("absent", r#"{"a":1}"#),
                ("not an object", r#"["tarball","u"]"#),
                ("truncated", r#"{"tarball":"#),
            ] {
                assert!(
                    json_field_span(obj.as_bytes(), "tarball").is_none(),
                    "{name}: matched a key that is not a field of this object"
                );
            }
        }

        /// Everything except the tarball literal has to survive byte for byte, field
        /// order included: the manifest is what the client verifies against, and the
        /// integrity hash next to the URL must not be re-encoded on the way through.
        #[test]
        fn rewrite_version_tarball_splices_only_the_url() {
            let raw = r#"{"zzz":"last","name":"pkg","dist":{"integrity":"sha512-AAA==","tarball":"https://registry.npmjs.org/pkg/-/pkg-1.0.0.tgz","shasum":"abc"},"aaa":{"nested":[1,2]}}"#;
            let out = rewrite_version_tarball(
                raw.as_bytes(),
                "https://forklift.example.com",
                "npmjs",
                "pkg",
            )
            .expect("rewrite reported no change");
            let want = raw.replace(
                r#""https://registry.npmjs.org/pkg/-/pkg-1.0.0.tgz""#,
                r#""https://forklift.example.com/npm/npmjs/pkg/-/pkg-1.0.0.tgz""#,
            );
            assert_eq!(out, want, "rewrite changed more than the URL");
        }

        #[test]
        fn rewrite_version_tarball_leaves_manifests_without_a_dist_url() {
            for (name, raw) in [
                ("no dist", r#"{"name":"pkg"}"#),
                ("no tarball", r#"{"dist":{"shasum":"abc"}}"#),
                ("dist is not an object", r#"{"dist":"nope"}"#),
            ] {
                assert!(
                    rewrite_version_tarball(
                        raw.as_bytes(),
                        "https://f.example.com",
                        "npmjs",
                        "pkg"
                    )
                    .is_none(),
                    "{name}: reported a rewrite"
                );
            }
        }

        /// Sorted keys keep the served document byte-stable across requests, which is
        /// what `json.Marshal` gave before and what any content hash over the output
        /// will depend on.
        #[test]
        fn marshal_raw_object_is_sorted_and_verbatim() {
            let mut members: BTreeMap<String, Box<RawValue>> = BTreeMap::new();
            for (key, value) in [("b", r#"{"x":1}"#), ("a", "[1,2]"), ("c\"d", r#""v""#)] {
                members.insert(
                    key.to_string(),
                    RawValue::from_string(value.to_string()).expect("raw value"),
                );
            }
            let got = marshal_raw_object(&members).expect("marshal");
            let want = r#"{"a":[1,2],"b":{"x":1},"c\"d":"v"}"#;
            assert_eq!(String::from_utf8_lossy(&got), want);
            let second = marshal_raw_object(&members).expect("marshal");
            assert_eq!(got, second, "output is not stable across calls");
            assert_eq!(
                String::from_utf8_lossy(&marshal_raw_object(&BTreeMap::new()).expect("marshal")),
                "{}",
                "empty object"
            );
        }

        /// With the age gate off there is nothing the packument timestamp can decide, so
        /// the tarball path must not re-read the whole document to obtain it. On a cold
        /// install that read lands once per package, on top of the packument fetch.
        #[tokio::test]
        async fn npm_tarball_skips_packument_read_when_age_gate_off() {
            let packument_hits = Arc::new(AtomicI64::new(0));
            let upstream_hits = Arc::clone(&packument_hits);
            let upstream = spawn_upstream(Router::new().fallback(any(move |uri: Uri| {
                let hits = Arc::clone(&upstream_hits);
                async move {
                    if uri.path().contains("/-/") {
                        return "TARBALLBYTES".to_string();
                    }
                    hits.fetch_add(1, Ordering::SeqCst);
                    r#"{"name":"pkg","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"dist":{"tarball":"http://up/pkg/-/pkg-1.0.0.tgz"}}},"time":{"1.0.0":"2024-01-01T00:00:00Z"}}"#
                        .to_string()
                }
            })))
            .await;

            let mut cfg = repoconfig::default();
            cfg.age_policy.enabled = false;
            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "p",
                meta::FORMAT_NPM,
                meta::TYPE_PROXY,
                &upstream,
                cfg,
            )
            .await;

            let resp = call(
                &mux(&tm.manager),
                Method::GET,
                "/npm/p/pkg/-/pkg-1.0.0.tgz",
                "",
            )
            .await;
            assert_eq!(resp.status, StatusCode::OK, "tarball: {}", resp.text());
            assert_eq!(
                packument_hits.load(Ordering::SeqCst),
                0,
                "packument fetched for a tarball with the age gate off"
            );
        }
    }

    mod npm_stall {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicI64, Ordering};
        use std::time::Duration;

        use async_trait::async_trait;
        use axum::Router;
        use axum::body::Body;
        use axum::response::{IntoResponse, Response};
        use axum::routing::any;
        use http::{Method, StatusCode, Uri};
        use parking_lot::Mutex;
        use prometheus::Registry;

        use crate::meta;
        use crate::repoconfig;
        use crate::storage::{BlobStore, FsStore, Result as StorageResult, SeekableStore};

        use crate::repo::{Engine, FetchSpec, MAX_CONCURRENT_REWRITES, Manager};
        use crate::testing::repo::{
            call, mk_format_repo, mk_repo, mux, new_test_manager, spawn_upstream,
        };

        /// Renders a minimal one-version packument for `pkg`.
        fn packument(pkg: &str) -> String {
            format!(
                r#"{{"name":"{pkg}","dist-tags":{{"latest":"1.0.0"}},"versions":{{"1.0.0":{{"dist":{{"tarball":"http://up/{pkg}/-/{pkg}-1.0.0.tgz"}}}}}},"time":{{"1.0.0":"2020-01-01T00:00:00Z"}}}}"#
            )
        }

        /// The regression test for the stall that made a slow upstream look like a total
        /// outage: the rewrite gate has only [`MAX_CONCURRENT_REWRITES`] slots, and
        /// while a cache miss held a slot for the whole upstream stream, every other
        /// packument queued behind it — cache hits included, because serving a cached
        /// packument needs a slot too. Installers saw no status code at all, just their
        /// own fetch timeout.
        #[tokio::test]
        async fn npm_slow_upstream_does_not_stall_cached_packuments() {
            // Broadcast so every stalled upstream handler is released at once.
            let (release, _) = tokio::sync::broadcast::channel::<()>(1);
            let handler_release = release.clone();
            let upstream = spawn_upstream(Router::new().fallback(any(move |uri: Uri| {
                let release = handler_release.clone();
                async move {
                    let pkg = uri.path().trim_start_matches('/').to_string();
                    if pkg == "warm" {
                        return packument(&pkg).into_response();
                    }
                    // Send headers plus a partial body, then stall mid-stream: the fetch
                    // is past the request future and inside the body copy, which is
                    // exactly where a slow upstream parks a request.
                    let mut released = release.subscribe();
                    let (chunks, rx) =
                        tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(2);
                    chunks
                        .send(Ok(bytes::Bytes::from(format!(
                            r#"{{"name":"{pkg}","versions":{{"#
                        ))))
                        .await
                        .expect("head chunk");
                    tokio::spawn(async move {
                        let _ = released.recv().await;
                        let _ = chunks.send(Ok(bytes::Bytes::from_static(b"}}"))).await;
                    });
                    Response::builder()
                        .header(http::header::CONTENT_TYPE, "application/json")
                        .body(Body::from_stream(
                            tokio_stream::wrappers::ReceiverStream::new(rx),
                        ))
                        .expect("stalled response")
                }
            })))
            .await;

            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "npmjs",
                meta::FORMAT_NPM,
                meta::TYPE_PROXY,
                &upstream,
                repoconfig::default(),
            )
            .await;
            let app = mux(&tm.manager);

            // Warm the cache for one package while the upstream is still responsive.
            let resp = call(&app, Method::GET, "/npm/npmjs/warm", "").await;
            assert_eq!(resp.status, StatusCode::OK, "warm packument");

            // Park more concurrent misses than the gate has slots.
            let stalled = 2 * MAX_CONCURRENT_REWRITES;
            let mut tasks = Vec::with_capacity(stalled);
            for index in 0..stalled {
                let app = app.clone();
                tasks.push(tokio::spawn(async move {
                    call(&app, Method::GET, &format!("/npm/npmjs/slow{index}"), "").await
                }));
            }
            // Give the stalled fetches time to reach the upstream body copy.
            tokio::time::sleep(Duration::from_millis(200)).await;

            let cached = tokio::time::timeout(
                Duration::from_secs(5),
                call(&app, Method::GET, "/npm/npmjs/warm", ""),
            )
            .await
            .expect("cached packument blocked behind stalled upstream fetches");
            assert_eq!(
                cached.status,
                StatusCode::OK,
                "cached packument during slow upstream"
            );

            let _ = release.send(());
            for task in tasks {
                let _ = task.await;
            }
        }

        /// Concurrent requests for the same packument collapse into one upstream
        /// round-trip. Without coalescing an install burst multiplies every uncached
        /// package by the number of parallel jobs, which is what turns upstream slowness
        /// into an upstream rate limit.
        #[tokio::test]
        async fn npm_packument_coalesces_concurrent_fetches() {
            let hits = Arc::new(AtomicI64::new(0));
            let upstream_hits = Arc::clone(&hits);
            let upstream = spawn_upstream(Router::new().fallback(any(move || {
                let hits = Arc::clone(&upstream_hits);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    // Widen the window callers coalesce into.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    packument("left-pad")
                }
            })))
            .await;

            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "npmjs",
                meta::FORMAT_NPM,
                meta::TYPE_PROXY,
                &upstream,
                repoconfig::default(),
            )
            .await;
            let app = mux(&tm.manager);

            let mut tasks = Vec::with_capacity(8);
            for _ in 0..8 {
                let app = app.clone();
                tasks.push(tokio::spawn(async move {
                    call(&app, Method::GET, "/npm/npmjs/left-pad", "")
                        .await
                        .status
                }));
            }
            for (index, task) in tasks.into_iter().enumerate() {
                let status = task.await.expect("request task");
                assert_eq!(status, StatusCode::OK, "request {index}");
            }
            assert_eq!(
                hits.load(Ordering::SeqCst),
                1,
                "upstream hits, want 1 (coalesced)"
            );
        }

        /// An upstream 429 reaches the client as a retryable status with `Retry-After`,
        /// and the coordinate is then cooled down so client retries are answered
        /// locally. Previously the packument path turned a 429 into a bare 502 and
        /// re-forwarded every retry upstream.
        #[tokio::test]
        async fn npm_packument_relays_upstream_rate_limit() {
            let hits = Arc::new(AtomicI64::new(0));
            let upstream_hits = Arc::clone(&hits);
            let upstream = spawn_upstream(Router::new().fallback(any(move || {
                let hits = Arc::clone(&upstream_hits);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        // Above the default upstream-cooldown floor.
                        [(http::header::RETRY_AFTER, "30")],
                    )
                }
            })))
            .await;

            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "npmjs",
                meta::FORMAT_NPM,
                meta::TYPE_PROXY,
                &upstream,
                repoconfig::default(),
            )
            .await;
            let app = mux(&tm.manager);

            let resp = call(&app, Method::GET, "/npm/npmjs/hot-pkg", "").await;
            assert_eq!(
                resp.status,
                StatusCode::TOO_MANY_REQUESTS,
                "first request, want 429 relayed"
            );
            assert_eq!(resp.header("Retry-After"), "30");

            let resp = call(&app, Method::GET, "/npm/npmjs/hot-pkg", "").await;
            assert!(
                resp.status == StatusCode::SERVICE_UNAVAILABLE
                    || resp.status == StatusCode::TOO_MANY_REQUESTS,
                "second request = {}, want a retryable status from the cooldown",
                resp.status
            );
            assert!(
                !resp.header("Retry-After").is_empty(),
                "cooled-down response carries no Retry-After"
            );
            assert_eq!(
                hits.load(Ordering::SeqCst),
                1,
                "upstream hits, want 1 (second request served from cooldown)"
            );
        }

        /// Wraps a blob store and runs `on_delete` before each delete.
        struct HookedBlobStore {
            inner: Arc<dyn BlobStore>,
            on_delete: Arc<dyn Fn(&str) + Send + Sync>,
        }

        #[async_trait]
        impl BlobStore for HookedBlobStore {
            async fn put(
                &self,
                r: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
            ) -> StorageResult<(String, i64)> {
                self.inner.put(r).await
            }
            async fn open(
                &self,
                digest: &str,
            ) -> StorageResult<(Box<dyn tokio::io::AsyncRead + Send + Unpin>, i64)> {
                self.inner.open(digest).await
            }
            async fn exists(&self, digest: &str) -> StorageResult<bool> {
                self.inner.exists(digest).await
            }
            async fn delete(&self, digest: &str) -> StorageResult<()> {
                (self.on_delete)(digest);
                self.inner.delete(digest).await
            }
            fn as_seekable(&self) -> Option<&dyn SeekableStore> {
                self.inner.as_seekable()
            }
        }

        /// A garbage-collection batch must not hold the GC write lock for its whole
        /// duration: a reference-creating write (upload or proxy cache write, both
        /// taking the `gc_mu` read guard) has to be able to interleave between digests.
        /// Holding the lock across a 256-digest batch stalled every write for as long as
        /// the batch took, which on an object-store backend is one API round-trip per
        /// digest.
        #[tokio::test]
        async fn sweeper_releases_gc_lock_between_blobs() {
            crate::server::install_crypto_provider();
            let db_dir = tempfile::tempdir().expect("temp dir");
            let blob_dir = tempfile::tempdir().expect("temp dir");
            let store = Arc::new(
                crate::meta::Store::open(db_dir.path().join("repo.db"))
                    .await
                    .expect("open store"),
            );
            let order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
            let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel::<()>(16);
            let delete_order = Arc::clone(&order);
            let blobs: Arc<dyn BlobStore> = Arc::new(HookedBlobStore {
                inner: Arc::new(FsStore::new(blob_dir.path()).expect("blob store")),
                on_delete: Arc::new(move |_digest| {
                    delete_order.lock().push("delete");
                    let _ = entered_tx.try_send(());
                    // Yield so a writer waiting on `gc_mu` can be woken by the unlock
                    // that follows this digest rather than only after the whole batch.
                    std::thread::sleep(Duration::from_millis(5));
                }),
            });
            let engine = Engine::new(Arc::clone(&store), blobs, &Registry::new());
            let manager = Manager::new(Arc::clone(&engine), Arc::clone(&store), None, None, None);
            let repo = mk_repo(
                &store,
                "mvn-gclock",
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
            let app = mux(&manager);

            // Upload then delete several artifacts so their blobs are unreferenced.
            const BLOBS: i64 = 10;
            for index in 0..BLOBS {
                let path = format!("/maven/mvn-gclock/com/example/app/1.{index}/app-1.{index}.jar");
                let resp = call(&app, Method::PUT, &path, &format!("BYTES-{index}")).await;
                assert_eq!(resp.status, StatusCode::CREATED, "put {index}");
                let relative = path
                    .strip_prefix("/maven/mvn-gclock/")
                    .expect("relative path");
                store
                    .delete_artifact(repo.id, relative)
                    .await
                    .unwrap_or_else(|e| panic!("delete artifact {index}: {e}"));
            }

            let sweeper = Arc::clone(&engine);
            let swept = tokio::spawn(async move { sweeper.sweep_once(Duration::ZERO).await });

            entered_rx.recv().await.expect("the batch is underway");
            let store_engine = Arc::clone(&engine);
            let store_order = Arc::clone(&order);
            let repo_for_store = repo.clone();
            let stored = tokio::spawn(async move {
                let result = store_engine
                    .store_artifact(
                        &FetchSpec {
                            repo: repo_for_store,
                            path: "com/example/app/2.0/app-2.0.jar".to_string(),
                            ..FetchSpec::blank()
                        },
                        Box::pin(std::io::Cursor::new(b"INTERLEAVED".to_vec())),
                        "application/java-archive",
                        None,
                        "",
                    )
                    .await;
                store_order.lock().push("store");
                result
            });

            tokio::time::timeout(Duration::from_secs(5), stored)
                .await
                .expect("store blocked for the whole sweep batch")
                .expect("store task")
                .expect("interleaved store");
            swept.await.expect("sweep task");

            let order = order.lock();
            assert_ne!(
                order.last().copied(),
                Some("store"),
                "store completed only after every delete: {order:?}"
            );
        }
    }
}
