//! Operational collectors for `forklift-mcp`.

use std::sync::Arc;
use std::time::Duration;

use prometheus::{CounterVec, HistogramOpts, HistogramVec, Opts, Registry};

/// Outcome label value for a tool call the upstream API accepted.
pub const OUTCOME_OK: &str = "ok";
/// Outcome label value for a call the upstream API rejected (4xx/5xx surfaced
/// to the model as a tool error).
pub const OUTCOME_TOOL_ERROR: &str = "tool_error";
/// Outcome label value for a call that failed at the MCP level (bad
/// arguments, transport).
pub const OUTCOME_ERROR: &str = "error";

/// Metrics holds the forklift-mcp operational collectors. Tool-call metrics
/// are recorded by [`crate::mcp::Server::call_tool`]; upstream metrics by
/// [`crate::mcp::Client::do_request`]. All collectors are registered on the
/// registry passed to [`Metrics::new`], so tests and the binary each own their
/// registry.
pub struct Metrics {
    pub(crate) tool_calls: CounterVec,
    pub(crate) tool_duration: HistogramVec,
    pub(crate) upstream: CounterVec,
}

impl Metrics {
    /// Builds and registers the forklift-mcp collectors.
    ///
    pub fn new(registry: &Registry) -> Arc<Metrics> {
        let tool_calls = CounterVec::new(
            Opts::new(
                "forklift_mcp_tool_calls_total",
                "MCP tool calls by tool name and outcome (ok, tool_error, error).",
            ),
            &["tool", "outcome"],
        )
        .expect("build forklift_mcp_tool_calls_total");
        let tool_duration = HistogramVec::new(
            HistogramOpts::new(
                "forklift_mcp_tool_call_duration_seconds",
                "MCP tool call latency by tool name, including the upstream API request.",
            )
            .buckets(vec![
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 2.5, 5.0, 10.0,
            ]),
            &["tool"],
        )
        .expect("build forklift_mcp_tool_call_duration_seconds");
        let upstream = CounterVec::new(
            Opts::new(
                "forklift_mcp_upstream_requests_total",
                "Requests proxied to the forklift management API by method and HTTP status code (code transport_error when no response was received).",
            ),
            &["method", "code"],
        )
        .expect("build forklift_mcp_upstream_requests_total");

        registry
            .register(Box::new(tool_calls.clone()))
            .expect("register forklift_mcp_tool_calls_total");
        registry
            .register(Box::new(tool_duration.clone()))
            .expect("register forklift_mcp_tool_call_duration_seconds");
        registry
            .register(Box::new(upstream.clone()))
            .expect("register forklift_mcp_upstream_requests_total");

        Arc::new(Metrics {
            tool_calls,
            tool_duration,
            upstream,
        })
    }

    /// Counts one proxied management-API request.
    pub fn record_upstream(&self, method: &str, status_code: u16, transport_err: bool) {
        let code = if transport_err {
            "transport_error".to_string()
        } else {
            status_code.to_string()
        };
        self.upstream.with_label_values(&[method, &code]).inc();
    }

    /// Records count and latency for one `tools/call` request.
    pub fn record_tool_call(&self, tool: &str, outcome: &str, elapsed: Duration) {
        let tool = if tool.is_empty() { "unknown" } else { tool };
        self.tool_calls.with_label_values(&[tool, outcome]).inc();
        self.tool_duration
            .with_label_values(&[tool])
            .observe(elapsed.as_secs_f64());
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use axum::Router;
    use axum::http::{StatusCode, Uri};
    use axum::response::IntoResponse;
    use prometheus::Registry;
    use prometheus::core::Collector;

    use crate::mcp::client::Client;
    use crate::mcp::metrics::{Metrics, OUTCOME_OK, OUTCOME_TOOL_ERROR};
    use crate::mcp::server::tests::{connect, install_crypto_provider, no_args, spawn_upstream};

    #[tokio::test]
    async fn metrics_record_tool_calls_and_upstream() {
        install_crypto_provider();
        let app = Router::new().fallback(|uri: Uri| async move {
            if uri.path() == "/api/v1/version" {
                return (StatusCode::OK, r#"{"version":"test"}"#).into_response();
            }
            (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response()
        });
        let upstream = spawn_upstream(app).await;

        let reg = Registry::new();
        let metrics = Metrics::new(&reg);
        let session = connect(
            Client::new(&upstream, "", Some(metrics.clone())),
            Some(metrics.clone()),
        );

        session
            .call(None, "forklift_version", no_args())
            .await
            .expect("forklift_version");
        session
            .call(None, "forklift_list_repositories", no_args())
            .await
            .expect("forklift_list_repositories");

        let got = metrics
            .tool_calls
            .with_label_values(&["forklift_version", OUTCOME_OK])
            .get();
        assert_eq!(
            got, 1.0,
            "tool_calls{{forklift_version,ok}} = {got}, want 1"
        );
        let got = metrics
            .tool_calls
            .with_label_values(&["forklift_list_repositories", OUTCOME_TOOL_ERROR])
            .get();
        assert_eq!(
            got, 1.0,
            "tool_calls{{forklift_list_repositories,tool_error}} = {got}, want 1"
        );
        let got = metrics.upstream.with_label_values(&["GET", "200"]).get();
        assert_eq!(got, 1.0, "upstream{{GET,200}} = {got}, want 1");
        let got = metrics.upstream.with_label_values(&["GET", "401"]).get();
        assert_eq!(got, 1.0, "upstream{{GET,401}} = {got}, want 1");

        let series: usize = metrics
            .tool_duration
            .collect()
            .iter()
            .map(|family| family.get_metric().len())
            .sum();
        assert_eq!(series, 2, "tool_duration series = {series}, want 2");
    }
}
