//! Identifies `PersistentVolumeClaims` and `PersistentVolumes` that no
//! workload is using, so the EBS volumes behind them stop billing unnoticed.
//!
//! By default it only identifies. "Unused" is an observation about the current
//! state of the cluster, not a statement that the data is disposable, so
//! deleting a claim is opt-in through orphan PVC cleanup, and nothing here ever deletes
//! a volume or touches EC2.
//!
//! The report is deliberately conservative about what counts as in use. A
//! claim mounted by a Pod in a terminal phase is unused (a completed Job's Pod
//! object outlives its run and mounts nothing), while a `volumeClaimTemplate`
//! claim inside its `StatefulSet`'s replica range is in use even with no Pod
//! at all, because every rolling update passes through that gap.

pub mod annotations;
pub mod decide;
pub mod defaults;
pub mod kube;
pub mod orphan_pvc_cleanup;
pub mod types;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use tracing::{debug, error, info, warn};

use crate::autoscaler::instance_volume::resizer::AlertNotifier;
use crate::k8s::events::{Emitter, TYPE_NORMAL, TYPE_WARNING, Target};
pub use annotations::{build_annotations, key};
pub use decide::classify;
pub use defaults::{INTERVAL, MIN_UNUSED_AGE};
pub use kube::KubeClient;
pub use orphan_pvc_cleanup::{
    Policy as OrphanPvcCleanupPolicy, Record as OrphanPvcRecord, State as OrphanPvcState, Verdict,
};
pub use types::*;

/// The subset of Kubernetes operations this module depends on. Lists and
/// annotation patches, plus the claim deletion that only orphan PVC cleanup calls.
#[async_trait]
pub trait KubeApi: Send + Sync {
    async fn inventory(&self) -> Result<Inventory, String>;
    async fn annotate_pvc(
        &self,
        namespace: &str,
        name: &str,
        set: &std::collections::BTreeMap<String, String>,
        remove: &[String],
    ) -> Result<(), String>;
    async fn annotate_pv(
        &self,
        name: &str,
        set: &std::collections::BTreeMap<String, String>,
        remove: &[String],
    ) -> Result<(), String>;
    /// `namespace/name` of every claim a workload's Pod template names.
    async fn workload_claim_references(&self) -> Result<std::collections::HashSet<String>, String>;
    /// Whether a live Pod mounts the claim right now.
    async fn claim_mounted(&self, namespace: &str, name: &str) -> Result<bool, String>;
    async fn delete_pvc(&self, namespace: &str, name: &str, uid: &str) -> Result<(), String>;
    /// Reads the controller-owned orphan PVC cleanup state. A missing store
    /// reads as empty.
    async fn load_orphan_state(&self) -> Result<OrphanPvcState, String>;
    /// Replaces the controller-owned orphan PVC cleanup state.
    async fn save_orphan_state(&self, state: &OrphanPvcState) -> Result<(), String>;
}

/// Receives metrics observations. `observability::Metrics` implements it.
pub trait Recorder: Send + Sync {
    fn reset_unused_volumes(&self);
    #[allow(clippy::too_many_arguments)]
    fn observe_unused_pvc(
        &self,
        namespace: &str,
        name: &str,
        volume_name: &str,
        volume_id: &str,
        storage_class: &str,
        reason: &str,
        age_seconds: f64,
        capacity_bytes: f64,
    );
    #[allow(clippy::too_many_arguments)]
    fn observe_unused_pv(
        &self,
        name: &str,
        volume_id: &str,
        storage_class: &str,
        reason: &str,
        reclaim_policy: &str,
        claim_namespace: &str,
        claim_name: &str,
        age_seconds: f64,
        capacity_bytes: f64,
    );
    fn observe_unused_summary(&self, kind: &str, reason: &str, count: usize, capacity_bytes: i64);
    fn observe_orphan_pvc_cleanup(&self, action: &str);
    fn observe_orphan_pvc_cleanup_scheduled(&self, count: usize);
    fn observe_error(&self, stage: &str);
}

impl Recorder for crate::observability::Metrics {
    fn reset_unused_volumes(&self) {
        Self::reset_unused_volumes(self);
    }
    fn observe_unused_pvc(
        &self,
        namespace: &str,
        name: &str,
        volume_name: &str,
        volume_id: &str,
        storage_class: &str,
        reason: &str,
        age_seconds: f64,
        capacity_bytes: f64,
    ) {
        Self::observe_unused_pvc(
            self,
            namespace,
            name,
            volume_name,
            volume_id,
            storage_class,
            reason,
            age_seconds,
            capacity_bytes,
        );
    }
    fn observe_unused_pv(
        &self,
        name: &str,
        volume_id: &str,
        storage_class: &str,
        reason: &str,
        reclaim_policy: &str,
        claim_namespace: &str,
        claim_name: &str,
        age_seconds: f64,
        capacity_bytes: f64,
    ) {
        Self::observe_unused_pv(
            self,
            name,
            volume_id,
            storage_class,
            reason,
            reclaim_policy,
            claim_namespace,
            claim_name,
            age_seconds,
            capacity_bytes,
        );
    }
    fn observe_unused_summary(&self, kind: &str, reason: &str, count: usize, capacity_bytes: i64) {
        Self::observe_unused_summary(self, kind, reason, count, capacity_bytes);
    }
    fn observe_orphan_pvc_cleanup(&self, action: &str) {
        Self::observe_orphan_pvc_cleanup(self, action);
    }
    fn observe_orphan_pvc_cleanup_scheduled(&self, count: usize) {
        Self::observe_orphan_pvc_cleanup_scheduled(self, count);
    }
    fn observe_error(&self, stage: &str) {
        Self::observe_error(self, stage);
    }
}

/// A recorder that drops every observation, for the CLI, which reports to
/// stdout rather than to the metrics registry the daemon serves.
pub struct DiscardRecorder;

impl Recorder for DiscardRecorder {
    fn reset_unused_volumes(&self) {}
    fn observe_unused_pvc(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: f64,
        _: f64,
    ) {
    }
    fn observe_unused_pv(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: f64,
        _: f64,
    ) {
    }
    fn observe_unused_summary(&self, _: &str, _: &str, _: usize, _: i64) {}
    fn observe_orphan_pvc_cleanup(&self, _: &str) {}
    fn observe_orphan_pvc_cleanup_scheduled(&self, _: usize) {}
    fn observe_error(&self, _: &str) {}
}

/// Kubernetes Event reasons published against a claim or a volume. Both are
/// Normal, not Warning: an unused claim is a cost observation, and a cluster
/// with a hundred of them would drown the Warnings that mean something is
/// failing.
pub const REASON_UNUSED_DETECTED: &str = "UnusedVolumeDetected";
pub const REASON_UNUSED_CLEARED: &str = "UnusedVolumeCleared";
/// Cleanup Events. Scheduled is a Warning because it is the last chance to
/// keep the data. It replaces `UnusedVolumeDetected` while it stands.
pub const REASON_DELETION_SCHEDULED: &str = "OrphanPvcDeletionScheduled";
pub const REASON_DELETION_CANCELED: &str = "OrphanPvcDeletionCanceled";
pub const REASON_DELETED: &str = "OrphanPvcDeleted";
/// Alertmanager alert names. Upcoming is sent once, `alertBefore` ahead of
/// the deadline. Deleted reuses the Event reason.
pub const ALERT_DELETION_UPCOMING: &str = "OrphanPvcDeletionUpcoming";
pub const ALERT_DELETION_FAILED: &str = "OrphanPvcDeletionFailed";

/// Outcomes of one object's annotation attempt.
pub const OUTCOME_WRITTEN: &str = "written";
pub const OUTCOME_UNCHANGED: &str = "unchanged";
pub const OUTCOME_DRY_RUN: &str = "dry_run";

/// Evaluates every claim and volume in the cluster once per pass.
pub struct Scanner {
    /// Suppresses the annotation patches and the Events. The scan itself
    /// still runs and still reports: reading the cluster is not a mutation.
    dry_run: bool,
    kube: Arc<dyn KubeApi>,
    rec: Arc<dyn Recorder>,
    events: Option<Emitter>,
    /// `MIN_UNUSED_AGE`, held as a field so tests can exercise the threshold
    /// without waiting a day.
    min_unused_age: Duration,
    /// Injectable so tests control the unused clock.
    now: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    /// `None` keeps the scanner identify-only and cancels every deletion an
    /// earlier run scheduled.
    orphan_pvc_cleanup: Option<OrphanPvcCleanupPolicy>,
    /// Alertmanager alerts for scheduled, deleted, and failed orphan PVC
    /// deletions. `None` while alerts are off.
    orphan_pvc_alerts: Option<Arc<dyn AlertNotifier>>,
}

/// What orphan PVC cleanup may do in one pass.
enum OrphanPvcCleanupState<'a> {
    Off,
    /// Enabled, but the workload references or the stored state could not
    /// be read. Nothing is scheduled, canceled, or deleted until a pass can
    /// read them.
    Hold,
    Active(
        &'a OrphanPvcCleanupPolicy,
        std::collections::HashSet<String>,
        OrphanPvcState,
    ),
}

impl Scanner {
    /// Constructs a scanner. `events` may be `None` to disable Kubernetes
    /// Events.
    #[must_use]
    pub fn new(
        dry_run: bool,
        kube: Arc<dyn KubeApi>,
        rec: Arc<dyn Recorder>,
        events: Option<Emitter>,
    ) -> Self {
        Self {
            dry_run,
            kube,
            rec,
            events,
            min_unused_age: MIN_UNUSED_AGE,
            now: Box::new(Utc::now),
            orphan_pvc_cleanup: None,
            orphan_pvc_alerts: None,
        }
    }

    #[must_use]
    pub fn with_orphan_pvc_alerts(mut self, notifier: Option<Arc<dyn AlertNotifier>>) -> Self {
        self.orphan_pvc_alerts = notifier;
        self
    }

    #[must_use]
    pub fn with_orphan_pvc_cleanup(mut self, policy: Option<OrphanPvcCleanupPolicy>) -> Self {
        self.orphan_pvc_cleanup = policy;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_clock(
        mut self,
        min_unused_age: Duration,
        now: impl Fn() -> DateTime<Utc> + Send + Sync + 'static,
    ) -> Self {
        self.min_unused_age = min_unused_age;
        self.now = Box::new(now);
        self
    }

    /// Classifies every claim and volume and publishes the result, returning
    /// the number of objects considered. Per-object annotation failures are
    /// logged and counted but never abort the pass.
    pub async fn reconcile(&self) -> Result<usize, anyhow::Error> {
        let inv = match self.kube.inventory().await {
            Ok(inv) => inv,
            Err(err) => {
                self.rec.observe_error("pv_inventory");
                anyhow::bail!("read volume inventory: {err}");
            }
        };

        let now = (self.now)();
        let mut findings = classify(&inv, self.min_unused_age, now);
        let state = self.orphan_pvc_cleanup_state().await;

        // The reset is what keeps a deleted claim from staying exported
        // forever and reading as a live one. It runs before the loop so a pass
        // that aborts partway leaves the gauges holding only what it managed
        // to observe, never a mix of two passes.
        self.rec.reset_unused_volumes();
        let mut due = Vec::new();
        let mut scheduled = 0usize;
        let mut next_state = OrphanPvcState::new();
        for (i, f) in findings.iter_mut().enumerate() {
            let (verdict, cause, record) = plan(f, &state, now);
            let upcoming = record.is_some_and(|r| {
                let (r, up) = self.mark_upcoming(r, &state, verdict, now);
                next_state.insert(f.uid.clone(), r);
                up
            });
            self.report(f);
            match self.publish(f, now).await {
                Ok(outcome) => {
                    if upcoming {
                        self.alert_upcoming(f).await;
                    }
                    debug!(kind = %f.kind, object = %f.key(), unused = f.unused, outcome, "unused-volume annotations settled");
                    self.emit(f, outcome, verdict);
                    self.record_orphan_pvc_cleanup(f, outcome, verdict, cause);
                    match verdict {
                        Some(Verdict::Due { .. }) => due.push(i),
                        Some(Verdict::Scheduled { .. }) => scheduled += 1,
                        _ => {}
                    }
                }
                Err(err) => {
                    self.rec.observe_error("pv_annotate");
                    error!(kind = %f.kind, object = %f.key(), reason = %f.reason, outcome = "failed", error = %err,
                        "failed to annotate object with its unused-volume verdict");
                }
            }
        }
        if let OrphanPvcCleanupState::Active(_, _, stored) = &state
            && !self.dry_run
            && *stored != next_state
            && let Err(err) = self.kube.save_orphan_state(&next_state).await
        {
            self.rec.observe_error("orphan_pvc_cleanup_state");
            error!(error = %err, "failed to save the orphan PVC cleanup state, new schedules start over on the next pass");
        }
        let pending = due.len();
        let deleted = self.delete_due(&findings, due).await;
        self.rec
            .observe_orphan_pvc_cleanup_scheduled(scheduled + pending - deleted);
        self.summarize(&findings);
        Ok(findings.len())
    }

    async fn orphan_pvc_cleanup_state(&self) -> OrphanPvcCleanupState<'_> {
        let Some(policy) = &self.orphan_pvc_cleanup else {
            return OrphanPvcCleanupState::Off;
        };
        let refs = match self.kube.workload_claim_references().await {
            Ok(refs) => refs,
            Err(err) => {
                self.rec.observe_error("orphan_pvc_cleanup_references");
                error!(error = %err,
                    "orphan PVC cleanup paused for this pass: workload claim references could not be read, so no claim is scheduled, canceled, or deleted");
                return OrphanPvcCleanupState::Hold;
            }
        };
        match self.kube.load_orphan_state().await {
            Ok(stored) => OrphanPvcCleanupState::Active(policy, refs, stored),
            Err(err) => {
                self.rec.observe_error("orphan_pvc_cleanup_state");
                error!(error = %err,
                    "orphan PVC cleanup paused for this pass: its state could not be read, so no claim is scheduled, canceled, or deleted");
                OrphanPvcCleanupState::Hold
            }
        }
    }

    /// Counts and logs a scheduling or cancellation once the annotation that
    /// carries it has been written.
    fn record_orphan_pvc_cleanup(
        &self,
        f: &Finding,
        outcome: &str,
        verdict: Option<Verdict>,
        cause: Option<&'static str>,
    ) {
        if let Some(Verdict::Scheduled {
            delete_after,
            new: true,
        }) = verdict
        {
            let action = if outcome == OUTCOME_DRY_RUN {
                orphan_pvc_cleanup::ACTION_DRY_RUN
            } else {
                orphan_pvc_cleanup::ACTION_SCHEDULED
            };
            self.rec.observe_orphan_pvc_cleanup(action);
            warn!(object = %f.key(), reason = %f.reason, unused_days = f.unused_days(),
                capacity_bytes = f.capacity_bytes, volume_id = %f.volume_id,
                delete_after = %delete_after.to_rfc3339_opts(SecondsFormat::Secs, true),
                dry_run = self.dry_run, "orphan PVC scheduled for deletion");
            return;
        }
        if outcome == OUTCOME_WRITTEN
            && f.delete_after.is_none()
            && f.annotations
                .contains_key(&key(annotations::KEY_ORPHAN_PVC_DELETE_AFTER))
        {
            self.rec
                .observe_orphan_pvc_cleanup(orphan_pvc_cleanup::ACTION_CANCELED);
            info!(object = %f.key(), cause = cause.unwrap_or_default(), "orphan PVC deletion canceled");
        }
    }

    /// Deletes the claims past their notice, longest unused first, at most
    /// `max_deletions_per_pass` of them. Each claim is checked for a Pod
    /// mounting it right before the delete, since the inventory is up to a
    /// pass old, and any error skips the claim rather than guessing.
    async fn delete_due(&self, findings: &[Finding], due: Vec<usize>) -> usize {
        let Some(policy) = &self.orphan_pvc_cleanup else {
            return 0;
        };
        let reclaim: HashMap<&str, &str> = findings
            .iter()
            .filter(|f| f.kind == KIND_PV)
            .map(|f| (f.name.as_str(), f.reclaim_policy.as_str()))
            .collect();
        let mut due: Vec<&Finding> = due.into_iter().map(|i| &findings[i]).collect();
        due.sort_by_key(|f| std::cmp::Reverse(f.age));
        let mut deleted = 0;
        for (i, f) in due.into_iter().enumerate() {
            if i >= policy.max_deletions_per_pass {
                self.rec
                    .observe_orphan_pvc_cleanup(orphan_pvc_cleanup::ACTION_DEFERRED);
                info!(object = %f.key(), max_deletions_per_pass = policy.max_deletions_per_pass,
                    "orphan PVC deletion deferred to a later pass by maxDeletionsPerPass");
                continue;
            }
            if self.dry_run {
                self.rec
                    .observe_orphan_pvc_cleanup(orphan_pvc_cleanup::ACTION_DRY_RUN);
                info!(object = %f.key(), reason = %f.reason, "dry run: would delete orphan PVC");
                continue;
            }
            match self.kube.claim_mounted(&f.namespace, &f.name).await {
                Ok(false) => {}
                Ok(true) => {
                    self.rec
                        .observe_orphan_pvc_cleanup(orphan_pvc_cleanup::ACTION_SKIPPED_IN_USE);
                    info!(object = %f.key(), "orphan PVC deletion skipped: a Pod mounts it again");
                    continue;
                }
                Err(err) => {
                    self.rec
                        .observe_orphan_pvc_cleanup(orphan_pvc_cleanup::ACTION_FAILED);
                    self.rec.observe_error("orphan_pvc_cleanup_recheck");
                    error!(object = %f.key(), error = %err, "orphan PVC deletion skipped: could not confirm no Pod mounts it");
                    self.alert_failed(f, &err).await;
                    continue;
                }
            }
            let reclaim_policy = reclaim
                .get(f.volume_name.as_str())
                .copied()
                .unwrap_or_default();
            match self.kube.delete_pvc(&f.namespace, &f.name, &f.uid).await {
                Ok(()) => {
                    deleted += 1;
                    self.rec
                        .observe_orphan_pvc_cleanup(orphan_pvc_cleanup::ACTION_DELETED);
                    warn!(object = %f.key(), reason = %f.reason, unused_days = f.unused_days(),
                        capacity_bytes = f.capacity_bytes, volume_name = %f.volume_name,
                        volume_id = %f.volume_id, reclaim_policy, "orphan PVC deleted");
                    self.emit_deleted(f, reclaim_policy);
                    self.alert(
                        f,
                        "info",
                        REASON_DELETED,
                        format!("Orphan PVC {} deleted", f.key()),
                        format!(
                            "Deleted after {} days unused ({}), holding {}. Reclaim policy of its volume: {}.",
                            f.unused_days(),
                            f.reason,
                            describe_capacity(f),
                            if reclaim_policy.is_empty() { "none" } else { reclaim_policy }
                        ),
                    )
                    .await;
                }
                Err(err) => {
                    self.rec
                        .observe_orphan_pvc_cleanup(orphan_pvc_cleanup::ACTION_FAILED);
                    self.rec.observe_error("orphan_pvc_delete");
                    error!(object = %f.key(), error = %err, "failed to delete orphan PVC");
                    self.alert_failed(f, &err).await;
                }
            }
        }
        deleted
    }

    async fn alert(
        &self,
        f: &Finding,
        severity: &str,
        alertname: &str,
        summary: String,
        description: String,
    ) {
        let Some(n) = &self.orphan_pvc_alerts else {
            return;
        };
        let mut labels = std::collections::BTreeMap::from([
            ("namespace".to_string(), f.namespace.clone()),
            ("persistentvolumeclaim".to_string(), f.name.clone()),
            ("reason".to_string(), f.reason.clone()),
        ]);
        if !f.volume_id.is_empty() {
            labels.insert("volume_id".into(), f.volume_id.clone());
        }
        n.notify(
            severity,
            alertname,
            &summary,
            &description,
            &labels,
            (self.now)(),
        )
        .await;
    }

    /// Marks a scheduled claim inside its alert window that has not alerted
    /// yet, so the same patch that publishes its annotations records the
    /// alert. Returns whether to send it once that patch is written.
    /// Marks a scheduled claim inside its alert window that has not alerted
    /// yet, recording the alert in its stored record. Returns the record to
    /// keep and whether to send the alert.
    fn mark_upcoming(
        &self,
        mut record: OrphanPvcRecord,
        state: &OrphanPvcCleanupState<'_>,
        verdict: Option<Verdict>,
        now: DateTime<Utc>,
    ) -> (OrphanPvcRecord, bool) {
        let OrphanPvcCleanupState::Active(policy, _, _) = state else {
            return (record, false);
        };
        let Some(Verdict::Scheduled { delete_after, .. }) = verdict else {
            return (record, false);
        };
        if self.dry_run
            || self.orphan_pvc_alerts.is_none()
            || record.alerted_at.is_some()
            || !policy.alert_due(delete_after, now)
        {
            return (record, false);
        }
        record.alerted_at = Some(now);
        (record, true)
    }

    async fn alert_upcoming(&self, f: &Finding) {
        let Some(t) = f.delete_after else {
            return;
        };
        self.alert(
            f,
            "warning",
            ALERT_DELETION_UPCOMING,
            format!(
                "Orphan PVC {} will be deleted at {}",
                f.key(),
                t.to_rfc3339_opts(SecondsFormat::Secs, true)
            ),
            format!(
                "Unused for {} days ({}), holding {}. Use it again or annotate it {}=disabled to keep it. This is the only alert before deletion.",
                f.unused_days(),
                f.reason,
                describe_capacity(f),
                key(annotations::KEY_ORPHAN_PVC_CLEANUP)
            ),
        )
        .await;
    }

    async fn alert_failed(&self, f: &Finding, err: &str) {
        self.alert(
            f,
            "warning",
            ALERT_DELETION_FAILED,
            format!("Orphan PVC {} could not be deleted", f.key()),
            format!("{err}. It stays scheduled and is retried on the next pass."),
        )
        .await;
    }

    fn emit_deleted(&self, f: &Finding, reclaim_policy: &str) {
        let Some(events) = &self.events else {
            return;
        };
        let volume = if f.volume_name.is_empty() {
            "No volume was bound.".to_string()
        } else {
            format!(
                "Volume {} follows its reclaim policy ({}).",
                f.volume_name,
                if reclaim_policy.is_empty() {
                    "unknown"
                } else {
                    reclaim_policy
                }
            )
        };
        events.event(
            Target::claim(&f.namespace, &f.name, &f.uid),
            TYPE_NORMAL,
            REASON_DELETED,
            format!(
                "Deleted after {} days unused ({}), holding {}. {volume}",
                f.unused_days(),
                f.reason,
                describe_capacity(f)
            ),
        );
    }

    /// Records one finding's metrics and logs it. Only a reportable finding
    /// (unused for at least `MIN_UNUSED_AGE`) is exported or logged at info.
    fn report(&self, f: &Finding) {
        if !f.reportable {
            return;
        }
        let age = f.age.as_secs_f64();
        #[allow(clippy::cast_precision_loss)]
        let size = f.capacity_bytes as f64;
        match f.kind.as_str() {
            KIND_PVC => self.rec.observe_unused_pvc(
                &f.namespace,
                &f.name,
                &f.volume_name,
                &f.volume_id,
                &f.storage_class,
                &f.reason,
                age,
                size,
            ),
            KIND_PV => self.rec.observe_unused_pv(
                &f.name,
                &f.volume_id,
                &f.storage_class,
                &f.reason,
                &f.reclaim_policy,
                &f.claim_namespace,
                &f.claim_name,
                age,
                size,
            ),
            _ => {}
        }
        info!(
            kind = %f.kind, object = %f.key(), reason = %f.reason,
            unused_since = f.unused_since.map_or_default(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true)),
            unused_days = f.unused_days(),
            capacity_bytes = f.capacity_bytes, storage_class = %f.storage_class,
            volume_id = %f.volume_id, bound_to = %f.bound_to(), reclaim_policy = %f.reclaim_policy,
            "unused volume identified, nothing was deleted"
        );
    }

    /// Publishes one finding's Kubernetes Event against the object itself. A
    /// reported finding is a standing state, republished every pass and
    /// aggregated by the emitter. Coming back into use is a transition,
    /// published once by the pass that erases the mark. A dry run emits
    /// nothing, and neither does a finding below the threshold.
    fn emit(&self, f: &Finding, outcome: &str, verdict: Option<Verdict>) {
        let Some(events) = &self.events else {
            return;
        };
        if self.dry_run {
            return;
        }
        let target = match f.kind.as_str() {
            KIND_PVC => Target::claim(&f.namespace, &f.name, &f.uid),
            _ => Target::volume(&f.name, &f.uid),
        };
        if let Some(t) = verdict.and_then(|v| v.delete_after()) {
            events.event(
                target,
                TYPE_WARNING,
                REASON_DELETION_SCHEDULED,
                format!(
                    "Unused for {} days ({}), holding {}. Scheduled for deletion at {}. Annotate it {}=disabled to keep it.",
                    f.unused_days(),
                    f.reason,
                    describe_capacity(f),
                    t.to_rfc3339_opts(SecondsFormat::Secs, true),
                    key(annotations::KEY_ORPHAN_PVC_CLEANUP)
                ),
            );
            return;
        }
        if outcome == OUTCOME_WRITTEN
            && f.annotations
                .contains_key(&key(annotations::KEY_ORPHAN_PVC_DELETE_AFTER))
            && f.delete_after.is_none()
        {
            events.event(
                target.clone(),
                TYPE_NORMAL,
                REASON_DELETION_CANCELED,
                "The scheduled deletion has been canceled.".to_string(),
            );
        }
        if f.reportable {
            events.event(
                target,
                TYPE_NORMAL,
                REASON_UNUSED_DETECTED,
                format!(
                    "Unused for {} days ({}), holding {}. Nothing was deleted. Review and remove it manually if the data is no longer needed.",
                    f.unused_days(), f.reason, describe_capacity(f)
                ),
            );
        } else if !f.unused && outcome == OUTCOME_WRITTEN {
            events.event(
                target,
                TYPE_NORMAL,
                REASON_UNUSED_CLEARED,
                format!(
                    "Back in use ({}). The unused mark has been removed.",
                    f.reason
                ),
            );
        }
    }

    /// Publishes the per-reason counts and the total capacity they hold, then
    /// logs the pass. Every known reason is published even when it matched
    /// nothing, so a query for a reason that has stopped occurring reads as
    /// zero rather than as no data.
    fn summarize(&self, findings: &[Finding]) {
        let mut counts: HashMap<String, (usize, i64)> = HashMap::new();
        for f in findings.iter().filter(|f| f.reportable) {
            let e = counts
                .entry(format!("{}/{}", f.kind, f.reason))
                .or_default();
            e.0 += 1;
            e.1 += f.capacity_bytes;
        }
        let mut total_count = 0usize;
        let mut total_bytes = 0i64;
        for (kind, reasons) in [
            (KIND_PVC, UNUSED_PVC_REASONS.as_slice()),
            (KIND_PV, UNUSED_PV_REASONS.as_slice()),
        ] {
            for reason in reasons {
                let (n, bytes) = counts
                    .get(&format!("{kind}/{reason}"))
                    .copied()
                    .unwrap_or_default();
                self.rec.observe_unused_summary(kind, reason, n, bytes);
                total_count += n;
                total_bytes += bytes;
            }
        }
        info!(
            objects_scanned = findings.len(),
            unused_reported = total_count,
            unused_capacity_bytes = total_bytes,
            min_unused_age = %crate::humanize::go_duration(self.min_unused_age),
            orphan_pvc_cleanup = self.orphan_pvc_cleanup.is_some(),
            dry_run = self.dry_run,
            "unused volume scan completed"
        );
    }

    /// Writes the object's annotations and reports which outcome happened.
    /// The patch is skipped when nothing changed and unused-observed-at is
    /// still fresh. Annotations are written from the first pass that sees an
    /// object unused, not from the pass that first reports it: the annotation
    /// is where the clock lives.
    async fn publish(&self, f: &Finding, now: DateTime<Utc>) -> Result<&'static str, String> {
        let mut desired = build_annotations(f);
        if !desired.needs_write(&f.annotations, now) {
            return Ok(OUTCOME_UNCHANGED);
        }
        if !desired.set.is_empty() {
            desired.set.insert(
                key(annotations::KEY_OBSERVED_AT),
                now.to_rfc3339_opts(SecondsFormat::Secs, true),
            );
        }
        if self.dry_run {
            return Ok(OUTCOME_DRY_RUN);
        }
        match f.kind.as_str() {
            KIND_PVC => {
                self.kube
                    .annotate_pvc(&f.namespace, &f.name, &desired.set, &desired.remove)
                    .await?;
            }
            _ => {
                self.kube
                    .annotate_pv(&f.name, &desired.set, &desired.remove)
                    .await?;
            }
        }
        Ok(OUTCOME_WRITTEN)
    }

    /// Runs one classification pass and returns its results without touching
    /// the cluster's annotations or the metrics. It is what the CLI
    /// subcommand reports.
    pub async fn findings(&self) -> Result<Vec<Finding>, anyhow::Error> {
        let inv = self
            .kube
            .inventory()
            .await
            .map_err(|err| anyhow::anyhow!("read volume inventory: {err}"))?;
        Ok(classify(&inv, self.min_unused_age, (self.now)()))
    }
}

/// Applies one pass's orphan PVC cleanup state to a finding: sets the
/// deadline its annotations will show and returns the verdict, why a claim is
/// not a candidate, and the record to keep for it.
fn plan(
    f: &mut Finding,
    state: &OrphanPvcCleanupState<'_>,
    now: DateTime<Utc>,
) -> (
    Option<Verdict>,
    Option<&'static str>,
    Option<OrphanPvcRecord>,
) {
    match state {
        OrphanPvcCleanupState::Off => {
            f.delete_after = None;
            (None, Some(orphan_pvc_cleanup::CAUSE_DISABLED), None)
        }
        OrphanPvcCleanupState::Hold => {
            f.delete_after = orphan_pvc_cleanup::existing_deadline(f);
            (None, None, None)
        }
        OrphanPvcCleanupState::Active(policy, refs, stored) => {
            let (v, record) = policy.decide(f, refs, stored.get(&f.uid), now);
            f.delete_after = v.delete_after();
            let cause = match v {
                Verdict::Ineligible(c) => Some(c),
                _ => None,
            };
            (Some(v), cause, record)
        }
    }
}

/// Renders the finding's size and backing volume for an Event message. A
/// claim that never bound has neither, and saying so beats printing a zero
/// that reads like a measurement.
fn describe_capacity(f: &Finding) -> String {
    if f.capacity_bytes == 0 {
        return "no provisioned capacity".into();
    }
    let size = format!("{}Gi", f.capacity_bytes / (1024 * 1024 * 1024));
    if f.volume_id.is_empty() {
        size
    } else {
        format!("{size} on {}", f.volume_id)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};
    use std::sync::Mutex;

    use super::*;
    use crate::k8s::events::capture::Capture;

    const GIB: i64 = 1024 * 1024 * 1024;

    type Patch = (String, BTreeMap<String, String>, Vec<String>);

    #[derive(Default)]
    #[allow(clippy::struct_excessive_bools)]
    struct FakeKube {
        inv: Mutex<Inventory>,
        fail_inventory: bool,
        fail_annotate: bool,
        patches: Mutex<Vec<Patch>>,
        references: HashSet<String>,
        fail_references: bool,
        mounted: HashSet<String>,
        fail_mounted: bool,
        fail_delete: bool,
        deletes: Mutex<Vec<(String, String)>>,
        /// An explicit stored state. `None` derives one from the inventory, as
        /// if earlier passes had recorded every claim's annotated clock and
        /// deadline.
        state: Mutex<Option<OrphanPvcState>>,
        fail_state: bool,
        saved: Mutex<Vec<OrphanPvcState>>,
    }

    /// The state earlier passes would have written for the inventory.
    fn derived_state(inv: &Inventory) -> OrphanPvcState {
        let at = |c: &Pvc, k: &str| {
            c.annotations
                .get(&key(k))
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&Utc))
        };
        inv.pvcs
            .iter()
            .filter_map(|c| {
                let first_seen = at(c, annotations::KEY_UNUSED_SINCE)?;
                Some((
                    c.uid.clone(),
                    OrphanPvcRecord {
                        claim: format!("{}/{}", c.namespace, c.name),
                        first_seen,
                        delete_after: at(c, annotations::KEY_ORPHAN_PVC_DELETE_AFTER),
                        alerted_at: None,
                    },
                ))
            })
            .collect()
    }

    #[async_trait]
    impl KubeApi for FakeKube {
        async fn inventory(&self) -> Result<Inventory, String> {
            if self.fail_inventory {
                return Err("forbidden".into());
            }
            Ok(self.inv.lock().unwrap().clone())
        }
        async fn annotate_pvc(
            &self,
            namespace: &str,
            name: &str,
            set: &BTreeMap<String, String>,
            remove: &[String],
        ) -> Result<(), String> {
            if self.fail_annotate {
                return Err("denied".into());
            }
            self.patches.lock().unwrap().push((
                format!("{namespace}/{name}"),
                set.clone(),
                remove.to_vec(),
            ));
            Ok(())
        }
        async fn annotate_pv(
            &self,
            name: &str,
            set: &BTreeMap<String, String>,
            remove: &[String],
        ) -> Result<(), String> {
            if self.fail_annotate {
                return Err("denied".into());
            }
            self.patches
                .lock()
                .unwrap()
                .push((name.to_string(), set.clone(), remove.to_vec()));
            Ok(())
        }
        async fn workload_claim_references(&self) -> Result<HashSet<String>, String> {
            if self.fail_references {
                return Err("forbidden".into());
            }
            Ok(self.references.clone())
        }
        async fn claim_mounted(&self, namespace: &str, name: &str) -> Result<bool, String> {
            if self.fail_mounted {
                return Err("timeout".into());
            }
            Ok(self.mounted.contains(&format!("{namespace}/{name}")))
        }
        async fn load_orphan_state(&self) -> Result<OrphanPvcState, String> {
            if self.fail_state {
                return Err("forbidden".into());
            }
            Ok(self
                .state
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| derived_state(&self.inv.lock().unwrap())))
        }
        async fn save_orphan_state(&self, state: &OrphanPvcState) -> Result<(), String> {
            self.saved.lock().unwrap().push(state.clone());
            Ok(())
        }
        async fn delete_pvc(&self, namespace: &str, name: &str, uid: &str) -> Result<(), String> {
            if self.fail_delete {
                return Err("conflict".into());
            }
            self.deletes
                .lock()
                .unwrap()
                .push((format!("{namespace}/{name}"), uid.to_string()));
            Ok(())
        }
    }

    #[derive(Default)]
    struct Rec {
        pvcs: Mutex<Vec<(String, String, f64)>>,
        pvs: Mutex<Vec<(String, String)>>,
        summaries: Mutex<Vec<(String, String, usize, i64)>>,
        errors: Mutex<Vec<String>>,
        resets: Mutex<usize>,
        cleanup: Mutex<Vec<String>>,
        scheduled: Mutex<Option<usize>>,
    }

    impl Recorder for Rec {
        fn reset_unused_volumes(&self) {
            *self.resets.lock().unwrap() += 1;
        }
        fn observe_unused_pvc(
            &self,
            namespace: &str,
            name: &str,
            _: &str,
            _: &str,
            _: &str,
            reason: &str,
            age: f64,
            _: f64,
        ) {
            self.pvcs
                .lock()
                .unwrap()
                .push((format!("{namespace}/{name}"), reason.into(), age));
        }
        fn observe_unused_pv(
            &self,
            name: &str,
            _: &str,
            _: &str,
            reason: &str,
            _: &str,
            _: &str,
            _: &str,
            _: f64,
            _: f64,
        ) {
            self.pvs.lock().unwrap().push((name.into(), reason.into()));
        }
        fn observe_unused_summary(&self, kind: &str, reason: &str, count: usize, bytes: i64) {
            self.summaries
                .lock()
                .unwrap()
                .push((kind.into(), reason.into(), count, bytes));
        }
        fn observe_orphan_pvc_cleanup(&self, action: &str) {
            self.cleanup.lock().unwrap().push(action.into());
        }
        fn observe_orphan_pvc_cleanup_scheduled(&self, count: usize) {
            *self.scheduled.lock().unwrap() = Some(count);
        }
        fn observe_error(&self, stage: &str) {
            self.errors.lock().unwrap().push(stage.into());
        }
    }

    fn pvc(ns: &str, name: &str, phase: &str, volume: &str) -> Pvc {
        Pvc {
            namespace: ns.into(),
            name: name.into(),
            uid: format!("uid-{name}"),
            phase: phase.into(),
            volume_name: volume.into(),
            storage_class: "gp3".into(),
            capacity_bytes: 20 * GIB,
            annotations: BTreeMap::new(),
            owned: false,
        }
    }

    fn pv(name: &str, phase: &str, claim: (&str, &str, &str)) -> Pv {
        Pv {
            name: name.into(),
            uid: format!("uid-{name}"),
            phase: phase.into(),
            storage_class: "gp3".into(),
            capacity_bytes: 20 * GIB,
            reclaim_policy: "Retain".into(),
            claim_namespace: claim.0.into(),
            claim_name: claim.1.into(),
            claim_uid: claim.2.into(),
            volume_id: format!("vol-{name}"),
            annotations: BTreeMap::new(),
        }
    }

    fn inventory() -> Inventory {
        Inventory {
            pvcs: vec![
                pvc("app", "used", "Bound", "pv-used"),
                pvc("legacy", "uploads", "Bound", "pv-uploads"),
                pvc("new", "pending", "Pending", ""),
            ],
            pvs: vec![
                pv("pv-used", "Bound", ("app", "used", "uid-used")),
                pv("pv-uploads", "Bound", ("legacy", "uploads", "uid-uploads")),
                pv("pv-orphan", "Released", ("gone", "claim", "uid-x")),
            ],
            claims_in_use: HashSet::from(["app/used".to_string()]),
            stateful_sets: vec![],
        }
    }

    fn scanner(
        kube: Arc<FakeKube>,
        rec: Arc<Rec>,
        events: Option<Emitter>,
        dry_run: bool,
        now: DateTime<Utc>,
    ) -> Scanner {
        Scanner::new(dry_run, kube, rec, events).with_clock(Duration::from_hours(24), move || now)
    }

    #[tokio::test]
    async fn reconcile_reports_only_past_the_threshold_and_writes_annotations() {
        let now = Utc::now();
        let mut inv = inventory();
        // uploads has been marked for two days already.
        inv.pvcs[1].annotations.insert(
            key("unused-since"),
            (now - chrono::TimeDelta::days(2)).to_rfc3339_opts(SecondsFormat::Secs, true),
        );
        let kube = Arc::new(FakeKube {
            inv: Mutex::new(inv),
            ..FakeKube::default()
        });
        let rec = Arc::new(Rec::default());
        let sink = Arc::new(Capture::default());
        let s = scanner(
            kube.clone(),
            rec.clone(),
            Some(Emitter::new(sink.clone())),
            false,
            now,
        );
        let n = s.reconcile().await.unwrap();
        assert_eq!(n, 6);
        assert_eq!(*rec.resets.lock().unwrap(), 1);
        let pvcs = rec.pvcs.lock().unwrap().clone();
        assert_eq!(
            pvcs.len(),
            1,
            "only the claim past the threshold is exported"
        );
        assert_eq!(pvcs[0].0, "legacy/uploads");
        assert_eq!(pvcs[0].1, REASON_NO_CONSUMER_POD);
        assert!((pvcs[0].2 - 172_800.0).abs() < 1.0);
        assert!(
            rec.pvs.lock().unwrap().is_empty(),
            "volumes below the threshold are not exported"
        );
        let summaries = rec.summaries.lock().unwrap().clone();
        assert_eq!(
            summaries.len(),
            UNUSED_PVC_REASONS.len() + UNUSED_PV_REASONS.len(),
            "every reason published"
        );
        assert!(summaries.contains(&(KIND_PVC.into(), REASON_NO_CONSUMER_POD.into(), 1, 20 * GIB)));
        assert!(summaries.contains(&(KIND_PV.into(), REASON_RELEASED.into(), 0, 0)));

        let patches = kube.patches.lock().unwrap().clone();
        let names: Vec<&str> = patches.iter().map(|p| p.0.as_str()).collect();
        assert_eq!(
            names,
            vec!["legacy/uploads", "new/pending", "pv-uploads", "pv-orphan"],
            "in-use objects without a mark are not patched"
        );
        let uploads = &patches[0];
        assert_eq!(uploads.1[&key("unused")], "true");
        assert_eq!(uploads.1[&key("unused-reason")], REASON_NO_CONSUMER_POD);
        assert_eq!(uploads.1[&key("unused-days")], "2");
        assert_eq!(uploads.1[&key("volume-id")], "vol-pv-uploads");
        assert!(uploads.1.contains_key(&key("unused-observed-at")));
        assert_eq!(
            uploads.2,
            vec![key(annotations::KEY_ORPHAN_PVC_DELETE_AFTER)],
            "an unscheduled claim queues only the absent deadline for removal"
        );
        let pending = &patches[1];
        assert!(
            !pending.1.contains_key(&key("volume-id")),
            "unbound claim has no volume"
        );
        assert_eq!(
            pending.2,
            vec![
                key("volume-id"),
                key(annotations::KEY_ORPHAN_PVC_DELETE_AFTER)
            ]
        );

        s.events.as_ref().unwrap().shutdown().await;
        let events = sink.summary();
        assert_eq!(events.len(), 1, "only the reported finding gets an Event");
        assert_eq!(events[0].0, "PersistentVolumeClaim");
        assert_eq!(events[0].1, "legacy");
        assert_eq!(events[0].4, REASON_UNUSED_DETECTED);
        assert_eq!(
            events[0].5,
            "Unused for 2 days (no_consumer_pod), holding 20Gi on vol-pv-uploads. Nothing was deleted. Review and remove it manually if the data is no longer needed."
        );
    }

    #[tokio::test]
    async fn reconcile_clears_a_mark_and_emits_cleared_once() {
        let now = Utc::now();
        let mut inv = inventory();
        inv.pvcs[0].annotations.insert(key("unused"), "true".into());
        inv.pvcs[0]
            .annotations
            .insert(key("unused-observed-at"), "x".into());
        let kube = Arc::new(FakeKube {
            inv: Mutex::new(inv),
            ..FakeKube::default()
        });
        let rec = Arc::new(Rec::default());
        let sink = Arc::new(Capture::default());
        let s = scanner(
            kube.clone(),
            rec,
            Some(Emitter::new(sink.clone())),
            false,
            now,
        );
        s.reconcile().await.unwrap();
        let patches = kube.patches.lock().unwrap().clone();
        let used = patches.iter().find(|p| p.0 == "app/used").unwrap();
        assert!(used.1.is_empty());
        assert!(used.2.contains(&key("unused")));
        assert!(used.2.contains(&key("unused-observed-at")));
        s.events.as_ref().unwrap().shutdown().await;
        let events = sink.summary();
        let cleared: Vec<_> = events
            .iter()
            .filter(|e| e.4 == REASON_UNUSED_CLEARED)
            .collect();
        assert_eq!(cleared.len(), 1);
        assert_eq!(cleared[0].2, "used");
        assert_eq!(
            cleared[0].5,
            "Back in use (mounted_by_pod). The unused mark has been removed."
        );
    }

    #[tokio::test]
    async fn dry_run_writes_nothing_and_emits_nothing() {
        let now = Utc::now();
        let mut inv = inventory();
        inv.pvcs[1].annotations.insert(
            key("unused-since"),
            (now - chrono::TimeDelta::days(3)).to_rfc3339_opts(SecondsFormat::Secs, true),
        );
        let kube = Arc::new(FakeKube {
            inv: Mutex::new(inv),
            ..FakeKube::default()
        });
        let rec = Arc::new(Rec::default());
        let sink = Arc::new(Capture::default());
        let s = scanner(
            kube.clone(),
            rec.clone(),
            Some(Emitter::new(sink.clone())),
            true,
            now,
        );
        s.reconcile().await.unwrap();
        assert!(kube.patches.lock().unwrap().is_empty());
        assert_eq!(rec.pvcs.lock().unwrap().len(), 1, "still reported");
        s.events.as_ref().unwrap().shutdown().await;
        assert!(sink.events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn reconcile_without_an_emitter_and_with_failures() {
        let now = Utc::now();
        let kube = Arc::new(FakeKube {
            inv: Mutex::new(inventory()),
            fail_annotate: true,
            ..FakeKube::default()
        });
        let rec = Arc::new(Rec::default());
        let s = scanner(kube, rec.clone(), None, false, now);
        let n = s.reconcile().await.unwrap();
        assert_eq!(n, 6, "annotation failures do not abort the pass");
        let errors = rec.errors.lock().unwrap().clone();
        assert_eq!(errors.iter().filter(|e| *e == "pv_annotate").count(), 4);

        let kube = Arc::new(FakeKube {
            fail_inventory: true,
            ..FakeKube::default()
        });
        let rec = Arc::new(Rec::default());
        let s = scanner(kube, rec.clone(), None, false, now);
        let err = s.reconcile().await.unwrap_err().to_string();
        assert!(err.contains("read volume inventory: forbidden"), "{err}");
        assert_eq!(rec.errors.lock().unwrap().clone(), vec!["pv_inventory"]);
        assert_eq!(*rec.resets.lock().unwrap(), 0);
        let err = s.findings().await.unwrap_err().to_string();
        assert!(err.contains("read volume inventory"));
    }

    #[tokio::test]
    async fn healthy_objects_are_not_patched_again() {
        let now = Utc::now();
        let mut inv = inventory();
        let since = now - chrono::TimeDelta::days(2);
        for (k, v) in [
            ("unused", "true".to_string()),
            (
                "unused-since",
                since.to_rfc3339_opts(SecondsFormat::Secs, true),
            ),
            ("unused-reason", REASON_NO_CONSUMER_POD.to_string()),
            ("unused-days", "2".to_string()),
            ("volume-id", "vol-pv-uploads".to_string()),
            (
                "unused-observed-at",
                (now - chrono::TimeDelta::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true),
            ),
        ] {
            inv.pvcs[1].annotations.insert(key(k), v);
        }
        let kube = Arc::new(FakeKube {
            inv: Mutex::new(inv),
            ..FakeKube::default()
        });
        let s = scanner(kube.clone(), Arc::new(Rec::default()), None, false, now);
        s.reconcile().await.unwrap();
        let patched: Vec<String> = kube
            .patches
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.0.clone())
            .collect();
        assert!(
            !patched.contains(&"legacy/uploads".to_string()),
            "{patched:?}"
        );
    }

    #[tokio::test]
    async fn findings_does_not_write() {
        let now = Utc::now();
        let kube = Arc::new(FakeKube {
            inv: Mutex::new(inventory()),
            ..FakeKube::default()
        });
        let rec = Arc::new(Rec::default());
        let s = scanner(kube.clone(), rec.clone(), None, false, now);
        let f = s.findings().await.unwrap();
        assert_eq!(f.len(), 6);
        assert!(kube.patches.lock().unwrap().is_empty());
        assert!(rec.pvcs.lock().unwrap().is_empty());
        assert_eq!(*rec.resets.lock().unwrap(), 0);
    }

    fn orphan_policy(max: usize) -> OrphanPvcCleanupPolicy {
        OrphanPvcCleanupPolicy {
            min_unused_age: Duration::from_hours(48),
            reasons: vec![REASON_NO_CONSUMER_POD.into()],
            include_namespaces: vec![],
            exclude_namespaces: vec!["kube-system".into()],
            max_deletions_per_pass: max,
            alert_before: Duration::from_hours(24),
        }
    }

    fn stamp(t: DateTime<Utc>) -> String {
        t.to_rfc3339_opts(SecondsFormat::Secs, true)
    }

    /// `inventory()` with `legacy/uploads` unused for three days, plus the
    /// given deadline on it.
    fn orphan_inventory(now: DateTime<Utc>, delete_after: Option<DateTime<Utc>>) -> Inventory {
        let mut inv = inventory();
        let a = &mut inv.pvcs[1].annotations;
        a.insert(key("unused-since"), stamp(now - chrono::TimeDelta::days(3)));
        if let Some(t) = delete_after {
            a.insert(key(annotations::KEY_ORPHAN_PVC_DELETE_AFTER), stamp(t));
        }
        inv
    }

    #[tokio::test]
    async fn orphan_pvc_cleanup_schedules_with_notice() {
        let now = Utc::now();
        let kube = Arc::new(FakeKube {
            inv: Mutex::new(orphan_inventory(now, None)),
            ..FakeKube::default()
        });
        let rec = Arc::new(Rec::default());
        let sink = Arc::new(Capture::default());
        let s = scanner(
            kube.clone(),
            rec.clone(),
            Some(Emitter::new(sink.clone())),
            false,
            now,
        )
        .with_orphan_pvc_cleanup(Some(orphan_policy(10)));
        s.reconcile().await.unwrap();

        assert!(kube.deletes.lock().unwrap().is_empty(), "notice first");
        let patches = kube.patches.lock().unwrap().clone();
        let uploads = patches.iter().find(|p| p.0 == "legacy/uploads").unwrap();
        assert_eq!(
            uploads.1[&key(annotations::KEY_ORPHAN_PVC_DELETE_AFTER)],
            stamp(now + chrono::TimeDelta::hours(24))
        );
        assert_eq!(
            rec.cleanup.lock().unwrap().clone(),
            vec![orphan_pvc_cleanup::ACTION_SCHEDULED]
        );
        assert_eq!(*rec.scheduled.lock().unwrap(), Some(1));

        s.events.as_ref().unwrap().shutdown().await;
        let events = sink.summary();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].3, TYPE_WARNING);
        assert_eq!(events[0].4, REASON_DELETION_SCHEDULED);
        assert!(
            events[0]
                .5
                .contains("Annotate it volume-autoscaler.sh/orphan-pvc-cleanup=disabled"),
            "{}",
            events[0].5
        );
    }

    #[tokio::test]
    async fn orphan_pvc_cleanup_deletes_past_the_notice() {
        let now = Utc::now();
        let kube = Arc::new(FakeKube {
            inv: Mutex::new(orphan_inventory(
                now,
                Some(now - chrono::TimeDelta::hours(1)),
            )),
            ..FakeKube::default()
        });
        let rec = Arc::new(Rec::default());
        let sink = Arc::new(Capture::default());
        let s = scanner(
            kube.clone(),
            rec.clone(),
            Some(Emitter::new(sink.clone())),
            false,
            now,
        )
        .with_orphan_pvc_cleanup(Some(orphan_policy(10)));
        s.reconcile().await.unwrap();

        assert_eq!(
            kube.deletes.lock().unwrap().clone(),
            vec![("legacy/uploads".to_string(), "uid-uploads".to_string())]
        );
        assert_eq!(
            rec.cleanup.lock().unwrap().clone(),
            vec![orphan_pvc_cleanup::ACTION_DELETED]
        );
        assert_eq!(*rec.scheduled.lock().unwrap(), Some(0));
        s.events.as_ref().unwrap().shutdown().await;
        let deleted: Vec<_> = sink
            .summary()
            .into_iter()
            .filter(|e| e.4 == REASON_DELETED)
            .collect();
        assert_eq!(deleted.len(), 1);
        assert_eq!(
            deleted[0].5,
            "Deleted after 3 days unused (no_consumer_pod), holding 20Gi on vol-pv-uploads. Volume pv-uploads follows its reclaim policy (Retain)."
        );
    }

    #[tokio::test]
    async fn orphan_pvc_cleanup_rechecks_and_reports_failures() {
        let now = Utc::now();
        let due = Some(now - chrono::TimeDelta::hours(1));
        for (kube, want, stage) in [
            (
                FakeKube {
                    mounted: HashSet::from(["legacy/uploads".to_string()]),
                    ..FakeKube::default()
                },
                orphan_pvc_cleanup::ACTION_SKIPPED_IN_USE,
                None,
            ),
            (
                FakeKube {
                    fail_mounted: true,
                    ..FakeKube::default()
                },
                orphan_pvc_cleanup::ACTION_FAILED,
                Some("orphan_pvc_cleanup_recheck"),
            ),
            (
                FakeKube {
                    fail_delete: true,
                    ..FakeKube::default()
                },
                orphan_pvc_cleanup::ACTION_FAILED,
                Some("orphan_pvc_delete"),
            ),
        ] {
            *kube.inv.lock().unwrap() = orphan_inventory(now, due);
            let kube = Arc::new(kube);
            let rec = Arc::new(Rec::default());
            let s = scanner(kube.clone(), rec.clone(), None, false, now)
                .with_orphan_pvc_cleanup(Some(orphan_policy(10)));
            s.reconcile().await.unwrap();
            assert!(kube.deletes.lock().unwrap().is_empty());
            assert_eq!(rec.cleanup.lock().unwrap().clone(), vec![want]);
            assert_eq!(*rec.scheduled.lock().unwrap(), Some(1), "still scheduled");
            if let Some(stage) = stage {
                assert!(rec.errors.lock().unwrap().contains(&stage.to_string()));
            }
        }
    }

    #[tokio::test]
    async fn orphan_pvc_cleanup_caps_deletions_oldest_first() {
        let now = Utc::now();
        let due = now - chrono::TimeDelta::hours(1);
        let mut inv = orphan_inventory(now, Some(due));
        let mut older = pvc("legacy", "older", "Bound", "pv-older");
        older
            .annotations
            .insert(key("unused-since"), stamp(now - chrono::TimeDelta::days(9)));
        older
            .annotations
            .insert(key(annotations::KEY_ORPHAN_PVC_DELETE_AFTER), stamp(due));
        inv.pvcs.push(older);
        let kube = Arc::new(FakeKube {
            inv: Mutex::new(inv),
            ..FakeKube::default()
        });
        let rec = Arc::new(Rec::default());
        let s = scanner(kube.clone(), rec.clone(), None, false, now)
            .with_orphan_pvc_cleanup(Some(orphan_policy(1)));
        s.reconcile().await.unwrap();
        let deletes = kube.deletes.lock().unwrap().clone();
        assert_eq!(deletes.len(), 1);
        assert_eq!(deletes[0].0, "legacy/older");
        let mut actions = rec.cleanup.lock().unwrap().clone();
        actions.sort();
        assert_eq!(
            actions,
            vec![
                orphan_pvc_cleanup::ACTION_DEFERRED,
                orphan_pvc_cleanup::ACTION_DELETED
            ]
        );
        assert_eq!(*rec.scheduled.lock().unwrap(), Some(1));
    }

    #[tokio::test]
    async fn orphan_pvc_cleanup_disabled_cancels_a_schedule() {
        let now = Utc::now();
        let kube = Arc::new(FakeKube {
            inv: Mutex::new(orphan_inventory(
                now,
                Some(now + chrono::TimeDelta::hours(5)),
            )),
            ..FakeKube::default()
        });
        let rec = Arc::new(Rec::default());
        let sink = Arc::new(Capture::default());
        let s = scanner(
            kube.clone(),
            rec.clone(),
            Some(Emitter::new(sink.clone())),
            false,
            now,
        );
        s.reconcile().await.unwrap();
        let patches = kube.patches.lock().unwrap().clone();
        let uploads = patches.iter().find(|p| p.0 == "legacy/uploads").unwrap();
        assert!(
            uploads
                .2
                .contains(&key(annotations::KEY_ORPHAN_PVC_DELETE_AFTER))
        );
        assert_eq!(
            rec.cleanup.lock().unwrap().clone(),
            vec![orphan_pvc_cleanup::ACTION_CANCELED]
        );
        assert_eq!(*rec.scheduled.lock().unwrap(), Some(0));
        s.events.as_ref().unwrap().shutdown().await;
        let reasons: Vec<String> = sink.summary().into_iter().map(|e| e.4).collect();
        assert!(
            reasons.contains(&REASON_DELETION_CANCELED.to_string()),
            "{reasons:?}"
        );
        assert!(
            reasons.contains(&REASON_UNUSED_DETECTED.to_string()),
            "{reasons:?}"
        );
    }

    #[tokio::test]
    async fn orphan_pvc_cleanup_holds_when_references_fail() {
        let now = Utc::now();
        let deadline = now - chrono::TimeDelta::hours(1);
        let kube = Arc::new(FakeKube {
            inv: Mutex::new(orphan_inventory(now, Some(deadline))),
            fail_references: true,
            ..FakeKube::default()
        });
        let rec = Arc::new(Rec::default());
        let s = scanner(kube.clone(), rec.clone(), None, false, now)
            .with_orphan_pvc_cleanup(Some(orphan_policy(10)));
        s.reconcile().await.unwrap();
        assert!(kube.deletes.lock().unwrap().is_empty());
        assert!(rec.cleanup.lock().unwrap().is_empty());
        assert!(
            rec.errors
                .lock()
                .unwrap()
                .contains(&"orphan_pvc_cleanup_references".to_string())
        );
        let patches = kube.patches.lock().unwrap().clone();
        if let Some(uploads) = patches.iter().find(|p| p.0 == "legacy/uploads") {
            assert!(
                !uploads
                    .2
                    .contains(&key(annotations::KEY_ORPHAN_PVC_DELETE_AFTER)),
                "a held deadline is kept"
            );
        }
    }

    type Alert = (String, String, BTreeMap<String, String>);

    #[derive(Default)]
    struct Alerts(Mutex<Vec<Alert>>);

    #[async_trait]
    impl AlertNotifier for Alerts {
        async fn notify(
            &self,
            severity: &str,
            alertname: &str,
            _: &str,
            _: &str,
            labels: &BTreeMap<String, String>,
            _: DateTime<Utc>,
        ) {
            self.0
                .lock()
                .unwrap()
                .push((severity.into(), alertname.into(), labels.clone()));
        }
    }

    #[tokio::test]
    async fn orphan_pvc_cleanup_alerts_when_enabled() {
        let now = Utc::now();
        for (deadline, fail_delete, want) in [
            (None, false, ("warning", ALERT_DELETION_UPCOMING)),
            (
                Some(now - chrono::TimeDelta::hours(1)),
                false,
                ("info", REASON_DELETED),
            ),
            (
                Some(now - chrono::TimeDelta::hours(1)),
                true,
                ("warning", ALERT_DELETION_FAILED),
            ),
        ] {
            let kube = Arc::new(FakeKube {
                inv: Mutex::new(orphan_inventory(now, deadline)),
                fail_delete,
                ..FakeKube::default()
            });
            let alerts = Arc::new(Alerts::default());
            let s = scanner(kube, Arc::new(Rec::default()), None, false, now)
                .with_orphan_pvc_cleanup(Some(orphan_policy(10)))
                .with_orphan_pvc_alerts(Some(alerts.clone()));
            s.reconcile().await.unwrap();
            let got = alerts.0.lock().unwrap().clone();
            assert_eq!(got.len(), 1, "{got:?}");
            assert_eq!((got[0].0.as_str(), got[0].1.as_str()), want);
            assert_eq!(got[0].2["namespace"], "legacy");
            assert_eq!(got[0].2["persistentvolumeclaim"], "uploads");
            assert_eq!(got[0].2["volume_id"], "vol-pv-uploads");
        }

        let kube = Arc::new(FakeKube {
            inv: Mutex::new(orphan_inventory(now, None)),
            ..FakeKube::default()
        });
        let alerts = Arc::new(Alerts::default());
        let s = scanner(kube, Arc::new(Rec::default()), None, true, now)
            .with_orphan_pvc_cleanup(Some(orphan_policy(10)))
            .with_orphan_pvc_alerts(Some(alerts.clone()));
        s.reconcile().await.unwrap();
        assert!(
            alerts.0.lock().unwrap().is_empty(),
            "a dry run alerts nothing"
        );
    }

    #[tokio::test]
    async fn orphan_pvc_upcoming_alert_is_sent_once_inside_the_window() {
        let now = Utc::now();
        let mut policy = orphan_policy(10);
        policy.alert_before = Duration::from_hours(6);
        let run = |inv: Inventory, state: Option<OrphanPvcState>| {
            let policy = policy.clone();
            async move {
                let kube = Arc::new(FakeKube {
                    inv: Mutex::new(inv),
                    state: Mutex::new(state),
                    ..FakeKube::default()
                });
                let alerts = Arc::new(Alerts::default());
                let s = scanner(kube.clone(), Arc::new(Rec::default()), None, false, now)
                    .with_orphan_pvc_cleanup(Some(policy))
                    .with_orphan_pvc_alerts(Some(alerts.clone()));
                s.reconcile().await.unwrap();
                let names: Vec<String> = alerts
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|a| a.1.clone())
                    .collect();
                let saved = kube.saved.lock().unwrap().last().cloned();
                (names, saved)
            }
        };

        let (names, _) = run(
            orphan_inventory(now, Some(now + chrono::TimeDelta::hours(10))),
            None,
        )
        .await;
        assert!(
            names.is_empty(),
            "a deadline 10h out is outside a 6h window"
        );

        let (names, _) = run(orphan_inventory(now, None), None).await;
        assert_eq!(
            names,
            vec![ALERT_DELETION_UPCOMING],
            "scheduling always lands inside the alert window"
        );

        let inv = orphan_inventory(now, Some(now + chrono::TimeDelta::hours(5)));
        let (names, saved) = run(inv.clone(), None).await;
        assert_eq!(names, vec![ALERT_DELETION_UPCOMING]);
        assert_eq!(saved.unwrap()["uid-uploads"].alerted_at, Some(now));

        let mut state = derived_state(&inv);
        state.get_mut("uid-uploads").unwrap().alerted_at = Some(now - chrono::TimeDelta::hours(1));
        let (names, _) = run(inv, Some(state)).await;
        assert!(names.is_empty(), "already alerted");
    }

    #[tokio::test]
    async fn orphan_pvc_cleanup_trusts_only_the_stored_state() {
        let now = Utc::now();
        let kube = Arc::new(FakeKube {
            inv: Mutex::new(orphan_inventory(
                now,
                Some(now - chrono::TimeDelta::hours(1)),
            )),
            state: Mutex::new(Some(OrphanPvcState::new())),
            ..FakeKube::default()
        });
        let rec = Arc::new(Rec::default());
        let s = scanner(kube.clone(), rec.clone(), None, false, now)
            .with_orphan_pvc_cleanup(Some(orphan_policy(10)));
        s.reconcile().await.unwrap();
        assert!(
            kube.deletes.lock().unwrap().is_empty(),
            "a past deadline and an old clock on the claim alone delete nothing"
        );
        let saved = kube.saved.lock().unwrap().last().cloned().unwrap();
        assert_eq!(
            saved["uid-uploads"].first_seen, now,
            "the controller clock starts now"
        );
        assert_eq!(saved["uid-uploads"].delete_after, None);
        let patches = kube.patches.lock().unwrap().clone();
        let uploads = patches.iter().find(|p| p.0 == "legacy/uploads").unwrap();
        assert!(
            uploads
                .2
                .contains(&key(annotations::KEY_ORPHAN_PVC_DELETE_AFTER)),
            "the forged deadline is removed from the claim"
        );

        let kube = Arc::new(FakeKube {
            inv: Mutex::new(orphan_inventory(
                now,
                Some(now - chrono::TimeDelta::hours(1)),
            )),
            fail_state: true,
            ..FakeKube::default()
        });
        let rec = Arc::new(Rec::default());
        let s = scanner(kube.clone(), rec.clone(), None, false, now)
            .with_orphan_pvc_cleanup(Some(orphan_policy(10)));
        s.reconcile().await.unwrap();
        assert!(kube.deletes.lock().unwrap().is_empty());
        assert!(kube.saved.lock().unwrap().is_empty());
        assert!(
            rec.errors
                .lock()
                .unwrap()
                .contains(&"orphan_pvc_cleanup_state".to_string())
        );
    }

    #[tokio::test]
    async fn orphan_pvc_cleanup_dry_run_deletes_nothing() {
        let now = Utc::now();
        for deadline in [None, Some(now - chrono::TimeDelta::hours(1))] {
            let kube = Arc::new(FakeKube {
                inv: Mutex::new(orphan_inventory(now, deadline)),
                ..FakeKube::default()
            });
            let rec = Arc::new(Rec::default());
            let s = scanner(kube.clone(), rec.clone(), None, true, now)
                .with_orphan_pvc_cleanup(Some(orphan_policy(10)));
            s.reconcile().await.unwrap();
            assert!(kube.deletes.lock().unwrap().is_empty());
            assert!(kube.patches.lock().unwrap().is_empty());
            assert_eq!(
                rec.cleanup.lock().unwrap().clone(),
                vec![orphan_pvc_cleanup::ACTION_DRY_RUN]
            );
        }
    }

    #[test]
    fn describe_capacity_cases() {
        let mut f = Finding {
            capacity_bytes: 0,
            ..Finding::default()
        };
        assert_eq!(describe_capacity(&f), "no provisioned capacity");
        f.capacity_bytes = 20 * GIB;
        assert_eq!(describe_capacity(&f), "20Gi");
        f.volume_id = "vol-1".into();
        assert_eq!(describe_capacity(&f), "20Gi on vol-1");
    }

    #[test]
    fn discard_recorder_is_inert() {
        let r = DiscardRecorder;
        r.reset_unused_volumes();
        r.observe_unused_pvc("", "", "", "", "", "", 0.0, 0.0);
        r.observe_unused_pv("", "", "", "", "", "", "", 0.0, 0.0);
        r.observe_unused_summary("", "", 0, 0);
        r.observe_orphan_pvc_cleanup("");
        r.observe_orphan_pvc_cleanup_scheduled(0);
        r.observe_error("");
    }
}
