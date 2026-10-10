//! API request logging and HTTP metrics.
//!
//! Requests are emitted as one structured line on stdout, which the cluster's
//! log pipeline already collects and can already query. The previous design
//! wrote a row per request into SQLite; with the database owned by the scraper
//! that would mean an HTTP write from the server on every request, which is
//! worse than the feature it powered.

use axum::{body::Body, extract::State, http::Request, middleware::Next, response::Response};
use std::time::Instant;
use tracing::info;

use crate::auth::session::AuthSession;
use crate::metrics::{HttpDurationLabels, HttpLabels};
use crate::web::AppState;

/// Paths excluded from the access log: infrastructure endpoints and the
/// unauthenticated identity probe the UI polls.
fn skip_access_log(path: &str) -> bool {
    !path.starts_with("/api/") || path.starts_with("/api/v1/auth/me")
}

/// Paths excluded from HTTP metrics: probes and static assets, which would
/// otherwise dominate the histograms.
fn skip_metrics(path: &str) -> bool {
    matches!(path, "/healthz" | "/readyz" | "/metrics")
        || path.starts_with("/assets/")
        || path.starts_with("/static/")
}

/// Log API requests to stdout and record Prometheus metrics.
pub async fn api_request_logger(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    let method = request.method().to_string();
    let user_agent = header_string(&request, "user-agent");
    let remote_addr = header_string(&request, "x-forwarded-for");

    // Set by require_auth; absent for unauthenticated routes.
    let (user_sub, user_email) = request
        .extensions()
        .get::<AuthSession>()
        .map(|s| (s.sub.clone(), s.email.clone().unwrap_or_default()))
        .unwrap_or_default();

    let start = Instant::now();
    let response = next.run(request).await;
    let duration_secs = start.elapsed().as_secs_f64();
    let status_code = response.status().as_u16();

    if !skip_metrics(&path) {
        if let Some(ref counter) = state.metrics.http_requests_total {
            counter
                .get_or_create(&HttpLabels {
                    method: method.clone(),
                    status: status_code.to_string(),
                })
                .inc();
        }
        if let Some(ref histogram) = state.metrics.http_request_duration_seconds {
            histogram
                .get_or_create(&HttpDurationLabels {
                    method: method.clone(),
                })
                .observe(duration_secs);
        }
    }

    if !skip_access_log(&path) {
        info!(
            target: "trivy_collector::access",
            method = %method,
            path = %path,
            status = status_code,
            duration_ms = (duration_secs * 1000.0) as u64,
            user_sub = %user_sub,
            user_email = %user_email,
            remote_addr = %remote_addr,
            user_agent = %user_agent,
            "api request"
        );
    }

    response
}

fn header_string(request: &Request<Body>, name: &str) -> String {
    request
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_api_paths_are_access_logged() {
        assert!(!skip_access_log("/api/v1/stats"));
        assert!(!skip_access_log("/api/v1/reports"));
        assert!(skip_access_log("/"));
        assert!(skip_access_log("/assets/app.js"));
        assert!(skip_access_log("/api-docs"));
    }

    #[test]
    fn the_identity_probe_is_not_access_logged() {
        // The UI polls it continuously; logging it drowns the real traffic.
        assert!(skip_access_log("/api/v1/auth/me"));
    }

    #[test]
    fn probes_and_assets_are_excluded_from_metrics() {
        for path in ["/healthz", "/readyz", "/metrics", "/assets/x", "/static/y"] {
            assert!(skip_metrics(path), "{path}");
        }
    }

    #[test]
    fn api_paths_are_included_in_metrics() {
        assert!(!skip_metrics("/api/v1/stats"));
        assert!(!skip_metrics("/"));
    }
}
