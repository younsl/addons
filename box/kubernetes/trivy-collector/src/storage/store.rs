//! The read/write surface the web tier depends on, decoupled from SQLite.
//!
//! Two implementations exist. `Database` is the real thing and is used by the
//! scraper, which owns the only SQLite file in the deployment. `RemoteStore`
//! (see `storage::remote`) speaks the scraper's internal HTTP API and is used
//! by the server pods, which hold no database at all.
//!
//! Trait objects and `async fn` in traits do not mix on stable Rust, hence
//! `#[async_trait]`. The alternative — making `AppState` generic over the
//! store — would push a type parameter through every axum handler signature.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::collector::types::ReportPayload;

use super::dashboard::TrendResponse;
use super::database::Database;
use super::models::{
    ClusterInfo, ComponentSearchResult, FullReport, QueryParams, ReportMeta, SbomComponentMatch,
    Stats, VulnSearchResult,
};

/// Initial-sync state of one watched cluster.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterSync {
    pub vuln_watcher_running: bool,
    pub sbom_watcher_running: bool,
    pub vuln_initial_sync_done: bool,
    pub sbom_initial_sync_done: bool,
}

impl ClusterSync {
    /// A cluster is hydrated once both of its watchers have replayed their
    /// initial list. Until then its slice of `reports` is legitimately partial.
    pub fn is_hydrated(&self) -> bool {
        self.vuln_initial_sync_done && self.sbom_initial_sync_done
    }
}

/// Fleet-wide hydration state. Between scraper start and the last `InitDone`
/// the database is incomplete by construction — an `emptyDir` starts empty —
/// so this is surfaced rather than hidden behind a confidently empty answer.
///
/// `watching` separates the two ways an empty report set can come about. A
/// scraper that expects clusters and has none yet is still starting up; one
/// configured to watch nothing is as complete as it will ever be. Collapsing
/// those would either hide a real rebuild or leave a rebuilding notice up
/// forever.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HydrationStatus {
    /// True when there is nothing left to wait for.
    pub hydrated: bool,
    /// False when the scraper watches no clusters at all, which makes an empty
    /// report set the correct answer rather than a temporary one.
    ///
    /// Defaults to true so a server talking to a scraper from before this field
    /// existed keeps the old reading: those always expected watchers.
    #[serde(default = "default_true")]
    pub watching: bool,
    /// Per-cluster detail, keyed by cluster name.
    pub clusters: BTreeMap<String, ClusterSync>,
}

fn default_true() -> bool {
    true
}

impl Default for HydrationStatus {
    fn default() -> Self {
        Self {
            hydrated: false,
            watching: true,
            clusters: BTreeMap::new(),
        }
    }
}

impl HydrationStatus {
    /// A store that is its own source of truth (a direct `Database`) has no
    /// separate scraper to wait on.
    pub fn complete() -> Self {
        Self {
            hydrated: true,
            watching: true,
            clusters: BTreeMap::new(),
        }
    }
}

#[async_trait]
pub trait ReportStore: Send + Sync {
    async fn query_reports(
        &self,
        report_type: &str,
        params: &QueryParams,
    ) -> Result<(Vec<ReportMeta>, i64)>;

    async fn get_report(
        &self,
        cluster: &str,
        namespace: &str,
        name: &str,
        report_type: &str,
    ) -> Result<Option<FullReport>>;

    async fn get_stats(&self) -> Result<Stats>;

    async fn list_clusters(&self) -> Result<Vec<ClusterInfo>>;

    async fn list_namespaces(&self, cluster: Option<&str>) -> Result<Vec<String>>;

    async fn search_vulnerabilities(
        &self,
        query: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<VulnSearchResult>, i64)>;

    async fn search_sbom_components(
        &self,
        component: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ComponentSearchResult>, i64)>;

    async fn suggest_vulnerability_ids(&self, query: &str, limit: i64) -> Result<Vec<String>>;

    async fn suggest_component_names(&self, query: &str, limit: i64) -> Result<Vec<String>>;

    async fn list_sbom_component_matches(
        &self,
        clusters: &[String],
        namespace: Option<&str>,
        package_name: Option<&str>,
    ) -> Result<Vec<SbomComponentMatch>>;

    async fn get_live_trends(
        &self,
        start_date: &str,
        end_date: &str,
        cluster: Option<&str>,
        granularity: &str,
    ) -> Result<TrendResponse>;

    async fn get_reports_data_range(&self) -> Result<(Option<String>, Option<String>)>;

    async fn hydration(&self) -> Result<HydrationStatus>;

    async fn upsert_report(&self, payload: &ReportPayload) -> Result<()>;

    async fn delete_report(
        &self,
        cluster: &str,
        namespace: &str,
        name: &str,
        report_type: &str,
    ) -> Result<bool>;

    async fn delete_reports_for_cluster(&self, cluster: &str) -> Result<u64>;
}

/// Wire form of a paged list response, shared by the scraper's internal API
/// and `RemoteStore` so the two cannot drift.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PagedResponse<T> {
    pub items: Vec<T>,
    pub total: i64,
}

/// Wire form of `get_reports_data_range`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DataRangeResponse {
    pub data_from: Option<String>,
    pub data_to: Option<String>,
}

#[async_trait]
impl ReportStore for Database {
    async fn query_reports(
        &self,
        report_type: &str,
        params: &QueryParams,
    ) -> Result<(Vec<ReportMeta>, i64)> {
        Database::query_reports(self, report_type, params).await
    }

    async fn get_report(
        &self,
        cluster: &str,
        namespace: &str,
        name: &str,
        report_type: &str,
    ) -> Result<Option<FullReport>> {
        Database::get_report(self, cluster, namespace, name, report_type).await
    }

    async fn get_stats(&self) -> Result<Stats> {
        Database::get_stats(self).await
    }

    async fn list_clusters(&self) -> Result<Vec<ClusterInfo>> {
        Database::list_clusters(self).await
    }

    async fn list_namespaces(&self, cluster: Option<&str>) -> Result<Vec<String>> {
        Database::list_namespaces(self, cluster).await
    }

    async fn search_vulnerabilities(
        &self,
        query: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<VulnSearchResult>, i64)> {
        Database::search_vulnerabilities(self, query, limit, offset).await
    }

    async fn search_sbom_components(
        &self,
        component: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ComponentSearchResult>, i64)> {
        Database::search_sbom_components(self, component, limit, offset).await
    }

    async fn suggest_vulnerability_ids(&self, query: &str, limit: i64) -> Result<Vec<String>> {
        Database::suggest_vulnerability_ids(self, query, limit).await
    }

    async fn suggest_component_names(&self, query: &str, limit: i64) -> Result<Vec<String>> {
        Database::suggest_component_names(self, query, limit).await
    }

    async fn list_sbom_component_matches(
        &self,
        clusters: &[String],
        namespace: Option<&str>,
        package_name: Option<&str>,
    ) -> Result<Vec<SbomComponentMatch>> {
        Database::list_sbom_component_matches(self, clusters, namespace, package_name).await
    }

    async fn get_live_trends(
        &self,
        start_date: &str,
        end_date: &str,
        cluster: Option<&str>,
        granularity: &str,
    ) -> Result<TrendResponse> {
        Database::get_live_trends(self, start_date, end_date, cluster, granularity).await
    }

    async fn get_reports_data_range(&self) -> Result<(Option<String>, Option<String>)> {
        Database::get_reports_data_range(self).await
    }

    async fn hydration(&self) -> Result<HydrationStatus> {
        Ok(HydrationStatus::complete())
    }

    async fn upsert_report(&self, payload: &ReportPayload) -> Result<()> {
        Database::upsert_report(self, payload).await
    }

    async fn delete_report(
        &self,
        cluster: &str,
        namespace: &str,
        name: &str,
        report_type: &str,
    ) -> Result<bool> {
        Database::delete_report(self, cluster, namespace, name, report_type).await
    }

    async fn delete_reports_for_cluster(&self, cluster: &str) -> Result<u64> {
        Database::delete_reports_for_cluster(self, cluster).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn cluster_sync_hydrated_needs_both_watchers() {
        let mut s = ClusterSync::default();
        assert!(!s.is_hydrated());
        s.vuln_initial_sync_done = true;
        assert!(!s.is_hydrated());
        s.sbom_initial_sync_done = true;
        assert!(s.is_hydrated());
    }

    #[test]
    fn hydration_status_complete_has_no_clusters() {
        let h = HydrationStatus::complete();
        assert!(h.hydrated);
        assert!(h.clusters.is_empty());
    }

    #[test]
    fn a_hydration_payload_without_watching_reads_as_watching() {
        // A new server can talk to a scraper from before the field existed
        // during a rolling upgrade. Those always expected watchers, so the
        // missing field must not be read as "watches nothing".
        let back: HydrationStatus =
            serde_json::from_str(r#"{"hydrated":false,"clusters":{}}"#).unwrap();
        assert!(back.watching);
        assert!(!back.hydrated);
    }

    #[test]
    fn hydration_status_roundtrips() {
        let mut h = HydrationStatus::default();
        h.clusters.insert(
            "prod".to_string(),
            ClusterSync {
                vuln_watcher_running: true,
                sbom_watcher_running: true,
                vuln_initial_sync_done: true,
                sbom_initial_sync_done: false,
            },
        );
        let json = serde_json::to_string(&h).unwrap();
        let back: HydrationStatus = serde_json::from_str(&json).unwrap();
        assert!(!back.hydrated);
        assert!(back.watching);
        assert!(!back.clusters["prod"].is_hydrated());
    }

    #[tokio::test]
    async fn database_satisfies_report_store() {
        let db = Database::new(":memory:").await.unwrap();
        let store: Arc<dyn ReportStore> = Arc::new(db);

        assert!(store.list_clusters().await.unwrap().is_empty());
        assert_eq!(store.get_stats().await.unwrap().total_clusters, 0);
        assert!(store.hydration().await.unwrap().hydrated);
    }
}
