//! HTTP listeners: the health endpoints mounted next to the webhook, the
//! `/metrics` endpoint on its own port, and the graceful serve loop both use.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::Metrics;

/// How long an in-flight request may finish after the shutdown signal before
/// the listener is torn down regardless.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Returns `/healthz` and `/readyz`, so the caller can mount them on the same
/// listener as the webhook. The gateway is ready as soon as its dependencies
/// are wired, because it holds no state that needs warming.
pub fn health_router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/healthz", get(ok))
        .route("/readyz", get(ok))
}

async fn ok() -> StatusCode {
    StatusCode::OK
}

/// Returns the router serving `/metrics` from the given metric set.
pub fn metrics_router(metrics: Arc<Metrics>) -> Router {
    Router::new()
        .route("/metrics", get(serve_metrics))
        .with_state(metrics)
}

async fn serve_metrics(State(metrics): State<Arc<Metrics>>) -> Response {
    match metrics.encode() {
        Ok(body) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                "application/openmetrics-text; version=1.0.0; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("encode metrics: {err}"),
        )
            .into_response(),
    }
}

/// Binds a listener on every interface at `port`.
///
/// # Errors
///
/// Returns an error when the port cannot be bound.
pub async fn bind(port: u16) -> Result<TcpListener> {
    TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("bind port {port}"))
}

/// Serves `router` on `listener` until `shutdown` fires, then drains the
/// connections still open.
///
/// # Errors
///
/// Returns an error when the accept loop fails.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    shutdown: CancellationToken,
) -> Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown.cancelled().await;
            // Give in-flight requests a bounded window; the drain of
            // detached analyses happens after this returns.
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        })
        .await
        .context("http server")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn spawn(
        router: Router,
    ) -> (
        String,
        CancellationToken,
        tokio::task::JoinHandle<Result<()>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let token = CancellationToken::new();
        let handle = tokio::spawn(serve(listener, router, token.clone()));
        (format!("http://{addr}"), token, handle)
    }

    #[tokio::test]
    async fn health_and_metrics_endpoints() {
        let metrics = Arc::new(Metrics::new());
        metrics.webhook("analyzing");
        let router = health_router().merge(metrics_router(metrics));
        let (base, token, handle) = spawn(router).await;

        for path in ["/healthz", "/readyz"] {
            let resp = reqwest::get(format!("{base}{path}")).await.unwrap();
            assert_eq!(resp.status(), 200, "{path}");
        }
        let resp = reqwest::get(format!("{base}/metrics")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(
            resp.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/openmetrics-text")
        );
        let body = resp.text().await.unwrap();
        assert!(body.contains("kagent_gateway_webhooks_received_total{result=\"analyzing\"} 1"));

        let resp = reqwest::get(format!("{base}/missing")).await.unwrap();
        assert_eq!(resp.status(), 404);

        token.cancel();
        // The grace period is real time; the listener closes once it elapses.
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn bind_rejects_a_busy_port() {
        let taken = TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port = taken.local_addr().unwrap().port();
        assert!(bind(port).await.is_err());
    }
}
