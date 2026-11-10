//! Loads and validates runtime configuration from a single YAML file. A small
//! number of runtime-injected values (the Pod identity from the downward API
//! and the Grafana and Prometheus tokens from Secrets) are read from the
//! environment instead, since they cannot live in a plain `ConfigMap` file.

pub mod parse;
mod validate;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

/// The config file path used when `CONFIG_FILE` is unset.
pub const DEFAULT_CONFIG_FILE: &str = "/etc/volume-autoscaler/config.yaml";

/// Alertmanager notify-on and Grafana annotate-on policy values.
pub const NOTIFY_ON_ALL: &str = "all";
pub const NOTIFY_ON_SUCCESS: &str = "success";
pub const NOTIFY_ON_FAILURE: &str = "failure";
pub const ANNOTATE_ON_ALL: &str = "all";
pub const ANNOTATE_ON_SUCCESS: &str = "success";
pub const ANNOTATE_ON_FAILURE: &str = "failure";

/// A configuration that could not be loaded.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("read config file {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parse config file {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("{0}")]
    Invalid(String),
}

/// The settings of the node EBS throughput recommender. It is a separate
/// subsystem from the resizer: it reads a Prometheus-compatible metrics backend
/// and the Kubernetes API, targets the in-cluster Nodes rather than the
/// standalone EC2 instances the resizer manages, and never mutates an EBS
/// volume. Disabled by default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThroughputRecommendation {
    pub enabled: bool,
    /// How often recommendations are recomputed. Separate from
    /// `reconcile_interval` and higher by default: the observation window spans
    /// days, so a much shorter interval would re-derive the same answer while
    /// repeatedly running the most expensive query the addon issues.
    pub interval: Duration,
    /// The metric label carrying the Kubernetes node name.
    /// kube-prometheus-stack relabels it to `node`, while a plain node exporter
    /// scrape leaves only `instance`.
    pub metric_node_name_label: String,
    /// How far back the observation window reaches, as a Prometheus duration
    /// string (which accepts units Go durations do not, such as `7d`).
    pub lookback_window: String,
    /// `lookback_window` parsed, used to derive how many data points a full
    /// window holds.
    pub lookback_duration: Duration,
    /// Lets the resizer fold a fresh increase recommendation into a volume
    /// modification it is already making for a size expansion. Enabled by
    /// default whenever the recommender is; this key is the kill switch that
    /// turns the recommender advisory-only without losing it.
    pub apply_on_resize: bool,
}

/// The Prometheus-compatible backend every metric query goes to: a
/// cluster-local Prometheus, or a central Mimir shared by many clusters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetricsBackend {
    /// The base URL of a Prometheus server or a Mimir query-frontend/gateway.
    /// The `/prometheus` API prefix may be included or omitted; the client
    /// probes for it at startup.
    pub url: String,
    /// Sent as the `X-Scope-OrgID` header, which Mimir requires when
    /// multi-tenancy is enabled. Empty for Prometheus, which ignores it.
    pub tenant_id: String,
    /// Sent as an `Authorization: Bearer` header. Read from the environment
    /// only (never the config file) so the token stays out of the `ConfigMap`.
    pub bearer_token: String,
    /// Label matchers added to every query, so a central backend that holds
    /// many clusters' series answers for this cluster only, for example
    /// `cluster: prd-cluster`. Empty for a cluster-local Prometheus.
    pub label_selector: BTreeMap<String, String>,
}

/// Orphan PVC cleanup: opt-in deletion of the `PersistentVolumeClaims` the
/// unused volume scan reports. Disabled by default. Only claims are deleted,
/// never a volume.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrphanPvcCleanup {
    pub enabled: bool,
    pub min_unused_age: Duration,
    pub reasons: Vec<String>,
    pub include_namespaces: Vec<String>,
    pub exclude_namespaces: Vec<String>,
    pub max_deletions_per_pass: usize,
    /// Sends Alertmanager alerts for scheduled, deleted, and failed
    /// deletions. `alertmanager.enabled` remains the master gate.
    pub alert_enabled: bool,
    pub alert_before: Duration,
}

/// All runtime settings for the resizer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Config {
    pub region: String,
    pub reconcile_interval: Duration,
    /// How often each `PersistentVolumeClaimAutoscaler` is re-evaluated.
    pub claim_reconcile_interval: Duration,
    /// Bounds how many instances are reconciled in parallel within one pass.
    pub reconcile_concurrency: usize,
    /// The delay between status polls for SSM command invocations and EBS
    /// volume modifications.
    pub ssm_poll_interval: Duration,
    pub ssm_command_timeout: Duration,
    pub volume_modify_timeout: Duration,
    /// Measures and decides but never mutates AWS resources.
    pub dry_run: bool,
    pub health_port: u16,
    pub metrics_port: u16,
    pub log_level: String,
    pub log_format: String,
    /// The controller's own Pod, for Kubernetes Event publishing. Populated
    /// via the downward API; when empty, Event publishing is disabled.
    pub pod_name: String,
    pub pod_namespace: String,
    pub pod_uid: String,
    /// Enables single-active-instance leader election via a Lease. Ignored
    /// when `pod_name` is empty.
    pub leader_elect: bool,
    pub lease_name: String,
    pub alertmanager_enabled: bool,
    pub alertmanager_url: String,
    pub alertmanager_timeout: Duration,
    /// Static labels merged into every alert for routing.
    pub alertmanager_labels: BTreeMap<String, String>,
    /// Which resize outcomes are alerted: all, success, or failure.
    pub alertmanager_notify_on: String,
    /// An optional dashboard URL template appended to each alert's description
    /// as a Slack mrkdwn link. `{key}` placeholders are substituted with the
    /// alert's labels.
    pub alertmanager_dashboard_url: String,
    pub grafana_annotation_enabled: bool,
    pub grafana_url: String,
    /// Read from the environment only, so the token stays out of the
    /// `ConfigMap`.
    pub grafana_api_token: String,
    pub grafana_timeout: Duration,
    pub grafana_annotation_tags: Vec<String>,
    /// Which resize outcomes are annotated: all, success, or failure.
    pub grafana_annotate_on: String,
    pub throughput_recommendation: ThroughputRecommendation,
    pub metrics: MetricsBackend,
    pub orphan_pvc_cleanup: OrphanPvcCleanup,
    /// Invalid values that were replaced instead of failing startup, logged
    /// once the logger is installed.
    pub warnings: Vec<String>,
}

/// The on-disk YAML shape. Durations are strings (Go duration syntax plus `d`
/// for days) parsed
/// during `load`. Every optional key carries its default here, so a key
/// omitted from the file keeps its default and a key present overrides it,
/// including with an explicit zero. Parsing is strict: an unknown key is an
/// error.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct FileSchema {
    region: String,
    /// Retired: `InstanceVolumeAutoscaler` `scaleTargetRef` scopes targets.
    tag_filters: Option<serde_yaml::Value>,
    /// Retired: an instance is managed only when an autoscaler targets it.
    #[serde(rename = "excludeEKSNodes")]
    exclude_eks_nodes: Option<serde_yaml::Value>,
    reconcile_interval: String,
    claim_reconcile_interval: String,
    reconcile_concurrency: i64,
    #[serde(rename = "ssmPollInterval")]
    ssm_poll_interval: String,
    /// Retired: resize settings live in `InstanceVolumeAutoscaler`
    /// resources. Kept as a key so an old file fails with a pointer to them.
    default_policy: Option<serde_yaml::Value>,
    #[serde(rename = "ssmCommandTimeout")]
    ssm_command_timeout: String,
    volume_modify_timeout: String,
    dry_run: bool,
    health_port: u16,
    metrics_port: u16,
    leader_elect: bool,
    lease_name: String,
    log_level: String,
    log_format: String,
    alertmanager: AlertmanagerFile,
    grafana_annotation: GrafanaFile,
    throughput_recommendation: ThroughputFile,
    metrics: MetricsFile,
    orphan_pvc_cleanup: OrphanPvcCleanupFile,
    policies: Option<serde_yaml::Value>,
    /// Keys this version does not know, reported and ignored.
    #[serde(flatten)]
    extra: BTreeMap<String, serde_yaml::Value>,
}

impl Default for FileSchema {
    fn default() -> Self {
        Self {
            region: String::new(),
            tag_filters: None,
            exclude_eks_nodes: None,
            reconcile_interval: "5m".into(),
            claim_reconcile_interval: "1m".into(),
            reconcile_concurrency: 10,
            ssm_poll_interval: "1s".into(),
            default_policy: None,
            ssm_command_timeout: "5m".into(),
            volume_modify_timeout: "10m".into(),
            dry_run: false,
            health_port: 8080,
            metrics_port: 8081,
            leader_elect: true,
            lease_name: "volume-autoscaler".into(),
            log_level: "info".into(),
            log_format: "json".into(),
            alertmanager: AlertmanagerFile::default(),
            grafana_annotation: GrafanaFile::default(),
            throughput_recommendation: ThroughputFile::default(),
            metrics: MetricsFile::default(),
            orphan_pvc_cleanup: OrphanPvcCleanupFile::default(),
            policies: None,
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct AlertmanagerFile {
    enabled: bool,
    url: String,
    timeout: String,
    labels: BTreeMap<String, String>,
    notify_on: String,
    dashboard_url: String,
    /// Keys this version does not know, reported and ignored.
    #[serde(flatten)]
    extra: BTreeMap<String, serde_yaml::Value>,
}

impl Default for AlertmanagerFile {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            timeout: "5s".into(),
            labels: BTreeMap::new(),
            notify_on: NOTIFY_ON_SUCCESS.into(),
            dashboard_url: String::new(),
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct GrafanaFile {
    enabled: bool,
    url: String,
    timeout: String,
    tags: Vec<String>,
    annotate_on: String,
    /// Keys this version does not know, reported and ignored.
    #[serde(flatten)]
    extra: BTreeMap<String, serde_yaml::Value>,
}

impl Default for GrafanaFile {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            timeout: "5s".into(),
            tags: vec!["event:ebs-resize".into()],
            annotate_on: ANNOTATE_ON_ALL.into(),
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct MetricsFile {
    url: String,
    tenant_id: String,
    label_selector: BTreeMap<String, String>,
    /// Keys this version does not know, reported and ignored.
    #[serde(flatten)]
    extra: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ThroughputFile {
    enabled: bool,
    /// Retired: the backend moved to the top-level `metrics` block.
    prometheus_url: Option<serde_yaml::Value>,
    /// Retired: the backend moved to the top-level `metrics` block.
    prometheus_tenant_id: Option<serde_yaml::Value>,
    interval: String,
    metric_node_name_label: String,
    lookback_window: String,
    apply_on_resize: bool,
    /// Keys this version does not know, reported and ignored.
    #[serde(flatten)]
    extra: BTreeMap<String, serde_yaml::Value>,
}

impl Default for ThroughputFile {
    fn default() -> Self {
        Self {
            enabled: false,
            prometheus_url: None,
            prometheus_tenant_id: None,
            interval: "30m".into(),
            // "node" is the label kube-prometheus-stack relabels onto node
            // exporter series. A plain node exporter scrape leaves only
            // "instance", in which case this needs overriding.
            metric_node_name_label: "node".into(),
            lookback_window: "7d".into(),
            // Piggybacking rides the recommender's own enabled switch; an
            // explicit false here is the kill switch that keeps it
            // advisory-only.
            apply_on_resize: true,
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct OrphanPvcCleanupFile {
    enabled: bool,
    min_unused_age: String,
    /// Retired: deletion happens at `minUnusedAge`.
    notice_period: Option<serde_yaml::Value>,
    reasons: Vec<String>,
    include_namespaces: Vec<String>,
    exclude_namespaces: Vec<String>,
    max_deletions_per_pass: i64,
    alert_enabled: bool,
    alert_before: String,
    /// Keys this version does not know, reported and ignored.
    #[serde(flatten)]
    extra: BTreeMap<String, serde_yaml::Value>,
}

impl OrphanPvcCleanupFile {
    fn parse(&self, fx: &mut Fixes) -> OrphanPvcCleanup {
        let d = Self::default();
        fx.unknown("orphanPvcCleanup.", &self.extra);
        if self.notice_period.is_some() {
            fx.push(
                "orphanPvcCleanup.noticePeriod is no longer a config key and is ignored: a claim is deleted once unused for minUnusedAge, and scheduled alertBefore ahead"
                    .into(),
            );
        }
        OrphanPvcCleanup {
            enabled: self.enabled,
            min_unused_age: fx.duration(
                "orphanPvcCleanup.minUnusedAge",
                &self.min_unused_age,
                &d.min_unused_age,
            ),
            reasons: self.reasons.clone(),
            include_namespaces: self.include_namespaces.clone(),
            exclude_namespaces: self.exclude_namespaces.clone(),
            max_deletions_per_pass: usize::try_from(self.max_deletions_per_pass).unwrap_or(0),
            alert_enabled: self.alert_enabled,
            alert_before: fx.duration(
                "orphanPvcCleanup.alertBefore",
                &self.alert_before,
                &d.alert_before,
            ),
        }
    }
}

/// Collects the problems corrected while loading, so an invalid value is
/// replaced and reported rather than failing startup.
#[derive(Debug, Default)]
struct Fixes(Vec<String>);

impl Fixes {
    fn push(&mut self, msg: String) {
        self.0.push(msg);
    }

    /// Parses `raw`, or falls back to `default` and records why.
    fn duration(&mut self, name: &str, raw: &str, default: &str) -> Duration {
        parse::parse_duration(name, raw).unwrap_or_else(|err| {
            self.push(format!("{err}, using {default}"));
            parse::parse_duration(name, default).unwrap_or_default()
        })
    }

    fn unknown(&mut self, prefix: &str, extra: &BTreeMap<String, serde_yaml::Value>) {
        for k in extra.keys() {
            self.push(format!("unknown key {prefix}{k} ignored"));
        }
    }
}

impl Default for OrphanPvcCleanupFile {
    fn default() -> Self {
        Self {
            enabled: false,
            min_unused_age: "7d".into(),
            notice_period: None,
            reasons: crate::pvscan::UNUSED_PVC_REASONS
                .iter()
                .map(|r| (*r).to_string())
                .collect(),
            include_namespaces: Vec::new(),
            exclude_namespaces: vec!["kube-system".into()],
            max_deletions_per_pass: 1,
            alert_enabled: true,
            alert_before: "6h".into(),
            extra: BTreeMap::new(),
        }
    }
}

/// The environment-injected runtime values: the Pod identity and the tokens
/// that never live in the config file.
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub pod_name: String,
    pub pod_namespace: String,
    pub pod_uid: String,
    pub grafana_api_token: String,
    pub prometheus_bearer_token: String,
    pub log_level: Option<String>,
    pub log_format: Option<String>,
}

impl Env {
    /// Reads the values from the process environment.
    #[must_use]
    pub fn from_process() -> Self {
        let get = |k: &str| std::env::var(k).unwrap_or_default();
        Self {
            pod_name: get("POD_NAME"),
            pod_namespace: get("POD_NAMESPACE"),
            pod_uid: get("POD_UID"),
            grafana_api_token: get("GRAFANA_API_TOKEN"),
            prometheus_bearer_token: get("PROMETHEUS_BEARER_TOKEN"),
            log_level: Some(get("LOG_LEVEL")).filter(|v| !v.is_empty()),
            log_format: Some(get("LOG_FORMAT")).filter(|v| !v.is_empty()),
        }
    }
}

/// Resolves the config file path: the `--config` flag, else `CONFIG_FILE`,
/// else the mounted default path.
#[must_use]
pub fn resolve_path(flag: Option<&str>) -> String {
    if let Some(p) = flag {
        return p.to_string();
    }
    match std::env::var("CONFIG_FILE") {
        Ok(p) if !p.is_empty() => p,
        _ => DEFAULT_CONFIG_FILE.to_string(),
    }
}

/// Reads, parses, and validates the YAML config file at `path`, then layers in
/// the environment-injected runtime values.
pub fn load(path: &Path, env: &Env) -> Result<Config, ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.display().to_string(),
        source,
    })?;
    parse(&raw, env).map_err(|err| match err {
        ConfigError::Parse { source, .. } => ConfigError::Parse {
            path: path.display().to_string(),
            source,
        },
        other => other,
    })
}

/// Where a retired resize key now lives.
const TO_CRD: &str = "declare targets and resize settings as autoscaler resources";
/// Where a retired metrics backend key now lives.
const TO_METRICS: &str = "set the backend under metrics.url and metrics.tenantId";

/// Records every key the file sets that this version ignores: retired keys,
/// with where their setting moved, and unknown keys.
fn report_ignored(f: &FileSchema, fx: &mut Fixes) {
    for (key, present, hint) in [
        ("defaultPolicy", f.default_policy.is_some(), TO_CRD),
        ("policies", f.policies.is_some(), TO_CRD),
        ("tagFilters", f.tag_filters.is_some(), TO_CRD),
        ("excludeEKSNodes", f.exclude_eks_nodes.is_some(), TO_CRD),
        (
            "throughputRecommendation.prometheusUrl",
            f.throughput_recommendation.prometheus_url.is_some(),
            TO_METRICS,
        ),
        (
            "throughputRecommendation.prometheusTenantId",
            f.throughput_recommendation.prometheus_tenant_id.is_some(),
            TO_METRICS,
        ),
    ] {
        if present {
            fx.push(format!(
                "{key} is no longer a config key and is ignored: {hint}"
            ));
        }
    }
    fx.unknown("", &f.extra);
    fx.unknown("alertmanager.", &f.alertmanager.extra);
    fx.unknown("grafanaAnnotation.", &f.grafana_annotation.extra);
    fx.unknown("metrics.", &f.metrics.extra);
    fx.unknown(
        "throughputRecommendation.",
        &f.throughput_recommendation.extra,
    );
}

/// Parses the throughput observation window, falling back to the default.
fn lookback_window(raw: &str, default: &str, fx: &mut Fixes) -> (String, Duration) {
    const KEY: &str = "throughputRecommendation.lookbackWindow";
    parse::parse_prom_duration(KEY, raw).unwrap_or_else(|err| {
        fx.push(format!("{err}, using {default}"));
        parse::parse_prom_duration(KEY, default).unwrap_or_default()
    })
}

/// Parses a config document. `load` wraps it with the file path. Malformed
/// YAML, a value of the wrong type, and a missing `region` fail. Every other
/// invalid value is replaced by its default or clamped into range, and listed
/// in `Config::warnings`, so a bad value never keeps the controller down.
pub fn parse(raw: &str, env: &Env) -> Result<Config, ConfigError> {
    let f: FileSchema = serde_yaml::from_str(raw).map_err(|source| ConfigError::Parse {
        path: String::new(),
        source,
    })?;
    let d = FileSchema::default();
    let mut fx = Fixes::default();

    report_ignored(&f, &mut fx);

    let tr = &f.throughput_recommendation;
    let dtr = &d.throughput_recommendation;
    let mut c = Config {
        region: f.region.clone(),
        reconcile_interval: fx.duration(
            "reconcileInterval",
            &f.reconcile_interval,
            &d.reconcile_interval,
        ),
        claim_reconcile_interval: fx.duration(
            "claimReconcileInterval",
            &f.claim_reconcile_interval,
            &d.claim_reconcile_interval,
        ),
        reconcile_concurrency: usize::try_from(f.reconcile_concurrency).unwrap_or(0),
        ssm_poll_interval: fx.duration(
            "ssmPollInterval",
            &f.ssm_poll_interval,
            &d.ssm_poll_interval,
        ),
        ssm_command_timeout: fx.duration(
            "ssmCommandTimeout",
            &f.ssm_command_timeout,
            &d.ssm_command_timeout,
        ),
        volume_modify_timeout: fx.duration(
            "volumeModifyTimeout",
            &f.volume_modify_timeout,
            &d.volume_modify_timeout,
        ),
        dry_run: f.dry_run,
        health_port: f.health_port,
        metrics_port: f.metrics_port,
        log_level: env.log_level.clone().unwrap_or_else(|| f.log_level.clone()),
        log_format: env
            .log_format
            .clone()
            .unwrap_or_else(|| f.log_format.clone()),
        pod_name: env.pod_name.clone(),
        pod_namespace: env.pod_namespace.clone(),
        pod_uid: env.pod_uid.clone(),
        leader_elect: f.leader_elect,
        lease_name: f.lease_name.clone(),
        alertmanager_enabled: f.alertmanager.enabled,
        alertmanager_url: f.alertmanager.url.clone(),
        alertmanager_timeout: fx.duration(
            "alertmanager.timeout",
            &f.alertmanager.timeout,
            &d.alertmanager.timeout,
        ),
        alertmanager_labels: f.alertmanager.labels.clone(),
        alertmanager_notify_on: f.alertmanager.notify_on.clone(),
        alertmanager_dashboard_url: f.alertmanager.dashboard_url.clone(),
        grafana_annotation_enabled: f.grafana_annotation.enabled,
        grafana_url: f.grafana_annotation.url.clone(),
        grafana_api_token: env.grafana_api_token.clone(),
        grafana_timeout: fx.duration(
            "grafanaAnnotation.timeout",
            &f.grafana_annotation.timeout,
            &d.grafana_annotation.timeout,
        ),
        grafana_annotation_tags: f.grafana_annotation.tags.clone(),
        grafana_annotate_on: f.grafana_annotation.annotate_on.clone(),
        throughput_recommendation: ThroughputRecommendation {
            enabled: tr.enabled,
            interval: fx.duration(
                "throughputRecommendation.interval",
                &tr.interval,
                &dtr.interval,
            ),
            metric_node_name_label: tr.metric_node_name_label.clone(),
            lookback_window: String::new(),
            lookback_duration: Duration::ZERO,
            apply_on_resize: tr.apply_on_resize,
        },
        metrics: MetricsBackend {
            url: f.metrics.url.trim().to_string(),
            tenant_id: f.metrics.tenant_id.trim().to_string(),
            bearer_token: env.prometheus_bearer_token.clone(),
            label_selector: f.metrics.label_selector.clone(),
        },
        orphan_pvc_cleanup: f.orphan_pvc_cleanup.parse(&mut fx),
        warnings: Vec::new(),
    };

    // The observation window is a Prometheus duration, not a Go duration: "7d"
    // is valid PromQL and invalid Go, while "1.5h" is the reverse.
    let (window, window_duration) =
        lookback_window(&tr.lookback_window, &dtr.lookback_window, &mut fx);
    c.throughput_recommendation.lookback_window = window;
    c.throughput_recommendation.lookback_duration = window_duration;

    c.sanitize(&mut fx)?;
    c.warnings = fx.0;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

    type Check = Box<dyn Fn(&Config) -> bool>;

    const MINIMAL: &str = indoc! {"
        region: ap-northeast-2
    "};

    fn env() -> Env {
        Env::default()
    }

    /// `MINIMAL` followed by extra top-level keys.
    fn with(extra: &str) -> String {
        format!("{MINIMAL}{extra}")
    }

    #[test]
    fn defaults() {
        let c = parse(MINIMAL, &env()).unwrap();
        assert_eq!(c.region, "ap-northeast-2");
        assert_eq!(c.reconcile_interval, Duration::from_mins(5));
        assert_eq!(c.claim_reconcile_interval, Duration::from_mins(1));
        assert_eq!(c.reconcile_concurrency, 10);
        assert_eq!(c.ssm_poll_interval, Duration::from_secs(1));
        assert_eq!(c.ssm_command_timeout, Duration::from_mins(5));
        assert_eq!(c.volume_modify_timeout, Duration::from_mins(10));
        assert!(!c.dry_run);
        assert_eq!(c.health_port, 8080);
        assert_eq!(c.metrics_port, 8081);
        assert!(c.leader_elect);
        assert_eq!(c.lease_name, "volume-autoscaler");
        assert_eq!(c.log_level, "info");
        assert_eq!(c.log_format, "json");
        assert!(!c.alertmanager_enabled);
        assert_eq!(c.alertmanager_timeout, Duration::from_secs(5));
        assert_eq!(c.alertmanager_notify_on, NOTIFY_ON_SUCCESS);
        assert!(!c.grafana_annotation_enabled);
        assert_eq!(c.grafana_annotation_tags, vec!["event:ebs-resize"]);
        assert_eq!(c.grafana_annotate_on, ANNOTATE_ON_ALL);
        let tr = &c.throughput_recommendation;
        assert!(!tr.enabled);
        assert_eq!(tr.interval, Duration::from_mins(30));
        assert_eq!(tr.metric_node_name_label, "node");
        assert_eq!(tr.lookback_window, "7d");
        assert_eq!(tr.lookback_duration, Duration::from_hours(168));
        assert!(tr.apply_on_resize);
        assert!(c.pod_name.is_empty(), "{:?}", c.pod_name);
        let uc = &c.orphan_pvc_cleanup;
        assert!(!uc.enabled);
        assert_eq!(uc.min_unused_age, Duration::from_hours(168));
        assert_eq!(
            uc.reasons,
            vec!["no_consumer_pod", "statefulset_scaled_down", "unbound"]
        );
        assert!(uc.alert_enabled);
        assert_eq!(uc.alert_before, Duration::from_hours(6));
        assert_eq!(uc.include_namespaces, Vec::<String>::new());
        assert_eq!(uc.exclude_namespaces, vec!["kube-system"]);
        assert_eq!(uc.max_deletions_per_pass, 1);
    }

    #[test]
    fn retired_keys_are_reported_and_ignored() {
        for (raw, hint) in [
            (
                with("defaultPolicy:\n  usageThresholdPercent: 80\n"),
                "autoscaler resources",
            ),
            (with("policies: []\n"), "autoscaler resources"),
            (with("tagFilters: \"\"\n"), "autoscaler resources"),
            (with("excludeEKSNodes: false\n"), "autoscaler resources"),
            (
                with("throughputRecommendation:\n  prometheusUrl: http://p\n"),
                "metrics.url",
            ),
            (
                with("throughputRecommendation:\n  prometheusTenantId: t\n"),
                "metrics.url",
            ),
            (
                with("orphanPvcCleanup:\n  noticePeriod: 24h\n"),
                "deleted once unused for minUnusedAge",
            ),
        ] {
            let c = parse(&raw, &env()).unwrap();
            assert_eq!(c.warnings.len(), 1, "{:?}", c.warnings);
            assert!(
                c.warnings[0].contains("no longer a config key"),
                "{:?}",
                c.warnings
            );
            assert!(c.warnings[0].contains(hint), "{:?}", c.warnings);
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn overrides_and_explicit_zeros() {
        let raw = indoc! {r#"
            region: us-east-1
            reconcileInterval: 1h30m
            reconcileConcurrency: 3
            ssmPollInterval: 500ms
            ssmCommandTimeout: 30s
            volumeModifyTimeout: 20m
            dryRun: true
            healthPort: 9000
            metricsPort: 9001
            leaderElect: false
            leaseName: custom
            logLevel: debug
            logFormat: text
            alertmanager:
              enabled: true
              url: http://am:9093/
              timeout: 2s
              labels:
                cluster: prod
              notifyOn: all
              dashboardUrl: "https://g/{instance_id}"
            grafanaAnnotation:
              enabled: true
              url: http://grafana:3000
              timeout: 3s
              tags: []
              annotateOn: failure
            metrics:
              url: " http://mimir/prometheus "
              tenantId: " tenant "
              labelSelector:
                cluster: prd
            throughputRecommendation:
              enabled: true
              interval: 1h
              metricNodeNameLabel: instance
              lookbackWindow: 12h
              applyOnResize: false
            orphanPvcCleanup:
              enabled: true
              minUnusedAge: 30d
              reasons: [no_consumer_pod, statefulset_scaled_down]
              includeNamespaces: [legacy]
              excludeNamespaces: []
              maxDeletionsPerPass: 3
              alertEnabled: false
              alertBefore: 2d
        "#};
        let e = Env {
            pod_name: "pod-0".into(),
            pod_namespace: "kube-system".into(),
            pod_uid: "uid".into(),
            grafana_api_token: "tok".into(),
            prometheus_bearer_token: "bearer".into(),
            ..Env::default()
        };
        let c = parse(raw, &e).unwrap();
        assert_eq!(c.reconcile_interval, Duration::from_mins(90));
        assert_eq!(c.reconcile_concurrency, 3);
        assert_eq!(c.ssm_poll_interval, Duration::from_millis(500));
        assert_eq!(c.ssm_command_timeout, Duration::from_secs(30));
        assert_eq!(c.volume_modify_timeout, Duration::from_mins(20));
        assert!(c.dry_run);
        assert_eq!(c.health_port, 9000);
        assert_eq!(c.metrics_port, 9001);
        assert!(!c.leader_elect);
        assert_eq!(c.lease_name, "custom");
        assert_eq!(c.log_level, "debug");
        assert_eq!(c.log_format, "text");
        assert!(c.alertmanager_enabled);
        assert_eq!(c.alertmanager_url, "http://am:9093/");
        assert_eq!(c.alertmanager_timeout, Duration::from_secs(2));
        assert_eq!(c.alertmanager_labels.get("cluster").unwrap(), "prod");
        assert_eq!(c.alertmanager_notify_on, "all");
        assert_eq!(c.alertmanager_dashboard_url, "https://g/{instance_id}");
        assert!(c.grafana_annotation_enabled);
        assert_eq!(c.grafana_api_token, "tok");
        assert_eq!(c.grafana_timeout, Duration::from_secs(3));
        assert!(
            c.grafana_annotation_tags.is_empty(),
            "explicit empty list overrides"
        );
        assert_eq!(c.grafana_annotate_on, "failure");
        let tr = &c.throughput_recommendation;
        assert!(tr.enabled);
        assert_eq!(c.metrics.url, "http://mimir/prometheus");
        assert_eq!(c.metrics.tenant_id, "tenant");
        assert_eq!(c.metrics.bearer_token, "bearer");
        assert_eq!(c.metrics.label_selector["cluster"], "prd");
        assert_eq!(tr.interval, Duration::from_hours(1));
        assert_eq!(tr.metric_node_name_label, "instance");
        assert_eq!(tr.lookback_window, "12h");
        assert_eq!(tr.lookback_duration, Duration::from_hours(12));
        assert!(!tr.apply_on_resize);
        assert_eq!(c.pod_name, "pod-0");
        assert_eq!(c.pod_namespace, "kube-system");
        assert_eq!(c.pod_uid, "uid");
        let uc = &c.orphan_pvc_cleanup;
        assert!(uc.enabled);
        assert_eq!(uc.min_unused_age, Duration::from_hours(720));
        assert_eq!(
            uc.reasons,
            vec!["no_consumer_pod", "statefulset_scaled_down"]
        );
        assert_eq!(uc.include_namespaces, vec!["legacy"]);
        assert_eq!(uc.exclude_namespaces, Vec::<String>::new());
        assert_eq!(uc.max_deletions_per_pass, 3);
        assert!(!uc.alert_enabled);
        assert_eq!(uc.alert_before, Duration::from_hours(48));
    }

    #[test]
    fn unknown_keys_are_reported_and_ignored() {
        for (raw, key) in [
            (with("bogus: 1\n"), "bogus"),
            (with("alertmanager:\n  bogus: 1\n"), "alertmanager.bogus"),
            (
                with("throughputRecommendation:\n  quantile: 0.9\n"),
                "throughputRecommendation.quantile",
            ),
            (
                with("orphanPvcCleanup:\n  enabeld: true\n"),
                "orphanPvcCleanup.enabeld",
            ),
        ] {
            let c = parse(&raw, &env()).unwrap();
            assert_eq!(c.warnings, vec![format!("unknown key {key} ignored")]);
        }
        assert!(
            !parse(&with("orphanPvcCleanup:\n  enabeld: true\n"), &env())
                .unwrap()
                .orphan_pvc_cleanup
                .enabled
        );
    }

    #[test]
    fn structural_errors_still_fail() {
        for raw in [
            with("dryRun: maybe\n"),
            with("orphanPvcCleanup:\n  maxDeletionsPerPass: many\n"),
            "region: [\n".to_string(),
        ] {
            let err = parse(&raw, &env()).unwrap_err();
            assert!(matches!(err, ConfigError::Parse { .. }), "{raw}: {err}");
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn invalid_values_fail_open() {
        assert!(
            parse("dryRun: true\n", &env())
                .unwrap_err()
                .to_string()
                .contains("region is required")
        );
        let cases: Vec<(String, &str, Check)> = vec![
            (
                with("reconcileInterval: 5min\n"),
                "invalid reconcileInterval",
                Box::new(|c| c.reconcile_interval == Duration::from_mins(5)),
            ),
            (
                with("reconcileInterval: 0s\n"),
                "reconcileInterval must be greater than 0",
                Box::new(|c| c.reconcile_interval == Duration::from_mins(5)),
            ),
            (
                with("claimReconcileInterval: 5s\n"),
                "claimReconcileInterval must be at least 10s",
                Box::new(|c| c.claim_reconcile_interval == Duration::from_secs(10)),
            ),
            (
                with("reconcileConcurrency: 0\n"),
                "reconcileConcurrency must be greater than 0",
                Box::new(|c| c.reconcile_concurrency == 10),
            ),
            (
                with("reconcileConcurrency: -1\n"),
                "reconcileConcurrency must be greater than 0",
                Box::new(|c| c.reconcile_concurrency == 10),
            ),
            (
                with("ssmPollInterval: 0\n"),
                "ssmPollInterval must be greater than 0",
                Box::new(|c| c.ssm_poll_interval == Duration::from_secs(1)),
            ),
            (
                with("ssmCommandTimeout: x\n"),
                "invalid ssmCommandTimeout",
                Box::new(|c| c.ssm_command_timeout == Duration::from_mins(5)),
            ),
            (
                with("volumeModifyTimeout: 1.5h\n"),
                "invalid volumeModifyTimeout",
                Box::new(|c| c.volume_modify_timeout == Duration::from_mins(10)),
            ),
            (
                with("alertmanager:\n  notifyOn: never\n"),
                "alertmanager.notifyOn must be one of",
                Box::new(|c| c.alertmanager_notify_on == NOTIFY_ON_SUCCESS),
            ),
            (
                with("alertmanager:\n  enabled: true\n"),
                "alertmanager.url is required",
                Box::new(|c| !c.alertmanager_enabled),
            ),
            (
                with("alertmanager:\n  timeout: bogus\n"),
                "invalid alertmanager.timeout",
                Box::new(|c| c.alertmanager_timeout == Duration::from_secs(5)),
            ),
            (
                with("grafanaAnnotation:\n  annotateOn: never\n"),
                "grafanaAnnotation.annotateOn must be one of",
                Box::new(|c| c.grafana_annotate_on == ANNOTATE_ON_ALL),
            ),
            (
                with("grafanaAnnotation:\n  enabled: true\n  url: \"\"\n"),
                "grafanaAnnotation.url is required",
                Box::new(|c| !c.grafana_annotation_enabled),
            ),
            (
                with("grafanaAnnotation:\n  timeout: bogus\n"),
                "invalid grafanaAnnotation.timeout",
                Box::new(|c| c.grafana_timeout == Duration::from_secs(5)),
            ),
            (
                with("throughputRecommendation:\n  enabled: true\n"),
                "metrics.url is required when throughputRecommendation.enabled",
                Box::new(|c| !c.throughput_recommendation.enabled),
            ),
            (
                with("metrics:\n  labelSelector:\n    bad-key: x\n    cluster: prd\n"),
                "invalid metrics.labelSelector key",
                Box::new(|c| c.metrics.label_selector.len() == 1),
            ),
            (
                with("throughputRecommendation:\n  interval: 0s\n"),
                "throughputRecommendation.interval must be greater than 0",
                Box::new(|c| c.throughput_recommendation.interval == Duration::from_mins(30)),
            ),
            (
                with("throughputRecommendation:\n  interval: soon\n"),
                "invalid throughputRecommendation.interval",
                Box::new(|c| c.throughput_recommendation.interval == Duration::from_mins(30)),
            ),
            (
                with("throughputRecommendation:\n  metricNodeNameLabel: \"no-de\"\n"),
                "invalid throughputRecommendation.metricNodeNameLabel",
                Box::new(|c| c.throughput_recommendation.metric_node_name_label == "node"),
            ),
            (
                with("throughputRecommendation:\n  lookbackWindow: 1.5h\n"),
                "invalid throughputRecommendation.lookbackWindow",
                Box::new(|c| c.throughput_recommendation.lookback_window == "7d"),
            ),
            (
                with("orphanPvcCleanup:\n  minUnusedAge: 1.5d\n"),
                "invalid orphanPvcCleanup.minUnusedAge",
                Box::new(|c| c.orphan_pvc_cleanup.min_unused_age == Duration::from_hours(168)),
            ),
            (
                with("orphanPvcCleanup:\n  minUnusedAge: 12h\n"),
                "orphanPvcCleanup.minUnusedAge must be at least 24h",
                Box::new(|c| c.orphan_pvc_cleanup.min_unused_age == Duration::from_hours(24)),
            ),
            (
                with("orphanPvcCleanup:\n  alertBefore: 30m\n"),
                "orphanPvcCleanup.alertBefore must be at least 1h",
                Box::new(|c| c.orphan_pvc_cleanup.alert_before == Duration::from_hours(1)),
            ),
            (
                with("orphanPvcCleanup:\n  reasons: []\n"),
                "orphanPvcCleanup.reasons has no valid entry",
                Box::new(|c| c.orphan_pvc_cleanup.reasons.len() == 3),
            ),
            (
                with("orphanPvcCleanup:\n  reasons: [released, unbound]\n"),
                "invalid orphanPvcCleanup.reasons entry \"released\"",
                Box::new(|c| c.orphan_pvc_cleanup.reasons == vec!["unbound"]),
            ),
            (
                with("orphanPvcCleanup:\n  maxDeletionsPerPass: 0\n"),
                "orphanPvcCleanup.maxDeletionsPerPass must be between 1 and 100",
                Box::new(|c| c.orphan_pvc_cleanup.max_deletions_per_pass == 1),
            ),
            (
                with("orphanPvcCleanup:\n  maxDeletionsPerPass: 101\n"),
                "orphanPvcCleanup.maxDeletionsPerPass must be between 1 and 100",
                Box::new(|c| c.orphan_pvc_cleanup.max_deletions_per_pass == 100),
            ),
            (
                with("orphanPvcCleanup:\n  includeNamespaces: [a]\n  excludeNamespaces: [a]\n"),
                "is both included and excluded, excluded wins",
                Box::new(|c| c.orphan_pvc_cleanup.exclude_namespaces == vec!["a"]),
            ),
        ];
        for (raw, want, check) in cases {
            let c = parse(&raw, &env()).unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert!(
                c.warnings.iter().any(|w| w.contains(want)),
                "{raw}\nwant {want:?}\ngot {:?}",
                c.warnings
            );
            assert!(check(&c), "{raw}: value not corrected");
        }
        assert_eq!(
            parse(MINIMAL, &env()).unwrap().warnings,
            Vec::<String>::new()
        );
    }

    #[test]
    fn grafana_enabled_without_token() {
        let raw = with("grafanaAnnotation:\n  enabled: true\n  url: http://g\n");
        let c = parse(&raw, &env()).unwrap();
        assert!(!c.grafana_annotation_enabled);
        assert!(
            c.warnings[0].contains("GRAFANA_API_TOKEN is required"),
            "{:?}",
            c.warnings
        );
        let e = Env {
            grafana_api_token: "t".into(),
            ..Env::default()
        };
        assert!(parse(&raw, &e).unwrap().grafana_annotation_enabled);
    }

    #[test]
    fn load_reads_file_and_reports_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, MINIMAL).unwrap();
        let c = load(&path, &env()).unwrap();
        assert_eq!(c.region, "ap-northeast-2");
        let err = load(&dir.path().join("missing.yaml"), &env()).unwrap_err();
        assert!(matches!(err, ConfigError::Read { .. }));
        assert!(err.to_string().contains("read config file"));
        std::fs::write(&path, "region: [\n").unwrap();
        let err = load(&path, &env()).unwrap_err().to_string();
        assert!(
            err.contains("parse config file") && err.contains("config.yaml"),
            "{err}"
        );
    }

    #[test]
    fn env_overrides_log_settings() {
        let e = Env {
            log_level: Some("warn".into()),
            log_format: Some("text".into()),
            ..Env::default()
        };
        let c = parse(MINIMAL, &e).unwrap();
        assert_eq!(c.log_level, "warn");
        assert_eq!(c.log_format, "text");
    }

    #[test]
    fn example_config_loads() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.yaml");
        let c = load(&path, &env()).unwrap();
        assert!(c.dry_run);
    }

    #[test]
    fn resolve_path_prefers_flag() {
        assert_eq!(resolve_path(Some("/x.yaml")), "/x.yaml");
        // With no flag the result is either CONFIG_FILE or the default.
        let p = resolve_path(None);
        assert!(!p.is_empty(), "p is empty");
    }
}
