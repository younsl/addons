//! `/healthz` (liveness) and `/readyz` (readiness).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;

/// Readiness flag shared between the collector and the health router.
#[derive(Debug, Default)]
pub struct Health {
    ready: AtomicBool,
}

impl Health {
    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::SeqCst);
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    /// Router serving both probes.
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/healthz", get(|| async { StatusCode::OK }))
            .route("/readyz", get(readyz))
            .with_state(self)
    }
}

async fn readyz(State(health): State<Arc<Health>>) -> StatusCode {
    if health.is_ready() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    async fn status(router: Router, path: &str) -> StatusCode {
        router
            .oneshot(Request::get(path).body(Body::empty()).expect("request"))
            .await
            .expect("response")
            .status()
    }

    #[tokio::test]
    async fn probes_follow_readiness() {
        let health = Arc::new(Health::default());
        assert_eq!(
            status(Arc::clone(&health).router(), "/healthz").await,
            StatusCode::OK
        );
        assert_eq!(
            status(Arc::clone(&health).router(), "/readyz").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        health.set_ready(true);
        assert_eq!(
            status(Arc::clone(&health).router(), "/readyz").await,
            StatusCode::OK
        );
        assert_eq!(
            status(health.router(), "/missing").await,
            StatusCode::NOT_FOUND
        );
    }
}
