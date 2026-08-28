//! Per-cluster watcher status, owned by the scraper.
//!
//! Two global booleans could not represent a fleet: with one scraper watching
//! N registered clusters, "initial sync done" is a property of each cluster's
//! pair of watchers, not of the process. Since the database now starts empty on
//! every restart, that distinction is load-bearing — an empty dashboard during
//! a rebuild must be distinguishable from a real answer.

use std::collections::BTreeMap;
use std::sync::RwLock;

use crate::storage::{ClusterSync, HydrationStatus};

#[derive(Debug)]
pub struct WatcherStatus {
    clusters: RwLock<BTreeMap<String, ClusterSync>>,
    /// Whether any watcher is ever going to start.
    ///
    /// An empty cluster map means two different things, and only the
    /// configuration can tell them apart: a scraper that expects clusters is
    /// still starting up, while one configured to watch nothing has already
    /// arrived. Without this, the second case would report "rebuilding" for the
    /// life of the pod.
    expects_clusters: bool,
}

impl Default for WatcherStatus {
    fn default() -> Self {
        Self {
            clusters: RwLock::new(BTreeMap::new()),
            // The safe assumption: an empty map is treated as a rebuild in
            // progress rather than as a complete answer.
            expects_clusters: true,
        }
    }
}

/// Which of a cluster's two watchers a status update refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    Vulnerability,
    Sbom,
}

impl ReportKind {
    /// The `report_type` value this kind is stored under in `reports`.
    pub fn as_report_type(self) -> &'static str {
        match self {
            ReportKind::Vulnerability => "vulnerabilityreport",
            ReportKind::Sbom => "sbomreport",
        }
    }
}

impl WatcherStatus {
    /// Status for a scraper that expects at least one cluster.
    pub fn new() -> Self {
        Self::default()
    }

    /// Status for a scraper with no watchers configured at all, whose empty
    /// report set is the final answer rather than a transient one.
    pub fn watching_nothing() -> Self {
        Self {
            expects_clusters: false,
            ..Self::default()
        }
    }

    /// Whether any cluster is being watched.
    pub fn is_watching(&self) -> bool {
        self.expects_clusters
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, ClusterSync>> {
        self.clusters.read().expect("watcher status poisoned")
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, ClusterSync>> {
        self.clusters.write().expect("watcher status poisoned")
    }

    /// Announce a cluster before its watchers start, so `/readyz` counts it as
    /// pending rather than reporting ready on an empty fleet.
    pub fn register_cluster(&self, cluster: &str) {
        self.write().entry(cluster.to_string()).or_default();
    }

    /// Drop a cluster whose registration was removed. Its reports are purged
    /// separately; leaving it here would hold hydration open forever.
    pub fn unregister_cluster(&self, cluster: &str) {
        self.write().remove(cluster);
    }

    pub fn set_running(&self, cluster: &str, kind: ReportKind, running: bool) {
        let mut guard = self.write();
        let entry = guard.entry(cluster.to_string()).or_default();
        match kind {
            ReportKind::Vulnerability => entry.vuln_watcher_running = running,
            ReportKind::Sbom => entry.sbom_watcher_running = running,
        }
    }

    pub fn set_sync_done(&self, cluster: &str, kind: ReportKind, done: bool) {
        let mut guard = self.write();
        let entry = guard.entry(cluster.to_string()).or_default();
        match kind {
            ReportKind::Vulnerability => entry.vuln_initial_sync_done = done,
            ReportKind::Sbom => entry.sbom_initial_sync_done = done,
        }
    }

    pub fn cluster(&self, cluster: &str) -> Option<ClusterSync> {
        self.read().get(cluster).copied()
    }

    pub fn cluster_count(&self) -> usize {
        self.read().len()
    }

    /// True once there is nothing left to wait for.
    ///
    /// With watchers expected, an empty fleet is not hydrated: nothing has
    /// confirmed anything yet, and the window before the first cluster
    /// registers must not read as a complete answer. With no watchers
    /// configured there is nothing to confirm, so it is hydrated immediately.
    pub fn is_hydrated(&self) -> bool {
        if !self.expects_clusters {
            return true;
        }
        let guard = self.read();
        !guard.is_empty() && guard.values().all(ClusterSync::is_hydrated)
    }

    pub fn snapshot(&self) -> HydrationStatus {
        HydrationStatus {
            hydrated: self.is_hydrated(),
            watching: self.expects_clusters,
            clusters: self.read().clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_kind_maps_to_its_stored_report_type() {
        assert_eq!(
            ReportKind::Vulnerability.as_report_type(),
            "vulnerabilityreport"
        );
        assert_eq!(ReportKind::Sbom.as_report_type(), "sbomreport");
    }

    #[test]
    fn a_fresh_status_is_empty_and_not_hydrated() {
        let s = WatcherStatus::new();
        assert_eq!(s.cluster_count(), 0);
        assert!(!s.is_hydrated());
        assert!(!s.snapshot().hydrated);
    }

    #[test]
    fn registering_a_cluster_makes_it_pending() {
        let s = WatcherStatus::new();
        s.register_cluster("prod");
        assert_eq!(s.cluster_count(), 1);
        assert!(!s.is_hydrated());
        assert_eq!(s.cluster("prod"), Some(ClusterSync::default()));
    }

    #[test]
    fn hydration_needs_both_watchers_of_every_cluster() {
        let s = WatcherStatus::new();
        s.register_cluster("prod");
        s.register_cluster("stage");

        s.set_sync_done("prod", ReportKind::Vulnerability, true);
        s.set_sync_done("prod", ReportKind::Sbom, true);
        assert!(!s.is_hydrated(), "stage has not synced yet");

        s.set_sync_done("stage", ReportKind::Vulnerability, true);
        assert!(!s.is_hydrated(), "stage sbom has not synced yet");

        s.set_sync_done("stage", ReportKind::Sbom, true);
        assert!(s.is_hydrated());
    }

    #[test]
    fn set_running_is_tracked_per_cluster_and_kind() {
        let s = WatcherStatus::new();
        s.set_running("prod", ReportKind::Vulnerability, true);

        let prod = s.cluster("prod").unwrap();
        assert!(prod.vuln_watcher_running);
        assert!(!prod.sbom_watcher_running);
        assert!(s.cluster("stage").is_none());
    }

    #[test]
    fn unregistering_a_cluster_stops_holding_hydration_open() {
        let s = WatcherStatus::new();
        s.register_cluster("prod");
        s.register_cluster("gone");
        s.set_sync_done("prod", ReportKind::Vulnerability, true);
        s.set_sync_done("prod", ReportKind::Sbom, true);
        assert!(!s.is_hydrated());

        s.unregister_cluster("gone");
        assert!(s.is_hydrated());
    }

    #[test]
    fn a_scraper_watching_nothing_is_hydrated_immediately() {
        // Its empty report set is the final answer, not a transient one, so it
        // must not read as a rebuild for the life of the pod.
        let s = WatcherStatus::watching_nothing();
        assert!(s.is_hydrated());
        assert!(!s.is_watching());

        let snap = s.snapshot();
        assert!(snap.hydrated);
        assert!(!snap.watching);
        assert!(snap.clusters.is_empty());
    }

    #[test]
    fn an_expectant_scraper_is_not_hydrated_before_its_first_cluster() {
        // The window between process start and the local watcher registering
        // must not report an empty database as a complete answer.
        let s = WatcherStatus::new();
        assert!(s.is_watching());
        assert!(!s.is_hydrated());
        assert!(s.snapshot().watching);
    }

    #[test]
    fn watching_nothing_stays_hydrated_even_if_a_cluster_appears_unsynced() {
        // Defensive: the flag describes configuration, so a stray registration
        // cannot flip a deployment that watches nothing back into rebuilding.
        let s = WatcherStatus::watching_nothing();
        s.register_cluster("stray");
        assert!(s.is_hydrated());
    }

    #[test]
    fn snapshot_reports_every_cluster() {
        let s = WatcherStatus::new();
        s.register_cluster("prod");
        s.set_running("prod", ReportKind::Sbom, true);

        let snap = s.snapshot();
        assert!(!snap.hydrated);
        assert!(snap.watching);
        assert_eq!(snap.clusters.len(), 1);
        assert!(snap.clusters["prod"].sbom_watcher_running);
    }
}
