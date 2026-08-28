//! The scraper's internal read API.
//!
//! Mirrors the `Database` methods the server actually calls rather than
//! inventing a second query language, and returns the same `serde` models the
//! local store returns.
//!
//! # Trust boundary
//!
//! This API returns every report in the fleet with no per-user filtering,
//! because RBAC is applied above it in the server. Reachable unauthenticated
//! from anywhere in the namespace it would be a straight downgrade from a
//! filesystem permission, so it is protected two ways: a shared token compared
//! in constant time on every request, and a NetworkPolicy admitting only the
//! server pods. The port is never added to the HTTPRoute or any ServiceMonitor.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get},
};
use serde::Deserialize;
use tracing::{debug, error, info, warn};

use crate::collector::status::WatcherStatus;
use crate::collector::types::{ReportEvent, ReportEventType};
use crate::storage::remote::{INTERNAL_API_PREFIX, INTERNAL_TOKEN_HEADER};
use crate::storage::store::{DataRangeResponse, PagedResponse};
use crate::storage::token_store::constant_time_eq;
use crate::storage::{Database, QueryParams};

/// Request body limit, sized for the largest Trivy report that can be pushed
/// through the ingest route.
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

#[derive(Clone)]
pub struct InternalApi {
    db: Arc<Database>,
    status: Arc<WatcherStatus>,
    token: String,
}

impl InternalApi {
    pub fn new(db: Arc<Database>, status: Arc<WatcherStatus>, token: String) -> Self {
        Self { db, status, token }
    }

    pub async fn serve(
        &self,
        port: u16,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        if self.token.is_empty() {
            warn!(
                "INTERNAL_TOKEN is empty — the internal API will reject every \
                 request. Set it on both the scraper and the server."
            );
        }

        let app = router(self.clone());
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let listener = tokio::net::TcpListener::bind(addr).await?;
        info!(addr = %addr, prefix = INTERNAL_API_PREFIX, "Internal API listening");

        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown.changed().await;
                info!("Internal API shutting down");
            })
            .await?;
        Ok(())
    }
}

pub fn router(state: InternalApi) -> Router {
    let routes = Router::new()
        .route("/reports", get(query_reports).post(ingest_report))
        .route(
            "/reports/{cluster}/{report_type}/{namespace}/{name}",
            get(get_report).delete(delete_report),
        )
        .route("/stats", get(get_stats))
        .route("/clusters", get(list_clusters))
        .route("/clusters/{cluster}", delete(delete_cluster_reports))
        .route("/namespaces", get(list_namespaces))
        .route("/search/vulnerabilities", get(search_vulnerabilities))
        .route("/search/components", get(search_components))
        .route("/sbom/component-matches", get(list_component_matches))
        .route("/suggest/vulnerability-ids", get(suggest_vulnerability_ids))
        .route("/suggest/component-names", get(suggest_component_names))
        .route("/dashboard/trends", get(get_trends))
        .route("/dashboard/data-range", get(get_data_range))
        .route("/hydration", get(get_hydration));

    Router::new()
        .nest(INTERNAL_API_PREFIX, routes)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_internal_token,
        ))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

/// Reject any request that does not carry the shared token. An empty
/// configured token denies everything rather than opening the API up.
async fn require_internal_token(
    State(state): State<InternalApi>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let presented = headers
        .get(INTERNAL_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    if state.token.is_empty() || !constant_time_eq(presented, &state.token) {
        debug!(path = %request.uri().path(), "Internal API request rejected");
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "invalid internal token"})),
        )
            .into_response();
    }
    next.run(request).await
}

/// Map a storage failure onto a 500 without leaking the query shape.
fn storage_error(context: &str, e: anyhow::Error) -> Response {
    error!(error = %e, context = context, "Internal API storage error");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": e.to_string()})),
    )
        .into_response()
}

fn paged<T: serde::Serialize>(items: Vec<T>, total: i64) -> Response {
    Json(PagedResponse { items, total }).into_response()
}

/// Every list route answers with the same envelope, so `total` is derived from
/// the item count when the query has no separate count.
fn paged_all<T: serde::Serialize>(items: Vec<T>) -> Response {
    let total = items.len() as i64;
    paged(items, total)
}

#[derive(Debug, Default, Deserialize)]
pub struct ReportsQuery {
    pub report_type: String,
    pub cluster: Option<String>,
    pub namespace: Option<String>,
    pub app: Option<String>,
    pub image: Option<String>,
    pub component: Option<String>,
    pub cve: Option<String>,
    /// Comma-separated severity names.
    pub severity: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

impl ReportsQuery {
    fn to_params(&self) -> QueryParams {
        QueryParams {
            cluster: self.cluster.clone(),
            namespace: self.namespace.clone(),
            app: self.app.clone(),
            severity: self.severity.as_ref().map(|s| split_csv(s)),
            image: self.image.clone(),
            cve: self.cve.clone(),
            component: self.component.clone(),
            limit: self.limit,
            offset: self.offset,
        }
    }
}

/// Split a comma-separated query value, dropping empties so a trailing comma
/// does not become a blank filter.
fn split_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

async fn query_reports(
    State(state): State<InternalApi>,
    Query(query): Query<ReportsQuery>,
) -> Response {
    match state
        .db
        .query_reports(&query.report_type, &query.to_params())
        .await
    {
        Ok((items, total)) => paged(items, total),
        Err(e) => storage_error("query_reports", e),
    }
}

async fn get_report(
    State(state): State<InternalApi>,
    Path((cluster, report_type, namespace, name)): Path<(String, String, String, String)>,
) -> Response {
    match state
        .db
        .get_report(&cluster, &namespace, &name, &report_type)
        .await
    {
        Ok(Some(report)) => Json(report).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Report not found"})),
        )
            .into_response(),
        Err(e) => storage_error("get_report", e),
    }
}

/// Ingest path for pushed reports. Kept so any remaining pusher keeps working,
/// and so its writes go through the same alert-evaluating path as a watch event.
async fn ingest_report(
    State(state): State<InternalApi>,
    Json(event): Json<ReportEvent>,
) -> Response {
    let payload = event.payload;
    match event.event_type {
        ReportEventType::Apply => match state.db.upsert_report(&payload).await {
            Ok(()) => {
                info!(
                    cluster = %payload.cluster,
                    report_type = %payload.report_type,
                    namespace = %payload.namespace,
                    name = %payload.name,
                    "Report ingested"
                );
                Json(serde_json::json!({"status": "ok"})).into_response()
            }
            Err(e) => storage_error("upsert_report", e),
        },
        ReportEventType::Delete => match state
            .db
            .delete_report(
                &payload.cluster,
                &payload.namespace,
                &payload.name,
                &payload.report_type,
            )
            .await
        {
            Ok(deleted) => {
                Json(serde_json::json!({"status": "ok", "deleted": deleted})).into_response()
            }
            Err(e) => storage_error("delete_report", e),
        },
    }
}

async fn delete_report(
    State(state): State<InternalApi>,
    Path((cluster, report_type, namespace, name)): Path<(String, String, String, String)>,
) -> Response {
    match state
        .db
        .delete_report(&cluster, &namespace, &name, &report_type)
        .await
    {
        Ok(true) => Json(serde_json::json!({"deleted": true})).into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Report not found"})),
        )
            .into_response(),
        Err(e) => storage_error("delete_report", e),
    }
}

async fn delete_cluster_reports(
    State(state): State<InternalApi>,
    Path(cluster): Path<String>,
) -> Response {
    match state.db.delete_reports_for_cluster(&cluster).await {
        Ok(deleted) => Json(serde_json::json!({"deleted": deleted})).into_response(),
        Err(e) => storage_error("delete_reports_for_cluster", e),
    }
}

async fn get_stats(State(state): State<InternalApi>) -> Response {
    match state.db.get_stats().await {
        Ok(stats) => Json(stats).into_response(),
        Err(e) => storage_error("get_stats", e),
    }
}

async fn list_clusters(State(state): State<InternalApi>) -> Response {
    match state.db.list_clusters().await {
        Ok(items) => paged_all(items),
        Err(e) => storage_error("list_clusters", e),
    }
}

#[derive(Debug, Deserialize)]
pub struct ClusterQuery {
    pub cluster: Option<String>,
}

async fn list_namespaces(
    State(state): State<InternalApi>,
    Query(query): Query<ClusterQuery>,
) -> Response {
    match state.db.list_namespaces(query.cluster.as_deref()).await {
        Ok(items) => paged_all(items),
        Err(e) => storage_error("list_namespaces", e),
    }
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub component: String,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

impl SearchQuery {
    fn limit(&self) -> i64 {
        self.limit.unwrap_or(500)
    }

    fn offset(&self) -> i64 {
        self.offset.unwrap_or(0)
    }

    fn suggest_limit(&self) -> i64 {
        self.limit.unwrap_or(20)
    }
}

async fn search_vulnerabilities(
    State(state): State<InternalApi>,
    Query(query): Query<SearchQuery>,
) -> Response {
    match state
        .db
        .search_vulnerabilities(&query.q, query.limit(), query.offset())
        .await
    {
        Ok((items, total)) => paged(items, total),
        Err(e) => storage_error("search_vulnerabilities", e),
    }
}

async fn search_components(
    State(state): State<InternalApi>,
    Query(query): Query<SearchQuery>,
) -> Response {
    match state
        .db
        .search_sbom_components(&query.component, query.limit(), query.offset())
        .await
    {
        Ok((items, total)) => paged(items, total),
        Err(e) => storage_error("search_sbom_components", e),
    }
}

async fn suggest_vulnerability_ids(
    State(state): State<InternalApi>,
    Query(query): Query<SearchQuery>,
) -> Response {
    match state
        .db
        .suggest_vulnerability_ids(&query.q, query.suggest_limit())
        .await
    {
        Ok(items) => paged_all(items),
        Err(e) => storage_error("suggest_vulnerability_ids", e),
    }
}

async fn suggest_component_names(
    State(state): State<InternalApi>,
    Query(query): Query<SearchQuery>,
) -> Response {
    match state
        .db
        .suggest_component_names(&query.q, query.suggest_limit())
        .await
    {
        Ok(items) => paged_all(items),
        Err(e) => storage_error("suggest_component_names", e),
    }
}

#[derive(Debug, Deserialize)]
pub struct ComponentMatchQuery {
    /// Comma-separated cluster names; empty means every cluster.
    pub clusters: Option<String>,
    pub namespace: Option<String>,
    pub package_name: Option<String>,
}

async fn list_component_matches(
    State(state): State<InternalApi>,
    Query(query): Query<ComponentMatchQuery>,
) -> Response {
    let clusters = query.clusters.as_deref().map(split_csv).unwrap_or_default();
    match state
        .db
        .list_sbom_component_matches(
            &clusters,
            query.namespace.as_deref(),
            query.package_name.as_deref(),
        )
        .await
    {
        Ok(items) => paged_all(items),
        Err(e) => storage_error("list_sbom_component_matches", e),
    }
}

#[derive(Debug, Deserialize)]
pub struct TrendsQuery {
    pub start_date: String,
    pub end_date: String,
    pub granularity: String,
    pub cluster: Option<String>,
}

async fn get_trends(
    State(state): State<InternalApi>,
    Query(query): Query<TrendsQuery>,
) -> Response {
    match state
        .db
        .get_live_trends(
            &query.start_date,
            &query.end_date,
            query.cluster.as_deref(),
            &query.granularity,
        )
        .await
    {
        Ok(trends) => Json(trends).into_response(),
        Err(e) => storage_error("get_live_trends", e),
    }
}

async fn get_data_range(State(state): State<InternalApi>) -> Response {
    match state.db.get_reports_data_range().await {
        Ok((data_from, data_to)) => Json(DataRangeResponse { data_from, data_to }).into_response(),
        Err(e) => storage_error("get_reports_data_range", e),
    }
}

async fn get_hydration(State(state): State<InternalApi>) -> Response {
    Json(state.status.snapshot()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const TOKEN: &str = "shared-internal-token";

    async fn api() -> InternalApi {
        let db = Arc::new(Database::new(":memory:").await.unwrap());
        InternalApi::new(db, Arc::new(WatcherStatus::new()), TOKEN.to_string())
    }

    fn authed(uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header(INTERNAL_TOKEN_HEADER, TOKEN)
            .body(Body::empty())
            .unwrap()
    }

    async fn json_body(resp: Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn split_csv_drops_blanks() {
        assert_eq!(split_csv("a,b"), vec!["a", "b"]);
        assert_eq!(split_csv(" a , ,b ,"), vec!["a", "b"]);
        assert!(split_csv("").is_empty());
        assert!(split_csv(",,").is_empty());
    }

    #[test]
    fn reports_query_maps_onto_storage_params() {
        let q = ReportsQuery {
            report_type: "vulnerabilityreport".into(),
            cluster: Some("prod".into()),
            severity: Some("critical, high".into()),
            limit: Some(10),
            ..Default::default()
        };
        let p = q.to_params();
        assert_eq!(p.cluster.as_deref(), Some("prod"));
        assert_eq!(
            p.severity,
            Some(vec!["critical".to_string(), "high".to_string()])
        );
        assert_eq!(p.limit, Some(10));
        assert!(p.namespace.is_none());
    }

    #[test]
    fn search_query_defaults_match_the_public_api() {
        let q = SearchQuery {
            q: String::new(),
            component: String::new(),
            limit: None,
            offset: None,
        };
        assert_eq!(q.limit(), 500);
        assert_eq!(q.offset(), 0);
        assert_eq!(q.suggest_limit(), 20);
    }

    #[tokio::test]
    async fn a_request_with_no_token_is_rejected() {
        let app = router(api().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/internal/v1/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_request_with_the_wrong_token_is_rejected() {
        let app = router(api().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/internal/v1/stats")
                    .header(INTERNAL_TOKEN_HEADER, "nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_empty_configured_token_denies_everything() {
        let db = Arc::new(Database::new(":memory:").await.unwrap());
        let state = InternalApi::new(db, Arc::new(WatcherStatus::new()), String::new());
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/internal/v1/stats")
                    .header(INTERNAL_TOKEN_HEADER, "")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn stats_answers_an_authenticated_request() {
        let app = router(api().await);
        let resp = app.oneshot(authed("/internal/v1/stats")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["total_clusters"], 0);
    }

    #[tokio::test]
    async fn list_routes_share_the_paged_envelope() {
        for uri in [
            "/internal/v1/clusters",
            "/internal/v1/namespaces",
            "/internal/v1/reports?report_type=sbomreport",
            "/internal/v1/search/vulnerabilities?q=CVE",
            "/internal/v1/search/components?component=log4j",
            "/internal/v1/suggest/vulnerability-ids?q=CVE",
            "/internal/v1/suggest/component-names?q=log4j",
            "/internal/v1/sbom/component-matches",
        ] {
            let app = router(api().await);
            let resp = app.oneshot(authed(uri)).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            let body = json_body(resp).await;
            assert!(body["items"].is_array(), "{uri}");
            assert_eq!(body["total"], 0, "{uri}");
        }
    }

    #[tokio::test]
    async fn a_missing_report_is_a_404_not_an_empty_body() {
        let app = router(api().await);
        let resp = app
            .oneshot(authed("/internal/v1/reports/prod/sbomreport/default/nginx"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn hydration_reports_an_unhydrated_empty_fleet() {
        let app = router(api().await);
        let resp = app.oneshot(authed("/internal/v1/hydration")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["hydrated"], false);
    }

    #[tokio::test]
    async fn hydration_flips_once_every_cluster_syncs() {
        let state = api().await;
        use crate::collector::status::ReportKind;
        state.status.register_cluster("prod");
        state
            .status
            .set_sync_done("prod", ReportKind::Vulnerability, true);
        state.status.set_sync_done("prod", ReportKind::Sbom, true);

        let app = router(state);
        let resp = app.oneshot(authed("/internal/v1/hydration")).await.unwrap();
        let body = json_body(resp).await;
        assert_eq!(body["hydrated"], true);
        assert_eq!(body["clusters"]["prod"]["sbom_initial_sync_done"], true);
    }

    #[tokio::test]
    async fn data_range_is_empty_on_a_fresh_database() {
        let app = router(api().await);
        let resp = app
            .oneshot(authed("/internal/v1/dashboard/data-range"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert!(body["data_from"].is_null());
    }

    #[tokio::test]
    async fn ingest_then_read_round_trips_a_report() {
        let state = api().await;
        let event = serde_json::json!({
            "event_type": "Apply",
            "payload": {
                "cluster": "prod",
                "report_type": "sbomreport",
                "namespace": "default",
                "name": "nginx",
                "data_json": "{\"report\":{\"summary\":{\"componentsCount\":3}}}",
                "received_at": "2026-01-01T00:00:00Z"
            }
        });

        let resp = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/internal/v1/reports")
                    .header(INTERNAL_TOKEN_HEADER, TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(event.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = router(state.clone())
            .oneshot(authed("/internal/v1/reports/prod/sbomreport/default/nginx"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["meta"]["cluster"], "prod");

        // Notes are never served by the scraper; the server joins them in.
        assert_eq!(body["meta"]["notes"], "");

        let resp = router(state)
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/internal/v1/reports/prod/sbomreport/default/nginx")
                    .header(INTERNAL_TOKEN_HEADER, TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_body(resp).await["deleted"], true);
    }

    #[tokio::test]
    async fn deleting_a_cluster_reports_how_many_rows_went() {
        let app = router(api().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/internal/v1/clusters/prod")
                    .header(INTERNAL_TOKEN_HEADER, TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_body(resp).await["deleted"], 0);
    }
}
