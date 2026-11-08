//! Logging, signal handling, and the `/healthz`, `/readyz`, `/metrics`
//! server every role runs.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use prometheus_client::registry::Registry;
use tokio::sync::watch;

pub type Subscriber = Box<dyn tracing::Subscriber + Send + Sync>;

/// A level and format subscriber. `RUST_LOG` overrides the level.
pub fn subscriber(level: &str, format: &str) -> Subscriber {
    let level = match level.to_ascii_lowercase().as_str() {
        "trace" => "trace",
        "debug" => "debug",
        "warn" => "warn",
        "error" => "error",
        _ => "info",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);
    if format.eq_ignore_ascii_case("text") {
        Box::new(builder.finish())
    } else {
        Box::new(builder.json().flatten_event(true).finish())
    }
}

/// Flips to `true` once on SIGTERM or Ctrl-C.
pub fn shutdown_channel() -> watch::Receiver<bool> {
    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
            tokio::select! {
                _ = ctrl_c => {}
                () = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending().await } } => {}
            }
        }
        #[cfg(not(unix))]
        let _ = ctrl_c.await;
        tracing::info!("shutdown signal received");
        let _ = tx.send(true);
    });
    rx
}

/// Resolves once `rx` turns `true`.
pub async fn stopped(mut rx: watch::Receiver<bool>) {
    let _ = rx.wait_for(|stop| *stop).await;
}

#[derive(Clone)]
pub struct Health {
    pub ready: Arc<AtomicBool>,
    pub registry: Arc<Registry>,
}

impl Health {
    pub fn new(registry: Registry) -> Self {
        Self {
            ready: Arc::new(AtomicBool::new(false)),
            registry: Arc::new(registry),
        }
    }

    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::Relaxed);
    }
}

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(h): State<Health>) -> impl IntoResponse {
    if h.ready.load(Ordering::Relaxed) {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

async fn metrics(State(h): State<Health>) -> impl IntoResponse {
    let mut body = String::new();
    if prometheus_client::encoding::text::encode(&mut body, &h.registry).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "text/plain")],
            String::new(),
        );
    }
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        body,
    )
}

pub fn router(health: Health) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .with_state(health)
}

/// Serves `app` on `addr` until `shutdown` fires.
pub async fn serve(
    addr: SocketAddr,
    app: Router,
    shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(stopped(shutdown))
        .await
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use prometheus_client::metrics::counter::Counter;
    use tower::ServiceExt as _;

    use super::*;

    async fn get_status(app: &Router, path: &str) -> (StatusCode, String) {
        let res = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn health_endpoints() {
        let mut reg = Registry::default();
        let c = Counter::<u64>::default();
        reg.register("probe", "probe counter", c.clone());
        c.inc();
        let h = Health::new(reg);
        let app = router(h.clone());
        assert_eq!(get_status(&app, "/healthz").await.0, StatusCode::OK);
        assert_eq!(
            get_status(&app, "/readyz").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        h.set_ready(true);
        assert_eq!(get_status(&app, "/readyz").await.0, StatusCode::OK);
        let (status, body) = get_status(&app, "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("probe_total 1"), "{body}");
    }

    #[tokio::test]
    async fn serve_stops_on_shutdown() {
        let (tx, rx) = watch::channel(false);
        let task = tokio::spawn(serve(
            "127.0.0.1:0".parse().unwrap(),
            router(Health::new(Registry::default())),
            rx.clone(),
        ));
        tx.send(true).unwrap();
        task.await.unwrap().unwrap();
        stopped(rx).await;
    }

    #[test]
    fn subscribers_build() {
        for (level, format) in [
            ("debug", "text"),
            ("x", "json"),
            ("trace", "JSON"),
            ("warn", "text"),
            ("error", "json"),
        ] {
            tracing::subscriber::with_default(subscriber(level, format), || {
                tracing::info!("probe");
            });
        }
    }
}
