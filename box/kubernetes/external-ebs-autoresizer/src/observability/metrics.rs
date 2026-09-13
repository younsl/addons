//! The application's Prometheus collectors on a private registry. The metric
//! names and label sets are the ones the Go version exported, so dashboards
//! and alerts survive the port unchanged.

use std::collections::HashMap;
use std::time::Duration;

use prometheus::{Counter, CounterVec, Gauge, GaugeVec, Opts, Registry, TextEncoder};

/// The shared identity label set of the per-instance gauges. Keeping it
/// identical across both gauges lets dashboards join them without relabeling.
const INSTANCE_LABELS: [&str; 4] = ["instance_id", "device", "volume_id", "name"];

// The unused volume series are split the way Prometheus splits identity from
// measurement: an _info gauge that is always 1 and carries every descriptive
// label, and value gauges keyed by nothing but the object's own name.
const UNUSED_PVC_INFO_LABELS: [&str; 6] = [
    "namespace",
    "name",
    "volume_name",
    "volume_id",
    "storage_class",
    "reason",
];
const UNUSED_PV_INFO_LABELS: [&str; 7] = [
    "name",
    "volume_id",
    "storage_class",
    "reason",
    "reclaim_policy",
    "claim_namespace",
    "claim_name",
];
const UNUSED_PVC_LABELS: [&str; 2] = ["namespace", "name"];
const UNUSED_PV_LABELS: [&str; 1] = ["name"];

/// The shared identity label set of the per-node throughput gauges.
const NODE_LABELS: [&str; 3] = ["node", "instance_id", "volume_id"];

/// Holds the application's Prometheus collectors and implements every
/// subsystem's recorder trait.
pub struct Metrics {
    registry: Registry,
    usage: GaugeVec,
    volume_size: GaugeVec,
    resize_total: CounterVec,
    skip_total: CounterVec,
    error_total: CounterVec,
    reconcile_total: Counter,
    policy_instances: GaugeVec,

    node_current_mibps: GaugeVec,
    node_peak_mibps: GaugeVec,
    node_recommended_mibps: GaugeVec,
    recommendation_total: CounterVec,
    throughput_apply_total: CounterVec,
    throughput_apply_skip_total: CounterVec,
    recommender_reconcile_total: Counter,

    unused_pvc_info: GaugeVec,
    unused_pv_info: GaugeVec,
    unused_pvc_age: GaugeVec,
    unused_pvc_capacity: GaugeVec,
    unused_pv_age: GaugeVec,
    unused_pv_capacity: GaugeVec,
    unused_total: GaugeVec,
    unused_capacity_total: GaugeVec,
    unused_scan_total: Counter,
    unused_scan_failure_total: Counter,
    unused_scan_last_success: Gauge,
    unused_scan_duration: Gauge,
    leader: Gauge,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

fn gauge_vec(registry: &Registry, name: &str, help: &str, labels: &[&str]) -> GaugeVec {
    let g = GaugeVec::new(Opts::new(name, help), labels).expect("valid gauge vec");
    registry
        .register(Box::new(g.clone()))
        .expect("unique metric name");
    g
}

fn counter_vec(registry: &Registry, name: &str, help: &str, labels: &[&str]) -> CounterVec {
    let c = CounterVec::new(Opts::new(name, help), labels).expect("valid counter vec");
    registry
        .register(Box::new(c.clone()))
        .expect("unique metric name");
    c
}

fn counter(registry: &Registry, name: &str, help: &str) -> Counter {
    let c = Counter::with_opts(Opts::new(name, help)).expect("valid counter");
    registry
        .register(Box::new(c.clone()))
        .expect("unique metric name");
    c
}

fn gauge(registry: &Registry, name: &str, help: &str) -> Gauge {
    let g = Gauge::with_opts(Opts::new(name, help)).expect("valid gauge");
    registry
        .register(Box::new(g.clone()))
        .expect("unique metric name");
    g
}

impl Metrics {
    /// Builds the collectors and registers them on a private registry.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn new() -> Self {
        let r = Registry::new();
        Self {
            usage: gauge_vec(
                &r,
                "external_ebs_autoresizer_root_usage_percent",
                "Most recently measured root filesystem usage percent per instance.",
                &INSTANCE_LABELS,
            ),
            volume_size: gauge_vec(
                &r,
                "external_ebs_autoresizer_root_volume_size_gib",
                "Most recently observed root EBS volume size in GiB per instance. Size is a gauge value, not a label, so the series identity survives resizes.",
                &INSTANCE_LABELS,
            ),
            resize_total: counter_vec(
                &r,
                "external_ebs_autoresizer_resize_total",
                "Total resize attempts by result and matched resize policy.",
                &["result", "policy"],
            ),
            skip_total: counter_vec(
                &r,
                "external_ebs_autoresizer_skip_total",
                "Total instances skipped without a resize attempt, by reason and matched resize policy.",
                &["reason", "policy"],
            ),
            error_total: counter_vec(
                &r,
                "external_ebs_autoresizer_error_total",
                "Total errors by reconcile stage.",
                &["stage"],
            ),
            reconcile_total: counter(
                &r,
                "external_ebs_autoresizer_reconcile_total",
                "Total reconcile passes started.",
            ),
            policy_instances: gauge_vec(
                &r,
                "external_ebs_autoresizer_policy_instances",
                "Number of discovered instances matched by each resize policy in the latest pass (policy=default for instances matching no named policy).",
                &["policy"],
            ),
            node_current_mibps: gauge_vec(
                &r,
                "external_ebs_autoresizer_node_throughput_current_mibps",
                "Provisioned EBS throughput in MiB/s of the volume attached to each Kubernetes node.",
                &NODE_LABELS,
            ),
            node_peak_mibps: gauge_vec(
                &r,
                "external_ebs_autoresizer_node_throughput_observed_peak_mibps",
                "Observed peak EBS throughput in MiB/s per Kubernetes node over the configured observation window.",
                &NODE_LABELS,
            ),
            node_recommended_mibps: gauge_vec(
                &r,
                "external_ebs_autoresizer_node_throughput_recommended_mibps",
                "Recommended EBS throughput in MiB/s per Kubernetes node. Equal to the current value when no change is recommended.",
                &NODE_LABELS,
            ),
            recommendation_total: counter_vec(
                &r,
                "external_ebs_autoresizer_recommendation_total",
                "Total throughput recommendations published, by action (increase, decrease, none, unknown) and reason.",
                &["action", "reason"],
            ),
            throughput_apply_total: counter_vec(
                &r,
                "external_ebs_autoresizer_throughput_apply_total",
                "Total throughput piggyback attempts on volume size modifications, by result (applied, fallback_size_only).",
                &["result"],
            ),
            throughput_apply_skip_total: counter_vec(
                &r,
                "external_ebs_autoresizer_throughput_apply_skip_total",
                "Total volume size modifications that carried no throughput piggyback, by reason (no_recommendation, stale, not_increase).",
                &["reason"],
            ),
            recommender_reconcile_total: counter(
                &r,
                "external_ebs_autoresizer_recommender_reconcile_total",
                "Total throughput recommender passes started.",
            ),
            unused_pvc_info: gauge_vec(
                &r,
                "external_ebs_autoresizer_unused_pvc_info",
                "Always 1, one series per reported unused PersistentVolumeClaim. Carries the descriptive labels the report is listed by (namespace, name, volume_name, volume_id, storage_class, reason); join it to unused_pvc_age_seconds and unused_pvc_capacity_bytes on namespace and name for the numbers.",
                &UNUSED_PVC_INFO_LABELS,
            ),
            unused_pv_info: gauge_vec(
                &r,
                "external_ebs_autoresizer_unused_pv_info",
                "Always 1, one series per reported unused PersistentVolume. Carries the descriptive labels the report is listed by (name, volume_id, storage_class, reason, reclaim_policy, claim_namespace, claim_name); join it to unused_pv_age_seconds and unused_pv_capacity_bytes on name for the numbers.",
                &UNUSED_PV_INFO_LABELS,
            ),
            unused_pvc_age: gauge_vec(
                &r,
                "external_ebs_autoresizer_unused_pvc_age_seconds",
                "How long each reported PersistentVolumeClaim has been continuously unused, in seconds. Only claims past unusedVolumeScan.minUnusedAge are exported. Descriptive labels live on unused_pvc_info.",
                &UNUSED_PVC_LABELS,
            ),
            unused_pvc_capacity: gauge_vec(
                &r,
                "external_ebs_autoresizer_unused_pvc_capacity_bytes",
                "Provisioned capacity in bytes of each reported unused PersistentVolumeClaim. Keyed identically to the age gauge so the two join without relabeling.",
                &UNUSED_PVC_LABELS,
            ),
            unused_pv_age: gauge_vec(
                &r,
                "external_ebs_autoresizer_unused_pv_age_seconds",
                "How long each reported PersistentVolume has been continuously unused, in seconds. Descriptive labels live on unused_pv_info.",
                &UNUSED_PV_LABELS,
            ),
            unused_pv_capacity: gauge_vec(
                &r,
                "external_ebs_autoresizer_unused_pv_capacity_bytes",
                "Provisioned capacity in bytes of each reported unused PersistentVolume. Keyed identically to the age gauge so the two join without relabeling.",
                &UNUSED_PV_LABELS,
            ),
            unused_total: gauge_vec(
                &r,
                "external_ebs_autoresizer_unused_objects",
                "Number of reported unused objects in the latest scan, by kind and reason. Every known reason is published on every pass, so a reason that has stopped occurring reads as zero rather than as no data.",
                &["kind", "reason"],
            ),
            unused_capacity_total: gauge_vec(
                &r,
                "external_ebs_autoresizer_unused_objects_capacity_bytes",
                "Total provisioned capacity in bytes held by reported unused objects in the latest scan, by kind and reason.",
                &["kind", "reason"],
            ),
            unused_scan_total: counter(
                &r,
                "external_ebs_autoresizer_unused_scan_total",
                "Total unused volume scan passes started.",
            ),
            unused_scan_failure_total: counter(
                &r,
                "external_ebs_autoresizer_unused_scan_failure_total",
                "Total unused volume scan passes that ended in an error. Subtract it from unused_scan_total for the number that succeeded; error_total counts per-object failures inside a pass, which is a different unit of work.",
            ),
            unused_scan_last_success: gauge(
                &r,
                "external_ebs_autoresizer_unused_scan_last_success_timestamp_seconds",
                "Unix timestamp of the last unused volume scan pass that completed without error. Zero until the first one does. Alert on time() minus this value rather than on a boolean up gauge: the findings gauges are republished every pass and hold their last value forever, so age is the only thing that distinguishes a fresh report from a frozen one.",
            ),
            unused_scan_duration: gauge(
                &r,
                "external_ebs_autoresizer_unused_scan_duration_seconds",
                "Wall-clock duration of the most recent unused volume scan pass, successful or not. The pass reads the whole cluster in four list calls, so a rising value is the API server slowing down before it starts failing.",
            ),
            leader: gauge(
                &r,
                "external_ebs_autoresizer_leader",
                "1 on the replica currently running the reconcile loops, 0 on every other replica. Scope liveness alerts to it: a follower publishes no scan or reconcile activity by design, and an alert that ignores this fires on every non-leader.",
            ),
            registry: r,
        }
    }

    /// Renders the registry in the Prometheus text format.
    #[must_use]
    pub fn render(&self) -> String {
        TextEncoder::new()
            .encode_to_string(&self.registry.gather())
            .unwrap_or_default()
    }

    /// Clears the per-node throughput gauges at the start of a recommender
    /// pass. Nodes are short-lived under Karpenter, and without the reset a
    /// terminated node's last reading would stay exported forever.
    pub fn reset_node_throughput(&self) {
        self.node_current_mibps.reset();
        self.node_peak_mibps.reset();
        self.node_recommended_mibps.reset();
    }

    /// Records one node's provisioned, observed, and recommended throughput.
    pub fn observe_node_throughput(
        &self,
        node: &str,
        instance_id: &str,
        volume_id: &str,
        current: f64,
        peak: f64,
        recommended: f64,
    ) {
        let labels = [node, instance_id, volume_id];
        self.node_current_mibps
            .with_label_values(&labels)
            .set(current);
        self.node_peak_mibps.with_label_values(&labels).set(peak);
        self.node_recommended_mibps
            .with_label_values(&labels)
            .set(recommended);
    }

    /// Counts one published recommendation by action and reason.
    pub fn observe_recommendation(&self, action: &str, reason: &str) {
        self.recommendation_total
            .with_label_values(&[action, reason])
            .inc();
    }

    /// Records the latest measured usage for an instance.
    pub fn observe_usage(
        &self,
        instance_id: &str,
        device: &str,
        volume_id: &str,
        name: &str,
        percent: f64,
    ) {
        self.usage
            .with_label_values(&[instance_id, device, volume_id, name])
            .set(percent);
    }

    /// Records the latest known root volume size for an instance.
    pub fn observe_volume_size(
        &self,
        instance_id: &str,
        device: &str,
        volume_id: &str,
        name: &str,
        size_gib: i32,
    ) {
        self.volume_size
            .with_label_values(&[instance_id, device, volume_id, name])
            .set(f64::from(size_gib));
    }

    /// Counts a resize attempt by outcome and matched policy.
    pub fn observe_resize(&self, success: bool, policy: &str) {
        let result = if success { "success" } else { "failure" };
        self.resize_total.with_label_values(&[result, policy]).inc();
    }

    /// Counts an instance skipped without a resize attempt.
    pub fn observe_skip(&self, reason: &str, policy: &str) {
        self.skip_total.with_label_values(&[reason, policy]).inc();
    }

    /// Records, per resize policy, how many discovered instances matched it in
    /// the latest reconcile pass. Policies that matched nothing this pass are
    /// set to 0 so stale counts do not linger.
    pub fn observe_policy_instances(&self, counts: &HashMap<String, usize>) {
        self.policy_instances.reset();
        for (policy, n) in counts {
            #[allow(clippy::cast_precision_loss)]
            self.policy_instances
                .with_label_values(&[policy])
                .set(*n as f64);
        }
    }

    /// Counts one attempted throughput piggyback: applied, or
    /// `fallback_size_only` when the combined request was rejected.
    pub fn observe_throughput_apply(&self, result: &str) {
        self.throughput_apply_total
            .with_label_values(&[result])
            .inc();
    }

    /// Counts one volume modification that proceeded without a throughput
    /// piggyback, by reason.
    pub fn observe_throughput_apply_skip(&self, reason: &str) {
        self.throughput_apply_skip_total
            .with_label_values(&[reason])
            .inc();
    }

    /// Counts a recommender pass start.
    pub fn observe_recommender_reconcile(&self) {
        self.recommender_reconcile_total.inc();
    }

    /// Clears every unused volume gauge at the start of a scan pass, so a
    /// deleted object's last reading does not stay exported forever.
    pub fn reset_unused_volumes(&self) {
        self.unused_pvc_info.reset();
        self.unused_pv_info.reset();
        self.unused_pvc_age.reset();
        self.unused_pvc_capacity.reset();
        self.unused_pv_age.reset();
        self.unused_pv_capacity.reset();
        self.unused_total.reset();
        self.unused_capacity_total.reset();
    }

    /// Records one reported unused `PersistentVolumeClaim`.
    #[allow(clippy::too_many_arguments)]
    pub fn observe_unused_pvc(
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
        self.unused_pvc_info
            .with_label_values(&[
                namespace,
                name,
                volume_name,
                volume_id,
                storage_class,
                reason,
            ])
            .set(1.0);
        self.unused_pvc_age
            .with_label_values(&[namespace, name])
            .set(age_seconds);
        self.unused_pvc_capacity
            .with_label_values(&[namespace, name])
            .set(capacity_bytes);
    }

    /// Records one reported unused `PersistentVolume`.
    #[allow(clippy::too_many_arguments)]
    pub fn observe_unused_pv(
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
        self.unused_pv_info
            .with_label_values(&[
                name,
                volume_id,
                storage_class,
                reason,
                reclaim_policy,
                claim_namespace,
                claim_name,
            ])
            .set(1.0);
        self.unused_pv_age
            .with_label_values(&[name])
            .set(age_seconds);
        self.unused_pv_capacity
            .with_label_values(&[name])
            .set(capacity_bytes);
    }

    /// Records the count and total capacity of one kind and reason.
    pub fn observe_unused_summary(
        &self,
        kind: &str,
        reason: &str,
        count: usize,
        capacity_bytes: i64,
    ) {
        #[allow(clippy::cast_precision_loss)]
        self.unused_total
            .with_label_values(&[kind, reason])
            .set(count as f64);
        #[allow(clippy::cast_precision_loss)]
        self.unused_capacity_total
            .with_label_values(&[kind, reason])
            .set(capacity_bytes as f64);
    }

    /// Counts a scan pass start.
    pub fn observe_unused_scan(&self) {
        self.unused_scan_total.inc();
    }

    /// Records how one scan pass ended. A pass that returned no error stamps
    /// the success timestamp. The duration is recorded either way.
    pub fn observe_unused_scan_result(&self, duration: Duration, failed: bool) {
        self.unused_scan_duration.set(duration.as_secs_f64());
        if failed {
            self.unused_scan_failure_total.inc();
            return;
        }
        #[allow(clippy::cast_precision_loss)]
        self.unused_scan_last_success
            .set(chrono::Utc::now().timestamp() as f64);
    }

    /// Records whether this replica is the one running the reconcile loops.
    pub fn set_leader(&self, leading: bool) {
        self.leader.set(if leading { 1.0 } else { 0.0 });
    }

    /// Counts an error in the given reconcile stage.
    pub fn observe_error(&self, stage: &str) {
        self.error_total.with_label_values(&[stage]).inc();
    }

    /// Counts a reconcile pass start.
    pub fn observe_reconcile(&self) {
        self.reconcile_total.inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resizer_observations_render() {
        let m = Metrics::new();
        m.observe_usage("i-1", "/dev/xvda", "vol-1", "web", 85.0);
        m.observe_volume_size("i-1", "/dev/xvda", "vol-1", "web", 100);
        m.observe_resize(true, "default");
        m.observe_resize(false, "db");
        m.observe_skip("cooldown", "default");
        m.observe_error("measure");
        m.observe_reconcile();
        m.observe_policy_instances(&HashMap::from([
            ("default".to_string(), 2usize),
            ("db".to_string(), 0),
        ]));
        m.observe_throughput_apply("applied");
        m.observe_throughput_apply_skip("stale");
        m.observe_recommender_reconcile();
        let out = m.render();
        for want in [
            "external_ebs_autoresizer_root_usage_percent{device=\"/dev/xvda\",instance_id=\"i-1\",name=\"web\",volume_id=\"vol-1\"} 85",
            "external_ebs_autoresizer_root_volume_size_gib{device=\"/dev/xvda\",instance_id=\"i-1\",name=\"web\",volume_id=\"vol-1\"} 100",
            "external_ebs_autoresizer_resize_total{policy=\"default\",result=\"success\"} 1",
            "external_ebs_autoresizer_resize_total{policy=\"db\",result=\"failure\"} 1",
            "external_ebs_autoresizer_skip_total{policy=\"default\",reason=\"cooldown\"} 1",
            "external_ebs_autoresizer_error_total{stage=\"measure\"} 1",
            "external_ebs_autoresizer_reconcile_total 1",
            "external_ebs_autoresizer_policy_instances{policy=\"default\"} 2",
            "external_ebs_autoresizer_policy_instances{policy=\"db\"} 0",
            "external_ebs_autoresizer_throughput_apply_total{result=\"applied\"} 1",
            "external_ebs_autoresizer_throughput_apply_skip_total{reason=\"stale\"} 1",
            "external_ebs_autoresizer_recommender_reconcile_total 1",
        ] {
            assert!(out.contains(want), "missing {want}\n{out}");
        }
        // A policy that vanishes from the next pass is reset, not left stale.
        m.observe_policy_instances(&HashMap::from([("default".to_string(), 1usize)]));
        let out = m.render();
        assert!(!out.contains("policy_instances{policy=\"db\"}"), "{out}");
    }

    #[test]
    fn node_throughput_gauges_reset() {
        let m = Metrics::new();
        m.observe_node_throughput("n1", "i-1", "vol-1", 125.0, 80.5, 250.0);
        m.observe_recommendation("increase", "observed_peak_above_provisioned");
        let out = m.render();
        assert!(out.contains("external_ebs_autoresizer_node_throughput_current_mibps{instance_id=\"i-1\",node=\"n1\",volume_id=\"vol-1\"} 125"), "{out}");
        assert!(out.contains("node_throughput_observed_peak_mibps{instance_id=\"i-1\",node=\"n1\",volume_id=\"vol-1\"} 80.5"));
        assert!(out.contains("node_throughput_recommended_mibps{instance_id=\"i-1\",node=\"n1\",volume_id=\"vol-1\"} 250"));
        assert!(out.contains(
            "recommendation_total{action=\"increase\",reason=\"observed_peak_above_provisioned\"} 1"
        ));
        m.reset_node_throughput();
        assert!(!m.render().contains("node=\"n1\""));
    }

    #[test]
    fn unused_volume_gauges_and_scan_result() {
        let m = Metrics::new();
        m.observe_unused_pvc(
            "ns",
            "claim",
            "pv-1",
            "vol-1",
            "gp3",
            "no_consumer_pod",
            90000.0,
            1024.0,
        );
        m.observe_unused_pv(
            "pv-2", "vol-2", "gp3", "released", "Retain", "ns", "old", 5.0, 2048.0,
        );
        m.observe_unused_summary("persistentvolumeclaim", "no_consumer_pod", 1, 1024);
        m.observe_unused_scan();
        m.observe_unused_scan_result(Duration::from_millis(1500), false);
        m.set_leader(true);
        let out = m.render();
        for want in [
            "unused_pvc_info{name=\"claim\",namespace=\"ns\",reason=\"no_consumer_pod\",storage_class=\"gp3\",volume_id=\"vol-1\",volume_name=\"pv-1\"} 1",
            "unused_pvc_age_seconds{name=\"claim\",namespace=\"ns\"} 90000",
            "unused_pvc_capacity_bytes{name=\"claim\",namespace=\"ns\"} 1024",
            "unused_pv_info{claim_name=\"old\",claim_namespace=\"ns\",name=\"pv-2\",reason=\"released\",reclaim_policy=\"Retain\",storage_class=\"gp3\",volume_id=\"vol-2\"} 1",
            "unused_pv_age_seconds{name=\"pv-2\"} 5",
            "unused_pv_capacity_bytes{name=\"pv-2\"} 2048",
            "unused_objects{kind=\"persistentvolumeclaim\",reason=\"no_consumer_pod\"} 1",
            "unused_objects_capacity_bytes{kind=\"persistentvolumeclaim\",reason=\"no_consumer_pod\"} 1024",
            "unused_scan_total 1",
            "unused_scan_duration_seconds 1.5",
            "external_ebs_autoresizer_leader 1",
        ] {
            assert!(out.contains(want), "missing {want}\n{out}");
        }
        assert!(!out.contains("unused_scan_last_success_timestamp_seconds 0"));
        m.observe_unused_scan_result(Duration::from_secs(2), true);
        m.set_leader(false);
        m.reset_unused_volumes();
        let out = m.render();
        assert!(out.contains("unused_scan_failure_total 1"));
        assert!(out.contains("unused_scan_duration_seconds 2"));
        assert!(out.contains("external_ebs_autoresizer_leader 0"));
        assert!(!out.contains("name=\"claim\""));
    }
}
