//! Orchestrates the measure -> decide -> grow -> wait -> expand flow for the
//! root EBS volume of each target standalone EC2 instance.

pub mod cordon;
pub mod piggyback;
pub mod report;
pub mod status;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

use crate::autoscaler::format_cooldown;
use crate::autoscaler::instance_volume::api::{AutoscalerApi, REASON_INVALID_SPEC};
use crate::autoscaler::instance_volume::crd::{InstanceStatus, InstanceVolumeAutoscaler};
use crate::autoscaler::instance_volume::policy::{Effective, Resolver};
use crate::awsx::{ApiError, Clients, CommandResult, Instance, ModifySpec, VolumeModification};
use crate::config::Config;
use crate::humanize::go_duration;
use crate::k8s::cordon::{CordonApi, CordonNode};
use crate::k8s::events::{Emitter, TYPE_WARNING, Target};
use crate::recstore::Store;
use crate::scripts;
pub use piggyback::{APPLY_RESULT_APPLIED, APPLY_RESULT_FALLBACK, recommendation_max_age};
pub use report::{AlertNotifier, Annotator};
use status::{
    Inactive, Observation, REASON_AMBIGUOUS, REASON_BELOW_TARGET, REASON_COOLDOWN, REASON_DRY_RUN,
    REASON_FAILED_GET_SCALE, REASON_FAILED_METRIC, REASON_FAILED_RESCALE, REASON_MAX_CAPACITY,
    REASON_NO_ROOT_VOLUME, REASON_SCALING_DISABLED, REASON_SUCCEEDED_RESCALE,
};

/// The subset of EC2 operations the resizer depends on.
#[async_trait]
pub trait Ec2Api: Send + Sync {
    async fn describe_target_instances(&self) -> Result<Vec<Instance>, ApiError>;
    async fn modify_volume(&self, volume_id: &str, spec: ModifySpec) -> Result<(), ApiError>;
    async fn describe_last_modification(
        &self,
        volume_id: &str,
    ) -> Result<Option<VolumeModification>, ApiError>;
    async fn wait_for_modification(
        &self,
        volume_id: &str,
        timeout: Duration,
    ) -> Result<(), ApiError>;
}

#[async_trait]
impl Ec2Api for Clients {
    async fn describe_target_instances(&self) -> Result<Vec<Instance>, ApiError> {
        Self::describe_target_instances(self).await
    }
    async fn modify_volume(&self, volume_id: &str, spec: ModifySpec) -> Result<(), ApiError> {
        Self::modify_volume(self, volume_id, spec).await
    }
    async fn describe_last_modification(
        &self,
        volume_id: &str,
    ) -> Result<Option<VolumeModification>, ApiError> {
        Self::describe_last_modification(self, volume_id).await
    }
    async fn wait_for_modification(
        &self,
        volume_id: &str,
        timeout: Duration,
    ) -> Result<(), ApiError> {
        Self::wait_for_modification(self, volume_id, timeout).await
    }
}

/// The subset of SSM operations the resizer depends on.
#[async_trait]
pub trait SsmApi: Send + Sync {
    async fn run_script(
        &self,
        instance_id: &str,
        script: &str,
        timeout: Duration,
    ) -> Result<CommandResult, ApiError>;
}

#[async_trait]
impl SsmApi for Clients {
    async fn run_script(
        &self,
        instance_id: &str,
        script: &str,
        timeout: Duration,
    ) -> Result<CommandResult, ApiError> {
        Self::run_script(self, instance_id, script, timeout).await
    }
}

/// Receives metrics observations. `observability::Metrics` implements it.
pub trait Recorder: Send + Sync {
    fn observe_usage(
        &self,
        instance_id: &str,
        device: &str,
        volume_id: &str,
        name: &str,
        percent: f64,
    );
    fn observe_volume_size(
        &self,
        instance_id: &str,
        device: &str,
        volume_id: &str,
        name: &str,
        size_gib: i32,
    );
    fn observe_resize(&self, success: bool, policy: &str);
    fn observe_skip(&self, reason: &str, policy: &str);
    fn observe_error(&self, stage: &str);
    fn observe_policy_instances(&self, counts: &HashMap<String, usize>);
    fn observe_throughput_apply(&self, result: &str);
    fn observe_throughput_apply_skip(&self, reason: &str);
    fn observe_protective_cordon(&self, action: &str, result: &str);
}

impl Recorder for crate::observability::Metrics {
    fn observe_usage(
        &self,
        instance_id: &str,
        device: &str,
        volume_id: &str,
        name: &str,
        percent: f64,
    ) {
        Self::observe_usage(self, instance_id, device, volume_id, name, percent);
    }
    fn observe_volume_size(
        &self,
        instance_id: &str,
        device: &str,
        volume_id: &str,
        name: &str,
        size_gib: i32,
    ) {
        Self::observe_volume_size(self, instance_id, device, volume_id, name, size_gib);
    }
    fn observe_resize(&self, success: bool, policy: &str) {
        Self::observe_resize(self, success, policy);
    }
    fn observe_skip(&self, reason: &str, policy: &str) {
        Self::observe_skip(self, reason, policy);
    }
    fn observe_error(&self, stage: &str) {
        Self::observe_error(self, stage);
    }
    fn observe_policy_instances(&self, counts: &HashMap<String, usize>) {
        Self::observe_policy_instances(self, counts);
    }
    fn observe_throughput_apply(&self, result: &str) {
        Self::observe_throughput_apply(self, result);
    }
    fn observe_throughput_apply_skip(&self, reason: &str) {
        Self::observe_throughput_apply_skip(self, reason);
    }
    fn observe_protective_cordon(&self, action: &str, result: &str) {
        Self::observe_protective_cordon(self, action, result);
    }
}

/// Skip reasons reported via `observe_skip` when an instance is above the
/// usage threshold but no resize is attempted. They surface the silent
/// states that `resize_total` and `error_total` do not capture.
pub const SKIP_BELOW_THRESHOLD: &str = "below_threshold";
pub const SKIP_MAX_SIZE: &str = "max_size";
pub const SKIP_COOLDOWN: &str = "cooldown";
pub const SKIP_DRY_RUN: &str = "dry_run";
pub const SKIP_PAUSED: &str = "paused";
pub const SKIP_AMBIGUOUS: &str = "ambiguous";

/// The Kubernetes Events sink about resize attempts: the emitter and the
/// controller's own Pod they attach to.
#[derive(Clone)]
pub struct PodEvents {
    pub emitter: Emitter,
    pub target: Target,
}

/// Holds dependencies for one reconcile pass.
pub struct Resizer {
    cfg: Arc<Config>,
    /// Where the resize settings come from. `None` targets no instance.
    autoscalers: Option<Arc<dyn AutoscalerApi>>,
    ec2: Arc<dyn Ec2Api>,
    ssm: Arc<dyn SsmApi>,
    rec: Arc<dyn Recorder>,
    events: Option<PodEvents>,
    notifier: Option<Arc<dyn AlertNotifier>>,
    annotator: Option<Arc<dyn Annotator>>,
    /// Where the resizer looks up the latest throughput recommendation for a
    /// volume it is about to modify, and the Node the volume belongs to.
    /// `None` disables both throughput piggybacking and Node events.
    recs: Option<Arc<Store>>,
    /// Publishes Events against Node objects. `None` disables Node Events.
    node_events: Option<Emitter>,
    /// Publishes Events against autoscalers. `None` disables them.
    autoscaler_events: Option<Emitter>,
    /// Cordons and uncordons Nodes. `None` disables the protective cordon.
    cordon: Option<Arc<dyn CordonApi>>,
}

/// The optional collaborators of a resizer.
#[derive(Default)]
pub struct Options {
    pub autoscalers: Option<Arc<dyn AutoscalerApi>>,
    pub events: Option<PodEvents>,
    pub notifier: Option<Arc<dyn AlertNotifier>>,
    pub annotator: Option<Arc<dyn Annotator>>,
    pub recs: Option<Arc<Store>>,
    pub node_events: Option<Emitter>,
    pub autoscaler_events: Option<Emitter>,
    pub cordon: Option<Arc<dyn CordonApi>>,
}

impl Resizer {
    /// Constructs a resizer.
    #[must_use]
    pub fn new(
        cfg: Arc<Config>,
        ec2: Arc<dyn Ec2Api>,
        ssm: Arc<dyn SsmApi>,
        rec: Arc<dyn Recorder>,
        opts: Options,
    ) -> Self {
        Self {
            cfg,
            autoscalers: opts.autoscalers,
            ec2,
            ssm,
            rec,
            events: opts.events,
            notifier: opts.notifier,
            annotator: opts.annotator,
            recs: opts.recs,
            node_events: opts.node_events,
            autoscaler_events: opts.autoscaler_events,
            cordon: opts.cordon,
        }
    }

    /// Lists the autoscalers, discovers all candidate instances, and
    /// processes each targeted one, returning the number of instances
    /// discovered. Per-instance failures are logged and counted but do not
    /// abort the pass. Each autoscaler's status is written at the end.
    #[allow(clippy::too_many_lines)]
    pub async fn reconcile(self: &Arc<Self>) -> Result<usize, anyhow::Error> {
        let ivas = match &self.autoscalers {
            Some(api) => match api.list().await {
                Ok(l) => l,
                Err(err) => {
                    self.rec.observe_error("autoscalers");
                    anyhow::bail!("list InstanceVolumeAutoscalers: {err}");
                }
            },
            None => Vec::new(),
        };
        let resolver = Resolver::new(&ivas);
        for (name, err) in &resolver.invalid {
            warn!(autoscaler = %name, error = %err, "InstanceVolumeAutoscaler rejected, its instances are left alone");
        }

        let instances = match self.ec2.describe_target_instances().await {
            Ok(i) => i,
            Err(err) => {
                self.rec.observe_error("discover");
                let err = format!("discover instances: {err}");
                self.write_statuses(
                    &ivas,
                    &resolver,
                    &BTreeMap::new(),
                    HashMap::new(),
                    Some(&err),
                )
                .await;
                anyhow::bail!(err);
            }
        };
        info!(
            count = instances.len(),
            autoscalers = resolver.autoscalers.len(),
            "discovered candidate instances"
        );

        let plan = resolver.plan(&instances);
        for (name, others) in &plan.conflicts {
            warn!(autoscaler = %name, overlaps = ?others, "InstanceVolumeAutoscaler shares instances with another, refusing to scale any of its instances");
        }
        // Every valid autoscaler is seeded at 0 so one that targets nothing
        // this pass still reports 0 rather than vanishing.
        let mut counts: HashMap<String, usize> =
            resolver.names().into_iter().map(|n| (n, 0)).collect();
        for (_, eff) in &plan.managed {
            *counts.entry(eff.policy.clone()).or_default() += 1;
        }
        Self::log_policy_counts(&resolver, &counts, plan.unmanaged);
        self.rec.observe_policy_instances(&counts);

        let mut entries: HashMap<String, Vec<InstanceStatus>> = HashMap::new();
        for (i, names) in &plan.refused {
            let inst = &instances[*i];
            let mut obs = Observation::new(inst.root_volume_size_gib);
            obs.set(
                REASON_AMBIGUOUS,
                format!(
                    "targeted by more than one InstanceVolumeAutoscaler: {}",
                    names.join(", ")
                ),
            );
            for name in names {
                self.rec.observe_skip(SKIP_AMBIGUOUS, name);
                entries
                    .entry(name.clone())
                    .or_default()
                    .push(obs.entry(inst));
            }
        }

        let mut nodes = self.protective_cordon_nodes().await;
        // Reconcile instances concurrently with a bounded worker pool. Each
        // instance targets an independent EBS volume, so parallelism is safe;
        // the semaphore caps in-flight SSM/EC2 calls to stay within API rate
        // limits.
        let sem = Arc::new(Semaphore::new(self.cfg.reconcile_concurrency.max(1)));
        let mut set = JoinSet::new();
        for (i, eff) in plan.managed {
            let inst = instances[i].clone();
            let permit = sem.clone().acquire_owned().await.expect("semaphore open");
            let this = self.clone();
            let node = nodes.remove(&inst.id);
            set.spawn(async move {
                let _permit = permit;
                let mut obs = Observation::new(inst.root_volume_size_gib);
                if let Err(err) = this.reconcile_instance(&inst, &eff, node, &mut obs).await {
                    error!(instance = %inst.id, name = %inst.name, policy = %eff.policy, error = %err, "instance reconcile failed");
                    if obs.reason.is_empty() {
                        obs.reason = REASON_FAILED_RESCALE;
                    }
                    obs.message = format!("{err:#}");
                }
                (eff.policy.clone(), obs.entry(&inst))
            });
        }
        while let Some(res) = set.join_next().await {
            if let Ok((name, entry)) = res {
                entries.entry(name).or_default().push(entry);
            }
        }

        self.write_statuses(&ivas, &resolver, &plan.conflicts, entries, None)
            .await;
        Ok(instances.len())
    }

    /// Writes every autoscaler's status for this pass and publishes an
    /// `AmbiguousSelector` Event on each overlapping one. `discovery_error`
    /// marks a pass that could not list instances: each autoscaler keeps its
    /// previous instances and reports `FailedGetScale`, as an HPA does when it
    /// cannot read its target.
    async fn write_statuses(
        &self,
        ivas: &[InstanceVolumeAutoscaler],
        resolver: &Resolver,
        conflicts: &BTreeMap<String, BTreeSet<String>>,
        mut entries: HashMap<String, Vec<InstanceStatus>>,
        discovery_error: Option<&str>,
    ) {
        let Some(api) = &self.autoscalers else {
            return;
        };
        let now = Utc::now();
        for iva in ivas {
            let name = iva.metadata.name.clone().unwrap_or_default();
            if discovery_error.is_some()
                && let Some(prev) = &iva.status
            {
                entries.insert(name.clone(), prev.instances.clone());
            }
            let inactive = if let Some((_, err)) = resolver.invalid.iter().find(|(n, _)| *n == name)
            {
                Some(Inactive {
                    reason: REASON_INVALID_SPEC,
                    message: err.clone(),
                })
            } else if resolver.get(&name).is_some_and(|c| c.effective.paused) {
                Some(Inactive {
                    reason: REASON_SCALING_DISABLED,
                    message: "behavior.scaleUp.selectPolicy is Disabled".into(),
                })
            } else if let Some(err) = discovery_error {
                Some(Inactive {
                    reason: REASON_FAILED_GET_SCALE,
                    message: err.to_string(),
                })
            } else if let Some(others) = conflicts.get(&name) {
                let message = format!(
                    "shares instances with InstanceVolumeAutoscaler {}",
                    others.iter().cloned().collect::<Vec<_>>().join(", ")
                );
                if let Some(events) = &self.autoscaler_events {
                    events.event(
                        Target::autoscaler(&name, iva.metadata.uid.as_deref().unwrap_or_default()),
                        TYPE_WARNING,
                        REASON_AMBIGUOUS,
                        message.clone(),
                    );
                }
                Some(Inactive {
                    reason: REASON_AMBIGUOUS,
                    message,
                })
            } else {
                None
            };
            let mut status = status::build(
                iva.status.as_ref(),
                iva.metadata.generation,
                entries.remove(&name).unwrap_or_default(),
                inactive,
                now,
            );
            status.refresh_summary(&iva.spec);
            if let Err(err) = api.patch_status(&name, &status).await {
                warn!(autoscaler = %name, error = %err, "failed to write InstanceVolumeAutoscaler status");
            }
        }
    }

    /// Logs how many discovered instances each autoscaler targets, one line
    /// per autoscaler in name order, then the untargeted remainder.
    fn log_policy_counts(resolver: &Resolver, counts: &HashMap<String, usize>, unmanaged: usize) {
        for name in resolver.names() {
            info!(policy_name = %name, instance_count = counts.get(&name).copied().unwrap_or(0), "instances targeted by InstanceVolumeAutoscaler");
        }
        info!(
            instance_count = unmanaged,
            "instances targeted by no InstanceVolumeAutoscaler are left alone"
        );
    }

    #[allow(clippy::too_many_lines)]
    async fn reconcile_instance(
        &self,
        inst: &Instance,
        eff: &Effective,
        mut node: Option<CordonNode>,
        obs: &mut Observation,
    ) -> Result<(), anyhow::Error> {
        let (instance, name, policy) = (inst.id.as_str(), inst.name.as_str(), eff.policy.as_str());
        if inst.root_volume_id.is_empty() {
            warn!(
                instance,
                name, policy, "no root EBS volume resolved, skipping"
            );
            obs.set(REASON_NO_ROOT_VOLUME, "no root EBS volume resolved");
            return Ok(());
        }
        // Volume size is known from discovery, so it is recorded before the
        // paused and threshold gates: even instances that are never measured
        // report a size.
        self.rec.observe_volume_size(
            &inst.id,
            &inst.root_device_name,
            &inst.root_volume_id,
            &inst.name,
            inst.root_volume_size_gib,
        );

        // A paused policy takes the instance entirely out of scope: no
        // measurement, no resize.
        if eff.paused {
            info!(instance, name, policy, "scaling disabled, skipping");
            self.rec.observe_skip(SKIP_PAUSED, policy);
            obs.set(
                REASON_SCALING_DISABLED,
                "behavior.scaleUp.selectPolicy is Disabled",
            );
            return Ok(());
        }

        let usage = match self.measure(&inst.id).await {
            Ok(u) => u,
            Err(err) => {
                self.rec.observe_error("measure");
                obs.reason = REASON_FAILED_METRIC;
                anyhow::bail!("measure usage: {err}");
            }
        };
        self.rec.observe_usage(
            &inst.id,
            &inst.root_device_name,
            &inst.root_volume_id,
            &inst.name,
            f64::from(usage),
        );
        obs.usage = Some(usage);
        debug!(
            instance,
            name,
            policy,
            usage_percent = usage,
            threshold_percent = eff.usage_threshold_percent,
            "measured root usage"
        );

        if usage < eff.usage_threshold_percent {
            debug!(
                instance,
                name, policy, "usage below threshold, nothing to do"
            );
            self.rec.observe_skip(SKIP_BELOW_THRESHOLD, policy);
            obs.set(
                REASON_BELOW_TARGET,
                format!(
                    "usage {usage}% is below the {}% target",
                    eff.usage_threshold_percent
                ),
            );
            self.release_protective_cordon(inst, &mut node, usage, eff)
                .await;
            return Ok(());
        }
        self.apply_protective_cordon(inst, &mut node, usage, eff)
            .await;

        let current = inst.root_volume_size_gib;
        let target = target_size(current, eff);
        let volume = inst.root_volume_id.as_str();
        obs.desired_gib = Some(target);

        if target > eff.max_volume_size_gib {
            warn!(
                instance,
                name,
                policy,
                volume,
                current_gib = current,
                target_gib = target,
                max_gib = eff.max_volume_size_gib,
                "target exceeds max volume size, skipping"
            );
            self.rec.observe_skip(SKIP_MAX_SIZE, policy);
            obs.set(
                REASON_MAX_CAPACITY,
                format!(
                    "{target} GiB would exceed maxCapacity {} GiB",
                    eff.max_volume_size_gib
                ),
            );
            return Ok(());
        }
        let blocked = match self.cooldown_block(volume, eff.cooldown).await {
            Ok(b) => b,
            Err(err) => {
                self.rec.observe_error("cooldown");
                anyhow::bail!("check cooldown: {err}");
            }
        };
        if let Some(until) = blocked {
            info!(
                instance,
                name,
                policy,
                volume,
                current_gib = current,
                target_gib = target,
                cooldown = %go_duration(eff.cooldown),
                cooldown_until = ?until,
                "volume modified within cooldown window, skipping"
            );
            self.rec.observe_skip(SKIP_COOLDOWN, policy);
            obs.cooldown_until = until;
            let message = until.map_or_else(
                || "a modification of the volume is still in progress".to_string(),
                |t| {
                    format!(
                        "the volume was modified within the {} cooldown, next modification after {}",
                        format_cooldown(eff.cooldown),
                        t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                    )
                },
            );
            obs.set(REASON_COOLDOWN, message);
            return Ok(());
        }

        let (rec, apply_skip) = self.throughput_piggyback(volume);
        let mut piggyback = rec.is_some();
        let rec = rec.unwrap_or_default();
        let mut modification = self.new_mod_summary(volume, current, target, &rec, piggyback);
        if self.cfg.dry_run {
            if piggyback {
                info!(
                    instance,
                    name,
                    policy,
                    volume,
                    current_gib = current,
                    target_gib = target,
                    throughput_mibps = rec.throughput_mibps,
                    iops = rec.iops,
                    "dry-run: would modify volume and resize filesystem, piggybacking throughput recommendation"
                );
            } else {
                info!(
                    instance,
                    name,
                    policy,
                    volume,
                    current_gib = current,
                    target_gib = target,
                    "dry-run: would modify volume and resize filesystem"
                );
            }
            self.rec.observe_skip(SKIP_DRY_RUN, policy);
            obs.set(REASON_DRY_RUN, format!("would grow to {target} GiB"));
            return Ok(());
        }

        // A skipped piggyback is only observed once a modification really
        // proceeds: the metric answers "a slot was spent, why did no
        // throughput change ride it".
        if !apply_skip.is_empty() {
            self.rec.observe_throughput_apply_skip(apply_skip);
        }

        let start = Utc::now();
        let started = Instant::now();
        self.report_started(inst, current, target, usage);
        let mut spec = ModifySpec {
            size_gib: target,
            ..ModifySpec::default()
        };
        if piggyback {
            spec.throughput_mibps = rec.throughput_mibps;
            spec.iops = rec.iops;
            info!(
                instance,
                name,
                policy,
                volume,
                current_throughput_mibps = rec.current_mibps,
                target_throughput_mibps = rec.throughput_mibps,
                current_iops = rec.current_iops,
                target_iops = rec.iops,
                "piggybacking throughput recommendation on volume modification"
            );
        }
        if let Err(err) = self.ec2.modify_volume(volume, spec).await {
            // The size expansion is the urgent operation: the disk is filling
            // up. A piggybacked performance change must never take it down
            // with it, so the combined request falls back to the plain
            // size-only request the resizer would have sent anyway.
            if !piggyback {
                self.report_failure(
                    inst,
                    eff,
                    usage,
                    &modification,
                    "modify",
                    &format!("ModifyVolume failed: {err}"),
                    start,
                )
                .await;
                anyhow::bail!("modify volume: {err}");
            }
            piggyback = false;
            modification.fallback = true;
            self.rec.observe_throughput_apply(APPLY_RESULT_FALLBACK);
            warn!(instance, name, policy, volume, error = %err, "combined volume modification failed, retrying size-only");
            if let Err(err) = self
                .ec2
                .modify_volume(
                    volume,
                    ModifySpec {
                        size_gib: target,
                        ..ModifySpec::default()
                    },
                )
                .await
            {
                self.report_failure(
                    inst,
                    eff,
                    usage,
                    &modification,
                    "modify",
                    &format!("ModifyVolume failed: {err}"),
                    start,
                )
                .await;
                anyhow::bail!("modify volume: {err}");
            }
        }
        if piggyback {
            self.rec.observe_throughput_apply(APPLY_RESULT_APPLIED);
        }
        info!(
            instance,
            name,
            policy,
            volume,
            current_gib = current,
            target_gib = target,
            "requested volume modification"
        );

        if let Err(err) = self
            .ec2
            .wait_for_modification(volume, self.cfg.volume_modify_timeout)
            .await
        {
            self.report_failure(
                inst,
                eff,
                usage,
                &modification,
                "wait",
                &format!("volume did not reach optimizing: {err}"),
                start,
            )
            .await;
            anyhow::bail!("wait for modification: {err}");
        }
        info!(instance, name, policy, volume, elapsed = %go_duration(started.elapsed()), "volume modification optimizing");

        let res = match self
            .ssm
            .run_script(
                &inst.id,
                scripts::RESIZE_ROOT_FS,
                self.cfg.ssm_command_timeout,
            )
            .await
        {
            Ok(r) => r,
            Err(err) => {
                self.report_failure(
                    inst,
                    eff,
                    usage,
                    &modification,
                    "resize",
                    &format!("filesystem resize failed: {err}"),
                    start,
                )
                .await;
                anyhow::bail!("resize filesystem: {err}");
            }
        };
        info!(
            instance,
            name,
            policy,
            volume,
            stdout = res.stdout.trim(),
            "filesystem resize completed"
        );

        let after = match self.measure(&inst.id).await {
            Ok(after) => {
                info!(
                    instance,
                    name,
                    policy,
                    volume,
                    usage_before_percent = usage,
                    usage_after_percent = after,
                    new_size_gib = target,
                    "resize verified"
                );
                if after < eff.usage_threshold_percent {
                    self.release_protective_cordon(inst, &mut node, after, eff)
                        .await;
                }
                after
            }
            Err(err) => {
                warn!(instance, name, policy, volume, error = %err, "post-resize verification failed");
                0
            }
        };
        self.report_success(inst, eff, usage, after, &modification, start)
            .await;
        obs.capacity_gib = target;
        obs.usage = Some(after);
        obs.scaled_at = Some(start);
        obs.cooldown_until =
            Some(start + chrono::TimeDelta::from_std(eff.cooldown).unwrap_or_default());
        obs.set(
            REASON_SUCCEEDED_RESCALE,
            format!("grew from {current} GiB to {target} GiB at {usage}% usage"),
        );
        Ok(())
    }

    async fn measure(&self, instance_id: &str) -> Result<i32, anyhow::Error> {
        let res = self
            .ssm
            .run_script(
                instance_id,
                scripts::MEASURE_ROOT_FS,
                self.cfg.ssm_command_timeout,
            )
            .await?;
        parse_usage_percent(&res.stdout)
    }

    /// Whether the volume must wait before its next modification:
    /// `Some(Some(until))` for a finished modification still inside
    /// `cooldown`, `Some(None)` while one is in progress, `None` when free.
    async fn cooldown_block(
        &self,
        volume_id: &str,
        cooldown: Duration,
    ) -> Result<Option<Option<chrono::DateTime<Utc>>>, ApiError> {
        let Some(m) = self.ec2.describe_last_modification(volume_id).await? else {
            return Ok(None);
        };
        if m.state == "modifying" || m.state == "optimizing" {
            return Ok(Some(None));
        }
        Ok(m.start_time.and_then(|start| {
            let until = start + chrono::TimeDelta::from_std(cooldown).unwrap_or_default();
            (Utc::now() < until).then_some(Some(until))
        }))
    }
}

/// Returns the new volume size in GiB after one scale-up step from `current`.
/// Each policy proposes a step (`Percent` rounds up to whole GiB), and the
/// largest wins unless the autoscaler selects `Min`. The result is always at
/// least one GiB larger than `current`.
#[must_use]
pub fn target_size(current: i32, eff: &Effective) -> i32 {
    crate::autoscaler::step_up(current, &eff.steps, eff.select_min)
}

/// Extracts a 0-100 integer from the measure script output.
fn parse_usage_percent(out: &str) -> Result<i32, anyhow::Error> {
    let s = out.trim().trim_end_matches('%').trim();
    if s.is_empty() {
        anyhow::bail!("empty usage output");
    }
    let n: i32 = s
        .parse()
        .map_err(|err| anyhow::anyhow!("parse usage {out:?}: {err}"))?;
    if !(0..=100).contains(&n) {
        anyhow::bail!("usage {n} out of range");
    }
    Ok(n)
}

#[cfg(test)]
#[allow(
    clippy::too_many_lines,
    clippy::type_complexity,
    clippy::option_if_let_else,
    clippy::or_fun_call,
    clippy::default_trait_access
)]
pub(crate) mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use chrono::DateTime;

    use super::*;
    use crate::autoscaler::Step;
    use crate::config::NOTIFY_ON_ALL;
    use crate::k8s::cordon::Condition;
    use crate::k8s::events::capture::Capture;
    use crate::recstore::{ACTION_INCREASE, Entry};

    #[derive(Default)]
    pub(crate) struct FakeEc2 {
        pub instances: Mutex<Vec<Instance>>,
        pub discover_error: Option<String>,
        pub modify_errors: Mutex<Vec<Option<String>>>,
        pub modify_calls: Mutex<Vec<(String, ModifySpec)>>,
        pub last_modification: Mutex<Option<VolumeModification>>,
        pub modification_error: Option<String>,
        pub wait_error: Option<String>,
    }

    #[async_trait]
    impl Ec2Api for FakeEc2 {
        async fn describe_target_instances(&self) -> Result<Vec<Instance>, ApiError> {
            if let Some(e) = &self.discover_error {
                return Err(ApiError::new(e.clone()));
            }
            Ok(self.instances.lock().unwrap().clone())
        }
        async fn modify_volume(&self, volume_id: &str, spec: ModifySpec) -> Result<(), ApiError> {
            self.modify_calls
                .lock()
                .unwrap()
                .push((volume_id.into(), spec));
            let mut errors = self.modify_errors.lock().unwrap();
            if errors.is_empty() {
                return Ok(());
            }
            match errors.remove(0) {
                Some(e) => Err(ApiError::new(e)),
                None => Ok(()),
            }
        }
        async fn describe_last_modification(
            &self,
            _: &str,
        ) -> Result<Option<VolumeModification>, ApiError> {
            if let Some(e) = &self.modification_error {
                return Err(ApiError::new(e.clone()));
            }
            Ok(self.last_modification.lock().unwrap().clone())
        }
        async fn wait_for_modification(&self, _: &str, _: Duration) -> Result<(), ApiError> {
            match &self.wait_error {
                Some(e) => Err(ApiError::new(e.clone())),
                None => Ok(()),
            }
        }
    }

    #[derive(Default)]
    pub(crate) struct FakeSsm {
        /// Outputs served in order to measure calls; the last one repeats.
        pub measurements: Mutex<Vec<Result<String, String>>>,
        pub resize_error: Option<String>,
        pub scripts: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl SsmApi for FakeSsm {
        async fn run_script(
            &self,
            _: &str,
            script: &str,
            _: Duration,
        ) -> Result<CommandResult, ApiError> {
            self.scripts.lock().unwrap().push(script.into());
            if script == scripts::RESIZE_ROOT_FS {
                return match &self.resize_error {
                    Some(e) => Err(ApiError::new(e.clone())),
                    None => Ok(CommandResult {
                        stdout: "after:\n".into(),
                        ..CommandResult::default()
                    }),
                };
            }
            let mut m = self.measurements.lock().unwrap();
            let next = if m.len() > 1 {
                m.remove(0)
            } else {
                m.first().cloned().unwrap_or(Ok("0".into()))
            };
            next.map(|stdout| CommandResult {
                stdout,
                ..CommandResult::default()
            })
            .map_err(ApiError::new)
        }
    }

    #[derive(Default)]
    pub(crate) struct Rec {
        pub usage: Mutex<Vec<(String, f64)>>,
        pub sizes: Mutex<Vec<(String, i32)>>,
        pub resizes: Mutex<Vec<(bool, String)>>,
        pub skips: Mutex<Vec<(String, String)>>,
        pub errors: Mutex<Vec<String>>,
        pub policy_counts: Mutex<Vec<HashMap<String, usize>>>,
        pub applies: Mutex<Vec<String>>,
        pub apply_skips: Mutex<Vec<String>>,
        pub cordons: Mutex<Vec<(String, String)>>,
    }

    impl Recorder for Rec {
        fn observe_usage(&self, instance_id: &str, _: &str, _: &str, _: &str, percent: f64) {
            self.usage
                .lock()
                .unwrap()
                .push((instance_id.into(), percent));
        }
        fn observe_volume_size(&self, instance_id: &str, _: &str, _: &str, _: &str, size_gib: i32) {
            self.sizes
                .lock()
                .unwrap()
                .push((instance_id.into(), size_gib));
        }
        fn observe_resize(&self, success: bool, policy: &str) {
            self.resizes.lock().unwrap().push((success, policy.into()));
        }
        fn observe_skip(&self, reason: &str, policy: &str) {
            self.skips
                .lock()
                .unwrap()
                .push((reason.into(), policy.into()));
        }
        fn observe_error(&self, stage: &str) {
            self.errors.lock().unwrap().push(stage.into());
        }
        fn observe_policy_instances(&self, counts: &HashMap<String, usize>) {
            self.policy_counts.lock().unwrap().push(counts.clone());
        }
        fn observe_throughput_apply(&self, result: &str) {
            self.applies.lock().unwrap().push(result.into());
        }
        fn observe_throughput_apply_skip(&self, reason: &str) {
            self.apply_skips.lock().unwrap().push(reason.into());
        }
        fn observe_protective_cordon(&self, action: &str, result: &str) {
            self.cordons
                .lock()
                .unwrap()
                .push((action.into(), result.into()));
        }
    }

    #[derive(Default)]
    pub(crate) struct FakeCordon {
        pub nodes: Mutex<HashMap<String, CordonNode>>,
        pub list_error: Option<String>,
        pub fail: bool,
        pub fail_condition: bool,
        /// Every condition write as `(status, reason)`.
        pub conditions: Mutex<Vec<(bool, String)>>,
        /// Every mutating call as `(action, node)`.
        pub calls: Mutex<Vec<(String, String)>>,
    }

    impl FakeCordon {
        fn record(&self, action: &str, name: &str) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push((action.into(), name.into()));
            if self.fail {
                Err("forbidden".into())
            } else {
                Ok(())
            }
        }
    }

    #[async_trait]
    impl CordonApi for FakeCordon {
        async fn list(&self) -> Result<HashMap<String, CordonNode>, String> {
            match &self.list_error {
                Some(e) => Err(e.clone()),
                None => Ok(self.nodes.lock().unwrap().clone()),
            }
        }
        async fn cordon(&self, name: &str, _: DateTime<Utc>) -> Result<(), String> {
            self.record("cordon", name)
        }
        async fn uncordon(&self, name: &str) -> Result<(), String> {
            self.record("uncordon", name)
        }
        async fn forget(&self, name: &str) -> Result<(), String> {
            self.record("forget", name)
        }
        async fn set_condition(&self, name: &str, c: &Condition) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push((format!("condition={}", c.status), name.into()));
            self.conditions
                .lock()
                .unwrap()
                .push((c.status, c.reason.into()));
            if self.fail_condition {
                Err("forbidden".into())
            } else {
                Ok(())
            }
        }
    }

    #[derive(Default)]
    pub(crate) struct FakeNotifier {
        pub alerts: Mutex<Vec<(String, String, String, BTreeMap<String, String>)>>,
    }

    #[async_trait]
    impl AlertNotifier for FakeNotifier {
        async fn notify(
            &self,
            severity: &str,
            alertname: &str,
            _: &str,
            description: &str,
            labels: &BTreeMap<String, String>,
            _: DateTime<Utc>,
        ) {
            self.alerts.lock().unwrap().push((
                severity.into(),
                alertname.into(),
                description.into(),
                labels.clone(),
            ));
        }
    }

    #[derive(Default)]
    pub(crate) struct FakeAnnotator {
        pub annotations: Mutex<Vec<(String, Vec<String>, bool)>>,
    }

    #[async_trait]
    impl Annotator for FakeAnnotator {
        async fn annotate(
            &self,
            text: &str,
            tags: &[String],
            _: DateTime<Utc>,
            end: Option<DateTime<Utc>>,
        ) {
            self.annotations
                .lock()
                .unwrap()
                .push((text.into(), tags.to_vec(), end.is_some()));
        }
    }

    pub(crate) fn config() -> Config {
        Config {
            region: "r".into(),
            reconcile_concurrency: 2,
            alertmanager_notify_on: NOTIFY_ON_ALL.into(),
            grafana_annotate_on: "all".into(),
            ssm_command_timeout: Duration::from_secs(5),
            volume_modify_timeout: Duration::from_secs(5),
            ..Config::default()
        }
    }

    pub(crate) fn instance(id: &str, size: i32) -> Instance {
        Instance {
            id: id.into(),
            name: format!("name-{id}"),
            tags: BTreeMap::from([
                ("Name".to_string(), format!("name-{id}")),
                ("Managed".to_string(), "true".to_string()),
            ]),
            root_device_name: "/dev/xvda".into(),
            root_volume_id: format!("vol-{id}"),
            root_volume_size_gib: size,
        }
    }

    /// One autoscaler named `default`, with every optional field omitted,
    /// targeting every instance `instance` builds.
    pub(crate) fn default_iva() -> InstanceVolumeAutoscaler {
        crate::autoscaler::instance_volume::policy::tests::iva(
            "default",
            crate::autoscaler::instance_volume::policy::tests::by_tags(&[("Managed", "true")]),
        )
    }

    /// `default_iva` with scaling disabled.
    pub(crate) fn disabled_iva() -> InstanceVolumeAutoscaler {
        crate::autoscaler::instance_volume::policy::tests::with_scale_up(
            default_iva(),
            Some(crate::autoscaler::SelectPolicy::Disabled),
            &[],
        )
    }

    #[derive(Default)]
    pub(crate) struct FakeAutoscalers {
        pub ivas: Vec<InstanceVolumeAutoscaler>,
        pub list_error: Option<String>,
        pub patch_error: Option<String>,
        pub patched: Mutex<
            Vec<(
                String,
                crate::autoscaler::instance_volume::crd::InstanceVolumeAutoscalerStatus,
            )>,
        >,
    }

    #[async_trait]
    impl AutoscalerApi for FakeAutoscalers {
        async fn list(&self) -> Result<Vec<InstanceVolumeAutoscaler>, String> {
            match &self.list_error {
                Some(e) => Err(e.clone()),
                None => Ok(self.ivas.clone()),
            }
        }
        async fn patch_status(
            &self,
            name: &str,
            status: &crate::autoscaler::instance_volume::crd::InstanceVolumeAutoscalerStatus,
        ) -> Result<(), String> {
            self.patched
                .lock()
                .unwrap()
                .push((name.into(), status.clone()));
            self.patch_error.clone().map_or(Ok(()), Err)
        }
    }

    pub(crate) struct Harness {
        pub ec2: Arc<FakeEc2>,
        pub ssm: Arc<FakeSsm>,
        pub rec: Arc<Rec>,
        pub notifier: Arc<FakeNotifier>,
        pub annotator: Arc<FakeAnnotator>,
        pub pod_events: Arc<Capture>,
        pub node_events: Arc<Capture>,
        pub autoscaler_events: Arc<Capture>,
        pub recs: Arc<Store>,
        pub cordon: Arc<FakeCordon>,
        pub autoscalers: Arc<FakeAutoscalers>,
        pub resizer: Arc<Resizer>,
    }

    pub(crate) fn harness(
        cfg: Config,
        ec2: FakeEc2,
        ssm: FakeSsm,
        ivas: Option<Vec<InstanceVolumeAutoscaler>>,
    ) -> Harness {
        harness_with_cordon(cfg, ec2, ssm, ivas, FakeCordon::default())
    }

    pub(crate) fn harness_with_cordon(
        cfg: Config,
        ec2: FakeEc2,
        ssm: FakeSsm,
        ivas: Option<Vec<InstanceVolumeAutoscaler>>,
        cordon: FakeCordon,
    ) -> Harness {
        let autoscalers = Arc::new(FakeAutoscalers {
            ivas: ivas.unwrap_or_else(|| vec![default_iva()]),
            ..FakeAutoscalers::default()
        });
        let cordon = Arc::new(cordon);
        let ec2 = Arc::new(ec2);
        let ssm = Arc::new(ssm);
        let rec = Arc::new(Rec::default());
        let notifier = Arc::new(FakeNotifier::default());
        let annotator = Arc::new(FakeAnnotator::default());
        let pod_events = Arc::new(Capture::default());
        let node_events = Arc::new(Capture::default());
        let autoscaler_events = Arc::new(Capture::default());
        let recs = Arc::new(Store::new());
        let resizer = Arc::new(Resizer::new(
            Arc::new(cfg),
            ec2.clone(),
            ssm.clone(),
            rec.clone(),
            Options {
                autoscalers: Some(autoscalers.clone()),
                events: Some(PodEvents {
                    emitter: Emitter::new(pod_events.clone()),
                    target: Target::pod("kube-system", "pod-0", "uid"),
                }),
                notifier: Some(notifier.clone()),
                annotator: Some(annotator.clone()),
                recs: Some(recs.clone()),
                node_events: Some(Emitter::new(node_events.clone())),
                autoscaler_events: Some(Emitter::new(autoscaler_events.clone())),
                cordon: Some(cordon.clone()),
            },
        ));
        Harness {
            ec2,
            ssm,
            rec,
            notifier,
            annotator,
            pod_events,
            node_events,
            autoscaler_events,
            recs,
            cordon,
            autoscalers,
            resizer,
        }
    }

    impl Harness {
        pub(crate) async fn flush(&self) {
            if let Some(e) = &self.resizer.events {
                e.emitter.shutdown().await;
            }
            if let Some(e) = &self.resizer.node_events {
                e.shutdown().await;
            }
            if let Some(e) = &self.resizer.autoscaler_events {
                e.shutdown().await;
            }
        }
    }

    fn ssm_with(usages: &[&str]) -> FakeSsm {
        FakeSsm {
            measurements: Mutex::new(usages.iter().map(|u| Ok((*u).to_string())).collect()),
            ..FakeSsm::default()
        }
    }

    #[tokio::test]
    async fn reconcile_triggers_a_resize_and_reports_everywhere() {
        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        let h = harness(config(), ec2, ssm_with(&["85%\n", "60"]), None);
        assert_eq!(h.resizer.reconcile().await.unwrap(), 1);
        let calls = h.ec2.modify_calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![(
                "vol-i-1".to_string(),
                ModifySpec {
                    size_gib: 110,
                    ..ModifySpec::default()
                }
            )]
        );
        assert_eq!(
            h.rec.resizes.lock().unwrap().clone(),
            vec![(true, "default".to_string())]
        );
        assert_eq!(h.rec.usage.lock().unwrap()[0], ("i-1".to_string(), 85.0));
        assert_eq!(
            h.rec.sizes.lock().unwrap().clone(),
            vec![("i-1".to_string(), 100), ("i-1".to_string(), 110)],
            "size reflected immediately"
        );
        assert_eq!(
            h.rec.apply_skips.lock().unwrap().len(),
            0,
            "feature off: no skip observed"
        );
        assert_eq!(
            h.rec.policy_counts.lock().unwrap()[0],
            HashMap::from([("default".to_string(), 1usize)])
        );
        assert_eq!(
            h.ssm.scripts.lock().unwrap().len(),
            3,
            "measure, resize, verify"
        );
        let alerts = h.notifier.alerts.lock().unwrap().clone();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].0, "info");
        assert_eq!(alerts[0].1, "EBSRootVolumeAutoresizeCompleted");
        assert_eq!(
            alerts[0].2,
            "Instance i-1 (name-i-1) device /dev/xvda was autoresized to 110 GiB. Root filesystem usage changed from 85% to 60%."
        );
        assert_eq!(alerts[0].3["instance_id"], "i-1");
        assert_eq!(alerts[0].3["volume_id"], "vol-i-1");
        let ann = h.annotator.annotations.lock().unwrap().clone();
        assert_eq!(ann.len(), 1);
        assert!(ann[0].2, "success is a region annotation");
        assert!(ann[0].1.contains(&"result:success".to_string()));
        h.flush().await;
        let events = h.pod_events.summary();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].4, "ResizeStarted");
        assert_eq!(
            events[0].5,
            "Resizing root filesystem on device /dev/xvda of instance name-i-1 (i-1) by growing volume vol-i-1 from 100 GiB to 110 GiB (usage 85%)"
        );
        assert_eq!(events[1].4, "ResizeCompleted");
        assert!(events[1].5.starts_with("Resized root filesystem on device /dev/xvda of instance name-i-1 (i-1) to 110 GiB in "), "{}", events[1].5);
        assert!(
            events[1].5.ends_with("Disk usage changed from 85% to 60%"),
            "{}",
            events[1].5
        );
        assert!(
            h.node_events.events.lock().unwrap().is_empty(),
            "no node ref, no node event"
        );
    }

    #[tokio::test]
    async fn reconcile_skip_reasons() {
        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![
            Instance {
                root_volume_id: String::new(),
                ..instance("i-novol", 10)
            },
            instance("i-below", 100),
            instance("i-max", 995),
        ];
        let h = harness(config(), ec2, ssm_with(&["50", "90"]), None);
        assert_eq!(h.resizer.reconcile().await.unwrap(), 3);
        let skips = h.rec.skips.lock().unwrap().clone();
        assert!(skips.contains(&(SKIP_BELOW_THRESHOLD.to_string(), "default".to_string())));
        assert!(skips.contains(&(SKIP_MAX_SIZE.to_string(), "default".to_string())));
        assert!(h.ec2.modify_calls.lock().unwrap().is_empty());
        assert_eq!(
            h.rec.sizes.lock().unwrap().len(),
            2,
            "instance without a volume records nothing"
        );

        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        *ec2.last_modification.lock().unwrap() = Some(VolumeModification {
            state: "modifying".into(),
            ..VolumeModification::default()
        });
        let h = harness(config(), ec2, ssm_with(&["90"]), None);
        h.resizer.reconcile().await.unwrap();
        assert_eq!(h.rec.skips.lock().unwrap()[0].0, SKIP_COOLDOWN);

        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        *ec2.last_modification.lock().unwrap() = Some(VolumeModification {
            state: "completed".into(),
            start_time: Some(Utc::now() - chrono::TimeDelta::hours(1)),
            target_gib: 100,
        });
        let h = harness(config(), ec2, ssm_with(&["90"]), None);
        h.resizer.reconcile().await.unwrap();
        assert_eq!(
            h.rec.skips.lock().unwrap()[0].0,
            SKIP_COOLDOWN,
            "recent completed modification"
        );

        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        *ec2.last_modification.lock().unwrap() = Some(VolumeModification {
            state: "completed".into(),
            start_time: Some(Utc::now() - chrono::TimeDelta::hours(7)),
            target_gib: 100,
        });
        let h = harness(config(), ec2, ssm_with(&["90"]), None);
        h.resizer.reconcile().await.unwrap();
        assert_eq!(
            h.ec2.modify_calls.lock().unwrap().len(),
            1,
            "old modification is past cooldown"
        );

        let mut cfg = config();
        cfg.dry_run = true;
        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        let h = harness(cfg, ec2, ssm_with(&["90"]), None);
        h.resizer.reconcile().await.unwrap();
        assert_eq!(h.rec.skips.lock().unwrap()[0].0, SKIP_DRY_RUN);
        assert!(h.ec2.modify_calls.lock().unwrap().is_empty());
        assert!(h.notifier.alerts.lock().unwrap().is_empty());
        assert!(h.annotator.annotations.lock().unwrap().is_empty());
        h.flush().await;
        assert!(h.pod_events.events.lock().unwrap().is_empty());

        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        let h = harness(config(), ec2, ssm_with(&["90"]), Some(vec![disabled_iva()]));
        h.resizer.reconcile().await.unwrap();
        assert_eq!(h.rec.skips.lock().unwrap()[0].0, SKIP_PAUSED);
        assert!(
            h.ssm.scripts.lock().unwrap().is_empty(),
            "paused instances are never measured"
        );
    }

    #[tokio::test]
    async fn cooldown_follows_the_autoscaler() {
        let long = crate::autoscaler::instance_volume::policy::tests::with_scale_up(
            default_iva(),
            None,
            &[],
        );
        let mut long = long;
        long.spec
            .behavior
            .as_mut()
            .unwrap()
            .scale_up
            .as_mut()
            .unwrap()
            .cooldown_duration = Some("12h".into());
        let modified = Utc::now() - chrono::TimeDelta::hours(8);
        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        *ec2.last_modification.lock().unwrap() = Some(VolumeModification {
            state: "completed".into(),
            start_time: Some(modified),
            target_gib: 100,
        });
        let h = harness(config(), ec2, ssm_with(&["90"]), Some(vec![long]));
        h.resizer.reconcile().await.unwrap();
        assert!(
            h.ec2.modify_calls.lock().unwrap().is_empty(),
            "8h is inside 12h"
        );
        let patched = h.autoscalers.patched.lock().unwrap().clone();
        let status = &patched[0].1;
        assert_eq!(status.cooldown_duration, "12h");
        let entry = &status.instances[0];
        assert_eq!(entry.reason, REASON_COOLDOWN);
        let until = entry
            .cooldown_until
            .as_ref()
            .and_then(crate::k8s::from_k8s_time)
            .unwrap();
        assert!(
            (until - (modified + chrono::TimeDelta::hours(12)))
                .num_milliseconds()
                .abs()
                < 1,
            "{until}"
        );
        assert!(
            entry
                .message
                .contains("12h cooldown, next modification after"),
            "{}",
            entry.message
        );

        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        *ec2.last_modification.lock().unwrap() = Some(VolumeModification {
            state: "optimizing".into(),
            start_time: Some(Utc::now()),
            target_gib: 100,
        });
        let h = harness(config(), ec2, ssm_with(&["90"]), None);
        h.resizer.reconcile().await.unwrap();
        let patched = h.autoscalers.patched.lock().unwrap().clone();
        let entry = &patched[0].1.instances[0];
        assert_eq!(entry.reason, REASON_COOLDOWN);
        assert!(
            entry.cooldown_until.is_none(),
            "unknown while still modifying"
        );
        assert!(
            entry.message.contains("still in progress"),
            "{}",
            entry.message
        );

        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        let h = harness(config(), ec2, ssm_with(&["90", "50"]), None);
        h.resizer.reconcile().await.unwrap();
        let patched = h.autoscalers.patched.lock().unwrap().clone();
        let entry = &patched[0].1.instances[0];
        assert_eq!(entry.reason, REASON_SUCCEEDED_RESCALE);
        let scaled = entry
            .last_scale_time
            .as_ref()
            .and_then(crate::k8s::from_k8s_time)
            .unwrap();
        let until = entry
            .cooldown_until
            .as_ref()
            .and_then(crate::k8s::from_k8s_time)
            .unwrap();
        assert_eq!(
            until - scaled,
            chrono::TimeDelta::hours(6),
            "a resize starts the cooldown"
        );
    }

    #[tokio::test]
    async fn reconcile_errors() {
        let h = harness(
            config(),
            FakeEc2 {
                discover_error: Some("denied".into()),
                ..FakeEc2::default()
            },
            FakeSsm::default(),
            None,
        );
        let err = h.resizer.reconcile().await.unwrap_err().to_string();
        assert!(err.contains("discover instances: denied"), "{err}");
        assert_eq!(h.rec.errors.lock().unwrap().clone(), vec!["discover"]);
        let patched = h.autoscalers.patched.lock().unwrap().clone();
        let active = crate::autoscaler::condition(
            &patched[0].1.conditions,
            crate::autoscaler::CONDITION_SCALING_ACTIVE,
        )
        .unwrap();
        assert_eq!(active.reason, REASON_FAILED_GET_SCALE);

        let mut prev = default_iva();
        prev.status = Some(status::build(
            None,
            Some(1),
            vec![Observation::new(100).entry(&instance("i-1", 100))],
            None,
            Utc::now(),
        ));
        let h = harness(
            config(),
            FakeEc2 {
                discover_error: Some("denied".into()),
                ..FakeEc2::default()
            },
            FakeSsm::default(),
            Some(vec![prev]),
        );
        assert!(h.resizer.reconcile().await.is_err());
        let patched = h.autoscalers.patched.lock().unwrap().clone();
        assert_eq!(
            patched[0].1.instances.len(),
            1,
            "previous instances are kept"
        );

        let h = harness(
            config(),
            FakeEc2 {
                discover_error: Some("denied".into()),
                ..FakeEc2::default()
            },
            FakeSsm::default(),
            Some(vec![disabled_iva()]),
        );
        assert!(h.resizer.reconcile().await.is_err());
        let patched = h.autoscalers.patched.lock().unwrap().clone();
        let active = crate::autoscaler::condition(
            &patched[0].1.conditions,
            crate::autoscaler::CONDITION_SCALING_ACTIVE,
        )
        .unwrap();
        assert_eq!(
            active.reason, REASON_SCALING_DISABLED,
            "the operator's intent wins"
        );

        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        let ssm = FakeSsm {
            measurements: Mutex::new(vec![Err("ssm down".into())]),
            ..FakeSsm::default()
        };
        let h = harness(config(), ec2, ssm, None);
        h.resizer.reconcile().await.unwrap();
        assert_eq!(h.rec.errors.lock().unwrap().clone(), vec!["measure"]);

        let ec2 = FakeEc2 {
            modification_error: Some("boom".into()),
            ..FakeEc2::default()
        };
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        let h = harness(config(), ec2, ssm_with(&["90"]), None);
        h.resizer.reconcile().await.unwrap();
        assert_eq!(h.rec.errors.lock().unwrap().clone(), vec!["cooldown"]);

        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        *ec2.modify_errors.lock().unwrap() = vec![Some("RateExceeded".into())];
        let h = harness(config(), ec2, ssm_with(&["90"]), None);
        h.resizer.reconcile().await.unwrap();
        assert_eq!(h.rec.errors.lock().unwrap().clone(), vec!["modify"]);
        assert_eq!(
            h.rec.resizes.lock().unwrap().clone(),
            vec![(false, "default".to_string())]
        );
        let alerts = h.notifier.alerts.lock().unwrap().clone();
        assert_eq!(alerts[0].0, "warning");
        assert_eq!(alerts[0].1, "EBSRootVolumeAutoresizeFailed");
        assert!(alerts[0].2.starts_with("Instance i-1 (name-i-1) device /dev/xvda failed to autoresize at 90% root filesystem usage. Cause: ModifyVolume failed: "), "{}", alerts[0].2);
        let ann = h.annotator.annotations.lock().unwrap().clone();
        assert!(!ann[0].2, "failure is a point annotation");
        assert!(ann[0].1.contains(&"result:failure".to_string()));
        h.flush().await;
        let events = h.pod_events.summary();
        assert_eq!(events[1].3, "Warning");
        assert_eq!(events[1].4, "ResizeFailed");

        let ec2 = FakeEc2 {
            wait_error: Some("timeout".into()),
            ..FakeEc2::default()
        };
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        let h = harness(config(), ec2, ssm_with(&["90"]), None);
        h.resizer.reconcile().await.unwrap();
        assert_eq!(h.rec.errors.lock().unwrap().clone(), vec!["wait"]);

        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        let ssm = FakeSsm {
            resize_error: Some("growpart failed".into()),
            ..ssm_with(&["90"])
        };
        let h = harness(config(), ec2, ssm, None);
        h.resizer.reconcile().await.unwrap();
        assert_eq!(h.rec.errors.lock().unwrap().clone(), vec!["resize"]);

        // A failed verification still reports success.
        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        let ssm = FakeSsm {
            measurements: Mutex::new(vec![Ok("90".into()), Err("gone".into())]),
            ..FakeSsm::default()
        };
        let h = harness(config(), ec2, ssm, None);
        h.resizer.reconcile().await.unwrap();
        assert_eq!(
            h.rec.resizes.lock().unwrap().clone(),
            vec![(true, "default".to_string())]
        );
        assert!(
            h.notifier.alerts.lock().unwrap()[0]
                .2
                .contains("from 90% to 0%")
        );
    }

    #[tokio::test]
    async fn reconcile_resolves_autoscalers_and_writes_status() {
        use crate::autoscaler::instance_volume::policy::tests::{
            by_id, by_tags, iva, with_scale_up,
        };
        use crate::autoscaler::{CONDITION_SCALING_ACTIVE, ScalingPolicyType, condition};

        let mut big = with_scale_up(
            iva("big", by_id("i-0000000b")),
            None,
            &[(ScalingPolicyType::GiB, 50)],
        );
        big.spec.alert_enabled = false;
        let ivas = vec![
            big,
            iva("plain", by_id("i-0000000c")),
            iva("idle", by_id("i-0000000f")),
            iva("left", by_id("i-0000000d")),
            iva("right", by_tags(&[("Name", "name-i-0000000d")])),
            iva("broken", by_id("web")),
        ];
        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![
            instance("i-0000000b", 100),
            instance("i-0000000c", 100),
            instance("i-0000000d", 100),
            instance("i-0000000e", 100),
        ];
        let h = harness(config(), ec2, ssm_with(&["90", "90"]), Some(ivas));
        assert_eq!(h.resizer.reconcile().await.unwrap(), 4);
        let counts = h.rec.policy_counts.lock().unwrap()[0].clone();
        assert_eq!(
            counts,
            HashMap::from([
                ("big".to_string(), 1usize),
                ("plain".to_string(), 1),
                ("idle".to_string(), 0),
                ("left".to_string(), 0),
                ("right".to_string(), 0),
            ])
        );
        let calls = h.ec2.modify_calls.lock().unwrap().clone();
        assert_eq!(
            calls.len(),
            2,
            "ambiguous and untargeted instances are left alone"
        );
        let big = calls.iter().find(|c| c.0 == "vol-i-0000000b").unwrap();
        assert_eq!(big.1.size_gib, 150, "GiB policy");
        let plain = calls.iter().find(|c| c.0 == "vol-i-0000000c").unwrap();
        assert_eq!(plain.1.size_gib, 110);
        let alerts = h.notifier.alerts.lock().unwrap().clone();
        assert_eq!(alerts.len(), 1, "muted autoscaler sends no alert");
        assert_eq!(alerts[0].3["instance_id"], "i-0000000c");
        let skips = h.rec.skips.lock().unwrap().clone();
        assert!(skips.contains(&(SKIP_AMBIGUOUS.to_string(), "left".to_string())));
        assert!(skips.contains(&(SKIP_AMBIGUOUS.to_string(), "right".to_string())));

        let patched: HashMap<
            String,
            crate::autoscaler::instance_volume::crd::InstanceVolumeAutoscalerStatus,
        > = h
            .autoscalers
            .patched
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect();
        assert_eq!(patched.len(), 6, "every autoscaler gets a status");
        let reason = |name: &str| {
            condition(&patched[name].conditions, CONDITION_SCALING_ACTIVE)
                .unwrap()
                .reason
                .clone()
        };
        assert_eq!(reason("big"), status::REASON_VALID_METRIC);
        assert_eq!(reason("idle"), status::REASON_NO_INSTANCES);
        assert_eq!(reason("left"), REASON_AMBIGUOUS);
        assert_eq!(reason("broken"), REASON_INVALID_SPEC);
        let entry = &patched["big"].instances[0];
        assert_eq!(entry.reason, REASON_SUCCEEDED_RESCALE);
        assert_eq!(entry.current_capacity.as_ref().unwrap().0, "150Gi");
        assert!(patched["big"].last_scale_time.is_some());
        assert_eq!(patched["left"].instances[0].reason, REASON_AMBIGUOUS);

        h.flush().await;
        let events = h.autoscaler_events.events.lock().unwrap().clone();
        assert!(
            events.iter().any(|(_, e)| e.involved_object.kind.as_deref()
                == Some("InstanceVolumeAutoscaler")
                && e.involved_object.name.as_deref() == Some("big")
                && e.reason.as_deref() == Some("SuccessfulRescale")),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|(_, e)| e.reason.as_deref() == Some(REASON_AMBIGUOUS)
                    && e.involved_object.api_version.as_deref()
                        == Some(crate::autoscaler::API_VERSION))
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn reconcile_driven_by_yaml_manifests() {
        use crate::autoscaler::instance_volume::crd::InstanceVolumeAutoscalerStatus;
        use crate::autoscaler::{
            CONDITION_ABLE_TO_SCALE, CONDITION_SCALING_ACTIVE, CONDITION_SCALING_LIMITED, condition,
        };

        let raw = indoc::indoc! {"
            apiVersion: autoscaling.volume-autoscaler.sh/v1alpha1
            kind: InstanceVolumeAutoscaler
            metadata:
              name: pinned
              uid: uid-pinned
              generation: 3
            spec:
              scaleTargetRef:
                kind: Instance
                name: i-0000000a
              maxCapacity: 1Ti
              behavior:
                scaleUp:
                  policies:
                    - type: Percent
                      value: 20
                    - type: GiB
                      value: 50
            ---
            apiVersion: autoscaling.volume-autoscaler.sh/v1alpha1
            kind: InstanceVolumeAutoscaler
            metadata:
              name: db
            spec:
              scaleTargetRef:
                kind: TagSelector
                tags:
                  Role: db
              maxCapacity: 1100Gi
              behavior:
                scaleUp:
                  selectPolicy: Min
                  policies:
                    - type: Percent
                      value: 20
                    - type: GiB
                      value: 50
            ---
            apiVersion: autoscaling.volume-autoscaler.sh/v1alpha1
            kind: InstanceVolumeAutoscaler
            metadata:
              name: web
            spec:
              scaleTargetRef:
                kind: TagSelector
                tags:
                  Role: web
              maxCapacity: 950Gi
            ---
            apiVersion: autoscaling.volume-autoscaler.sh/v1alpha1
            kind: InstanceVolumeAutoscaler
            metadata:
              name: calm
            spec:
              scaleTargetRef:
                kind: TagSelector
                tags:
                  Role: calm
              maxCapacity: 100Gi
              metrics:
                - type: Resource
                  resource:
                    name: storage
                    target:
                      type: Utilization
                      averageUtilization: 95
            ---
            apiVersion: autoscaling.volume-autoscaler.sh/v1alpha1
            kind: InstanceVolumeAutoscaler
            metadata:
              name: frozen
            spec:
              scaleTargetRef:
                kind: TagSelector
                tags:
                  Role: batch
              maxCapacity: 100Gi
              behavior:
                scaleUp:
                  selectPolicy: Disabled
            ---
            apiVersion: autoscaling.volume-autoscaler.sh/v1alpha1
            kind: InstanceVolumeAutoscaler
            metadata:
              name: ghost
            spec:
              scaleTargetRef:
                kind: Instance
                name: i-00000099
              maxCapacity: 100Gi
        "};
        let ivas: Vec<InstanceVolumeAutoscaler> = serde_yaml::Deserializer::from_str(raw)
            .map(|d| serde::Deserialize::deserialize(d).unwrap())
            .collect();
        let tagged = |id: &str, size: i32, role: &str| {
            let mut i = instance(id, size);
            i.tags.insert("Role".into(), role.into());
            i
        };
        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![
            instance("i-0000000a", 100),
            tagged("i-0000000b", 1000, "db"),
            tagged("i-0000000c", 900, "web"),
            tagged("i-0000000d", 50, "calm"),
            tagged("i-0000000e", 50, "batch"),
            instance("i-0000000f", 50),
        ];
        let h = harness(config(), ec2, ssm_with(&["90"]), Some(ivas));
        assert_eq!(h.resizer.reconcile().await.unwrap(), 6);

        let mut calls: Vec<(String, i32)> = h
            .ec2
            .modify_calls
            .lock()
            .unwrap()
            .iter()
            .map(|(v, m)| (v.clone(), m.size_gib))
            .collect();
        calls.sort();
        assert_eq!(
            calls,
            vec![
                ("vol-i-0000000a".to_string(), 150),
                ("vol-i-0000000b".to_string(), 1050),
            ],
            "Max(20%,50GiB) of 100 is 50, Min(20%,50GiB) of 1000 is 50"
        );

        let patched: HashMap<String, InstanceVolumeAutoscalerStatus> = h
            .autoscalers
            .patched
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect();
        let cond = |name: &str, t: &str| {
            let c = condition(&patched[name].conditions, t).unwrap();
            (c.status.clone(), c.reason.clone())
        };
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());

        let pinned = serde_yaml::to_string(&patched["pinned"]).unwrap();
        for want in [
            "observedGeneration: 3",
            "currentVolumes: 1",
            "instanceId: i-0000000a",
            "currentCapacity: 150Gi",
            "desiredCapacity: 150Gi",
            "currentUtilization: 90",
            "reason: SucceededRescale",
            "lastScaleTime:",
            "target: i-0000000a",
            "utilization: 90%/80%",
            "reason: Ready",
        ] {
            assert!(pinned.contains(want), "missing {want:?} in\n{pinned}");
        }
        assert_eq!(
            cond("pinned", CONDITION_ABLE_TO_SCALE),
            pair("True", "SucceededRescale")
        );
        assert_eq!(
            cond("db", CONDITION_SCALING_ACTIVE),
            pair("True", "ValidMetricFound")
        );
        assert_eq!(
            patched["db"].instances[0]
                .current_capacity
                .as_ref()
                .unwrap()
                .0,
            "1050Gi"
        );

        let web = &patched["web"].instances[0];
        assert_eq!(web.reason, "MaxCapacityReached");
        assert_eq!(web.desired_capacity.as_ref().unwrap().0, "990Gi");
        assert_eq!(web.current_capacity.as_ref().unwrap().0, "900Gi");
        assert_eq!(
            cond("web", CONDITION_SCALING_LIMITED),
            pair("True", "MaxCapacityReached")
        );

        assert_eq!(patched["calm"].instances[0].reason, "BelowTarget");
        assert_eq!(
            cond("calm", CONDITION_ABLE_TO_SCALE),
            pair("True", "ReadyForNewScale")
        );
        assert_eq!(
            cond("calm", CONDITION_SCALING_LIMITED),
            pair("False", "DesiredWithinRange")
        );

        assert_eq!(patched["frozen"].instances[0].reason, "ScalingDisabled");
        assert!(
            patched["frozen"].instances[0].current_utilization.is_none(),
            "never measured"
        );
        assert_eq!(
            cond("frozen", CONDITION_SCALING_ACTIVE),
            pair("False", "ScalingDisabled")
        );

        assert_eq!(patched["ghost"].current_volumes, 0);
        assert_eq!(patched["ghost"].utilization, "<unknown>/80%");
        assert_eq!(patched["ghost"].reason, "NoMatchingInstances");
        assert_eq!(patched["db"].target, "Role=db");
        assert_eq!(patched["web"].reason, "MaxCapacityReached");
        assert_eq!(patched["calm"].utilization, "90%/95%");
        assert_eq!(patched["frozen"].utilization, "<unknown>/80%");
        assert_eq!(patched["frozen"].reason, "ScalingDisabled");
        assert_eq!(
            cond("ghost", CONDITION_SCALING_ACTIVE),
            pair("False", "NoMatchingInstances")
        );

        assert_eq!(patched.len(), 6);
        assert!(
            patched
                .values()
                .all(|s| s.instances.iter().all(|i| i.instance_id != "i-0000000f")),
            "an untargeted instance shows up nowhere"
        );
        assert_eq!(
            h.ssm
                .scripts
                .lock()
                .unwrap()
                .iter()
                .filter(|s| *s == scripts::RESIZE_ROOT_FS)
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn reconcile_fails_without_autoscalers_and_tolerates_status_errors() {
        let rec = Arc::new(Rec::default());
        let r = Arc::new(Resizer::new(
            Arc::new(config()),
            Arc::new(FakeEc2::default()),
            Arc::new(FakeSsm::default()),
            rec.clone(),
            Options {
                autoscalers: Some(Arc::new(FakeAutoscalers {
                    list_error: Some("forbidden".into()),
                    ..FakeAutoscalers::default()
                })),
                ..Options::default()
            },
        ));
        let err = r.reconcile().await.unwrap_err().to_string();
        assert!(
            err.contains("list InstanceVolumeAutoscalers: forbidden"),
            "{err}"
        );
        assert_eq!(rec.errors.lock().unwrap().clone(), vec!["autoscalers"]);

        let ec2 = Arc::new(FakeEc2::default());
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        let autoscalers = Arc::new(FakeAutoscalers {
            ivas: vec![default_iva()],
            patch_error: Some("conflict".into()),
            ..FakeAutoscalers::default()
        });
        let r = Arc::new(Resizer::new(
            Arc::new(config()),
            ec2,
            Arc::new(ssm_with(&["50"])),
            Arc::new(Rec::default()),
            Options {
                autoscalers: Some(autoscalers.clone()),
                ..Options::default()
            },
        ));
        assert_eq!(
            r.reconcile().await.unwrap(),
            1,
            "a status write failure is not fatal"
        );
        let patched = autoscalers.patched.lock().unwrap().clone();
        assert_eq!(patched[0].1.instances[0].reason, REASON_BELOW_TARGET);
        assert_eq!(patched[0].1.instances[0].current_utilization, Some(50));
    }

    #[tokio::test]
    async fn notify_and_annotate_policies() {
        for (notify_on, annotate_on, success, want_alert, want_ann) in [
            ("success", "success", true, true, true),
            ("success", "success", false, false, false),
            ("failure", "failure", true, false, false),
            ("failure", "failure", false, true, true),
            ("all", "all", false, true, true),
        ] {
            let mut cfg = config();
            cfg.alertmanager_notify_on = notify_on.into();
            cfg.grafana_annotate_on = annotate_on.into();
            let ec2 = FakeEc2::default();
            *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
            if !success {
                *ec2.modify_errors.lock().unwrap() = vec![Some("nope".into())];
            }
            let h = harness(cfg, ec2, ssm_with(&["90", "50"]), None);
            h.resizer.reconcile().await.unwrap();
            assert_eq!(
                h.notifier.alerts.lock().unwrap().len(),
                usize::from(want_alert),
                "{notify_on} {success}"
            );
            assert_eq!(
                h.annotator.annotations.lock().unwrap().len(),
                usize::from(want_ann),
                "{annotate_on} {success}"
            );
        }
    }

    #[tokio::test]
    async fn piggyback_applies_and_falls_back() {
        let mut cfg = config();
        cfg.throughput_recommendation.enabled = true;
        cfg.throughput_recommendation.apply_on_resize = true;
        cfg.throughput_recommendation.interval = Duration::from_mins(30);
        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        let h = harness(cfg.clone(), ec2, ssm_with(&["90", "50"]), None);
        h.recs.publish(
            "vol-i-1",
            Entry {
                node_name: "node-1".into(),
                node_uid: "nuid".into(),
                action: ACTION_INCREASE.into(),
                throughput_mibps: 250,
                iops: 4000,
                current_mibps: 125,
                current_iops: 3000,
                observed_at: Some(Utc::now()),
            },
        );
        h.resizer.reconcile().await.unwrap();
        let calls = h.ec2.modify_calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![(
                "vol-i-1".to_string(),
                ModifySpec {
                    size_gib: 110,
                    throughput_mibps: 250,
                    iops: 4000
                }
            )]
        );
        assert_eq!(
            h.rec.applies.lock().unwrap().clone(),
            vec![APPLY_RESULT_APPLIED]
        );
        let alerts = h.notifier.alerts.lock().unwrap().clone();
        assert!(alerts[0].2.ends_with("Root filesystem usage changed from 90% to 50%. Throughput was raised from 125 to 250 MiB/s (IOPS 3000 to 4000) on the piggybacked recommendation."), "{}", alerts[0].2);
        h.flush().await;
        let node = h.node_events.summary();
        assert_eq!(node.len(), 1);
        assert_eq!(node[0].0, "Node");
        assert_eq!(node[0].2, "node-1");
        assert_eq!(node[0].4, "VolumeModified");
        assert_eq!(
            node[0].5,
            "Modified EBS volume vol-i-1 in one modification slot: size 100 GiB to 110 GiB, throughput 125 to 250 MiB/s, IOPS 3000 to 4000. Root filesystem usage changed from 90% to 50%."
        );

        // The combined request is rejected: size-only retry.
        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        *ec2.modify_errors.lock().unwrap() = vec![Some("InvalidParameterCombination".into()), None];
        let h = harness(cfg.clone(), ec2, ssm_with(&["90", "50"]), None);
        h.recs.publish(
            "vol-i-1",
            Entry {
                node_name: "node-1".into(),
                action: ACTION_INCREASE.into(),
                throughput_mibps: 250,
                iops: 3000,
                current_mibps: 125,
                current_iops: 3000,
                observed_at: Some(Utc::now()),
                ..Entry::default()
            },
        );
        h.resizer.reconcile().await.unwrap();
        let calls = h.ec2.modify_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[1].1,
            ModifySpec {
                size_gib: 110,
                ..ModifySpec::default()
            }
        );
        assert_eq!(
            h.rec.applies.lock().unwrap().clone(),
            vec![APPLY_RESULT_FALLBACK]
        );
        assert_eq!(
            h.rec.resizes.lock().unwrap().clone(),
            vec![(true, "default".to_string())]
        );
        let alerts = h.notifier.alerts.lock().unwrap().clone();
        assert!(alerts[0].2.ends_with("A piggybacked throughput increase (125 to 250 MiB/s) was rejected by EC2 and was not applied; the size change proceeded alone."), "{}", alerts[0].2);
        h.flush().await;
        let node = h.node_events.summary();
        assert_eq!(
            node[0].5,
            "Modified EBS volume vol-i-1 in one modification slot: size 100 GiB to 110 GiB. Root filesystem usage changed from 90% to 50%. A piggybacked throughput increase (125 to 250 MiB/s) was rejected by EC2 and was not applied; the size change proceeded alone."
        );

        // Both requests fail: the failure names the attempted changes.
        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        *ec2.modify_errors.lock().unwrap() = vec![Some("bad".into()), Some("worse".into())];
        let h = harness(cfg, ec2, ssm_with(&["90"]), None);
        h.recs.publish(
            "vol-i-1",
            Entry {
                node_name: "node-1".into(),
                action: ACTION_INCREASE.into(),
                throughput_mibps: 250,
                current_mibps: 125,
                observed_at: Some(Utc::now()),
                ..Entry::default()
            },
        );
        h.resizer.reconcile().await.unwrap();
        assert_eq!(h.rec.errors.lock().unwrap().clone(), vec!["modify"]);
        h.flush().await;
        let node = h.node_events.summary();
        assert_eq!(node[0].4, "VolumeModifyFailed");
        assert_eq!(node[0].3, "Warning");
        assert!(node[0].5.starts_with("Failed to modify EBS volume vol-i-1 (attempted size 100 GiB to 110 GiB) at stage \"modify\": ModifyVolume failed: "), "{}", node[0].5);
    }

    #[tokio::test]
    async fn piggyback_skips() {
        let mut cfg = config();
        cfg.throughput_recommendation.enabled = true;
        cfg.throughput_recommendation.apply_on_resize = true;
        cfg.throughput_recommendation.interval = Duration::from_mins(30);
        // No recommendation at all.
        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        let h = harness(cfg.clone(), ec2, ssm_with(&["90"]), None);
        h.resizer.reconcile().await.unwrap();
        assert_eq!(
            h.rec.apply_skips.lock().unwrap().clone(),
            vec!["no_recommendation"]
        );
        assert_eq!(
            h.ec2.modify_calls.lock().unwrap()[0].1,
            ModifySpec {
                size_gib: 110,
                ..ModifySpec::default()
            }
        );

        // Stale recommendation: skipped, but the node event still fires.
        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        let h = harness(cfg.clone(), ec2, ssm_with(&["90"]), None);
        h.recs.publish(
            "vol-i-1",
            Entry {
                node_name: "node-1".into(),
                action: ACTION_INCREASE.into(),
                throughput_mibps: 250,
                current_mibps: 125,
                observed_at: Some(Utc::now() - chrono::TimeDelta::hours(3)),
                ..Entry::default()
            },
        );
        h.resizer.reconcile().await.unwrap();
        assert_eq!(h.rec.apply_skips.lock().unwrap().clone(), vec!["stale"]);
        h.flush().await;
        assert_eq!(
            h.node_events.summary()[0].5,
            "Modified EBS volume vol-i-1 in one modification slot: size 100 GiB to 110 GiB. Root filesystem usage changed from 90% to 90%."
        );

        // Not an increase, or wrong direction.
        for entry in [
            Entry {
                action: "decrease".into(),
                throughput_mibps: 125,
                current_mibps: 250,
                ..Entry::default()
            },
            Entry {
                action: ACTION_INCREASE.into(),
                throughput_mibps: 125,
                current_mibps: 250,
                ..Entry::default()
            },
        ] {
            let ec2 = FakeEc2::default();
            *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
            let h = harness(cfg.clone(), ec2, ssm_with(&["90"]), None);
            h.recs.publish(
                "vol-i-1",
                Entry {
                    node_name: "n".into(),
                    observed_at: Some(Utc::now()),
                    ..entry
                },
            );
            h.resizer.reconcile().await.unwrap();
            assert_eq!(
                h.rec.apply_skips.lock().unwrap().clone(),
                vec!["not_increase"]
            );
        }

        // Kill switch: nothing observed.
        cfg.throughput_recommendation.apply_on_resize = false;
        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        let h = harness(cfg.clone(), ec2, ssm_with(&["90"]), None);
        h.recs.publish(
            "vol-i-1",
            Entry {
                node_name: "n".into(),
                action: ACTION_INCREASE.into(),
                throughput_mibps: 250,
                current_mibps: 125,
                observed_at: Some(Utc::now()),
                ..Entry::default()
            },
        );
        h.resizer.reconcile().await.unwrap();
        assert!(h.rec.apply_skips.lock().unwrap().is_empty());
        assert!(h.rec.applies.lock().unwrap().is_empty());

        // Dry run never observes a skip.
        cfg.throughput_recommendation.apply_on_resize = true;
        cfg.dry_run = true;
        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        let h = harness(cfg, ec2, ssm_with(&["90"]), None);
        h.recs.publish(
            "vol-i-1",
            Entry {
                node_name: "n".into(),
                action: ACTION_INCREASE.into(),
                throughput_mibps: 250,
                current_mibps: 125,
                observed_at: Some(Utc::now()),
                ..Entry::default()
            },
        );
        h.resizer.reconcile().await.unwrap();
        assert!(h.rec.apply_skips.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn resizer_without_optional_sinks() {
        let ec2 = Arc::new(FakeEc2::default());
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        let rec = Arc::new(Rec::default());
        let r = Arc::new(Resizer::new(
            Arc::new(config()),
            ec2,
            Arc::new(ssm_with(&["90", "50"])),
            rec.clone(),
            Options {
                autoscalers: Some(Arc::new(FakeAutoscalers {
                    ivas: vec![default_iva()],
                    ..FakeAutoscalers::default()
                })),
                ..Options::default()
            },
        ));
        assert_eq!(r.reconcile().await.unwrap(), 1);
        assert_eq!(
            rec.resizes.lock().unwrap().clone(),
            vec![(true, "default".to_string())]
        );
    }

    #[test]
    fn target_sizes() {
        let mut eff = Effective::defaults("d", 1000);
        assert_eq!(target_size(100, &eff), 110);
        assert_eq!(target_size(1, &eff), 2, "at least one GiB");
        assert_eq!(target_size(8, &eff), 9, "8*1.1=8.8 rounds up");
        assert_eq!(target_size(5, &eff), 6, "the 1 GiB policy wins");
        eff.steps = vec![Step::Percent(33)];
        assert_eq!(target_size(100, &eff), 133);
        eff.steps = vec![Step::GiB(50), Step::Percent(20)];
        assert_eq!(target_size(100, &eff), 150, "max");
        eff.select_min = true;
        assert_eq!(target_size(100, &eff), 120, "min");
        eff.steps = vec![];
        assert_eq!(target_size(100, &eff), 101);
    }

    #[test]
    fn usage_parsing() {
        assert_eq!(parse_usage_percent("85\n").unwrap(), 85);
        assert_eq!(parse_usage_percent(" 85% ").unwrap(), 85);
        assert_eq!(parse_usage_percent("0").unwrap(), 0);
        assert_eq!(parse_usage_percent("100").unwrap(), 100);
        assert!(
            parse_usage_percent("")
                .unwrap_err()
                .to_string()
                .contains("empty usage output")
        );
        assert!(
            parse_usage_percent("abc")
                .unwrap_err()
                .to_string()
                .contains("parse usage")
        );
        assert!(
            parse_usage_percent("101")
                .unwrap_err()
                .to_string()
                .contains("out of range")
        );
        assert!(
            parse_usage_percent("-1")
                .unwrap_err()
                .to_string()
                .contains("out of range")
        );
    }
}
