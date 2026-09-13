//! `/metrics` router and the build info metric.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::info::Info;
use prometheus_client::registry::Registry;

use crate::config::BuildInfo;

const CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// Register `ec2_metadata_build_info` on the registry.
pub fn register_build_info(registry: &mut Registry, build: BuildInfo) {
    let info = Info::new(vec![
        ("version", build.version),
        ("commit", build.commit),
        ("rust_version", build.rustc),
    ]);
    registry.register(
        "ec2_metadata_build",
        "Build information. Value is always 1; labels carry the version, git commit, and Rust compiler version",
        info,
    );
}

/// Router serving the registry on `/metrics`.
pub fn router(registry: Arc<Registry>) -> Router {
    Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(registry)
}

async fn metrics_handler(State(registry): State<Arc<Registry>>) -> impl IntoResponse {
    let mut body = String::new();
    match encode(&mut body, &registry) {
        Ok(()) => (StatusCode::OK, [(header::CONTENT_TYPE, CONTENT_TYPE)], body).into_response(),
        Err(err) => {
            tracing::error!(error = %err, "failed to encode metrics");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn serves_build_info() {
        let mut registry = Registry::default();
        register_build_info(
            &mut registry,
            BuildInfo {
                version: "1.2.3",
                commit: "abc1234",
                date: "2026-01-01",
                rustc: "1.98.0",
            },
        );
        let resp = router(Arc::new(registry))
            .oneshot(
                Request::get("/metrics")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .map(|v| v.to_str().expect("ascii")),
            Some(CONTENT_TYPE)
        );
        let body = resp.into_body().collect().await.expect("body").to_bytes();
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(
            text.contains(r#"ec2_metadata_build_info{version="1.2.3",commit="abc1234",rust_version="1.98.0"} 1"#),
            "{text}"
        );
        assert!(text.ends_with("# EOF\n"), "{text}");
    }
}
