//! Mounts the package-format protocol routes and owns the per-request
//! middleware chain (group fan-out, audit, RBAC, IP ACL).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::Router;
use axum::extract::Request;
use axum::response::Response;
use chrono::{DateTime, Utc};
use http::request::Parts;
use http::{Method, StatusCode};
use prometheus::{CounterVec, Histogram, HistogramOpts, Opts, Registry};

use crate::audit;
use crate::auth;
use crate::license;
use crate::meta::{self, Repository, Store};
use crate::repoconfig::{self, Config};
use crate::server::http_error;
use crate::vuln;

use super::group_metadata::GroupMetadataLock;
use super::licensescan::ResolveJob;
use super::oci_proxy::OciTokenCache;
use super::uiupload::Uploader;
use super::vulnscan::{ScanJob, ScanRatio};
use super::{Engine, NegCache, header_str, username_from_context};

/// The boxed future every format handler returns, so the router can hold them
/// as plain function pointers.
pub(crate) type HandlerFuture = Pin<Box<dyn Future<Output = Response> + Send>>;

/// A per-format protocol handler (`handle_maven`, `handle_npm`, …).
pub(crate) type HandlerFn = fn(Arc<Manager>, Request) -> HandlerFuture;

/// The boxed future a policy gate returns: `Some(response)` when the gate
/// answered the request, `None` to continue.
pub(crate) type HandlerFutureOpt = Pin<Box<dyn Future<Output = Option<Response>> + Send>>;

/// Invoked when a package is newly quarantined pending approval.
pub(crate) type OnApprovalFn =
    Arc<dyn Fn(String, i64, String, String, String, String, Vec<String>) + Send + Sync>;

/// The cached per-repository scan-coverage aggregate and when it was computed.
#[derive(Default)]
pub(crate) struct ScanRatioState {
    pub(crate) ratios: HashMap<i64, ScanRatio>,
    pub(crate) at: Option<DateTime<Utc>>,
}

/// A bounded work queue: the sender lives on the manager, the receiver is taken
/// once by the background worker.
pub(crate) struct JobQueue<T> {
    pub(crate) tx: tokio::sync::mpsc::Sender<T>,
    pub(crate) rx: tokio::sync::Mutex<Option<tokio::sync::mpsc::Receiver<T>>>,
}

impl<T> JobQueue<T> {
    fn new(capacity: usize) -> JobQueue<T> {
        let (tx, rx) = tokio::sync::mpsc::channel(capacity);
        JobQueue {
            tx,
            rx: tokio::sync::Mutex::new(Some(rx)),
        }
    }
}

/// Mounts the package-format protocol routes onto a router.
pub struct Manager {
    pub(crate) engine: Arc<Engine>,
    pub(crate) store: Arc<Store>,
    pub(crate) authz: Option<Arc<auth::Service>>,
    pub(crate) rec: Option<Arc<audit::Recorder>>,
    pub(crate) uploader: parking_lot::RwLock<Option<Arc<Uploader>>>,
    /// Serialises group aggregate rebuilds per cache key.
    pub(crate) group_metadata_locks: parking_lot::Mutex<HashMap<String, Arc<GroupMetadataLock>>>,
    /// When set, overrides request-derived bases in synthesised URLs (see
    /// `external_base`).
    pub(crate) external_url: parking_lot::RwLock<String>,

    /// Caches the per-repository scan-coverage aggregate the console's
    /// repository list shows (see `clean_scan_ratios`): the computation reads
    /// every versioned artifact, so it is served from here for `SCAN_RATIO_TTL`
    /// instead of being run once per request.
    pub(crate) scan_ratios: parking_lot::Mutex<ScanRatioState>,
    /// Serialises the recomputation itself.
    pub(crate) scan_ratio_refresh: tokio::sync::Mutex<()>,
    pub(crate) scan_ratio_served: CounterVec,
    pub(crate) scan_ratio_cost: Histogram,

    /// Suppresses repeated pending-approval upserts (see `approval_gate`).
    pub(crate) req_marks: NegCache,
    pub(crate) approval_blocked: CounterVec,
    pub(crate) approval_auto_approved: CounterVec,
    pub(crate) deny_blocked: CounterVec,
    pub(crate) ttl_expired: CounterVec,
    pub(crate) vuln_blocked: CounterVec,
    pub(crate) vuln_scans: CounterVec,
    pub(crate) license_blocked: CounterVec,
    pub(crate) license_resolves: CounterVec,
    pub(crate) ip_blocked: CounterVec,
    pub(crate) oci_prune_deleted: CounterVec,

    /// OCI push/pull state: the upload-session byte directory (shared across
    /// replicas, see `oci_push.rs`), size caps, and the upstream bearer-token
    /// cache for the proxy path (see `oci_proxy.rs`).
    pub(crate) oci_upload_dir: parking_lot::RwLock<String>,
    pub(crate) oci_max_manifest_bytes: AtomicI64,
    pub(crate) oci_max_blob_bytes: AtomicI64,
    pub(crate) oci_tokens: OciTokenCache,

    /// Performs vulnerability lookups; `None` disables the vuln gate. Scan jobs
    /// are queued and processed by `run_vuln_worker` so the serving path never
    /// blocks on an advisory lookup.
    pub(crate) scanner: parking_lot::RwLock<Option<Arc<dyn vuln::Scanner>>>,
    pub(crate) scan_queue: JobQueue<ScanJob>,

    /// Performs license lookups; `None` disables the license gate. Resolve jobs
    /// are queued and processed by `run_license_worker` so the serving path
    /// never blocks on a license lookup.
    pub(crate) resolver: parking_lot::RwLock<Option<Arc<dyn license::Resolver>>>,
    pub(crate) resolve_queue: JobQueue<ResolveJob>,

    /// When set, invoked when a package is newly quarantined pending approval,
    /// so an outbound alarm can be dispatched to the repository's selected
    /// receivers. Best-effort and must not block; `None` disables it.
    pub(crate) on_approval: parking_lot::RwLock<Option<OnApprovalFn>>,
}

impl Manager {
    /// Creates a manager. `authz` may be `None` to disable authorization (all
    /// access allowed), `rec` may be `None` to disable audit logging, and
    /// `registry` may be `None` to skip metric registration, all of which are
    /// useful in tests.
    pub fn new(
        engine: Arc<Engine>,
        store: Arc<Store>,
        authz: Option<Arc<auth::Service>>,
        rec: Option<Arc<audit::Recorder>>,
        registry: Option<&Registry>,
    ) -> Arc<Manager> {
        let m = Arc::new(Manager {
            engine: Arc::clone(&engine),
            store: Arc::clone(&store),
            authz,
            rec,
            uploader: parking_lot::RwLock::new(None),
            group_metadata_locks: parking_lot::Mutex::new(HashMap::new()),
            external_url: parking_lot::RwLock::new(String::new()),
            scan_ratios: parking_lot::Mutex::new(ScanRatioState::default()),
            scan_ratio_refresh: tokio::sync::Mutex::new(()),
            scan_ratio_served: counter_vec(
                "scan_ratio_lookups_total",
                "Repository-list scan-coverage lookups by cache outcome (hit means no database scan).",
                &["result"],
            ),
            scan_ratio_cost: Histogram::with_opts(
                HistogramOpts::new(
                    "scan_ratio_refresh_seconds",
                    "Time to recompute the per-repository scan-coverage aggregate (whole-table scan).",
                )
                .namespace("forklift")
                .buckets(vec![0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]),
            )
            .expect("valid histogram"),
            req_marks: NegCache::new(),
            approval_blocked: counter_vec(
                "approval_blocked_total",
                "Requests blocked (or counted in audit mode) by the package approval policy.",
                &["repo", "mode"],
            ),
            approval_auto_approved: counter_vec(
                "approval_auto_approved_total",
                "Packages auto-approved by the clean-scan approval policy (max severity none).",
                &["repo"],
            ),
            deny_blocked: counter_vec(
                "version_deny_blocked_total",
                "Requests blocked by the per-version deny list.",
                &["repo"],
            ),
            ttl_expired: counter_vec(
                "ttl_expired_total",
                "Artifacts auto-deleted by the idle retention reaper.",
                &["repo"],
            ),
            vuln_blocked: counter_vec(
                "vuln_blocked_total",
                "Requests blocked (or counted in warn/audit mode) by the vulnerability policy.",
                &["repo", "action"],
            ),
            vuln_scans: counter_vec(
                "vuln_scans_total",
                "Vulnerability scans performed, by result (clean|vulnerable|error).",
                &["result"],
            ),
            license_blocked: counter_vec(
                "license_blocked_total",
                "Requests blocked (or counted in warn/audit mode) by the license policy.",
                &["repo", "action"],
            ),
            license_resolves: counter_vec(
                "license_resolves_total",
                "License resolutions performed, by result (resolved|unknown|error).",
                &["result"],
            ),
            ip_blocked: counter_vec(
                "ip_blocked_total",
                "Requests refused by the per-repository source-IP access control list.",
                &["repo"],
            ),
            oci_prune_deleted: counter_vec(
                "oci_prune_deleted_total",
                "OCI artifact rows deleted by the reachability prune (untagged manifests, unreferenced blobs).",
                &["repo"],
            ),
            oci_upload_dir: parking_lot::RwLock::new(String::new()),
            oci_max_manifest_bytes: AtomicI64::new(0),
            oci_max_blob_bytes: AtomicI64::new(0),
            oci_tokens: OciTokenCache::new(),
            scanner: parking_lot::RwLock::new(None),
            scan_queue: JobQueue::new(1024),
            resolver: parking_lot::RwLock::new(None),
            resolve_queue: JobQueue::new(1024),
            on_approval: parking_lot::RwLock::new(None),
        });

        if let Some(registry) = registry {
            for c in [
                Box::new(m.approval_blocked.clone()) as Box<dyn prometheus::core::Collector>,
                Box::new(m.approval_auto_approved.clone()),
                Box::new(m.deny_blocked.clone()),
                Box::new(m.ttl_expired.clone()),
                Box::new(m.vuln_blocked.clone()),
                Box::new(m.vuln_scans.clone()),
                Box::new(m.license_blocked.clone()),
                Box::new(m.license_resolves.clone()),
                Box::new(m.ip_blocked.clone()),
                Box::new(m.oci_prune_deleted.clone()),
                Box::new(m.scan_ratio_served.clone()),
                Box::new(m.scan_ratio_cost.clone()),
            ] {
                if let Err(err) = registry.register(c) {
                    tracing::error!(err = %err, "manager metric registration failed");
                }
            }
            // Computed on scrape from the sessions table, so the value is
            // correct across replicas and restarts (an in-memory counter would
            // drift).
            let sessions = OciUploadSessionsGauge {
                store: Arc::clone(&store),
                desc: prometheus::core::Desc::new(
                    "forklift_oci_upload_sessions_active".into(),
                    "OCI blob push sessions currently in flight (opened, not yet finalized or expired)."
                        .into(),
                    vec![],
                    HashMap::new(),
                )
                .expect("valid desc"),
                last: Arc::new(AtomicI64::new(-1)),
            };
            if let Err(err) = registry.register(Box::new(sessions)) {
                tracing::error!(err = %err, "manager metric registration failed");
            }
        }

        // Scan and resolve freshly cached proxy artifacts regardless of any
        // per-repository policy: collection is gated only by whether a scanner
        // is configured, so vulnerability/license data is populated even when no
        // policy enforces it. Hosted uploads trigger this from their PUT
        // handlers. The hook holds a weak reference so the engine does not keep
        // the manager alive.
        let weak = Arc::downgrade(&m);
        engine.set_on_store(Some(Arc::new(move |repo: Repository, path: String| {
            if let Some(m) = weak.upgrade() {
                m.scan_stored(&repo, &path);
                m.resolve_stored(&repo, &path);
            }
        })));
        m
    }

    /// Makes ecosystem-native publishers use the same validation and atomic
    /// publication service as the browser/API upload route.
    pub fn set_uploader(self: &Arc<Self>, uploader: Option<Arc<Uploader>>) {
        if let Some(uploader) = &uploader {
            uploader.set_scan_enabled(self.scanner.read().is_some());
            uploader.set_publish_hook(Arc::clone(self));
        }
        *self.uploader.write() = uploader;
    }

    /// Registers a callback invoked when a package is newly quarantined pending
    /// approval (one call per (repo, package) per suppression window).
    /// `receivers` is the repository's selected receiver names. The callback
    /// must not block the serving path.
    pub fn set_approval_notifier(&self, f: Option<OnApprovalFn>) {
        *self.on_approval.write() = f;
    }

    /// Enables the vulnerability gate and background scanning. When unset, the
    /// gate is a no-op (the feature is disabled).
    pub fn set_vuln_scanner(&self, s: Option<Arc<dyn vuln::Scanner>>) {
        let enabled = s.is_some();
        *self.scanner.write() = s;
        if let Some(uploader) = self.uploader.read().as_ref() {
            uploader.set_scan_enabled(enabled);
        }
    }

    /// Enables the license gate and background resolution. When unset, the gate
    /// is a no-op (the feature is disabled).
    pub fn set_license_resolver(&self, r: Option<Arc<dyn license::Resolver>>) {
        *self.resolver.write() = r;
    }

    /// Pins the externally-visible base URL (`FORKLIFT_EXTERNAL_URL`) instead of
    /// deriving it from request Host/X-Forwarded-* headers.
    pub fn set_external_url(&self, u: &str) {
        *self.external_url.write() = u.trim_end_matches('/').to_string();
    }

    /// Sets the directory holding in-flight OCI blob push session bytes.
    pub fn set_oci_upload_dir(&self, dir: &str) {
        *self.oci_upload_dir.write() = dir.to_string();
    }

    /// Caps the size of an accepted OCI manifest.
    pub fn set_oci_max_manifest_bytes(&self, n: i64) {
        self.oci_max_manifest_bytes.store(n, Ordering::Relaxed);
    }

    /// Caps the size of an accepted OCI blob.
    pub fn set_oci_max_blob_bytes(&self, n: i64) {
        self.oci_max_blob_bytes.store(n, Ordering::Relaxed);
    }

    /// Exposes the underlying engine (for the background sweeper in main).
    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    /// Mounts all format endpoints onto `router`. Each format lives under its
    /// own prefix with the repository name as the first path segment:
    ///
    /// ```text
    /// /maven/{repo}/<maven coordinate path>
    /// ```
    pub fn register(self: &Arc<Self>, router: Router) -> Router {
        use super::{cargo, gomod, maven, npm, oci, pypi, raw};

        macro_rules! format_route {
            ($handler:path) => {{
                let mgr = Arc::clone(self);
                axum::routing::any(move |req: Request| {
                    let mgr = Arc::clone(&mgr);
                    async move { Manager::wrap(mgr, req, |m, r| Box::pin($handler(m, r))).await }
                })
            }};
        }

        let oci_base = {
            let mgr = Arc::clone(self);
            axum::routing::any(move |req: Request| {
                let mgr = Arc::clone(&mgr);
                async move { oci::handle_oci_base(mgr, req).await }
            })
        };

        // A `{*rest}` wildcard does not match an empty tail, so `/maven/{repo}/`
        // needs its own route: without it the request would fall through to
        // the SPA fallback and answer with the console's index.html. The
        // handlers reject the empty path with 400, as before.
        router
            .route("/maven/{repo}/", format_route!(maven::handle_maven))
            .route("/maven/{repo}/{*rest}", format_route!(maven::handle_maven))
            .route("/npm/{repo}/", format_route!(npm::handle_npm))
            .route("/npm/{repo}/{*rest}", format_route!(npm::handle_npm))
            .route("/cargo/{repo}/", format_route!(cargo::handle_cargo))
            .route("/cargo/{repo}/{*rest}", format_route!(cargo::handle_cargo))
            .route("/go/{repo}/", format_route!(gomod::handle_go))
            .route("/go/{repo}/{*rest}", format_route!(gomod::handle_go))
            // The root routes accept twine uploads, which POST to the
            // repository root with or without a trailing slash.
            .route("/pypi/{repo}", format_route!(pypi::handle_pypi))
            .route("/pypi/{repo}/", format_route!(pypi::handle_pypi))
            .route("/pypi/{repo}/{*rest}", format_route!(pypi::handle_pypi))
            .route("/raw/{repo}/", format_route!(raw::handle_raw))
            .route("/raw/{repo}/{*rest}", format_route!(raw::handle_raw))
            // OCI distribution API. The docker client always addresses /v2 at
            // the host root (the path comes from the image reference), so the
            // Forklift repository name is the first segment after /v2 rather
            // than a format prefix. The base endpoint is the client's auth probe
            // and resolves no repository, so it skips the group/audit wrappers.
            .route("/v2", oci_base.clone())
            .route("/v2/", oci_base)
            .route("/v2/{repo}/{*rest}", format_route!(oci::handle_oci))
    }

    /// Applies the shared format-handler middleware: group fan-out innermost,
    /// audit logging outermost (so a group request is audited once, under the
    /// group's own name with the final status).
    pub(crate) async fn wrap(m: Arc<Manager>, req: Request, h: HandlerFn) -> Response {
        Manager::audited(m, req, h).await
    }

    /// Wraps a format handler so every repository request — including denied and
    /// not-found ones — lands in the audit log with its final status.
    pub(crate) async fn audited(m: Arc<Manager>, req: Request, h: HandlerFn) -> Response {
        let Some(rec) = m.rec.clone() else {
            return super::group::grouped(m, req, h).await;
        };
        let (parts, body) = req.into_parts();
        let (repo, path) = route_params(parts.uri.path());
        let username = username_from_context(&parts);
        let method = parts.method.clone();
        let client_ip = audit::client_ip_parts(&parts);
        let user_agent = header_str(&parts.headers, "User-Agent").to_string();
        let resp = super::group::grouped(m, Request::from_parts(parts, body), h).await;
        rec.record(audit::Event {
            repo,
            action: event_for_method(&method).to_string(),
            path,
            username,
            method: method.to_string(),
            status: resp.status().as_u16() as i64,
            client_ip,
            user_agent,
            ..Default::default()
        });
        resp
    }

    /// Enforces RBAC for a repository request. `action` is read, write or
    /// delete. It returns the response to send when access is denied. Requests
    /// routed through a group repository were already authorized against the
    /// group, so member-level checks are skipped.
    ///
    /// `public` is the repository's `config.public` flag: a public repository
    /// serves reads to anyone — anonymous callers and authenticated principals
    /// without an explicit permission alike (Harbor public-project semantics).
    /// The global anonymous-read switch keeps its instance-wide meaning on top.
    // Boxing it would push an extra indirection through every per-format handler for no benefit
    // on a path that allocates a response body anyway.
    #[allow(clippy::result_large_err)]
    pub(crate) fn authorize(
        &self,
        parts: &Parts,
        repo_name: &str,
        action: &str,
        public: bool,
    ) -> Result<(), Response> {
        let Some(authz) = &self.authz else {
            return Ok(());
        };
        if super::group::via_group(parts) {
            return Ok(());
        }
        if action == auth::ACTION_READ && public {
            return Ok(());
        }
        let Some(p) = auth::from_request_parts(parts) else {
            if action == auth::ACTION_READ && authz.anonymous_read() {
                return Ok(());
            }
            return Err(auth::unauthorized_basic());
        };
        if !p.can(repo_name, action) {
            return Err(http_error(StatusCode::FORBIDDEN, "forbidden"));
        }
        Ok(())
    }

    /// Enforces the repository's source-IP access control list. It returns the
    /// 403 to send when the ACL is enabled and the client IP is not in the allow
    /// list. Requests routed through a group were already checked against the
    /// group's ACL when it was resolved, so member-level checks are skipped,
    /// mirroring [`Manager::authorize`] (Nexus semantics: the group governs
    /// access through it).
    // Boxing it would push an extra indirection through every per-format handler for no benefit
    // on a path that allocates a response body anyway.
    #[allow(clippy::result_large_err)]
    pub(crate) fn ip_allowed(
        &self,
        parts: &Parts,
        repo_name: &str,
        cfg: &Config,
    ) -> Result<(), Response> {
        if !cfg.ip_acl.enabled {
            return Ok(());
        }
        if super::group::via_group(parts) {
            return Ok(());
        }
        if cfg.ip_acl.allowed(&audit::client_ip_parts(parts)) {
            return Ok(());
        }
        self.ip_blocked.with_label_values(&[repo_name]).inc();
        Err(http_error(
            StatusCode::FORBIDDEN,
            "forbidden: source IP not allowed",
        ))
    }

    /// Loads the repository named in the `{repo}` URL param, verifies its
    /// format, parses its config, and extracts the repo-relative wildcard path,
    /// which must be non-empty.
    // Boxing it would push an extra indirection through every per-format handler for no benefit
    // on a path that allocates a response body anyway.
    #[allow(clippy::result_large_err)]
    pub(crate) async fn resolve(&self, parts: &Parts, format: &str) -> Result<Resolved, Response> {
        let res = self.resolve_repo(parts, format).await?;
        if res.path.is_empty() {
            return Err(http_error(StatusCode::BAD_REQUEST, "invalid path"));
        }
        Ok(res)
    }

    /// [`Manager::resolve`] without the non-empty path requirement, for formats
    /// whose protocol addresses the repository root (PyPI uploads).
    // Boxing it would push an extra indirection through every per-format handler for no benefit
    // on a path that allocates a response body anyway.
    #[allow(clippy::result_large_err)]
    pub(crate) async fn resolve_repo(
        &self,
        parts: &Parts,
        format: &str,
    ) -> Result<Resolved, Response> {
        let (name, path) = route_params(parts.uri.path());
        let repo = match self.store.get_repository_by_name(&name).await {
            Ok(repo) => repo,
            Err(e) if e.is_not_found() => {
                return Err(http_error(StatusCode::NOT_FOUND, "repository not found"));
            }
            Err(_) => {
                return Err(http_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "metadata error",
                ));
            }
        };
        if repo.format != format {
            return Err(http_error(
                StatusCode::NOT_FOUND,
                "repository format mismatch",
            ));
        }
        if repo.disabled {
            return Err(http_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "repository disabled",
            ));
        }
        let Ok(cfg) = repoconfig::parse(&repo.config_json) else {
            return Err(http_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "invalid repository config",
            ));
        };
        self.ip_allowed(parts, &repo.name, &cfg)?;
        if path.contains("..") {
            return Err(http_error(StatusCode::BAD_REQUEST, "invalid path"));
        }
        Ok(Resolved { repo, cfg, path })
    }
}

/// Bundles a repository with its parsed config and the repo-relative request
/// path.
#[derive(Clone)]
pub(crate) struct Resolved {
    pub(crate) repo: Repository,
    pub(crate) cfg: Config,
    pub(crate) path: String,
}

/// Maps an HTTP method to an RBAC action.
pub(crate) fn action_for_method(method: &Method) -> &'static str {
    match *method {
        Method::PUT | Method::POST | Method::PATCH => auth::ACTION_WRITE,
        Method::DELETE => auth::ACTION_DELETE,
        _ => auth::ACTION_READ,
    }
}

/// Maps an HTTP method to an audit event type.
pub(crate) fn event_for_method(method: &Method) -> &'static str {
    match *method {
        Method::PUT | Method::POST => meta::EVENT_UPLOAD,
        Method::DELETE => meta::EVENT_DELETE,
        _ => meta::EVENT_DOWNLOAD,
    }
}

/// Builds an upstream URL from a base and a repo-relative path.
pub(crate) fn join_upstream(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// Splits a mounted format path into the repository name and the repo-relative
/// remainder.
///
/// Every route here has the same `/{prefix}/{repo}/{rest…}` shape, so the split is done on the
/// raw URI path, preserving that behaviour exactly.
pub(crate) fn route_params(path: &str) -> (String, String) {
    let after_prefix = match path.trim_start_matches('/').split_once('/') {
        Some((_, rest)) => rest,
        None => "",
    };
    match after_prefix.split_once('/') {
        Some((repo, rest)) => (repo.to_string(), rest.trim_start_matches('/').to_string()),
        None => (after_prefix.to_string(), String::new()),
    }
}

fn counter_vec(name: &str, help: &str, labels: &[&str]) -> CounterVec {
    CounterVec::new(Opts::new(name, help).namespace("forklift"), labels).expect("valid counter")
}

/// A scrape-time gauge over the OCI upload-session table.
///
/// Rust's `Collector::collect` is synchronous and the store API is async, so each scrape
/// publishes the last observed count and kicks off the refresh for the next one. The value
/// therefore trails by at most one scrape interval; it is a diagnostic gauge, and the
/// alternative (blocking a runtime worker inside a scrape) is worse.
struct OciUploadSessionsGauge {
    store: Arc<Store>,
    desc: prometheus::core::Desc,
    last: Arc<AtomicI64>,
}

impl prometheus::core::Collector for OciUploadSessionsGauge {
    fn desc(&self) -> Vec<&prometheus::core::Desc> {
        vec![&self.desc]
    }

    fn collect(&self) -> Vec<prometheus::proto::MetricFamily> {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let store = Arc::clone(&self.store);
            let last = Arc::clone(&self.last);
            handle.spawn(async move {
                let n = store.count_oci_upload_sessions().await.unwrap_or(-1);
                last.store(n, Ordering::Relaxed);
            });
        }
        let gauge = prometheus::Gauge::with_opts(
            Opts::new(
                "oci_upload_sessions_active",
                "OCI blob push sessions currently in flight (opened, not yet finalized or expired).",
            )
            .namespace("forklift"),
        )
        .expect("valid gauge");
        gauge.set(self.last.load(Ordering::Relaxed) as f64);
        gauge.collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    mod audit {
        use http::{Method, StatusCode};

        use crate::meta;
        use crate::repoconfig;

        use crate::repo::router::event_for_method;
        use crate::testing::repo::{call, mk_repo, mux, new_test_manager_with_recorder};

        #[tokio::test]
        async fn audited_traffic_is_recorded() {
            let (tm, rec) = new_test_manager_with_recorder().await;
            mk_repo(
                &tm.store,
                "mvn-local",
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
            let app = mux(&tm.manager);
            let path = "/maven/mvn-local/com/acme/app/1.0/app-1.0.jar";

            let resp = call(&app, Method::PUT, path, "JAR").await;
            assert_eq!(resp.status, StatusCode::CREATED, "put");
            let resp = call(&app, Method::GET, path, "").await;
            assert_eq!(resp.status, StatusCode::OK, "get");
            // A miss must be audited too.
            let resp = call(&app, Method::GET, "/maven/mvn-local/missing.jar", "").await;
            assert_eq!(resp.status, StatusCode::NOT_FOUND, "missing get");

            rec.close().await; // flush buffered events

            let logs = tm
                .store
                .list_audit_logs("mvn-local", "", 10, 0)
                .await
                .expect("list");
            assert_eq!(logs.len(), 3, "{logs:?}");
            // Newest first: 404 download, 200 download, 201 upload.
            assert_eq!(logs[0].event, meta::EVENT_DOWNLOAD, "{:?}", logs[0]);
            assert_eq!(logs[0].status, 404, "{:?}", logs[0]);
            assert_eq!(logs[0].path, "missing.jar", "{:?}", logs[0]);
            assert_eq!(logs[1].event, meta::EVENT_DOWNLOAD, "{:?}", logs[1]);
            assert_eq!(logs[1].status, 200, "{:?}", logs[1]);
            assert_eq!(logs[2].event, meta::EVENT_UPLOAD, "{:?}", logs[2]);
            assert_eq!(logs[2].status, 201, "{:?}", logs[2]);
            assert_eq!(
                logs[2].path, "com/acme/app/1.0/app-1.0.jar",
                "{:?}",
                logs[2]
            );
            assert_eq!(logs[2].method, "PUT", "{:?}", logs[2]);
        }

        #[test]
        fn event_for_method_maps_every_verb() {
            for (method, want) in [
                (Method::GET, meta::EVENT_DOWNLOAD),
                (Method::HEAD, meta::EVENT_DOWNLOAD),
                (Method::PUT, meta::EVENT_UPLOAD),
                (Method::POST, meta::EVENT_UPLOAD),
                (Method::DELETE, meta::EVENT_DELETE),
            ] {
                assert_eq!(
                    event_for_method(&method),
                    want,
                    "event_for_method({method})"
                );
            }
        }
    }
}
