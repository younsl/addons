//! Liveness and readiness probes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use http::{HeaderValue, StatusCode, header};

use super::{Metrics, Ready, http_error};

/// Bounds the database check inside the readiness probe. The probe itself has
/// a short timeout (one second by default), so answering "not ready" quickly
/// is more useful than blocking until the caller gives up: a bounded answer is
/// recorded, counted and visible, while a hung handler is only a probe failure
/// with no reason attached.
const READYZ_TIMEOUT: Duration = Duration::from_millis(750);

/// What the probe handlers need: the readiness flag, the store to ping and the
/// probe metrics.
pub(crate) struct HealthState {
    pub(crate) ready: Ready,
    pub(crate) store: Arc<crate::meta::Store>,
    pub(crate) metrics: Arc<Metrics>,
}

/// The liveness probe: the process is up and serving.
pub(crate) async fn handle_healthz() -> Response {
    text_ok("ok")
}

/// The readiness probe. In HA mode only the elected leader is ready, so the
/// Service routes traffic to a single active instance with a healthy database
/// connection.
pub(crate) async fn handle_readyz(State(state): State<Arc<HealthState>>) -> Response {
    let started = Instant::now();
    let resp = readyz(&state).await;
    state
        .metrics
        .ready_duration
        .observe(started.elapsed().as_secs_f64());
    resp
}

async fn readyz(state: &HealthState) -> Response {
    let started = Instant::now();
    if !state.ready.get() {
        state
            .metrics
            .ready_fail
            .with_label_values(&["not_leader"])
            .inc();
        return http_error(StatusCode::SERVICE_UNAVAILABLE, "not leader");
    }
    let ping = tokio::time::timeout(READYZ_TIMEOUT, state.store.ping()).await;
    let failure = match ping {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e.to_string()),
        Err(_) => Some(format!(
            "context deadline exceeded after {READYZ_TIMEOUT:?}"
        )),
    };
    if let Some(err) = failure {
        state
            .metrics
            .ready_fail
            .with_label_values(&["database"])
            .inc();
        tracing::warn!(
            err = %err,
            elapsed = ?started.elapsed(),
            "readiness check failed"
        );
        return http_error(StatusCode::SERVICE_UNAVAILABLE, "database unavailable");
    }
    text_ok("ready")
}

/// A 200 with a plain-text body, the shape both probes answer with.
fn text_ok(body: &'static str) -> Response {
    let mut resp = Response::new(Body::from(body));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}
