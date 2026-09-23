//! Serves the package-format protocols (Maven, npm, Cargo, Go, PyPI, raw, OCI)
//! over Hosted and Proxy (cached upstream) repositories. The [`Engine`] holds
//! the shared cache/store logic; per-format files translate protocol requests
//! into engine operations.

use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::response::Response;
use chrono::{DateTime, Utc};
use futures_util::TryStreamExt;
use http::request::Parts;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use prometheus::{CounterVec, Gauge, HistogramOpts, HistogramVec, Opts, Registry};
use tokio::io::AsyncRead;
use tokio_util::io::ReaderStream;
use url::Url;

use crate::meta::{self, Artifact, Repository, Store};
use crate::repoconfig::{
    Config, UPSTREAM_AUTH_BASIC, UPSTREAM_AUTH_BEARER, UPSTREAM_AUTH_HEADER, UpstreamAuthConfig,
};
use crate::server::http_error;
use crate::storage::BlobStore;

mod agepolicy;
mod dangling;
mod flight;
mod native_upload;
mod negcache;
mod policypipeline;
mod reaper;
mod rendercache;
mod router;
mod safedial;
mod seed;
mod sweeper;
mod upload;
mod upstreamurl;

mod approvalgate;
pub(crate) mod cargo;
pub(crate) mod cargo_search;
mod gomod;
mod group;
mod group_metadata;
mod licensegate;
mod licensescan;
mod maven;
mod raw;
mod vulngate;
mod vulnscan;

mod npm;
mod oci;
mod oci_detail;
mod oci_images;
mod oci_proxy;
mod oci_prune;
mod oci_push;
mod pypi;
mod uiupload;
mod uiupload_cargo;
mod uiupload_go;
mod uiupload_lifecycle;
mod uiupload_maven;
mod uiupload_npm;
mod uiupload_pypi;

pub use dangling::DanglingRef;
pub use group::validate_group_members;
pub use licensescan::license_coordinate;
pub use oci::oci_manifest_path_public as oci_manifest_path;
pub use oci_detail::OCIArtifactDetail;
pub use oci_images::OCITagInfo;
pub use seed::{DEFAULT_REPOSITORIES, DefaultRepo, is_default_repo, seed_defaults};
pub use upload::{
    MAX_UI_UPLOAD_BYTES, UploadError, UploadPlan, UploadResult, UploadValidationError,
};
pub use upstreamurl::upstream_package_url;
pub use vulnscan::osv_ecosystem;
pub use vulnscan::{ScanRatio, vuln_coordinate};

pub use router::Manager;
pub use uiupload::{
    ArtifactUploadManifest, ArtifactUploadResult, ReceiveOutcome, UploadProblem, Uploader,
};
pub use uiupload_lifecycle::PublicationLifecycleResult;

pub(crate) use agepolicy::{AgeDecision, evaluate_age};
pub(crate) use dangling::DanglingRegistry;
pub(crate) use flight::Flight;
pub(crate) use negcache::NegCache;
pub(crate) use rendercache::{RENDER_CACHE_BYTES, RENDER_CACHE_TTL, RenderCache};
pub(crate) use router::Resolved;

/// How long an upstream coordinate is shielded from re-fetching after a 429/503
/// with no usable Retry-After, so client retries are answered locally instead of
/// forwarded into a rate-limit storm.
pub(crate) const DEFAULT_UPSTREAM_COOLDOWN: Duration = Duration::from_secs(15);
/// Caps a Retry-After-derived cooldown so a hostile or buggy upstream cannot
/// park a coordinate for an unbounded time.
pub(crate) const MAX_UPSTREAM_COOLDOWN: Duration = Duration::from_secs(5 * 60);
/// Bounds how many metadata parse/rewrite operations (packument and
/// simple-index documents decoded into generic maps) run at once. Decoding a
/// large index into a dynamic value costs several times the document size, so an
/// install burst of hundreds of packages must queue here instead of multiplying
/// that cost by the request count and OOMing the pod.
pub(crate) const MAX_CONCURRENT_REWRITES: usize = 4;
/// Caps the rendered metadata resident outside the rewrite gate, waiting on
/// client writes. A burst of a few hundred packuments would otherwise hold every
/// rendered body at once.
pub(crate) const MAX_INFLIGHT_RENDER_BYTES: i64 = 128 << 20;
/// Caps how large a metadata document (packument, simple index) may be before it
/// is truncated, whether it is cached first or buffered in memory. Real-world
/// documents are orders of magnitude smaller; the cap keeps a hostile or broken
/// upstream from filling the blob store or the heap.
pub(crate) const MAX_METADATA_BYTES: i64 = 64 << 20;

/// Bounds how often a served artifact's `last_accessed_at` is rewritten. The
/// timestamp only feeds LRU eviction and idle retention, which operate on
/// cache-TTL and multi-day scales, so skipping the UPDATE while the stored value
/// is this recent keeps hot-path serving read-only without changing either
/// consumer's behavior.
pub(crate) const TOUCH_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Classifies a request target, which selects the cache freshness policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Immutable artifact bytes.
    #[default]
    Artifact,
    /// Mutable index documents (revalidated on `metadata_ttl`).
    Metadata,
}

/// An injectable clock.
pub(crate) type NowFn = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// The wall clock, the production value of every [`NowFn`].
pub(crate) fn system_clock() -> NowFn {
    Arc::new(Utc::now)
}

/// Invoked after a proxy fetch caches an artifact.
pub(crate) type OnStoreFn = Arc<dyn Fn(Repository, String) + Send + Sync>;

/// Runs after age evaluation and immediately before serving. `Some(response)`
/// means the gate answered the request and evaluation stops.
pub(crate) type FinalGate =
    Arc<dyn Fn(Arc<Parts>) -> Pin<Box<dyn Future<Output = Option<Response>> + Send>> + Send + Sync>;

/// Derives the upstream release time from a proxy response.
pub(crate) type ExtractPublished =
    Arc<dyn Fn(&reqwest::Response) -> Option<DateTime<Utc>> + Send + Sync>;

/// A body handed to the engine for storage.
pub(crate) type StoreBody = Pin<Box<dyn AsyncRead + Send + Unpin>>;

/// Implements the shared repository cache/store logic.
pub struct Engine {
    pub(crate) store: Arc<Store>,
    pub(crate) blobs: Arc<dyn BlobStore>,
    pub(crate) client: reqwest::Client,
    /// Fetches client-supplied URLs (`FetchSpec::untrusted_url`); its
    /// destinations are screened so private/loopback/link-local addresses are
    /// refused, preventing SSRF.
    pub(crate) ext_client: reqwest::Client,
    pub(crate) neg: NegCache,
    /// Shields a coordinate from re-fetching for a short window after the
    /// upstream returned 429/503, keyed like `neg` by repo-relative path.
    pub(crate) cool: NegCache,
    /// Collapses concurrent identical proxy fetches into one upstream
    /// round-trip so a cold cache cannot fan a burst of identical requests out
    /// into a matching burst of upstream requests.
    pub(crate) flight: Flight,
    /// Caches rendered metadata documents so a repeat request skips the decode
    /// and rewrite (and therefore the rewrite gate) entirely.
    pub(crate) renders: RenderCache,
    /// Bounds the rendered documents held outside the rewrite gate while they
    /// are written to clients. Releasing the gate before the write is what keeps
    /// a slow reader from capping metadata throughput at the gate width, but the
    /// body stays resident until the write finishes, so the memory the gate used
    /// to bound implicitly needs an explicit bound here.
    pub(crate) render_bytes: Arc<tokio::sync::Semaphore>,
    /// A counting semaphore (see [`MAX_CONCURRENT_REWRITES`]) acquired around
    /// memory-heavy metadata rewrites.
    pub(crate) rewrite_gate: Arc<tokio::sync::Semaphore>,
    /// Serializes blob garbage collection against reference-creating writes. A
    /// path that writes blob bytes and then records the artifact that references
    /// them (`blobs.put` -> `put_artifact`/`record_upload`) holds the read guard
    /// across both steps; the sweeper holds the write guard while it reclaims a
    /// batch. This closes the window where the sweeper would otherwise delete
    /// bytes that an in-flight write is about to reference (content-addressed
    /// digests collide by design). Writers share the read guard and never block
    /// each other; only the periodic sweeper's brief exclusive hold does.
    pub(crate) gc_mu: tokio::sync::RwLock<()>,
    pub(crate) user_agent: String,
    now: parking_lot::RwLock<NowFn>,
    /// When set, invoked after a proxy fetch caches an artifact, so the manager
    /// can enqueue vulnerability/license scanning for it. Best-effort and must
    /// not block the serving path; `None` disables it.
    pub(crate) on_store: parking_lot::RwLock<Option<OnStoreFn>>,

    pub(crate) cache_hits: CounterVec,
    pub(crate) cache_miss: CounterVec,
    pub(crate) age_blocks: CounterVec,
    pub(crate) upstream_err: CounterVec,
    pub(crate) upstream_dur: HistogramVec,
    pub(crate) bytes: CounterVec,
    pub(crate) blob_missing: CounterVec,
    /// Rewrite-gate saturation. An install burst resolves hundreds of packages
    /// at once, so the gate is where a slow decode turns into client-visible
    /// latency with no status code attached: the installer's own fetch timeout
    /// fires while its request is still queued. `gate_wait`/`gate_abandoned`
    /// make that visible, and `gate_hold` separates "the gate is too narrow"
    /// from "each decode is too slow".
    pub(crate) gate_wait: HistogramVec,
    pub(crate) gate_hold: HistogramVec,
    pub(crate) gate_abandoned: CounterVec,
    /// Counts rendered-document reuse. A hit skips the decode and the rewrite
    /// gate entirely, so the hit ratio is what says whether metadata serving is
    /// doing the same work twice.
    pub(crate) render_cache_ops: CounterVec,
    pub(crate) gate_inflight: Gauge,
    pub(crate) gate_queued: Gauge,
    pub(crate) gate_capacity: Gauge,
    /// Remembers references found to be missing their bytes, so the UI can mark
    /// the affected artifacts instead of leaving users to discover it as a 5xx
    /// mid-build.
    pub(crate) dangling: DanglingRegistry,
}

impl Engine {
    /// Builds an engine and registers its metrics.
    pub fn new(store: Arc<Store>, blobs: Arc<dyn BlobStore>, registry: &Registry) -> Arc<Engine> {
        let e = Engine {
            store,
            blobs,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                // Redirects are followed by hand (see
                // `strip_auth_on_cross_host_redirect`) because credential
                // headers must be dropped when the host changes, which
                // reqwest's redirect policies cannot express.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default(),
            ext_client: safedial::new_public_only_client(Duration::from_secs(60)),
            neg: NegCache::new(),
            cool: NegCache::new(),
            flight: Flight::new(),
            renders: RenderCache::new(RENDER_CACHE_BYTES, RENDER_CACHE_TTL),
            render_bytes: Arc::new(tokio::sync::Semaphore::new(
                MAX_INFLIGHT_RENDER_BYTES as usize,
            )),
            rewrite_gate: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REWRITES)),
            gc_mu: tokio::sync::RwLock::new(()),
            user_agent: format!("forklift/{}", crate::version::VERSION),
            now: parking_lot::RwLock::new(system_clock()),
            on_store: parking_lot::RwLock::new(None),
            cache_hits: counter_vec("cache_hits_total", "Proxy cache hits.", &["repo"]),
            cache_miss: counter_vec("cache_misses_total", "Proxy cache misses.", &["repo"]),
            age_blocks: counter_vec(
                "age_policy_violations_total",
                "Age policy violations.",
                &["repo", "action"],
            ),
            upstream_err: counter_vec("upstream_errors_total", "Upstream fetch errors.", &["repo"]),
            upstream_dur: histogram_vec(
                "upstream_request_duration_seconds",
                "Upstream fetch latency to first response byte, per proxy repository.",
                // The upstream client times out at 60s, so the range extends past
                // the default 10s bucket cap to keep slow-but-alive upstreams
                // visible.
                vec![0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 2.5, 5.0, 10.0, 30.0, 60.0],
                &["repo"],
            ),
            bytes: counter_vec(
                "bytes_transferred_total",
                "Artifact bytes transferred to/from clients (egress=downloads, ingress=uploads).",
                &["direction", "format"],
            ),
            blob_missing: counter_vec(
                "dangling_blob_refs_total",
                "Artifact metadata references whose blob bytes are absent from the blob store.",
                &["repo", "role"],
            ),
            gate_wait: histogram_vec(
                "metadata_rewrite_wait_seconds",
                "Time a metadata request spent queued for a rewrite slot, including waits that ended in the client giving up.",
                // Installers abandon around their own fetch timeout (pnpm and npm
                // both default to 60s), so the range has to reach it: a histogram
                // capped at 10s reports every one of those waits identically.
                vec![
                    0.001, 0.005, 0.025, 0.1, 0.5, 1.0, 2.0, 2.5, 5.0, 10.0, 30.0, 60.0,
                ],
                &["repo"],
            ),
            gate_hold: histogram_vec(
                "metadata_rewrite_duration_seconds",
                "Time a metadata request held a rewrite slot (decode plus rewrite).",
                vec![
                    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 2.5, 5.0, 10.0,
                ],
                &["repo"],
            ),
            gate_abandoned: counter_vec(
                "metadata_rewrite_abandoned_total",
                "Metadata requests whose client disconnected while still queued for a rewrite slot.",
                &["repo"],
            ),
            render_cache_ops: counter_vec(
                "metadata_render_cache_total",
                "Rendered metadata document lookups, by result (hit, miss).",
                &["result"],
            ),
            gate_inflight: gauge("metadata_rewrite_inflight", "Rewrite slots currently held."),
            gate_queued: gauge(
                "metadata_rewrite_queued",
                "Metadata requests currently waiting for a rewrite slot.",
            ),
            gate_capacity: gauge(
                "metadata_rewrite_capacity",
                "Total rewrite slots (see MAX_CONCURRENT_REWRITES); inflight/capacity is gate saturation.",
            ),
            dangling: DanglingRegistry::new(system_clock()),
        };
        e.gate_capacity.set(MAX_CONCURRENT_REWRITES as f64);
        for c in [
            Box::new(e.cache_hits.clone()) as Box<dyn prometheus::core::Collector>,
            Box::new(e.cache_miss.clone()),
            Box::new(e.age_blocks.clone()),
            Box::new(e.upstream_err.clone()),
            Box::new(e.upstream_dur.clone()),
            Box::new(e.bytes.clone()),
            Box::new(e.blob_missing.clone()),
            Box::new(e.gate_wait.clone()),
            Box::new(e.gate_hold.clone()),
            Box::new(e.gate_abandoned.clone()),
            Box::new(e.gate_inflight.clone()),
            Box::new(e.gate_queued.clone()),
            Box::new(e.gate_capacity.clone()),
            Box::new(e.render_cache_ops.clone()),
        ] {
            if let Err(err) = registry.register(c) {
                tracing::error!(err = %err, "engine metric registration failed");
            }
        }
        Arc::new(e)
    }

    /// The engine clock. Tests swap it with [`Engine::set_now`].
    pub(crate) fn now(&self) -> DateTime<Utc> {
        let f = self.now.read().clone();
        f()
    }

    /// Replaces the engine clock (tests, and any caller that needs a fixed
    /// reference instant).
    #[cfg(test)]
    pub(crate) fn set_now(&self, now: NowFn) {
        *self.now.write() = now;
    }

    /// Registers the post-cache hook. `None` disables it.
    pub(crate) fn set_on_store(&self, f: Option<OnStoreFn>) {
        *self.on_store.write() = f;
    }

    /// Records a dangling reference: metadata still points at a blob whose bytes
    /// are gone from the blob store. Every caller turns this into a 5xx, and the
    /// condition is not self-healing, so it must be loud -- diagnosing one of
    /// these from response codes alone means reconstructing it from blob store
    /// histograms.
    ///
    /// `repo_name` may be empty where only the id is at hand; the metric then
    /// labels by id, and the registry keys by id regardless.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn note_blob_missing(
        &self,
        repo_id: i64,
        repo_name: &str,
        path: &str,
        sha: &str,
        role: &str,
        status: i64,
        err: &str,
    ) {
        let label = if repo_name.is_empty() {
            format!("id:{}", itoa(repo_id))
        } else {
            repo_name.to_string()
        };
        self.blob_missing.with_label_values(&[&label, role]).inc();
        self.dangling.record(repo_id, path, sha, role, status);
        tracing::error!(
            repo = %label, repo_id, path, sha256 = sha, role, status, err,
            "blob missing for artifact reference"
        );
    }

    /// Handles a GET or HEAD for a repository path.
    pub(crate) async fn serve(self: &Arc<Self>, parts: Arc<Parts>, spec: FetchSpec) -> Response {
        let key = format!("{}/{}", spec.repo.name, spec.path);

        match self.store.get_artifact(spec.repo.id, &spec.path).await {
            Ok(art) => {
                // Hosted repositories are authoritative and always serve stored
                // artifacts; proxy repositories serve from cache only while the
                // entry is fresh.
                if spec.repo.r#type == meta::TYPE_HOSTED || self.fresh(&art, &spec.cfg, spec.kind) {
                    if let Some(resp) = self.age_gate(&spec, art.published_at) {
                        return resp;
                    }
                    if let Some(gate) = &spec.final_gate
                        && let Some(resp) = gate(Arc::clone(&parts)).await
                    {
                        return resp;
                    }
                    if spec.repo.r#type == meta::TYPE_PROXY {
                        self.cache_hits.with_label_values(&[&spec.repo.name]).inc();
                    }
                    self.touch(&art, &username_from_context(&parts)).await;
                    let (resp, n) = self.serve_artifact(&parts, &art).await;
                    self.bytes
                        .with_label_values(&["egress", &spec.repo.format])
                        .inc_by(n as f64);
                    return resp;
                }
            }
            Err(e) if !e.is_not_found() => {
                return http_error(StatusCode::INTERNAL_SERVER_ERROR, "metadata error");
            }
            Err(_) => {}
        }

        if spec.repo.r#type == meta::TYPE_HOSTED {
            return not_found();
        }

        // Proxy path.
        if self.neg.has(&key) {
            return not_found();
        }
        self.cache_miss.with_label_values(&[&spec.repo.name]).inc();
        self.fetch_and_serve(parts, spec, &key).await
    }

    pub(crate) async fn fetch_and_serve(
        self: &Arc<Self>,
        parts: Arc<Parts>,
        spec: FetchSpec,
        key: &str,
    ) -> Response {
        // Recently rate-limited: answer locally so client retries do not
        // re-enter the upstream storm. Relay a Retry-After so the build tool
        // backs off correctly.
        if let Some(d) = self.cool.remaining(key) {
            return self.write_retry(StatusCode::SERVICE_UNAVAILABLE, &retry_after_seconds(d));
        }

        // Pass-through repositories stream the body straight to the client and
        // cannot be coalesced (each caller needs its own stream), so they take a
        // direct path.
        if !spec.cfg.cache.enabled {
            return self.fetch_passthrough(parts, spec, key).await;
        }

        // Collapse concurrent identical fetches into one upstream round-trip.
        // The shared fetch is detached from any single client's cancellation so
        // a waiter disconnecting cannot abort the fetch the others depend on.
        let outcome = {
            let engine = Arc::clone(self);
            let detached = spec.detached();
            let key_owned = key.to_string();
            let username = username_from_context(&parts);
            self.flight
                .do_call(key, move || async move {
                    engine
                        .fetch_and_store(detached, &key_owned, &username)
                        .await
                })
                .await
        };

        match outcome.kind {
            FetchKind::Stored => {
                let Ok(art) = self.store.get_artifact(spec.repo.id, &spec.path).await else {
                    return http_error(StatusCode::INTERNAL_SERVER_ERROR, "cache read failed");
                };
                if let Some(gate) = &spec.final_gate
                    && let Some(resp) = gate(Arc::clone(&parts)).await
                {
                    return resp;
                }
                let (resp, n) = self.serve_artifact(&parts, &art).await;
                self.bytes
                    .with_label_values(&["egress", &spec.repo.format])
                    .inc_by(n as f64);
                resp
            }
            FetchKind::NotFound => not_found(),
            FetchKind::AgeBlocked => http_error(StatusCode::NOT_FOUND, "blocked by age policy"),
            FetchKind::Retry => self.write_retry(
                StatusCode::from_u16(outcome.status as u16)
                    .unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
                &outcome.retry_after,
            ),
            FetchKind::Error => http_error(StatusCode::BAD_GATEWAY, "upstream error"),
        }
    }

    /// Performs the upstream GET and caches the result. It writes no HTTP
    /// response; it records side effects (cached artifact, negative entry, or
    /// cooldown) and returns an outcome each caller renders independently. It
    /// runs detached from individual client cancellation.
    pub(crate) async fn fetch_and_store(
        &self,
        spec: FetchSpec,
        key: &str,
        username: &str,
    ) -> FetchOutcome {
        // A concurrent caller we were coalesced behind may have just populated
        // the cache; serve that instead of re-fetching.
        if let Ok(art) = self.store.get_artifact(spec.repo.id, &spec.path).await
            && self.fresh(&art, &spec.cfg, spec.kind)
        {
            return FetchOutcome::stored();
        }

        let resp = match self.upstream_get(&spec).await {
            Ok(resp) => resp,
            Err(err) => {
                self.upstream_err
                    .with_label_values(&[&spec.repo.name])
                    .inc();
                tracing::error!(
                    repo = %spec.repo.name, url = %spec.upstream_url, err = %err,
                    "upstream fetch failed"
                );
                return FetchOutcome::error();
            }
        };

        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            self.neg.set(key, spec.cfg.cache.negative_ttl.d());
            return FetchOutcome {
                kind: FetchKind::NotFound,
                ..Default::default()
            };
        }
        if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
            let d = parse_retry_after(header_str(resp.headers(), "Retry-After"), self.now());
            self.cool.set(key, d);
            self.upstream_err
                .with_label_values(&[&spec.repo.name])
                .inc();
            tracing::warn!(
                repo = %spec.repo.name, url = %spec.upstream_url, status = status.as_u16(),
                cooldown = %humantime::format_duration(d),
                "upstream rate-limited; cooling down"
            );
            return FetchOutcome {
                kind: FetchKind::Retry,
                status: status.as_u16() as i64,
                retry_after: retry_after_seconds(d),
            };
        }
        if !status.is_success() {
            self.upstream_err
                .with_label_values(&[&spec.repo.name])
                .inc();
            tracing::error!(
                repo = %spec.repo.name, url = %spec.upstream_url, status = status.as_u16(),
                "upstream non-2xx"
            );
            return FetchOutcome::error();
        }

        let published = spec.extract_published.as_ref().and_then(|f| f(&resp));
        if self.eval_age(&spec, published) {
            return FetchOutcome {
                kind: FetchKind::AgeBlocked,
                ..Default::default()
            };
        }

        let mut content_type = spec.content_type.clone();
        if content_type.is_empty() {
            content_type = header_str(resp.headers(), "Content-Type").to_string();
        }
        let body = response_body(resp, spec.max_store_bytes);
        if let Err(err) = self
            .store_artifact(&spec, body, &content_type, published, username)
            .await
        {
            tracing::error!(
                repo = %spec.repo.name, path = %spec.path, err = %err,
                "cache write failed"
            );
            return FetchOutcome::error();
        }
        self.maybe_evict(&spec).await;
        let hook = self.on_store.read().clone();
        if let Some(hook) = hook {
            hook(spec.repo.clone(), spec.path.clone());
        }
        FetchOutcome::stored()
    }

    /// Serves a cache-disabled proxy repository, streaming the upstream body
    /// straight to the client without persisting it.
    pub(crate) async fn fetch_passthrough(
        &self,
        parts: Arc<Parts>,
        spec: FetchSpec,
        key: &str,
    ) -> Response {
        let resp = match self.upstream_get(&spec).await {
            Ok(resp) => resp,
            Err(err) => {
                self.upstream_err
                    .with_label_values(&[&spec.repo.name])
                    .inc();
                tracing::error!(
                    repo = %spec.repo.name, url = %spec.upstream_url, err = %err,
                    "upstream fetch failed"
                );
                return http_error(StatusCode::BAD_GATEWAY, "upstream unreachable");
            }
        };

        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return not_found();
        }
        if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
            let d = parse_retry_after(header_str(resp.headers(), "Retry-After"), self.now());
            self.cool.set(key, d);
            self.upstream_err
                .with_label_values(&[&spec.repo.name])
                .inc();
            tracing::warn!(
                repo = %spec.repo.name, url = %spec.upstream_url, status = status.as_u16(),
                cooldown = %humantime::format_duration(d),
                "upstream rate-limited; cooling down"
            );
            return self.write_retry(status, &retry_after_seconds(d));
        }
        if !status.is_success() {
            self.upstream_err
                .with_label_values(&[&spec.repo.name])
                .inc();
            tracing::error!(
                repo = %spec.repo.name, url = %spec.upstream_url, status = status.as_u16(),
                "upstream non-2xx"
            );
            return http_error(StatusCode::BAD_GATEWAY, "upstream error");
        }

        let published = spec.extract_published.as_ref().and_then(|f| f(&resp));
        if let Some(resp) = self.age_gate(&spec, published) {
            return resp;
        }
        if let Some(gate) = &spec.final_gate
            && let Some(resp) = gate(Arc::clone(&parts)).await
        {
            return resp;
        }
        let mut content_type = spec.content_type.clone();
        if content_type.is_empty() {
            content_type = header_str(resp.headers(), "Content-Type").to_string();
        }
        let content_length = resp.content_length();
        let mut builder = Response::builder().status(StatusCode::OK);
        if !content_type.is_empty()
            && let Ok(v) = HeaderValue::from_str(&content_type)
        {
            builder = builder.header(http::header::CONTENT_TYPE, v);
        }
        if parts.method == Method::HEAD {
            return builder
                .body(Body::empty())
                .unwrap_or_else(|_| server_error());
        }
        if let Some(n) = content_length {
            self.bytes
                .with_label_values(&["egress", &spec.repo.format])
                .inc_by(n as f64);
        }
        let stream = resp.bytes_stream().map_err(std::io::Error::other);
        builder
            .body(Body::from_stream(stream))
            .unwrap_or_else(|_| server_error())
    }

    /// Builds the headers for an authenticated upstream request: a descriptive
    /// User-Agent (public registries, notably Maven Central, throttle a generic
    /// user-agent harder) plus the repository's upstream credentials. It is the
    /// single seam every upstream call goes through, so a future
    /// challenge-based authenticator (e.g. the OCI registry token flow used by
    /// Harbor and Docker Hub, which needs a 401 round trip and a per-repo token
    /// cache) plugs in here without touching the format handlers.
    ///
    /// Returns the headers and the name of the custom auth header, if any, so
    /// [`strip_auth_on_cross_host_redirect`] can drop it.
    pub(crate) fn new_upstream_request(
        &self,
        cfg: &UpstreamAuthConfig,
    ) -> (HeaderMap, Option<HeaderName>) {
        let mut headers = HeaderMap::new();
        if let Ok(ua) = HeaderValue::from_str(&self.user_agent) {
            headers.insert(http::header::USER_AGENT, ua);
        }
        let mut custom = None;
        match cfg.type_.as_str() {
            UPSTREAM_AUTH_BASIC => {
                use base64::Engine as _;
                let raw = format!("{}:{}", cfg.username, cfg.password);
                let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
                if let Ok(v) = HeaderValue::from_str(&format!("Basic {encoded}")) {
                    headers.insert(http::header::AUTHORIZATION, v);
                }
            }
            UPSTREAM_AUTH_BEARER => {
                if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", cfg.token)) {
                    headers.insert(http::header::AUTHORIZATION, v);
                }
            }
            UPSTREAM_AUTH_HEADER => {
                if let (Ok(name), Ok(value)) = (
                    HeaderName::try_from(cfg.header.as_str()),
                    HeaderValue::from_str(&cfg.value),
                ) {
                    headers.insert(name.clone(), value);
                    custom = Some(name);
                }
            }
            _ => {}
        }
        (headers, custom)
    }

    /// Issues the upstream GET, following redirects by hand so credentials are
    /// dropped when the chain leaves the current host.
    pub(crate) async fn upstream_get(
        &self,
        spec: &FetchSpec,
    ) -> Result<reqwest::Response, UpstreamError> {
        let auth = if spec.untrusted_url {
            // Client-supplied URLs (e.g. PyPI base64url file refs) may point
            // anywhere; never present the repository's upstream credentials to
            // them.
            UpstreamAuthConfig::default()
        } else {
            spec.cfg.upstream_auth.clone()
        };
        let client = if spec.untrusted_url {
            &self.ext_client
        } else {
            &self.client
        };
        // Time until the response headers (or a transport error) arrive, not the
        // body stream; failed fetches are observed too so timeouts show up in
        // the tail.
        let start = Instant::now();
        let result = self
            .follow_upstream(
                client,
                &spec.upstream_url,
                &auth,
                spec.untrusted_url,
                &spec.accept,
            )
            .await;
        self.upstream_dur
            .with_label_values(&[&spec.repo.name])
            .observe(start.elapsed().as_secs_f64());
        result
    }

    async fn follow_upstream(
        &self,
        client: &reqwest::Client,
        url: &str,
        auth: &UpstreamAuthConfig,
        untrusted: bool,
        accept: &str,
    ) -> Result<reqwest::Response, UpstreamError> {
        let mut next = Url::parse(url).map_err(|e| UpstreamError::Url(e.to_string()))?;
        let (mut headers, custom) = self.new_upstream_request(auth);
        if !accept.is_empty()
            && let Ok(v) = HeaderValue::from_str(accept)
        {
            headers.insert(http::header::ACCEPT, v);
        }
        let mut via: Vec<Url> = Vec::new();
        loop {
            if untrusted {
                safedial::guard_public_url(&next)
                    .await
                    .map_err(|e| UpstreamError::Blocked(e.to_string()))?;
            }
            let resp = client
                .get(next.clone())
                .headers(headers.clone())
                .send()
                .await?;
            if !resp.status().is_redirection() {
                return Ok(resp);
            }
            let Some(location) = header_str_opt(resp.headers(), "Location") else {
                return Ok(resp);
            };
            let target = next
                .join(location)
                .map_err(|e| UpstreamError::Url(e.to_string()))?;
            via.push(next);
            strip_auth_on_cross_host_redirect(&target, &via, &mut headers, custom.as_ref())
                .map_err(|e| UpstreamError::Url(e.to_string()))?;
            next = target;
        }
    }

    /// Applies the age policy without writing a response, returning true when
    /// the artifact must be blocked. Counters and logs mirror
    /// [`Engine::age_gate`].
    pub(crate) fn eval_age(&self, spec: &FetchSpec, published: Option<DateTime<Utc>>) -> bool {
        let (decision, reason) = evaluate_age(&spec.cfg.age_policy, published, self.now());
        match decision {
            AgeDecision::Block => {
                self.age_blocks
                    .with_label_values(&[&spec.repo.name, "block"])
                    .inc();
                tracing::warn!(
                    repo = %spec.repo.name, path = %spec.path, reason,
                    "age policy blocked artifact"
                );
                true
            }
            AgeDecision::Warn => {
                self.age_blocks
                    .with_label_values(&[&spec.repo.name, "warn"])
                    .inc();
                tracing::warn!(
                    repo = %spec.repo.name, path = %spec.path, reason,
                    "age policy warning"
                );
                false
            }
            AgeDecision::Allow => false,
        }
    }

    /// Tells the client to back off, relaying the upstream's status (429/503)
    /// and a Retry-After hint so build tools wait instead of hammering.
    pub(crate) fn write_retry(&self, status: StatusCode, retry_after: &str) -> Response {
        let status = if status != StatusCode::TOO_MANY_REQUESTS
            && status != StatusCode::SERVICE_UNAVAILABLE
        {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            status
        };
        let mut resp = http_error(status, "upstream rate-limited, retry later");
        if !retry_after.is_empty()
            && let Ok(v) = HeaderValue::from_str(retry_after)
        {
            resp.headers_mut().insert(http::header::RETRY_AFTER, v);
        }
        resp
    }

    /// Streams `body` into the blob store and records the artifact.
    pub(crate) async fn store_artifact(
        &self,
        spec: &FetchSpec,
        body: StoreBody,
        content_type: &str,
        published: Option<DateTime<Utc>>,
        username: &str,
    ) -> Result<Artifact, StoreError> {
        // Hold the GC read guard from the byte write through the reference
        // insert so the sweeper cannot reclaim these bytes in the gap between
        // `put` and `put_artifact` (see `Engine::gc_mu`).
        let _gc = self.gc_mu.read().await;
        let (digest, size) = self.blobs.put(body).await?;
        let now = self.now();
        Ok(self
            .store
            .put_artifact(Artifact {
                repo_id: spec.repo.id,
                path: spec.path.clone(),
                version: spec.version.clone(),
                blob_sha256: digest,
                size,
                content_type: content_type.to_string(),
                published_at: published,
                cached_at: now,
                last_accessed_at: now,
                cached_by: username.to_string(),
                ..Default::default()
            })
            .await?)
    }

    /// Reserves `n` bytes of the in-flight render budget. A document larger than
    /// the whole budget reserves all of it rather than deadlocking on an
    /// impossible request. Returns `None` only when the semaphore has been
    /// closed, which happens at shutdown.
    pub(crate) async fn acquire_render_bytes(&self, n: i64) -> Option<RenderBytesPermit> {
        let n = n.clamp(1, MAX_INFLIGHT_RENDER_BYTES) as u32;
        let permit = Arc::clone(&self.render_bytes)
            .acquire_many_owned(n)
            .await
            .ok()?;
        Some(RenderBytesPermit { _permit: permit })
    }

    /// Takes a slot on the rewrite semaphore, waiting until one is free.
    ///
    /// The queue is instrumented here rather than at the call sites so every format reports it
    /// the same way. The wait is observed on both outcomes because a wait that ended in the
    /// client disconnecting is the one that matters most: that is what an installer sees as a
    /// request with no status code at all, and it would otherwise be missing from the histogram
    /// entirely.
    ///
    /// Returns `None` only when the gate has been closed at shutdown.
    pub(crate) async fn acquire_rewrite(&self, repo: &str) -> Option<RewriteSlot> {
        let mut queue = GateQueue {
            engine_queued: self.gate_queued.clone(),
            wait: self.gate_wait.clone(),
            abandoned: self.gate_abandoned.clone(),
            repo: repo.to_string(),
            queued: Instant::now(),
            armed: true,
        };
        self.gate_queued.inc();
        let permit = Arc::clone(&self.rewrite_gate).acquire_owned().await.ok();
        queue.armed = false;
        self.gate_queued.dec();
        self.gate_wait
            .with_label_values(&[repo])
            .observe(queue.queued.elapsed().as_secs_f64());
        let permit = permit?;
        self.gate_inflight.inc();
        Some(RewriteSlot {
            _permit: permit,
            inflight: self.gate_inflight.clone(),
            hold: self.gate_hold.clone(),
            repo: repo.to_string(),
            held: Instant::now(),
        })
    }

    /// Registers an already-stored blob as a hosted artifact. Streaming upload
    /// handlers use it to store the blob the moment its multipart part arrives
    /// and attach the metadata once the remaining form fields have been read
    /// (the temp-blob-then-attach pattern). The caller must hold the `gc_mu`
    /// read guard across the earlier `blobs.put` and this call so the sweeper
    /// cannot reclaim the bytes in between; `record_upload` therefore takes no
    /// lock of its own (a second read guard here would risk a writer-starvation
    /// deadlock).
    #[allow(clippy::too_many_arguments)] // Domain operation parameters.
    pub(crate) async fn record_upload(
        &self,
        repo: &Repository,
        path: &str,
        version: &str,
        content_type: &str,
        digest: &str,
        size: i64,
        username: &str,
    ) -> Result<(), meta::Error> {
        let now = self.now();
        self.store
            .put_artifact(Artifact {
                repo_id: repo.id,
                path: path.to_string(),
                version: version.to_string(),
                blob_sha256: digest.to_string(),
                size,
                content_type: content_type.to_string(),
                cached_at: now,
                last_accessed_at: now,
                cached_by: username.to_string(),
                ..Default::default()
            })
            .await?;
        self.neg.clear(&format!("{}/{}", repo.name, path));
        self.bytes
            .with_label_values(&["ingress", &repo.format])
            .inc_by(size as f64);
        Ok(())
    }

    /// Hands a streamed-but-never-recorded blob to the sweeper by ensuring a
    /// zero-reference blob record exists for it. Deleting the bytes directly
    /// would be wrong: the store is content-addressed, so an identical blob may
    /// already be referenced by other artifacts.
    pub(crate) async fn abandon_blob(&self, digest: &str, size: i64) {
        if let Err(err) = self.store.ensure_blob(digest, size).await {
            tracing::warn!(sha = digest, err = %err, "record abandoned upload blob failed");
        }
    }

    /// Stores an uploaded artifact for a hosted repository.
    #[allow(clippy::too_many_arguments)] // Domain operation parameters.
    pub(crate) async fn put(
        &self,
        repo: &Repository,
        path: &str,
        version: &str,
        content_type: &str,
        published: Option<DateTime<Utc>>,
        body: StoreBody,
        username: &str,
    ) -> Result<(), StoreError> {
        let spec = FetchSpec {
            repo: repo.clone(),
            path: path.to_string(),
            version: version.to_string(),
            content_type: content_type.to_string(),
            ..FetchSpec::blank()
        };
        let result = self
            .store_artifact(&spec, body, content_type, published, username)
            .await;
        self.neg.clear(&format!("{}/{}", repo.name, path));
        match result {
            Ok(art) => {
                self.bytes
                    .with_label_values(&["ingress", &repo.format])
                    .inc_by(art.size as f64);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Writes the artifact body and returns the response plus the number of
    /// bytes it will hand the client (0 for HEAD or a 304).
    ///
    /// The body here is a stream handed to hyper, so the count is taken from the blob's known
    /// size at the moment the response is built; the two differ only when a client aborts
    /// mid-download.
    pub(crate) async fn serve_artifact(&self, parts: &Parts, art: &Artifact) -> (Response, i64) {
        let (reader, size) = match self.blobs.open(&art.blob_sha256).await {
            Ok(v) => v,
            Err(err) => {
                self.note_blob_missing(
                    art.repo_id,
                    "",
                    &art.path,
                    &art.blob_sha256,
                    &art.artifact_role,
                    StatusCode::INTERNAL_SERVER_ERROR.as_u16() as i64,
                    &err.to_string(),
                );
                return (
                    http_error(StatusCode::INTERNAL_SERVER_ERROR, "blob missing"),
                    0,
                );
            }
        };
        let etag = format!("\"{}\"", art.blob_sha256);
        let mut builder = Response::builder();
        if !art.content_type.is_empty()
            && let Ok(v) = HeaderValue::from_str(&art.content_type)
        {
            builder = builder.header(http::header::CONTENT_TYPE, v);
        }
        if let Ok(v) = HeaderValue::from_str(&etag) {
            builder = builder.header(http::header::ETAG, v);
        }
        if header_str(&parts.headers, "If-None-Match") == etag {
            return (
                builder
                    .status(StatusCode::NOT_MODIFIED)
                    .body(Body::empty())
                    .unwrap_or_else(|_| server_error()),
                0,
            );
        }
        builder = builder.header(http::header::CONTENT_LENGTH, itoa(size));
        if parts.method == Method::HEAD {
            return (
                builder
                    .status(StatusCode::OK)
                    .body(Body::empty())
                    .unwrap_or_else(|_| server_error()),
                0,
            );
        }
        let stream = ReaderStream::new(reader);
        (
            builder
                .status(StatusCode::OK)
                .body(Body::from_stream(stream))
                .unwrap_or_else(|_| server_error()),
            size,
        )
    }

    /// Refreshes an artifact's `last_accessed_at` unless it is already recent.
    /// The staleness check runs against the row the caller just loaded, so the
    /// common case (an artifact served repeatedly) costs no database write at
    /// all.
    pub(crate) async fn touch(&self, art: &Artifact, username: &str) {
        let since = self.now() - art.last_accessed_at;
        if since < chrono::TimeDelta::from_std(TOUCH_INTERVAL).unwrap_or(chrono::TimeDelta::MAX) {
            return;
        }
        let _ = self.store.touch(art.repo_id, &art.path, username).await;
    }

    /// Evaluates the age policy and, when blocking, returns the 404 the caller
    /// must send. Warnings are logged and counted but allowed.
    pub(crate) fn age_gate(
        &self,
        spec: &FetchSpec,
        published: Option<DateTime<Utc>>,
    ) -> Option<Response> {
        if self.eval_age(spec, published) {
            return Some(http_error(StatusCode::NOT_FOUND, "blocked by age policy"));
        }
        None
    }

    /// Reports whether a cached artifact is still fresh under the cache policy.
    pub(crate) fn fresh(&self, art: &Artifact, cfg: &Config, k: Kind) -> bool {
        if !cfg.cache.enabled {
            return false;
        }
        let ttl = match k {
            Kind::Metadata => cfg.cache.metadata_ttl.d(),
            Kind::Artifact => cfg.cache.artifact_ttl.d(),
        };
        if ttl.is_zero() {
            // Artifacts are immutable (ttl 0 = never revalidate); metadata with
            // no TTL is treated as always-revalidate to avoid serving stale
            // indexes.
            return k == Kind::Artifact;
        }
        let age = self.now() - art.cached_at;
        age < chrono::TimeDelta::from_std(ttl).unwrap_or(chrono::TimeDelta::MAX)
    }

    /// Trims the repository cache to its configured size cap. OCI repositories
    /// are exempt: LRU eviction deletes rows individually, and evicting a layer
    /// out from under a still-tagged manifest breaks an image the moment the
    /// sweeper reclaims the bytes. OCI space is reclaimed by the manifest-aware
    /// reachability prune instead (see `oci_prune.rs`).
    pub(crate) async fn maybe_evict(&self, spec: &FetchSpec) {
        if spec.repo.format == meta::FORMAT_OCI {
            return;
        }
        let max = spec.cfg.cache.max_size_bytes;
        if max <= 0 {
            return;
        }
        match self.store.repo_size(spec.repo.id).await {
            Ok(size) if size > max => {}
            _ => return,
        }
        // Evict in small batches until under the cap (bounded to avoid long
        // loops).
        for _ in 0..64 {
            match self.store.evict_lru(spec.repo.id, 16).await {
                Ok(n) if n > 0 => {}
                _ => break,
            }
            match self.store.repo_size(spec.repo.id).await {
                Ok(size) if size > max => {}
                _ => break,
            }
        }
    }
}

/// Parameterises a GET/HEAD against the engine for one request.
#[derive(Clone)]
pub(crate) struct FetchSpec {
    pub(crate) repo: Repository,
    pub(crate) cfg: Config,
    /// Repo-relative storage key.
    pub(crate) path: String,
    /// The full upstream URL (proxy only).
    pub(crate) upstream_url: String,
    /// Marks `upstream_url` as client-supplied (not derived from the
    /// admin-configured upstream); it is fetched via the SSRF-guarded client.
    pub(crate) untrusted_url: bool,
    pub(crate) kind: Kind,
    pub(crate) version: String,
    pub(crate) content_type: String,
    /// Sent as the upstream `Accept` header when non-empty. PyPI needs it to
    /// negotiate the PEP 691 JSON simple index.
    pub(crate) accept: String,
    /// Caps how many upstream bytes are cached. Metadata documents are parsed
    /// after caching, so their size is bounded here rather than trusting the
    /// upstream; 0 (artifacts) streams the body whole.
    pub(crate) max_store_bytes: i64,
    /// Runs after age evaluation and immediately before serving.
    pub(crate) final_gate: Option<FinalGate>,
    /// Derives the upstream release time from a proxy response.
    pub(crate) extract_published: Option<ExtractPublished>,
}

impl FetchSpec {
    /// `Config` cannot derive it because its `Default` is the populated repository default.
    pub(crate) fn blank() -> FetchSpec {
        FetchSpec {
            repo: Repository::default(),
            cfg: Config::default(),
            path: String::new(),
            upstream_url: String::new(),
            untrusted_url: false,
            kind: Kind::Artifact,
            version: String::new(),
            content_type: String::new(),
            accept: String::new(),
            max_store_bytes: 0,
            final_gate: None,
            extract_published: None,
        }
    }

    /// A copy for the detached single-flight fetch: the final gate holds a
    /// request-scoped closure that must not outlive the request, and
    /// `fetch_and_store` never calls it.
    fn detached(&self) -> FetchSpec {
        FetchSpec {
            final_gate: None,
            ..self.clone()
        }
    }
}

/// Classifies the result of a coalesced upstream fetch so each waiting caller
/// can render its own response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum FetchKind {
    /// The artifact is now cached; serve it from the store.
    #[default]
    Stored,
    /// Upstream 404 (negative-cached).
    NotFound,
    /// Age policy blocked the artifact.
    AgeBlocked,
    /// Upstream 429/503; cooled down, ask the client to retry.
    Retry,
    /// Any other failure.
    Error,
}

/// The shared result of one coalesced fetch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FetchOutcome {
    pub(crate) kind: FetchKind,
    /// For [`FetchKind::Retry`]: 429 or 503 to relay to clients.
    pub(crate) status: i64,
    /// For [`FetchKind::Retry`]: seconds, set as the Retry-After header.
    pub(crate) retry_after: String,
}

impl FetchOutcome {
    pub(crate) fn stored() -> FetchOutcome {
        FetchOutcome::default()
    }

    pub(crate) fn error() -> FetchOutcome {
        FetchOutcome {
            kind: FetchKind::Error,
            ..Default::default()
        }
    }
}

/// Failures from an upstream fetch.
#[derive(Debug, thiserror::Error)]
pub(crate) enum UpstreamError {
    #[error("{0}")]
    Http(#[from] reqwest::Error),
    /// A client-supplied URL resolved to a non-public destination.
    #[error("{0}")]
    Blocked(String),
    #[error("{0}")]
    Url(String),
}

/// Failures from writing an artifact into the blob store and metadata database.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StoreError {
    #[error("{0}")]
    Blob(#[from] crate::storage::Error),
    #[error("{0}")]
    Meta(#[from] meta::Error),
}

/// A held slot on the rewrite gate.
pub(crate) struct RewriteSlot {
    _permit: tokio::sync::OwnedSemaphorePermit,
    inflight: Gauge,
    hold: HistogramVec,
    repo: String,
    held: Instant,
}

impl Drop for RewriteSlot {
    fn drop(&mut self) {
        self.inflight.dec();
        self.hold
            .with_label_values(&[&self.repo])
            .observe(self.held.elapsed().as_secs_f64());
    }
}

/// Tracks a request queued for the rewrite gate.
struct GateQueue {
    engine_queued: Gauge,
    wait: HistogramVec,
    abandoned: CounterVec,
    repo: String,
    queued: Instant,
    armed: bool,
}

impl Drop for GateQueue {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.engine_queued.dec();
        self.wait
            .with_label_values(&[&self.repo])
            .observe(self.queued.elapsed().as_secs_f64());
        self.abandoned.with_label_values(&[&self.repo]).inc();
    }
}

/// A reservation on the in-flight render budget; dropping it releases the bytes.
pub(crate) struct RenderBytesPermit {
    _permit: tokio::sync::OwnedSemaphorePermit,
}

/// The redirect policy for upstream fetch clients: redirects are followed
/// (registries bounce to CDNs), but when the redirect leaves the current host
/// every credential header is removed.
///
/// So redirects are followed manually and this function is the per-hop decision: it errors when
/// the chain is too long, and otherwise edits `headers` in place.
pub(crate) fn strip_auth_on_cross_host_redirect(
    next: &Url,
    via: &[Url],
    headers: &mut HeaderMap,
    upstream_auth_header: Option<&HeaderName>,
) -> Result<(), &'static str> {
    if via.len() >= 10 {
        return Err("stopped after 10 redirects");
    }
    let prev = via.last().ok_or("redirect with no previous request")?;
    let same_host = next.host_str() == prev.host_str()
        && next.port_or_known_default() == prev.port_or_known_default();
    if !same_host {
        headers.remove(http::header::AUTHORIZATION);
        if let Some(name) = upstream_auth_header {
            headers.remove(name);
        }
    }
    Ok(())
}

/// Returns the authenticated principal's username for the request, or "" when
/// the request is anonymous.
pub(crate) fn username_from_context(parts: &Parts) -> String {
    crate::auth::from_request_parts(parts)
        .map(|p| p.username.clone())
        .unwrap_or_default()
}

/// Interprets an HTTP Retry-After header (delta-seconds or an HTTP date),
/// clamped to `[DEFAULT_UPSTREAM_COOLDOWN, MAX_UPSTREAM_COOLDOWN]`. A missing or
/// unparseable value falls back to the default cooldown.
pub(crate) fn parse_retry_after(h: &str, now: DateTime<Utc>) -> Duration {
    let mut d = DEFAULT_UPSTREAM_COOLDOWN;
    if !h.is_empty() {
        if let Ok(secs) = h.parse::<i64>() {
            d = if secs <= 0 {
                Duration::ZERO
            } else {
                Duration::from_secs(secs as u64)
            };
        } else if let Ok(t) = httpdate::parse_http_date(h) {
            let t: DateTime<Utc> = t.into();
            d = (t - now).to_std().unwrap_or(Duration::ZERO);
        }
    }
    d.clamp(DEFAULT_UPSTREAM_COOLDOWN, MAX_UPSTREAM_COOLDOWN)
}

/// Renders a cooldown as a whole-seconds Retry-After value.
pub(crate) fn retry_after_seconds(d: Duration) -> String {
    let millis = d.as_millis() as i64;
    let secs = (millis + 500) / 1000;
    itoa(secs.max(1))
}

pub(crate) fn itoa(n: i64) -> String {
    n.to_string()
}

pub(crate) fn not_found() -> Response {
    http_error(StatusCode::NOT_FOUND, "404 page not found")
}

fn server_error() -> Response {
    http_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

/// Reads a header as a string, or "" when absent or non-ASCII.
pub(crate) fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    header_str_opt(headers, name).unwrap_or("")
}

fn header_str_opt<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn response_body(resp: reqwest::Response, max: i64) -> StoreBody {
    let stream: Pin<Box<dyn futures_util::Stream<Item = std::io::Result<bytes::Bytes>> + Send>> =
        Box::pin(resp.bytes_stream().map_err(std::io::Error::other));
    let reader = tokio_util::io::StreamReader::new(stream);
    if max > 0 {
        Box::pin(tokio::io::AsyncReadExt::take(reader, max as u64))
    } else {
        Box::pin(reader)
    }
}

/// Wraps an inbound request body as an async reader for the engine's store path.
pub(crate) fn request_body(body: Body) -> StoreBody {
    Box::pin(tokio_util::io::StreamReader::new(
        body.into_data_stream().map_err(std::io::Error::other),
    ))
}

/// Empty input yields `"."`, an all-slash input `"/"`.
pub(crate) fn path_base(p: &str) -> &str {
    if p.is_empty() {
        return ".";
    }
    let p = p.trim_end_matches('/');
    if p.is_empty() {
        return "/";
    }
    match p.rsplit_once('/') {
        Some((_, base)) => base,
        None => p,
    }
}

pub(crate) fn path_ext(p: &str) -> &str {
    let base = path_base(p);
    match base.rfind('.') {
        Some(i) => &base[i..],
        None => "",
    }
}

fn counter_vec(name: &str, help: &str, labels: &[&str]) -> CounterVec {
    CounterVec::new(Opts::new(name, help).namespace("forklift"), labels).expect("valid counter")
}

fn histogram_vec(name: &str, help: &str, buckets: Vec<f64>, labels: &[&str]) -> HistogramVec {
    HistogramVec::new(
        HistogramOpts::new(name, help)
            .namespace("forklift")
            .buckets(buckets),
        labels,
    )
    .expect("valid histogram")
}

fn gauge(name: &str, help: &str) -> Gauge {
    Gauge::with_opts(Opts::new(name, help).namespace("forklift")).expect("valid gauge")
}

#[cfg(test)]
pub(crate) mod tests {
    mod extra {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicI32, Ordering};

        use axum::Router;
        use axum::routing::any;
        use chrono::{TimeZone, Utc};
        use http::{Method, StatusCode};

        use crate::meta;
        use crate::repoconfig::{self, Duration};

        use crate::testing::repo::{call, mk_repo, mux, new_test_manager, spawn_upstream};

        /// Mutable metadata is revalidated once its TTL elapses, unlike immutable
        /// artifacts which are served from cache indefinitely.
        #[tokio::test]
        async fn maven_metadata_revalidation() {
            let hits = Arc::new(AtomicI32::new(0));
            let upstream_hits = Arc::clone(&hits);
            let upstream = spawn_upstream(Router::new().fallback(any(move || {
                let hits = Arc::clone(&upstream_hits);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    "<metadata/>"
                }
            })))
            .await;

            let mut cfg = repoconfig::default();
            cfg.cache.metadata_ttl = Duration::from_std(std::time::Duration::from_secs(60));
            let tm = new_test_manager().await;
            mk_repo(&tm.store, "p", meta::TYPE_PROXY, &upstream, cfg).await;
            let app = mux(&tm.manager);
            let path = "/maven/p/g/a/maven-metadata.xml";

            let base = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
            tm.engine.set_now(Arc::new(move || base));

            let resp = call(&app, Method::GET, path, "").await;
            assert_eq!(resp.status, StatusCode::OK, "first");
            // Within TTL: served from cache.
            call(&app, Method::GET, path, "").await;
            assert_eq!(hits.load(Ordering::SeqCst), 1, "within ttl hits");
            // Past TTL: revalidated upstream.
            let later = base + chrono::TimeDelta::minutes(2);
            tm.engine.set_now(Arc::new(move || later));
            call(&app, Method::GET, path, "").await;
            assert_eq!(hits.load(Ordering::SeqCst), 2, "past ttl hits");
        }

        #[tokio::test]
        async fn maven_method_not_allowed() {
            let tm = new_test_manager().await;
            mk_repo(
                &tm.store,
                "mvn-local",
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
            let resp = call(
                &mux(&tm.manager),
                Method::DELETE,
                "/maven/mvn-local/g/a/1/a-1.jar",
                "",
            )
            .await;
            assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED, "delete");
        }

        #[tokio::test]
        async fn proxy_head_from_cache() {
            let upstream = spawn_upstream(Router::new().fallback(|| async { "BODY" })).await;
            let tm = new_test_manager().await;
            mk_repo(
                &tm.store,
                "p",
                meta::TYPE_PROXY,
                &upstream,
                repoconfig::default(),
            )
            .await;
            let app = mux(&tm.manager);
            let path = "/maven/p/g/a/1/a-1.jar";

            // Prime the cache.
            call(&app, Method::GET, path, "").await;
            // HEAD from cache returns headers, no body.
            let resp = call(&app, Method::HEAD, path, "").await;
            assert_eq!(resp.status, StatusCode::OK, "head");
            assert!(resp.body.is_empty(), "head body = {:?}", resp.text());
        }

        /// Hosted repositories are authoritative: a stored artifact must be served
        /// regardless of cache freshness (TTL) or caching being disabled.
        #[tokio::test]
        async fn local_serves_regardless_of_freshness() {
            let tm = new_test_manager().await;
            // Cache disabled, which would make `fresh()` return false for a proxy.
            let mut cfg = repoconfig::default();
            cfg.cache.enabled = false;
            mk_repo(&tm.store, "loc", meta::TYPE_HOSTED, "", cfg).await;
            let app = mux(&tm.manager);

            let resp = call(&app, Method::PUT, "/maven/loc/g/a/1/a-1.jar", "LOCALJAR").await;
            assert_eq!(resp.status, StatusCode::CREATED, "put");

            // Advance well past any metadata TTL; local must still serve.
            let later = Utc::now() + chrono::TimeDelta::hours(1000);
            tm.engine.set_now(Arc::new(move || later));
            let resp = call(&app, Method::GET, "/maven/loc/g/a/1/a-1.jar", "").await;
            assert_eq!(
                resp.status,
                StatusCode::OK,
                "local get (regression: local 404 on stale/disabled cache)"
            );
            assert_eq!(resp.text(), "LOCALJAR");

            // Local metadata (maven-metadata.xml) must also persist beyond TTL.
            call(
                &app,
                Method::PUT,
                "/maven/loc/g/a/maven-metadata.xml",
                "<m/>",
            )
            .await;
            let resp = call(&app, Method::GET, "/maven/loc/g/a/maven-metadata.xml", "").await;
            assert_eq!(resp.status, StatusCode::OK, "local metadata get");
            assert_eq!(resp.text(), "<m/>");
        }
    }

    mod formats {
        use std::collections::BTreeMap;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicI32, Ordering};

        use axum::Router;
        use axum::body::Body;
        use axum::response::IntoResponse;
        use axum::routing::any;
        use base64::Engine as _;
        use chrono::{TimeZone, Utc};
        use http::{HeaderMap, Method, Request, StatusCode, Uri};

        use crate::meta;
        use crate::repoconfig::{self, ACTION_BLOCK, AgePolicyConfig, Duration};

        use crate::repo::Kind;
        use crate::repo::cargo::{cargo_kind, cargo_version};
        use crate::repo::gomod::{go_kind, go_version};
        use crate::repo::npm::{highest_stable_version, rewrite_packument};
        use crate::testing::repo::http_time;
        use crate::testing::repo::{
            call, mk_format_repo, mux, new_test_manager, send, spawn_upstream,
        };

        // --- Go modules ---

        #[tokio::test]
        async fn go_proxy_flow() {
            let upstream = spawn_upstream(Router::new().fallback(any(|uri: Uri| async move {
                let path = uri.path().to_string();
                if path.ends_with("/@v/list") {
                    return "v1.0.0\nv1.1.0\n".into_response();
                }
                if path.ends_with(".info") {
                    return r#"{"Version":"v1.0.0","Time":"2024-01-01T00:00:00Z"}"#.into_response();
                }
                if path.ends_with(".mod") {
                    return "module example.com/foo\n".into_response();
                }
                if path.ends_with(".zip") {
                    return "ZIPDATA".into_response();
                }
                (StatusCode::NOT_FOUND, "404 page not found\n").into_response()
            })))
            .await;

            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "goproxy",
                meta::FORMAT_GO,
                meta::TYPE_PROXY,
                &upstream,
                repoconfig::default(),
            )
            .await;
            let app = mux(&tm.manager);

            for (path, want) in [
                ("/go/goproxy/example.com/foo/@v/list", "v1.0.0\nv1.1.0\n"),
                (
                    "/go/goproxy/example.com/foo/@v/v1.0.0.info",
                    r#""Version":"v1.0.0""#,
                ),
                (
                    "/go/goproxy/example.com/foo/@v/v1.0.0.mod",
                    "module example.com/foo",
                ),
                ("/go/goproxy/example.com/foo/@v/v1.0.0.zip", "ZIPDATA"),
            ] {
                let resp = call(&app, Method::GET, path, "").await;
                assert_eq!(resp.status, StatusCode::OK, "{path}");
                assert!(resp.text().contains(want), "{path}: {}", resp.text());
            }
        }

        #[test]
        fn go_helpers() {
            assert_eq!(go_kind("m/@v/list"), Kind::Metadata, "goKind misclassified");
            assert_eq!(
                go_kind("m/@v/v1.0.0.zip"),
                Kind::Artifact,
                "goKind misclassified"
            );
            assert_eq!(go_version("example.com/foo/@v/v1.2.3.mod"), "v1.2.3");
        }

        // --- Cargo ---

        #[tokio::test]
        async fn cargo_config_and_download() {
            let upstream = spawn_upstream(Router::new().fallback(any(|uri: Uri| async move {
                if uri.path().ends_with("/download") {
                    "CRATEDATA"
                } else {
                    r#"{"name":"serde","vers":"1.0.0"}"#
                }
            })))
            .await;

            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "crates",
                meta::FORMAT_CARGO,
                meta::TYPE_PROXY,
                &upstream,
                repoconfig::default(),
            )
            .await;
            let app = mux(&tm.manager);

            // config.json is synthesised and points back at this repo.
            let resp = call(&app, Method::GET, "/cargo/crates/config.json", "").await;
            let cfg: BTreeMap<String, String> =
                serde_json::from_slice(&resp.body).unwrap_or_default();
            assert!(
                cfg.get("dl")
                    .is_some_and(|dl| dl.contains("/cargo/crates/api/v1/crates/")),
                "config dl = {:?}",
                cfg.get("dl")
            );

            // Index entry (metadata).
            let resp = call(&app, Method::GET, "/cargo/crates/se/rd/serde", "").await;
            assert_eq!(resp.status, StatusCode::OK, "index");
            assert!(resp.text().contains("serde"), "index = {}", resp.text());

            // Download (artifact).
            let resp = call(
                &app,
                Method::GET,
                "/cargo/crates/api/v1/crates/serde/1.0.0/download",
                "",
            )
            .await;
            assert_eq!(resp.status, StatusCode::OK, "download");
            assert_eq!(resp.text(), "CRATEDATA");
        }

        #[test]
        fn cargo_helpers() {
            let dl = "se/rd/serde-x/api/v1/crates/serde/1.2.3/download";
            assert_eq!(
                cargo_kind(dl),
                Kind::Artifact,
                "download should be artifact"
            );
            assert_eq!(cargo_version(dl), "1.2.3");
            // The repo-relative download path arrives with its leading slash stripped
            // (`resolve`), so "api/v1/crates/..." must classify and version-extract just
            // like the prefixed form above.
            let real = "api/v1/crates/serde/1.0.197/download";
            assert_eq!(
                cargo_kind(real),
                Kind::Artifact,
                "leading-slash-stripped download should be artifact"
            );
            assert_eq!(cargo_version(real), "1.0.197");
            assert_eq!(
                cargo_kind("se/rd/serde"),
                Kind::Metadata,
                "index should be metadata"
            );
        }

        // --- npm ---

        #[tokio::test]
        async fn npm_proxy_rewrites_tarball_urls() {
            let upstream = spawn_upstream(Router::new().fallback(any(
                |uri: Uri, headers: HeaderMap| async move {
                    if uri.path().contains("/-/") {
                        return "TARBALL".to_string();
                    }
                    // The packument's tarball URL points back at this test server.
                    let host = headers
                        .get(http::header::HOST)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    format!(
                        r#"{{
        			"name":"left-pad",
        			"dist-tags":{{"latest":"1.3.0"}},
        			"versions":{{"1.3.0":{{"dist":{{"tarball":"http://{host}/left-pad/-/left-pad-1.3.0.tgz"}}}}}},
        			"time":{{"1.3.0":"2020-01-01T00:00:00Z"}}
        		}}"#
                    )
                },
            )))
            .await;

            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "npmproxy",
                meta::FORMAT_NPM,
                meta::TYPE_PROXY,
                &upstream,
                repoconfig::default(),
            )
            .await;
            let app = mux(&tm.manager);

            let resp = call(&app, Method::GET, "/npm/npmproxy/left-pad", "").await;
            assert_eq!(resp.status, StatusCode::OK, "packument");
            let doc: serde_json::Value =
                serde_json::from_slice(&resp.body).expect("packument json");
            let tarball = doc["versions"]["1.3.0"]["dist"]["tarball"]
                .as_str()
                .unwrap_or_default();
            assert!(
                tarball.contains("/npm/npmproxy/left-pad/-/left-pad-1.3.0.tgz"),
                "tarball not rewritten: {tarball:?}"
            );

            // Tarball fetch via the rewritten path.
            let resp = call(
                &app,
                Method::GET,
                "/npm/npmproxy/left-pad/-/left-pad-1.3.0.tgz",
                "",
            )
            .await;
            assert_eq!(resp.status, StatusCode::OK, "tarball");
            assert_eq!(resp.text(), "TARBALL");
        }

        /// The age-policy config both npm age tests use: a 30-day cooldown that blocks.
        fn age_block_30d() -> repoconfig::Config {
            let mut cfg = repoconfig::default();
            cfg.age_policy = AgePolicyConfig {
                enabled: true,
                min_age: Duration::from_std(std::time::Duration::from_secs(30 * 24 * 60 * 60)),
                action: ACTION_BLOCK.to_string(),
                ..Default::default()
            };
            cfg
        }

        #[tokio::test]
        async fn npm_age_policy_filters_versions() {
            let upstream = spawn_upstream(Router::new().fallback(any(|| async {
                r#"{
        			"name":"pkg",
        			"dist-tags":{"latest":"2.0.0"},
        			"versions":{
        				"1.0.0":{"dist":{"tarball":"http://up/pkg/-/pkg-1.0.0.tgz"}},
        				"2.0.0":{"dist":{"tarball":"http://up/pkg/-/pkg-2.0.0.tgz"}}
        			},
        			"time":{"1.0.0":"2024-01-01T00:00:00Z","2.0.0":"2025-06-09T00:00:00Z"}
        		}"#
            })))
            .await;

            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "p",
                meta::FORMAT_NPM,
                meta::TYPE_PROXY,
                &upstream,
                age_block_30d(),
            )
            .await;
            let at = Utc.with_ymd_and_hms(2025, 6, 10, 0, 0, 0).unwrap();
            tm.engine.set_now(Arc::new(move || at));

            let resp = call(&mux(&tm.manager), Method::GET, "/npm/p/pkg", "").await;
            let doc: serde_json::Value =
                serde_json::from_slice(&resp.body).expect("packument json");
            let versions = doc["versions"].as_object().expect("versions");
            assert!(
                !versions.contains_key("2.0.0"),
                "2.0.0 (1 day old) should be filtered by 30d cooldown"
            );
            assert!(versions.contains_key("1.0.0"), "1.0.0 (old) should remain");
            assert_eq!(
                doc["dist-tags"]["latest"], "1.0.0",
                "latest should be remapped to the best allowed version"
            );
        }

        /// Guards the age-gate fix: the tarball gate must derive the publish time from
        /// the packument `time` map, not the CDN's `Last-Modified` header. Here the
        /// header is recent (within the cooldown) while the packument says the version
        /// is old, so the tarball must be served.
        #[tokio::test]
        async fn npm_tarball_age_uses_packument_time() {
            let upstream = spawn_upstream(Router::new().fallback(any(|uri: Uri| async move {
                if uri.path().contains("/-/") {
                    // The npm CDN bumped the tarball mtime to yesterday; a
                    // Last-Modified based gate would wrongly block this under a 30d
                    // cooldown.
                    return (
                        [(
                            http::header::LAST_MODIFIED,
                            http_time(Utc.with_ymd_and_hms(2025, 6, 9, 0, 0, 0).unwrap()),
                        )],
                        "TARBALLBYTES",
                    )
                        .into_response();
                }
                r#"{
        			"name":"pkg",
        			"dist-tags":{"latest":"1.0.0"},
        			"versions":{"1.0.0":{"dist":{"tarball":"http://up/pkg/-/pkg-1.0.0.tgz"}}},
        			"time":{"1.0.0":"2024-01-01T00:00:00Z"}
        		}"#
                .into_response()
            })))
            .await;

            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "p",
                meta::FORMAT_NPM,
                meta::TYPE_PROXY,
                &upstream,
                age_block_30d(),
            )
            .await;
            let at = Utc.with_ymd_and_hms(2025, 6, 10, 0, 0, 0).unwrap();
            tm.engine.set_now(Arc::new(move || at));

            let resp = call(
                &mux(&tm.manager),
                Method::GET,
                "/npm/p/pkg/-/pkg-1.0.0.tgz",
                "",
            )
            .await;
            assert_eq!(
                resp.status,
                StatusCode::OK,
                "tarball should be allowed (packument time 2024-01-01 predates 30d cooldown): {}",
                resp.text()
            );
            assert_eq!(resp.text(), "TARBALLBYTES");
        }

        /// When the packument has no timestamp for the version, the gate falls back to
        /// the `Last-Modified` header.
        #[tokio::test]
        async fn npm_tarball_age_falls_back_to_last_modified() {
            let upstream = spawn_upstream(Router::new().fallback(any(|uri: Uri| async move {
                if uri.path().contains("/-/") {
                    return (
                        [(
                            http::header::LAST_MODIFIED,
                            http_time(Utc.with_ymd_and_hms(2025, 6, 9, 0, 0, 0).unwrap()),
                        )],
                        "TARBALLBYTES",
                    )
                        .into_response();
                }
                // The packument omits the `time` entry for 1.0.0, forcing the fallback.
                r#"{
        			"name":"pkg",
        			"dist-tags":{"latest":"1.0.0"},
        			"versions":{"1.0.0":{"dist":{"tarball":"http://up/pkg/-/pkg-1.0.0.tgz"}}},
        			"time":{}
        		}"#
                .into_response()
            })))
            .await;

            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "p",
                meta::FORMAT_NPM,
                meta::TYPE_PROXY,
                &upstream,
                age_block_30d(),
            )
            .await;
            let at = Utc.with_ymd_and_hms(2025, 6, 10, 0, 0, 0).unwrap();
            tm.engine.set_now(Arc::new(move || at));

            let resp = call(
                &mux(&tm.manager),
                Method::GET,
                "/npm/p/pkg/-/pkg-1.0.0.tgz",
                "",
            )
            .await;
            assert_eq!(
                resp.status,
                StatusCode::NOT_FOUND,
                "tarball should be blocked via Last-Modified fallback (1 day < 30d)"
            );
        }

        #[test]
        fn highest_stable_version_cases() {
            let cases: [(&[&str], &str); 5] = [
                (&["1.0.0", "2.1.0", "2.0.5"], "2.1.0"),
                (&["1.0.0", "2.0.0-beta.1"], "1.0.0"),
                (&["2.0.0-beta.1", "weird"], ""),
                (&["v3.0.0", "2.9.9"], "v3.0.0"),
                (&["0.0.10", "0.0.9"], "0.0.10"),
            ];
            for (versions, want) in cases {
                let map: BTreeMap<String, ()> =
                    versions.iter().map(|v| ((*v).to_string(), ())).collect();
                assert_eq!(
                    highest_stable_version(&map),
                    want,
                    "highest_stable_version({versions:?})"
                );
            }
        }

        /// Guards the raw-fragment rewrite: only `dist.tarball` is touched, and every
        /// other field of a version manifest (and the top-level document) must survive
        /// verbatim. This is the property the memory optimization trades on — versions
        /// stay as raw bytes instead of being expanded into a map — so a regression here
        /// silently corrupts packuments.
        #[test]
        fn rewrite_packument_preserves_fields() {
            let body = br#"{
        		"name":"pkg",
        		"description":"top-level field must survive",
        		"dist-tags":{"latest":"1.0.0"},
        		"versions":{"1.0.0":{
        			"name":"pkg",
        			"version":"1.0.0",
        			"dependencies":{"left-pad":"^1.0.0"},
        			"dist":{"tarball":"http://up/pkg/-/pkg-1.0.0.tgz","integrity":"sha512-abc","shasum":"deadbeef"}
        		}},
        		"time":{"1.0.0":"2020-01-01T00:00:00Z"}
        	}"#;
            let (out, removed) = rewrite_packument(
                body,
                "https://forklift.example",
                "npmproxy",
                "pkg",
                &AgePolicyConfig::default(),
                Utc::now(),
            );
            assert_eq!(removed, 0, "removed, want 0 (no age policy)");
            let doc: serde_json::Value =
                serde_json::from_slice(&out).expect("rewritten packument is not valid JSON");
            assert_eq!(
                doc["description"], "top-level field must survive",
                "top-level field dropped"
            );
            let version = &doc["versions"]["1.0.0"];
            assert_eq!(version["version"], "1.0.0", "version field dropped");
            assert_eq!(
                version["dependencies"]["left-pad"], "^1.0.0",
                "dependencies dropped"
            );
            assert_eq!(
                version["dist"]["integrity"], "sha512-abc",
                "sibling dist fields dropped"
            );
            assert_eq!(
                version["dist"]["shasum"], "deadbeef",
                "sibling dist fields dropped"
            );
            assert_eq!(
                version["dist"]["tarball"],
                "https://forklift.example/npm/npmproxy/pkg/-/pkg-1.0.0.tgz",
                "tarball not rewritten to forklift"
            );
        }

        #[tokio::test]
        async fn npm_publish_and_install() {
            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "local",
                meta::FORMAT_NPM,
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
            let app = mux(&tm.manager);

            let tarball = base64::engine::general_purpose::STANDARD.encode(b"TGZBYTES");
            let publish_doc = format!(
                r#"{{
        		"name":"mylib",
        		"versions":{{"1.0.0":{{"dist":{{"tarball":"http://x/mylib/-/mylib-1.0.0.tgz"}}}}}},
        		"_attachments":{{"mylib-1.0.0.tgz":{{"data":"{tarball}"}}}}
        	}}"#
            );
            let resp = call(&app, Method::PUT, "/npm/local/mylib", &publish_doc).await;
            assert_eq!(resp.status, StatusCode::CREATED, "publish: {}", resp.text());

            // The packument is retrievable and attachments are stripped.
            let resp = call(&app, Method::GET, "/npm/local/mylib", "").await;
            assert_eq!(resp.status, StatusCode::OK, "packument");
            assert!(
                !resp.text().contains("_attachments"),
                "packument = {}",
                resp.text()
            );

            // The tarball is retrievable.
            let resp = call(&app, Method::GET, "/npm/local/mylib/-/mylib-1.0.0.tgz", "").await;
            assert_eq!(resp.status, StatusCode::OK, "tarball");
            assert_eq!(resp.text(), "TGZBYTES");
            let repo = tm
                .store
                .get_repository_by_name("local")
                .await
                .expect("repository");
            let artifact = tm
                .store
                .get_artifact(repo.id, "mylib/-/mylib-1.0.0.tgz")
                .await
                .expect("stored tarball");
            assert_eq!(artifact.version, "1.0.0", "stored tarball version");
        }

        /// Guards the scope-slash decoding fix: npm publish PUTs a scoped package with a
        /// lowercase `%2f`, while pnpm fetches it with an uppercase `%2F` (or a literal
        /// slash). All spellings must resolve to the one artifact. Before the fix the raw
        /// request path was the storage key, so publish and fetch missed each other on
        /// encoding alone and hosted GETs 404'd.
        #[tokio::test]
        async fn npm_scoped_publish_install_encoding() {
            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "local",
                meta::FORMAT_NPM,
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
            let app = mux(&tm.manager);

            let tarball = base64::engine::general_purpose::STANDARD.encode(b"SCOPEDTGZ");
            let publish_doc = format!(
                r#"{{
        		"name":"@scope/name",
        		"versions":{{"1.0.0":{{"dist":{{"tarball":"http://x/@scope/name/-/name-1.0.0.tgz"}}}}}},
        		"_attachments":{{"name-1.0.0.tgz":{{"data":"{tarball}"}}}}
        	}}"#
            );
            // npm publish encodes the scope separator as lowercase %2f.
            let resp = call(&app, Method::PUT, "/npm/local/@scope%2fname", &publish_doc).await;
            assert_eq!(resp.status, StatusCode::CREATED, "publish: {}", resp.text());

            // The packument resolves via every scope spelling clients send: lowercase
            // %2f (npm), uppercase %2F (pnpm), a literal slash, and a fully-encoded @.
            for path in [
                "@scope%2fname",
                "@scope%2Fname",
                "@scope/name",
                "%40scope%2fname",
            ] {
                let resp = call(&app, Method::GET, &format!("/npm/local/{path}"), "").await;
                assert_eq!(resp.status, StatusCode::OK, "packument {path:?}");
                assert!(
                    !resp.text().contains("_attachments"),
                    "packument {path:?} = {}",
                    resp.text()
                );
            }

            // HEAD resolves the same way (npm/pnpm probe with it before download).
            let resp = call(&app, Method::HEAD, "/npm/local/@scope%2Fname", "").await;
            assert_eq!(resp.status, StatusCode::OK, "HEAD packument");

            // The tarball resolves whether the scope slash is literal or encoded.
            for path in [
                "@scope/name/-/name-1.0.0.tgz",
                "@scope%2fname/-/name-1.0.0.tgz",
            ] {
                let resp = call(&app, Method::GET, &format!("/npm/local/{path}"), "").await;
                assert_eq!(resp.status, StatusCode::OK, "tarball {path:?}");
                assert_eq!(resp.text(), "SCOPEDTGZ", "tarball {path:?}");
            }

            // Both are stored under the decoded canonical key, not the encoded path.
            let repo = tm
                .store
                .get_repository_by_name("local")
                .await
                .expect("repository");
            tm.store
                .get_artifact(repo.id, "@scope/name")
                .await
                .expect("packument not stored under decoded key");
            tm.store
                .get_artifact(repo.id, "@scope/name/-/name-1.0.0.tgz")
                .await
                .expect("tarball not stored under decoded key");
        }

        /// The decoded-path traversal recheck: a percent-encoded `../` (`%2e%2e%2f`)
        /// slips past `resolve`'s raw-path check but must be rejected once decoded, on
        /// both fetch and publish.
        #[tokio::test]
        async fn npm_encoded_traversal_rejected() {
            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "local",
                meta::FORMAT_NPM,
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
            let app = mux(&tm.manager);

            for (method, target) in [
                (Method::GET, "/npm/local/%2e%2e%2fetc%2fpasswd"),
                (Method::GET, "/npm/local/@scope%2f%2e%2e%2f%2e%2e"),
                (Method::PUT, "/npm/local/%2e%2e%2fevil"),
            ] {
                let resp = call(&app, method.clone(), target, "{}").await;
                assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{method} {target}");
            }
        }

        /// A scoped package installed through a proxy repo: pnpm requests it with an
        /// encoded scope slash, and the cache keys on the decoded identity so the second
        /// request is a hit rather than a duplicate fetch.
        #[tokio::test]
        async fn npm_proxy_scoped_encoded() {
            let hits = Arc::new(AtomicI32::new(0));
            let upstream_hits = Arc::clone(&hits);
            let upstream = spawn_upstream(Router::new().fallback(any(move || {
                let hits = Arc::clone(&upstream_hits);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    r#"{"name":"@scope/name","versions":{"1.0.0":{"dist":{"tarball":"http://up/@scope/name/-/name-1.0.0.tgz"}}},"time":{"1.0.0":"2020-01-01T00:00:00Z"}}"#
                }
            })))
            .await;

            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "np",
                meta::FORMAT_NPM,
                meta::TYPE_PROXY,
                &upstream,
                repoconfig::default(),
            )
            .await;
            let app = mux(&tm.manager);

            // The first request (encoded) misses and fetches upstream; the second
            // (uppercase encoding) must be served from the cache under the same decoded
            // key.
            for (index, path) in ["@scope%2fname", "@scope%2Fname"].iter().enumerate() {
                let resp = call(&app, Method::GET, &format!("/npm/np/{path}"), "").await;
                assert_eq!(resp.status, StatusCode::OK, "iter {index} ({path:?})");
            }
            assert_eq!(
                hits.load(Ordering::SeqCst),
                1,
                "upstream packument hits, want 1 (cached under decoded key)"
            );

            let repo = tm
                .store
                .get_repository_by_name("np")
                .await
                .expect("repository");
            tm.store
                .get_artifact(repo.id, "@scope/name")
                .await
                .expect("proxy packument not cached under decoded key");
        }

        /// Guards the hosted-packument rewrite: a scoped package published to a hosted
        /// member carries an absolute `dist.tarball` pointing at the publish registry,
        /// sometimes with a malformed scoped filename (as npm/pnpm emit). Served through
        /// a group, that URL must be rewritten to the group's own path with a filename
        /// derived from the stored tarball, so a client with only the group's credential
        /// can install it in one hop.
        #[tokio::test]
        async fn npm_group_rewrites_hosted_tarball_url() {
            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "npm-hosted",
                meta::FORMAT_NPM,
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
            let mut group_cfg = repoconfig::default();
            group_cfg.group.members = vec!["npm-hosted".to_string()];
            mk_format_repo(
                &tm.store,
                "npm-public",
                meta::FORMAT_NPM,
                meta::TYPE_GROUP,
                "",
                group_cfg,
            )
            .await;
            let app = mux(&tm.manager);

            let tarball = base64::engine::general_purpose::STANDARD.encode(b"SCOPEDTGZ");
            // The publish doc mirrors pnpm: an absolute tarball URL at the hosted
            // registry, with the scope duplicated into the filename.
            let publish_doc = format!(
                r#"{{
        		"name":"@scope/name",
        		"versions":{{"1.0.0":{{"dist":{{"tarball":"http://localhost:8080/npm/npm-hosted/@scope/name/-/@scope/name-1.0.0.tgz"}}}}}},
        		"_attachments":{{"name-1.0.0.tgz":{{"data":"{tarball}"}}}}
        	}}"#
            );
            let resp = call(
                &app,
                Method::PUT,
                "/npm/npm-hosted/@scope%2fname",
                &publish_doc,
            )
            .await;
            assert_eq!(resp.status, StatusCode::CREATED, "publish: {}", resp.text());

            // Fetch the packument through the group and read back the rewritten tarball.
            let request = Request::builder()
                .method(Method::GET)
                .uri("/npm/npm-public/@scope%2Fname")
                .header(http::header::HOST, "reg.example")
                .body(Body::empty())
                .expect("build request");
            let resp = send(&app, request).await;
            assert_eq!(resp.status, StatusCode::OK, "group packument");
            let doc: serde_json::Value =
                serde_json::from_slice(&resp.body).expect("packument json");
            assert_eq!(
                doc["versions"]["1.0.0"]["dist"]["tarball"],
                "http://reg.example/npm/npm-public/@scope/name/-/name-1.0.0.tgz",
                "rewritten tarball"
            );

            // That rewritten URL must actually serve the tarball through the group.
            let resp = call(
                &app,
                Method::GET,
                "/npm/npm-public/@scope/name/-/name-1.0.0.tgz",
                "",
            )
            .await;
            assert_eq!(resp.status, StatusCode::OK, "group tarball");
            assert_eq!(resp.text(), "SCOPEDTGZ");
        }

        #[tokio::test]
        async fn npm_proxy_cached_and_missing() {
            let hits = Arc::new(AtomicI32::new(0));
            let upstream_hits = Arc::clone(&hits);
            let upstream = spawn_upstream(Router::new().fallback(any(move |uri: Uri| {
                let hits = Arc::clone(&upstream_hits);
                async move {
                    if uri.path().contains("missing") {
                        return (StatusCode::NOT_FOUND, "404 page not found\n").into_response();
                    }
                    hits.fetch_add(1, Ordering::SeqCst);
                    r#"{"name":"p","versions":{"1.0.0":{"dist":{"tarball":"http://up/p/-/p-1.0.0.tgz"}}},"time":{"1.0.0":"2020-01-01T00:00:00Z"}}"#
                        .into_response()
                }
            })))
            .await;

            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "np",
                meta::FORMAT_NPM,
                meta::TYPE_PROXY,
                &upstream,
                repoconfig::default(),
            )
            .await;
            let app = mux(&tm.manager);

            for index in 0..2 {
                let resp = call(&app, Method::GET, "/npm/np/p", "").await;
                assert_eq!(resp.status, StatusCode::OK, "iter {index}");
            }
            assert_eq!(
                hits.load(Ordering::SeqCst),
                1,
                "packument hits, want 1 (cached)"
            );

            // A missing packument is a 404.
            let resp = call(&app, Method::GET, "/npm/np/missing", "").await;
            assert_eq!(resp.status, StatusCode::NOT_FOUND, "missing packument");
        }

        #[tokio::test]
        async fn npm_local_missing() {
            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "nl",
                meta::FORMAT_NPM,
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
            let resp = call(&mux(&tm.manager), Method::GET, "/npm/nl/nopkg", "").await;
            assert_eq!(resp.status, StatusCode::NOT_FOUND, "local missing");
        }

        #[tokio::test]
        async fn cargo_local_publish() {
            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "cl",
                meta::FORMAT_CARGO,
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
            let app = mux(&tm.manager);
            let path = "/cargo/cl/api/v1/crates/mylib/1.0.0/download";
            let resp = call(&app, Method::PUT, path, "CRATE").await;
            assert_eq!(resp.status, StatusCode::CREATED, "publish");
            let resp = call(&app, Method::GET, path, "").await;
            assert_eq!(resp.status, StatusCode::OK, "download");
            assert_eq!(resp.text(), "CRATE");
        }

        #[tokio::test]
        async fn go_local_put() {
            let tm = new_test_manager().await;
            mk_format_repo(
                &tm.store,
                "gl",
                meta::FORMAT_GO,
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
            let app = mux(&tm.manager);
            let path = "/go/gl/example.com/m/@v/v1.0.0.zip";
            let resp = call(&app, Method::PUT, path, "Z").await;
            assert_eq!(resp.status, StatusCode::CREATED, "put");
            let resp = call(&app, Method::GET, path, "").await;
            assert_eq!(resp.status, StatusCode::OK, "get");
            assert_eq!(resp.text(), "Z");
        }
    }

    mod ipacl {
        use axum::Router;
        use axum::body::Body;
        use http::{Method, Request, StatusCode};

        use crate::meta;
        use crate::repoconfig::{self, GroupConfig, IPACLConfig};

        use crate::testing::repo::{TestResponse, mk_repo, mux, new_test_manager, send};

        /// Issues a request carrying an `X-Forwarded-For` hop so the IP ACL sees a
        /// deterministic client IP regardless of the synthetic peer address.
        async fn do_ip(
            app: &Router,
            method: Method,
            path: &str,
            xff: &str,
            body: &str,
        ) -> TestResponse {
            let mut request = Request::builder().method(method).uri(path);
            if !xff.is_empty() {
                request = request.header("X-Forwarded-For", xff);
            }
            let request = request
                .body(if body.is_empty() {
                    Body::empty()
                } else {
                    Body::from(body.to_string())
                })
                .expect("build request");
            send(app, request).await
        }

        /// Covers the per-repository source-IP allow list on a hosted repository: an
        /// allowed IP is served, a disallowed IP gets 403, and the gate applies to
        /// writes too.
        #[tokio::test]
        async fn ip_acl_enforcement() {
            let tm = new_test_manager().await;
            let mut cfg = repoconfig::default();
            cfg.ip_acl = IPACLConfig {
                enabled: true,
                allow: vec!["203.0.113.0/24".to_string()],
            };
            mk_repo(&tm.store, "mvn-acl", meta::TYPE_HOSTED, "", cfg).await;
            let app = mux(&tm.manager);
            let path = "/maven/mvn-acl/com/example/app/1.0/app-1.0.jar";

            // Upload from an allowed IP succeeds.
            let resp = do_ip(&app, Method::PUT, path, "203.0.113.7", "JARBYTES").await;
            assert_eq!(resp.status, StatusCode::CREATED, "allowed put");
            // Download from an allowed IP succeeds.
            let resp = do_ip(&app, Method::GET, path, "203.0.113.9", "").await;
            assert_eq!(resp.status, StatusCode::OK, "allowed get");
            // Download from a disallowed IP is refused before serving.
            let resp = do_ip(&app, Method::GET, path, "198.51.100.4", "").await;
            assert_eq!(resp.status, StatusCode::FORBIDDEN, "denied get");
            // Write from a disallowed IP is refused too.
            let resp = do_ip(&app, Method::PUT, path, "198.51.100.4", "X").await;
            assert_eq!(resp.status, StatusCode::FORBIDDEN, "denied put");
        }

        /// On a group repository the group's own ACL governs entry, and member ACLs are
        /// not re-checked during fan-out.
        #[tokio::test]
        async fn ip_acl_group_governs_entry() {
            let tm = new_test_manager().await;

            // Member: hosted repo with an ACL that would block the test client outright.
            let mut member_cfg = repoconfig::default();
            member_cfg.ip_acl = IPACLConfig {
                enabled: true,
                allow: vec!["10.0.0.0/8".to_string()],
            };
            mk_repo(&tm.store, "mvn-member", meta::TYPE_HOSTED, "", member_cfg).await;
            // Seed an artifact in the member, bypassing the ACL by writing as an
            // allowed IP.
            let app = mux(&tm.manager);
            let member_path = "/maven/mvn-member/com/example/app/1.0/app-1.0.jar";
            let resp = do_ip(&app, Method::PUT, member_path, "10.1.2.3", "JARBYTES").await;
            assert_eq!(resp.status, StatusCode::CREATED, "seed member put");

            // Group: allows the test client; members listed in lookup order.
            let mut group_cfg = repoconfig::default();
            group_cfg.ip_acl = IPACLConfig {
                enabled: true,
                allow: vec!["203.0.113.0/24".to_string()],
            };
            group_cfg.group = GroupConfig {
                members: vec!["mvn-member".to_string()],
            };
            mk_repo(&tm.store, "mvn-group", meta::TYPE_GROUP, "", group_cfg).await;
            let group_path = "/maven/mvn-group/com/example/app/1.0/app-1.0.jar";

            // Allowed by the group ACL: served from the member even though the member's
            // own ACL would block this client (member checks skipped via group).
            let resp = do_ip(&app, Method::GET, group_path, "203.0.113.5", "").await;
            assert_eq!(resp.status, StatusCode::OK, "group allowed get");
            // Denied by the group ACL: refused at entry, members never tried.
            let resp = do_ip(&app, Method::GET, group_path, "198.51.100.4", "").await;
            assert_eq!(resp.status, StatusCode::FORBIDDEN, "group denied get");
        }
    }

    mod rewritegate {
        use std::time::{Duration, Instant};

        use crate::repo::MAX_CONCURRENT_REWRITES;
        use crate::testing::repo::new_test_manager;

        /// Polls until `want` is observed, so the test never depends on how fast the
        /// queued task reaches the semaphore.
        async fn wait_for_gauge(read: impl Fn() -> f64, want: f64) {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if read() == want {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            panic!("gauge = {}, want {want}", read());
        }

        fn series(histogram: &prometheus::HistogramVec) -> usize {
            use prometheus::core::Collector as _;
            histogram
                .collect()
                .iter()
                .map(|family| family.get_metric().len())
                .sum()
        }

        /// A client that gives up while queued is the failure this instrumentation
        /// exists for: the installer sees a request with no status code, so nothing in
        /// the response metrics records it. The abandoned counter is the only place it
        /// shows up, and the queue gauges have to unwind on that path too.
        #[tokio::test]
        async fn rewrite_gate_reports_queueing_and_abandonment() {
            let tm = new_test_manager().await;
            let engine = std::sync::Arc::clone(&tm.engine);
            assert_eq!(
                engine.gate_capacity.get(),
                MAX_CONCURRENT_REWRITES as f64,
                "capacity"
            );

            let mut slots = Vec::with_capacity(MAX_CONCURRENT_REWRITES);
            for _ in 0..MAX_CONCURRENT_REWRITES {
                slots.push(
                    engine
                        .acquire_rewrite("npmjs")
                        .await
                        .expect("acquire failed on a free gate"),
                );
            }
            assert_eq!(
                engine.gate_inflight.get(),
                MAX_CONCURRENT_REWRITES as f64,
                "inflight"
            );

            let queued_engine = std::sync::Arc::clone(&engine);
            let queued =
                tokio::spawn(async move { queued_engine.acquire_rewrite("npmjs").await.is_some() });
            {
                let engine = std::sync::Arc::clone(&engine);
                wait_for_gauge(move || engine.gate_queued.get(), 1.0).await;
            }

            queued.abort();
            assert!(
                queued.await.is_err(),
                "acquire succeeded for a cancelled request"
            );
            {
                let engine = std::sync::Arc::clone(&engine);
                wait_for_gauge(
                    move || engine.gate_abandoned.with_label_values(&["npmjs"]).get(),
                    1.0,
                )
                .await;
            }
            assert_eq!(
                engine.gate_queued.get(),
                0.0,
                "queued after the client left"
            );

            slots.clear();
            assert_eq!(engine.gate_inflight.get(), 0.0, "inflight");
            // A `RewriteSlot` releases by being dropped and cannot be dropped twice, so the type system
            // carries that invariant instead of a test.
        }

        /// The gate is shared across formats, so a queue built by one repository has to
        /// be attributable to the requests that actually waited in it.
        #[tokio::test]
        async fn rewrite_gate_labels_wait_by_repository() {
            let tm = new_test_manager().await;
            let slot = tm
                .engine
                .acquire_rewrite("pypi")
                .await
                .expect("acquire failed on a free gate");
            drop(slot);
            assert_eq!(series(&tm.engine.gate_wait), 1, "wait series");
            assert_eq!(series(&tm.engine.gate_hold), 1, "hold series");
        }
    }

    mod unit {
        use std::sync::Arc;
        use std::time::Duration as StdDuration;

        use chrono::{DateTime, TimeZone, Utc};
        use parking_lot::Mutex;

        use crate::repoconfig::{ACTION_BLOCK, ACTION_WARN, AgePolicyConfig, Duration};

        use crate::repo::maven::{maven_kind, maven_version};
        use crate::repo::router::join_upstream;
        use crate::repo::{AgeDecision, Kind, NegCache, evaluate_age, itoa};

        fn at(y: i32, mo: u32, d: u32) -> DateTime<Utc> {
            Utc.with_ymd_and_hms(y, mo, d, 0, 0, 0)
                .single()
                .expect("valid instant")
        }

        fn days(n: i64) -> Duration {
            Duration(n * 24 * 3600 * 1_000_000_000)
        }

        #[test]
        fn evaluate_age_cases() {
            let now = at(2025, 6, 10);
            let recent = now - chrono::TimeDelta::hours(24);
            let old = now - chrono::TimeDelta::days(100);

            let cases: Vec<(&str, AgePolicyConfig, Option<DateTime<Utc>>, AgeDecision)> = vec![
                (
                    "disabled",
                    AgePolicyConfig {
                        enabled: false,
                        min_age: Duration(3_600_000_000_000),
                        ..Default::default()
                    },
                    Some(recent),
                    AgeDecision::Allow,
                ),
                (
                    "no publish time",
                    AgePolicyConfig {
                        enabled: true,
                        min_age: Duration(3_600_000_000_000),
                        ..Default::default()
                    },
                    None,
                    AgeDecision::Allow,
                ),
                (
                    "too new block",
                    AgePolicyConfig {
                        enabled: true,
                        min_age: days(7),
                        action: ACTION_BLOCK.into(),
                        ..Default::default()
                    },
                    Some(recent),
                    AgeDecision::Block,
                ),
                (
                    "too new warn",
                    AgePolicyConfig {
                        enabled: true,
                        min_age: days(7),
                        action: ACTION_WARN.into(),
                        ..Default::default()
                    },
                    Some(recent),
                    AgeDecision::Warn,
                ),
                (
                    "old enough",
                    AgePolicyConfig {
                        enabled: true,
                        min_age: days(7),
                        action: ACTION_BLOCK.into(),
                        ..Default::default()
                    },
                    Some(old),
                    AgeDecision::Allow,
                ),
                (
                    "too old block",
                    AgePolicyConfig {
                        enabled: true,
                        max_age: days(30),
                        action: ACTION_BLOCK.into(),
                        ..Default::default()
                    },
                    Some(old),
                    AgeDecision::Block,
                ),
            ];
            for (name, cfg, pub_at, want) in cases {
                let (got, _) = evaluate_age(&cfg, pub_at, now);
                assert_eq!(got, want, "evaluate_age({name})");
            }
        }

        #[test]
        fn neg_cache() {
            let mut c = NegCache::new();
            let base = at(2025, 1, 1);
            let clock = Arc::new(Mutex::new(base));
            let handle = Arc::clone(&clock);
            c.set_now(Arc::new(move || *handle.lock()));

            // Zero TTL is ignored.
            c.set("a", StdDuration::ZERO);
            assert!(!c.has("a"), "zero ttl should not cache");

            c.set("b", StdDuration::from_secs(60));
            assert!(c.has("b"), "b should be cached");
            // Advance past expiry.
            *clock.lock() = base + chrono::TimeDelta::minutes(2);
            assert!(!c.has("b"), "b should have expired");

            *clock.lock() = base;
            c.set("c", StdDuration::from_secs(60));
            c.clear("c");
            assert!(!c.has("c"), "c should be cleared");
        }

        #[test]
        fn itoa_renders_decimal() {
            for (input, want) in [(0i64, "0"), (5, "5"), (42, "42"), (1024, "1024")] {
                assert_eq!(itoa(input), want, "itoa({input})");
            }
        }

        #[test]
        fn maven_helpers() {
            assert_eq!(
                maven_kind("g/a/maven-metadata.xml"),
                Kind::Metadata,
                "metadata not classified"
            );
            assert_eq!(
                maven_kind("g/a/1.0/a-1.0.jar"),
                Kind::Artifact,
                "artifact not classified"
            );
            assert_eq!(maven_version("com/ex/app/1.2.3/app-1.2.3.jar"), "1.2.3");
            assert_eq!(
                maven_version("com/ex/app/maven-metadata.xml"),
                "",
                "metadata version should be empty"
            );
        }

        #[test]
        fn join_upstream_slash_handling() {
            assert_eq!(join_upstream("https://repo/", "/g/a"), "https://repo/g/a");
        }
    }

    mod upstreamauth {
        use std::sync::Arc;

        use axum::Router;
        use axum::extract::State;
        use axum::response::{IntoResponse, Response};
        use axum::routing::any;
        use base64::Engine as _;
        use http::{HeaderMap, Method, StatusCode};
        use parking_lot::Mutex;

        use crate::meta;
        use crate::repoconfig::{
            self, UPSTREAM_AUTH_BASIC, UPSTREAM_AUTH_HEADER, UpstreamAuthConfig,
        };
        use crate::server::http_error;

        use crate::testing::repo::{call, mk_format_repo, mux, new_test_manager, spawn_upstream};

        /// A proxy configured with upstream credentials fetches from an upstream that
        /// rejects anonymous requests; without credentials the same fetch fails.
        #[tokio::test]
        async fn upstream_auth_proxy_fetch() {
            let upstream =
                spawn_upstream(Router::new().fallback(any(|headers: HeaderMap| async move {
                    if basic_auth(&headers) != Some(("mirror".to_string(), "s3cret".to_string())) {
                        return (
                            [(http::header::WWW_AUTHENTICATE, "Basic realm=\"upstream\"")],
                            http_error(StatusCode::UNAUTHORIZED, "unauthorized"),
                        )
                            .into_response();
                    }
                    "jar-bytes".into_response()
                })))
                .await;

            let tm = new_test_manager().await;
            let mut cfg = repoconfig::default();
            cfg.upstream_auth = UpstreamAuthConfig {
                type_: UPSTREAM_AUTH_BASIC.to_string(),
                username: "mirror".to_string(),
                password: "s3cret".to_string(),
                ..Default::default()
            };
            mk_format_repo(
                &tm.store,
                "maven-priv",
                meta::FORMAT_MAVEN,
                meta::TYPE_PROXY,
                &upstream,
                cfg,
            )
            .await;
            mk_format_repo(
                &tm.store,
                "maven-anon",
                meta::FORMAT_MAVEN,
                meta::TYPE_PROXY,
                &upstream,
                repoconfig::default(),
            )
            .await;

            let app = mux(&tm.manager);
            let resp = call(
                &app,
                Method::GET,
                "/maven/maven-priv/org/x/lib/1.0/lib-1.0.jar",
                "",
            )
            .await;
            assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
            assert_eq!(resp.text(), "jar-bytes");

            let resp = call(
                &app,
                Method::GET,
                "/maven/maven-anon/org/x/lib/1.0/lib-1.0.jar",
                "",
            )
            .await;
            assert_ne!(
                resp.status,
                StatusCode::OK,
                "anonymous fetch should fail: {}",
                resp.text()
            );
        }

        /// A compromised upstream redirecting to another host must not receive the
        /// repository's credentials: Authorization and custom auth headers are stripped
        /// on cross-host redirects.
        #[tokio::test]
        async fn upstream_auth_stripped_on_cross_host_redirect() {
            let seen: Arc<Mutex<(String, String)>> =
                Arc::new(Mutex::new((String::new(), String::new())));
            let attacker_state = Arc::clone(&seen);
            let attacker = spawn_upstream(
                Router::new()
                    .fallback(any(
                        move |State(seen): State<Arc<Mutex<(String, String)>>>,
                              headers: HeaderMap| async move {
                            let header = |name: http::HeaderName| {
                                headers
                                    .get(name)
                                    .and_then(|v| v.to_str().ok())
                                    .unwrap_or("")
                                    .to_string()
                            };
                            *seen.lock() = (
                                header(http::header::AUTHORIZATION),
                                header(http::HeaderName::from_static("x-api-key")),
                            );
                            "redirected-bytes"
                        },
                    ))
                    .with_state(attacker_state),
            )
            .await;
            let redirect_target = attacker.clone();
            let upstream = spawn_upstream(Router::new().fallback(any(move |uri: http::Uri| {
                let target = redirect_target.clone();
                async move {
                    Response::builder()
                        .status(StatusCode::FOUND)
                        .header(http::header::LOCATION, format!("{target}{}", uri.path()))
                        .body(axum::body::Body::empty())
                        .expect("redirect response")
                }
            })))
            .await;

            let tm = new_test_manager().await;
            let mut cfg = repoconfig::default();
            cfg.upstream_auth = UpstreamAuthConfig {
                type_: UPSTREAM_AUTH_HEADER.to_string(),
                header: "X-Api-Key".to_string(),
                value: "k".to_string(),
                ..Default::default()
            };
            mk_format_repo(
                &tm.store,
                "maven-hdr",
                meta::FORMAT_MAVEN,
                meta::TYPE_PROXY,
                &upstream,
                cfg,
            )
            .await;

            let app = mux(&tm.manager);
            let resp = call(
                &app,
                Method::GET,
                "/maven/maven-hdr/org/x/lib/1.0/lib-1.0.jar",
                "",
            )
            .await;
            assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
            let (authorization, api_key) = seen.lock().clone();
            assert!(
                authorization.is_empty() && api_key.is_empty(),
                "credentials leaked across hosts: Authorization={authorization:?} X-Api-Key={api_key:?}"
            );
        }

        fn basic_auth(headers: &HeaderMap) -> Option<(String, String)> {
            let value = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
            let encoded = value.strip_prefix("Basic ")?;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()?;
            let decoded = String::from_utf8(decoded).ok()?;
            let (user, password) = decoded.split_once(':')?;
            Some((user.to_string(), password.to_string()))
        }
    }

    mod engine {
        use axum::Router;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicI32, Ordering};

        use chrono::{TimeZone, Utc};
        use http::{Method, StatusCode};

        use crate::meta::{self};
        use crate::repoconfig::{self, ACTION_BLOCK, AgePolicyConfig, Duration};

        use crate::testing::repo::*;
        #[tokio::test]
        async fn maven_local_round_trip() {
            let tm = new_test_manager().await;
            mk_repo(
                &tm.store,
                "mvn-local",
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
            let h = mux(&tm.manager);
            let path = "/maven/mvn-local/com/example/app/1.0/app-1.0.jar";

            // Upload.
            let resp = call(&h, Method::PUT, path, "JARBYTES").await;
            assert_eq!(resp.status, StatusCode::CREATED, "put");

            // Download.
            let resp = call(&h, Method::GET, path, "").await;
            assert_eq!(resp.status, StatusCode::OK, "get");
            assert_eq!(resp.text(), "JARBYTES");
            assert_eq!(resp.header("Content-Type"), "application/java-archive");

            // HEAD returns headers, no body.
            let resp = call(&h, Method::HEAD, path, "").await;
            assert_eq!(resp.status, StatusCode::OK, "head");
            assert_eq!(resp.body.len(), 0, "head body");
            assert_eq!(resp.header("Content-Length"), "8");
        }

        #[tokio::test]
        async fn maven_local_missing() {
            let tm = new_test_manager().await;
            mk_repo(
                &tm.store,
                "mvn-local",
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
            let resp = call(
                &mux(&tm.manager),
                Method::GET,
                "/maven/mvn-local/x/y/1/y-1.jar",
                "",
            )
            .await;
            assert_eq!(resp.status, StatusCode::NOT_FOUND, "missing");
        }

        /// The repository root with a trailing slash is routed to the format handler,
        /// which rejects the empty path, rather than to the console fallback.
        #[tokio::test]
        async fn maven_repository_root_is_invalid_path() {
            let tm = new_test_manager().await;
            mk_repo(
                &tm.store,
                "mvn-local",
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
            let resp = call(&mux(&tm.manager), Method::GET, "/maven/mvn-local/", "").await;
            assert_eq!(resp.status, StatusCode::BAD_REQUEST, "root path");
        }

        #[tokio::test]
        async fn maven_proxy_caching() {
            let hits = Arc::new(AtomicI32::new(0));
            let counter = Arc::clone(&hits);
            let upstream = spawn_upstream(Router::new().fallback(move || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let stamp = http_time(Utc::now() - chrono::TimeDelta::days(365));
                    ([("Last-Modified", stamp)], "UPSTREAM-JAR")
                }
            }))
            .await;

            let tm = new_test_manager().await;
            mk_repo(
                &tm.store,
                "mvn-proxy",
                meta::TYPE_PROXY,
                &upstream,
                repoconfig::default(),
            )
            .await;
            let h = mux(&tm.manager);
            let path = "/maven/mvn-proxy/g/a/1.0/a-1.0.jar";

            for i in 0..3 {
                let resp = call(&h, Method::GET, path, "").await;
                assert_eq!(resp.status, StatusCode::OK, "iter {i}");
                assert_eq!(resp.text(), "UPSTREAM-JAR", "iter {i}");
            }
            assert_eq!(
                hits.load(Ordering::SeqCst),
                1,
                "upstream hits (artifact cached)"
            );
        }

        #[tokio::test]
        async fn maven_proxy_cache_disabled_passthrough() {
            let hits = Arc::new(AtomicI32::new(0));
            let counter = Arc::clone(&hits);
            let upstream = spawn_upstream(Router::new().fallback(move || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    "X"
                }
            }))
            .await;

            let mut cfg = repoconfig::default();
            cfg.cache.enabled = false;
            let tm = new_test_manager().await;
            mk_repo(&tm.store, "p", meta::TYPE_PROXY, &upstream, cfg).await;
            let h = mux(&tm.manager);
            for _ in 0..2 {
                let resp = call(&h, Method::GET, "/maven/p/g/a/1/a-1.jar", "").await;
                assert_eq!(resp.status, StatusCode::OK);
            }
            assert_eq!(hits.load(Ordering::SeqCst), 2, "passthrough hits");
        }

        #[tokio::test]
        async fn maven_proxy_negative_cache() {
            let hits = Arc::new(AtomicI32::new(0));
            let counter = Arc::clone(&hits);
            let upstream = spawn_upstream(Router::new().fallback(move || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::NOT_FOUND, "404 page not found\n")
                }
            }))
            .await;

            let tm = new_test_manager().await;
            mk_repo(
                &tm.store,
                "p",
                meta::TYPE_PROXY,
                &upstream,
                repoconfig::default(),
            )
            .await;
            let h = mux(&tm.manager);
            for i in 0..3 {
                let resp = call(&h, Method::GET, "/maven/p/g/a/1/missing.jar", "").await;
                assert_eq!(resp.status, StatusCode::NOT_FOUND, "iter {i}");
            }
            assert_eq!(
                hits.load(Ordering::SeqCst),
                1,
                "upstream 404 hits (negative cached)"
            );
        }

        #[tokio::test]
        async fn maven_proxy_age_policy_blocks() {
            let publish_time = Utc
                .with_ymd_and_hms(2025, 6, 1, 0, 0, 0)
                .single()
                .expect("valid instant");
            let stamp = http_time(publish_time);
            let upstream = spawn_upstream(Router::new().fallback(move || {
                let stamp = stamp.clone();
                async move { ([("Last-Modified", stamp)], "FRESH") }
            }))
            .await;

            let mut cfg = repoconfig::default();
            cfg.age_policy = AgePolicyConfig {
                enabled: true,
                min_age: Duration(30 * 24 * 3600 * 1_000_000_000),
                action: ACTION_BLOCK.to_string(),
                ..Default::default()
            };
            let tm = new_test_manager().await;
            mk_repo(&tm.store, "p", meta::TYPE_PROXY, &upstream, cfg).await;
            let h = mux(&tm.manager);

            // "Now" is 10 days after publish: younger than the 30-day cooldown -> blocked.
            let at = publish_time + chrono::TimeDelta::days(10);
            tm.engine.set_now(Arc::new(move || at));
            let resp = call(&h, Method::GET, "/maven/p/g/a/2.0/a-2.0.jar", "").await;
            assert_eq!(resp.status, StatusCode::NOT_FOUND, "within cooldown");

            // 60 days after publish: past the cooldown -> allowed.
            let at = publish_time + chrono::TimeDelta::days(60);
            tm.engine.set_now(Arc::new(move || at));
            let resp = call(&h, Method::GET, "/maven/p/g/a/3.0/a-3.0.jar", "").await;
            assert_eq!(resp.status, StatusCode::OK, "past cooldown");
            assert_eq!(resp.text(), "FRESH");
        }

        #[tokio::test]
        async fn resolve_errors() {
            let tm = new_test_manager().await;
            mk_repo(
                &tm.store,
                "mvn-local",
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
            let h = mux(&tm.manager);

            // Unknown repository.
            let resp = call(&h, Method::GET, "/maven/nope/a/b/1/x.jar", "").await;
            assert_eq!(resp.status, StatusCode::NOT_FOUND, "unknown repo");

            // Path traversal attempt.
            let resp = call(&h, Method::GET, "/maven/mvn-local/../../etc/passwd", "").await;
            assert_ne!(resp.status, StatusCode::OK, "traversal should not succeed");

            // Upload to a proxy repo is rejected.
            mk_repo(
                &tm.store,
                "mvn-proxy",
                meta::TYPE_PROXY,
                "https://example.com",
                repoconfig::default(),
            )
            .await;
            let resp = call(&h, Method::PUT, "/maven/mvn-proxy/g/a/1/a-1.jar", "x").await;
            assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED, "proxy put");
        }
    }
}
