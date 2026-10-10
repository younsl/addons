//! Readiness state behind the liveness and readiness probes.

use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;

/// Tracks readiness. `/healthz` always returns 200, so a controller waiting for
/// leadership is not restarted; `/readyz` also requires a live approval
/// channel, so a revoked token or blocked egress shows up instead of
/// maintenance piling up unapproved.
#[derive(Debug, Default)]
pub struct Health {
    ready: AtomicBool,
    slack_connected: AtomicBool,
}

impl Health {
    /// Flips the readiness state.
    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::SeqCst);
    }

    /// Records whether the Socket Mode connection is up.
    pub fn set_slack_connected(&self, connected: bool) {
        self.slack_connected.store(connected, Ordering::SeqCst);
    }

    /// The readiness verdict and its reason.
    #[must_use]
    pub fn readiness(&self) -> (StatusCode, &'static str) {
        if !self.ready.load(Ordering::SeqCst) {
            (StatusCode::SERVICE_UNAVAILABLE, "not ready")
        } else if !self.slack_connected.load(Ordering::SeqCst) {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "slack socket mode disconnected",
            )
        } else {
            (StatusCode::OK, "ready")
        }
    }

    /// The probe routes.
    pub fn router(self: &std::sync::Arc<Self>) -> Router {
        let health = self.clone();
        Router::new()
            .route("/healthz", get(|| async { (StatusCode::OK, "ok") }))
            .route("/readyz", get(move || async move { health.readiness() }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

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
    async fn readiness_needs_both_flags() {
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
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "slack socket mode disconnected".into()
            )
        );
        h.set_slack_connected(true);
        assert_eq!(
            probe(h.router(), "/readyz").await,
            (StatusCode::OK, "ready".into())
        );
        h.set_ready(false);
        assert_eq!(h.readiness().0, StatusCode::SERVICE_UNAVAILABLE);
    }
}
