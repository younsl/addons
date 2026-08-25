//! MCP tool definitions backed by the shared SQLite report store.
//!
//! All tools are read-only. Each one performs a per-tool RBAC check using the
//! same resource/action names as the REST endpoint it mirrors, then calls the
//! storage layer directly (no HTTP hop). Responses are compact JSON text with
//! explicit `total` and `truncated` fields so an agent knows when to page.

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use axum::http::request::Parts;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorCode, Extensions, Implementation, ServerCapabilities,
    ServerInfo,
};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Semaphore;
use tracing::warn;

use super::authz;
use super::params::*;
use crate::metrics::{McpToolDurationLabels, McpToolLabels};
use crate::storage::{ApiLogEntry, QueryParams};
use crate::web::AppState;

const INSTRUCTIONS: &str = "Read-only access to Trivy Operator VulnerabilityReports and SbomReports \
collected from multiple Kubernetes clusters. Start with list_clusters or get_stats to orient, \
then list_* to find reports and get_* for details. Every list result includes `total`; \
increase `offset` to page. Results are capped per call, so narrow filters instead of raising `limit`.";

/// Endpoint-wide cap on concurrently executing tool calls.
///
/// Shared by every session so an agent fanning out dozens of parallel calls
/// cannot starve the SQLite pool the UI depends on. `None` means unlimited.
#[derive(Clone)]
pub struct ToolLimiter(Option<Arc<Semaphore>>);

impl ToolLimiter {
    /// `0` disables the limit.
    pub fn new(max_concurrency: usize) -> Self {
        Self((max_concurrency > 0).then(|| Arc::new(Semaphore::new(max_concurrency))))
    }

    pub fn unlimited() -> Self {
        Self(None)
    }

    /// Slots currently free, or `None` when unlimited.
    pub fn available(&self) -> Option<usize> {
        self.0.as_ref().map(|s| s.available_permits())
    }

    async fn acquire(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        match &self.0 {
            // The semaphore is never closed, so acquire cannot fail.
            Some(s) => s.clone().acquire_owned().await.ok(),
            None => None,
        }
    }
}

/// MCP server handler. Cheap to clone: holds only `Arc`s.
#[derive(Clone)]
pub struct TrivyMcp {
    state: AppState,
    limiter: ToolLimiter,
    tool_router: ToolRouter<Self>,
}

impl TrivyMcp {
    pub fn new(state: AppState, limiter: ToolLimiter) -> Self {
        Self {
            state,
            limiter,
            tool_router: Self::tool_router(),
        }
    }

    fn authorize(&self, ext: &Extensions, resource: &str, action: &str) -> Result<(), McpError> {
        authz::require(ext, &self.state.rbac, resource, action)
    }

    /// Run one tool body under the shared concurrency limit, then record
    /// Prometheus metrics and an API audit log row. The audit row uses method
    /// `MCP` and path `/mcp/tools/call/<tool>` so it shows up next to REST
    /// traffic in the Admin console and can be filtered by prefix.
    async fn observed<F>(
        &self,
        ext: &Extensions,
        tool: &'static str,
        body: F,
    ) -> Result<CallToolResult, McpError>
    where
        F: Future<Output = Result<CallToolResult, McpError>>,
    {
        let start = Instant::now();
        let _permit = self.limiter.acquire().await;

        if let Some(ref g) = self.state.metrics.mcp_tool_calls_in_flight {
            g.inc();
        }
        let result = body.await;
        if let Some(ref g) = self.state.metrics.mcp_tool_calls_in_flight {
            g.dec();
        }

        let elapsed = start.elapsed();
        let outcome = match &result {
            Ok(_) => "success",
            Err(e) => error_class(e),
        };

        if let Some(ref c) = self.state.metrics.mcp_tool_calls_total {
            c.get_or_create(&McpToolLabels {
                tool: tool.to_string(),
                result: outcome.to_string(),
            })
            .inc();
        }
        if let Some(ref h) = self.state.metrics.mcp_tool_duration_seconds {
            h.get_or_create(&McpToolDurationLabels {
                tool: tool.to_string(),
            })
            .observe(elapsed.as_secs_f64());
        }

        let entry = audit_entry(ext, tool, &result, elapsed.as_millis() as u64);
        let db = self.state.db.clone();
        tokio::spawn(async move {
            if let Err(e) = db.insert_api_log(&entry).await {
                warn!(error = %e, "Failed to log MCP tool call");
            }
        });

        result
    }

    fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![ContentBlock::json(value)?]))
    }

    fn db_error(e: anyhow::Error) -> McpError {
        McpError::internal_error(format!("database error: {e}"), None)
    }

    fn not_found(kind: &str, cluster: &str, namespace: &str, name: &str) -> McpError {
        McpError::resource_not_found(
            format!("{kind} {cluster}/{namespace}/{name} not found"),
            None,
        )
    }
}

/// Uniform paged envelope for list-style tools.
#[derive(Serialize)]
struct Page<T: Serialize> {
    items: Vec<T>,
    total: i64,
    limit: i64,
    offset: i64,
    truncated: bool,
}

impl<T: Serialize> Page<T> {
    fn new(items: Vec<T>, total: i64, limit: i64, offset: i64) -> Self {
        let truncated = offset + (items.len() as i64) < total;
        Self {
            items,
            total,
            limit,
            offset,
            truncated,
        }
    }
}

/// Parse the stored report JSON and return the array at `pointer`, or an
/// empty vector when the path is absent.
fn json_array(data_json: &str, pointer: &str) -> Result<Vec<Value>, McpError> {
    let data: Value = serde_json::from_str(data_json)
        .map_err(|e| McpError::internal_error(format!("stored report is not JSON: {e}"), None))?;
    Ok(data
        .pointer(pointer)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Bucket a tool error for the `result` metric label. Cardinality stays fixed.
fn error_class(e: &McpError) -> &'static str {
    match e.code {
        ErrorCode::INVALID_REQUEST => "denied",
        ErrorCode::INVALID_PARAMS => "invalid_params",
        ErrorCode::RESOURCE_NOT_FOUND => "not_found",
        _ => "error",
    }
}

/// HTTP-style status for the audit log so MCP rows sort with REST rows.
fn status_for(result: &Result<CallToolResult, McpError>) -> u16 {
    match result {
        Ok(_) => 200,
        Err(e) => match e.code {
            ErrorCode::INVALID_REQUEST => 403,
            ErrorCode::INVALID_PARAMS => 400,
            ErrorCode::RESOURCE_NOT_FOUND => 404,
            _ => 500,
        },
    }
}

fn audit_entry(
    ext: &Extensions,
    tool: &str,
    result: &Result<CallToolResult, McpError>,
    duration_ms: u64,
) -> ApiLogEntry {
    let parts = ext.get::<Parts>();
    let header = |name: &str| -> String {
        parts
            .and_then(|p| p.headers.get(name))
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let (user_sub, user_email) = authz::session_from(ext)
        .map(|s| (s.sub, s.email.unwrap_or_default()))
        .unwrap_or_default();
    ApiLogEntry {
        id: None,
        method: "MCP".to_string(),
        path: format!("{}/tools/call/{tool}", super::MCP_PATH),
        status_code: status_for(result),
        duration_ms,
        user_sub,
        user_email,
        remote_addr: header("x-forwarded-for"),
        user_agent: header("user-agent"),
        created_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    }
}

#[tool_router]
impl TrivyMcp {
    #[tool(
        description = "List registered clusters with their report counts and last-seen time.",
        annotations(read_only_hint = true)
    )]
    async fn list_clusters(&self, ext: Extensions) -> Result<CallToolResult, McpError> {
        self.observed(&ext, "list_clusters", async {
            self.authorize(&ext, "clusters", "get")?;
            let clusters = self
                .state
                .db
                .list_clusters()
                .await
                .map_err(Self::db_error)?;
            Self::json_result(&serde_json::json!({ "clusters": clusters, "total": clusters.len() }))
        })
        .await
    }

    #[tool(
        description = "List namespaces that have at least one report, optionally scoped to a cluster.",
        annotations(read_only_hint = true)
    )]
    async fn list_namespaces(
        &self,
        ext: Extensions,
        Parameters(p): Parameters<ListNamespacesParams>,
    ) -> Result<CallToolResult, McpError> {
        self.observed(&ext, "list_namespaces", async {
            self.authorize(&ext, "clusters", "get")?;
            let namespaces = self
                .state
                .db
                .list_namespaces(p.cluster.as_deref())
                .await
                .map_err(Self::db_error)?;
            Self::json_result(
                &serde_json::json!({ "namespaces": namespaces, "total": namespaces.len() }),
            )
        })
        .await
    }

    #[tool(
        description = "Fleet-wide totals: clusters, report counts, and vulnerability counts by severity.",
        annotations(read_only_hint = true)
    )]
    async fn get_stats(&self, ext: Extensions) -> Result<CallToolResult, McpError> {
        self.observed(&ext, "get_stats", async {
            self.authorize(&ext, "stats", "get")?;
            let stats = self.state.db.get_stats().await.map_err(Self::db_error)?;
            Self::json_result(&stats)
        })
        .await
    }

    #[tool(
        description = "List VulnerabilityReports (one per container image) with severity summaries. \
Filter by cluster, namespace, app, image, or severity. Paged; check `total` and `truncated`.",
        annotations(read_only_hint = true)
    )]
    async fn list_vulnerability_reports(
        &self,
        ext: Extensions,
        Parameters(p): Parameters<ListReportsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.observed(&ext, "list_vulnerability_reports", async {
            self.authorize(&ext, "reports", "get")?;
            self.list_reports("vulnerabilityreport", p).await
        })
        .await
    }

    #[tool(
        description = "List SbomReports (one per container image) with component counts. \
Filter by cluster, namespace, app, image, or component name. Paged; check `total` and `truncated`.",
        annotations(read_only_hint = true)
    )]
    async fn list_sbom_reports(
        &self,
        ext: Extensions,
        Parameters(p): Parameters<ListReportsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.observed(&ext, "list_sbom_reports", async {
            self.authorize(&ext, "reports", "get")?;
            self.list_reports("sbomreport", p).await
        })
        .await
    }

    #[tool(
        description = "Get one VulnerabilityReport: metadata, severity summary, and a paged list of \
findings (id, severity, score, package, installed/fixed version, title). Filter by severity or fixed_only.",
        annotations(read_only_hint = true)
    )]
    async fn get_vulnerability_report(
        &self,
        ext: Extensions,
        Parameters(p): Parameters<GetVulnerabilityReportParams>,
    ) -> Result<CallToolResult, McpError> {
        self.observed(&ext, "get_vulnerability_report", async {
            self.authorize(&ext, "reports", "get")?;
            let report = self
                .state
                .db
                .get_report(&p.cluster, &p.namespace, &p.name, "vulnerabilityreport")
                .await
                .map_err(Self::db_error)?
                .ok_or_else(|| {
                    Self::not_found("VulnerabilityReport", &p.cluster, &p.namespace, &p.name)
                })?;

            let severities = normalize_severities(p.severity);
            let fixed_only = p.fixed_only.unwrap_or(false);
            let limit = clamp_limit(p.limit, MAX_ITEM_LIMIT);
            let offset = clamp_offset(p.offset);

            let all = json_array(&report.data_json, "/report/vulnerabilities")?;
            let filtered: Vec<Value> = all
                .into_iter()
                .filter(|v| {
                    severities.as_ref().is_none_or(|s| {
                        s.iter()
                            .any(|sev| sev.eq_ignore_ascii_case(str_field(v, "severity")))
                    })
                })
                .filter(|v| !fixed_only || !str_field(v, "fixedVersion").is_empty())
                .map(|v| {
                    serde_json::json!({
                        "id": str_field(&v, "vulnerabilityID"),
                        "severity": str_field(&v, "severity"),
                        "score": v.get("score").cloned().unwrap_or(Value::Null),
                        "package": str_field(&v, "resource"),
                        "installed_version": str_field(&v, "installedVersion"),
                        "fixed_version": str_field(&v, "fixedVersion"),
                        "title": str_field(&v, "title"),
                        "link": str_field(&v, "primaryLink"),
                    })
                })
                .collect();

            let total = filtered.len() as i64;
            let items: Vec<Value> = filtered
                .into_iter()
                .skip(offset as usize)
                .take(limit as usize)
                .collect();

            Self::json_result(&serde_json::json!({
                "report": report.meta,
                "vulnerabilities": Page::new(items, total, limit, offset),
            }))
        })
        .await
    }

    #[tool(
        description = "Get one SbomReport: metadata and a paged list of components (name, version, \
type, purl). Filter by component name substring.",
        annotations(read_only_hint = true)
    )]
    async fn get_sbom_report(
        &self,
        ext: Extensions,
        Parameters(p): Parameters<GetSbomReportParams>,
    ) -> Result<CallToolResult, McpError> {
        self.observed(&ext, "get_sbom_report", async {
            self.authorize(&ext, "reports", "get")?;
            let report = self
                .state
                .db
                .get_report(&p.cluster, &p.namespace, &p.name, "sbomreport")
                .await
                .map_err(Self::db_error)?
                .ok_or_else(|| Self::not_found("SbomReport", &p.cluster, &p.namespace, &p.name))?;

            let needle = p.component.map(|c| c.to_ascii_lowercase());
            let limit = clamp_limit(p.limit, MAX_ITEM_LIMIT);
            let offset = clamp_offset(p.offset);

            let all = json_array(&report.data_json, "/report/components/components")?;
            let filtered: Vec<Value> = all
                .into_iter()
                .filter(|c| {
                    needle
                        .as_ref()
                        .is_none_or(|n| str_field(c, "name").to_ascii_lowercase().contains(n))
                })
                .map(|c| {
                    serde_json::json!({
                        "name": str_field(&c, "name"),
                        "version": str_field(&c, "version"),
                        "type": str_field(&c, "type"),
                        "purl": str_field(&c, "purl"),
                    })
                })
                .collect();

            let total = filtered.len() as i64;
            let items: Vec<Value> = filtered
                .into_iter()
                .skip(offset as usize)
                .take(limit as usize)
                .collect();

            Self::json_result(&serde_json::json!({
                "report": report.meta,
                "components": Page::new(items, total, limit, offset),
            }))
        })
        .await
    }

    #[tool(
        description = "Search vulnerabilities across every cluster by CVE id or package name \
(substring). Returns one row per affected image. Paged.",
        annotations(read_only_hint = true)
    )]
    async fn search_vulnerabilities(
        &self,
        ext: Extensions,
        Parameters(p): Parameters<SearchVulnerabilitiesParams>,
    ) -> Result<CallToolResult, McpError> {
        self.observed(&ext, "search_vulnerabilities", async {
            self.authorize(&ext, "reports", "get")?;
            let query = p.query.trim();
            if query.is_empty() {
                return Err(McpError::invalid_params("query must not be empty", None));
            }
            let limit = clamp_limit(p.limit, MAX_LIST_LIMIT);
            let offset = clamp_offset(p.offset);
            let (items, total) = self
                .state
                .db
                .search_vulnerabilities(query, limit, offset)
                .await
                .map_err(Self::db_error)?;
            Self::json_result(&Page::new(items, total, limit, offset))
        })
        .await
    }

    #[tool(
        description = "Search SBOM components across every cluster by package name (substring), \
optionally pinned to an exact version. Returns one row per image containing the component. Paged.",
        annotations(read_only_hint = true)
    )]
    async fn search_sbom_components(
        &self,
        ext: Extensions,
        Parameters(p): Parameters<SearchSbomComponentsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.observed(&ext, "search_sbom_components", async {
            self.authorize(&ext, "reports", "get")?;
            let component = p.component.trim();
            if component.is_empty() {
                return Err(McpError::invalid_params(
                    "component must not be empty",
                    None,
                ));
            }
            let limit = clamp_limit(p.limit, MAX_LIST_LIMIT);
            let offset = clamp_offset(p.offset);

            match p
                .version
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
            {
                None => {
                    let (items, total) = self
                        .state
                        .db
                        .search_sbom_components(component, limit, offset)
                        .await
                        .map_err(Self::db_error)?;
                    Self::json_result(&Page::new(items, total, limit, offset))
                }
                Some(version) => {
                    // The storage layer has no version predicate. Pull the full
                    // name match set (bounded by the row cap below), filter in
                    // memory, then page. Version pins are rare and narrow.
                    const VERSION_SCAN_CAP: i64 = 5_000;
                    let (rows, name_total) = self
                        .state
                        .db
                        .search_sbom_components(component, VERSION_SCAN_CAP, 0)
                        .await
                        .map_err(Self::db_error)?;
                    let scan_truncated = name_total > VERSION_SCAN_CAP;
                    let filtered: Vec<_> = rows
                        .into_iter()
                        .filter(|r| r.component_version == version)
                        .collect();
                    let total = filtered.len() as i64;
                    let items: Vec<_> = filtered
                        .into_iter()
                        .skip(offset as usize)
                        .take(limit as usize)
                        .collect();
                    let mut page = serde_json::to_value(Page::new(items, total, limit, offset))
                        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                    if scan_truncated {
                        page["scan_truncated"] = Value::Bool(true);
                    }
                    Self::json_result(&page)
                }
            }
        })
        .await
    }
}

impl TrivyMcp {
    async fn list_reports(
        &self,
        report_type: &str,
        p: ListReportsParams,
    ) -> Result<CallToolResult, McpError> {
        let limit = clamp_limit(p.limit, MAX_LIST_LIMIT);
        let offset = clamp_offset(p.offset);
        let params = QueryParams {
            cluster: p.cluster,
            namespace: p.namespace,
            app: p.app,
            image: p.image,
            severity: normalize_severities(p.severity),
            component: p.component,
            limit: Some(limit),
            offset: Some(offset),
            ..QueryParams::default()
        };
        let (items, total) = self
            .state
            .db
            .query_reports(report_type, &params)
            .await
            .map_err(Self::db_error)?;
        Self::json_result(&Page::new(items, total, limit, offset))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TrivyMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "trivy-collector",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::AlertEvaluator;
    use crate::auth::rbac::RbacPolicy;
    use crate::collector::types::ReportPayload;
    use crate::metrics::Metrics;
    use crate::storage::Database;
    use crate::web::state::{ConfigInfo, RuntimeInfo, WatcherStatus};
    use clap::Parser;
    use std::sync::Arc;

    async fn state_with(db: Database, rbac_csv: &str, default_policy: &str) -> AppState {
        let config = crate::config::Config::try_parse_from(["trivy-collector"]).unwrap();
        let mut registry = prometheus_client::registry::Registry::default();
        AppState {
            db: Arc::new(db),
            watcher_status: Arc::new(WatcherStatus::new()),
            config: Arc::new(ConfigInfo::from(&config)),
            runtime: Arc::new(RuntimeInfo::new()),
            auth: None,
            rbac: Arc::new(RbacPolicy::from_csv(rbac_csv, default_policy).unwrap()),
            metrics: Metrics::new(&mut registry, crate::config::Mode::Server),
            alerts: None::<Arc<AlertEvaluator>>,
        }
    }

    fn vuln_payload(cluster: &str, ns: &str, name: &str) -> ReportPayload {
        ReportPayload {
            cluster: cluster.into(),
            report_type: "vulnerabilityreport".into(),
            namespace: ns.into(),
            name: name.into(),
            data_json: serde_json::json!({
                "metadata": {"labels": {"trivy-operator.resource.name": "web"}},
                "report": {
                    "artifact": {"repository": "library/nginx", "tag": "1.25"},
                    "summary": {"criticalCount": 1, "highCount": 1, "mediumCount": 0, "lowCount": 1, "unknownCount": 0},
                    "vulnerabilities": [
                        {"vulnerabilityID": "CVE-2024-0001", "severity": "CRITICAL", "score": 9.8,
                         "resource": "openssl", "installedVersion": "1.1.1", "fixedVersion": "1.1.2",
                         "title": "bad", "primaryLink": "https://example.com/1"},
                        {"vulnerabilityID": "CVE-2024-0002", "severity": "HIGH", "score": 7.5,
                         "resource": "zlib", "installedVersion": "1.2", "fixedVersion": "",
                         "title": "meh", "primaryLink": "https://example.com/2"},
                        {"vulnerabilityID": "CVE-2024-0003", "severity": "LOW", "score": 2.0,
                         "resource": "bash", "installedVersion": "5.0", "fixedVersion": "5.1",
                         "title": "low", "primaryLink": "https://example.com/3"}
                    ]
                }
            })
            .to_string(),
            received_at: chrono::Utc::now(),
        }
    }

    fn sbom_payload(cluster: &str, ns: &str, name: &str) -> ReportPayload {
        ReportPayload {
            cluster: cluster.into(),
            report_type: "sbomreport".into(),
            namespace: ns.into(),
            name: name.into(),
            data_json: serde_json::json!({
                "metadata": {"labels": {"trivy-operator.resource.name": "api"}},
                "report": {
                    "artifact": {"repository": "library/app", "tag": "2.0"},
                    "components": {"components": [
                        {"name": "log4j-core", "version": "2.17.1", "type": "library", "purl": "pkg:maven/org.apache.logging.log4j/log4j-core@2.17.1"},
                        {"name": "log4j-api", "version": "2.17.1", "type": "library", "purl": "pkg:maven/org.apache.logging.log4j/log4j-api@2.17.1"},
                        {"name": "jackson-databind", "version": "2.15.0", "type": "library", "purl": "pkg:maven/com.fasterxml.jackson.core/jackson-databind@2.15.0"}
                    ]}
                }
            })
            .to_string(),
            received_at: chrono::Utc::now(),
        }
    }

    async fn seeded() -> TrivyMcp {
        let db = Database::new(":memory:").await.unwrap();
        db.upsert_report(&vuln_payload("prod", "default", "replicaset-web-abc"))
            .await
            .unwrap();
        db.upsert_report(&vuln_payload("stage", "payments", "replicaset-web-def"))
            .await
            .unwrap();
        db.upsert_report(&sbom_payload("prod", "default", "replicaset-api-xyz"))
            .await
            .unwrap();
        let state = state_with(db, RbacPolicy::default_csv(), "role:readonly").await;
        TrivyMcp::new(state, ToolLimiter::unlimited())
    }

    fn body(result: CallToolResult) -> Value {
        assert_ne!(result.is_error, Some(true));
        let text = match &result.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("unexpected content: {other:?}"),
        };
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn page_truncation_flag() {
        let p = Page::new(vec![1, 2], 5, 2, 0);
        assert!(p.truncated);
        let p = Page::new(vec![1], 3, 2, 2);
        assert!(!p.truncated);
        let p = Page::new(Vec::<i32>::new(), 0, 20, 0);
        assert!(!p.truncated);
    }

    #[test]
    fn json_array_handles_missing_path_and_bad_json() {
        assert!(
            json_array("{}", "/report/vulnerabilities")
                .unwrap()
                .is_empty()
        );
        assert!(json_array("not json", "/x").is_err());
    }

    #[test]
    fn tool_router_registers_all_tools() {
        let names: Vec<String> = TrivyMcp::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for expected in [
            "list_clusters",
            "list_namespaces",
            "get_stats",
            "list_vulnerability_reports",
            "list_sbom_reports",
            "get_vulnerability_report",
            "get_sbom_report",
            "search_vulnerabilities",
            "search_sbom_components",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
        assert_eq!(names.len(), 9);
    }

    #[test]
    fn server_info_advertises_tools() {
        let db = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(Database::new(":memory:"))
            .unwrap();
        let state = tokio::runtime::Runtime::new().unwrap().block_on(state_with(
            db,
            RbacPolicy::default_csv(),
            "role:readonly",
        ));
        let info = TrivyMcp::new(state, ToolLimiter::unlimited()).get_info();
        assert!(info.capabilities.tools.is_some());
        assert_eq!(info.server_info.name, "trivy-collector");
        assert!(info.instructions.is_some());
    }

    #[tokio::test]
    async fn clusters_and_namespaces() {
        let mcp = seeded().await;
        let v = body(mcp.list_clusters(Extensions::new()).await.unwrap());
        assert_eq!(v["total"], 2);

        let v = body(
            mcp.list_namespaces(
                Extensions::new(),
                Parameters(ListNamespacesParams {
                    cluster: Some("stage".into()),
                }),
            )
            .await
            .unwrap(),
        );
        assert_eq!(v["namespaces"], serde_json::json!(["payments"]));
    }

    #[tokio::test]
    async fn stats() {
        let mcp = seeded().await;
        let v = body(mcp.get_stats(Extensions::new()).await.unwrap());
        assert_eq!(v["total_vuln_reports"], 2);
        assert_eq!(v["total_sbom_reports"], 1);
    }

    #[tokio::test]
    async fn list_vulnerability_reports_filters_and_pages() {
        let mcp = seeded().await;
        let v = body(
            mcp.list_vulnerability_reports(
                Extensions::new(),
                Parameters(ListReportsParams {
                    severity: Some(vec!["critical".into()]),
                    limit: Some(1),
                    ..Default::default()
                }),
            )
            .await
            .unwrap(),
        );
        assert_eq!(v["total"], 2);
        assert_eq!(v["items"].as_array().unwrap().len(), 1);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["limit"], 1);

        let v = body(
            mcp.list_vulnerability_reports(
                Extensions::new(),
                Parameters(ListReportsParams {
                    cluster: Some("nope".into()),
                    ..Default::default()
                }),
            )
            .await
            .unwrap(),
        );
        assert_eq!(v["total"], 0);
        assert_eq!(v["truncated"], false);
    }

    #[tokio::test]
    async fn list_sbom_reports_by_component() {
        let mcp = seeded().await;
        let v = body(
            mcp.list_sbom_reports(
                Extensions::new(),
                Parameters(ListReportsParams {
                    component: Some("log4j".into()),
                    ..Default::default()
                }),
            )
            .await
            .unwrap(),
        );
        assert_eq!(v["total"], 1);
        assert_eq!(v["items"][0]["report_type"], "sbomreport");
    }

    #[tokio::test]
    async fn get_vulnerability_report_filters() {
        let mcp = seeded().await;
        let base = |severity: Option<Vec<String>>, fixed_only: Option<bool>| {
            GetVulnerabilityReportParams {
                cluster: "prod".into(),
                namespace: "default".into(),
                name: "replicaset-web-abc".into(),
                severity,
                fixed_only,
                limit: None,
                offset: None,
            }
        };

        let v = body(
            mcp.get_vulnerability_report(Extensions::new(), Parameters(base(None, None)))
                .await
                .unwrap(),
        );
        assert_eq!(v["vulnerabilities"]["total"], 3);
        assert_eq!(v["report"]["cluster"], "prod");
        assert_eq!(v["vulnerabilities"]["items"][0]["id"], "CVE-2024-0001");
        assert_eq!(v["vulnerabilities"]["items"][0]["package"], "openssl");

        let v = body(
            mcp.get_vulnerability_report(
                Extensions::new(),
                Parameters(base(Some(vec!["high".into(), "LOW".into()]), None)),
            )
            .await
            .unwrap(),
        );
        assert_eq!(v["vulnerabilities"]["total"], 2);

        let v = body(
            mcp.get_vulnerability_report(Extensions::new(), Parameters(base(None, Some(true))))
                .await
                .unwrap(),
        );
        assert_eq!(v["vulnerabilities"]["total"], 2);
        for item in v["vulnerabilities"]["items"].as_array().unwrap() {
            assert_ne!(item["fixed_version"], "");
        }

        let mut paged = base(None, None);
        paged.limit = Some(2);
        paged.offset = Some(2);
        let v = body(
            mcp.get_vulnerability_report(Extensions::new(), Parameters(paged))
                .await
                .unwrap(),
        );
        assert_eq!(v["vulnerabilities"]["items"].as_array().unwrap().len(), 1);
        assert_eq!(v["vulnerabilities"]["truncated"], false);
    }

    #[tokio::test]
    async fn get_vulnerability_report_missing() {
        let mcp = seeded().await;
        let err = mcp
            .get_vulnerability_report(
                Extensions::new(),
                Parameters(GetVulnerabilityReportParams {
                    cluster: "prod".into(),
                    namespace: "default".into(),
                    name: "missing".into(),
                    severity: None,
                    fixed_only: None,
                    limit: None,
                    offset: None,
                }),
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("not found"));
    }

    #[tokio::test]
    async fn get_sbom_report_filters() {
        let mcp = seeded().await;
        let v = body(
            mcp.get_sbom_report(
                Extensions::new(),
                Parameters(GetSbomReportParams {
                    cluster: "prod".into(),
                    namespace: "default".into(),
                    name: "replicaset-api-xyz".into(),
                    component: Some("LOG4J".into()),
                    limit: Some(1),
                    offset: None,
                }),
            )
            .await
            .unwrap(),
        );
        assert_eq!(v["components"]["total"], 2);
        assert_eq!(v["components"]["items"].as_array().unwrap().len(), 1);
        assert_eq!(v["components"]["truncated"], true);
        assert!(
            v["components"]["items"][0]["purl"]
                .as_str()
                .unwrap()
                .starts_with("pkg:maven/")
        );

        let err = mcp
            .get_sbom_report(
                Extensions::new(),
                Parameters(GetSbomReportParams {
                    cluster: "prod".into(),
                    namespace: "default".into(),
                    name: "nope".into(),
                    component: None,
                    limit: None,
                    offset: None,
                }),
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("SbomReport"));
    }

    #[tokio::test]
    async fn search_vulnerabilities_by_cve_and_package() {
        let mcp = seeded().await;
        let v = body(
            mcp.search_vulnerabilities(
                Extensions::new(),
                Parameters(SearchVulnerabilitiesParams {
                    query: "CVE-2024-0001".into(),
                    limit: None,
                    offset: None,
                }),
            )
            .await
            .unwrap(),
        );
        assert_eq!(v["total"], 2);
        assert_eq!(v["items"][0]["vulnerability_id"], "CVE-2024-0001");

        let v = body(
            mcp.search_vulnerabilities(
                Extensions::new(),
                Parameters(SearchVulnerabilitiesParams {
                    query: "zlib".into(),
                    limit: Some(1),
                    offset: Some(1),
                }),
            )
            .await
            .unwrap(),
        );
        assert_eq!(v["total"], 2);
        assert_eq!(v["items"].as_array().unwrap().len(), 1);
        assert_eq!(v["truncated"], false);

        let err = mcp
            .search_vulnerabilities(
                Extensions::new(),
                Parameters(SearchVulnerabilitiesParams {
                    query: "   ".into(),
                    limit: None,
                    offset: None,
                }),
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("empty"));
    }

    #[tokio::test]
    async fn search_sbom_components_with_and_without_version() {
        let mcp = seeded().await;
        let v = body(
            mcp.search_sbom_components(
                Extensions::new(),
                Parameters(SearchSbomComponentsParams {
                    component: "log4j".into(),
                    version: None,
                    limit: None,
                    offset: None,
                }),
            )
            .await
            .unwrap(),
        );
        assert_eq!(v["total"], 2);

        let v = body(
            mcp.search_sbom_components(
                Extensions::new(),
                Parameters(SearchSbomComponentsParams {
                    component: "log4j".into(),
                    version: Some("2.17.1".into()),
                    limit: Some(1),
                    offset: None,
                }),
            )
            .await
            .unwrap(),
        );
        assert_eq!(v["total"], 2);
        assert_eq!(v["items"].as_array().unwrap().len(), 1);
        assert_eq!(v["truncated"], true);
        assert!(v.get("scan_truncated").is_none());

        let v = body(
            mcp.search_sbom_components(
                Extensions::new(),
                Parameters(SearchSbomComponentsParams {
                    component: "log4j".into(),
                    version: Some("9.9.9".into()),
                    limit: None,
                    offset: None,
                }),
            )
            .await
            .unwrap(),
        );
        assert_eq!(v["total"], 0);

        let err = mcp
            .search_sbom_components(
                Extensions::new(),
                Parameters(SearchSbomComponentsParams {
                    component: "".into(),
                    version: None,
                    limit: None,
                    offset: None,
                }),
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("empty"));
    }

    #[test]
    fn error_class_and_status_mapping() {
        let denied = McpError::invalid_request("x", None);
        let bad = McpError::invalid_params("x", None);
        let missing = McpError::resource_not_found("x", None);
        let internal = McpError::internal_error("x", None);
        assert_eq!(error_class(&denied), "denied");
        assert_eq!(error_class(&bad), "invalid_params");
        assert_eq!(error_class(&missing), "not_found");
        assert_eq!(error_class(&internal), "error");
        assert_eq!(status_for(&Err(denied)), 403);
        assert_eq!(status_for(&Err(bad)), 400);
        assert_eq!(status_for(&Err(missing)), 404);
        assert_eq!(status_for(&Err(internal)), 500);
        assert_eq!(status_for(&Ok(CallToolResult::success(vec![]))), 200);
    }

    #[test]
    fn audit_entry_reads_session_and_headers() {
        let (mut parts, _) = axum::http::Request::builder()
            .header("user-agent", "kagent/1.0")
            .header("x-forwarded-for", "10.0.0.9")
            .body(())
            .unwrap()
            .into_parts();
        parts.extensions.insert(crate::auth::session::AuthSession {
            sub: "alice".into(),
            email: Some("alice@example.com".into()),
            name: None,
            preferred_username: None,
            groups: vec![],
            expires_at: i64::MAX,
        });
        let mut ext = Extensions::new();
        ext.insert(parts);
        let entry = audit_entry(&ext, "get_stats", &Ok(CallToolResult::success(vec![])), 12);
        assert_eq!(entry.method, "MCP");
        assert_eq!(entry.path, "/mcp/tools/call/get_stats");
        assert_eq!(entry.status_code, 200);
        assert_eq!(entry.duration_ms, 12);
        assert_eq!(entry.user_sub, "alice");
        assert_eq!(entry.user_email, "alice@example.com");
        assert_eq!(entry.user_agent, "kagent/1.0");
        assert_eq!(entry.remote_addr, "10.0.0.9");

        let anon = audit_entry(
            &Extensions::new(),
            "x",
            &Err(McpError::invalid_request("d", None)),
            1,
        );
        assert_eq!(anon.status_code, 403);
        assert!(anon.user_sub.is_empty());
    }

    #[test]
    fn limiter_zero_means_unlimited() {
        assert!(ToolLimiter::new(0).available().is_none());
        assert_eq!(ToolLimiter::new(3).available(), Some(3));
        assert!(ToolLimiter::unlimited().available().is_none());
    }

    #[tokio::test]
    async fn limiter_bounds_concurrent_tool_calls() {
        let db = Database::new(":memory:").await.unwrap();
        let state = state_with(db, RbacPolicy::default_csv(), "role:readonly").await;
        let limiter = ToolLimiter::new(1);
        let mcp = TrivyMcp::new(state, limiter.clone());

        // Hold the only permit, then confirm a tool call blocks until release.
        let permit = limiter.acquire().await.expect("permit");
        let mcp2 = mcp.clone();
        let call = tokio::spawn(async move { mcp2.get_stats(Extensions::new()).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!call.is_finished(), "call must wait for a permit");
        assert_eq!(limiter.available(), Some(0));
        drop(permit);
        let res = tokio::time::timeout(std::time::Duration::from_secs(2), call)
            .await
            .expect("call finishes after permit release")
            .unwrap();
        assert!(res.is_ok());
        assert_eq!(limiter.available(), Some(1));
    }

    #[tokio::test]
    async fn observed_records_metrics_and_audit_log() {
        let mcp = seeded().await;
        let db = mcp.state.db.clone();
        let before = db.count_api_logs().await.unwrap();

        mcp.get_stats(Extensions::new()).await.unwrap();
        mcp.search_vulnerabilities(
            Extensions::new(),
            Parameters(SearchVulnerabilitiesParams {
                query: " ".into(),
                limit: None,
                offset: None,
            }),
        )
        .await
        .unwrap_err();

        let calls = mcp.state.metrics.mcp_tool_calls_total.as_ref().unwrap();
        assert_eq!(
            calls
                .get_or_create(&McpToolLabels {
                    tool: "get_stats".into(),
                    result: "success".into()
                })
                .get(),
            1
        );
        assert_eq!(
            calls
                .get_or_create(&McpToolLabels {
                    tool: "search_vulnerabilities".into(),
                    result: "invalid_params".into()
                })
                .get(),
            1
        );
        assert_eq!(
            mcp.state
                .metrics
                .mcp_tool_calls_in_flight
                .as_ref()
                .unwrap()
                .get(),
            0
        );

        // Audit rows are written on a spawned task. Poll briefly.
        let mut after = before;
        for _ in 0..50 {
            after = db.count_api_logs().await.unwrap();
            if after >= before + 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(after, before + 2);
        let logs = db
            .list_api_logs(&crate::storage::ApiLogQuery {
                path_prefix: Some("/mcp/tools/call/".into()),
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(logs.0.iter().all(|l| l.method == "MCP"));
        assert!(logs.0.iter().any(|l| l.status_code == 400));
    }

    #[tokio::test]
    async fn rbac_denies_without_permission() {
        let db = Database::new(":memory:").await.unwrap();
        // Default policy grants nothing, no groups on the (absent) session.
        let state = state_with(db, "p, role:admin, *, *, allow\n", "").await;
        let mcp = TrivyMcp::new(state, ToolLimiter::unlimited());
        let err = mcp.get_stats(Extensions::new()).await.unwrap_err();
        assert!(err.message.contains("RBAC denied: stats:get"));
        let err = mcp.list_clusters(Extensions::new()).await.unwrap_err();
        assert!(err.message.contains("clusters:get"));
    }
}
