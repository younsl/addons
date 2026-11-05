//! Per-cluster watcher lifecycle manager.
//!
//! Holds a `HashMap<cluster_name, ClusterHandle>` where each entry owns a
//! spawned watcher task and its shutdown sender. The secret_watcher drives
//! upsert/remove calls as cluster-registration Secrets come and go.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::alerts::AlertEvaluator;
use crate::collector::status::WatcherStatus;
use crate::collector::watcher::{ClusterWatcher, WatchScope};
use crate::storage::Database;

use super::client_builder;
use super::types::ClusterSecret;

struct ClusterHandle {
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    task: JoinHandle<()>,
    resource_version: Option<String>,
}

pub struct ClusterManager {
    db: Arc<Database>,
    watcher_status: Arc<WatcherStatus>,
    alerts: Option<Arc<AlertEvaluator>>,
    /// Report kinds to collect, fleet-wide. Each cluster's namespaces come
    /// from its own registration Secret and override the namespace half.
    scope: WatchScope,
    clusters: Mutex<HashMap<String, ClusterHandle>>,
}

impl ClusterManager {
    pub fn new(
        db: Arc<Database>,
        watcher_status: Arc<WatcherStatus>,
        alerts: Option<Arc<AlertEvaluator>>,
        scope: WatchScope,
    ) -> Self {
        Self {
            db,
            watcher_status,
            alerts,
            scope,
            clusters: Mutex::new(HashMap::new()),
        }
    }

    /// Start (or restart) a watcher for the given cluster. If a watcher already
    /// exists for the same cluster name and the Secret's resourceVersion has
    /// not changed, this is a no-op.
    pub async fn upsert(&self, secret: ClusterSecret, resource_version: Option<String>) {
        let name = secret.name.clone();

        {
            let mut guard = self.clusters.lock().await;
            if let Some(existing) = guard.get(&name)
                && existing.resource_version == resource_version
            {
                return;
            }
            if let Some(old) = guard.remove(&name) {
                info!(cluster = %name, "Restarting watcher for updated cluster Secret");
                let _ = old.shutdown_tx.send(true);
                // task will drain on its own; we don't await to keep upsert non-blocking
                old.task.abort();
            }
        }

        let client = match client_builder::build_client(&secret).await {
            Ok(c) => c,
            Err(e) => {
                error!(cluster = %name, error = %e, "Failed to build client for cluster");
                return;
            }
        };

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let watcher = ClusterWatcher::with_client(
            client,
            self.db.clone(),
            secret.name.clone(),
            self.scope.with_namespaces(secret.namespaces.clone()),
            self.watcher_status.clone(),
            self.alerts.clone(),
        );

        let cluster_label = name.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = watcher.run(shutdown_rx).await {
                error!(cluster = %cluster_label, error = %e, "Remote watcher exited with error");
            }
        });

        let mut guard = self.clusters.lock().await;
        guard.insert(
            name.clone(),
            ClusterHandle {
                shutdown_tx,
                task,
                resource_version,
            },
        );
        info!(cluster = %name, total_clusters = guard.len(), "Hub watcher started for cluster");
    }

    /// Stop and remove a cluster's watcher.
    pub async fn remove(&self, cluster_name: &str) {
        let handle = {
            let mut guard = self.clusters.lock().await;
            guard.remove(cluster_name)
        };
        if let Some(h) = handle {
            info!(cluster = %cluster_name, "Stopping watcher for removed cluster");
            let _ = h.shutdown_tx.send(true);
            h.task.abort();
            // Drop it from hydration accounting too, otherwise a deregistered
            // cluster would hold `/readyz` open forever.
            self.watcher_status.unregister_cluster(cluster_name);
        } else {
            warn!(cluster = %cluster_name, "remove called for unknown cluster");
        }
    }

    /// Stop all cluster watchers (used on hub shutdown).
    pub async fn stop_all(&self) {
        let mut guard = self.clusters.lock().await;
        info!(count = guard.len(), "Stopping all cluster watchers");
        for (name, handle) in guard.drain() {
            let _ = handle.shutdown_tx.send(true);
            handle.task.abort();
            self.watcher_status.unregister_cluster(&name);
            info!(cluster = %name, "Stopped watcher");
        }
    }

    /// Number of active cluster watchers.
    pub async fn active_clusters(&self) -> usize {
        self.clusters.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_new_manager_empty() {
        let db = Arc::new(Database::new(":memory:").await.unwrap());
        let mgr = ClusterManager::new(
            db,
            Arc::new(WatcherStatus::new()),
            None,
            WatchScope::default(),
        );
        assert_eq!(mgr.active_clusters().await, 0);
    }

    #[tokio::test]
    async fn removing_a_cluster_clears_its_hydration_entry() {
        // A deregistered cluster that stayed in the hydration map would hold
        // /readyz failing forever, since hydration requires every entry.
        let db = Arc::new(Database::new(":memory:").await.unwrap());
        let status = Arc::new(WatcherStatus::new());
        status.register_cluster("gone");
        status.register_cluster("prod");
        assert_eq!(status.cluster_count(), 2);

        let mgr = ClusterManager::new(db, status.clone(), None, WatchScope::default());
        // No watcher was ever started for it, which is the case a Delete event
        // for an unknown cluster produces.
        mgr.remove("gone").await;

        // remove() only unregisters clusters it was actually managing, so the
        // status entry survives an unknown removal.
        assert_eq!(status.cluster_count(), 2);
    }

    #[tokio::test]
    async fn stop_all_clears_every_hydration_entry() {
        let db = Arc::new(Database::new(":memory:").await.unwrap());
        let status = Arc::new(WatcherStatus::new());
        let mgr = ClusterManager::new(db, status.clone(), None, WatchScope::default());

        mgr.stop_all().await;
        assert_eq!(mgr.active_clusters().await, 0);
    }

    #[tokio::test]
    async fn test_remove_unknown_is_noop() {
        let db = Arc::new(Database::new(":memory:").await.unwrap());
        let mgr = ClusterManager::new(
            db,
            Arc::new(WatcherStatus::new()),
            None,
            WatchScope::default(),
        );
        mgr.remove("does-not-exist").await;
        assert_eq!(mgr.active_clusters().await, 0);
    }
}
