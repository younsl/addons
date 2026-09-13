//! Request logging with per-route metrics, and the panic recoverer that keeps
//! one bad handler from taking the listener down.

use std::any::Any;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{ConnectInfo, MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use http::StatusCode;
use tower_http::catch_panic::CatchPanicLayer;

use super::{Metrics, http_error};

/// Records one request's latency and outcome, then logs it at debug level.
///
/// The route label is the matched path pattern rather than the concrete URI, so
/// `/api/v1/repositories/{id}` stays one series instead of one per id; requests that matched no
/// route are labelled `unmatched`.
pub(crate) async fn log_requests(
    State(metrics): State<Arc<Metrics>>,
    req: Request,
    next: Next,
) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| route_label(p.as_str()))
        .unwrap_or_else(|| "unmatched".to_string());
    let remote = req
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.to_string())
        .unwrap_or_default();

    let resp = next.run(req).await;

    let elapsed = start.elapsed();
    let status = status_text(resp.status());
    let method_str = method.as_str();
    metrics
        .req_total
        .with_label_values(&[method_str, route.as_str(), status])
        .inc();
    metrics
        .req_duration
        .with_label_values(&[method_str, route.as_str(), status])
        .observe(elapsed.as_secs_f64());

    tracing::debug!(
        method = %method,
        path = %path,
        status = resp.status().as_u16(),
        duration_ms = elapsed.as_millis() as i64,
        remote = %remote,
        "request"
    );
    resp
}

/// Spells a matched axum route the way the previous router did, so the `route`
/// label values that dashboards and alerts already match stay the same: the
/// catch-all is `*`, and a repository root such as `/maven/{repo}/` was served
/// by the catch-all route there.
pub(crate) fn route_label(matched: &str) -> String {
    let label = matched.replace("{*rest}", "*");
    if label.ends_with("/{repo}/") {
        format!("{label}*")
    } else {
        label
    }
}

pub(crate) fn status_text(status: StatusCode) -> &'static str {
    status.canonical_reason().unwrap_or("")
}

///
/// Everything else - the message, the `err` and `stack` fields, the plain-text 500 body - is
/// unchanged, and the request log emitted by the surrounding middleware still carries the path.
pub(crate) fn recoverer()
-> CatchPanicLayer<impl Fn(Box<dyn Any + Send + 'static>) -> Response + Clone> {
    CatchPanicLayer::custom(|err: Box<dyn Any + Send + 'static>| -> Response {
        let msg = panic_message(err.as_ref());
        tracing::error!(
            err = %msg,
            stack = %std::backtrace::Backtrace::force_capture(),
            "panic recovered"
        );
        http_error(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
    })
}

fn panic_message(err: &(dyn Any + Send)) -> String {
    if let Some(s) = err.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}
