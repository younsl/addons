//! HTTP listeners for the probes and the metrics.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use axum::Router;
use axum::routing::get;
use tokio_util::sync::CancellationToken;

use super::metrics::Metrics;

/// The `/metrics` route.
pub fn metrics_router(metrics: Arc<Metrics>) -> Router {
    Router::new().route(
        "/metrics",
        get(move || {
            let metrics = metrics.clone();
            async move {
                (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/plain; version=0.0.4; charset=utf-8",
                    )],
                    metrics.render(),
                )
            }
        }),
    )
}

/// Serves `router` on `port` until `shutdown` fires.
pub async fn serve(router: Router, port: u16, shutdown: CancellationToken) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("listen on :{port}"))?;
    axum::serve(listener, router)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
        .context("serve")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn metrics_route_renders_registry() {
        let m = Arc::new(Metrics::new());
        m.observe_reconcile();
        let resp = metrics_router(m)
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(
            text.contains("external_ebs_autoresizer_reconcile_total 1"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn serve_binds_and_stops() {
        let shutdown = CancellationToken::new();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let router = Router::new().route("/", get(|| async { "hi" }));
        let handle = tokio::spawn(serve(router, port, shutdown.clone()));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let body = reqwest::get(format!("http://127.0.0.1:{port}/"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "hi");
        shutdown.cancel();
        handle.await.unwrap().unwrap();
        let busy = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let port = busy.local_addr().unwrap().port();
        let err = serve(Router::new(), port, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("listen on"), "{err}");
    }
}
