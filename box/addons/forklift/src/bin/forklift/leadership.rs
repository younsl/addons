use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use forklift::{
    audit, auth, cluster, config, coverage, insights, meta, notify, objstore, replication, repo,
    server,
};
use prometheus::IntCounter;
use tokio_util::sync::CancellationToken;

use crate::FINAL_SYNC_TIMEOUT;
use crate::LOG_TARGET;
use crate::coverage_schedule::run_coverage_schedule;

/// Writes everything this process owns declaratively: the bootstrap admin, the
/// chart-provided RBAC policy and the seeded default repositories. It is
/// idempotent, and it runs twice on purpose.
///
/// The first call happens at startup, before anything else, so a malformed
/// policy or an unreachable database fails the process immediately instead of
/// surfacing later as a half-configured instance. The second happens on
/// leadership acquisition, after the promotion path has swapped in the
/// authoritative database. Without that second call the work would be silently
/// discarded in the s3 and replication topologies: promotion replaces the whole
/// local database with the snapshot it fetches, so state written before it never
/// reaches the object store, and the next leader reads the same stale snapshot
/// again. That made every chart policy edit a no-op after the first boot on an
/// empty bucket.
pub(crate) async fn apply_declarative_state(
    cfg: &config::Config,
    store: &Arc<meta::Store>,
    authz: &Arc<auth::Service>,
) -> anyhow::Result<()> {
    authz
        .bootstrap_admin(
            &cfg.auth.bootstrap_admin_user,
            &cfg.auth.bootstrap_admin_password,
        )
        .await
        .context("bootstrap admin")?;
    // Declarative RBAC: reconcile chart-provided roles, grants, group mappings
    // and local accounts. No-op when no policy file is configured.
    auth::reconcile_rbac(
        Arc::clone(store),
        &cfg.auth.rbac.policy_file,
        &cfg.auth.rbac.accounts_dir,
    )
    .await
    .context("reconcile rbac")?;
    if cfg.seed_default_repos {
        repo::seed_defaults(store)
            .await
            .context("seed default repositories")?;
    }
    Ok(())
}

/// Everything the leadership callbacks touch.
pub(crate) struct Leadership {
    pub(crate) cfg: Arc<config::Config>,
    pub(crate) store: Arc<meta::Store>,
    pub(crate) authz: Arc<auth::Service>,
    pub(crate) engine: Arc<repo::Engine>,
    pub(crate) manager: Arc<repo::Manager>,
    pub(crate) recorder: Option<Arc<audit::Recorder>>,
    pub(crate) coverage: Arc<coverage::Scanner>,
    pub(crate) insights: Option<Arc<insights::Analyzer>>,
    pub(crate) notifier: Arc<notify::Notifier>,
    pub(crate) meta_sync: Option<Arc<objstore::MetaSync>>,
    pub(crate) replicator: Option<Arc<replication::Replicator>>,
    pub(crate) elector: Option<Arc<cluster::Elector>>,
    pub(crate) ready: server::Ready,
    pub(crate) leader_gauge: prometheus::Gauge,
    pub(crate) leader_transitions: IntCounter,
    pub(crate) leader_state: Arc<AtomicBool>,
    pub(crate) label_routing: bool,
}

impl Leadership {
    /// The blob sweeper and audit retention are gated on leadership. In
    /// single-instance mode this process is always the leader; in HA mode a
    /// Kubernetes Lease elects exactly one active instance so SQLite has a
    /// single writer. With replication enabled, the replicated snapshot is
    /// applied before this instance takes traffic.
    pub(crate) async fn start_leading(self: Arc<Self>, cancel: CancellationToken) {
        self.leader_gauge.set(1.0);
        self.leader_transitions.inc();
        self.leader_state.store(true, Ordering::SeqCst);
        if let Some(replicator) = &self.replicator
            && let Err(e) = replicator.promote().await
        {
            tracing::error!(target: LOG_TARGET, err = %e, "replication: promote failed; serving local data");
        }
        // In s3 mode, apply the latest metadata snapshot before serving so the
        // new leader takes traffic on current data. The fencing token (Lease
        // transition count) tags snapshot uploads so a superseded leader cannot
        // overwrite this term's metadata.
        if let Some(sync) = &self.meta_sync {
            let mut fence = 0i64;
            if let Some(elector) = &self.elector {
                match elector.fencing_token().await {
                    Ok(t) => fence = t,
                    Err(e) => {
                        tracing::warn!(target: LOG_TARGET, err = %e, "objstore: read fencing token failed; using 0");
                    }
                }
            }
            if let Err(e) = sync.promote(fence).await {
                // Fail closed. Without the current snapshot this process cannot
                // know how far behind its local database is, and leading on
                // stale metadata is what produces dangling blob references: the
                // sweeper reclaims bytes a newer state still refers to. Skip the
                // leader label so no traffic is routed here and start no
                // background job that mutates state; another replica (or this
                // one, on the next term) can take over cleanly.
                tracing::error!(target: LOG_TARGET, err = %e, "objstore: promote failed; refusing to lead on stale metadata");
                self.refuse_to_lead().await;
                return;
            }
        }
        // Re-apply the declarative state on the database this term actually
        // serves. Promotion above may have replaced it wholesale with the
        // snapshot from the object store, discarding what the startup pass
        // wrote; re-running here is what makes a chart policy edit take effect.
        // Refuse to lead if it fails, for the same reason a failed promotion
        // does: serving as leader on state we could not reconcile would publish
        // that state to every later term.
        if let Err(e) = apply_declarative_state(&self.cfg, &self.store, &self.authz).await {
            tracing::error!(target: LOG_TARGET, err = %format!("{e:#}"), "declarative state apply failed; refusing to lead");
            self.refuse_to_lead().await;
            return;
        }
        self.ready.set(true);
        self.set_pod_role(cluster::ROLE_LEADER).await;
        // A partitioned former leader may not have removed its own leader
        // label; strip it so the Service routes to this pod only.
        let cfg = &self.cfg;
        if self.label_routing
            && !cfg.replication.pod_name.is_empty()
            && let Some(elector) = &self.elector
            && let Err(e) = elector
                .demote_peers(&cfg.replication.pod_namespace, &cfg.replication.pod_name)
                .await
        {
            tracing::error!(target: LOG_TARGET, err = %e, "demote peer leader labels");
        }
        self.spawn_leader_jobs(&cancel);
    }

    /// Steps back from a term this process cannot serve correctly: no leader
    /// label, no leader gauge, and no leader-only background job.
    async fn refuse_to_lead(&self) {
        self.set_pod_role(cluster::ROLE_STANDBY).await;
        self.leader_gauge.set(0.0);
        self.leader_state.store(false, Ordering::SeqCst);
    }

    /// Starts every background job that mutates shared state, each bound to
    /// the term's `cancel` token.
    fn spawn_leader_jobs(&self, cancel: &CancellationToken) {
        let cfg = &self.cfg;
        tokio::spawn(Arc::clone(&self.engine).run_sweeper(
            cancel.clone(),
            Duration::from_mins(5),
            config::BLOB_GC_GRACE,
        ));
        tokio::spawn(
            Arc::clone(&self.manager).run_idle_reaper(cancel.clone(), Duration::from_hours(1)),
        );
        tokio::spawn(Arc::clone(&self.manager).run_oci_prune(
            cancel.clone(),
            cfg.oci.prune_interval,
            cfg.oci.upload_session_ttl,
        ));
        // Vulnerability scan worker pool + backfill (scans already-stored
        // artifacts) + periodic re-scanner (no-ops without a scanner). Multiple
        // workers drain the queue concurrently so freshly cached coordinates are
        // scanned promptly under burst.
        for _ in 0..cfg.vuln.workers.max(1) {
            tokio::spawn(Arc::clone(&self.manager).run_vuln_worker(cancel.clone()));
        }
        tokio::spawn(
            Arc::clone(&self.manager).run_vuln_backfill(cancel.clone(), cfg.vuln.rescan_interval),
        );
        tokio::spawn(Arc::clone(&self.manager).run_vuln_rescanner(
            cancel.clone(),
            cfg.vuln.rescan_interval,
            cfg.vuln.ttl,
        ));
        // License resolution worker pool + backfill + periodic re-resolver
        // (no-ops without a resolver).
        for _ in 0..cfg.license.workers.max(1) {
            tokio::spawn(Arc::clone(&self.manager).run_license_worker(cancel.clone()));
        }
        tokio::spawn(
            Arc::clone(&self.manager)
                .run_license_backfill(cancel.clone(), cfg.license.rescan_interval),
        );
        tokio::spawn(Arc::clone(&self.manager).run_license_rescanner(
            cancel.clone(),
            cfg.license.rescan_interval,
            cfg.license.ttl,
        ));
        if let Some(recorder) = &self.recorder {
            tokio::spawn(Arc::clone(recorder).run_retention(
                cancel.clone(),
                Duration::from_hours(1),
                audit::Retention {
                    entries: cfg.audit.retention,
                    downloads: cfg.audit.download_retention,
                },
            ));
        }
        if let Some(analyzer) = &self.insights {
            tokio::spawn(Arc::clone(analyzer).run(cancel.clone(), Arc::clone(&self.notifier)));
        }
        if self.coverage.enabled() {
            tokio::spawn(run_coverage_schedule(
                cancel.clone(),
                Arc::clone(&self.store),
                Arc::clone(&self.coverage),
                Arc::clone(&self.notifier),
            ));
        }
    }

    pub(crate) async fn stop_leading(self: Arc<Self>) {
        self.leader_gauge.set(0.0);
        self.leader_state.store(false, Ordering::SeqCst);
        if let Some(sync) = &self.meta_sync {
            // Flush the writes made since the last cycle, then resume
            // downloading the new leader's snapshots.
            let _ = tokio::time::timeout(FINAL_SYNC_TIMEOUT, sync.demote()).await;
        }
        if let Some(replicator) = &self.replicator {
            replicator.demote();
        }
        if self.label_routing {
            // Stay Ready so rollouts proceed; moving the forklift.io/role=leader
            // label is what redirects traffic to the new leader.
            let _ = tokio::time::timeout(
                Duration::from_secs(10),
                self.set_pod_role(cluster::ROLE_STANDBY),
            )
            .await;
            return;
        }
        // Shared-volume HA without label routing: readiness gates the leader.
        self.ready.set(false);
    }

    async fn set_pod_role(&self, role: &str) {
        let cfg = &self.cfg;
        if !self.label_routing || cfg.replication.pod_name.is_empty() {
            return;
        }
        let Some(elector) = &self.elector else {
            return;
        };
        if let Err(e) = elector
            .set_pod_role(
                &cfg.replication.pod_namespace,
                &cfg.replication.pod_name,
                role,
            )
            .await
        {
            tracing::error!(target: LOG_TARGET, role = %role, err = %e, "set pod role label");
        }
    }
}
