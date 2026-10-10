//! HTTP routes. Handlers never touch the file system: they read the last
//! report and wake the reconcile loop.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use prometheus_client::registry::Registry;

use crate::observability::metrics;
use crate::reconciler::Shared;

const OPENMETRICS: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

#[derive(Clone)]
pub struct AppState {
    pub shared: Arc<Shared>,
    pub registry: Arc<Registry>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/status", get(status))
        .route("/reconcile", post(reconcile))
        .route("/metrics", get(render_metrics))
        .with_state(state)
}

async fn healthz() -> StatusCode {
    StatusCode::OK
}

async fn readyz(State(state): State<AppState>) -> StatusCode {
    if state.shared.is_ready() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn status(State(state): State<AppState>) -> Response {
    state.shared.report().map_or_else(
        || {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "no reconcile has finished yet",
            )
                .into_response()
        },
        |report| Json(report).into_response(),
    )
}

async fn reconcile(State(state): State<AppState>) -> StatusCode {
    state.shared.request_reconcile();
    StatusCode::ACCEPTED
}

async fn render_metrics(State(state): State<AppState>) -> Response {
    metrics::render(&state.registry).map_or_else(
        |_| StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        |body| ([(header::CONTENT_TYPE, OPENMETRICS)], body).into_response(),
    )
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Method, Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;
    use crate::reconciler::{Settings, run_once};

    fn state() -> AppState {
        AppState {
            shared: Arc::new(Shared::default()),
            registry: Arc::new(Registry::default()),
        }
    }

    async fn call(state: AppState, method: Method, uri: &str) -> (StatusCode, String) {
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        (status, String::from_utf8(body.to_vec()).expect("utf8"))
    }

    #[tokio::test]
    async fn before_first_reconcile() {
        let s = state();
        assert_eq!(
            call(s.clone(), Method::GET, "/healthz").await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(s.clone(), Method::GET, "/readyz").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            call(s.clone(), Method::GET, "/status").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let (code, body) = call(s, Method::GET, "/metrics").await;
        assert_eq!(code, StatusCode::OK);
        assert!(body.ends_with("# EOF\n"), "{body}");
    }

    #[tokio::test]
    async fn status_after_reconcile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_file = dir.path().join("config.toml");
        std::fs::write(&config_file, "source_root = \"~\"\nbackup_root = \"~/b\"\n").expect("spec");
        let settings = Settings {
            config_file,
            home: dir.path().to_path_buf(),
            dry_run: true,
        };
        let s = state();
        s.shared
            .publish(run_once(&settings, jiff::Timestamp::now()));

        assert_eq!(
            call(s.clone(), Method::GET, "/readyz").await.0,
            StatusCode::OK
        );
        let (code, body) = call(s, Method::GET, "/status").await;
        assert_eq!(code, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(json["dry_run"], true);
        assert_eq!(json["entries"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn reconcile_is_accepted_and_get_is_rejected() {
        let s = state();
        assert_eq!(
            call(s.clone(), Method::POST, "/reconcile").await.0,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            call(s, Method::GET, "/reconcile").await.0,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }
}
