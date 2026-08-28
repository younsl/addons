//! HTTP request handlers for API endpoints

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use tracing::{debug, error, info};

use crate::collector::types::{ReportEvent, ReportEventType};
use crate::config::env;
use crate::metrics::ReportReceivedLabels;
use crate::storage::{
    ClusterInfo, ComponentSearchResult, FullReport, NotesError, ReportMeta, Stats, TrendResponse,
    VulnSearchResult,
};

use super::state::AppState;
use super::types::{
    ComponentSearchQuery, ComponentSuggestQuery, ConfigItem, ConfigResponse, ErrorResponse,
    HealthResponse, ListQuery, ListResponse, StatusResponse, TrendQuery, UpdateNotesRequest,
    VersionResponse, VulnSearchQuery, VulnSuggestQuery, WatcherStatusResponse,
};

/// Health check endpoint for collectors
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "Health",
    responses(
        (status = 200, description = "Service is healthy", body = HealthResponse)
    )
)]
pub async fn healthz(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    debug!(
        remote_addr = %addr,
        "Health check request received"
    );

    // Read memory usage from /proc/self/statm (Linux only)
    // Format: size resident shared text lib data dt (all in pages)
    // We use the second field (resident) which is RSS in pages
    let memory_mb = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| {
            let parts: Vec<&str> = s.split_whitespace().collect();
            // Second field is RSS (Resident Set Size) in pages
            parts.get(1).and_then(|rss| rss.parse::<u64>().ok())
        })
        .map(|pages| pages * 4096 / 1024 / 1024); // Convert pages to MB (4KB page size)

    (
        StatusCode::OK,
        Json(HealthResponse {
            status: "ok".to_string(),
            memory_mb,
        }),
    )
}

/// Receive a pushed report and forward it to the scraper.
///
/// The server holds no database, so this route is a thin proxy onto the
/// internal ingest endpoint. Any remaining pusher keeps working, and its
/// writes go through the same alert-evaluating path as a watch event.
#[utoipa::path(
    post,
    path = "/api/v1/reports",
    tag = "Reports",
    request_body = ReportEvent,
    responses(
        (status = 200, description = "Report accepted"),
        (status = 502, description = "Scraper unreachable", body = ErrorResponse)
    )
)]
pub async fn receive_report(
    State(state): State<AppState>,
    Json(event): Json<ReportEvent>,
) -> impl IntoResponse {
    let payload = &event.payload;
    debug!(
        cluster = %payload.cluster,
        report_type = %payload.report_type,
        namespace = %payload.namespace,
        name = %payload.name,
        event = ?event.event_type,
        "Forwarding report event to the scraper"
    );

    if let Some(ref counter) = state.metrics.reports_received_total {
        counter
            .get_or_create(&ReportReceivedLabels {
                cluster: payload.cluster.clone(),
                report_type: payload.report_type.clone(),
            })
            .inc();
    }

    let result = match event.event_type {
        ReportEventType::Apply => state.store.upsert_report(payload).await.map(|()| true),
        ReportEventType::Delete => {
            state
                .store
                .delete_report(
                    &payload.cluster,
                    &payload.namespace,
                    &payload.name,
                    &payload.report_type,
                )
                .await
        }
    };

    match result {
        Ok(applied) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "ok", "applied": applied})),
        ),
        Err(e) => {
            error!(error = %e, "Failed to forward report to the scraper");
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        }
    }
}

/// `report_type` values the storage layer stores reports under.
pub const VULNERABILITY_REPORT: &str = "vulnerabilityreport";
pub const SBOM_REPORT: &str = "sbomreport";

/// List one report type, joining stored notes onto the page.
///
/// Both report types answer with the same envelope, so they share one body;
/// the only difference is which `report_type` is queried.
async fn list_reports(state: AppState, query: ListQuery, report_type: &str) -> Response {
    let params = query.to_query_params();

    match state.store.query_reports(report_type, &params).await {
        Ok((mut reports, total)) => {
            state.merge_notes(&mut reports);
            (
                StatusCode::OK,
                Json(ListResponse {
                    items: reports,
                    total: total as usize,
                }),
            )
                .into_response()
        }
        Err(e) => {
            error!(error = %e, report_type = report_type, "Failed to query reports");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ListResponse::<ReportMeta> {
                    items: vec![],
                    total: 0,
                }),
            )
                .into_response()
        }
    }
}

/// Fetch one report with its full data, joining its stored note onto the
/// metadata.
async fn report_detail(
    state: AppState,
    cluster: &str,
    namespace: &str,
    name: &str,
    report_type: &str,
) -> Response {
    match state
        .store
        .get_report(cluster, namespace, name, report_type)
        .await
    {
        Ok(Some(mut report)) => {
            state.merge_note(&mut report.meta);
            Json(report).into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Report not found"})),
        )
            .into_response(),
        Err(e) => {
            error!(error = %e, report_type = report_type, "Failed to get report");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

/// List vulnerability reports
#[utoipa::path(
    get,
    path = "/api/v1/vulnerabilityreports",
    tag = "Vulnerability Reports",
    params(ListQuery),
    responses(
        (status = 200, description = "List of vulnerability reports", body = ListResponse<ReportMeta>),
        (status = 500, description = "Internal server error")
    )
)]
pub async fn list_vulnerability_reports(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> impl IntoResponse {
    list_reports(state, query, VULNERABILITY_REPORT).await
}

/// Get specific vulnerability report
#[utoipa::path(
    get,
    path = "/api/v1/vulnerabilityreports/{cluster}/{namespace}/{name}",
    tag = "Vulnerability Reports",
    params(
        ("cluster" = String, Path, description = "Cluster name"),
        ("namespace" = String, Path, description = "Kubernetes namespace"),
        ("name" = String, Path, description = "Report name")
    ),
    responses(
        (status = 200, description = "Vulnerability report details", body = FullReport),
        (status = 404, description = "Report not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn get_vulnerability_report(
    State(state): State<AppState>,
    Path((cluster, namespace, name)): Path<(String, String, String)>,
) -> impl IntoResponse {
    report_detail(state, &cluster, &namespace, &name, VULNERABILITY_REPORT).await
}

/// Search vulnerabilities across all reports
#[utoipa::path(
    get,
    path = "/api/v1/vulnerabilityreports/vulnerabilities/search",
    tag = "Vulnerability Reports",
    params(VulnSearchQuery),
    responses(
        (status = 200, description = "Vulnerability search results", body = ListResponse<VulnSearchResult>),
        (status = 400, description = "Missing query parameter", body = ErrorResponse),
        (status = 500, description = "Internal server error")
    )
)]
pub async fn search_vulnerabilities(
    State(state): State<AppState>,
    Query(query): Query<VulnSearchQuery>,
) -> impl IntoResponse {
    let q = query.q.trim();
    if q.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "q parameter is required"})),
        );
    }

    let limit = query.limit.unwrap_or(500);
    let offset = query.offset.unwrap_or(0);

    match state.store.search_vulnerabilities(q, limit, offset).await {
        Ok((results, total)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "items": results,
                "total": total,
            })),
        ),
        Err(e) => {
            error!(error = %e, "Failed to search vulnerabilities");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        }
    }
}

/// Suggest vulnerability IDs (autocomplete)
#[utoipa::path(
    get,
    path = "/api/v1/vulnerabilityreports/vulnerabilities/suggest",
    tag = "Vulnerability Reports",
    params(VulnSuggestQuery),
    responses(
        (status = 200, description = "Vulnerability ID suggestions", body = Vec<String>),
        (status = 500, description = "Internal server error")
    )
)]
pub async fn suggest_vulnerabilities(
    State(state): State<AppState>,
    Query(query): Query<VulnSuggestQuery>,
) -> impl IntoResponse {
    let q = query.q.trim();
    if q.is_empty() {
        return (StatusCode::OK, Json(serde_json::json!([])));
    }

    let limit = query.limit.unwrap_or(20);

    match state.store.suggest_vulnerability_ids(q, limit).await {
        Ok(names) => (StatusCode::OK, Json(serde_json::json!(names))),
        Err(e) => {
            error!(error = %e, "Failed to suggest vulnerability IDs");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!([])),
            )
        }
    }
}

/// List SBOM reports
#[utoipa::path(
    get,
    path = "/api/v1/sbomreports",
    tag = "SBOM Reports",
    params(ListQuery),
    responses(
        (status = 200, description = "List of SBOM reports", body = ListResponse<ReportMeta>),
        (status = 500, description = "Internal server error")
    )
)]
pub async fn list_sbom_reports(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> impl IntoResponse {
    list_reports(state, query, SBOM_REPORT).await
}

/// Search SBOM components across all reports
#[utoipa::path(
    get,
    path = "/api/v1/sbomreports/components/search",
    tag = "SBOM Reports",
    params(ComponentSearchQuery),
    responses(
        (status = 200, description = "Component search results", body = ListResponse<ComponentSearchResult>),
        (status = 400, description = "Missing component parameter", body = ErrorResponse),
        (status = 500, description = "Internal server error")
    )
)]
pub async fn search_sbom_components(
    State(state): State<AppState>,
    Query(query): Query<ComponentSearchQuery>,
) -> impl IntoResponse {
    let component = query.component.trim();
    if component.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "component parameter is required"})),
        );
    }

    let limit = query.limit.unwrap_or(500);
    let offset = query.offset.unwrap_or(0);

    match state
        .store
        .search_sbom_components(component, limit, offset)
        .await
    {
        Ok((results, total)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "items": results,
                "total": total,
            })),
        ),
        Err(e) => {
            error!(error = %e, "Failed to search SBOM components");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        }
    }
}

/// Suggest SBOM component names (autocomplete)
#[utoipa::path(
    get,
    path = "/api/v1/sbomreports/components/suggest",
    tag = "SBOM Reports",
    params(ComponentSuggestQuery),
    responses(
        (status = 200, description = "Component name suggestions", body = Vec<String>),
        (status = 400, description = "Missing query parameter"),
        (status = 500, description = "Internal server error")
    )
)]
pub async fn suggest_sbom_components(
    State(state): State<AppState>,
    Query(query): Query<ComponentSuggestQuery>,
) -> impl IntoResponse {
    let q = query.q.trim();
    if q.is_empty() {
        return (StatusCode::OK, Json(serde_json::json!([])));
    }

    let limit = query.limit.unwrap_or(20);

    match state.store.suggest_component_names(q, limit).await {
        Ok(names) => (StatusCode::OK, Json(serde_json::json!(names))),
        Err(e) => {
            error!(error = %e, "Failed to suggest component names");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!([])),
            )
        }
    }
}

/// Get specific SBOM report
#[utoipa::path(
    get,
    path = "/api/v1/sbomreports/{cluster}/{namespace}/{name}",
    tag = "SBOM Reports",
    params(
        ("cluster" = String, Path, description = "Cluster name"),
        ("namespace" = String, Path, description = "Kubernetes namespace"),
        ("name" = String, Path, description = "Report name")
    ),
    responses(
        (status = 200, description = "SBOM report details", body = FullReport),
        (status = 404, description = "Report not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn get_sbom_report(
    State(state): State<AppState>,
    Path((cluster, namespace, name)): Path<(String, String, String)>,
) -> impl IntoResponse {
    report_detail(state, &cluster, &namespace, &name, SBOM_REPORT).await
}

/// List clusters
#[utoipa::path(
    get,
    path = "/api/v1/clusters",
    tag = "Clusters",
    responses(
        (status = 200, description = "List of clusters", body = ListResponse<ClusterInfo>),
        (status = 500, description = "Internal server error")
    )
)]
pub async fn list_clusters(State(state): State<AppState>) -> impl IntoResponse {
    match state.store.list_clusters().await {
        Ok(clusters) => {
            let total = clusters.len();
            (
                StatusCode::OK,
                Json(ListResponse {
                    items: clusters,
                    total,
                }),
            )
        }
        Err(e) => {
            error!(error = %e, "Failed to list clusters");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ListResponse {
                    items: vec![],
                    total: 0,
                }),
            )
        }
    }
}

/// Get statistics
#[utoipa::path(
    get,
    path = "/api/v1/stats",
    tag = "Statistics",
    responses(
        (status = 200, description = "Overall statistics", body = Stats),
        (status = 500, description = "Internal server error")
    )
)]
pub async fn get_stats(State(state): State<AppState>) -> impl IntoResponse {
    match state.store.get_stats().await {
        Ok(stats) => (StatusCode::OK, Json(stats)),
        Err(e) => {
            error!(error = %e, "Failed to get stats");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(Stats {
                    total_clusters: 0,
                    total_vuln_reports: 0,
                    total_sbom_reports: 0,
                    total_critical: 0,
                    total_high: 0,
                    total_medium: 0,
                    total_low: 0,
                    total_unknown: 0,
                    db_size_bytes: 0,
                    db_size_human: "0 B".to_string(),
                    sqlite_version: "unknown".to_string(),
                }),
            )
        }
    }
}

/// List namespaces
#[utoipa::path(
    get,
    path = "/api/v1/namespaces",
    tag = "Namespaces",
    params(
        ("cluster" = Option<String>, Query, description = "Filter by cluster name")
    ),
    responses(
        (status = 200, description = "List of namespaces", body = ListResponse<String>),
        (status = 500, description = "Internal server error")
    )
)]
pub async fn list_namespaces(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> impl IntoResponse {
    match state.store.list_namespaces(query.cluster.as_deref()).await {
        Ok(namespaces) => {
            let total = namespaces.len();
            (
                StatusCode::OK,
                Json(ListResponse {
                    items: namespaces,
                    total,
                }),
            )
        }
        Err(e) => {
            error!(error = %e, "Failed to list namespaces");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ListResponse {
                    items: vec![],
                    total: 0,
                }),
            )
        }
    }
}

/// Delete report
#[utoipa::path(
    delete,
    path = "/api/v1/reports/{cluster}/{report_type}/{namespace}/{name}",
    tag = "Reports",
    params(
        ("cluster" = String, Path, description = "Cluster name"),
        ("report_type" = String, Path, description = "Report type (vulnerabilityreport or sbomreport)"),
        ("namespace" = String, Path, description = "Kubernetes namespace"),
        ("name" = String, Path, description = "Report name")
    ),
    responses(
        (status = 200, description = "Report deleted successfully"),
        (status = 404, description = "Report not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn delete_report(
    State(state): State<AppState>,
    Path((cluster, report_type, namespace, name)): Path<(String, String, String, String)>,
) -> impl IntoResponse {
    match state
        .store
        .delete_report(&cluster, &namespace, &name, &report_type)
        .await
    {
        Ok(deleted) => {
            if deleted {
                (
                    StatusCode::OK,
                    Json(serde_json::json!({"status": "deleted"})),
                )
            } else {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "Report not found"})),
                )
            }
        }
        Err(e) => {
            error!(error = %e, "Failed to delete report");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        }
    }
}

/// Update report notes
///
/// Notes live in a ConfigMap rather than alongside the report row: the report
/// is a regenerable mirror, the note is authored and irreplaceable.
#[utoipa::path(
    put,
    path = "/api/v1/reports/{cluster}/{report_type}/{namespace}/{name}/notes",
    tag = "Reports",
    params(
        ("cluster" = String, Path, description = "Cluster name"),
        ("report_type" = String, Path, description = "Report type (vulnerabilityreport or sbomreport)"),
        ("namespace" = String, Path, description = "Kubernetes namespace"),
        ("name" = String, Path, description = "Report name")
    ),
    request_body = UpdateNotesRequest,
    responses(
        (status = 200, description = "Notes updated successfully"),
        (status = 404, description = "Report not found", body = ErrorResponse),
        (status = 413, description = "Note too large, or the notes store is full", body = ErrorResponse),
        (status = 503, description = "Notes store unavailable", body = ErrorResponse),
    )
)]
pub async fn update_notes(
    State(state): State<AppState>,
    Path((cluster, report_type, namespace, name)): Path<(String, String, String, String)>,
    Json(request): Json<UpdateNotesRequest>,
) -> impl IntoResponse {
    let Some(notes) = state.notes.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "Notes are unavailable (Kubernetes API not reachable)"
            })),
        )
            .into_response();
    };

    // A note on a report nobody has is a note nobody will ever see again.
    match state
        .store
        .get_report(&cluster, &namespace, &name, &report_type)
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Report not found"})),
            )
                .into_response();
        }
        Err(e) => {
            error!(error = %e, "Failed to verify report before writing notes");
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    }

    match notes
        .upsert(&cluster, &report_type, &namespace, &name, &request.notes)
        .await
    {
        Ok(()) => {
            info!(
                cluster = %cluster,
                report_type = %report_type,
                namespace = %namespace,
                name = %name,
                "Report notes updated"
            );
            state.record_notes_size();
            (
                StatusCode::OK,
                Json(serde_json::json!({"status": "updated"})),
            )
                .into_response()
        }
        Err(e @ (NotesError::NoteTooLarge | NotesError::StoreFull)) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
        Err(e) => {
            error!(error = %e, "Failed to update notes");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

/// Get watcher status
///
/// Watchers run in the scraper pod, so this reports the scraper's per-cluster
/// hydration state, flattened to the fleet-wide shape the UI has always read.
#[utoipa::path(
    get,
    path = "/api/v1/watcher/status",
    tag = "Watcher",
    responses(
        (status = 200, description = "Watcher status", body = WatcherStatusResponse)
    )
)]
pub async fn get_watcher_status(State(state): State<AppState>) -> impl IntoResponse {
    let hydration = state.store.hydration().await.unwrap_or_default();
    (
        StatusCode::OK,
        Json(WatcherStatusResponse::from(&hydration)),
    )
}

/// Get fleet hydration status
///
/// The report set is legitimately incomplete between scraper start and the
/// last `InitDone`, because the database now starts empty on every restart.
/// The UI reads this to render a rebuilding banner instead of empty tables.
#[utoipa::path(
    get,
    path = "/api/v1/hydration",
    tag = "Watcher",
    responses(
        (status = 200, description = "Per-cluster hydration status"),
        (status = 502, description = "Scraper unreachable", body = ErrorResponse)
    )
)]
pub async fn get_hydration(State(state): State<AppState>) -> impl IntoResponse {
    match state.store.hydration().await {
        Ok(status) => (StatusCode::OK, Json(serde_json::json!(status))),
        Err(e) => {
            error!(error = %e, "Failed to read hydration status");
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        }
    }
}

/// Get version info (build-time information)
#[utoipa::path(
    get,
    path = "/api/v1/version",
    tag = "Version",
    responses(
        (status = 200, description = "Version information", body = VersionResponse)
    )
)]
pub async fn get_version() -> impl IntoResponse {
    let version = VersionResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        commit: env!("VERGEN_GIT_SHA").to_string(),
        build_date: env!("VERGEN_BUILD_TIMESTAMP").to_string(),
        rust_version: env!("VERGEN_RUSTC_SEMVER").to_string(),
        rust_channel: env!("VERGEN_RUSTC_CHANNEL").to_string(),
        platform: env!("VERGEN_CARGO_TARGET_TRIPLE").to_string(),
        llvm_version: option_env!("VERGEN_RUSTC_LLVM_VERSION")
            .unwrap_or("unknown")
            .to_string(),
    };
    (StatusCode::OK, Json(version))
}

/// Get server status (runtime information)
#[utoipa::path(
    get,
    path = "/api/v1/status",
    tag = "Status",
    responses(
        (status = 200, description = "Server status", body = StatusResponse)
    )
)]
pub async fn get_status(State(state): State<AppState>) -> impl IntoResponse {
    let collectors = state
        .store
        .list_clusters()
        .await
        .map(|c| c.len() as i64)
        .unwrap_or(0);

    let status = StatusResponse {
        hostname: state.runtime.hostname.clone(),
        uptime: state.runtime.uptime_string(),
        collectors,
    };
    (StatusCode::OK, Json(status))
}

/// Get config info
#[utoipa::path(
    get,
    path = "/api/v1/config",
    tag = "Config",
    responses(
        (status = 200, description = "Configuration information", body = ConfigResponse)
    )
)]
pub async fn get_config(State(state): State<AppState>) -> impl IntoResponse {
    let c = &state.config;

    // Helper to format namespaces
    let namespaces_str = if c.namespaces.is_empty() {
        "all".to_string()
    } else {
        c.namespaces.join(", ")
    };

    // Build config items list - easy to extend by adding new entries
    // Use ConfigItem::public() for normal values
    // Use ConfigItem::sensitive() for values that should be masked (e.g., API keys, passwords)
    // ENV names are defined in crate::config::env module (single source of truth)
    let auth_mode_str = c.auth_mode.as_deref().unwrap_or("none");

    let items = vec![
        ConfigItem::public(env::MODE, &c.mode),
        ConfigItem::public(env::CLUSTER_NAME, &c.cluster_name),
        ConfigItem::public(env::NAMESPACES, &namespaces_str),
        ConfigItem::public(env::SERVER_PORT, c.server_port),
        ConfigItem::public(env::HEALTH_PORT, c.health_port),
        ConfigItem::public(env::SCRAPER_URL, &c.scraper_url),
        ConfigItem::public(env::LOG_LEVEL, &c.log_level),
        ConfigItem::public(env::LOG_FORMAT, &c.log_format),
        ConfigItem::public(env::WATCH_LOCAL, c.watch_local),
        ConfigItem::public(env::COLLECT_VULN, c.collect_vulnerability_reports),
        ConfigItem::public(env::COLLECT_SBOM, c.collect_sbom_reports),
        ConfigItem::public(env::AUTH_MODE, auth_mode_str),
        ConfigItem::public(env::MCP_ENABLED, c.mcp_enabled),
    ];

    (StatusCode::OK, Json(ConfigResponse { items }))
}

/// Get dashboard trend data
#[utoipa::path(
    get,
    path = "/api/v1/dashboard/trends",
    tag = "Dashboard",
    params(TrendQuery),
    responses(
        (status = 200, description = "Trend data for dashboard", body = TrendResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn get_dashboard_trends(
    State(state): State<AppState>,
    Query(query): Query<TrendQuery>,
) -> impl IntoResponse {
    let (start_date, end_date) = query.parse_range();
    let granularity = query.get_granularity();

    // Get data range for metadata from reports table (matches live trends data source)
    let (actual_from, actual_to) = state
        .store
        .get_reports_data_range()
        .await
        .unwrap_or((None, None));

    debug!(
        start = %start_date,
        end = %end_date,
        cluster = ?query.cluster,
        granularity = %granularity,
        "Dashboard trends requested"
    );

    // Always use live trends for consistent point-in-time (cumulative) values
    debug!("Using live trends for {} granularity", granularity);
    match state
        .store
        .get_live_trends(
            &start_date,
            &end_date,
            query.cluster.as_deref(),
            granularity,
        )
        .await
    {
        Ok(mut live_trends) => {
            live_trends.meta.data_from = actual_from;
            live_trends.meta.data_to = actual_to;
            (StatusCode::OK, Json(live_trends))
        }
        Err(e) => {
            error!(error = %e, "Failed to get live trends");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(TrendResponse {
                    meta: crate::storage::TrendMeta {
                        range_start: start_date,
                        range_end: end_date,
                        granularity: granularity.to_string(),
                        clusters: vec![],
                        data_from: actual_from,
                        data_to: actual_to,
                    },
                    series: vec![],
                }),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::{delete, get, post, put};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::collector::types::{ReportEvent, ReportEventType, ReportPayload};
    use crate::web::test_support;

    async fn create_test_state() -> AppState {
        test_support::app_state().await
    }

    fn create_test_router(state: AppState) -> Router {
        Router::new()
            .route("/api/v1/reports", post(receive_report))
            .route(
                "/api/v1/vulnerabilityreports",
                get(list_vulnerability_reports),
            )
            .route(
                "/api/v1/vulnerabilityreports/{cluster}/{namespace}/{name}",
                get(get_vulnerability_report),
            )
            .route("/api/v1/sbomreports", get(list_sbom_reports))
            .route(
                "/api/v1/sbomreports/{cluster}/{namespace}/{name}",
                get(get_sbom_report),
            )
            .route("/api/v1/clusters", get(list_clusters))
            .route("/api/v1/stats", get(get_stats))
            .route("/api/v1/namespaces", get(list_namespaces))
            .route("/api/v1/watcher/status", get(get_watcher_status))
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
            .with_state(state)
    }

    fn create_test_payload(
        cluster: &str,
        namespace: &str,
        name: &str,
        report_type: &str,
    ) -> ReportPayload {
        ReportPayload {
            cluster: cluster.to_string(),
            namespace: namespace.to_string(),
            name: name.to_string(),
            report_type: report_type.to_string(),
            data_json: serde_json::json!({
                "metadata": {
                    "labels": {
                        "trivy-operator.resource.name": "test-app"
                    }
                },
                "report": {
                    "artifact": {
                        "repository": "nginx",
                        "tag": "1.25"
                    },
                    "registry": {
                        "server": "docker.io"
                    },
                    "summary": {
                        "criticalCount": 2,
                        "highCount": 5,
                        "mediumCount": 10,
                        "lowCount": 3,
                        "unknownCount": 1,
                        "componentsCount": 50
                    }
                }
            })
            .to_string(),
            received_at: chrono::Utc::now(),
        }
    }

    /// Seed 3 vulnerability reports + 1 SBOM report across 2 clusters
    async fn seed_test_data(state: &AppState) {
        state
            .store
            .upsert_report(&create_test_payload(
                "prod",
                "default",
                "nginx-vuln",
                "vulnerabilityreport",
            ))
            .await
            .unwrap();
        state
            .store
            .upsert_report(&create_test_payload(
                "prod",
                "kube-system",
                "coredns-vuln",
                "vulnerabilityreport",
            ))
            .await
            .unwrap();
        state
            .store
            .upsert_report(&create_test_payload(
                "staging",
                "default",
                "redis-vuln",
                "vulnerabilityreport",
            ))
            .await
            .unwrap();
        state
            .store
            .upsert_report(&create_test_payload(
                "prod",
                "default",
                "nginx-sbom",
                "sbomreport",
            ))
            .await
            .unwrap();
    }

    async fn response_json(response: axum::http::Response<axum::body::Body>) -> serde_json::Value {
        let body = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&body).unwrap()
    }

    // ===== receive_report =====

    #[tokio::test]
    async fn test_receive_report_apply() {
        let state = create_test_state().await;
        let app = create_test_router(state.clone());

        let event = ReportEvent {
            event_type: ReportEventType::Apply,
            payload: create_test_payload("prod", "default", "app1", "vulnerabilityreport"),
        };

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/v1/reports")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_string(&event).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["status"], "ok");

        let report = state
            .store
            .get_report("prod", "default", "app1", "vulnerabilityreport")
            .await
            .unwrap();
        assert!(report.is_some());
    }

    #[tokio::test]
    async fn test_receive_report_delete() {
        let state = create_test_state().await;
        state
            .store
            .upsert_report(&create_test_payload(
                "prod",
                "default",
                "app1",
                "vulnerabilityreport",
            ))
            .await
            .unwrap();
        let app = create_test_router(state);

        let event = ReportEvent {
            event_type: ReportEventType::Delete,
            payload: create_test_payload("prod", "default", "app1", "vulnerabilityreport"),
        };

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/v1/reports")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_string(&event).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["applied"], true);
    }

    #[tokio::test]
    async fn test_receive_report_delete_nonexistent() {
        let state = create_test_state().await;
        let app = create_test_router(state);

        let event = ReportEvent {
            event_type: ReportEventType::Delete,
            payload: create_test_payload("prod", "default", "nonexistent", "vulnerabilityreport"),
        };

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/v1/reports")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_string(&event).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["applied"], false);
    }

    // ===== list_vulnerability_reports =====

    #[tokio::test]
    async fn test_list_vulnerability_reports_empty() {
        let state = create_test_state().await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/vulnerabilityreports")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["total"], 0);
        assert!(json["items"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_list_vulnerability_reports_with_data() {
        let state = create_test_state().await;
        seed_test_data(&state).await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/vulnerabilityreports")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["total"], 3);
    }

    #[tokio::test]
    async fn test_list_vulnerability_reports_with_cluster_filter() {
        let state = create_test_state().await;
        seed_test_data(&state).await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/vulnerabilityreports?cluster=prod")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["total"], 2);
    }

    // ===== get_vulnerability_report =====

    #[tokio::test]
    async fn test_get_vulnerability_report_found() {
        let state = create_test_state().await;
        seed_test_data(&state).await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/vulnerabilityreports/prod/default/nginx-vuln")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert!(json["meta"].is_object());
        assert!(json["data"].is_object());
        assert_eq!(json["meta"]["cluster"], "prod");
    }

    #[tokio::test]
    async fn test_get_vulnerability_report_not_found() {
        let state = create_test_state().await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/vulnerabilityreports/prod/default/nonexistent")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // ===== list_sbom_reports =====

    #[tokio::test]
    async fn test_list_sbom_reports_empty() {
        let state = create_test_state().await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/sbomreports")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["total"], 0);
    }

    #[tokio::test]
    async fn test_list_sbom_reports_with_data() {
        let state = create_test_state().await;
        seed_test_data(&state).await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/sbomreports")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["total"], 1);
    }

    // ===== get_sbom_report =====

    #[tokio::test]
    async fn test_get_sbom_report_found() {
        let state = create_test_state().await;
        seed_test_data(&state).await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/sbomreports/prod/default/nginx-sbom")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert!(json["meta"].is_object());
        assert!(json["data"].is_object());
    }

    #[tokio::test]
    async fn test_get_sbom_report_not_found() {
        let state = create_test_state().await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/sbomreports/prod/default/nonexistent")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // ===== list_clusters =====

    #[tokio::test]
    async fn test_list_clusters_empty() {
        let state = create_test_state().await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/clusters")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["total"], 0);
    }

    #[tokio::test]
    async fn test_list_clusters_with_data() {
        let state = create_test_state().await;
        seed_test_data(&state).await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/clusters")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["total"], 2);
    }

    // ===== get_stats =====

    #[tokio::test]
    async fn test_get_stats_empty() {
        let state = create_test_state().await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/stats")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["total_clusters"], 0);
        assert_eq!(json["total_vuln_reports"], 0);
        assert_eq!(json["total_sbom_reports"], 0);
        assert_eq!(json["total_critical"], 0);
    }

    #[tokio::test]
    async fn test_get_stats_with_data() {
        let state = create_test_state().await;
        seed_test_data(&state).await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/stats")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["total_clusters"], 2);
        assert_eq!(json["total_vuln_reports"], 3);
        assert_eq!(json["total_sbom_reports"], 1);
        assert_eq!(json["total_critical"], 6);
        assert_eq!(json["total_high"], 15);
    }

    // ===== list_namespaces =====

    #[tokio::test]
    async fn test_list_namespaces_all() {
        let state = create_test_state().await;
        seed_test_data(&state).await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/namespaces")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["total"], 2);
    }

    #[tokio::test]
    async fn test_list_namespaces_with_cluster_filter() {
        let state = create_test_state().await;
        seed_test_data(&state).await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/namespaces?cluster=staging")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["total"], 1);
    }

    // ===== delete_report =====

    #[tokio::test]
    async fn test_delete_report_found() {
        let state = create_test_state().await;
        seed_test_data(&state).await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/reports/prod/vulnerabilityreport/default/nginx-vuln")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["status"], "deleted");
    }

    #[tokio::test]
    async fn test_delete_report_not_found() {
        let state = create_test_state().await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/reports/prod/vulnerabilityreport/default/nonexistent")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // ===== update_notes =====

    #[tokio::test]
    async fn notes_are_unavailable_without_a_configmap_store() {
        // Notes live in a ConfigMap now, so a server that cannot reach the
        // Kubernetes API says so instead of accepting a write it cannot keep.
        let state = create_test_state().await;
        seed_test_data(&state).await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/api/v1/reports/prod/vulnerabilityreport/default/nginx-vuln/notes")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"notes":"Reviewed"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // ===== get_watcher_status =====

    #[tokio::test]
    async fn test_get_watcher_status_default() {
        let state = create_test_state().await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/watcher/status")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["vuln_watcher"]["running"], false);
        assert_eq!(json["sbom_watcher"]["running"], false);
        assert_eq!(json["vuln_watcher"]["initial_sync_done"], false);
        assert_eq!(json["sbom_watcher"]["initial_sync_done"], false);
    }

    #[tokio::test]
    async fn watcher_status_reports_the_scrapers_hydration() {
        // A direct-Database store is its own source of truth, so it reports a
        // hydrated fleet with no clusters — flattened to "nothing running".
        let state = create_test_state().await;
        let hydration = state.store.hydration().await.unwrap();
        let flattened = WatcherStatusResponse::from(&hydration);

        assert!(hydration.hydrated);
        assert!(!flattened.vuln_watcher.running);
        assert!(!flattened.sbom_watcher.initial_sync_done);
    }

    #[test]
    fn watcher_status_flattens_a_partially_synced_fleet() {
        use crate::storage::{ClusterSync, HydrationStatus};

        let mut hydration = HydrationStatus::default();
        hydration.clusters.insert(
            "prod".to_string(),
            ClusterSync {
                vuln_watcher_running: true,
                sbom_watcher_running: true,
                vuln_initial_sync_done: true,
                sbom_initial_sync_done: true,
            },
        );
        hydration.clusters.insert(
            "stage".to_string(),
            ClusterSync {
                vuln_watcher_running: true,
                sbom_watcher_running: true,
                vuln_initial_sync_done: true,
                sbom_initial_sync_done: false,
            },
        );

        let flattened = WatcherStatusResponse::from(&hydration);
        // Running anywhere is running; synced only once synced everywhere.
        assert!(flattened.vuln_watcher.running);
        assert!(flattened.vuln_watcher.initial_sync_done);
        assert!(flattened.sbom_watcher.running);
        assert!(!flattened.sbom_watcher.initial_sync_done);
    }

    // ===== get_version =====

    #[tokio::test]
    async fn test_get_version() {
        let state = create_test_state().await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/version")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert!(!json["version"].as_str().unwrap().is_empty());
        assert!(!json["commit"].as_str().unwrap().is_empty());
        assert!(!json["platform"].as_str().unwrap().is_empty());
    }

    // ===== get_status =====

    #[tokio::test]
    async fn test_get_status() {
        let state = create_test_state().await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/status")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert!(json["hostname"].is_string());
        assert!(json["uptime"].is_string());
    }

    // ===== get_config =====

    #[tokio::test]
    async fn test_get_config() {
        let state = create_test_state().await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/config")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        let items = json["items"].as_array().unwrap();
        assert!(!items.is_empty());

        let env_names: Vec<&str> = items.iter().map(|i| i["env"].as_str().unwrap()).collect();
        assert!(env_names.contains(&"MODE"));
        assert!(env_names.contains(&"CLUSTER_NAME"));
    }

    // ===== get_dashboard_trends =====

    #[tokio::test]
    async fn test_get_dashboard_trends_empty() {
        let state = create_test_state().await;
        let app = create_test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/dashboard/trends?range=7d")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert!(json["meta"].is_object());
        assert!(json["series"].is_array());
    }
}
