//! The scraper: the only process in the deployment that opens a database.
//!
//! It watches the hub's own cluster plus every registered edge cluster, mirrors
//! their Trivy CRs into SQLite on an `emptyDir`, evaluates alert rules on the
//! ingest path, and serves that data back to the stateless server pods over an
//! internal HTTP API. Because it owns the file exclusively, the server tier
//! needs no volume and can scale and roll freely.

pub mod api;
pub mod status;
pub mod types;
pub mod watcher;

use anyhow::Result;
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::alerts::AlertEvaluator;
use crate::config::Config;
use crate::health::HealthServer;
use crate::hub::{self, HubConfig};
use crate::metrics::Metrics;
use crate::storage::Database;

use status::WatcherStatus;
use watcher::{ClusterWatcher, WatchScope};

/// How often the readiness loop re-checks fleet hydration.
const READINESS_POLL_SECS: u64 = 2;

pub async fn run(
    config: Config,
    health_server: HealthServer,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    metrics: Arc<Metrics>,
) -> Result<()> {
    info!(
        cluster = %config.cluster_name,
        namespaces = ?config.namespaces,
        storage_path = %config.storage_path,
        hub_secret_namespace = %config.hub_secret_namespace,
        internal_port = config.internal_port,
        "Starting scraper (sole database owner, hub-pull)"
    );

    let db = Arc::new(Database::new(&config.get_db_path()).await?);

    // The configuration is the only thing that knows whether an empty cluster
    // map means "still starting" or "nothing to watch", so it is decided once
    // here and every consumer reads it back off the status.
    let watcher_status = Arc::new(if watches_nothing(&config) {
        warn!(
            "No watchers configured — the report set will stay empty, and that \
             is reported as a complete answer rather than a rebuild"
        );
        WatcherStatus::watching_nothing()
    } else {
        WatcherStatus::new()
    });

    // Alert evaluation lives here rather than in the server: this is where
    // writes happen, so this is where a net-new finding can be detected.
    let alerts = match crate::alerts::build_evaluator(&config.external_url).await {
        Ok(eval) => Some(Arc::new(eval)),
        Err(e) => {
            warn!(error = %e, "Alerts subsystem disabled (Kubernetes API unavailable)");
            None
        }
    };

    // Internal read API the server pods proxy through.
    let api_handle = {
        let server = api::InternalApi::new(
            db.clone(),
            watcher_status.clone(),
            config.internal_token.clone(),
        );
        let port = config.internal_port;
        let shutdown_rx = shutdown.clone();
        tokio::spawn(async move {
            if let Err(e) = server.serve(port, shutdown_rx).await {
                error!(error = %e, "Internal API server failed");
            }
        })
    };

    // One place derives the watch scope, so COLLECT_VULN / COLLECT_SBOM apply
    // identically to the hub's own cluster and to every registered edge.
    let scope = WatchScope::from_config(&config);

    let local_handle =
        spawn_local_watcher(&config, &db, &watcher_status, &alerts, &scope, &shutdown);
    let hub_handle = spawn_hub_watcher(&config, &db, &watcher_status, &alerts, &scope, &shutdown);

    // Readiness follows hydration: until every registered cluster has replayed
    // its initial list, the report set is legitimately incomplete and the
    // scraper must not be routed to.
    let readiness = spawn_readiness_loop(
        health_server,
        watcher_status.clone(),
        metrics.clone(),
        shutdown.clone(),
    );

    let metrics_handle = spawn_metrics_refresh(&db, &metrics, &shutdown);

    let _ = shutdown.changed().await;
    info!("Scraper shutdown signal received");

    for handle in [
        Some(api_handle),
        local_handle,
        hub_handle,
        Some(readiness),
        Some(metrics_handle),
    ]
    .into_iter()
    .flatten()
    {
        let _ = handle.await;
    }

    Ok(())
}

/// Watcher for the hub's own cluster, when trivy-operator is deployed there.
fn spawn_local_watcher(
    config: &Config,
    db: &Arc<Database>,
    watcher_status: &Arc<WatcherStatus>,
    alerts: &Option<Arc<AlertEvaluator>>,
    scope: &WatchScope,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !config.watch_local {
        return None;
    }

    let db = db.clone();
    let status = watcher_status.clone();
    let alerts = alerts.clone();
    let scope = scope.clone();
    let cluster_name = config.cluster_name.clone();
    let shutdown_rx = shutdown.clone();

    info!(cluster = %cluster_name, namespaces = ?scope.namespaces, "Local watcher enabled");

    Some(tokio::spawn(async move {
        match ClusterWatcher::local(db, cluster_name, scope, status, alerts).await {
            Ok(w) => {
                if let Err(e) = w.run(shutdown_rx).await {
                    error!(error = %e, "Local watcher exited with error");
                }
            }
            Err(e) => warn!(error = %e, "Failed to create local watcher — skipping"),
        }
    }))
}

/// Secret watcher that spawns one `ClusterWatcher` per registered edge cluster.
fn spawn_hub_watcher(
    config: &Config,
    db: &Arc<Database>,
    watcher_status: &Arc<WatcherStatus>,
    alerts: &Option<Arc<AlertEvaluator>>,
    scope: &WatchScope,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> Option<tokio::task::JoinHandle<()>> {
    if config.hub_secret_namespace.trim().is_empty() {
        warn!(
            "HUB_SECRET_NAMESPACE is empty \
             (Downward API not wired or running outside a pod) \
             — skipping Edge cluster watcher"
        );
        return None;
    }

    let hub_cfg = HubConfig {
        secret_namespace: config.hub_secret_namespace.clone(),
        cluster_name: config.cluster_name.clone(),
        namespaces: config.namespaces.clone(),
    };
    let db = db.clone();
    let status = watcher_status.clone();
    let alerts = alerts.clone();
    let scope = scope.clone();
    let shutdown_rx = shutdown.clone();
    let watch_local = config.watch_local;
    let cluster_name = config.cluster_name.clone();
    let namespaces = config.namespaces.clone();

    info!(
        secret_namespace = %hub_cfg.secret_namespace,
        label_selector = %hub_cfg.label_selector(),
        "Hub Secret watcher enabled"
    );

    Some(tokio::spawn(async move {
        // Self-register the hub's own cluster as a display-only Secret so it
        // shows up alongside registered edges in the UI. Only when the local
        // watcher is active — otherwise we'd list a cluster nobody watches.
        if watch_local
            && let Err(e) = hub::self_register::ensure_local_cluster_secret(
                &hub_cfg.secret_namespace,
                &cluster_name,
                &namespaces,
            )
            .await
        {
            warn!(error = %e, "self-register: non-fatal failure");
        }

        if let Err(e) = hub::run(hub_cfg, db, status, alerts, scope, shutdown_rx).await {
            error!(error = %e, "Hub Secret watcher exited with error");
        }
    }))
}

/// True when no watcher will ever start, so hydration can never complete.
///
/// Readiness has to short-circuit in that case: the fleet is legitimately
/// empty rather than still rebuilding, and waiting for a sync nobody will run
/// would leave the pod permanently unready.
fn watches_nothing(config: &Config) -> bool {
    !config.watch_local && config.hub_secret_namespace.trim().is_empty()
}

/// Flip `/readyz` to follow hydration.
///
/// `WatcherStatus` already accounts for a deployment that watches nothing, so
/// this loop has one rule and the UI banner reads the same answer. Two separate
/// judgements of the same condition is how they came to disagree before.
fn spawn_readiness_loop(
    health_server: HealthServer,
    watcher_status: Arc<WatcherStatus>,
    metrics: Arc<Metrics>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(READINESS_POLL_SECS));
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = interval.tick() => {
                    let hydrated = watcher_status.is_hydrated();
                    metrics.record_hydration(watcher_status.cluster_count(), hydrated);
                    if hydrated != health_server.is_ready() {
                        if hydrated {
                            info!(
                                clusters = watcher_status.cluster_count(),
                                "Fleet hydrated — scraper is ready"
                            );
                        } else {
                            info!("Fleet no longer hydrated — scraper is not ready");
                        }
                        health_server.set_ready(hydrated);
                    }
                }
            }
        }
    })
}

/// Refresh the database gauges the scraper owns.
fn spawn_metrics_refresh(
    db: &Arc<Database>,
    metrics: &Arc<Metrics>,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let db = db.clone();
    let metrics = metrics.clone();
    let mut shutdown = shutdown.clone();

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = interval.tick() => {
                    metrics.refresh_db_gauges(&db).await;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::status::ReportKind;
    use prometheus_client::registry::Registry;

    fn health() -> HealthServer {
        HealthServer::new(Arc::new(Registry::default()))
    }

    fn metrics() -> Arc<Metrics> {
        let mut registry = Registry::default();
        Metrics::new(&mut registry, crate::config::Mode::Scraper)
    }

    /// Poll until the predicate holds or the budget runs out. The readiness
    /// loop runs on its own task, so its effect is observed rather than
    /// awaited.
    async fn eventually(mut check: impl FnMut() -> bool) -> bool {
        for _ in 0..100 {
            if check() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        false
    }

    #[test]
    fn a_deployment_with_no_watchers_is_recognised() {
        let mut config = Config::for_test(crate::config::Mode::Scraper);

        config.watch_local = true;
        config.hub_secret_namespace = String::new();
        assert!(!watches_nothing(&config), "the local watcher still runs");

        config.watch_local = false;
        config.hub_secret_namespace = "trivy-system".to_string();
        assert!(!watches_nothing(&config), "edge clusters are still watched");

        config.watch_local = false;
        config.hub_secret_namespace = "   ".to_string();
        assert!(
            watches_nothing(&config),
            "a blank hub namespace is not a hub namespace"
        );
    }

    #[tokio::test]
    async fn readiness_short_circuits_when_nothing_is_watched() {
        let health = health();
        let status = Arc::new(WatcherStatus::watching_nothing());
        let (tx, rx) = tokio::sync::watch::channel(false);
        let task = spawn_readiness_loop(health.clone(), status, metrics(), rx);

        assert!(
            eventually(|| health.is_ready()).await,
            "a fleet nobody watches must not wait for a sync that never comes"
        );
        let _ = tx.send(true);
        let _ = task.await;
    }

    #[tokio::test]
    async fn readiness_waits_for_every_registered_cluster() {
        let health = health();
        let status = Arc::new(WatcherStatus::new());
        let (tx, rx) = tokio::sync::watch::channel(false);
        let task = spawn_readiness_loop(health.clone(), status.clone(), metrics(), rx);

        status.register_cluster("prod");
        status.register_cluster("stage");
        status.set_sync_done("prod", ReportKind::Vulnerability, true);
        status.set_sync_done("prod", ReportKind::Sbom, true);

        // prod is done but stage has not started, so serving now would answer
        // from a partial report set.
        assert!(!health.is_ready());

        status.set_sync_done("stage", ReportKind::Vulnerability, true);
        status.set_sync_done("stage", ReportKind::Sbom, true);
        assert!(eventually(|| health.is_ready()).await);

        let _ = tx.send(true);
        let _ = task.await;
    }

    #[tokio::test]
    async fn readiness_is_withdrawn_when_a_new_cluster_starts_syncing() {
        let health = health();
        let status = Arc::new(WatcherStatus::new());
        let (tx, rx) = tokio::sync::watch::channel(false);
        let task = spawn_readiness_loop(health.clone(), status.clone(), metrics(), rx);

        status.register_cluster("prod");
        status.set_sync_done("prod", ReportKind::Vulnerability, true);
        status.set_sync_done("prod", ReportKind::Sbom, true);
        assert!(eventually(|| health.is_ready()).await);

        // A cluster registered later is unhydrated, and the fleet answer is
        // incomplete again until it catches up.
        status.register_cluster("newly-registered");
        assert!(eventually(|| !health.is_ready()).await);

        let _ = tx.send(true);
        let _ = task.await;
    }

    #[tokio::test]
    async fn readiness_stops_on_shutdown() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let task = spawn_readiness_loop(health(), Arc::new(WatcherStatus::new()), metrics(), rx);

        let _ = tx.send(true);
        // A loop that ignored shutdown would hang this test.
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("readiness loop must observe shutdown")
            .unwrap();
    }
}
