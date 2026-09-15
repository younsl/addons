//! The MCP server: tool routing, the HTTP listener that carries it, and the
//! bearer check in front of the endpoint.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, tool_handler};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::backstage::Client;

/// How long in-flight requests may finish after the shutdown signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

pub const INSTRUCTIONS: &str = "Read-only access to a Backstage developer portal. Nothing here creates, changes or deletes anything.

Start with catalog_search_entities or search_query to find things, then drill in with the *_get_* tools. Entity references look like kind:namespace/name (component:default/payments-api). Most list tools accept offset and limit and report total and truncated, so page rather than raising limit. Cost, token and IAM data comes from the last scheduled collection, so check the *_get_status or *_get_config tools for the collection time when freshness matters.";

/// The tool handler. Cheap to clone: the HTTP transport builds one per
/// session and every clone shares the same Backstage client.
#[derive(Clone)]
pub struct BackstageMcp {
    pub(crate) client: Arc<Client>,
    pub(crate) max_result_chars: usize,
    tool_router: ToolRouter<Self>,
}

impl BackstageMcp {
    #[must_use]
    pub fn new(client: Arc<Client>, max_result_chars: usize) -> Self {
        Self {
            client,
            max_result_chars,
            tool_router: Self::catalog_router()
                + Self::search_router()
                + Self::techdocs_router()
                + Self::platforms_router()
                + Self::openapi_registry_router()
                + Self::catalog_health_router()
                + Self::argocd_router()
                + Self::gitlab_token_audit_router()
                + Self::opencost_router()
                + Self::iam_user_audit_router()
                + Self::opensearch_router()
                + Self::opensearch_scaling_router()
                + Self::s3_log_extract_router()
                + Self::pat_router(),
        }
    }

    /// Names of every registered tool, sorted.
    #[must_use]
    pub fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        names.sort();
        names
    }
}

// The macro emits `async fn` bodies without awaits, which the nursery lint
// flags; the signature is the trait's, not ours.
#[allow(clippy::unused_async_trait_impl)]
#[tool_handler(router = self.tool_router)]
impl ServerHandler for BackstageMcp {
    fn get_info(&self) -> ServerInfo {
        let mut implementation = Implementation::from_build_env();
        implementation.name = env!("CARGO_PKG_NAME").to_string();
        implementation.version = env!("CARGO_PKG_VERSION").to_string();
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(implementation)
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_instructions(INSTRUCTIONS.to_string())
    }
}

/// Settings for the HTTP listener.
#[derive(Debug, Clone)]
pub struct HttpOptions {
    pub mcp_path: String,
    /// Bearer token clients must present; empty disables the check.
    pub bearer_token: String,
}

#[derive(Clone)]
struct AppState {
    bearer_token: Arc<String>,
}

/// Builds the axum application: probes, the bearer check and the MCP
/// endpoint. Stateless MCP mode, so any replica can answer any request.
pub fn app(handler: BackstageMcp, options: &HttpOptions, shutdown: CancellationToken) -> Router {
    let client = Arc::clone(&handler.client);
    let state = AppState {
        bearer_token: Arc::new(options.bearer_token.clone()),
    };

    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_cancellation_token(shutdown)
        // Host validation guards local servers against DNS rebinding; this
        // one is reached through a Kubernetes Service name, so the Host header
        // is whatever the caller used and the check has to be off.
        .disable_allowed_hosts();
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        LocalSessionManager::default().into(),
        config,
    );

    let mcp = Router::new()
        .nest_service(&options.mcp_path, service)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ));

    Router::new()
        .route("/healthz", get(healthz))
        .route(
            "/readyz",
            // The client is captured here rather than pulled from `State`, so
            // the readiness call is not reachable from a handler argument.
            get(move || {
                let client = Arc::clone(&client);
                async move { readyz(&client).await }
            }),
        )
        .merge(mcp)
        .with_state(state)
}

async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

/// Ready once Backstage answers its own readiness probe, so a pod that
/// cannot reach the portal is taken out of the Service.
async fn readyz(client: &Client) -> Response {
    match client
        .get_json::<serde_json::Value>("/.backstage/health/v1/readiness", &[])
        .await
    {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({ "status": "ok" }))).into_response(),
        Err(err) => {
            warn!(error = %err, "readiness check failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "status": "backstage unreachable" })),
            )
                .into_response()
        }
    }
}

async fn require_bearer(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if state.bearer_token.is_empty()
        || token_matches(
            &state.bearer_token,
            request.headers().get(header::AUTHORIZATION),
        )
    {
        return next.run(request).await;
    }
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "error": { "code": -32001, "message": "Unauthorized" },
        "id": null
    });
    let mut response = (StatusCode::UNAUTHORIZED, Json(body)).into_response();
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

/// Constant-time comparison of the presented bearer token.
#[must_use]
pub fn token_matches(expected: &str, header: Option<&HeaderValue>) -> bool {
    let Some(value) = header.and_then(|h| h.to_str().ok()) else {
        return false;
    };
    let Some((scheme, token)) = value.trim().split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("bearer") {
        return false;
    }
    let token = token.trim();
    token.len() == expected.len() && token.as_bytes().ct_eq(expected.as_bytes()).into()
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

/// Serves `router` until `shutdown` fires, then drains open connections.
///
/// # Errors
///
/// Returns an error when the accept loop fails.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    shutdown: CancellationToken,
) -> Result<()> {
    info!(address = %listener.local_addr()?, "listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown.cancelled().await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        })
        .await
        .context("http server")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    use crate::tools::testing;

    fn bearer(value: &str) -> HeaderValue {
        HeaderValue::from_str(value).unwrap()
    }

    #[test]
    fn bearer_comparison() {
        assert!(token_matches("abc", Some(&bearer("Bearer abc"))));
        assert!(token_matches("abc", Some(&bearer("bearer  abc "))));
        assert!(!token_matches("abc", Some(&bearer("Bearer abd"))));
        assert!(!token_matches("abc", Some(&bearer("Bearer abcd"))));
        assert!(!token_matches("abc", Some(&bearer("Basic abc"))));
        assert!(!token_matches("abc", Some(&bearer("abc"))));
        assert!(!token_matches("abc", None));
    }

    #[test]
    fn server_info_and_tools() {
        let (_server, handler) = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(testing::mcp());
        let info = handler.get_info();
        assert!(info.capabilities.tools.is_some());
        assert_eq!(info.instructions.as_deref(), Some(INSTRUCTIONS));
        let names = handler.tool_names();
        assert!(names.len() > 40, "{names:?}");
        assert!(names.contains(&"catalog_search_entities".to_string()));
        assert!(
            names
                .iter()
                .all(|n| n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        );
    }

    async fn call(router: Router, request: Request) -> (StatusCode, String) {
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    fn options(token: &str) -> HttpOptions {
        HttpOptions {
            mcp_path: "/mcp".to_string(),
            bearer_token: token.to_string(),
        }
    }

    fn initialize_request(auth: Option<&str>) -> Request {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        });
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            // A Service hostname rather than loopback, which the default rmcp
            // host allow-list would reject.
            .header("host", "backstage-mcp.backstage.svc:8080")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(auth) = auth {
            builder = builder.header("authorization", auth);
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn probes_and_auth() {
        let (server, handler) = testing::mcp().await;
        Mock::given(method("GET"))
            .and(path("/.backstage/health/v1/readiness"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"status": "ok"})),
            )
            .mount(&server)
            .await;
        let router = app(handler, &options("secret"), CancellationToken::new());

        let (status, body) = call(
            router.clone(),
            Request::get("/healthz").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("ok"));

        let (status, _) = call(
            router.clone(),
            Request::get("/readyz").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(router.clone(), initialize_request(None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body.contains("Unauthorized"));

        let (status, body) = call(router.clone(), initialize_request(Some("Bearer wrong"))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body.contains("Unauthorized"));

        let (status, body) = call(router, initialize_request(Some("Bearer secret"))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("backstage-mcp"), "{body}");
    }

    #[tokio::test]
    async fn readiness_fails_without_backstage_and_auth_can_be_off() {
        let (_server, handler) = testing::mcp().await;
        let router = app(handler, &options(""), CancellationToken::new());
        let (status, body) = call(
            router.clone(),
            Request::get("/readyz").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("unreachable"));

        let (status, body) = call(router, initialize_request(None)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}
