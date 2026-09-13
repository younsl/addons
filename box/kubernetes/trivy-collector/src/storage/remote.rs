//! `ReportStore` over the scraper's internal HTTP API.
//!
//! This is what makes the server pod disposable: it holds no database, no
//! volume, and no connection pool, and every report read becomes a call to the
//! one process that owns the SQLite file. Response bodies are the same `serde`
//! models the local store returns, so the two implementations cannot drift in
//! shape.
//!
//! Every request carries the shared internal token. The port is also fenced by
//! a NetworkPolicy admitting only the server pods, and is never added to the
//! HTTPRoute or any ServiceMonitor.

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use std::time::Duration;
use tracing::debug;

use crate::collector::types::{ReportEvent, ReportEventType, ReportPayload};

use super::dashboard::TrendResponse;
use super::models::{
    ClusterInfo, ComponentSearchResult, FullReport, QueryParams, ReportMeta, SbomComponentMatch,
    Stats, VulnSearchResult,
};
use super::store::{DataRangeResponse, HydrationStatus, PagedResponse, ReportStore};

/// Header carrying the shared internal token.
pub const INTERNAL_TOKEN_HEADER: &str = "x-internal-token";

/// Prefix every internal route sits under.
pub const INTERNAL_API_PREFIX: &str = "/internal/v1";

const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[derive(Clone)]
pub struct RemoteStore {
    client: reqwest::Client,
    base_url: String,
    token: String,
}

impl RemoteStore {
    /// `base_url` is the scraper's internal endpoint, e.g.
    /// `http://trivy-collector-scraper:8081`.
    pub fn new(base_url: &str, token: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .build()
            .context("Failed to build internal API HTTP client")?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}{}", self.base_url, INTERNAL_API_PREFIX, path)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        let url = self.url(path);
        debug!(url = %url, "Internal API request");
        let resp = self
            .client
            .get(&url)
            .header(INTERNAL_TOKEN_HEADER, &self.token)
            .query(query)
            .send()
            .await
            .with_context(|| format!("Internal API request to {} failed", url))?;
        Self::decode(resp, &url).await
    }

    async fn decode<T: DeserializeOwned>(resp: reqwest::Response, url: &str) -> Result<T> {
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Internal API {} returned {}: {}",
                url,
                status,
                body.chars().take(256).collect::<String>()
            ));
        }
        resp.json::<T>()
            .await
            .with_context(|| format!("Failed to decode internal API response from {}", url))
    }

    /// Path segment encoding for the report-identity routes. Names and
    /// namespaces are Kubernetes identifiers, but cluster names come from a
    /// registration Secret and are not guaranteed to be path-safe.
    fn report_path(cluster: &str, report_type: &str, namespace: &str, name: &str) -> String {
        format!(
            "/reports/{}/{}/{}/{}",
            urlencoding::encode(cluster),
            urlencoding::encode(report_type),
            urlencoding::encode(namespace),
            urlencoding::encode(name)
        )
    }
}

/// Flatten `QueryParams` into the internal API's query string. Only the fields
/// the SQL layer actually reads are sent.
pub fn query_params_to_pairs(
    report_type: &str,
    params: &QueryParams,
) -> Vec<(&'static str, String)> {
    let mut pairs = vec![("report_type", report_type.to_string())];
    if let Some(v) = &params.cluster {
        pairs.push(("cluster", v.clone()));
    }
    if let Some(v) = &params.namespace {
        pairs.push(("namespace", v.clone()));
    }
    if let Some(v) = &params.app {
        pairs.push(("app", v.clone()));
    }
    if let Some(v) = &params.image {
        pairs.push(("image", v.clone()));
    }
    if let Some(v) = &params.component {
        pairs.push(("component", v.clone()));
    }
    if let Some(v) = &params.cve {
        pairs.push(("cve", v.clone()));
    }
    if let Some(v) = &params.severity
        && !v.is_empty()
    {
        pairs.push(("severity", v.join(",")));
    }
    if let Some(v) = params.limit {
        pairs.push(("limit", v.to_string()));
    }
    if let Some(v) = params.offset {
        pairs.push(("offset", v.to_string()));
    }
    pairs
}

#[async_trait]
impl ReportStore for RemoteStore {
    async fn query_reports(
        &self,
        report_type: &str,
        params: &QueryParams,
    ) -> Result<(Vec<ReportMeta>, i64)> {
        let resp: PagedResponse<ReportMeta> = self
            .get("/reports", &query_params_to_pairs(report_type, params))
            .await?;
        Ok((resp.items, resp.total))
    }

    async fn get_report(
        &self,
        cluster: &str,
        namespace: &str,
        name: &str,
        report_type: &str,
    ) -> Result<Option<FullReport>> {
        let path = Self::report_path(cluster, report_type, namespace, name);
        let url = self.url(&path);
        let resp = self
            .client
            .get(&url)
            .header(INTERNAL_TOKEN_HEADER, &self.token)
            .send()
            .await
            .with_context(|| format!("Internal API request to {} failed", url))?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(Self::decode(resp, &url).await?))
    }

    async fn get_stats(&self) -> Result<Stats> {
        self.get("/stats", &[]).await
    }

    async fn list_clusters(&self) -> Result<Vec<ClusterInfo>> {
        let resp: PagedResponse<ClusterInfo> = self.get("/clusters", &[]).await?;
        Ok(resp.items)
    }

    async fn list_namespaces(&self, cluster: Option<&str>) -> Result<Vec<String>> {
        let query: Vec<(&str, String)> = cluster
            .map(|c| vec![("cluster", c.to_string())])
            .unwrap_or_default();
        let resp: PagedResponse<String> = self.get("/namespaces", &query).await?;
        Ok(resp.items)
    }

    async fn search_vulnerabilities(
        &self,
        query: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<VulnSearchResult>, i64)> {
        let resp: PagedResponse<VulnSearchResult> = self
            .get(
                "/search/vulnerabilities",
                &[
                    ("q", query.to_string()),
                    ("limit", limit.to_string()),
                    ("offset", offset.to_string()),
                ],
            )
            .await?;
        Ok((resp.items, resp.total))
    }

    async fn search_sbom_components(
        &self,
        component: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ComponentSearchResult>, i64)> {
        let resp: PagedResponse<ComponentSearchResult> = self
            .get(
                "/search/components",
                &[
                    ("component", component.to_string()),
                    ("limit", limit.to_string()),
                    ("offset", offset.to_string()),
                ],
            )
            .await?;
        Ok((resp.items, resp.total))
    }

    async fn suggest_vulnerability_ids(&self, query: &str, limit: i64) -> Result<Vec<String>> {
        let resp: PagedResponse<String> = self
            .get(
                "/suggest/vulnerability-ids",
                &[("q", query.to_string()), ("limit", limit.to_string())],
            )
            .await?;
        Ok(resp.items)
    }

    async fn suggest_component_names(&self, query: &str, limit: i64) -> Result<Vec<String>> {
        let resp: PagedResponse<String> = self
            .get(
                "/suggest/component-names",
                &[("q", query.to_string()), ("limit", limit.to_string())],
            )
            .await?;
        Ok(resp.items)
    }

    async fn list_sbom_component_matches(
        &self,
        clusters: &[String],
        namespace: Option<&str>,
        package_name: Option<&str>,
    ) -> Result<Vec<SbomComponentMatch>> {
        let mut query: Vec<(&str, String)> = Vec::new();
        if !clusters.is_empty() {
            query.push(("clusters", clusters.join(",")));
        }
        if let Some(ns) = namespace {
            query.push(("namespace", ns.to_string()));
        }
        if let Some(pkg) = package_name {
            query.push(("package_name", pkg.to_string()));
        }
        let resp: PagedResponse<SbomComponentMatch> =
            self.get("/sbom/component-matches", &query).await?;
        Ok(resp.items)
    }

    async fn get_live_trends(
        &self,
        start_date: &str,
        end_date: &str,
        cluster: Option<&str>,
        granularity: &str,
    ) -> Result<TrendResponse> {
        let mut query = vec![
            ("start_date", start_date.to_string()),
            ("end_date", end_date.to_string()),
            ("granularity", granularity.to_string()),
        ];
        if let Some(c) = cluster {
            query.push(("cluster", c.to_string()));
        }
        self.get("/dashboard/trends", &query).await
    }

    async fn get_reports_data_range(&self) -> Result<(Option<String>, Option<String>)> {
        let resp: DataRangeResponse = self.get("/dashboard/data-range", &[]).await?;
        Ok((resp.data_from, resp.data_to))
    }

    async fn hydration(&self) -> Result<HydrationStatus> {
        self.get("/hydration", &[]).await
    }

    async fn upsert_report(&self, payload: &ReportPayload) -> Result<()> {
        let url = self.url("/reports");
        let event = ReportEvent {
            event_type: ReportEventType::Apply,
            payload: payload.clone(),
        };
        let resp = self
            .client
            .post(&url)
            .header(INTERNAL_TOKEN_HEADER, &self.token)
            .json(&event)
            .send()
            .await
            .with_context(|| format!("Internal API request to {} failed", url))?;
        let _: serde_json::Value = Self::decode(resp, &url).await?;
        Ok(())
    }

    async fn delete_report(
        &self,
        cluster: &str,
        namespace: &str,
        name: &str,
        report_type: &str,
    ) -> Result<bool> {
        let path = Self::report_path(cluster, report_type, namespace, name);
        let url = self.url(&path);
        let resp = self
            .client
            .delete(&url)
            .header(INTERNAL_TOKEN_HEADER, &self.token)
            .send()
            .await
            .with_context(|| format!("Internal API request to {} failed", url))?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        let body: serde_json::Value = Self::decode(resp, &url).await?;
        Ok(body
            .get("deleted")
            .and_then(|v| v.as_bool())
            .unwrap_or(true))
    }

    async fn delete_reports_for_cluster(&self, cluster: &str) -> Result<u64> {
        let url = self.url(&format!("/clusters/{}", urlencoding::encode(cluster)));
        let resp = self
            .client
            .delete(&url)
            .header(INTERNAL_TOKEN_HEADER, &self.token)
            .send()
            .await
            .with_context(|| format!("Internal API request to {} failed", url))?;
        let body: serde_json::Value = Self::decode(resp, &url).await?;
        Ok(body.get("deleted").and_then(|v| v.as_u64()).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> RemoteStore {
        RemoteStore::new("http://scraper:8081/", "shh").unwrap()
    }

    #[test]
    fn base_url_loses_its_trailing_slash() {
        assert_eq!(store().base_url(), "http://scraper:8081");
    }

    #[test]
    fn urls_sit_under_the_internal_prefix() {
        assert_eq!(
            store().url("/stats"),
            "http://scraper:8081/internal/v1/stats"
        );
    }

    #[test]
    fn report_paths_encode_every_segment() {
        let p = RemoteStore::report_path("prod/eu", "sbomreport", "kube-system", "a b/c");
        assert_eq!(p, "/reports/prod%2Feu/sbomreport/kube-system/a%20b%2Fc");
    }

    #[test]
    fn query_params_carry_only_what_is_set() {
        let params = QueryParams {
            cluster: Some("prod".into()),
            severity: Some(vec!["critical".into(), "high".into()]),
            limit: Some(50),
            ..Default::default()
        };
        let pairs = query_params_to_pairs("vulnerabilityreport", &params);

        assert!(pairs.contains(&("report_type", "vulnerabilityreport".to_string())));
        assert!(pairs.contains(&("cluster", "prod".to_string())));
        assert!(pairs.contains(&("severity", "critical,high".to_string())));
        assert!(pairs.contains(&("limit", "50".to_string())));
        assert!(!pairs.iter().any(|(k, _)| *k == "namespace"));
        assert!(!pairs.iter().any(|(k, _)| *k == "offset"));
    }

    #[test]
    fn empty_severity_list_is_not_sent() {
        let params = QueryParams {
            severity: Some(vec![]),
            ..Default::default()
        };
        let pairs = query_params_to_pairs("sbomreport", &params);
        assert!(!pairs.iter().any(|(k, _)| *k == "severity"));
    }

    // ───── Wire contract against the real internal API ─────
    //
    // These serve `collector::api::router` over a real socket and drive it
    // through a real `RemoteStore`. Anything else only proves the client and
    // the server each compile: the point of this store is that the two agree
    // on paths, query names, and response envelopes, and that is a property of
    // the pair rather than of either half.

    use crate::collector::api::InternalApi;
    use crate::collector::status::{ReportKind, WatcherStatus};
    use crate::collector::types::ReportPayload;
    use crate::storage::Database;
    use std::sync::Arc;

    const TOKEN: &str = "shared-internal-token";

    /// A live internal API plus a store pointed at it. Dropping the guard stops
    /// the server.
    struct Harness {
        store: RemoteStore,
        db: Arc<Database>,
        status: Arc<WatcherStatus>,
        shutdown: tokio::sync::watch::Sender<bool>,
    }

    impl Harness {
        async fn start() -> Self {
            Self::start_with_token(TOKEN).await
        }

        async fn start_with_token(client_token: &str) -> Self {
            let db = Arc::new(Database::new(":memory:").await.unwrap());
            let status = Arc::new(WatcherStatus::new());
            let api = InternalApi::new(db.clone(), status.clone(), TOKEN.to_string());
            let app = crate::collector::api::router(api);

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (shutdown, mut rx) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = rx.changed().await;
                    })
                    .await;
            });

            let store = RemoteStore::new(&format!("http://{}", addr), client_token).unwrap();
            Self {
                store,
                db,
                status,
                shutdown,
            }
        }

        /// Seed through the local database rather than the API, so a broken
        /// ingest route cannot hide a broken read route.
        async fn seed(&self) {
            for payload in [
                vuln_payload("prod", "default", "nginx-vuln"),
                vuln_payload("stage", "default", "redis-vuln"),
                sbom_payload("prod", "default", "nginx-sbom"),
            ] {
                self.db.upsert_report(&payload).await.unwrap();
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = self.shutdown.send(true);
        }
    }

    fn vuln_payload(cluster: &str, namespace: &str, name: &str) -> ReportPayload {
        ReportPayload {
            cluster: cluster.into(),
            namespace: namespace.into(),
            name: name.into(),
            report_type: "vulnerabilityreport".into(),
            data_json: serde_json::json!({
                "metadata": {"labels": {"trivy-operator.resource.name": "web"}},
                "report": {
                    "artifact": {"repository": "library/nginx", "tag": "1.25"},
                    "registry": {"server": "docker.io"},
                    "summary": {
                        "criticalCount": 1, "highCount": 2, "mediumCount": 0,
                        "lowCount": 0, "unknownCount": 0
                    },
                    "vulnerabilities": [
                        {"vulnerabilityID": "CVE-2024-0001", "severity": "CRITICAL",
                         "score": 9.8, "resource": "openssl",
                         "installedVersion": "1.1.1", "fixedVersion": "1.1.2"},
                        {"vulnerabilityID": "CVE-2024-0002", "severity": "HIGH",
                         "score": 7.5, "resource": "zlib",
                         "installedVersion": "1.2", "fixedVersion": ""}
                    ]
                }
            })
            .to_string(),
            received_at: chrono::Utc::now(),
        }
    }

    fn sbom_payload(cluster: &str, namespace: &str, name: &str) -> ReportPayload {
        ReportPayload {
            cluster: cluster.into(),
            namespace: namespace.into(),
            name: name.into(),
            report_type: "sbomreport".into(),
            data_json: serde_json::json!({
                "metadata": {"labels": {"trivy-operator.resource.name": "web"}},
                "report": {
                    "artifact": {"repository": "library/nginx", "tag": "1.25"},
                    "registry": {"server": "docker.io"},
                    "summary": {"componentsCount": 2},
                    "components": {
                        "bomFormat": "CycloneDX",
                        "components": [
                            {"name": "log4j-core", "version": "2.17.1", "type": "library"},
                            {"name": "axios", "version": "1.6.0", "type": "library"}
                        ]
                    }
                }
            })
            .to_string(),
            received_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn query_reports_round_trips_through_the_wire() {
        let h = Harness::start().await;
        h.seed().await;

        let (reports, total) = h
            .store
            .query_reports("vulnerabilityreport", &QueryParams::default())
            .await
            .unwrap();
        assert_eq!(total, 2);
        assert_eq!(reports.len(), 2);
        // Severity counts survive the JSON envelope.
        let summary = reports[0].summary.as_ref().unwrap();
        assert_eq!(summary.critical, 1);
        assert_eq!(summary.high, 2);
    }

    #[tokio::test]
    async fn query_filters_reach_the_sql_layer() {
        let h = Harness::start().await;
        h.seed().await;

        // A filter that is dropped on the wire would return both clusters.
        let (reports, total) = h
            .store
            .query_reports(
                "vulnerabilityreport",
                &QueryParams {
                    cluster: Some("prod".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(reports[0].cluster, "prod");

        // Severity is sent as a comma-joined list and split server-side.
        let (_, high) = h
            .store
            .query_reports(
                "vulnerabilityreport",
                &QueryParams {
                    severity: Some(vec!["critical".into()]),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(high, 2);

        // A severity no report carries must narrow the result, proving the
        // filter is applied rather than ignored.
        let (_, none) = h
            .store
            .query_reports(
                "vulnerabilityreport",
                &QueryParams {
                    severity: Some(vec!["medium".into()]),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(none, 0);
    }

    #[tokio::test]
    async fn pagination_is_carried_across_the_wire() {
        let h = Harness::start().await;
        h.seed().await;

        let (page, total) = h
            .store
            .query_reports(
                "vulnerabilityreport",
                &QueryParams {
                    limit: Some(1),
                    offset: Some(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.len(), 1, "limit must be honoured");
        assert_eq!(total, 2, "total must count the whole match, not the page");
    }

    #[tokio::test]
    async fn get_report_returns_the_full_data_blob() {
        let h = Harness::start().await;
        h.seed().await;

        let report = h
            .store
            .get_report("prod", "default", "nginx-vuln", "vulnerabilityreport")
            .await
            .unwrap()
            .expect("seeded report");

        assert_eq!(report.meta.cluster, "prod");
        // FullReport serializes `data` as JSON and parses it back into a raw
        // string, so the round trip has to preserve the payload.
        let data: serde_json::Value = serde_json::from_str(&report.data_json).unwrap();
        assert_eq!(
            data["report"]["vulnerabilities"][0]["vulnerabilityID"],
            "CVE-2024-0001"
        );
    }

    #[tokio::test]
    async fn a_missing_report_is_none_rather_than_an_error() {
        let h = Harness::start().await;
        let missing = h
            .store
            .get_report("prod", "default", "nope", "sbomreport")
            .await
            .unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn notes_are_never_served_by_the_scraper() {
        let h = Harness::start().await;
        h.seed().await;

        // The scraper's schema has no notes columns; the server joins them in
        // from the ConfigMap. Anything else would mean the split leaked.
        let report = h
            .store
            .get_report("prod", "default", "nginx-vuln", "vulnerabilityreport")
            .await
            .unwrap()
            .unwrap();
        assert!(report.meta.notes.is_empty());
        assert!(report.meta.notes_created_at.is_none());

        let (reports, _) = h
            .store
            .query_reports("vulnerabilityreport", &QueryParams::default())
            .await
            .unwrap();
        assert!(reports.iter().all(|r| r.notes.is_empty()));
    }

    #[tokio::test]
    async fn aggregates_round_trip() {
        let h = Harness::start().await;
        h.seed().await;

        let stats = h.store.get_stats().await.unwrap();
        assert_eq!(stats.total_clusters, 2);
        assert_eq!(stats.total_vuln_reports, 2);
        assert_eq!(stats.total_sbom_reports, 1);
        assert_eq!(stats.total_critical, 2);

        let clusters = h.store.list_clusters().await.unwrap();
        assert_eq!(clusters.len(), 2);

        let namespaces = h.store.list_namespaces(None).await.unwrap();
        assert_eq!(namespaces, vec!["default".to_string()]);

        // The cluster filter is a query parameter, so a dropped one would
        // still return every namespace.
        let scoped = h.store.list_namespaces(Some("prod")).await.unwrap();
        assert_eq!(scoped, vec!["default".to_string()]);
        assert!(
            h.store
                .list_namespaces(Some("nonexistent"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn search_and_suggest_round_trip() {
        let h = Harness::start().await;
        h.seed().await;

        let (vulns, total) = h
            .store
            .search_vulnerabilities("CVE-2024", 10, 0)
            .await
            .unwrap();
        assert_eq!(total, 4, "two CVEs across two vulnerability reports");
        assert!(vulns.iter().any(|v| v.vulnerability_id == "CVE-2024-0001"));

        let (components, total) = h
            .store
            .search_sbom_components("log4j", 10, 0)
            .await
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(components[0].component_name, "log4j-core");
        assert_eq!(components[0].component_version, "2.17.1");

        let ids = h.store.suggest_vulnerability_ids("CVE", 10).await.unwrap();
        assert!(ids.contains(&"CVE-2024-0001".to_string()));

        let names = h.store.suggest_component_names("axi", 10).await.unwrap();
        assert_eq!(names, vec!["axios".to_string()]);
    }

    #[tokio::test]
    async fn component_matches_carry_the_alert_matcher_filters() {
        let h = Harness::start().await;
        h.seed().await;

        let all = h
            .store
            .list_sbom_component_matches(&[], None, None)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);

        // package_name is pushed into SQL, so a dropped filter would return
        // both components instead of one.
        let scoped = h
            .store
            .list_sbom_component_matches(&["prod".to_string()], Some("default"), Some("log4j-core"))
            .await
            .unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].version, "2.17.1");
        assert_eq!(scoped[0].workload_name, "nginx-sbom");

        // A cluster with no SBOM reports must come back empty.
        assert!(
            h.store
                .list_sbom_component_matches(&["stage".to_string()], None, None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn dashboard_endpoints_round_trip() {
        let h = Harness::start().await;
        h.seed().await;

        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let trends = h
            .store
            .get_live_trends(&today, &today, None, "day")
            .await
            .unwrap();
        assert_eq!(trends.meta.granularity, "day");

        let (from, to) = h.store.get_reports_data_range().await.unwrap();
        assert!(
            from.is_some(),
            "seeded reports give the range a lower bound"
        );
        assert!(to.is_some());
    }

    #[tokio::test]
    async fn hydration_reflects_the_scrapers_own_state() {
        let h = Harness::start().await;

        let empty = h.store.hydration().await.unwrap();
        assert!(!empty.hydrated, "an empty fleet has confirmed nothing");

        h.status.register_cluster("prod");
        h.status
            .set_sync_done("prod", ReportKind::Vulnerability, true);
        let partial = h.store.hydration().await.unwrap();
        assert!(!partial.hydrated);
        assert!(partial.clusters.contains_key("prod"));

        h.status.set_sync_done("prod", ReportKind::Sbom, true);
        assert!(h.store.hydration().await.unwrap().hydrated);
    }

    #[tokio::test]
    async fn writes_round_trip_through_the_ingest_route() {
        let h = Harness::start().await;

        h.store
            .upsert_report(&sbom_payload("prod", "default", "pushed"))
            .await
            .unwrap();
        assert!(
            h.store
                .get_report("prod", "default", "pushed", "sbomreport")
                .await
                .unwrap()
                .is_some()
        );

        assert!(
            h.store
                .delete_report("prod", "default", "pushed", "sbomreport")
                .await
                .unwrap()
        );
        assert!(
            h.store
                .get_report("prod", "default", "pushed", "sbomreport")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn deleting_an_absent_report_is_false_not_an_error() {
        let h = Harness::start().await;
        assert!(
            !h.store
                .delete_report("prod", "default", "nope", "sbomreport")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn deleting_a_cluster_reports_the_row_count() {
        let h = Harness::start().await;
        h.seed().await;

        // prod holds one vulnerability report and one SBOM report.
        assert_eq!(h.store.delete_reports_for_cluster("prod").await.unwrap(), 2);
        assert_eq!(h.store.list_clusters().await.unwrap().len(), 1);
        assert_eq!(h.store.delete_reports_for_cluster("gone").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn identities_that_need_encoding_survive_the_round_trip() {
        let h = Harness::start().await;
        // Cluster names come from a registration Secret and are not guaranteed
        // path-safe, so the encoding has to hold end to end.
        let payload = sbom_payload("prod/eu", "kube-system", "a b");
        h.db.upsert_report(&payload).await.unwrap();

        let report = h
            .store
            .get_report("prod/eu", "kube-system", "a b", "sbomreport")
            .await
            .unwrap()
            .expect("encoded identity must resolve");
        assert_eq!(report.meta.cluster, "prod/eu");
        assert_eq!(report.meta.name, "a b");

        assert_eq!(
            h.store.delete_reports_for_cluster("prod/eu").await.unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn a_wrong_token_fails_every_read() {
        let h = Harness::start_with_token("not-the-token").await;

        let err = h.store.get_stats().await.expect_err("must be refused");
        assert!(err.to_string().contains("401"), "got: {err}");

        // A refused read must not look like an empty answer.
        assert!(h.store.list_clusters().await.is_err());
        assert!(h.store.hydration().await.is_err());
        assert!(
            h.store
                .get_report("prod", "default", "nginx-vuln", "vulnerabilityreport")
                .await
                .is_err(),
            "a 401 must not be reported as a missing report"
        );
    }

    #[tokio::test]
    async fn an_unreachable_scraper_is_an_error_not_an_empty_answer() {
        // Port 1 is reserved and refuses connections, so this exercises the
        // transport-failure path rather than a 500 from a live scraper.
        let store = RemoteStore::new("http://127.0.0.1:1", "shh").unwrap();
        let err = store.get_stats().await.expect_err("must not succeed");
        assert!(err.to_string().contains("Internal API request"));
    }
}
