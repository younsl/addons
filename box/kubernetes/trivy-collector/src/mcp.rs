//! Embedded MCP (Model Context Protocol) server
//!
//! Exposes the report store to LLM agents over the MCP Streamable HTTP
//! transport at `/mcp`. Runs inside the server pod and reads the same SQLite
//! database as the REST API, so no extra deployment is needed.
//!
//! # Module Structure
//! - `handler`: `ServerHandler` implementation and tool definitions
//! - `params`: tool input types and limit clamping
//! - `authz`: per-tool RBAC checks against the HTTP-layer `AuthSession`
//!
//! # Compatibility
//! Tested against clients that speak the 2025-03-26 and 2025-06-18 protocol
//! revisions with `Mcp-Session-Id` sessions (kagent `RemoteMCPServer` with
//! `protocol: STREAMABLE_HTTP`). Authentication is handled by the surrounding
//! axum auth middleware; the MCP layer only reads the resulting session.

pub mod authz;
pub mod handler;
pub mod params;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::web::AppState;
pub use handler::TrivyMcp;

/// Path where the MCP endpoint is mounted on the server router.
pub const MCP_PATH: &str = "/mcp";

/// Maximum accepted JSON-RPC request body. Tool arguments are small; anything
/// larger is a misbehaving client.
const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;

/// SSE keep-alive interval. Keeps idle streams alive through proxies and
/// under client-side read timeouts (kagent `sseReadTimeout`).
const SSE_KEEP_ALIVE: Duration = Duration::from_secs(15);

/// Runtime options for the embedded MCP server, derived from [`crate::config::Config`].
#[derive(Debug, Clone, Default)]
pub struct McpOptions {
    /// Allowed `Host` header values. Empty disables validation, which is the
    /// right choice for in-cluster access through a Service DNS name.
    pub allowed_hosts: Vec<String>,
    /// Disable sessions. Required when several replicas share one Service.
    pub stateless: bool,
    /// Concurrent tool executions allowed across all sessions. 0 = unlimited.
    pub max_concurrency: usize,
}

impl McpOptions {
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            allowed_hosts: config
                .mcp_allowed_hosts
                .iter()
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
                .collect(),
            stateless: config.mcp_stateless,
            max_concurrency: config.mcp_max_concurrency,
        }
    }
}

/// Build the transport config shared by both session modes.
fn transport_config(opts: &McpOptions, shutdown: CancellationToken) -> StreamableHttpServerConfig {
    let mut config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(!opts.stateless)
        .with_json_response(true)
        .with_sse_keep_alive(Some(SSE_KEEP_ALIVE))
        .with_max_request_body_bytes(MAX_REQUEST_BODY_BYTES)
        .with_cancellation_token(shutdown)
        // Server-to-server calls carry no Origin header.
        .disable_allowed_origins();

    config = if opts.allowed_hosts.is_empty() {
        config.disable_allowed_hosts()
    } else {
        config.with_allowed_hosts(opts.allowed_hosts.clone())
    };
    config
}

/// Build an axum router that serves MCP at [`MCP_PATH`].
///
/// The returned router carries no state of its own; callers merge it into the
/// protected route group so the auth and RBAC middleware run first.
pub fn router(state: AppState, opts: &McpOptions, shutdown: CancellationToken) -> Router<AppState> {
    info!(
        path = MCP_PATH,
        stateless = opts.stateless,
        host_validation = !opts.allowed_hosts.is_empty(),
        "Embedded MCP server enabled"
    );

    let config = transport_config(opts, shutdown);
    // One limiter for the whole endpoint. The factory runs once per session,
    // so the semaphore must be created here, not inside `TrivyMcp::new`.
    let limiter = handler::ToolLimiter::new(opts.max_concurrency);
    let factory_state = state;
    let factory = move || Ok(TrivyMcp::new(factory_state.clone(), limiter.clone()));

    if opts.stateless {
        let service =
            StreamableHttpService::new(factory, Arc::new(NeverSessionManager::default()), config);
        Router::new().nest_service(MCP_PATH, service)
    } else {
        let service =
            StreamableHttpService::new(factory, Arc::new(LocalSessionManager::default()), config);
        Router::new().nest_service(MCP_PATH, service)
    }
}

/// Bridge the process-wide `watch` shutdown signal into a `CancellationToken`
/// that rmcp understands. Cancelling the token closes every open MCP session.
pub fn shutdown_token(mut shutdown: tokio::sync::watch::Receiver<bool>) -> CancellationToken {
    let token = CancellationToken::new();
    let child = token.clone();
    tokio::spawn(async move {
        let _ = shutdown.changed().await;
        child.cancel();
    });
    token
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn options_strip_blank_hosts() {
        let mut config = crate::config::Config::try_parse_from(["trivy-collector"]).unwrap();
        config.mcp_allowed_hosts = vec![
            "".to_string(),
            " trivy.example.com ".to_string(),
            "  ".to_string(),
        ];
        let opts = McpOptions::from_config(&config);
        assert_eq!(opts.allowed_hosts, vec!["trivy.example.com".to_string()]);
        assert!(!opts.stateless);
        assert_eq!(opts.max_concurrency, 8);
    }

    #[test]
    fn transport_config_disables_host_check_when_empty() {
        let opts = McpOptions::default();
        let cfg = transport_config(&opts, CancellationToken::new());
        assert!(cfg.allowed_hosts.is_empty());
        assert!(cfg.allowed_origins.is_empty());
        assert!(cfg.legacy_session_mode);
        assert!(cfg.json_response);
        assert_eq!(cfg.max_request_body_bytes, MAX_REQUEST_BODY_BYTES);
    }

    #[test]
    fn transport_config_stateless_and_hosts() {
        let opts = McpOptions {
            allowed_hosts: vec!["a.example.com".into()],
            stateless: true,
            max_concurrency: 0,
        };
        let cfg = transport_config(&opts, CancellationToken::new());
        assert_eq!(cfg.allowed_hosts, vec!["a.example.com".to_string()]);
        assert!(!cfg.legacy_session_mode);
    }

    #[tokio::test]
    async fn shutdown_token_cancels_on_signal() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let token = shutdown_token(rx);
        assert!(!token.is_cancelled());
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), token.cancelled())
            .await
            .expect("token cancelled after shutdown signal");
    }

    // ───── HTTP transport tests (full axum router, auth + RBAC middleware) ─────

    use crate::auth::AuthMode;
    use crate::web::test_support;

    const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"kagent","version":"test"}}}"#;
    const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    const LIST_TOOLS: &str = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
    const CALL_STATS: &str = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_stats","arguments":{}}}"#;

    async fn spawn(
        auth_mode: AuthMode,
        default_policy: &str,
        stateless: bool,
    ) -> (String, CancellationToken) {
        let mut config = crate::config::Config::for_test(crate::config::Mode::Server);
        config.mcp_enabled = true;
        config.mcp_stateless = stateless;
        let mut state = test_support::state_with(
            crate::storage::Database::new(":memory:").await.unwrap(),
            crate::auth::rbac::RbacPolicy::default_csv(),
            default_policy,
        );
        state.config = Arc::new(crate::web::state::ConfigInfo::from(&config));
        let ct = CancellationToken::new();
        let mcp = router(state.clone(), &McpOptions::from_config(&config), ct.clone());
        let app = crate::web::build_router(state, auth_mode, Some(mcp));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let ct2 = ct.clone();
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(async move { ct2.cancelled_owned().await })
            .await;
        });
        (format!("http://{addr}{MCP_PATH}"), ct)
    }

    fn sse_messages(body: &str) -> Vec<serde_json::Value> {
        body.lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(|d| serde_json::from_str(d).unwrap())
            .collect()
    }

    /// Decode a Streamable HTTP response body regardless of whether the server
    /// chose `application/json` or `text/event-stream`.
    async fn decode(resp: reqwest::Response) -> serde_json::Value {
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = resp.text().await.unwrap();
        if ct.contains("text/event-stream") {
            sse_messages(&body).pop().expect("at least one SSE message")
        } else {
            serde_json::from_str(&body).unwrap()
        }
    }

    fn post(client: &reqwest::Client, url: &str, body: &'static str) -> reqwest::RequestBuilder {
        client
            .post(url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            // Simulate an in-cluster Service DNS Host header.
            .header(
                "Host",
                "trivy-collector.trivy-system.svc.cluster.local:3000",
            )
            .body(body)
    }

    #[tokio::test]
    async fn http_session_handshake_list_and_call() {
        let (url, ct) = spawn(AuthMode::None, "role:readonly", false).await;
        let client = reqwest::Client::new();

        let resp = post(&client, &url, INIT).send().await.unwrap();
        assert_eq!(resp.status(), 200, "initialize");
        let session = resp
            .headers()
            .get("mcp-session-id")
            .expect("session id issued in legacy session mode")
            .to_str()
            .unwrap()
            .to_string();
        let init = decode(resp).await;
        assert_eq!(init["result"]["serverInfo"]["name"], "trivy-collector");
        assert!(init["result"]["capabilities"]["tools"].is_object());

        let resp = post(&client, &url, INITIALIZED)
            .header("Mcp-Session-Id", &session)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202, "initialized notification");

        let resp = post(&client, &url, LIST_TOOLS)
            .header("Mcp-Session-Id", &session)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let tools = decode(resp).await;
        let names: Vec<&str> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), 9);
        assert!(names.contains(&"get_stats"));
        assert!(names.contains(&"search_sbom_components"));
        let stats_tool = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "get_stats")
            .unwrap();
        assert_eq!(stats_tool["annotations"]["readOnlyHint"], true);

        let resp = post(&client, &url, CALL_STATS)
            .header("Mcp-Session-Id", &session)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let call = decode(resp).await;
        assert_ne!(call["result"]["isError"], true);
        let text = call["result"]["content"][0]["text"].as_str().unwrap();
        let stats: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(stats["total_clusters"], 0);

        // Missing session id on a non-initialize request is rejected.
        let resp = post(&client, &url, LIST_TOOLS).send().await.unwrap();
        assert!(
            resp.status().is_client_error(),
            "expected 4xx without session, got {}",
            resp.status()
        );

        // Session teardown.
        let resp = client
            .delete(&url)
            .header("Mcp-Session-Id", &session)
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "DELETE session: {}",
            resp.status()
        );

        ct.cancel();
    }

    #[tokio::test]
    async fn http_stateless_mode_returns_json_without_session() {
        let (url, ct) = spawn(AuthMode::None, "role:readonly", true).await;
        let client = reqwest::Client::new();

        let resp = post(&client, &url, INIT).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(resp.headers().get("mcp-session-id").is_none());
        let ctype = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(ctype.contains("application/json"), "got {ctype}");

        let resp = post(&client, &url, CALL_STATS).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let call = decode(resp).await;
        assert!(call["result"]["content"][0]["text"].is_string());

        ct.cancel();
    }

    #[tokio::test]
    async fn http_rbac_gate_blocks_endpoint() {
        // Keycloak mode with no OIDC state: require_auth passes through with no
        // session, require_rbac evaluates the empty default policy and denies.
        let (url, ct) = spawn(AuthMode::Keycloak, "", false).await;
        let client = reqwest::Client::new();
        let resp = post(&client, &url, INIT).send().await.unwrap();
        assert_eq!(resp.status(), 403);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["resource"], "reports");
        assert_eq!(body["action"], "get");
        ct.cancel();
    }

    #[tokio::test]
    async fn http_rbac_gate_allows_readonly_default() {
        let (url, ct) = spawn(AuthMode::Keycloak, "role:readonly", false).await;
        let client = reqwest::Client::new();
        let resp = post(&client, &url, INIT).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        ct.cancel();
    }
}
