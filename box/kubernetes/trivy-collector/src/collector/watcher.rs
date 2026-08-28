//! Kubernetes watchers that project Trivy report CRs into the scraper's
//! database.
//!
//! The scraper does not author reports. Each `kube::runtime::watcher` stream
//! starts with `Event::Init`, a full paginated list, and `Event::InitDone`, so a
//! scraper starting against an empty database rebuilds the complete report set
//! from the source of truth with no extra code and no extra API calls beyond
//! the ones it already makes on every restart. That is what lets the volume go
//! away: `reports` is a mirror, not a record.
//!
//! One `ClusterWatcher` exists per watched cluster — the hub's own plus one per
//! registered edge — and each runs the same generic watch loop twice, once per
//! report kind.

use std::sync::Arc;

use anyhow::{Context, Result};
use futures::StreamExt;
use kube::{
    Client, Resource,
    api::Api,
    runtime::watcher::{Config as WatcherConfig, Event, watcher},
};
use serde::{Serialize, de::DeserializeOwned};
use tracing::{debug, error, info, warn};

use crate::alerts::AlertEvaluator;
use crate::collector::status::{ReportKind, WatcherStatus};
use crate::collector::types::{ReportPayload, SbomReport, VulnerabilityReport};
use crate::config::Config;
use crate::storage::Database;

/// Page size for the watcher's initial list. SBOM reports can be very large,
/// so the default of 500 is a memory hazard on a big fleet.
const WATCH_PAGE_SIZE: u32 = 50;

/// A Trivy report CRD the watcher can mirror.
pub trait TrivyReport:
    Resource<DynamicType = (), Scope = k8s_openapi::NamespaceResourceScope>
    + Clone
    + DeserializeOwned
    + Serialize
    + std::fmt::Debug
    + Send
    + Sync
    + 'static
{
    const KIND: ReportKind;

    /// Short, kind-specific summary for the ingest log line.
    fn summary(&self) -> String;
}

impl TrivyReport for VulnerabilityReport {
    const KIND: ReportKind = ReportKind::Vulnerability;

    fn summary(&self) -> String {
        format!(
            "critical={} high={}",
            self.report.summary.critical_count, self.report.summary.high_count
        )
    }
}

impl TrivyReport for SbomReport {
    const KIND: ReportKind = ReportKind::Sbom;

    fn summary(&self) -> String {
        format!("components={}", self.report.summary.components_count)
    }
}

/// What a watcher is asked to cover: which namespaces, and which report kinds.
///
/// Grouped into one value because both constructors need it and it is derived
/// from config in one place, which is also what makes `COLLECT_VULN` and
/// `COLLECT_SBOM` actually mean something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchScope {
    /// Namespaces to watch. Empty means every namespace.
    pub namespaces: Vec<String>,
    pub collect_vulnerability_reports: bool,
    pub collect_sbom_reports: bool,
}

impl WatchScope {
    pub fn from_config(config: &Config) -> Self {
        Self {
            namespaces: config.namespaces.clone(),
            collect_vulnerability_reports: config.collect_vulnerability_reports,
            collect_sbom_reports: config.collect_sbom_reports,
        }
    }

    /// Same report kinds, different namespaces. Edge clusters carry their own
    /// namespace list in their registration Secret, while the report kinds are
    /// a fleet-wide setting.
    pub fn with_namespaces(&self, namespaces: Vec<String>) -> Self {
        Self {
            namespaces,
            ..self.clone()
        }
    }

    pub fn collects(&self, kind: ReportKind) -> bool {
        match kind {
            ReportKind::Vulnerability => self.collect_vulnerability_reports,
            ReportKind::Sbom => self.collect_sbom_reports,
        }
    }
}

impl Default for WatchScope {
    fn default() -> Self {
        Self {
            namespaces: Vec::new(),
            collect_vulnerability_reports: true,
            collect_sbom_reports: true,
        }
    }
}

/// Watches one cluster's Trivy CRs and mirrors them into the local database.
pub struct ClusterWatcher {
    client: Client,
    db: Arc<Database>,
    cluster_name: String,
    scope: WatchScope,
    status: Arc<WatcherStatus>,
    /// Alert evaluation hangs off the upsert path, where the previous
    /// `data_json` needed for net-new diffing is already in hand.
    alerts: Option<Arc<AlertEvaluator>>,
}

impl ClusterWatcher {
    /// Build a watcher against the ambient in-cluster (or kubeconfig) client.
    pub async fn local(
        db: Arc<Database>,
        cluster_name: String,
        scope: WatchScope,
        status: Arc<WatcherStatus>,
        alerts: Option<Arc<AlertEvaluator>>,
    ) -> Result<Self> {
        let client = Client::try_default()
            .await
            .context("Failed to create Kubernetes client")?;
        Ok(Self::with_client(
            client,
            db,
            cluster_name,
            scope,
            status,
            alerts,
        ))
    }

    /// Build a watcher bound to a pre-built client, used by hub-pull mode when
    /// the client is derived from a registered cluster Secret.
    pub fn with_client(
        client: Client,
        db: Arc<Database>,
        cluster_name: String,
        scope: WatchScope,
        status: Arc<WatcherStatus>,
        alerts: Option<Arc<AlertEvaluator>>,
    ) -> Self {
        Self {
            client,
            db,
            cluster_name,
            scope,
            status,
            alerts,
        }
    }

    pub async fn run(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
        info!(
            cluster = %self.cluster_name,
            namespaces = ?self.scope.namespaces,
            vuln = self.scope.collect_vulnerability_reports,
            sbom = self.scope.collect_sbom_reports,
            "Starting cluster watcher"
        );
        self.status.register_cluster(&self.cluster_name);

        let vuln = self.spawn_watch::<VulnerabilityReport>(shutdown.clone());
        let sbom = self.spawn_watch::<SbomReport>(shutdown.clone());

        tokio::select! {
            _ = shutdown.changed() => {
                info!(cluster = %self.cluster_name, "Cluster watcher shutdown signal received");
            }
            result = vuln => {
                if let Err(e) = result {
                    error!(cluster = %self.cluster_name, error = %e, "VulnerabilityReport watcher failed");
                }
            }
            result = sbom => {
                if let Err(e) = result {
                    error!(cluster = %self.cluster_name, error = %e, "SbomReport watcher failed");
                }
            }
        }

        Ok(())
    }

    /// Start one report kind's watch, or resolve immediately when that kind is
    /// switched off.
    ///
    /// A disabled kind is marked synced rather than left pending: hydration
    /// requires every registered cluster's pair of flags, so leaving it unset
    /// would hold `/readyz` failing forever.
    fn spawn_watch<K: TrivyReport>(
        &self,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        let kind = K::KIND;
        if !self.scope.collects(kind) {
            info!(
                cluster = %self.cluster_name,
                report_type = kind.as_report_type(),
                "Collection disabled — not watching this report kind"
            );
            self.status.set_sync_done(&self.cluster_name, kind, true);
            return tokio::spawn(async {});
        }

        let ctx = WatchContext {
            db: self.db.clone(),
            cluster_name: self.cluster_name.clone(),
            namespaces: self.scope.namespaces.clone(),
            status: self.status.clone(),
            alerts: self.alerts.clone(),
        };
        let client = self.client.clone();
        tokio::spawn(async move { ctx.watch::<K>(client, shutdown).await })
    }
}

/// Everything one watch loop needs, cloned out of `ClusterWatcher` so the loop
/// can be spawned without borrowing it.
struct WatchContext {
    db: Arc<Database>,
    cluster_name: String,
    namespaces: Vec<String>,
    status: Arc<WatcherStatus>,
    alerts: Option<Arc<AlertEvaluator>>,
}

/// Counters for one initial-sync pass, logged at `InitDone`.
struct SyncProgress {
    count: u64,
    started: Option<std::time::Instant>,
}

impl SyncProgress {
    fn new() -> Self {
        Self {
            count: 0,
            started: None,
        }
    }

    fn start(&mut self) {
        self.count = 0;
        self.started = Some(std::time::Instant::now());
    }

    fn increment(&mut self) {
        self.count += 1;
    }

    fn elapsed_secs(&self) -> f64 {
        self.started
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0)
    }
}

impl WatchContext {
    async fn watch<K: TrivyReport>(
        &self,
        client: Client,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let kind = K::KIND;
        let report_type = kind.as_report_type();
        let api: Api<K> = Api::all(client);
        let mut stream = watcher(api, WatcherConfig::default().page_size(WATCH_PAGE_SIZE)).boxed();

        self.status.set_running(&self.cluster_name, kind, true);
        info!(cluster = %self.cluster_name, report_type = %report_type, "Watcher started");

        let mut progress = SyncProgress::new();

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    info!(cluster = %self.cluster_name, report_type = %report_type, "Watcher shutting down");
                    self.status.set_running(&self.cluster_name, kind, false);
                    break;
                }
                event = stream.next() => {
                    match event {
                        Some(Ok(ev)) => {
                            if let Err(e) = self.handle_event::<K>(ev, &mut progress).await {
                                error!(
                                    cluster = %self.cluster_name,
                                    report_type = %report_type,
                                    error = %e,
                                    "Failed to handle watch event"
                                );
                            }
                        }
                        Some(Err(e)) => {
                            error!(cluster = %self.cluster_name, report_type = %report_type, error = %e, "Watcher error");
                        }
                        None => {
                            warn!(cluster = %self.cluster_name, report_type = %report_type, "Watcher stream ended");
                            self.status.set_running(&self.cluster_name, kind, false);
                            break;
                        }
                    }
                }
            }
        }
    }

    async fn handle_event<K: TrivyReport>(
        &self,
        event: Event<K>,
        progress: &mut SyncProgress,
    ) -> Result<()> {
        let kind = K::KIND;
        let report_type = kind.as_report_type();

        match event {
            Event::Apply(report) | Event::InitApply(report) => {
                let Some(id) = self.identity(&report) else {
                    return Ok(());
                };
                let payload = ReportPayload {
                    cluster: self.cluster_name.clone(),
                    report_type: report_type.to_string(),
                    namespace: id.namespace,
                    name: id.name,
                    data_json: serde_json::to_string(&report)?,
                    received_at: chrono::Utc::now(),
                };

                // Read the previous revision before overwriting it so alerts
                // fire only on net-new findings.
                let previous = self.previous_data_json(&payload).await;
                self.db.upsert_report(&payload).await?;

                info!(
                    cluster = %payload.cluster,
                    report_type = %report_type,
                    namespace = %payload.namespace,
                    name = %payload.name,
                    summary = %report.summary(),
                    "Report stored"
                );

                self.evaluate_alerts(payload, previous);
                progress.increment();
            }
            Event::Delete(report) => {
                let Some(id) = self.identity(&report) else {
                    return Ok(());
                };
                self.db
                    .delete_report(&self.cluster_name, &id.namespace, &id.name, report_type)
                    .await?;
                info!(
                    cluster = %self.cluster_name,
                    report_type = %report_type,
                    namespace = %id.namespace,
                    name = %id.name,
                    "Report deleted"
                );
            }
            Event::Init => {
                progress.start();
                info!(cluster = %self.cluster_name, report_type = %report_type, "Initial sync started");
            }
            Event::InitDone => {
                self.status.set_sync_done(&self.cluster_name, kind, true);
                info!(
                    cluster = %self.cluster_name,
                    report_type = %report_type,
                    reports_synced = progress.count,
                    elapsed_secs = format!("{:.2}", progress.elapsed_secs()),
                    "Initial sync completed"
                );
            }
        }

        Ok(())
    }

    /// Namespace and name of a report, or `None` when the namespace is outside
    /// the watched set.
    fn identity<K: TrivyReport>(&self, report: &K) -> Option<ReportIdentity> {
        report_identity(&self.namespaces, report.meta())
    }

    /// The stored `data_json` for this identity, if any. Only fetched when an
    /// evaluator is attached — it exists solely for net-new diffing.
    async fn previous_data_json(&self, payload: &ReportPayload) -> Option<String> {
        // Only fetched when an evaluator is attached.
        self.alerts.as_ref()?;
        self.db
            .get_report(
                &payload.cluster,
                &payload.namespace,
                &payload.name,
                &payload.report_type,
            )
            .await
            .ok()
            .flatten()
            .map(|r| r.data_json)
    }

    /// Dispatch alert evaluation off the watch loop.
    ///
    /// Evaluation stays suppressed until hydration completes: a rebuild replays
    /// every report in the fleet through `InitApply`, and without this guard
    /// each one would look net-new and re-fire.
    fn evaluate_alerts(&self, payload: ReportPayload, previous: Option<String>) {
        let Some(evaluator) = self.alerts.clone() else {
            return;
        };
        if !self.status.is_hydrated() {
            debug!(
                cluster = %payload.cluster,
                name = %payload.name,
                "Alert evaluation suppressed until hydration completes"
            );
            return;
        }
        let db = self.db.clone();
        tokio::spawn(async move {
            evaluator
                .evaluate(&payload, previous.as_deref(), db.as_ref())
                .await;
        });
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ReportIdentity {
    namespace: String,
    name: String,
}

/// Resolve a report's `(namespace, name)`, or `None` when its namespace falls
/// outside the watched set. An empty set means every namespace.
///
/// Kept a free function so the filter is testable without a database.
fn report_identity(
    namespaces: &[String],
    meta: &k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
) -> Option<ReportIdentity> {
    let namespace = meta.namespace.as_deref().unwrap_or("default");
    if !namespaces.is_empty() && !namespaces.iter().any(|ns| ns == namespace) {
        debug!(namespace = %namespace, "Skipping report from non-watched namespace");
        return None;
    }
    Some(ReportIdentity {
        namespace: namespace.to_string(),
        name: meta.name.as_deref().unwrap_or("unknown").to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    fn vuln(namespace: Option<&str>, name: Option<&str>) -> VulnerabilityReport {
        VulnerabilityReport {
            types: None,
            metadata: ObjectMeta {
                namespace: namespace.map(str::to_string),
                name: name.map(str::to_string),
                ..Default::default()
            },
            report: Default::default(),
        }
    }

    #[test]
    fn report_kinds_map_to_their_stored_report_type() {
        assert_eq!(
            <VulnerabilityReport as TrivyReport>::KIND.as_report_type(),
            "vulnerabilityreport"
        );
        assert_eq!(
            <SbomReport as TrivyReport>::KIND.as_report_type(),
            "sbomreport"
        );
    }

    #[test]
    fn summaries_are_kind_specific() {
        assert!(vuln(None, None).summary().starts_with("critical="));
        let sbom = SbomReport {
            types: None,
            metadata: ObjectMeta::default(),
            report: Default::default(),
        };
        assert!(sbom.summary().starts_with("components="));
    }

    #[test]
    fn scope_from_config_carries_the_collect_flags() {
        let mut config = Config::for_test(crate::config::Mode::Scraper);
        config.namespaces = vec!["kube-system".to_string()];
        config.collect_sbom_reports = false;

        let scope = WatchScope::from_config(&config);
        assert_eq!(scope.namespaces, vec!["kube-system".to_string()]);
        assert!(scope.collects(ReportKind::Vulnerability));
        assert!(!scope.collects(ReportKind::Sbom));
    }

    #[test]
    fn scope_defaults_to_collecting_both_kinds_everywhere() {
        let scope = WatchScope::default();
        assert!(scope.namespaces.is_empty());
        assert!(scope.collects(ReportKind::Vulnerability));
        assert!(scope.collects(ReportKind::Sbom));
    }

    #[test]
    fn with_namespaces_overrides_only_the_namespace_half() {
        // An edge cluster brings its own namespaces; the report kinds stay a
        // fleet-wide setting.
        let base = WatchScope {
            namespaces: vec!["hub-only".to_string()],
            collect_vulnerability_reports: true,
            collect_sbom_reports: false,
        };
        let edge = base.with_namespaces(vec!["edge-ns".to_string()]);

        assert_eq!(edge.namespaces, vec!["edge-ns".to_string()]);
        assert!(edge.collects(ReportKind::Vulnerability));
        assert!(!edge.collects(ReportKind::Sbom));
    }

    #[test]
    fn a_disabled_kind_is_marked_synced_so_hydration_can_complete() {
        // Hydration requires both flags of every registered cluster. A kind
        // nobody watches must not hold /readyz failing forever.
        let status = WatcherStatus::new();
        status.register_cluster("prod");
        status.set_sync_done("prod", ReportKind::Vulnerability, true);
        assert!(!status.is_hydrated());

        // This is what spawn_watch does for a disabled kind.
        status.set_sync_done("prod", ReportKind::Sbom, true);
        assert!(status.is_hydrated());
    }

    #[test]
    fn identity_defaults_a_missing_namespace_and_name() {
        let id = report_identity(&[], vuln(None, None).meta()).expect("no filter set");
        assert_eq!(
            id,
            ReportIdentity {
                namespace: "default".to_string(),
                name: "unknown".to_string(),
            }
        );
    }

    #[test]
    fn identity_passes_a_watched_namespace() {
        let watched = vec!["kube-system".to_string(), "default".to_string()];
        let id = report_identity(&watched, vuln(Some("default"), Some("nginx")).meta())
            .expect("default is watched");
        assert_eq!(id.namespace, "default");
        assert_eq!(id.name, "nginx");
    }

    #[test]
    fn identity_filters_out_an_unwatched_namespace() {
        let watched = vec!["kube-system".to_string()];
        assert!(report_identity(&watched, vuln(Some("default"), Some("nginx")).meta()).is_none());
    }

    // ───── Event handling against a real database ─────
    //
    // `handle_event` is the whole mirroring contract: Apply upserts, Delete
    // removes, InitDone flips hydration, and namespaces outside the scope are
    // skipped. It is also the reason the volume could go away, so it gets
    // exercised rather than assumed.

    async fn context(namespaces: Vec<String>) -> WatchContext {
        WatchContext {
            db: Arc::new(Database::new(":memory:").await.unwrap()),
            cluster_name: "prod".to_string(),
            namespaces,
            status: Arc::new(WatcherStatus::new()),
            alerts: None,
        }
    }

    fn sbom(namespace: &str, name: &str, components: usize) -> SbomReport {
        let mut report = SbomReport {
            types: None,
            metadata: ObjectMeta {
                namespace: Some(namespace.to_string()),
                name: Some(name.to_string()),
                ..Default::default()
            },
            report: Default::default(),
        };
        report.report.summary.components_count = components as i64;
        report
    }

    #[tokio::test]
    async fn apply_mirrors_a_report_into_the_database() {
        let ctx = context(vec![]).await;
        let mut progress = SyncProgress::new();

        ctx.handle_event(
            Event::Apply(vuln(Some("default"), Some("nginx"))),
            &mut progress,
        )
        .await
        .unwrap();

        let stored = ctx
            .db
            .get_report("prod", "default", "nginx", "vulnerabilityreport")
            .await
            .unwrap();
        assert!(stored.is_some(), "Apply must write the row");
        assert_eq!(progress.count, 1);
    }

    #[tokio::test]
    async fn init_apply_mirrors_the_same_way_as_apply() {
        // The initial list arrives as InitApply, which is how an empty database
        // is rebuilt on every restart. Treating it differently would defeat the
        // whole design.
        let ctx = context(vec![]).await;
        let mut progress = SyncProgress::new();

        ctx.handle_event(
            Event::InitApply(vuln(Some("default"), Some("nginx"))),
            &mut progress,
        )
        .await
        .unwrap();

        assert!(
            ctx.db
                .get_report("prod", "default", "nginx", "vulnerabilityreport")
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(progress.count, 1);
    }

    #[tokio::test]
    async fn apply_is_idempotent_on_the_same_identity() {
        let ctx = context(vec![]).await;
        let mut progress = SyncProgress::new();

        for _ in 0..3 {
            ctx.handle_event(
                Event::Apply(vuln(Some("default"), Some("nginx"))),
                &mut progress,
            )
            .await
            .unwrap();
        }

        // A relist re-sends every report, so upsert has to collapse duplicates
        // rather than accumulate rows.
        assert_eq!(
            ctx.db.count_reports("vulnerabilityreport").await.unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn delete_removes_the_mirrored_row() {
        let ctx = context(vec![]).await;
        let mut progress = SyncProgress::new();

        ctx.handle_event(
            Event::Apply(vuln(Some("default"), Some("nginx"))),
            &mut progress,
        )
        .await
        .unwrap();
        ctx.handle_event(
            Event::Delete(vuln(Some("default"), Some("nginx"))),
            &mut progress,
        )
        .await
        .unwrap();

        assert!(
            ctx.db
                .get_report("prod", "default", "nginx", "vulnerabilityreport")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_unwatched_namespace_is_neither_written_nor_counted() {
        let ctx = context(vec!["kube-system".to_string()]).await;
        let mut progress = SyncProgress::new();

        ctx.handle_event(
            Event::Apply(vuln(Some("default"), Some("nginx"))),
            &mut progress,
        )
        .await
        .unwrap();

        assert_eq!(
            ctx.db.count_reports("vulnerabilityreport").await.unwrap(),
            0
        );
        assert_eq!(progress.count, 0);
    }

    #[tokio::test]
    async fn deleting_in_an_unwatched_namespace_is_a_noop() {
        let ctx = context(vec!["kube-system".to_string()]).await;
        let mut progress = SyncProgress::new();

        // Seed directly so the row exists despite the namespace filter, then
        // confirm a filtered Delete leaves it alone.
        ctx.db
            .upsert_report(&ReportPayload {
                cluster: "prod".into(),
                report_type: "vulnerabilityreport".into(),
                namespace: "default".into(),
                name: "nginx".into(),
                data_json: "{}".into(),
                received_at: chrono::Utc::now(),
            })
            .await
            .unwrap();

        ctx.handle_event(
            Event::Delete(vuln(Some("default"), Some("nginx"))),
            &mut progress,
        )
        .await
        .unwrap();

        assert_eq!(
            ctx.db.count_reports("vulnerabilityreport").await.unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn both_report_kinds_mirror_under_their_own_report_type() {
        let ctx = context(vec![]).await;
        let mut progress = SyncProgress::new();

        ctx.handle_event(
            Event::Apply(vuln(Some("default"), Some("nginx"))),
            &mut progress,
        )
        .await
        .unwrap();
        ctx.handle_event(Event::Apply(sbom("default", "nginx", 7)), &mut progress)
            .await
            .unwrap();

        // Same namespace and name, different kind: the UNIQUE key includes
        // report_type, so both must survive.
        assert_eq!(
            ctx.db.count_reports("vulnerabilityreport").await.unwrap(),
            1
        );
        assert_eq!(ctx.db.count_reports("sbomreport").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn init_done_flips_that_kinds_hydration_only() {
        let ctx = context(vec![]).await;
        let mut progress = SyncProgress::new();
        ctx.status.register_cluster("prod");

        ctx.handle_event::<VulnerabilityReport>(Event::InitDone, &mut progress)
            .await
            .unwrap();

        let sync = ctx.status.cluster("prod").unwrap();
        assert!(sync.vuln_initial_sync_done);
        assert!(!sync.sbom_initial_sync_done);
        assert!(!ctx.status.is_hydrated(), "one kind is still pending");

        ctx.handle_event::<SbomReport>(Event::InitDone, &mut progress)
            .await
            .unwrap();
        assert!(ctx.status.is_hydrated());
    }

    #[tokio::test]
    async fn init_resets_the_progress_counter() {
        let ctx = context(vec![]).await;
        let mut progress = SyncProgress::new();

        ctx.handle_event(
            Event::Apply(vuln(Some("default"), Some("nginx"))),
            &mut progress,
        )
        .await
        .unwrap();
        assert_eq!(progress.count, 1);

        // A watch stream that reconnects replays Init, and the counter it
        // reports at InitDone has to describe that pass alone.
        ctx.handle_event::<VulnerabilityReport>(Event::Init, &mut progress)
            .await
            .unwrap();
        assert_eq!(progress.count, 0);
    }

    #[tokio::test]
    async fn no_previous_revision_is_fetched_without_an_evaluator() {
        // The lookup exists only to diff net-new findings for alerts, so it
        // must not cost a query when nothing consumes it.
        let ctx = context(vec![]).await;
        let payload = ReportPayload {
            cluster: "prod".into(),
            report_type: "sbomreport".into(),
            namespace: "default".into(),
            name: "nginx".into(),
            data_json: "{}".into(),
            received_at: chrono::Utc::now(),
        };
        ctx.db.upsert_report(&payload).await.unwrap();

        assert!(ctx.previous_data_json(&payload).await.is_none());
    }

    #[test]
    fn sync_progress_counts_and_resets() {
        let mut p = SyncProgress::new();
        assert_eq!(p.elapsed_secs(), 0.0);
        p.increment();
        assert_eq!(p.count, 1);

        p.start();
        assert_eq!(p.count, 0);
        assert!(p.started.is_some());
    }
}
