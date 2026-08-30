//! Orchestrates the measure -> decide -> grow -> wait -> expand flow for the
//! root EBS volume of each target standalone EC2 instance.

pub mod piggyback;
pub mod report;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

use crate::awsx::{
    ApiError, Clients, CommandResult, Instance, ModifySpec, TagFilter, VolumeModification,
};
use crate::config::{Config, GROW_MODE_ABSOLUTE};
use crate::humanize::go_duration;
use crate::k8s::events::{Emitter, Target};
use crate::policy::{DEFAULT_POLICY_NAME, Effective, Resolver, from_config};
use crate::recstore::Store;
use crate::scripts;
pub use piggyback::{APPLY_RESULT_APPLIED, APPLY_RESULT_FALLBACK, recommendation_max_age};
pub use report::{AlertNotifier, Annotator};

/// The minimum interval between modifications of the same EBS volume. It is
/// fixed at the AWS-enforced limit (one modification per volume per 6 hours)
/// and is intentionally not configurable: a shorter value only triggers
/// `VolumeModificationRateExceeded` errors.
const MODIFICATION_COOLDOWN: Duration = Duration::from_hours(6);

/// The subset of EC2 operations the resizer depends on.
#[async_trait]
pub trait Ec2Api: Send + Sync {
    async fn describe_target_instances(
        &self,
        filters: &[TagFilter],
        exclude_eks_nodes: bool,
    ) -> Result<Vec<Instance>, ApiError>;
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
    async fn describe_target_instances(
        &self,
        filters: &[TagFilter],
        exclude_eks_nodes: bool,
    ) -> Result<Vec<Instance>, ApiError> {
        Self::describe_target_instances(self, filters, exclude_eks_nodes).await
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
}

/// Skip reasons reported via `observe_skip` when an instance is above the
/// usage threshold but no resize is attempted. They surface the silent
/// states that `resize_total` and `error_total` do not capture.
pub const SKIP_BELOW_THRESHOLD: &str = "below_threshold";
pub const SKIP_MAX_SIZE: &str = "max_size";
pub const SKIP_COOLDOWN: &str = "cooldown";
pub const SKIP_DRY_RUN: &str = "dry_run";
pub const SKIP_PAUSED: &str = "paused";

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
    /// `None` runs every instance on the global settings.
    resolver: Option<Arc<Resolver>>,
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
}

/// The optional collaborators of a resizer.
#[derive(Default)]
pub struct Options {
    pub resolver: Option<Arc<Resolver>>,
    pub events: Option<PodEvents>,
    pub notifier: Option<Arc<dyn AlertNotifier>>,
    pub annotator: Option<Arc<dyn Annotator>>,
    pub recs: Option<Arc<Store>>,
    pub node_events: Option<Emitter>,
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
            resolver: opts.resolver,
            ec2,
            ssm,
            rec,
            events: opts.events,
            notifier: opts.notifier,
            annotator: opts.annotator,
            recs: opts.recs,
            node_events: opts.node_events,
        }
    }

    /// The resize settings for one instance: the matched policy's overlay
    /// when a resolver is set, otherwise the global settings.
    fn effective(&self, inst: &Instance) -> Effective {
        self.resolver.as_ref().map_or_else(
            || from_config(&self.cfg),
            |r| r.resolve(&inst.name, &inst.tags),
        )
    }

    /// Discovers all target instances and processes each one, returning the
    /// number of instances discovered. Per-instance failures are logged and
    /// counted but do not abort the pass.
    pub async fn reconcile(self: &Arc<Self>) -> Result<usize, anyhow::Error> {
        let filters: Vec<TagFilter> = self
            .cfg
            .tag_filters
            .iter()
            .map(|f| TagFilter {
                key: f.key.clone(),
                value: f.value.clone(),
            })
            .collect();
        let instances = match self
            .ec2
            .describe_target_instances(&filters, self.cfg.exclude_eks_nodes)
            .await
        {
            Ok(i) => i,
            Err(err) => {
                self.rec.observe_error("discover");
                anyhow::bail!("discover instances: {err}");
            }
        };
        info!(count = instances.len(), "discovered target instances");

        // Resolve each instance's effective policy once, up front, and record
        // how many instances each policy identified. Seed every named policy
        // (and the default bucket) at 0 so a policy that matches nothing this
        // pass still reports 0 rather than vanishing.
        let mut counts: HashMap<String, usize> =
            HashMap::from([(DEFAULT_POLICY_NAME.to_string(), 0)]);
        if let Some(r) = &self.resolver {
            for name in r.names() {
                counts.insert(name, 0);
            }
        }
        let effs: Vec<Effective> = instances.iter().map(|i| self.effective(i)).collect();
        for eff in &effs {
            *counts.entry(eff.policy.clone()).or_default() += 1;
        }
        self.log_policy_counts(&counts);
        self.rec.observe_policy_instances(&counts);

        // Reconcile instances concurrently with a bounded worker pool. Each
        // instance targets an independent EBS volume, so parallelism is safe;
        // the semaphore caps in-flight SSM/EC2 calls to stay within API rate
        // limits.
        let sem = Arc::new(Semaphore::new(self.cfg.reconcile_concurrency.max(1)));
        let mut set = JoinSet::new();
        for (inst, eff) in instances.iter().cloned().zip(effs) {
            let permit = sem.clone().acquire_owned().await.expect("semaphore open");
            let this = self.clone();
            set.spawn(async move {
                let _permit = permit;
                if let Err(err) = this.reconcile_instance(&inst, &eff).await {
                    error!(instance = %inst.id, name = %inst.name, error = %err, "instance reconcile failed");
                }
            });
        }
        while set.join_next().await.is_some() {}
        Ok(instances.len())
    }

    /// Logs how many discovered instances each policy matched, one line per
    /// policy, named policies in configured order then the default bucket.
    fn log_policy_counts(&self, counts: &HashMap<String, usize>) {
        let Some(r) = &self.resolver else {
            return;
        };
        if r.is_empty() {
            return;
        }
        for name in r.names() {
            info!(policy_name = %name, instance_count = counts.get(&name).copied().unwrap_or(0), "instances matched by resize policy");
        }
        info!(
            policy_name = DEFAULT_POLICY_NAME,
            instance_count = counts.get(DEFAULT_POLICY_NAME).copied().unwrap_or(0),
            "instances matched by resize policy"
        );
    }

    #[allow(clippy::too_many_lines)]
    async fn reconcile_instance(
        &self,
        inst: &Instance,
        eff: &Effective,
    ) -> Result<(), anyhow::Error> {
        let (instance, name, policy) = (inst.id.as_str(), inst.name.as_str(), eff.policy.as_str());
        if inst.root_volume_id.is_empty() {
            warn!(
                instance,
                name, policy, "no root EBS volume resolved, skipping"
            );
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
            info!(instance, name, policy, "resize policy paused, skipping");
            self.rec.observe_skip(SKIP_PAUSED, policy);
            return Ok(());
        }

        let usage = match self.measure(&inst.id).await {
            Ok(u) => u,
            Err(err) => {
                self.rec.observe_error("measure");
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
            return Ok(());
        }

        let current = inst.root_volume_size_gib;
        let target = target_size(current, eff);
        let volume = inst.root_volume_id.as_str();

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
            return Ok(());
        }
        let skip = match self.within_cooldown(volume).await {
            Ok(s) => s,
            Err(err) => {
                self.rec.observe_error("cooldown");
                anyhow::bail!("check cooldown: {err}");
            }
        };
        if skip {
            info!(
                instance,
                name,
                policy,
                volume,
                current_gib = current,
                target_gib = target,
                "volume modified within cooldown window, skipping"
            );
            self.rec.observe_skip(SKIP_COOLDOWN, policy);
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
                after
            }
            Err(err) => {
                warn!(instance, name, policy, volume, error = %err, "post-resize verification failed");
                0
            }
        };
        self.report_success(inst, eff, usage, after, &modification, start)
            .await;
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

    /// Reports whether the volume was modified too recently (or is currently
    /// being modified) to safely modify again.
    async fn within_cooldown(&self, volume_id: &str) -> Result<bool, ApiError> {
        let Some(m) = self.ec2.describe_last_modification(volume_id).await? else {
            return Ok(false);
        };
        if m.state == "modifying" || m.state == "optimizing" {
            return Ok(true);
        }
        if let Some(start) = m.start_time {
            let since = (Utc::now() - start).to_std().unwrap_or_default();
            if since < MODIFICATION_COOLDOWN {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Returns the new volume size in GiB after growing `current` per the
/// effective grow mode. In percent mode it grows `current` by the grow
/// percent, rounded up; in absolute mode it adds the grow amount. The result
/// is always at least one GiB larger than `current`.
#[must_use]
pub fn target_size(current: i32, eff: &Effective) -> i32 {
    let current = i64::from(current);
    let grown = if eff.grow_mode == GROW_MODE_ABSOLUTE {
        current + i64::from(eff.grow_amount_gib)
    } else {
        (current * (100 + i64::from(eff.grow_percent)) + 99) / 100
    };
    i32::try_from(grown.max(current + 1)).unwrap_or(i32::MAX)
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
    use crate::config::{GROW_MODE_PERCENT, NOTIFY_ON_ALL};
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
        async fn describe_target_instances(
            &self,
            _: &[TagFilter],
            _: bool,
        ) -> Result<Vec<Instance>, ApiError> {
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
            usage_threshold_percent: 80,
            grow_mode: GROW_MODE_PERCENT.into(),
            grow_percent: 10,
            grow_amount: "10GiB".into(),
            grow_amount_gib: 10,
            max_volume_size_gib: 1000,
            alert_enabled: true,
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
            tags: BTreeMap::from([("Name".to_string(), format!("name-{id}"))]),
            root_device_name: "/dev/xvda".into(),
            root_volume_id: format!("vol-{id}"),
            root_volume_size_gib: size,
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
        pub recs: Arc<Store>,
        pub resizer: Arc<Resizer>,
    }

    pub(crate) fn harness(
        cfg: Config,
        ec2: FakeEc2,
        ssm: FakeSsm,
        resolver: Option<Resolver>,
    ) -> Harness {
        let ec2 = Arc::new(ec2);
        let ssm = Arc::new(ssm);
        let rec = Arc::new(Rec::default());
        let notifier = Arc::new(FakeNotifier::default());
        let annotator = Arc::new(FakeAnnotator::default());
        let pod_events = Arc::new(Capture::default());
        let node_events = Arc::new(Capture::default());
        let recs = Arc::new(Store::new());
        let resizer = Arc::new(Resizer::new(
            Arc::new(cfg),
            ec2.clone(),
            ssm.clone(),
            rec.clone(),
            Options {
                resolver: resolver.map(Arc::new),
                events: Some(PodEvents {
                    emitter: Emitter::new(pod_events.clone()),
                    target: Target::pod("kube-system", "pod-0", "uid"),
                }),
                notifier: Some(notifier.clone()),
                annotator: Some(annotator.clone()),
                recs: Some(recs.clone()),
                node_events: Some(Emitter::new(node_events.clone())),
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
            recs,
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

        let mut cfg = config();
        cfg.paused = true;
        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        let h = harness(cfg, ec2, ssm_with(&["90"]), None);
        h.resizer.reconcile().await.unwrap();
        assert_eq!(h.rec.skips.lock().unwrap()[0].0, SKIP_PAUSED);
        assert!(
            h.ssm.scripts.lock().unwrap().is_empty(),
            "paused instances are never measured"
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
    async fn reconcile_resolves_policies_and_counts() {
        let mut cfg = config();
        cfg.policies = vec![
            crate::config::ResizePolicy {
                name: "big".into(),
                weight: 1,
                instance_selector: crate::config::InstanceSelector {
                    name_regex: "big".into(),
                    ..Default::default()
                },
                resize: crate::config::ResizeSpec {
                    grow_mode: Some("absolute".into()),
                    grow_amount: Some("50GiB".into()),
                    alert_enabled: Some(false),
                    ..Default::default()
                },
            },
            crate::config::ResizePolicy {
                name: "idle".into(),
                weight: 1,
                instance_selector: crate::config::InstanceSelector {
                    name_regex: "never".into(),
                    ..Default::default()
                },
                resize: Default::default(),
            },
        ];
        let resolver = Resolver::new(&cfg).unwrap();
        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-big", 100), instance("i-plain", 100)];
        let h = harness(cfg, ec2, ssm_with(&["90"]), Some(resolver));
        h.resizer.reconcile().await.unwrap();
        let counts = h.rec.policy_counts.lock().unwrap()[0].clone();
        assert_eq!(
            counts,
            HashMap::from([
                ("big".to_string(), 1usize),
                ("idle".to_string(), 0),
                ("default".to_string(), 1)
            ])
        );
        let calls = h.ec2.modify_calls.lock().unwrap().clone();
        let big = calls.iter().find(|c| c.0 == "vol-i-big").unwrap();
        assert_eq!(big.1.size_gib, 150, "absolute policy");
        let plain = calls.iter().find(|c| c.0 == "vol-i-plain").unwrap();
        assert_eq!(plain.1.size_gib, 110);
        let alerts = h.notifier.alerts.lock().unwrap().clone();
        assert_eq!(alerts.len(), 1, "muted policy sends no alert");
        assert_eq!(alerts[0].3["instance_id"], "i-plain");
        let resizes = h.rec.resizes.lock().unwrap().clone();
        assert!(resizes.contains(&(true, "big".to_string())));
        assert!(resizes.contains(&(true, "default".to_string())));
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
            Options::default(),
        ));
        assert_eq!(r.reconcile().await.unwrap(), 1);
        assert_eq!(
            rec.resizes.lock().unwrap().clone(),
            vec![(true, "default".to_string())]
        );
    }

    #[test]
    fn target_sizes() {
        let mut eff = from_config(&config());
        assert_eq!(target_size(100, &eff), 110);
        assert_eq!(target_size(1, &eff), 2, "at least one GiB");
        assert_eq!(target_size(8, &eff), 9, "8*1.1=8.8 rounds up");
        eff.grow_percent = 33;
        assert_eq!(target_size(100, &eff), 133);
        eff.grow_mode = GROW_MODE_ABSOLUTE.into();
        eff.grow_amount_gib = 50;
        assert_eq!(target_size(100, &eff), 150);
        eff.grow_amount_gib = 0;
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
