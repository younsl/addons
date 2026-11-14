//! Serves `/healthz`, `/readyz`, and `/metrics` on one port.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use prometheus_client::encoding::text::encode;
use prometheus_client::registry::Registry;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[derive(Clone, Default)]
pub struct Health {
    ready: Arc<AtomicBool>,
}

impl Health {
    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::SeqCst);
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }
}

#[derive(Clone)]
struct AppState {
    health: Health,
    registry: Arc<Registry>,
}

async fn healthz() -> StatusCode {
    StatusCode::OK
}

async fn readyz(State(state): State<AppState>) -> StatusCode {
    if state.health.is_ready() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let mut body = String::new();
    if encode(&mut body, &state.registry).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    (
        [(
            header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

pub fn router(health: Health, registry: Arc<Registry>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .with_state(AppState { health, registry })
}

pub async fn serve(
    port: u16,
    health: Health,
    registry: Arc<Registry>,
    shutdown: CancellationToken,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    info!(port, "HTTP server listening");
    axum::serve(listener, router(health, registry))
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot::Build;
    use crate::telemetry::metrics::Reconciles;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    async fn get_path(app: &Router, path: &str) -> (StatusCode, String) {
        let response = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn readiness_follows_the_health_state() {
        let health = Health::default();
        let app = router(health.clone(), Arc::new(Registry::default()));
        assert_eq!(get_path(&app, "/healthz").await.0, StatusCode::OK);
        assert_eq!(
            get_path(&app, "/readyz").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        health.set_ready(true);
        assert_eq!(get_path(&app, "/readyz").await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn metrics_expose_the_build_and_the_reconciles() {
        let mut registry = Registry::default();
        let reconciles = Reconciles::new(&mut registry, &Build::TEST);
        reconciles.record(true);
        reconciles.record(false);

        let app = router(Health::default(), Arc::new(registry));
        let (status, body) = get_path(&app, "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        for needle in [
            "node_configuration_build_info{version=\"0.0.0\",revision=\"test\",rust_version=\"0.0.0\"} 1",
            "node_configuration_reconcile_total{result=\"success\"} 1",
            "node_configuration_reconcile_total{result=\"error\"} 1",
        ] {
            assert!(body.contains(needle), "missing {needle:?} in:\n{body}");
        }
    }

    #[tokio::test]
    async fn serve_stops_on_shutdown() {
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        serve(
            0,
            Health::default(),
            Arc::new(Registry::default()),
            shutdown,
        )
        .await
        .unwrap();
    }
}
