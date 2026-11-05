//! Readiness state behind the liveness and readiness probes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;

/// Tracks readiness. `/healthz` always returns 200 (liveness); `/readyz`
/// returns 200 only when ready.
#[derive(Debug, Default)]
pub struct Health {
    ready: AtomicBool,
}

impl Health {
    /// Flips the readiness state reported by `/readyz`.
    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::SeqCst);
    }

    /// The readiness verdict.
    #[must_use]
    pub fn readiness(&self) -> (StatusCode, &'static str) {
        if self.ready.load(Ordering::SeqCst) {
            (StatusCode::OK, "ready")
        } else {
            (StatusCode::SERVICE_UNAVAILABLE, "not ready")
        }
    }

    /// The probe routes.
    pub fn router(self: &Arc<Self>) -> Router {
        let health = self.clone();
        Router::new()
            .route("/healthz", get(|| async { (StatusCode::OK, "ok") }))
            .route("/readyz", get(move || async move { health.readiness() }))
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    async fn probe(router: Router, path: &str) -> (StatusCode, String) {
        let resp = router
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn readiness_flips() {
        let h = Arc::new(Health::default());
        assert_eq!(
            probe(h.router(), "/healthz").await,
            (StatusCode::OK, "ok".into())
        );
        assert_eq!(
            probe(h.router(), "/readyz").await,
            (StatusCode::SERVICE_UNAVAILABLE, "not ready".into())
        );
        h.set_ready(true);
        assert_eq!(
            probe(h.router(), "/readyz").await,
            (StatusCode::OK, "ready".into())
        );
        h.set_ready(false);
        assert_eq!(h.readiness().0, StatusCode::SERVICE_UNAVAILABLE);
    }
}
