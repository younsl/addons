//! The `PersistentVolumeClaimAutoscaler` controller. Each autoscaler is
//! reconciled when it changes and again every `claimReconcileInterval`: its
//! target claims are resolved, measured on the metrics backend, grown where a
//! target is crossed, and the outcome is written to status.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::StreamExt as _;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher;
use kube::{Api, Client};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::autoscaler::persistent_volume_claim::api::{self, ClaimApi};
use crate::autoscaler::persistent_volume_claim::crd::ClaimStatus;
use crate::autoscaler::persistent_volume_claim::crd::{
    PersistentVolumeClaimAutoscaler, PersistentVolumeClaimAutoscalerStatus,
};
use crate::autoscaler::persistent_volume_claim::metrics::{self, UsageSource};
use crate::autoscaler::persistent_volume_claim::policy::{
    Claim, Effective, LastScale, Outcome, REASON_EXPANSION_NOT_ALLOWED, Usage, compile, decide,
};
use crate::autoscaler::persistent_volume_claim::status::{self, Inactive, entry};
use crate::autoscaler::{
    CONDITION_ABLE_TO_SCALE, REASON_AMBIGUOUS, REASON_FAILED_GET_SCALE, REASON_FAILED_METRIC,
    REASON_FAILED_RESCALE, REASON_SCALING_DISABLED, condition, gib_quantity, quantity_bytes,
};
use crate::k8s::events::{Emitter, Target};
use crate::k8s::from_k8s_time;
use crate::observability::Metrics;

/// The `ScalingActive` reason for a spec the controller rejects.
pub const REASON_INVALID_SPEC: &str = "InvalidSpec";

#[derive(Debug, Error)]
pub enum ReconcileError {
    #[error("kubernetes API: {0}")]
    Api(String),
}

/// Everything a reconcile needs.
pub struct Context {
    pub api: Arc<dyn ClaimApi>,
    pub usage: Arc<dyn UsageSource>,
    /// Scopes every query to this cluster on a shared backend.
    pub label_selector: BTreeMap<String, String>,
    pub dry_run: bool,
    pub interval: Duration,
    pub metrics: Arc<Metrics>,
    pub events: Option<Emitter>,
}

impl Context {
    fn event(&self, target: Target, warning: bool, reason: &str, message: String) {
        if let Some(e) = &self.events {
            e.event(
                target,
                if warning { "Warning" } else { "Normal" },
                reason,
                message,
            );
        }
    }
}

/// The claims of every other autoscaler in the namespace that `mine` shares,
/// as `claim -> other autoscaler`.
fn overlaps(
    me: &PersistentVolumeClaimAutoscaler,
    mine: &BTreeSet<String>,
    others: &[Arc<PersistentVolumeClaimAutoscaler>],
    claims: &[String],
    stateful_sets: &[Arc<k8s_openapi::api::apps::v1::StatefulSet>],
) -> BTreeMap<String, String> {
    let mut shared = BTreeMap::new();
    for other in others {
        if other.metadata.name == me.metadata.name {
            continue;
        }
        let Ok(theirs) = api::resolve(&other.spec.scale_target_ref, claims, stateful_sets) else {
            continue;
        };
        for c in mine.intersection(&theirs) {
            shared
                .entry(c.clone())
                .or_insert_with(|| other.metadata.name.clone().unwrap_or_default());
        }
    }
    shared
}

/// A status for an autoscaler that cannot evaluate its claims.
fn halted(
    pvca: &PersistentVolumeClaimAutoscaler,
    entries: Vec<crate::autoscaler::persistent_volume_claim::crd::ClaimStatus>,
    reason: &'static str,
    message: String,
    now: DateTime<Utc>,
) -> PersistentVolumeClaimAutoscalerStatus {
    let mut s = status::build(
        pvca.status.as_ref(),
        pvca.metadata.generation,
        entries,
        Some(Inactive { reason, message }),
        now,
    );
    s.refresh_summary(&pvca.spec);
    s
}

/// One claim set to a single reason, for a status written without measuring.
fn uniform(
    claims: &[Claim],
    recorded: &HashMap<String, LastScale>,
    reason: &'static str,
    message: &str,
) -> Vec<crate::autoscaler::persistent_volume_claim::crd::ClaimStatus> {
    claims
        .iter()
        .map(|c| {
            let o = Outcome {
                reason,
                message: message.to_string(),
                storage: None,
                inodes: None,
                current_gib: None,
                desired_gib: None,
                cooldown_until: None,
                capacity_at_last_scale: recorded.get(&c.name).and_then(|r| r.capacity_bytes),
            };
            entry(c, &o, None)
        })
        .collect()
}

/// Evaluates one autoscaler, grows its claims, and returns the status to
/// write.
pub async fn evaluate(
    pvca: &PersistentVolumeClaimAutoscaler,
    ctx: &Context,
    now: DateTime<Utc>,
) -> Result<PersistentVolumeClaimAutoscalerStatus, ReconcileError> {
    let namespace = pvca.metadata.namespace.clone().unwrap_or_default();
    let eff = match compile(pvca) {
        Ok(eff) => eff,
        Err(err) => return Ok(halted(pvca, Vec::new(), REASON_INVALID_SPEC, err, now)),
    };

    let (objects, stateful_sets, classes, autoscalers) = futures::try_join!(
        ctx.api.list_claims(&namespace),
        ctx.api.list_stateful_sets(&namespace),
        ctx.api.storage_classes(),
        ctx.api.list_autoscalers(&namespace),
    )
    .map_err(ReconcileError::Api)?;
    let names: Vec<String> = objects
        .iter()
        .filter_map(|p| p.metadata.name.clone())
        .collect();
    let targets = match api::resolve(&pvca.spec.scale_target_ref, &names, &stateful_sets) {
        Ok(t) => t,
        Err(err) => return Ok(halted(pvca, Vec::new(), REASON_FAILED_GET_SCALE, err, now)),
    };
    let claims: Vec<Claim> = objects
        .iter()
        .filter(|p| {
            p.metadata
                .name
                .as_ref()
                .is_some_and(|n| targets.contains(n))
        })
        .map(|p| api::claim(p, &classes))
        .collect();
    let blockers = api::expansion_blockers(
        &pvca.spec.scale_target_ref,
        &stateful_sets,
        &claims,
        &classes,
    );

    let shared = overlaps(pvca, &targets, &autoscalers, &names, &stateful_sets);
    if !shared.is_empty() {
        let detail: Vec<String> = shared.iter().map(|(c, o)| format!("{c} ({o})")).collect();
        let message = format!(
            "claims also targeted by another autoscaler: {}",
            detail.join(", ")
        );
        let entries = uniform(&claims, &recorded(pvca), REASON_AMBIGUOUS, &message);
        return Ok(halted(pvca, entries, REASON_AMBIGUOUS, message, now));
    }

    let claim_names: Vec<&str> = claims.iter().map(|c| c.name.as_str()).collect();
    let usage = match metrics::usage(
        ctx.usage.as_ref(),
        &namespace,
        &claim_names,
        &ctx.label_selector,
        metrics::Wanted {
            storage: eff.storage_target.is_some(),
            inodes: eff.inodes_target.is_some(),
            capacity: eff.storage_target.is_some() || eff.inodes_target.is_some(),
        },
    )
    .await
    {
        Ok(u) => u,
        Err(err) => {
            let message = format!("metrics query failed: {err}");
            let entries = uniform(&claims, &recorded(pvca), REASON_FAILED_METRIC, &message);
            return Ok(halted(pvca, entries, REASON_FAILED_METRIC, message, now));
        }
    };

    let entries = decide_claims(ctx, pvca, &eff, &claims, &usage, now).await;

    let inactive = eff.paused.then(|| Inactive {
        reason: REASON_SCALING_DISABLED,
        message: "behavior.scaleUp.selectPolicy is Disabled".into(),
    });
    let mut s = status::build(
        pvca.status.as_ref(),
        pvca.metadata.generation,
        entries,
        inactive,
        now,
    );
    block_unexpandable(ctx, pvca, &mut s, &blockers, now);
    s.refresh_summary(&pvca.spec);
    Ok(s)
}

/// Decides, resizes, and records every target claim.
async fn decide_claims(
    ctx: &Context,
    pvca: &PersistentVolumeClaimAutoscaler,
    eff: &Effective,
    claims: &[Claim],
    usage: &HashMap<String, Usage>,
    now: DateTime<Utc>,
) -> Vec<ClaimStatus> {
    let namespace = pvca.metadata.namespace.clone().unwrap_or_default();
    let name = pvca.metadata.name.clone().unwrap_or_default();
    let recorded = recorded(pvca);
    let mut entries = Vec::with_capacity(claims.len());
    for c in claims {
        let u = usage.get(&c.name).copied().unwrap_or_default();
        let last = recorded.get(&c.name).copied().unwrap_or_default();
        let mut o = decide(eff, c, u, last, ctx.dry_run, now);
        let scaled_at = apply(ctx, pvca, &namespace, &name, c, &mut o, now).await;
        ctx.metrics.observe_claim(
            [&namespace, &c.name, &name],
            c.capacity_bytes,
            [("storage", o.storage), ("inodes", o.inodes)],
        );
        entries.push(entry(c, &o, scaled_at));
    }
    entries
}

/// What the previous status recorded about each claim's last resize.
fn recorded(pvca: &PersistentVolumeClaimAutoscaler) -> HashMap<String, LastScale> {
    pvca.status
        .iter()
        .flat_map(|s| &s.claims)
        .map(|c| {
            (
                c.name.clone(),
                LastScale {
                    at: c.last_scale_time.as_ref().and_then(from_k8s_time),
                    capacity_bytes: c
                        .capacity_at_last_scale
                        .as_ref()
                        .and_then(|q| quantity_bytes(q).ok()),
                },
            )
        })
        .collect()
}

/// Turns `AbleToScale` false while a `StorageClass` blocks expansion, and
/// warns once when that starts.
fn block_unexpandable(
    ctx: &Context,
    pvca: &PersistentVolumeClaimAutoscaler,
    s: &mut PersistentVolumeClaimAutoscalerStatus,
    blockers: &[String],
    now: DateTime<Utc>,
) {
    if blockers.is_empty() {
        return;
    }
    status::block_expansion(s, blockers, pvca.metadata.generation, now);
    let was_blocked = pvca
        .status
        .as_ref()
        .and_then(|p| condition(&p.conditions, CONDITION_ABLE_TO_SCALE))
        .is_some_and(|c| c.reason == REASON_EXPANSION_NOT_ALLOWED);
    if was_blocked {
        return;
    }
    let namespace = pvca.metadata.namespace.as_deref().unwrap_or_default();
    let name = pvca.metadata.name.as_deref().unwrap_or_default();
    warn!(namespace, autoscaler = name, blockers = ?blockers, "PersistentVolumeClaimAutoscaler targets volumes that cannot be expanded");
    ctx.event(
        Target::claim_autoscaler(
            namespace,
            name,
            pvca.metadata.uid.as_deref().unwrap_or_default(),
        ),
        true,
        REASON_EXPANSION_NOT_ALLOWED,
        blockers.join("; "),
    );
}

/// The Warning Event reason for a resize Argo CD will see as drift.
pub const REASON_GITOPS_DRIFT: &str = "GitOpsDriftExpected";

/// What an operator must do so Argo CD accepts a resized claim.
fn drift_warning(app: &str, namespace: &str, claim: &str, size: &str) -> String {
    format!(
        "Argo CD Application {app} tracks this claim and now sees spec.resources.requests.storage {size} as drift. Unless it sets ignoreDifferences {{kind: PersistentVolumeClaim, name: {claim}, namespace: {namespace}, jsonPointers: [/spec/resources/requests/storage]}}, a sync tries to shrink the claim and the API server rejects it"
    )
}

/// Sends the resize the outcome asks for and returns when it was sent.
async fn apply(
    ctx: &Context,
    pvca: &PersistentVolumeClaimAutoscaler,
    namespace: &str,
    name: &str,
    c: &Claim,
    o: &mut Outcome,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if !o.resize() {
        ctx.metrics.observe_claim_skip(namespace, name, o.reason);
        return None;
    }
    let desired = gib_quantity(o.desired_gib.unwrap_or_default());
    let pvca_target = Target::claim_autoscaler(
        namespace,
        name,
        pvca.metadata.uid.as_deref().unwrap_or_default(),
    );
    match ctx.api.resize_claim(namespace, &c.name, &desired).await {
        Ok(()) => {
            ctx.metrics.observe_claim_resize(namespace, name, true);
            info!(namespace, autoscaler = name, claim = %c.name, desired = %desired.0, detail = %o.message, "PersistentVolumeClaim resize requested");
            let msg = format!("{}: {}", c.name, o.message);
            ctx.event(
                Target::claim(namespace, &c.name, &c.uid),
                false,
                o.reason,
                o.message.clone(),
            );
            ctx.event(pvca_target.clone(), false, o.reason, msg);
            if let Some(app) = &c.tracked_by {
                let drift = drift_warning(app, namespace, &c.name, &desired.0);
                warn!(namespace, autoscaler = name, claim = %c.name, application = %app, "resized a claim an Argo CD Application tracks");
                ctx.event(
                    Target::claim(namespace, &c.name, &c.uid),
                    true,
                    REASON_GITOPS_DRIFT,
                    drift.clone(),
                );
                ctx.event(
                    pvca_target,
                    true,
                    REASON_GITOPS_DRIFT,
                    format!("{}: {drift}", c.name),
                );
            }
            Some(now)
        }
        Err(err) => {
            ctx.metrics.observe_claim_resize(namespace, name, false);
            warn!(namespace, autoscaler = name, claim = %c.name, error = %err, "PersistentVolumeClaim resize failed");
            o.reason = REASON_FAILED_RESCALE;
            o.message = format!("resize to {} failed: {err}", desired.0);
            ctx.event(
                pvca_target,
                true,
                REASON_FAILED_RESCALE,
                format!("{}: {}", c.name, o.message),
            );
            None
        }
    }
}

async fn reconcile(
    pvca: Arc<PersistentVolumeClaimAutoscaler>,
    ctx: Arc<Context>,
) -> Result<Action, ReconcileError> {
    let namespace = pvca.metadata.namespace.clone().unwrap_or_default();
    let name = pvca.metadata.name.clone().unwrap_or_default();
    let result = evaluate(&pvca, &ctx, Utc::now()).await;
    ctx.metrics
        .observe_claim_reconcile(&namespace, &name, result.is_err());
    let next = result?;
    if pvca.status.as_ref() != Some(&next) {
        ctx.api
            .patch_status(&namespace, &name, &next)
            .await
            .map_err(ReconcileError::Api)?;
        debug!(namespace, autoscaler = name, reason = %next.reason, "PersistentVolumeClaimAutoscaler status written");
    }
    Ok(Action::requeue(ctx.interval))
}

#[allow(clippy::needless_pass_by_value)]
fn error_policy(
    pvca: Arc<PersistentVolumeClaimAutoscaler>,
    err: &ReconcileError,
    ctx: Arc<Context>,
) -> Action {
    warn!(
        namespace = pvca.metadata.namespace.as_deref().unwrap_or_default(),
        autoscaler = pvca.metadata.name.as_deref().unwrap_or_default(),
        error = %err,
        "PersistentVolumeClaimAutoscaler reconcile failed"
    );
    Action::requeue(ctx.interval)
}

/// Runs the controller until `shutdown` fires. Leader-only, like every other
/// writer.
pub async fn run(client: Client, ctx: Arc<Context>, shutdown: CancellationToken) {
    tokio::select! {
        () = shutdown.cancelled() => return,
        ready = ctx.api.ready() => if let Err(err) = ready {
            warn!(error = %err, "PersistentVolumeClaimAutoscaler caches never synced");
            return;
        },
    }
    info!("PersistentVolumeClaimAutoscaler caches synced");
    Controller::new(
        Api::<PersistentVolumeClaimAutoscaler>::all(client),
        watcher::Config::default(),
    )
    .graceful_shutdown_on(shutdown.cancelled_owned())
    .run(reconcile, error_policy, ctx)
    .for_each(|res| async move {
        if let Err(err) = res {
            debug!(error = %err, "PersistentVolumeClaimAutoscaler controller event");
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::autoscaler::persistent_volume_claim::api::tests::{Fake, pvc, sts};
    use crate::autoscaler::persistent_volume_claim::metrics::tests::{Fake as FakeUsage, sample};
    use crate::autoscaler::persistent_volume_claim::policy::REASON_AWAITING_FILESYSTEM_RESIZE;
    use crate::autoscaler::persistent_volume_claim::policy::tests::{base, pvca};
    use crate::autoscaler::{
        CONDITION_ABLE_TO_SCALE, CONDITION_SCALING_ACTIVE, REASON_BELOW_TARGET, REASON_DRY_RUN,
        REASON_SUCCEEDED_RESCALE, condition,
    };
    use crate::k8s::events::capture::Capture;

    fn cluster() -> Fake {
        Fake {
            claims: vec![
                pvc("data-web-0", "10Gi", "10Gi"),
                pvc("data-web-1", "10Gi", "10Gi"),
                pvc("other", "10Gi", "10Gi"),
            ],
            stateful_sets: vec![sts("web", &["data"])],
            classes: BTreeMap::from([("gp3".to_string(), true)]),
            ..Fake::default()
        }
    }

    fn ctx(api: Fake, usage: FakeUsage, dry_run: bool) -> (Context, Arc<Fake>, Arc<Capture>) {
        let api = Arc::new(api);
        let capture = Arc::new(Capture::default());
        (
            Context {
                api: api.clone(),
                usage: Arc::new(usage),
                label_selector: BTreeMap::new(),
                dry_run,
                interval: Duration::from_mins(1),
                metrics: Arc::new(Metrics::new()),
                events: Some(Emitter::new(capture.clone())),
            },
            api,
            capture,
        )
    }

    fn full(claim: &str) -> FakeUsage {
        FakeUsage {
            answers: vec![(
                "used_bytes",
                Ok(vec![sample(claim, 90.0), sample("data-web-1", 10.0)]),
            )],
            ..FakeUsage::default()
        }
    }

    fn reason_of(s: &PersistentVolumeClaimAutoscalerStatus, t: &str) -> String {
        condition(&s.conditions, t).unwrap().reason.clone()
    }

    #[tokio::test]
    async fn grows_the_full_claim_and_reports() {
        let (ctx, api, capture) = ctx(cluster(), full("data-web-0"), false);
        let p = pvca(base());
        let s = evaluate(&p, &ctx, Utc::now()).await.unwrap();
        assert_eq!(
            *api.resized.lock().unwrap(),
            vec![("data-web-0".to_string(), "11Gi".to_string())]
        );
        assert_eq!(s.current_volumes, 2);
        assert_eq!(s.claims[0].reason, REASON_SUCCEEDED_RESCALE);
        assert!(s.claims[0].last_scale_time.is_some());
        assert_eq!(s.claims[1].reason, REASON_BELOW_TARGET);
        assert_eq!(s.utilization, "90%/80%");
        assert_eq!(s.target, "StatefulSet/web");
        assert_eq!(
            reason_of(&s, CONDITION_ABLE_TO_SCALE),
            REASON_SUCCEEDED_RESCALE
        );
        capture.wait_until(|c| c.summary().len() == 2).await;
        let kinds: Vec<String> = capture.summary().into_iter().map(|e| e.0).collect();
        assert!(kinds.contains(&"PersistentVolumeClaim".to_string()));
        assert!(kinds.contains(&"PersistentVolumeClaimAutoscaler".to_string()));

        let mut again = p.clone();
        again.status = Some(s);
        let s = evaluate(&again, &ctx, Utc::now()).await.unwrap();
        assert_eq!(s.claims[0].reason, crate::autoscaler::REASON_COOLDOWN);
        assert_eq!(api.resized.lock().unwrap().len(), 1, "no second resize");
    }

    #[tokio::test]
    async fn resizing_an_argocd_tracked_claim_warns_about_drift() {
        let mut api = cluster();
        api.claims[0].metadata.annotations = Some(BTreeMap::from([(
            crate::autoscaler::persistent_volume_claim::api::ARGOCD_TRACKING_ANNOTATION.to_string(),
            "shared-web:/PersistentVolumeClaim:ns/data-web-0".to_string(),
        )]));
        let (c, api, capture) = ctx(api, full("data-web-0"), false);
        let s = evaluate(&pvca(base()), &c, Utc::now()).await.unwrap();
        assert_eq!(api.resized.lock().unwrap().len(), 1);
        assert_eq!(s.claims[0].tracked_by.as_deref(), Some("shared-web"));
        assert_eq!(s.claims[1].tracked_by, None);
        capture.wait_until(|c| c.summary().len() == 4).await;
        let drift: Vec<_> = capture
            .summary()
            .into_iter()
            .filter(|e| e.4 == REASON_GITOPS_DRIFT)
            .collect();
        assert_eq!(drift.len(), 2, "on the claim and on the autoscaler");
        assert!(drift.iter().all(|e| e.3 == "Warning"));
        assert!(
            drift[0].5.contains("/spec/resources/requests/storage"),
            "{}",
            drift[0].5
        );
        assert!(drift[0].5.contains("shared-web"), "{}", drift[0].5);

        let (c, _, capture) = ctx(cluster(), full("data-web-0"), false);
        evaluate(&pvca(base()), &c, Utc::now()).await.unwrap();
        capture.wait_until(|c| c.summary().len() == 2).await;
        assert!(
            capture.summary().iter().all(|e| e.4 != REASON_GITOPS_DRIFT),
            "untracked claims stay quiet"
        );
    }

    #[tokio::test]
    async fn stale_filesystem_size_does_not_resize_twice() {
        let gib = crate::autoscaler::GIB;
        let usage = |fs_gib: f64| FakeUsage {
            answers: vec![
                ("used_bytes", Ok(vec![sample("data-web-0", 90.0)])),
                (
                    "capacity_bytes",
                    Ok(vec![sample("data-web-0", fs_gib * gib)]),
                ),
            ],
            ..FakeUsage::default()
        };
        let (c, api, _) = ctx(cluster(), usage(9.8), false);
        let first = evaluate(&pvca(base()), &c, Utc::now()).await.unwrap();
        assert_eq!(api.resized.lock().unwrap().len(), 1);
        let recorded = first.claims[0].capacity_at_last_scale.clone().unwrap();
        assert_eq!(
            quantity_bytes(&recorded).unwrap().round(),
            (9.8 * gib).round()
        );

        let mut later = pvca(base());
        let mut status = first.clone();
        status.claims[0].last_scale_time = Some(crate::k8s::k8s_time(
            Utc::now() - chrono::TimeDelta::hours(1),
        ));
        later.status = Some(status);
        let s = evaluate(&later, &c, Utc::now()).await.unwrap();
        assert_eq!(s.claims[0].reason, REASON_AWAITING_FILESYSTEM_RESIZE);
        assert_eq!(
            api.resized.lock().unwrap().len(),
            1,
            "no second resize on a stale size"
        );
        assert!(
            s.claims[0].capacity_at_last_scale.is_some(),
            "still recorded"
        );

        let (c, api, _) = ctx(cluster(), usage(10.7), false);
        let s = evaluate(&later, &c, Utc::now()).await.unwrap();
        assert_eq!(
            s.claims[0].reason, REASON_SUCCEEDED_RESCALE,
            "grew, still full"
        );
        assert_eq!(api.resized.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn reconcile_writes_status_once() {
        let (ctx, api, _) = ctx(cluster(), full("data-web-0"), true);
        let ctx = Arc::new(ctx);
        let p = pvca(base());
        reconcile(Arc::new(p.clone()), ctx.clone()).await.unwrap();
        let written = api.statuses.lock().unwrap()[0].clone();
        assert_eq!(written.claims[0].reason, REASON_DRY_RUN);
        assert!(api.resized.lock().unwrap().is_empty());
        let mut same = p;
        same.status = Some(written);
        reconcile(Arc::new(same), ctx.clone()).await.unwrap();
        assert_eq!(
            api.statuses.lock().unwrap().len(),
            1,
            "unchanged status is not rewritten"
        );
        let a = error_policy(
            Arc::new(pvca(base())),
            &ReconcileError::Api("x".into()),
            ctx,
        );
        assert_eq!(a, Action::requeue(Duration::from_mins(1)));
    }

    #[tokio::test]
    async fn halts_on_invalid_missing_ambiguous_and_metric_failures() {
        let now = Utc::now();
        let mut bad = base();
        bad["maxCapacity"] = "100Mi".into();
        let (c, _, _) = ctx(cluster(), FakeUsage::default(), false);
        let s = evaluate(&pvca(bad), &c, now).await.unwrap();
        assert_eq!(reason_of(&s, CONDITION_SCALING_ACTIVE), REASON_INVALID_SPEC);

        let mut gone = base();
        gone["scaleTargetRef"]["name"] = "db".into();
        let s = evaluate(&pvca(gone), &c, now).await.unwrap();
        assert_eq!(
            reason_of(&s, CONDITION_ABLE_TO_SCALE),
            REASON_FAILED_GET_SCALE
        );
        assert_eq!(s.reason, REASON_FAILED_GET_SCALE);

        let mut api = cluster();
        let mut single = base();
        single["scaleTargetRef"] =
            serde_json::json!({"kind": "PersistentVolumeClaim", "name": "data-web-1"});
        let mut other = pvca(single);
        other.metadata.name = Some("single".into());
        api.autoscalers = vec![pvca(base()), other];
        let (c, api, _) = ctx(api, full("data-web-0"), false);
        let s = evaluate(&pvca(base()), &c, now).await.unwrap();
        assert_eq!(reason_of(&s, CONDITION_SCALING_ACTIVE), REASON_AMBIGUOUS);
        assert!(s.claims.iter().all(|c| c.reason == REASON_AMBIGUOUS));
        assert!(api.resized.lock().unwrap().is_empty());

        let failing = FakeUsage {
            answers: vec![("used_bytes", Err("503 Service Unavailable".into()))],
            ..FakeUsage::default()
        };
        let (c, _, _) = ctx(cluster(), failing, false);
        let s = evaluate(&pvca(base()), &c, now).await.unwrap();
        assert_eq!(
            reason_of(&s, CONDITION_SCALING_ACTIVE),
            REASON_FAILED_METRIC
        );
        assert!(s.claims[0].message.contains("503"));

        let mut down = cluster();
        down.fail_list = true;
        let (c, _, _) = ctx(down, FakeUsage::default(), false);
        assert!(evaluate(&pvca(base()), &c, now).await.is_err());
    }

    #[tokio::test]
    async fn non_expandable_class_blocks_and_warns_once() {
        let mut api = cluster();
        api.classes.insert("gp3".into(), false);
        let (c, api, capture) = ctx(api, full("data-web-0"), false);
        let s = evaluate(&pvca(base()), &c, Utc::now()).await.unwrap();
        let able = condition(&s.conditions, CONDITION_ABLE_TO_SCALE).unwrap();
        assert_eq!(able.reason, REASON_EXPANSION_NOT_ALLOWED);
        assert!(
            able.message
                .contains("StorageClass gp3 does not set allowVolumeExpansion"),
            "{}",
            able.message
        );
        assert!(
            able.message.contains("volumeClaimTemplate data"),
            "{}",
            able.message
        );
        assert_eq!(
            s.reason, REASON_EXPANSION_NOT_ALLOWED,
            "shown in kubectl get"
        );
        assert!(api.resized.lock().unwrap().is_empty());
        capture.wait_until(|c| c.summary().len() == 1).await;

        let mut again = pvca(base());
        again.status = Some(s);
        evaluate(&again, &c, Utc::now()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(capture.summary().len(), 1, "warned once, not every pass");
        assert_eq!(capture.summary()[0].4, REASON_EXPANSION_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn failed_resize_and_pause() {
        let mut api = cluster();
        api.fail_resize = true;
        let (c, _, _) = ctx(api, full("data-web-0"), false);
        let s = evaluate(&pvca(base()), &c, Utc::now()).await.unwrap();
        assert_eq!(s.claims[0].reason, REASON_FAILED_RESCALE);
        assert!(s.claims[0].message.contains("forbidden"));
        assert_eq!(
            reason_of(&s, CONDITION_ABLE_TO_SCALE),
            REASON_FAILED_RESCALE
        );

        let mut paused = base();
        paused["behavior"] = serde_json::json!({"scaleUp": {"selectPolicy": "Disabled"}});
        let (c, api, _) = ctx(cluster(), full("data-web-0"), false);
        let s = evaluate(&pvca(paused), &c, Utc::now()).await.unwrap();
        assert_eq!(
            reason_of(&s, CONDITION_SCALING_ACTIVE),
            REASON_SCALING_DISABLED
        );
        assert_eq!(s.claims[0].reason, REASON_SCALING_DISABLED);
        assert!(api.resized.lock().unwrap().is_empty());
    }
}
