//! The server tier: UI, public API, MCP, OIDC, and RBAC.
//!
//! This pod holds no database and mounts no volume. Reports are read through a
//! `RemoteStore` pointed at the scraper's internal API; notes and API tokens
//! live in a watched ConfigMap and Secret; request logs go to stdout. That is
//! what makes it disposable — image bumps, config changes, and replica changes
//! here cost nothing, which is the point of the split.
//!
//! # Module Structure
//! - `handlers`: HTTP request handlers
//! - `state`: application state
//! - `types`: request and response types
//! - `admin_handlers`: admin summary
//! - `alert_handlers`: alert rule CRUD, preview, and test delivery
//! - `cluster_handlers`: hub-pull cluster registration
//! - `logging_middleware`: access logging and HTTP metrics

mod admin_handlers;
mod alert_handlers;
mod cluster_handlers;
mod handlers;
mod logging_middleware;
pub mod state;
mod types;

// Re-export public types
pub use handlers::{
    delete_report, get_config, get_dashboard_trends, get_hydration, get_sbom_report, get_stats,
    get_status, get_version, get_vulnerability_report, get_watcher_status, healthz, list_clusters,
    list_namespaces, list_sbom_reports, list_vulnerability_reports, receive_report,
    search_sbom_components, search_vulnerabilities, suggest_sbom_components,
    suggest_vulnerabilities, update_notes,
};
pub use state::{AppState, RuntimeInfo};
pub use types::{
    ComponentSearchQuery, ComponentSuggestQuery, ConfigItem, ConfigResponse, ErrorResponse,
    HealthResponse, ListQuery, ListResponse, StatusResponse, TrendQuery, UpdateNotesRequest,
    VersionResponse, VulnSearchQuery, VulnSuggestQuery, WatcherInfo, WatcherStatusResponse,
};

use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::collector::types::{ReportEvent, ReportEventType, ReportPayload};
use crate::storage::{
    ClusterInfo, ComponentSearchResult, FullReport, ReportMeta, Stats, TrendDataPoint, TrendMeta,
    TrendResponse, VulnSearchResult, VulnSummary,
};

/// OpenAPI documentation
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Trivy Collector API",
        description = "Multi-cluster Trivy report collector and viewer API",
        version = env!("CARGO_PKG_VERSION"),
        license(name = "Apache-2.0")
    ),
    paths(
        handlers::healthz,
        handlers::receive_report,
        handlers::list_vulnerability_reports,
        handlers::search_vulnerabilities,
        handlers::suggest_vulnerabilities,
        handlers::get_vulnerability_report,
        handlers::list_sbom_reports,
        handlers::search_sbom_components,
        handlers::suggest_sbom_components,
        handlers::get_sbom_report,
        handlers::list_clusters,
        handlers::get_stats,
        handlers::list_namespaces,
        handlers::delete_report,
        handlers::update_notes,
        handlers::get_watcher_status,
        handlers::get_hydration,
        handlers::get_version,
        handlers::get_status,
        handlers::get_config,
        handlers::get_dashboard_trends,
        admin_handlers::admin_info,
        alert_handlers::list_alerts,
        alert_handlers::get_alert,
        alert_handlers::create_alert,
        alert_handlers::update_alert,
        alert_handlers::delete_alert,
        alert_handlers::preview_alert,
        alert_handlers::test_alert_draft,
        cluster_handlers::list_registered_clusters,
        cluster_handlers::register_cluster,
        cluster_handlers::delete_registered_cluster,
        cluster_handlers::validate_cluster,
        crate::auth::handlers::auth_me,
        crate::auth::handlers::list_tokens,
        crate::auth::handlers::create_token,
        crate::auth::handlers::delete_token,
        crate::auth::handlers::logout,
    ),
    components(schemas(
        HealthResponse,
        ErrorResponse,
        UpdateNotesRequest,
        WatcherStatusResponse,
        WatcherInfo,
        VersionResponse,
        StatusResponse,
        ConfigResponse,
        ReportMeta,
        FullReport,
        ComponentSearchResult,
        VulnSearchResult,
        ClusterInfo,
        Stats,
        VulnSummary,
        ReportEvent,
        ReportEventType,
        ReportPayload,
        TrendResponse,
        TrendMeta,
        TrendDataPoint,
        cluster_handlers::RegisterClusterRequest,
        cluster_handlers::RegisteredCluster,
        cluster_handlers::ValidationResponse,
        alert_handlers::AlertRuleInput,
        alert_handlers::PreviewRequest,
        crate::alerts::preview::PreviewMatch,
        crate::alerts::preview::PreviewResult,
        crate::alerts::notifier::TestDeliveryResult,
        crate::alerts::types::AlertRule,
        crate::alerts::types::Matchers,
        crate::alerts::types::Receiver,
        crate::alerts::types::SlackReceiver,
    )),
    tags(
        (name = "Health", description = "Health check endpoints"),
        (name = "Reports", description = "Report management endpoints"),
        (name = "Vulnerability Reports", description = "Vulnerability report endpoints"),
        (name = "SBOM Reports", description = "SBOM report endpoints"),
        (name = "Clusters", description = "Cluster listing endpoints"),
        (name = "Namespaces", description = "Namespace listing endpoints"),
        (name = "Statistics", description = "Statistics endpoints"),
        (name = "Watcher", description = "Watcher and hydration status endpoints"),
        (name = "Version", description = "Build version information endpoints"),
        (name = "Status", description = "Server runtime status endpoints"),
        (name = "Config", description = "Configuration endpoints"),
        (name = "Dashboard", description = "Dashboard trend analysis endpoints"),
        (name = "Admin", description = "Admin summary endpoints"),
        (name = "Auth", description = "Authentication and token management endpoints"),
        (name = "Hub", description = "Cluster registration endpoints for hub-pull mode"),
        (name = "Alerts", description = "Alert rule management (ConfigMap-backed)"),
    )
)]
pub struct ApiDoc;

use anyhow::Result;
use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{Method, StatusCode, header},
    middleware as axum_middleware,
    response::{Html, IntoResponse},
    routing::{delete, get, post, put},
};
use rust_embed::Embed;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info, warn};

use crate::auth;
use crate::auth::rbac::RbacPolicy;
use crate::config::Config;
use crate::health::HealthServer;
use crate::metrics::Metrics;
use crate::storage::{NotesStore, RemoteStore, ReportStore, TokenStore};

/// Request body limit, sized for the largest Trivy report the ingest proxy
/// forwards.
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// How often the authored-state gauges are refreshed.
const GAUGE_REFRESH_SECS: u64 = 60;

/// How often the scraper's reachability is probed for readiness. `tokio`
/// intervals fire immediately on the first tick, so this is also how quickly a
/// freshly started pod can report ready.
const READINESS_PROBE_SECS: u64 = 5;

#[derive(Embed)]
#[folder = "static/"]
struct StaticAssets;

pub async fn run(
    config: Config,
    health_server: HealthServer,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    metrics: Arc<Metrics>,
) -> Result<()> {
    info!(
        port = config.server_port,
        scraper_url = %config.scraper_url,
        "Starting server mode (UI/API only — no database, no volume)"
    );

    let store: Arc<dyn ReportStore> = Arc::new(RemoteStore::new(
        &config.scraper_url,
        &config.internal_token,
    )?);

    let auth_mode = auth::AuthMode::from_str_lossy(&config.auth_mode);
    info!(auth_mode = %auth_mode, "Authentication mode configured");
    let auth_state = build_auth_state(&config, auth_mode).await?;

    let rbac = Arc::new(load_rbac_policy(&config));

    let alerts = match crate::alerts::build_evaluator(&config.external_url).await {
        Ok(eval) => Some(Arc::new(eval)),
        Err(e) => {
            warn!(error = %e, "Alerts subsystem disabled (Kubernetes API unavailable)");
            None
        }
    };

    // Authored state lives in Kubernetes objects, watched into memory so a
    // render never costs an API server round trip.
    let (notes, tokens) = build_authored_state_stores(&config, &shutdown).await;

    let state = AppState {
        store,
        config: Arc::new(state::ConfigInfo::from(&config)),
        runtime: Arc::new(state::RuntimeInfo::new()),
        auth: auth_state,
        rbac,
        metrics: metrics.clone(),
        alerts,
        notes,
        tokens,
    };

    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_origin(Any)
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION]);

    // Embedded MCP endpoint (opt-in). Shares the auth/RBAC middleware stack.
    let mcp_router = config.mcp_enabled.then(|| {
        crate::mcp::router(
            state.clone(),
            &crate::mcp::McpOptions::from_config(&config),
            crate::mcp::shutdown_token(shutdown.clone()),
        )
    });

    let app = build_router(state.clone(), auth_mode, mcp_router)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(cors);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.server_port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(addr = %addr, "Server listening");

    // Readiness tracks whether the scraper is reachable, not merely whether we
    // bound a port. Every report read is a call to the scraper now, so a pod
    // that cannot reach it serves nothing but 502s — and reporting Ready anyway
    // would let a wrong SCRAPER_URL or INTERNAL_TOKEN roll out fully green.
    let readiness = spawn_readiness_probe(state.clone(), health_server, shutdown.clone());
    let gauges = spawn_gauge_refresh(state, shutdown.clone());

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = shutdown.changed().await;
        info!("Server shutting down");
    })
    .await?;

    let _ = readiness.await;
    let _ = gauges.await;
    Ok(())
}

/// Track whether the scraper's internal API answers, and publish that as
/// readiness.
///
/// The probe is deliberately reachability rather than hydration: a scraper
/// mid-rebuild still serves, and the UI banner explains the partial data. Only
/// an unreachable scraper makes this pod useless.
fn spawn_readiness_probe(
    state: AppState,
    health_server: HealthServer,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(READINESS_PROBE_SECS));
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = interval.tick() => {
                    let reachable = match state.store.hydration().await {
                        Ok(_) => true,
                        Err(e) => {
                            warn!(
                                error = %e,
                                scraper_url = %state.config.scraper_url,
                                "Scraper unreachable — reporting not ready"
                            );
                            false
                        }
                    };
                    if reachable != health_server.is_ready() {
                        if reachable {
                            info!("Scraper reachable — server is ready");
                        }
                        health_server.set_ready(reachable);
                    }
                }
            }
        }
    })
}

/// Discover the OIDC provider when keycloak mode is on.
async fn build_auth_state(
    config: &Config,
    auth_mode: auth::AuthMode,
) -> Result<Option<Arc<auth::AuthState>>> {
    if auth_mode != auth::AuthMode::Keycloak {
        return Ok(None);
    }

    // validate() has already established that every field is present.
    let issuer_url = config.oidc_issuer_url.as_deref().unwrap();
    let client_id = config.oidc_client_id.as_deref().unwrap();
    let redirect_url = config.oidc_redirect_url.as_deref().unwrap();

    info!(
        issuer_url = %issuer_url,
        client_id = %client_id,
        redirect_url = %redirect_url,
        scopes = %config.oidc_scopes,
        "Connecting to OIDC provider"
    );

    let started = std::time::Instant::now();
    match auth::oidc::OidcClient::discover(
        issuer_url,
        client_id,
        config.oidc_client_secret.as_deref().unwrap(),
        redirect_url,
        &config.oidc_scopes,
    )
    .await
    {
        Ok(oidc_client) => {
            info!(
                issuer_url = %issuer_url,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "OIDC provider connected successfully"
            );
            Ok(Some(Arc::new(auth::AuthState {
                oidc_client,
                cookie_key: cookie::Key::generate(),
            })))
        }
        Err(e) => {
            error!(
                issuer_url = %issuer_url,
                elapsed_ms = started.elapsed().as_millis() as u64,
                error = %e,
                "Failed to connect to OIDC provider"
            );
            Err(e)
        }
    }
}

/// Load the RBAC policy from an inline CSV, a file path, or the built-in
/// default. A malformed policy falls back to the default rather than starting
/// with no policy at all.
fn load_rbac_policy(config: &Config) -> RbacPolicy {
    let csv = if config.rbac_policy_csv.is_empty() {
        RbacPolicy::default_csv().to_string()
    } else if std::path::Path::new(&config.rbac_policy_csv).exists() {
        std::fs::read_to_string(&config.rbac_policy_csv).unwrap_or_else(|e| {
            warn!(
                error = %e,
                path = %config.rbac_policy_csv,
                "Failed to read RBAC policy file, using default"
            );
            RbacPolicy::default_csv().to_string()
        })
    } else {
        config.rbac_policy_csv.clone()
    };

    match RbacPolicy::from_csv(&csv, &config.rbac_default_policy) {
        Ok(policy) => {
            info!(default_policy = %config.rbac_default_policy, "RBAC policy loaded");
            policy
        }
        Err(e) => {
            error!(error = %e, "Failed to parse RBAC policy, using permissive default");
            RbacPolicy::from_csv(RbacPolicy::default_csv(), &config.rbac_default_policy)
                .expect("default policy must parse")
        }
    }
}

/// Build the notes ConfigMap and tokens Secret stores and start their watches.
///
/// A server that cannot reach the Kubernetes API still serves reports; it just
/// loses notes and Bearer-token auth, and says so.
async fn build_authored_state_stores(
    config: &Config,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> (Option<Arc<NotesStore>>, Option<Arc<TokenStore>>) {
    let (client, namespace) = match crate::kube_env::client_and_namespace().await {
        Ok(pair) => pair,
        Err(e) => {
            warn!(
                error = %e,
                "Notes and API tokens are unavailable (Kubernetes API not reachable)"
            );
            return (None, None);
        }
    };

    let notes = Arc::new(NotesStore::new(
        client.clone(),
        namespace.clone(),
        config.notes_configmap.clone(),
    ));
    if let Err(e) = notes.ensure_exists().await {
        warn!(error = %e, "Failed to pre-create notes ConfigMap; will retry on first write");
    }
    let watcher = notes.clone();
    let rx = shutdown.clone();
    tokio::spawn(async move { watcher.run_watch(rx).await });

    let tokens = Arc::new(TokenStore::new(
        client,
        namespace,
        config.api_tokens_secret.clone(),
    ));
    if let Err(e) = tokens.ensure_exists().await {
        warn!(error = %e, "Failed to pre-create API tokens Secret; will retry on first write");
    }
    let watcher = tokens.clone();
    let rx = shutdown.clone();
    tokio::spawn(async move { watcher.run_watch(rx).await });

    info!(
        notes_configmap = %config.notes_configmap,
        api_tokens_secret = %config.api_tokens_secret,
        "Authored state stores attached"
    );
    (Some(notes), Some(tokens))
}

/// Keep the authored-state gauges current. Both are derived from watch caches,
/// so this is a cheap in-memory read on an interval rather than a query.
fn spawn_gauge_refresh(
    state: AppState,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(GAUGE_REFRESH_SECS));
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = interval.tick() => {
                    state.record_notes_size();
                    state.record_token_count();
                }
            }
        }
    })
}

/// Build the router with conditional auth middleware
pub(crate) fn build_router(
    state: AppState,
    auth_mode: auth::AuthMode,
    mcp_router: Option<Router<AppState>>,
) -> Router {
    // Public routes (never require auth)
    let public_routes = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/reports", post(receive_report))
        .route("/api/v1/auth/me", get(auth::handlers::auth_me))
        .route("/assets/{*path}", get(serve_asset))
        .route("/static/{*path}", get(serve_static));

    // Auth routes (login, callback, logout, error)
    let auth_routes = Router::new()
        .route("/auth/login", get(auth::handlers::login))
        .route("/auth/callback", get(auth::handlers::callback))
        .route("/auth/logout", get(auth::handlers::logout))
        .route("/auth/error", get(auth::handlers::auth_error));

    let protected_routes = Router::new()
        .merge(
            SwaggerUi::new("/swagger-ui")
                .url("/api-docs/openapi.json", ApiDoc::openapi())
                .config(
                    utoipa_swagger_ui::Config::from("/api-docs/openapi.json")
                        .display_request_duration(true)
                        .filter(true)
                        .try_it_out_enabled(true)
                        .deep_linking(true),
                ),
        )
        .route(
            "/api/v1/vulnerabilityreports",
            get(list_vulnerability_reports),
        )
        .route(
            "/api/v1/vulnerabilityreports/vulnerabilities/search",
            get(search_vulnerabilities),
        )
        .route(
            "/api/v1/vulnerabilityreports/vulnerabilities/suggest",
            get(suggest_vulnerabilities),
        )
        .route(
            "/api/v1/vulnerabilityreports/{cluster}/{namespace}/{name}",
            get(get_vulnerability_report),
        )
        .route("/api/v1/sbomreports", get(list_sbom_reports))
        .route(
            "/api/v1/sbomreports/components/search",
            get(search_sbom_components),
        )
        .route(
            "/api/v1/sbomreports/components/suggest",
            get(suggest_sbom_components),
        )
        .route(
            "/api/v1/sbomreports/{cluster}/{namespace}/{name}",
            get(get_sbom_report),
        )
        .route("/api/v1/clusters", get(list_clusters))
        .route("/api/v1/stats", get(get_stats))
        .route("/api/v1/namespaces", get(list_namespaces))
        .route("/api/v1/watcher/status", get(get_watcher_status))
        .route("/api/v1/hydration", get(get_hydration))
        .route("/api/v1/version", get(get_version))
        .route("/api/v1/status", get(get_status))
        .route("/api/v1/config", get(get_config))
        .route("/api/v1/dashboard/trends", get(get_dashboard_trends))
        .route(
            "/api/v1/reports/{cluster}/{report_type}/{namespace}/{name}",
            delete(delete_report),
        )
        .route(
            "/api/v1/reports/{cluster}/{report_type}/{namespace}/{name}/notes",
            put(update_notes),
        )
        .route(
            "/api/v1/auth/tokens",
            get(auth::handlers::list_tokens).post(auth::handlers::create_token),
        )
        .route(
            "/api/v1/auth/tokens/{prefix}",
            delete(auth::handlers::delete_token),
        )
        .route("/api/v1/admin/info", get(admin_handlers::admin_info))
        // Alert rules
        .route(
            "/api/v1/alerts",
            get(alert_handlers::list_alerts).post(alert_handlers::create_alert),
        )
        .route(
            "/api/v1/alerts/preview",
            post(alert_handlers::preview_alert),
        )
        .route(
            "/api/v1/alerts/test",
            post(alert_handlers::test_alert_draft),
        )
        .route(
            "/api/v1/alerts/{name}",
            get(alert_handlers::get_alert)
                .put(alert_handlers::update_alert)
                .delete(alert_handlers::delete_alert),
        )
        // Hub-pull cluster registration
        .route(
            "/api/v1/hub/clusters",
            get(cluster_handlers::list_registered_clusters)
                .post(cluster_handlers::register_cluster),
        )
        .route(
            "/api/v1/hub/clusters/validate",
            post(cluster_handlers::validate_cluster),
        )
        .route(
            "/api/v1/hub/clusters/{name}",
            delete(cluster_handlers::delete_registered_cluster),
        )
        .route("/", get(serve_index))
        .fallback(get(serve_index));

    // MCP sits inside the protected group so require_auth/require_rbac gate it.
    let protected_routes = match mcp_router {
        Some(mcp) => protected_routes.merge(mcp),
        None => protected_routes,
    };

    // Apply auth middleware only when keycloak is enabled.
    // Axum layers execute outer-to-inner, so add RBAC first, then auth.
    let protected_routes = if auth_mode == auth::AuthMode::Keycloak {
        protected_routes
            .layer(axum_middleware::from_fn_with_state(
                state.clone(),
                auth::middleware::require_rbac,
            ))
            .layer(axum_middleware::from_fn_with_state(
                state.clone(),
                auth::middleware::require_auth,
            ))
    } else {
        protected_routes
    };

    Router::new()
        .merge(public_routes)
        .merge(auth_routes)
        .merge(protected_routes)
        .layer(axum_middleware::from_fn_with_state(
            state.clone(),
            logging_middleware::api_request_logger,
        ))
        .with_state(state)
}

async fn serve_index() -> impl IntoResponse {
    match StaticAssets::get("index.html") {
        Some(content) => Html(
            std::str::from_utf8(content.data.as_ref())
                .unwrap_or("")
                .to_string(),
        )
        .into_response(),
        None => (StatusCode::NOT_FOUND, "Not found").into_response(),
    }
}

async fn serve_asset(axum::extract::Path(path): axum::extract::Path<String>) -> impl IntoResponse {
    serve_embedded(&format!("assets/{}", path.trim_start_matches('/')))
}

async fn serve_static(axum::extract::Path(path): axum::extract::Path<String>) -> impl IntoResponse {
    serve_embedded(path.trim_start_matches('/'))
}

/// Serve one embedded asset with a guessed content type.
fn serve_embedded(path: &str) -> axum::response::Response {
    match StaticAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref())],
                content.data.to_vec(),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "Not found").into_response(),
    }
}

/// Shared fixtures for the crate's HTTP tests.
///
/// The server normally runs on a `RemoteStore`; tests run it on an in-memory
/// `Database`, which satisfies the same trait. That keeps handler tests free of
/// both a cluster and a live scraper.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub async fn app_state() -> AppState {
        state_with(
            crate::storage::Database::new(":memory:").await.unwrap(),
            RbacPolicy::default_csv(),
            "role:readonly",
        )
    }

    /// Build state over a specific database and RBAC policy.
    pub fn state_with(
        db: crate::storage::Database,
        rbac_csv: &str,
        default_policy: &str,
    ) -> AppState {
        let store: Arc<dyn ReportStore> = Arc::new(db);
        let mut registry = prometheus_client::registry::Registry::default();
        let metrics = Metrics::new(&mut registry, crate::config::Mode::Server);

        AppState {
            store,
            config: Arc::new(state::ConfigInfo::from(&Config::for_test(
                crate::config::Mode::Server,
            ))),
            runtime: Arc::new(state::RuntimeInfo::new()),
            auth: None,
            rbac: Arc::new(RbacPolicy::from_csv(rbac_csv, default_policy).unwrap()),
            metrics,
            alerts: None,
            notes: None,
            tokens: None,
        }
    }

    pub async fn router_without_auth() -> Router {
        build_router(app_state().await, auth::AuthMode::None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use test_support::router_without_auth;
    use tower::ServiceExt;

    async fn get(uri: &str) -> axum::response::Response {
        router_without_auth()
            .await
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn every_read_route_answers_ok() {
        // One assertion per route, table-driven: the point is that the route is
        // wired and its handler can run, not that each has bespoke coverage.
        for uri in [
            "/api/v1/version",
            "/api/v1/status",
            "/api/v1/config",
            "/api/v1/stats",
            "/api/v1/clusters",
            "/api/v1/namespaces",
            "/api/v1/watcher/status",
            "/api/v1/hydration",
            "/api/v1/vulnerabilityreports",
            "/api/v1/sbomreports",
            "/api/v1/admin/info",
            "/api/v1/dashboard/trends",
            "/api/v1/vulnerabilityreports/vulnerabilities/search?q=CVE-2024",
            "/api/v1/vulnerabilityreports/vulnerabilities/suggest?q=CVE",
            "/api/v1/sbomreports/components/search?component=log4j",
            "/api/v1/sbomreports/components/suggest?q=log",
            "/api/v1/auth/me",
        ] {
            assert_eq!(get(uri).await.status(), StatusCode::OK, "{uri}");
        }
    }

    #[tokio::test]
    async fn the_removed_api_log_routes_are_gone() {
        // Deliberate feature removal: requests are stdout lines now.
        for uri in ["/api/v1/admin/logs", "/api/v1/admin/logs/stats"] {
            // The SPA fallback serves index.html for unknown GET paths, so the
            // proof is that the JSON endpoint no longer answers as JSON.
            let resp = get(uri).await;
            let content_type = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            assert!(
                !content_type.starts_with("application/json"),
                "{uri} still answers as JSON"
            );
        }
    }

    #[tokio::test]
    async fn version_reports_build_information() {
        let resp = get("/api/v1/version").await;
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["version"].is_string());
    }

    #[tokio::test]
    async fn config_exposes_the_scraper_url_not_a_storage_path() {
        let resp = get("/api/v1/config").await;
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("SCRAPER_URL"));
        assert!(!text.contains("STORAGE_PATH"));
    }

    #[tokio::test]
    async fn unknown_assets_are_not_found() {
        for uri in ["/static/nonexistent.js", "/assets/nonexistent.css"] {
            assert_eq!(get(uri).await.status(), StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[tokio::test]
    async fn the_spa_fallback_serves_the_index_or_nothing() {
        for uri in ["/", "/some/unknown/path"] {
            let status = get(uri).await.status();
            assert!(
                status == StatusCode::OK || status == StatusCode::NOT_FOUND,
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn swagger_ui_is_mounted() {
        let status = get("/swagger-ui/").await.status();
        assert!(status.is_success() || status.is_redirection());
    }

    #[tokio::test]
    async fn auth_routes_redirect_when_oidc_is_off() {
        assert!(get("/auth/login").await.status().is_redirection());
        let logout = get("/auth/logout").await.status();
        assert!(logout.is_redirection() || logout.is_success());
        let callback = get("/auth/callback").await.status();
        assert!(callback.is_redirection() || callback.is_client_error());
    }

    #[tokio::test]
    async fn listing_tokens_without_a_session_is_refused() {
        let status = get("/api/v1/auth/tokens").await.status();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_push_ingest_route_forwards_to_the_store() {
        let body = serde_json::json!({
            "event_type": "Apply",
            "payload": {
                "cluster": "test",
                "report_type": "vulnerabilityreport",
                "namespace": "default",
                "name": "test-report",
                "data_json": "{}",
                "received_at": "2024-01-01T00:00:00Z"
            }
        });
        let resp = router_without_auth()
            .await
            .oneshot(
                axum::http::Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/reports")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn writing_notes_without_a_notes_store_is_unavailable() {
        let resp = router_without_auth()
            .await
            .oneshot(
                axum::http::Request::builder()
                    .method(Method::PUT)
                    .uri("/api/v1/reports/prod/sbomreport/default/nginx/notes")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"notes":"hello"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn rbac_falls_back_to_the_default_policy_on_a_malformed_csv() {
        let mut config = Config::for_test(crate::config::Mode::Server);
        config.rbac_policy_csv = "this is not a policy".to_string();
        let policy = load_rbac_policy(&config);
        assert_eq!(policy.default_policy_name(), "role:readonly");
    }

    #[test]
    fn rbac_uses_the_built_in_default_when_unset() {
        let config = Config::for_test(crate::config::Mode::Server);
        let policy = load_rbac_policy(&config);
        assert_eq!(policy.default_policy_name(), "role:readonly");
    }

    #[tokio::test]
    async fn readiness_follows_scraper_reachability() {
        use crate::health::HealthServer;

        // Port 1 is reserved and refuses connections, so this store can never
        // reach a scraper.
        let mut state = test_support::app_state().await;
        state.store = Arc::new(RemoteStore::new("http://127.0.0.1:1", "shh").unwrap());

        let registry = Arc::new(prometheus_client::registry::Registry::default());
        let health = HealthServer::new(registry);
        health.set_ready(true);

        let (tx, rx) = tokio::sync::watch::channel(false);
        let probe = spawn_readiness_probe(state, health.clone(), rx);

        // The probe's first tick fires immediately; give it room to land.
        for _ in 0..50 {
            if !health.is_ready() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            !health.is_ready(),
            "an unreachable scraper must make the server not ready"
        );

        let _ = tx.send(true);
        let _ = probe.await;
    }

    #[tokio::test]
    async fn readiness_reports_ready_against_a_reachable_store() {
        use crate::health::HealthServer;

        // The in-memory Database store is always reachable.
        let state = test_support::app_state().await;
        let registry = Arc::new(prometheus_client::registry::Registry::default());
        let health = HealthServer::new(registry);

        let (tx, rx) = tokio::sync::watch::channel(false);
        let probe = spawn_readiness_probe(state, health.clone(), rx);

        for _ in 0..50 {
            if health.is_ready() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(health.is_ready());

        let _ = tx.send(true);
        let _ = probe.await;
    }

    #[tokio::test]
    async fn auth_state_is_absent_when_auth_is_off() {
        let config = Config::for_test(crate::config::Mode::Server);
        let state = build_auth_state(&config, auth::AuthMode::None)
            .await
            .unwrap();
        assert!(state.is_none());
    }
}
