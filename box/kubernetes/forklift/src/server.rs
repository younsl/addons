//! Wires the axum router, middleware, health/metrics endpoints and graceful
//! shutdown. Package protocol handlers and the admin API are mounted by the
//! caller (`Server::merge`) before [`Server::run`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::response::Response;
use axum::routing::get;
use http::{HeaderValue, StatusCode, header};
use prometheus::{
    Histogram, HistogramOpts, HistogramVec, IntCounterVec, Opts, Registry, TextEncoder,
};
use tokio_util::sync::CancellationToken;

pub mod health;
pub mod logging;
pub mod middleware;
pub(crate) mod pprof_routes;
mod serve;

pub use logging::init_logging;

const READ_HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether this instance should receive traffic. In HA mode it is toggled by
/// leader election; otherwise it is set true at startup. Shared with the
/// cluster module, so it is a cheap clonable handle around one atomic.
#[derive(Debug, Clone, Default)]
pub struct Ready(Arc<AtomicBool>);

impl Ready {
    /// A flag with the given initial value.
    pub fn new(ready: bool) -> Ready {
        Ready(Arc::new(AtomicBool::new(ready)))
    }

    /// Reads the flag.
    pub fn get(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Sets the flag.
    pub fn set(&self, ready: bool) {
        self.0.store(ready, Ordering::SeqCst);
    }
}

pub fn http_error(status: StatusCode, msg: &str) -> Response {
    let mut resp = Response::new(Body::from(format!("{msg}\n")));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

/// Installs rustls' ring provider as the process-wide default. Idempotent: a
/// second call (another test binary, another entry point) is a no-op, which is
/// why the result is discarded.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The HTTP metrics recorded by the logging middleware, shared with it as
/// middleware state.
#[derive(Debug)]
pub(crate) struct Metrics {
    pub(crate) req_duration: HistogramVec,
    pub(crate) req_total: IntCounterVec,
    /// Counts readiness refusals by reason. A pod removed from the Service
    /// looks identical from outside whether it stepped down as leader or could
    /// not reach the database in time, and that distinction is the first
    /// question during an outage.
    pub(crate) ready_fail: IntCounterVec,
    /// How long the probe itself took. The probe has a one-second default
    /// timeout, so its own latency is the early warning that the database is
    /// congested, before the failures start.
    pub(crate) ready_duration: Histogram,
}

/// HTTP server state.
pub struct Server {
    cfg: Arc<crate::config::Config>,
    store: Arc<crate::meta::Store>,
    router: Router,
    ready: Ready,
    metrics: Arc<Metrics>,
}

impl Server {
    /// Creates a Server with health, metrics and middleware configured. Mount
    /// additional routes via [`Server::merge`] before calling [`Server::run`].
    pub fn new(
        cfg: Arc<crate::config::Config>,
        store: Arc<crate::meta::Store>,
        registry: &Registry,
    ) -> Server {
        let metrics = Arc::new(Metrics {
            req_duration: HistogramVec::new(
                HistogramOpts::new("http_request_duration_seconds", "HTTP request latency.")
                    .namespace("forklift")
                    .buckets(prometheus::DEFAULT_BUCKETS.to_vec()),
                &["method", "route", "status"],
            )
            .expect("http_request_duration_seconds"),
            req_total: IntCounterVec::new(
                Opts::new("http_requests_total", "Total HTTP requests.").namespace("forklift"),
                &["method", "route", "status"],
            )
            .expect("http_requests_total"),
            ready_fail: IntCounterVec::new(
                Opts::new(
                    "readyz_failures_total",
                    "Readiness probe refusals by reason (not_leader, database).",
                )
                .namespace("forklift"),
                &["reason"],
            )
            .expect("readyz_failures_total"),
            ready_duration: Histogram::with_opts(
                HistogramOpts::new(
                    "readyz_duration_seconds",
                    "Readiness probe handler latency, including the database check.",
                )
                .namespace("forklift")
                .buckets(vec![0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5]),
            )
            .expect("readyz_duration_seconds"),
        });
        registry
            .register(Box::new(metrics.req_duration.clone()))
            .expect("register forklift_http_request_duration_seconds");
        registry
            .register(Box::new(metrics.req_total.clone()))
            .expect("register forklift_http_requests_total");
        registry
            .register(Box::new(metrics.ready_fail.clone()))
            .expect("register forklift_readyz_failures_total");
        registry
            .register(Box::new(metrics.ready_duration.clone()))
            .expect("register forklift_readyz_duration_seconds");

        let ready = Ready::default();
        let health = Arc::new(health::HealthState {
            ready: ready.clone(),
            store: Arc::clone(&store),
            metrics: Arc::clone(&metrics),
        });
        let router = Router::new()
            .route("/healthz", get(health::handle_healthz))
            .route("/readyz", get(health::handle_readyz))
            .with_state(health);

        Server {
            cfg,
            store,
            router,
            ready,
            metrics,
        }
    }

    /// The metadata store the health probe checks, for callers that mount
    /// routes needing it.
    pub fn store(&self) -> &Arc<crate::meta::Store> {
        &self.store
    }

    /// The readiness flag, shared with leader election.
    pub fn ready_flag(&self) -> Ready {
        self.ready.clone()
    }

    /// Mutable access to the bare router so callers can mount routes before
    /// [`Server::run`]. axum's `Router` combinators consume `self`, so the
    /// usual shape is `*s.router_mut() = std::mem::take(s.router_mut()).route(...)`;
    /// [`Server::merge`] wraps that for the common case.
    pub fn router_mut(&mut self) -> &mut Router {
        &mut self.router
    }

    pub fn merge(&mut self, other: Router) {
        let router = std::mem::take(&mut self.router);
        self.router = router.merge(other);
    }

    /// The router with middleware applied, ready to serve.
    ///
    /// So the middleware is layered here, at the end, rather than in [`Server::new`], and every
    /// route mounted through [`Server::merge`] is still covered. The recoverer stays outermost
    /// and the request log inside it, exactly as the chi stack was ordered.
    pub fn router(&self) -> Router {
        self.router
            .clone()
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&self.metrics),
                middleware::log_requests,
            ))
            .layer(middleware::recoverer())
    }

    /// Toggles readiness (used by leader election).
    pub fn set_ready(&self, ready: bool) {
        self.ready.set(ready);
    }

    /// Starts the main and metrics listeners and blocks until `cancel` fires,
    /// then shuts down gracefully.
    pub async fn run(self, cancel: CancellationToken, registry: Registry) -> anyhow::Result<()> {
        let main_router = self.router();
        let main_listener = listen(&self.cfg.http_addr).await?;
        let metrics_router = Router::new()
            .route("/metrics", get(handle_metrics))
            .with_state(Arc::new(registry));
        let metrics_listener = listen(&self.cfg.metrics_addr).await?;

        // pprof gets its own listener rather than a path on the metrics mux:
        // the metrics port is exposed cluster-wide through the Service, and a
        // profiling endpoint with no authentication does not belong there. The
        // loopback default is still reachable with kubectl port-forward, which
        // is the only way it is meant to be used.
        let profiler = if self.cfg.pprof_addr.is_empty() {
            None
        } else {
            Some((pprof_routes::routes(), listen(&self.cfg.pprof_addr).await?))
        };

        tracing::info!(addr = %self.cfg.http_addr, "http listening");
        let timeout = self.cfg.shutdown_timeout;
        let main = tokio::spawn(serve::serve(
            main_listener,
            main_router,
            cancel.clone(),
            timeout,
            true,
        ));

        tracing::info!(addr = %self.cfg.metrics_addr, "metrics listening");
        let metrics = tokio::spawn(serve::serve(
            metrics_listener,
            metrics_router,
            cancel.clone(),
            timeout,
            false,
        ));

        let profiler = profiler.map(|(router, listener)| {
            tracing::info!(addr = %self.cfg.pprof_addr, "pprof listening");
            tokio::spawn(serve::serve(
                listener,
                router,
                cancel.clone(),
                timeout,
                false,
            ))
        });

        cancel.cancelled().await;
        tracing::info!("shutting down");

        // Each listener bounds its own grace period with the configured
        // timeout, so joining them cannot outlast it either.
        let _ = metrics.await;
        if let Some(profiler) = profiler {
            let _ = profiler.await;
        }
        let _ = main.await;
        Ok(())
    }
}

/// Serves the Prometheus text exposition of `registry`.
async fn handle_metrics(
    axum::extract::State(registry): axum::extract::State<Arc<Registry>>,
) -> Response {
    match TextEncoder::new().encode_to_string(&registry.gather()) {
        Ok(body) => {
            let mut resp = Response::new(Body::from(body));
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(prometheus::TEXT_FORMAT),
            );
            resp
        }
        Err(e) => http_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn listen(addr: &str) -> anyhow::Result<tokio::net::TcpListener> {
    let normalized = normalize_addr(addr);
    let listener = tokio::net::TcpListener::bind(&normalized).await?;
    Ok(listener)
}

pub(crate) fn normalize_addr(addr: &str) -> String {
    if let Some(port) = addr.strip_prefix(':') {
        format!("0.0.0.0:{port}")
    } else {
        addr.to_string()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    mod endpoints {

        use axum::Router;
        use axum::body::Body;
        use axum::routing::get;
        use http::{Request, StatusCode};
        use std::time::Duration;

        use crate::server::*;
        use crate::testing::server::*;
        use tokio_util::sync::CancellationToken;
        use tower::ServiceExt;
        #[tokio::test]
        async fn healthz() {
            let (s, _reg, _dir) = new_test_server().await;
            let resp = get_request(&s, "/healthz").await;
            assert_eq!(resp.status(), StatusCode::OK, "healthz");
        }

        #[tokio::test]
        async fn readyz_reflects_leadership() {
            let (s, _reg, _dir) = new_test_server().await;

            let resp = get_request(&s, "/readyz").await;
            assert_eq!(
                resp.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "readyz before leadership"
            );

            s.set_ready(true);
            let resp = get_request(&s, "/readyz").await;
            assert_eq!(resp.status(), StatusCode::OK, "readyz after leadership");
        }

        #[tokio::test]
        async fn recoverer_and_metrics() {
            let (mut s, _reg, _dir) = new_test_server().await;
            async fn boom() -> axum::response::Response {
                panic!("kaboom")
            }
            s.merge(Router::new().route("/boom", get(boom)));
            let resp = get_request(&s, "/boom").await;
            assert_eq!(
                resp.status(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "panic route"
            );
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(&body[..], b"internal server error\n");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn run_shuts_down_on_cancel() {
            let (s, reg, _dir) = new_test_server().await;
            s.set_ready(true);
            let cancel = CancellationToken::new();

            let done = tokio::spawn({
                let cancel = cancel.clone();
                async move { s.run(cancel, reg).await }
            });

            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();

            let out = tokio::time::timeout(Duration::from_secs(5), done)
                .await
                .expect("Run did not shut down in time")
                .expect("run task panicked");
            out.expect("Run returned error");
        }

        /// The metrics the middleware records are labelled by matched route, method
        /// and status reason phrase; an unmatched path is one series, not one per URI.
        #[tokio::test]
        async fn request_metrics_label_route_and_status() {
            let (s, reg, _dir) = new_test_server().await;
            let _ = get_request(&s, "/healthz").await;
            let _ = get_request(&s, "/nope/1").await;
            let _ = get_request(&s, "/nope/2").await;

            let text = prometheus::TextEncoder::new()
                .encode_to_string(&reg.gather())
                .unwrap();
            assert!(
                text.contains(
                    r#"forklift_http_requests_total{method="GET",route="/healthz",status="OK"} 1"#
                ),
                "matched route series missing:\n{text}"
            );
            assert!(
                text.contains(
                    r#"forklift_http_requests_total{method="GET",route="unmatched",status="Not Found"} 2"#
                ),
                "unmatched series missing:\n{text}"
            );
        }

        /// Format routes keep their historical `route` label spelling.
        #[test]
        fn route_label_keeps_previous_spelling() {
            use crate::server::middleware::route_label;
            assert_eq!(route_label("/maven/{repo}/{*rest}"), "/maven/{repo}/*");
            assert_eq!(route_label("/maven/{repo}/"), "/maven/{repo}/*");
            assert_eq!(route_label("/pypi/{repo}"), "/pypi/{repo}");
            assert_eq!(route_label("/v2/{repo}/{*rest}"), "/v2/{repo}/*");
            assert_eq!(
                route_label("/api/v1/repositories/{name}"),
                "/api/v1/repositories/{name}"
            );
        }

        #[tokio::test]
        async fn http_error_matches_go() {
            let resp = http_error(StatusCode::NOT_FOUND, "no such thing");
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
            assert_eq!(
                resp.headers()
                    .get(http::header::CONTENT_TYPE)
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "text/plain; charset=utf-8"
            );
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(&body[..], b"no such thing\n");
        }

        #[test]
        fn normalize_addr_expands_wildcard() {
            assert_eq!(normalize_addr(":8080"), "0.0.0.0:8080");
            assert_eq!(normalize_addr("127.0.0.1:0"), "127.0.0.1:0");
        }

        /// Installing the crypto provider twice must not panic.
        #[test]
        fn install_crypto_provider_is_idempotent() {
            install_crypto_provider();
            install_crypto_provider();
        }

        #[tokio::test]
        async fn pprof_routes_answer() {
            let router = crate::server::pprof_routes::routes();
            for (path, want) in [
                ("/debug/pprof/", StatusCode::OK),
                ("/debug/pprof/cmdline", StatusCode::OK),
                ("/debug/pprof/symbol", StatusCode::NOT_IMPLEMENTED),
                ("/debug/pprof/trace", StatusCode::NOT_IMPLEMENTED),
            ] {
                let resp = router
                    .clone()
                    .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(resp.status(), want, "{path}");
            }
        }

        /// A CPU profile is a gzipped pprof protobuf, the format `go tool pprof` reads.
        #[tokio::test(flavor = "multi_thread")]
        async fn pprof_profile_returns_gzipped_protobuf() {
            let resp = crate::server::pprof_routes::routes()
                .oneshot(
                    Request::builder()
                        .uri("/debug/pprof/profile?seconds=1")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(
                resp.headers()
                    .get(http::header::CONTENT_TYPE)
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "application/octet-stream"
            );
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            assert!(
                body.len() > 2 && body[0] == 0x1f && body[1] == 0x8b,
                "not gzip"
            );
        }
    }

    mod readyz {
        //!
        //! A pod dropped from the Service looks the same from outside whichever way it
        //! happened, so the probe has to say which: leadership or the database. Those
        //! two counters are the first thing to look at when an instance goes
        //! unreachable, and the latency histogram is what shows the database getting
        //! slow before the failures start.

        use http::StatusCode;

        use crate::testing::server::{get_request, new_test_server};

        #[tokio::test]
        async fn readyz_records_outcome() {
            let (s, reg, _dir) = new_test_server().await;

            // Standby: refused as not_leader, and that is what is counted.
            let resp = get_request(&s, "/readyz").await;
            assert_eq!(
                resp.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "standby readyz"
            );
            assert_eq!(
                s.metrics
                    .ready_fail
                    .with_label_values(&["not_leader"])
                    .get(),
                1,
                "not_leader failures"
            );
            assert_eq!(
                s.metrics.ready_fail.with_label_values(&["database"]).get(),
                0,
                "database failures: leadership never reached the database check"
            );

            s.set_ready(true);
            let resp = get_request(&s, "/readyz").await;
            assert_eq!(resp.status(), StatusCode::OK, "leader readyz");

            // Both probes are timed, successes included: the histogram is the early
            // warning, so it must not only record failures.
            let families = reg.gather();
            let histogram = families
                .iter()
                .find(|f| f.name() == "forklift_readyz_duration_seconds")
                .expect("readyz duration histogram is not being collected");
            assert_eq!(
                histogram.get_metric()[0].get_histogram().get_sample_count(),
                2,
                "both probes must be timed"
            );
            assert_eq!(
                s.metrics
                    .ready_fail
                    .with_label_values(&["not_leader"])
                    .get(),
                1,
                "not_leader failures after a successful probe"
            );
        }
    }
}
