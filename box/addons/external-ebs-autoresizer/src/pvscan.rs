//! Identifies `PersistentVolumeClaims` and `PersistentVolumes` that no
//! workload is using, so the EBS volumes behind them stop billing unnoticed.
//!
//! It only ever identifies. Nothing here deletes a claim or a volume, and the
//! module has no access to one: the Kubernetes surface it depends on is list
//! and annotate, and it never touches EC2 at all. "Unused" is an observation
//! about the current state of the cluster, not a statement that the data is
//! disposable, so the decision stays with an operator reading the annotation.
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
pub mod types;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use tracing::{debug, error, info};

use crate::k8s::events::{Emitter, TYPE_NORMAL, Target};
pub use annotations::{build_annotations, key};
pub use decide::classify;
pub use defaults::{INTERVAL, MIN_UNUSED_AGE};
pub use kube::KubeClient;
pub use types::*;

/// The subset of Kubernetes operations this module depends on. Every
/// operation is a list or an annotation patch: there is no delete, by
/// construction.
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
    fn observe_error(&self, _: &str) {}
}

/// Kubernetes Event reasons published against a claim or a volume. Both are
/// Normal, not Warning: an unused claim is a cost observation, and a cluster
/// with a hundred of them would drown the Warnings that mean something is
/// failing.
pub const REASON_UNUSED_DETECTED: &str = "UnusedVolumeDetected";
pub const REASON_UNUSED_CLEARED: &str = "UnusedVolumeCleared";

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
        }
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
        let findings = classify(&inv, self.min_unused_age, now);

        // The reset is what keeps a deleted claim from staying exported
        // forever and reading as a live one. It runs before the loop so a pass
        // that aborts partway leaves the gauges holding only what it managed
        // to observe, never a mix of two passes.
        self.rec.reset_unused_volumes();
        for f in &findings {
            self.report(f);
            match self.publish(f, now).await {
                Ok(outcome) => {
                    debug!(kind = %f.kind, object = %f.key(), unused = f.unused, outcome, "unused-volume annotations settled");
                    self.emit(f, outcome);
                }
                Err(err) => {
                    self.rec.observe_error("pv_annotate");
                    error!(kind = %f.kind, object = %f.key(), reason = %f.reason, outcome = "failed", error = %err,
                        "failed to annotate object with its unused-volume verdict");
                }
            }
        }
        self.summarize(&findings);
        Ok(findings.len())
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
            unused_since = f.unused_since.map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true)).unwrap_or_default(),
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
    fn emit(&self, f: &Finding, outcome: &str) {
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
    struct FakeKube {
        inv: Mutex<Inventory>,
        fail_inventory: bool,
        fail_annotate: bool,
        patches: Mutex<Vec<Patch>>,
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
    }

    #[derive(Default)]
    struct Rec {
        pvcs: Mutex<Vec<(String, String, f64)>>,
        pvs: Mutex<Vec<(String, String)>>,
        summaries: Mutex<Vec<(String, String, usize, i64)>>,
        errors: Mutex<Vec<String>>,
        resets: Mutex<usize>,
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
        assert!(uploads.2.is_empty());
        let pending = &patches[1];
        assert!(
            !pending.1.contains_key(&key("volume-id")),
            "unbound claim has no volume"
        );
        assert_eq!(pending.2, vec![key("volume-id")]);

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
        r.observe_error("");
    }
}
