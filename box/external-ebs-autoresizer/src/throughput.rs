//! Recommends an EBS throughput (and the IOPS it requires) for each
//! Kubernetes Node in the cluster the addon runs in, and publishes the
//! recommendation as annotations on the Node object.
//!
//! It only ever recommends. No EC2 mutation happens here and the module has
//! no access to one: the AWS surface it depends on is read-only by
//! construction. Whether a recommendation is ever applied is the consumer's
//! decision: an operator reading the annotation, or the resizer folding an
//! increase into a size expansion it is already making (the `applyOnResize`
//! hand-off through the recommendation store).
//!
//! The recommendation is intentionally undefined for anything but a single
//! gp3 volume per node. A node with more than one attached volume is reported
//! as `multiple_attached_volumes` and left alone.

pub mod annotations;
pub mod decide;
pub mod defaults;
pub mod observation;
pub mod probe;
pub mod query;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use tracing::{debug, error, info, warn};

use crate::awsx::{ApiError, Clients, EbsCaps, Volume};
use crate::k8s::events::{Emitter, TYPE_NORMAL, Target};
use crate::k8s::nodes::{Node, NodeApi};
use crate::promql::{QueryError, Sample};
use crate::recstore::{Entry, Store};
pub use annotations::{AnnotationSet, build_annotations};
pub use decide::*;
pub use defaults::{ANNOTATION_PREFIX, Config, QUERY_TIMEOUT};
pub use observation::Observation;
pub use probe::Probe;
pub use query::{NODE_BATCH, Query};

/// The subset of the Prometheus-compatible query API this module depends on.
#[async_trait]
pub trait MetricsApi: Send + Sync {
    async fn query(&self, query: &str) -> Result<Vec<Sample>, QueryError>;
}

#[async_trait]
impl MetricsApi for crate::promql::Client {
    async fn query(&self, query: &str) -> Result<Vec<Sample>, QueryError> {
        Self::query(self, query).await
    }
}

/// The read-only EC2 surface this module depends on. It deliberately
/// excludes every mutating operation.
#[async_trait]
pub trait Ec2Api: Send + Sync {
    async fn describe_attached_volumes(
        &self,
        instance_ids: &[String],
    ) -> Result<HashMap<String, Vec<Volume>>, ApiError>;
    async fn describe_instance_type_ebs_caps(
        &self,
        instance_types: &[String],
    ) -> Result<HashMap<String, EbsCaps>, ApiError>;
}

#[async_trait]
impl Ec2Api for Clients {
    async fn describe_attached_volumes(
        &self,
        instance_ids: &[String],
    ) -> Result<HashMap<String, Vec<Volume>>, ApiError> {
        Self::describe_attached_volumes(self, instance_ids).await
    }
    async fn describe_instance_type_ebs_caps(
        &self,
        instance_types: &[String],
    ) -> Result<HashMap<String, EbsCaps>, ApiError> {
        Self::describe_instance_type_ebs_caps(self, instance_types).await
    }
}

/// Receives metrics observations. `observability::Metrics` implements it.
pub trait Recorder: Send + Sync {
    fn reset_node_throughput(&self);
    fn observe_node_throughput(
        &self,
        node: &str,
        instance_id: &str,
        volume_id: &str,
        current: f64,
        peak: f64,
        recommended: f64,
    );
    fn observe_recommendation(&self, action: &str, reason: &str);
    fn observe_error(&self, stage: &str);
}

impl Recorder for crate::observability::Metrics {
    fn reset_node_throughput(&self) {
        Self::reset_node_throughput(self);
    }
    fn observe_node_throughput(
        &self,
        node: &str,
        instance_id: &str,
        volume_id: &str,
        current: f64,
        peak: f64,
        recommended: f64,
    ) {
        Self::observe_node_throughput(
            self,
            node,
            instance_id,
            volume_id,
            current,
            peak,
            recommended,
        );
    }
    fn observe_recommendation(&self, action: &str, reason: &str) {
        Self::observe_recommendation(self, action, reason);
    }
    fn observe_error(&self, stage: &str) {
        Self::observe_error(self, stage);
    }
}

/// The Kubernetes Event reason published against a Node as its evaluation
/// begins. Repeating it every pass does not create a new Event object: the
/// emitter aggregates it.
const REASON_MEASUREMENT_STARTED: &str = "ThroughputMeasurementStarted";

/// Outcomes of one node's annotation attempt.
pub const OUTCOME_WRITTEN: &str = "written";
pub const OUTCOME_UNCHANGED: &str = "unchanged";
pub const OUTCOME_DRY_RUN: &str = "dry_run";
pub const OUTCOME_NOT_APPLICABLE: &str = "not_applicable";

/// Evaluates every Node in the cluster once per pass.
pub struct Recommender {
    cfg: Config,
    query: Query,
    settings: Settings,
    nodes: Arc<dyn NodeApi>,
    prom: Arc<dyn MetricsApi>,
    ec2: Arc<dyn Ec2Api>,
    rec: Arc<dyn Recorder>,
    events: Option<Emitter>,
    /// The in-process hand-off of decisions to the resizer. `None` disables
    /// it.
    sink: Option<Arc<Store>>,
    /// Injectable so tests control the observed-at timestamp and the
    /// staleness refresh.
    now: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

impl Recommender {
    /// Constructs a recommender, deriving the query and the decision tunables
    /// from the operator-facing config and the fixed policy constants.
    #[must_use]
    pub fn new(
        cfg: Config,
        nodes: Arc<dyn NodeApi>,
        prom: Arc<dyn MetricsApi>,
        ec2: Arc<dyn Ec2Api>,
        rec: Arc<dyn Recorder>,
        events: Option<Emitter>,
        sink: Option<Arc<Store>>,
    ) -> Self {
        Self {
            query: cfg.query(),
            settings: cfg.settings(),
            cfg,
            nodes,
            prom,
            ec2,
            rec,
            events,
            sink,
            now: Box::new(Utc::now),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_clock(
        mut self,
        now: impl Fn() -> DateTime<Utc> + Send + Sync + 'static,
    ) -> Self {
        self.now = Box::new(now);
        self
    }

    /// The Event emitter, for the wiring code to flush on shutdown.
    #[must_use]
    pub const fn events(&self) -> Option<&Emitter> {
        self.events.as_ref()
    }

    /// Evaluates every in-scope Node and publishes its recommendation,
    /// returning the number of Nodes considered. It gathers the whole
    /// cluster's data with four calls (two queries, two EC2 describes) rather
    /// than per node. Per-node annotation failures are logged and counted but
    /// never abort the pass.
    pub async fn reconcile(&self) -> Result<usize, anyhow::Error> {
        // Every Node is evaluated. Nodes that cannot carry a recommendation
        // report why rather than being filtered out up front.
        let node_list = match self.nodes.list("").await {
            Ok(list) => list,
            Err(err) => {
                self.rec.observe_error("node_list");
                anyhow::bail!("list nodes: {err}");
            }
        };
        if node_list.is_empty() {
            info!("no nodes found to evaluate");
            // With no nodes there is no volume a recommendation could still
            // apply to, so the hand-off store is emptied.
            if let Some(sink) = &self.sink {
                sink.retain(&HashSet::new());
            }
            return Ok(0);
        }

        // Nodes too young to hold enough history are not queried at all: a
        // node created an hour ago can only ever come back as
        // insufficient_samples, so reading a multi-day window for it is pure
        // waste. Under Karpenter this is most of the saving available.
        let (names, too_young) = self.split_by_age(&node_list);
        if !too_young.is_empty() {
            info!(
                skipped = too_young.len(),
                queried = names.len(),
                minimum_age = %crate::humanize::go_duration(self.cfg.min_node_age()),
                window = %self.cfg.lookback,
                "skipping nodes younger than the observation window"
            );
        }

        // The queries are scoped to this cluster's node names, so a metrics
        // backend shared by several clusters only ever reads the series that
        // belong here.
        let peaks = match self.query_per_batch(&names, |b| self.query.peak(b)).await {
            Ok(p) => p,
            Err(err) => {
                self.rec.observe_error("query_peak");
                anyhow::bail!("query peak throughput: {err}");
            }
        };
        let samples = match self
            .query_per_batch(&names, |b| self.query.sample_count(b))
            .await
        {
            Ok(s) => s,
            Err(err) => {
                self.rec.observe_error("query_samples");
                anyhow::bail!("query sample count: {err}");
            }
        };

        let (volumes, caps) = self.describe(&node_list).await?;

        self.rec.reset_node_throughput();
        let young: HashSet<&String> = too_young.iter().collect();
        // Every volume evaluated this pass, so the hand-off store can be
        // swept down to exactly the volumes that still exist. Applied only
        // after the loop completes: an aborted pass has not seen every
        // volume, and sweeping on partial knowledge would drop live entries.
        let mut seen = HashSet::with_capacity(node_list.len());
        for n in &node_list {
            self.event_measurement_started(n);
            let obs = observe(
                n,
                &volumes,
                &caps,
                &peaks,
                &samples,
                young.contains(&n.name),
            );
            let d = decide_or_block(&obs, &self.settings);
            if !obs.volume.id.is_empty() {
                seen.insert(obs.volume.id.clone());
            }
            self.hand_off(&obs, &d);
            self.report(&obs, &d);
            match self.publish(&obs, &d).await {
                Ok((outcome, annotations)) => {
                    log_annotation_outcome(&obs, &d, outcome, annotations.as_ref());
                }
                Err(err) => {
                    self.rec.observe_error("annotate");
                    error!(node = %n.name, volume = %obs.volume.id, recommendation = %d.action, reason = %d.reason,
                        outcome = "failed", error = %err, "failed to annotate node with EBS throughput recommendation");
                }
            }
        }
        if let Some(sink) = &self.sink {
            sink.retain(&seen);
        }
        Ok(node_list.len())
    }

    /// Forwards one node's decision to the in-process sink. A decision the
    /// resizer could act on is published; a volume that can no longer be
    /// decided has its entry deleted, so no earlier recommendation outlives
    /// the conditions that produced it. The hand-off ignores dry run on
    /// purpose: writing process-local memory is not a mutation, and the
    /// consumer sits behind the same global dry-run gate.
    fn hand_off(&self, obs: &Observation, d: &Decision) {
        let Some(sink) = &self.sink else {
            return;
        };
        if obs.volume.id.is_empty() {
            return;
        }
        if d.action == ACTION_UNKNOWN {
            sink.delete(&obs.volume.id);
            return;
        }
        sink.publish(
            &obs.volume.id,
            Entry {
                node_name: obs.node.name.clone(),
                node_uid: obs.node.uid.clone(),
                action: d.action.clone(),
                throughput_mibps: d.recommended_throughput_mibps,
                iops: d.recommended_iops,
                current_mibps: obs.volume.throughput_mibps,
                current_iops: obs.volume.iops,
                observed_at: Some((self.now)()),
            },
        );
    }

    /// Publishes a Node Event as that Node's evaluation begins, so the
    /// measurement is visible in `kubectl describe node`.
    fn event_measurement_started(&self, n: &Node) {
        if let Some(events) = &self.events {
            events.event(
                Target::node(&n.name, &n.uid),
                TYPE_NORMAL,
                REASON_MEASUREMENT_STARTED,
                format!(
                    "Measuring EBS throughput over {} to recommend a gp3 throughput; no volume is modified",
                    self.cfg.window()
                ),
            );
        }
    }

    /// Fetches the volume and instance-type data for the node set.
    async fn describe(
        &self,
        node_list: &[Node],
    ) -> Result<(HashMap<String, Vec<Volume>>, HashMap<String, EbsCaps>), anyhow::Error> {
        let instance_ids: Vec<String> = node_list
            .iter()
            .filter(|n| !n.instance_id.is_empty())
            .map(|n| n.instance_id.clone())
            .collect();
        let instance_types: Vec<String> = node_list
            .iter()
            .filter(|n| !n.instance_type.is_empty())
            .map(|n| n.instance_type.clone())
            .collect();
        let volumes = match self.ec2.describe_attached_volumes(&instance_ids).await {
            Ok(v) => v,
            Err(err) => {
                self.rec.observe_error("describe_volumes");
                anyhow::bail!("describe attached volumes: {err}");
            }
        };
        let caps = match self
            .ec2
            .describe_instance_type_ebs_caps(&instance_types)
            .await
        {
            Ok(c) => c,
            Err(err) => {
                self.rec.observe_error("describe_instance_types");
                anyhow::bail!("describe instance type EBS caps: {err}");
            }
        };
        Ok((volumes, caps))
    }

    /// Records metrics and logs one node's outcome. An actionable
    /// recommendation logs at info; everything else stays at debug, since a
    /// large cluster is mostly nodes with nothing to do.
    fn report(&self, obs: &Observation, d: &Decision) {
        self.rec.observe_recommendation(&d.action, &d.reason);
        if obs.blocked.is_empty() {
            self.rec.observe_node_throughput(
                &obs.node.name,
                &obs.node.instance_id,
                &obs.volume.id,
                f64::from(obs.volume.throughput_mibps),
                obs.input.peak_mibps,
                f64::from(d.recommended_throughput_mibps),
            );
        }
        if d.action == ACTION_INCREASE || d.action == ACTION_DECREASE {
            info!(
                node = %obs.node.name, instance = %obs.node.instance_id, volume = %obs.volume.id,
                recommendation = %d.action, reason = %d.reason,
                current_mibps = obs.volume.throughput_mibps, observed_peak_mibps = obs.input.peak_mibps,
                recommended_mibps = d.recommended_throughput_mibps, current_iops = obs.volume.iops,
                recommended_iops = d.recommended_iops, samples = obs.input.samples, capped = d.capped,
                "EBS throughput recommendation"
            );
        } else {
            debug!(
                node = %obs.node.name, instance = %obs.node.instance_id, volume = %obs.volume.id,
                recommendation = %d.action, reason = %d.reason,
                current_mibps = obs.volume.throughput_mibps, observed_peak_mibps = obs.input.peak_mibps,
                samples = obs.input.samples, capped = d.capped,
                "no EBS throughput change recommended"
            );
        }
    }

    /// Writes the node's annotations and reports which outcome happened. A
    /// node that is not an EC2 instance is never annotated: writing a
    /// `not_an_ec2_node` annotation onto every Fargate or virtual node would
    /// be noise.
    async fn publish(
        &self,
        obs: &Observation,
        d: &Decision,
    ) -> Result<(&'static str, Option<AnnotationSet>), String> {
        if obs.blocked == REASON_NOT_EC2_NODE {
            return Ok((OUTCOME_NOT_APPLICABLE, None));
        }
        let now = (self.now)();
        let mut desired = build_annotations(obs, d, &self.cfg.window());
        if !desired.needs_write(&obs.node.annotations, now) {
            return Ok((OUTCOME_UNCHANGED, None));
        }
        desired.set.insert(
            annotations::key(annotations::KEY_OBSERVED_AT),
            now.to_rfc3339_opts(SecondsFormat::Secs, true),
        );
        if self.cfg.dry_run {
            return Ok((OUTCOME_DRY_RUN, Some(desired)));
        }
        self.nodes
            .annotate(&obs.node.name, &desired.set, &desired.remove)
            .await?;
        Ok((OUTCOME_WRITTEN, Some(desired)))
    }

    /// Separates the Nodes worth querying from those too young to hold
    /// enough history. A Node with no creation timestamp is queried: an
    /// unknown age is not evidence of youth.
    fn split_by_age(&self, node_list: &[Node]) -> (Vec<String>, Vec<String>) {
        let min_age = chrono::TimeDelta::from_std(self.cfg.min_node_age()).unwrap_or_default();
        let now = (self.now)();
        let mut query = Vec::new();
        let mut too_young = Vec::new();
        for n in node_list {
            match n.created_at {
                Some(created) if now - created < min_age => too_young.push(n.name.clone()),
                _ => query.push(n.name.clone()),
            }
        }
        (query, too_young)
    }

    /// Evaluates `build` once per batch of node names and merges the
    /// results. Batching bounds the expression size on a large cluster
    /// without changing the total work.
    async fn query_per_batch(
        &self,
        names: &[String],
        build: impl Fn(&[String]) -> String,
    ) -> Result<HashMap<String, f64>, QueryError> {
        let mut out = HashMap::with_capacity(names.len());
        for batch in names.chunks(NODE_BATCH) {
            out.extend(self.query_by_node(&build(batch)).await?);
        }
        Ok(out)
    }

    /// Runs one query and keys the result by the node name carried in the
    /// configured node label. Series missing that label are dropped with a
    /// warning: silently ignoring them would understate the peak for
    /// whichever node they belong to.
    async fn query_by_node(&self, query: &str) -> Result<HashMap<String, f64>, QueryError> {
        debug!(query, "querying metrics backend");
        let result = self.prom.query(query).await?;
        let mut out = HashMap::with_capacity(result.len());
        let mut unlabelled = 0usize;
        for s in result {
            match s
                .labels
                .get(&self.query.node_label)
                .filter(|n| !n.is_empty())
            {
                Some(name) => {
                    out.insert(name.clone(), s.value);
                }
                None => unlabelled += 1,
            }
        }
        if unlabelled > 0 {
            warn!(
                metric_node_name_label = %self.query.node_label,
                series = unlabelled,
                hint = "set throughputRecommendation.metricNodeNameLabel to the label carrying the Kubernetes node name",
                "dropped query result series with no node label"
            );
        }
        Ok(out)
    }
}

/// Assembles one node's decision input from the gathered data, or records
/// why the node cannot be evaluated. `too_young` marks a node the queries
/// deliberately skipped.
fn observe(
    n: &Node,
    volumes: &HashMap<String, Vec<Volume>>,
    caps: &HashMap<String, EbsCaps>,
    peaks: &HashMap<String, f64>,
    samples: &HashMap<String, f64>,
    too_young: bool,
) -> Observation {
    let mut obs = Observation {
        node: n.clone(),
        ..Observation::default()
    };
    if n.instance_id.is_empty() {
        obs.blocked = REASON_NOT_EC2_NODE.into();
        return obs;
    }
    match volumes.get(&n.instance_id).map(Vec::as_slice) {
        None | Some([]) => {
            obs.blocked = REASON_NO_VOLUME.into();
            return obs;
        }
        Some([one]) => obs.volume = one.clone(),
        Some(_) => {
            // The volume is known but which one carries the measured IO is
            // not, so the current provisioning is deliberately not reported.
            obs.blocked = REASON_MULTIPLE_VOLUMES.into();
            return obs;
        }
    }
    // Checked after the volume is resolved so the current provisioning is
    // still reported, and before the metrics lookup so a node that was never
    // queried does not read as a missing scrape.
    if too_young {
        obs.blocked = REASON_NODE_TOO_YOUNG.into();
        return obs;
    }
    let Some(&peak) = peaks.get(&n.name) else {
        obs.blocked = REASON_NO_METRICS.into();
        return obs;
    };
    obs.has_metrics = true;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let sample_count = samples.get(&n.name).copied().unwrap_or(0.0).max(0.0) as usize;
    obs.input = Input {
        volume_type: obs.volume.kind.clone(),
        current_throughput_mibps: obs.volume.throughput_mibps,
        current_iops: obs.volume.iops,
        peak_mibps: peak,
        samples: sample_count,
        ..Input::default()
    };
    if let Some(c) = caps.get(&n.instance_type) {
        obs.input.instance_max_mibps = mbps_to_mibps(c.maximum_mbps);
        obs.input.instance_baseline_mibps = mbps_to_mibps(c.baseline_mbps);
    }
    obs
}

/// Returns the blocked reason as an unknown decision, or runs the decision
/// when the node is evaluable.
fn decide_or_block(obs: &Observation, s: &Settings) -> Decision {
    if obs.blocked.is_empty() {
        decide(&obs.input, s)
    } else {
        Decision {
            action: ACTION_UNKNOWN.into(),
            reason: obs.blocked.clone(),
            ..Decision::default()
        }
    }
}

/// Logs what happened to one node's annotations. A write is logged at info
/// because it is a cluster mutation, and it carries the values so the log
/// alone is enough to reconstruct what landed on the Node.
fn log_annotation_outcome(
    obs: &Observation,
    d: &Decision,
    outcome: &str,
    ann: Option<&AnnotationSet>,
) {
    let set = ann.map(|a| format!("{:?}", a.set)).unwrap_or_default();
    let removed = ann.map(|a| format!("{:?}", a.remove)).unwrap_or_default();
    match outcome {
        OUTCOME_WRITTEN => {
            info!(node = %obs.node.name, volume = %obs.volume.id, recommendation = %d.action, reason = %d.reason,
            outcome, annotations = %set, removed = %removed, "annotated node with EBS throughput recommendation");
        }
        OUTCOME_DRY_RUN => {
            info!(node = %obs.node.name, volume = %obs.volume.id, recommendation = %d.action, reason = %d.reason,
            outcome, annotations = %set, removed = %removed, "dry-run: would annotate node with EBS throughput recommendation");
        }
        OUTCOME_UNCHANGED => {
            debug!(node = %obs.node.name, volume = %obs.volume.id, recommendation = %d.action, reason = %d.reason,
            outcome, "node annotations already current, no patch issued");
        }
        _ => {
            debug!(node = %obs.node.name, volume = %obs.volume.id, recommendation = %d.action, reason = %d.reason,
            outcome, "node is out of scope for a throughput recommendation");
        }
    }
}

#[cfg(test)]
#[allow(clippy::too_many_lines, clippy::type_complexity)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use crate::k8s::events::capture::Capture;

    #[derive(Default)]
    pub(crate) struct FakeNodes {
        pub nodes: Mutex<Vec<Node>>,
        pub fail_list: bool,
        pub fail_annotate: bool,
        pub patches: Mutex<Vec<(String, BTreeMap<String, String>, Vec<String>)>>,
        pub selectors: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl NodeApi for FakeNodes {
        async fn list(&self, label_selector: &str) -> Result<Vec<Node>, String> {
            self.selectors.lock().unwrap().push(label_selector.into());
            if self.fail_list {
                return Err("forbidden".into());
            }
            Ok(self.nodes.lock().unwrap().clone())
        }
        async fn annotate(
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
                .push((name.into(), set.clone(), remove.to_vec()));
            Ok(())
        }
    }

    #[derive(Default)]
    pub(crate) struct FakeProm {
        /// Keyed by a substring of the query.
        pub responses: Mutex<Vec<(&'static str, Result<Vec<Sample>, String>)>>,
        pub queries: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl MetricsApi for FakeProm {
        async fn query(&self, query: &str) -> Result<Vec<Sample>, QueryError> {
            self.queries.lock().unwrap().push(query.into());
            let responses = self.responses.lock().unwrap();
            for (needle, resp) in responses.iter() {
                if query.contains(needle) {
                    return resp.clone().map_err(|m| QueryError {
                        status: 500,
                        message: m,
                    });
                }
            }
            Ok(vec![])
        }
    }

    #[derive(Default)]
    pub(crate) struct FakeEc2 {
        pub volumes: HashMap<String, Vec<Volume>>,
        pub caps: HashMap<String, EbsCaps>,
        pub fail_volumes: bool,
        pub fail_caps: bool,
    }

    #[async_trait]
    impl Ec2Api for FakeEc2 {
        async fn describe_attached_volumes(
            &self,
            instance_ids: &[String],
        ) -> Result<HashMap<String, Vec<Volume>>, ApiError> {
            if self.fail_volumes {
                return Err(ApiError::new("ec2 down"));
            }
            Ok(self
                .volumes
                .iter()
                .filter(|(k, _)| instance_ids.contains(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect())
        }
        async fn describe_instance_type_ebs_caps(
            &self,
            _: &[String],
        ) -> Result<HashMap<String, EbsCaps>, ApiError> {
            if self.fail_caps {
                return Err(ApiError::new("types down"));
            }
            Ok(self.caps.clone())
        }
    }

    #[derive(Default)]
    pub(crate) struct Rec {
        pub recommendations: Mutex<Vec<(String, String)>>,
        pub throughput: Mutex<Vec<(String, f64, f64, f64)>>,
        pub errors: Mutex<Vec<String>>,
        pub resets: Mutex<usize>,
    }

    impl Recorder for Rec {
        fn reset_node_throughput(&self) {
            *self.resets.lock().unwrap() += 1;
        }
        fn observe_node_throughput(
            &self,
            node: &str,
            _: &str,
            _: &str,
            current: f64,
            peak: f64,
            recommended: f64,
        ) {
            self.throughput
                .lock()
                .unwrap()
                .push((node.into(), current, peak, recommended));
        }
        fn observe_recommendation(&self, action: &str, reason: &str) {
            self.recommendations
                .lock()
                .unwrap()
                .push((action.into(), reason.into()));
        }
        fn observe_error(&self, stage: &str) {
            self.errors.lock().unwrap().push(stage.into());
        }
    }

    pub(crate) fn sample(node: &str, value: f64) -> Sample {
        Sample {
            labels: BTreeMap::from([("node".to_string(), node.to_string())]),
            value,
        }
    }

    pub(crate) fn node(name: &str, instance: &str, age_days: i64, now: DateTime<Utc>) -> Node {
        Node {
            name: name.into(),
            uid: format!("uid-{name}"),
            instance_id: instance.into(),
            instance_type: "m5.large".into(),
            zone: "ap-northeast-2a".into(),
            created_at: Some(now - chrono::TimeDelta::days(age_days)),
            annotations: BTreeMap::new(),
        }
    }

    pub(crate) fn gp3(id: &str, instance: &str, throughput: i32, iops: i32) -> Volume {
        Volume {
            id: id.into(),
            kind: "gp3".into(),
            device: "/dev/xvda".into(),
            instance_id: instance.into(),
            size_gib: 100,
            throughput_mibps: throughput,
            iops,
        }
    }

    pub(crate) fn config(dry_run: bool) -> Config {
        Config {
            metric_node_name_label: "node".into(),
            lookback: "7d".into(),
            lookback_duration: Duration::from_hours(168),
            dry_run,
        }
    }

    struct Harness {
        nodes: Arc<FakeNodes>,
        prom: Arc<FakeProm>,
        rec: Arc<Rec>,
        sink: Arc<Store>,
        events: Arc<Capture>,
        recommender: Recommender,
        now: DateTime<Utc>,
    }

    fn harness(
        nodes: Vec<Node>,
        volumes: HashMap<String, Vec<Volume>>,
        responses: Vec<(&'static str, Result<Vec<Sample>, String>)>,
        dry_run: bool,
    ) -> Harness {
        let now = Utc::now();
        let fake_nodes = Arc::new(FakeNodes {
            nodes: Mutex::new(nodes),
            ..FakeNodes::default()
        });
        let prom = Arc::new(FakeProm {
            responses: Mutex::new(responses),
            ..FakeProm::default()
        });
        let ec2 = Arc::new(FakeEc2 {
            volumes,
            caps: HashMap::from([(
                "m5.large".to_string(),
                EbsCaps {
                    baseline_mbps: 650.0,
                    maximum_mbps: 650.0,
                },
            )]),
            ..FakeEc2::default()
        });
        let rec = Arc::new(Rec::default());
        let sink = Arc::new(Store::with_clock(move || now));
        let events = Arc::new(Capture::default());
        let recommender = Recommender::new(
            config(dry_run),
            fake_nodes.clone(),
            prom.clone(),
            ec2,
            rec.clone(),
            Some(Emitter::new(events.clone())),
            Some(sink.clone()),
        )
        .with_clock(move || now);
        Harness {
            nodes: fake_nodes,
            prom,
            rec,
            sink,
            events,
            recommender,
            now,
        }
    }

    #[tokio::test]
    async fn reconcile_annotates_an_increase_and_hands_off() {
        let now = Utc::now();
        let h = harness(
            vec![node("n1", "i-1", 30, now)],
            HashMap::from([("i-1".to_string(), vec![gp3("vol-1", "i-1", 125, 3000)])]),
            vec![
                ("quantile_over_time", Ok(vec![sample("n1", 200.0)])),
                ("count_over_time", Ok(vec![sample("n1", 9000.0)])),
            ],
            false,
        );
        let n = h.recommender.reconcile().await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(*h.rec.resets.lock().unwrap(), 1);
        let patches = h.nodes.patches.lock().unwrap().clone();
        assert_eq!(patches.len(), 1);
        let (name, set, remove) = &patches[0];
        assert_eq!(name, "n1");
        assert_eq!(
            set[&annotations::key(annotations::KEY_RECOMMENDATION)],
            "increase"
        );
        assert_eq!(
            set[&annotations::key(annotations::KEY_RECOMMEND_MIBPS)],
            "375"
        );
        assert_eq!(set[&annotations::key(annotations::KEY_WINDOW)], "7d/p99");
        assert!(set.contains_key(&annotations::key(annotations::KEY_OBSERVED_AT)));
        assert!(remove.is_empty());
        assert_eq!(
            h.rec.recommendations.lock().unwrap()[0],
            ("increase".to_string(), REASON_ABOVE_PROVISIONED.to_string())
        );
        assert_eq!(
            h.rec.throughput.lock().unwrap()[0],
            ("n1".to_string(), 125.0, 200.0, 375.0)
        );
        let entry = h.sink.lookup("vol-1", Duration::from_mins(1)).unwrap();
        assert_eq!(entry.action, ACTION_INCREASE);
        assert_eq!(entry.throughput_mibps, 375);
        assert_eq!(entry.node_name, "n1");
        assert_eq!(entry.current_mibps, 125);
        // Both queries are scoped to the cluster's node names.
        let queries = h.prom.queries.lock().unwrap().clone();
        assert_eq!(queries.len(), 2);
        assert!(
            queries.iter().all(|q| q.contains("node=~\"n1\"")),
            "{queries:?}"
        );
        h.recommender.events().unwrap().shutdown().await;
        let events = h.events.summary();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "Node");
        assert_eq!(events[0].4, REASON_MEASUREMENT_STARTED);
        assert!(events[0].5.contains("over 7d/p99"));
    }

    #[tokio::test]
    async fn reconcile_blocked_reasons_and_sink_deletion() {
        let now = Utc::now();
        let mut stale = node("multi", "i-multi", 30, now);
        stale.annotations.insert(
            annotations::key(annotations::KEY_RECOMMEND_MIBPS),
            "250".into(),
        );
        let h = harness(
            vec![
                node("fargate", "", 30, now),
                node("novol", "i-novol", 30, now),
                stale,
                node("young", "i-young", 1, now),
                node("nometrics", "i-nometrics", 30, now),
                node("gp2", "i-gp2", 30, now),
            ],
            HashMap::from([
                (
                    "i-multi".to_string(),
                    vec![
                        gp3("vol-a", "i-multi", 125, 3000),
                        gp3("vol-b", "i-multi", 125, 3000),
                    ],
                ),
                (
                    "i-young".to_string(),
                    vec![gp3("vol-young", "i-young", 125, 3000)],
                ),
                (
                    "i-nometrics".to_string(),
                    vec![gp3("vol-nm", "i-nometrics", 125, 3000)],
                ),
                (
                    "i-gp2".to_string(),
                    vec![Volume {
                        kind: "gp2".into(),
                        ..gp3("vol-gp2", "i-gp2", 0, 0)
                    }],
                ),
            ]),
            vec![
                ("quantile_over_time", Ok(vec![sample("gp2", 10.0)])),
                ("count_over_time", Ok(vec![sample("gp2", 9000.0)])),
            ],
            false,
        );
        h.sink.publish(
            "vol-young",
            Entry {
                node_name: "young".into(),
                action: ACTION_INCREASE.into(),
                observed_at: Some(now),
                ..Entry::default()
            },
        );
        h.sink.publish(
            "vol-gone",
            Entry {
                node_name: "gone".into(),
                action: ACTION_INCREASE.into(),
                observed_at: Some(now),
                ..Entry::default()
            },
        );
        h.recommender.reconcile().await.unwrap();
        let recs = h.rec.recommendations.lock().unwrap().clone();
        let reasons: Vec<&str> = recs.iter().map(|(_, r)| r.as_str()).collect();
        assert_eq!(
            reasons,
            vec![
                REASON_NOT_EC2_NODE,
                REASON_NO_VOLUME,
                REASON_MULTIPLE_VOLUMES,
                REASON_NODE_TOO_YOUNG,
                REASON_NO_METRICS,
                REASON_UNSUPPORTED_VOLUME_TYPE
            ]
        );
        assert!(recs.iter().all(|(a, _)| a == ACTION_UNKNOWN));
        let patches = h.nodes.patches.lock().unwrap().clone();
        let names: Vec<&str> = patches.iter().map(|entry| entry.0.as_str()).collect();
        assert_eq!(
            names,
            vec!["novol", "multi", "young", "nometrics", "gp2"],
            "the non-EC2 node is never annotated"
        );
        let multi = patches.iter().find(|p| p.0 == "multi").unwrap();
        assert!(
            multi
                .2
                .contains(&annotations::key(annotations::KEY_RECOMMEND_MIBPS)),
            "stale number removed"
        );
        assert!(
            !multi
                .1
                .contains_key(&annotations::key(annotations::KEY_VOLUME_ID))
        );
        assert!(
            h.sink.node_ref("vol-young").is_none(),
            "undecidable volume is deleted from the sink"
        );
        assert!(
            h.sink.node_ref("vol-gone").is_none(),
            "unseen volume is swept"
        );
        // Only the old, EC2-backed nodes are queried.
        let q = &h.prom.queries.lock().unwrap()[0];
        assert!(!q.contains("young"), "{q}");
        assert!(
            q.contains("fargate"),
            "every node name goes in the matcher; the backend is the wrong place to decide EC2-ness"
        );
        assert!(
            h.rec
                .throughput
                .lock()
                .unwrap()
                .iter()
                .all(|t| t.0 == "gp2"),
            "blocked nodes publish no gauge"
        );
    }

    #[tokio::test]
    async fn reconcile_dry_run_skips_unchanged_and_refreshes_stale() {
        let now = Utc::now();
        let mut current = node("cur", "i-cur", 30, now);
        let d = Decision {
            action: ACTION_NONE.into(),
            reason: REASON_FITS.into(),
            recommended_throughput_mibps: 125,
            recommended_iops: 3000,
            capped: false,
        };
        let obs = Observation {
            node: current.clone(),
            volume: gp3("vol-cur", "i-cur", 125, 3000),
            input: Input {
                volume_type: "gp3".into(),
                current_throughput_mibps: 125,
                current_iops: 3000,
                peak_mibps: 50.0,
                samples: 9000,
                ..Input::default()
            },
            has_metrics: true,
            blocked: String::new(),
        };
        let mut ann = build_annotations(&obs, &d, "7d/p99").set;
        ann.insert(
            annotations::key(annotations::KEY_OBSERVED_AT),
            (now - chrono::TimeDelta::hours(1)).to_rfc3339(),
        );
        current.annotations = ann.clone();
        let mut stale = node("stale", "i-stale", 30, now);
        let mut stale_ann = ann.clone();
        stale_ann.insert(
            annotations::key(annotations::KEY_VOLUME_ID),
            "vol-stale".into(),
        );
        stale_ann.insert(
            annotations::key(annotations::KEY_OBSERVED_AT),
            (now - chrono::TimeDelta::days(2)).to_rfc3339(),
        );
        stale.annotations = stale_ann;
        let h = harness(
            vec![current, stale, node("dry", "i-dry", 30, now)],
            HashMap::from([
                (
                    "i-cur".to_string(),
                    vec![gp3("vol-cur", "i-cur", 125, 3000)],
                ),
                (
                    "i-stale".to_string(),
                    vec![gp3("vol-stale", "i-stale", 125, 3000)],
                ),
                (
                    "i-dry".to_string(),
                    vec![gp3("vol-dry", "i-dry", 125, 3000)],
                ),
            ]),
            vec![
                (
                    "quantile_over_time",
                    Ok(vec![
                        sample("cur", 50.0),
                        sample("stale", 50.0),
                        sample("dry", 900.0),
                    ]),
                ),
                (
                    "count_over_time",
                    Ok(vec![
                        sample("cur", 9000.0),
                        sample("stale", 9000.0),
                        sample("dry", 9000.0),
                    ]),
                ),
            ],
            true,
        );
        h.recommender.reconcile().await.unwrap();
        assert!(
            h.nodes.patches.lock().unwrap().is_empty(),
            "dry run writes nothing"
        );
        // The hand-off ignores dry run.
        assert_eq!(
            h.sink
                .lookup("vol-dry", Duration::from_secs(1))
                .unwrap()
                .action,
            ACTION_INCREASE
        );
        assert_eq!(
            h.sink
                .lookup("vol-cur", Duration::from_secs(1))
                .unwrap()
                .action,
            ACTION_NONE
        );

        // Same cluster, live: only the stale and the changed node are patched.
        let h = harness(
            h.nodes.nodes.lock().unwrap().clone(),
            HashMap::from([
                (
                    "i-cur".to_string(),
                    vec![gp3("vol-cur", "i-cur", 125, 3000)],
                ),
                (
                    "i-stale".to_string(),
                    vec![gp3("vol-stale", "i-stale", 125, 3000)],
                ),
                (
                    "i-dry".to_string(),
                    vec![gp3("vol-dry", "i-dry", 125, 3000)],
                ),
            ]),
            vec![
                (
                    "quantile_over_time",
                    Ok(vec![
                        sample("cur", 50.0),
                        sample("stale", 50.0),
                        sample("dry", 900.0),
                    ]),
                ),
                (
                    "count_over_time",
                    Ok(vec![
                        sample("cur", 9000.0),
                        sample("stale", 9000.0),
                        sample("dry", 9000.0),
                    ]),
                ),
            ],
            false,
        );
        h.recommender.reconcile().await.unwrap();
        let patched: Vec<String> = h
            .nodes
            .patches
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.0.clone())
            .collect();
        assert_eq!(patched, vec!["stale", "dry"]);
        let _ = h.now;
    }

    #[tokio::test]
    async fn reconcile_errors_and_empty_cluster() {
        let now = Utc::now();
        let h = harness(vec![], HashMap::new(), vec![], false);
        h.sink.publish(
            "vol-x",
            Entry {
                node_name: "x".into(),
                observed_at: Some(now),
                ..Entry::default()
            },
        );
        assert_eq!(h.recommender.reconcile().await.unwrap(), 0);
        assert!(
            h.sink.node_ref("vol-x").is_none(),
            "store emptied when no nodes exist"
        );
        assert!(h.prom.queries.lock().unwrap().is_empty());

        let fail_nodes = Arc::new(FakeNodes {
            fail_list: true,
            ..FakeNodes::default()
        });
        let rec = Arc::new(Rec::default());
        let r = Recommender::new(
            config(false),
            fail_nodes,
            Arc::new(FakeProm::default()),
            Arc::new(FakeEc2::default()),
            rec.clone(),
            None,
            None,
        );
        let err = r.reconcile().await.unwrap_err().to_string();
        assert!(err.contains("list nodes: forbidden"), "{err}");
        assert_eq!(rec.errors.lock().unwrap().clone(), vec!["node_list"]);

        let h = harness(
            vec![node("n1", "i-1", 30, now)],
            HashMap::new(),
            vec![("quantile_over_time", Err("timeout".into()))],
            false,
        );
        let err = h.recommender.reconcile().await.unwrap_err().to_string();
        assert!(err.contains("query peak throughput: timeout"), "{err}");
        assert_eq!(h.rec.errors.lock().unwrap().clone(), vec!["query_peak"]);

        let h = harness(
            vec![node("n1", "i-1", 30, now)],
            HashMap::new(),
            vec![("count_over_time", Err("timeout".into()))],
            false,
        );
        let err = h.recommender.reconcile().await.unwrap_err().to_string();
        assert!(err.contains("query sample count: timeout"), "{err}");

        for (fail_volumes, fail_caps, want) in [
            (true, false, "describe attached volumes"),
            (false, true, "describe instance type EBS caps"),
        ] {
            let nodes = Arc::new(FakeNodes {
                nodes: Mutex::new(vec![node("n1", "i-1", 30, now)]),
                ..FakeNodes::default()
            });
            let ec2 = Arc::new(FakeEc2 {
                fail_volumes,
                fail_caps,
                ..FakeEc2::default()
            });
            let rec = Arc::new(Rec::default());
            let r = Recommender::new(
                config(false),
                nodes,
                Arc::new(FakeProm::default()),
                ec2,
                rec.clone(),
                None,
                None,
            );
            let err = r.reconcile().await.unwrap_err().to_string();
            assert!(err.contains(want), "{err}");
            assert_eq!(rec.errors.lock().unwrap().len(), 1);
        }

        // An annotation failure does not abort the pass.
        let nodes = Arc::new(FakeNodes {
            nodes: Mutex::new(vec![node("n1", "i-1", 30, now), node("n2", "i-2", 30, now)]),
            fail_annotate: true,
            ..FakeNodes::default()
        });
        let rec = Arc::new(Rec::default());
        let r = Recommender::new(
            config(false),
            nodes,
            Arc::new(FakeProm::default()),
            Arc::new(FakeEc2::default()),
            rec.clone(),
            None,
            None,
        );
        assert_eq!(r.reconcile().await.unwrap(), 2);
        assert_eq!(
            rec.errors.lock().unwrap().clone(),
            vec!["annotate", "annotate"]
        );
    }

    #[tokio::test]
    async fn query_by_node_drops_unlabelled_series_and_nodes_without_timestamps_are_queried() {
        let now = Utc::now();
        let mut no_ts = node("nots", "i-nots", 30, now);
        no_ts.created_at = None;
        let h = harness(
            vec![no_ts, node("young", "i-young", 1, now)],
            HashMap::new(),
            vec![(
                "quantile_over_time",
                Ok(vec![
                    sample("nots", 1.0),
                    Sample {
                        labels: BTreeMap::new(),
                        value: 2.0,
                    },
                ]),
            )],
            false,
        );
        h.recommender.reconcile().await.unwrap();
        let q = h.prom.queries.lock().unwrap()[0].clone();
        assert!(q.contains("node=~\"nots\""), "{q}");
        let nodes = h.nodes.nodes.lock().unwrap().clone();
        let (query, young) = h.recommender.split_by_age(&nodes);
        assert_eq!(query, vec!["nots"]);
        assert_eq!(young, vec!["young"]);
        let peaks = h
            .recommender
            .query_by_node("quantile_over_time")
            .await
            .unwrap();
        assert_eq!(peaks.len(), 1);
    }

    #[tokio::test]
    async fn reconcile_issues_no_query_when_every_node_is_too_young() {
        let now = Utc::now();
        let h = harness(
            vec![node("young", "i-young", 0, now)],
            HashMap::new(),
            vec![],
            false,
        );
        h.recommender.reconcile().await.unwrap();
        assert!(h.prom.queries.lock().unwrap().is_empty());
    }

    #[test]
    fn observe_and_decide_or_block() {
        let now = Utc::now();
        let n = node("n1", "i-1", 30, now);
        let volumes = HashMap::from([("i-1".to_string(), vec![gp3("vol-1", "i-1", 125, 3000)])]);
        let caps = HashMap::from([(
            "m5.large".to_string(),
            EbsCaps {
                baseline_mbps: 100.0,
                maximum_mbps: 650.0,
            },
        )]);
        let peaks = HashMap::from([("n1".to_string(), 50.0)]);
        let samples = HashMap::from([("n1".to_string(), 9000.0)]);
        let obs = observe(&n, &volumes, &caps, &peaks, &samples, false);
        assert!(obs.blocked.is_empty());
        assert!(obs.has_metrics);
        assert_eq!(obs.input.samples, 9000);
        assert!((obs.input.instance_max_mibps - mbps_to_mibps(650.0)).abs() < f64::EPSILON);
        assert!((obs.input.instance_baseline_mibps - mbps_to_mibps(100.0)).abs() < f64::EPSILON);
        let d = decide_or_block(&obs, &config(false).settings());
        assert_eq!(d.action, ACTION_NONE);
        let blocked = observe(&n, &volumes, &caps, &peaks, &samples, true);
        assert_eq!(blocked.blocked, REASON_NODE_TOO_YOUNG);
        assert_eq!(blocked.volume.id, "vol-1", "provisioning is still reported");
        let d = decide_or_block(&blocked, &config(false).settings());
        assert_eq!(
            (d.action.as_str(), d.reason.as_str()),
            (ACTION_UNKNOWN, REASON_NODE_TOO_YOUNG)
        );
        // Samples absent for a node with a peak read as zero, not a crash.
        let obs = observe(&n, &volumes, &caps, &peaks, &HashMap::new(), false);
        assert_eq!(obs.input.samples, 0);
        log_annotation_outcome(&obs, &d, OUTCOME_WRITTEN, Some(&AnnotationSet::default()));
        log_annotation_outcome(&obs, &d, OUTCOME_DRY_RUN, Some(&AnnotationSet::default()));
        log_annotation_outcome(&obs, &d, OUTCOME_UNCHANGED, None);
        log_annotation_outcome(&obs, &d, OUTCOME_NOT_APPLICABLE, None);
    }

    #[tokio::test]
    async fn probe_reports_series_and_the_busiest_node() {
        let now = Utc::now();
        let h = harness(
            vec![
                node("a", "i-a", 30, now),
                node("b", "i-b", 30, now),
                node("young", "i-y", 0, now),
            ],
            HashMap::new(),
            vec![(
                "quantile_over_time",
                Ok(vec![
                    sample("a", 10.0),
                    sample("b", f64::NAN),
                    sample("b", 42.5),
                    Sample {
                        labels: BTreeMap::new(),
                        value: 99.0,
                    },
                ]),
            )],
            false,
        );
        let p = h.recommender.probe().await.unwrap();
        assert_eq!(p.cluster_nodes, 3);
        assert_eq!(p.eligible_nodes, 2);
        assert_eq!(p.series, 4);
        assert_eq!(p.nodes, 3);
        assert_eq!(p.dropped, 1);
        assert_eq!(p.max_node, "b");
        assert!((p.max_peak_mibps - 42.5).abs() < f64::EPSILON);
        assert_eq!(p.backend_series, 0);
        assert!(p.query.contains("node=~\"a|b\""), "{}", p.query);
    }

    #[tokio::test]
    async fn probe_edge_cases() {
        let now = Utc::now();
        let h = harness(vec![], HashMap::new(), vec![], false);
        let p = h.recommender.probe().await.unwrap();
        assert_eq!(p.cluster_nodes, 0);
        assert!(
            h.prom.queries.lock().unwrap().is_empty(),
            "no eligible node, no query"
        );

        let h = harness(
            vec![node("a", "i-a", 30, now)],
            HashMap::new(),
            vec![(
                "count(node_disk_read_bytes_total",
                Ok(vec![sample("", 7.0), sample("", 3.0)]),
            )],
            false,
        );
        let p = h.recommender.probe().await.unwrap();
        assert_eq!(p.series, 0);
        assert_eq!(
            p.backend_series, 7,
            "presence check runs when nothing matched"
        );

        let h = harness(
            vec![node("a", "i-a", 30, now)],
            HashMap::new(),
            vec![("count(node_disk_read_bytes_total", Err("nope".into()))],
            false,
        );
        assert_eq!(h.recommender.probe().await.unwrap().backend_series, 0);
        let h = harness(
            vec![node("a", "i-a", 30, now)],
            HashMap::new(),
            vec![(
                "count(node_disk_read_bytes_total",
                Ok(vec![sample("", f64::NAN)]),
            )],
            false,
        );
        assert_eq!(h.recommender.probe().await.unwrap().backend_series, 0);

        let h = harness(
            vec![node("a", "i-a", 30, now)],
            HashMap::new(),
            vec![("quantile_over_time", Err("timeout".into()))],
            false,
        );
        let (p, err) = h.recommender.probe().await.unwrap_err();
        assert_eq!(err, "timeout");
        assert!(!p.query.is_empty());

        let nodes = Arc::new(FakeNodes {
            fail_list: true,
            ..FakeNodes::default()
        });
        let r = Recommender::new(
            config(false),
            nodes,
            Arc::new(FakeProm::default()),
            Arc::new(FakeEc2::default()),
            Arc::new(Rec::default()),
            None,
            None,
        );
        let (_, err) = r.probe().await.unwrap_err();
        assert_eq!(err, "forbidden");

        // Only the first batch is queried.
        let many: Vec<Node> = (0..(NODE_BATCH + 5))
            .map(|i| node(&format!("n{i}"), &format!("i-{i}"), 30, now))
            .collect();
        let h = harness(many, HashMap::new(), vec![], false);
        let p = h.recommender.probe().await.unwrap();
        assert_eq!(p.eligible_nodes, NODE_BATCH + 5);
        assert!(!p.query.contains(&format!("n{}", NODE_BATCH + 1)));
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    //! Fakes shared with the wiring tests.

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use chrono::Utc;

    pub(crate) use super::tests::sample;
    use super::tests::{FakeEc2, FakeNodes, FakeProm, Rec, config, node};
    use super::*;

    /// A recommender over one old node and the given backend responses.
    pub fn probe_recommender(
        responses: Vec<(&'static str, Result<Vec<Sample>, String>)>,
        with_node: bool,
    ) -> (Recommender, Arc<FakeProm>) {
        let nodes = if with_node {
            vec![node("a", "i-a", 30, Utc::now())]
        } else {
            vec![]
        };
        let prom = Arc::new(FakeProm {
            responses: Mutex::new(responses),
            ..FakeProm::default()
        });
        let r = Recommender::new(
            config(false),
            Arc::new(FakeNodes {
                nodes: Mutex::new(nodes),
                ..FakeNodes::default()
            }),
            prom.clone(),
            Arc::new(FakeEc2 {
                volumes: HashMap::new(),
                ..FakeEc2::default()
            }),
            Arc::new(Rec::default()),
            None,
            None,
        );
        (r, prom)
    }
}
