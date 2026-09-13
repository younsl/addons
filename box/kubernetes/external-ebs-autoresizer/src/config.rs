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

pub use parse::parse_grow_amount;

/// The config file path used when `CONFIG_FILE` is unset.
pub const DEFAULT_CONFIG_FILE: &str = "/etc/external-ebs-autoresizer/config.yaml";

/// Alertmanager notify-on and Grafana annotate-on policy values.
pub const NOTIFY_ON_ALL: &str = "all";
pub const NOTIFY_ON_SUCCESS: &str = "success";
pub const NOTIFY_ON_FAILURE: &str = "failure";
pub const ANNOTATE_ON_ALL: &str = "all";
pub const ANNOTATE_ON_SUCCESS: &str = "success";
pub const ANNOTATE_ON_FAILURE: &str = "failure";

/// Grow mode values selecting how the resize target size is computed.
pub const GROW_MODE_PERCENT: &str = "percent";
pub const GROW_MODE_ABSOLUTE: &str = "absolute";

/// The built-in defaults of the optional `defaultPolicy` fields.
const DEFAULT_GROW_PERCENT: i32 = 10;
const DEFAULT_GROW_AMOUNT: &str = "10GiB";
const DEFAULT_MAX_VOLUME_SIZE_GIB: i32 = 1000;

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

/// A single EC2 tag key/value used to scope target instances.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagFilter {
    pub key: String,
    pub value: String,
}

/// Scopes a resize policy to a group of instances. Both criteria must match
/// (AND); at least one must be set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase", default)]
pub struct InstanceSelector {
    /// Instances carrying every listed tag key with the exact value.
    pub tags: BTreeMap<String, String>,
    /// Matched against the instance Name tag using RE2 regexp syntax.
    /// Unanchored: anchor with `^` and `$` for exact-name matching.
    pub name_regex: String,
}

/// The volume-expansion settings block, used both as the global `defaultPolicy`
/// and as a per-policy override. Every field is optional at the type level, so
/// "declared" is uniformly distinguishable from "omitted" across the whole
/// policy engine. Which fields are required versus optional is decided by the
/// consumer: as `defaultPolicy`, `usage_threshold_percent` and `grow_mode` are
/// REQUIRED and the rest fall back to built-in defaults; as a per-policy
/// override, every field inherits the effective `defaultPolicy` value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase", default)]
pub struct ResizeSpec {
    /// When true, stops the resizer from touching matching instances: they are
    /// skipped with reason "paused" and never measured or resized.
    pub paused: Option<bool>,
    /// When false, suppresses Alertmanager alerts for resize outcomes on
    /// matching instances. The global `alertmanager.enabled` switch remains the
    /// master gate. Defaults to true.
    pub alert_enabled: Option<bool>,
    pub usage_threshold_percent: Option<i32>,
    pub grow_mode: Option<String>,
    pub grow_percent: Option<i32>,
    pub grow_amount: Option<String>,
    #[serde(rename = "maxVolumeSizeGiB")]
    pub max_volume_size_gib: Option<i32>,
}

/// One per-instance-group override entry. The policy module validates and
/// compiles these into an effective settings resolver.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase", default)]
pub struct ResizePolicy {
    /// Identifies the policy in logs and metrics. Required, unique.
    pub name: String,
    /// Breaks ties when multiple policies match an instance: the highest
    /// weight wins; equal weights fall back to file order (earlier wins).
    pub weight: i32,
    pub instance_selector: InstanceSelector,
    /// Overrides the `defaultPolicy` settings for this group. Every field is
    /// optional; an omitted field inherits from `defaultPolicy`.
    pub resize: ResizeSpec,
}

/// The settings of the node EBS throughput recommender. It is a separate
/// subsystem from the resizer: it reads a Prometheus-compatible metrics backend
/// and the Kubernetes API, targets the in-cluster Nodes rather than the
/// standalone EC2 instances the resizer manages, and never mutates an EBS
/// volume. Disabled by default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThroughputRecommendation {
    pub enabled: bool,
    /// The base URL of a Prometheus server or a Mimir query-frontend/gateway.
    /// The `/prometheus` API prefix may be included or omitted; the client
    /// probes for it at startup.
    pub prometheus_url: String,
    /// Sent as the `X-Scope-OrgID` header, which Mimir requires when
    /// multi-tenancy is enabled. Empty for Prometheus, which ignores it.
    pub prometheus_tenant_id: String,
    /// Sent as an `Authorization: Bearer` header. Read from the environment
    /// only (never the config file) so the token stays out of the `ConfigMap`.
    pub prometheus_bearer_token: String,
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

/// All runtime settings for the resizer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Config {
    pub region: String,
    /// Which standalone EC2 instances are managed. When empty, every running
    /// instance in the account/region is a candidate (subject to
    /// `exclude_eks_nodes`).
    pub tag_filters: Vec<TagFilter>,
    /// Drops instances that belong to an EKS cluster from the candidate set.
    pub exclude_eks_nodes: bool,
    pub reconcile_interval: Duration,
    /// Bounds how many instances are reconciled in parallel within one pass.
    pub reconcile_concurrency: usize,
    /// The delay between status polls for SSM command invocations and EBS
    /// volume modifications.
    pub ssm_poll_interval: Duration,
    pub usage_threshold_percent: i32,
    /// `percent` grows the volume relative to its current size by
    /// `grow_percent`, `absolute` grows it by a fixed amount (`grow_amount`).
    pub grow_mode: String,
    pub grow_percent: i32,
    /// The raw fixed growth per resize with a MiB or GiB unit.
    pub grow_amount: String,
    /// `grow_amount` parsed and rounded up to whole GiB.
    pub grow_amount_gib: i32,
    /// A safety ceiling; resizes that would exceed it are skipped.
    pub max_volume_size_gib: i32,
    /// The default-policy pause switch.
    pub paused: bool,
    /// The default-policy alert switch. Only consulted when
    /// `alertmanager_enabled` is true.
    pub alert_enabled: bool,
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
    /// The per-instance-group resize overrides, in file order.
    pub policies: Vec<ResizePolicy>,
}

/// The on-disk YAML shape. Durations are strings (Go duration syntax) parsed
/// during `load`. Every optional key carries its default here, so a key
/// omitted from the file keeps its default and a key present overrides it,
/// including with an explicit zero. Parsing is strict: an unknown key is an
/// error.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase", default)]
struct FileSchema {
    region: String,
    tag_filters: String,
    #[serde(rename = "excludeEKSNodes")]
    exclude_eks_nodes: bool,
    reconcile_interval: String,
    reconcile_concurrency: i64,
    #[serde(rename = "ssmPollInterval")]
    ssm_poll_interval: String,
    default_policy: ResizeSpec,
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
    policies: Vec<ResizePolicy>,
}

impl Default for FileSchema {
    fn default() -> Self {
        Self {
            region: String::new(),
            tag_filters: String::new(),
            exclude_eks_nodes: true,
            reconcile_interval: "5m".into(),
            reconcile_concurrency: 10,
            ssm_poll_interval: "1s".into(),
            // The two required fields (usageThresholdPercent, growMode) stay
            // unset so parse can reject a config that omits them; every other
            // defaultPolicy field falls back to a built-in default in parse.
            default_policy: ResizeSpec::default(),
            ssm_command_timeout: "5m".into(),
            volume_modify_timeout: "10m".into(),
            dry_run: false,
            health_port: 8080,
            metrics_port: 8081,
            leader_elect: true,
            lease_name: "external-ebs-autoresizer".into(),
            log_level: "info".into(),
            log_format: "json".into(),
            alertmanager: AlertmanagerFile::default(),
            grafana_annotation: GrafanaFile::default(),
            throughput_recommendation: ThroughputFile::default(),
            policies: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase", default)]
struct AlertmanagerFile {
    enabled: bool,
    url: String,
    timeout: String,
    labels: BTreeMap<String, String>,
    notify_on: String,
    dashboard_url: String,
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
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase", default)]
struct GrafanaFile {
    enabled: bool,
    url: String,
    timeout: String,
    tags: Vec<String>,
    annotate_on: String,
}

impl Default for GrafanaFile {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            timeout: "5s".into(),
            tags: vec!["event:ebs-resize".into()],
            annotate_on: ANNOTATE_ON_ALL.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase", default)]
struct ThroughputFile {
    enabled: bool,
    prometheus_url: String,
    prometheus_tenant_id: String,
    interval: String,
    metric_node_name_label: String,
    lookback_window: String,
    apply_on_resize: bool,
}

impl Default for ThroughputFile {
    fn default() -> Self {
        Self {
            enabled: false,
            prometheus_url: String::new(),
            prometheus_tenant_id: String::new(),
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

/// Parses and validates a config document. `load` wraps it with the file path.
pub fn parse(raw: &str, env: &Env) -> Result<Config, ConfigError> {
    let f: FileSchema = serde_yaml::from_str(raw).map_err(|source| ConfigError::Parse {
        path: String::new(),
        source,
    })?;

    // defaultPolicy.usageThresholdPercent and growMode are required: they set
    // the baseline every unmatched instance uses, so they must be an explicit
    // choice rather than an implicit default.
    let dp = &f.default_policy;
    let Some(threshold) = dp.usage_threshold_percent else {
        return Err(ConfigError::Invalid(
            "defaultPolicy.usageThresholdPercent is required".into(),
        ));
    };
    let grow_mode = match dp.grow_mode.as_deref().map(str::trim) {
        Some(m) if !m.is_empty() => m.to_ascii_lowercase(),
        _ => {
            return Err(ConfigError::Invalid(
                "defaultPolicy.growMode is required".into(),
            ));
        }
    };

    let tr = &f.throughput_recommendation;
    let mut c = Config {
        region: f.region.clone(),
        tag_filters: parse::parse_tag_filters(&f.tag_filters)?,
        exclude_eks_nodes: f.exclude_eks_nodes,
        reconcile_interval: parse::parse_duration("reconcileInterval", &f.reconcile_interval)?,
        reconcile_concurrency: usize::try_from(f.reconcile_concurrency).unwrap_or(0),
        ssm_poll_interval: parse::parse_duration("ssmPollInterval", &f.ssm_poll_interval)?,
        usage_threshold_percent: threshold,
        grow_mode,
        grow_percent: dp.grow_percent.unwrap_or(DEFAULT_GROW_PERCENT),
        grow_amount: dp
            .grow_amount
            .clone()
            .unwrap_or_else(|| DEFAULT_GROW_AMOUNT.into()),
        grow_amount_gib: 0,
        max_volume_size_gib: dp
            .max_volume_size_gib
            .unwrap_or(DEFAULT_MAX_VOLUME_SIZE_GIB),
        paused: dp.paused.unwrap_or(false),
        alert_enabled: dp.alert_enabled.unwrap_or(true),
        ssm_command_timeout: parse::parse_duration("ssmCommandTimeout", &f.ssm_command_timeout)?,
        volume_modify_timeout: parse::parse_duration(
            "volumeModifyTimeout",
            &f.volume_modify_timeout,
        )?,
        dry_run: f.dry_run,
        health_port: f.health_port,
        metrics_port: f.metrics_port,
        log_level: f.log_level.clone(),
        log_format: f.log_format.clone(),
        pod_name: env.pod_name.clone(),
        pod_namespace: env.pod_namespace.clone(),
        pod_uid: env.pod_uid.clone(),
        leader_elect: f.leader_elect,
        lease_name: f.lease_name.clone(),
        alertmanager_enabled: f.alertmanager.enabled,
        alertmanager_url: f.alertmanager.url.clone(),
        alertmanager_timeout: parse::parse_duration(
            "alertmanager.timeout",
            &f.alertmanager.timeout,
        )?,
        alertmanager_labels: f.alertmanager.labels.clone(),
        alertmanager_notify_on: f.alertmanager.notify_on.clone(),
        alertmanager_dashboard_url: f.alertmanager.dashboard_url.clone(),
        grafana_annotation_enabled: f.grafana_annotation.enabled,
        grafana_url: f.grafana_annotation.url.clone(),
        grafana_api_token: env.grafana_api_token.clone(),
        grafana_timeout: parse::parse_duration(
            "grafanaAnnotation.timeout",
            &f.grafana_annotation.timeout,
        )?,
        grafana_annotation_tags: f.grafana_annotation.tags.clone(),
        grafana_annotate_on: f.grafana_annotation.annotate_on.clone(),
        throughput_recommendation: ThroughputRecommendation {
            enabled: tr.enabled,
            prometheus_url: tr.prometheus_url.trim().to_string(),
            prometheus_tenant_id: tr.prometheus_tenant_id.trim().to_string(),
            prometheus_bearer_token: env.prometheus_bearer_token.clone(),
            interval: parse::parse_duration("throughputRecommendation.interval", &tr.interval)?,
            metric_node_name_label: tr.metric_node_name_label.clone(),
            lookback_window: String::new(),
            lookback_duration: Duration::ZERO,
            apply_on_resize: tr.apply_on_resize,
        },
        policies: f.policies.clone(),
    };

    // The observation window is a Prometheus duration, not a Go duration: "7d"
    // is valid PromQL and invalid Go, while "1.5h" is the reverse.
    let (window, window_duration) = parse::parse_prom_duration(
        "throughputRecommendation.lookbackWindow",
        &tr.lookback_window,
    )?;
    c.throughput_recommendation.lookback_window = window;
    c.throughput_recommendation.lookback_duration = window_duration;

    // growAmount is always parsed (not only in absolute mode) so per-group
    // policies that switch to absolute mode inherit a usable default amount,
    // and a malformed value fails at startup regardless of the active mode.
    c.grow_amount_gib = parse_grow_amount(&c.grow_amount)
        .map_err(|err| ConfigError::Invalid(format!("invalid growAmount: {err}")))?;

    c.validate()?;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "region: ap-northeast-2\ndefaultPolicy:\n  usageThresholdPercent: 80\n  growMode: percent\n";

    fn env() -> Env {
        Env::default()
    }

    #[test]
    fn defaults() {
        let c = parse(MINIMAL, &env()).unwrap();
        assert_eq!(c.region, "ap-northeast-2");
        assert!(c.tag_filters.is_empty());
        assert!(c.exclude_eks_nodes);
        assert_eq!(c.reconcile_interval, Duration::from_mins(5));
        assert_eq!(c.reconcile_concurrency, 10);
        assert_eq!(c.ssm_poll_interval, Duration::from_secs(1));
        assert_eq!(c.usage_threshold_percent, 80);
        assert_eq!(c.grow_mode, GROW_MODE_PERCENT);
        assert_eq!(c.grow_percent, 10);
        assert_eq!(c.grow_amount, "10GiB");
        assert_eq!(c.grow_amount_gib, 10);
        assert_eq!(c.max_volume_size_gib, 1000);
        assert!(!c.paused);
        assert!(c.alert_enabled);
        assert_eq!(c.ssm_command_timeout, Duration::from_mins(5));
        assert_eq!(c.volume_modify_timeout, Duration::from_mins(10));
        assert!(!c.dry_run);
        assert_eq!(c.health_port, 8080);
        assert_eq!(c.metrics_port, 8081);
        assert!(c.leader_elect);
        assert_eq!(c.lease_name, "external-ebs-autoresizer");
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
        assert!(c.policies.is_empty());
        assert!(c.pod_name.is_empty());
    }

    #[test]
    fn required_default_policy_fields() {
        let err = parse("region: r\ndefaultPolicy:\n  growMode: percent\n", &env())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("defaultPolicy.usageThresholdPercent is required"),
            "{err}"
        );
        let err = parse(
            "region: r\ndefaultPolicy:\n  usageThresholdPercent: 80\n",
            &env(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("defaultPolicy.growMode is required"), "{err}");
        let err = parse(
            "region: r\ndefaultPolicy:\n  usageThresholdPercent: 80\n  growMode: \"  \"\n",
            &env(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("defaultPolicy.growMode is required"), "{err}");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn overrides_and_explicit_zeros() {
        let raw = r#"
region: us-east-1
tagFilters: "Env=prod,Team=infra"
excludeEKSNodes: false
reconcileInterval: 1h30m
reconcileConcurrency: 3
ssmPollInterval: 500ms
defaultPolicy:
  usageThresholdPercent: 0
  growMode: " ABSOLUTE "
  paused: true
  alertEnabled: false
  growPercent: 25
  growAmount: 5120MiB
  maxVolumeSizeGiB: 2000
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
throughputRecommendation:
  enabled: true
  prometheusUrl: " http://mimir/prometheus "
  prometheusTenantId: " tenant "
  interval: 1h
  metricNodeNameLabel: instance
  lookbackWindow: 12h
  applyOnResize: false
policies:
  - name: db
    weight: 10
    instanceSelector:
      tags:
        Role: database
      nameRegex: "^prod-"
    resize:
      usageThresholdPercent: 70
      growMode: absolute
      growAmount: 50GiB
      maxVolumeSizeGiB: 3000
      paused: false
      alertEnabled: true
      growPercent: 5
"#;
        let e = Env {
            pod_name: "pod-0".into(),
            pod_namespace: "kube-system".into(),
            pod_uid: "uid".into(),
            grafana_api_token: "tok".into(),
            prometheus_bearer_token: "bearer".into(),
        };
        let c = parse(raw, &e).unwrap();
        assert_eq!(c.tag_filters.len(), 2);
        assert!(!c.exclude_eks_nodes);
        assert_eq!(c.reconcile_interval, Duration::from_mins(90));
        assert_eq!(c.reconcile_concurrency, 3);
        assert_eq!(c.ssm_poll_interval, Duration::from_millis(500));
        assert_eq!(c.usage_threshold_percent, 0, "explicit zero survives");
        assert_eq!(c.grow_mode, GROW_MODE_ABSOLUTE, "normalized");
        assert!(c.paused);
        assert!(!c.alert_enabled);
        assert_eq!(c.grow_percent, 25);
        assert_eq!(c.grow_amount_gib, 5);
        assert_eq!(c.max_volume_size_gib, 2000);
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
        assert_eq!(tr.prometheus_url, "http://mimir/prometheus");
        assert_eq!(tr.prometheus_tenant_id, "tenant");
        assert_eq!(tr.prometheus_bearer_token, "bearer");
        assert_eq!(tr.interval, Duration::from_hours(1));
        assert_eq!(tr.metric_node_name_label, "instance");
        assert_eq!(tr.lookback_window, "12h");
        assert_eq!(tr.lookback_duration, Duration::from_hours(12));
        assert!(!tr.apply_on_resize);
        assert_eq!(c.pod_name, "pod-0");
        assert_eq!(c.pod_namespace, "kube-system");
        assert_eq!(c.pod_uid, "uid");
        assert_eq!(c.policies.len(), 1);
        let p = &c.policies[0];
        assert_eq!(p.name, "db");
        assert_eq!(p.weight, 10);
        assert_eq!(p.instance_selector.tags.get("Role").unwrap(), "database");
        assert_eq!(p.instance_selector.name_regex, "^prod-");
        assert_eq!(p.resize.usage_threshold_percent, Some(70));
        assert_eq!(p.resize.grow_mode.as_deref(), Some("absolute"));
        assert_eq!(p.resize.grow_amount.as_deref(), Some("50GiB"));
        assert_eq!(p.resize.max_volume_size_gib, Some(3000));
        assert_eq!(p.resize.paused, Some(false));
        assert_eq!(p.resize.alert_enabled, Some(true));
        assert_eq!(p.resize.grow_percent, Some(5));
    }

    #[test]
    fn policy_omitted_fields_stay_unset() {
        let raw =
            format!("{MINIMAL}policies:\n  - name: a\n    instanceSelector:\n      nameRegex: x\n");
        let c = parse(&raw, &env()).unwrap();
        assert_eq!(c.policies[0].resize, ResizeSpec::default());
        assert_eq!(c.policies[0].weight, 0);
    }

    #[test]
    fn unknown_keys_fail() {
        for raw in [
            format!("{MINIMAL}bogus: 1\n"),
            format!("{MINIMAL}alertmanager:\n  bogus: 1\n"),
            format!("{MINIMAL}throughputRecommendation:\n  quantile: 0.9\n"),
            format!("{MINIMAL}policies:\n  - name: a\n    bogus: 1\n"),
        ] {
            let err = parse(&raw, &env()).unwrap_err();
            assert!(matches!(err, ConfigError::Parse { .. }), "{raw}: {err}");
            assert!(err.to_string().contains("parse config file"), "{err}");
        }
    }

    #[test]
    fn validation_errors() {
        let cases: Vec<(String, &str)> = vec![
            (
                "defaultPolicy:\n  usageThresholdPercent: 80\n  growMode: percent\n".into(),
                "region is required",
            ),
            (
                "region: r\ndefaultPolicy:\n  usageThresholdPercent: 101\n  growMode: percent\n"
                    .into(),
                "usageThresholdPercent must be between 0 and 100",
            ),
            (
                format!("{MINIMAL}reconcileInterval: 5min\n"),
                "invalid reconcileInterval",
            ),
            (format!("{MINIMAL}reconcileInterval: 0s\n"), "reconcileInterval must be greater than 0"),
            (format!("{MINIMAL}reconcileConcurrency: 0\n"), "reconcileConcurrency must be greater than 0"),
            (format!("{MINIMAL}reconcileConcurrency: -1\n"), "reconcileConcurrency must be greater than 0"),
            (format!("{MINIMAL}ssmPollInterval: 0\n"), "ssmPollInterval must be greater than 0"),
            (
                "region: r\ndefaultPolicy:\n  usageThresholdPercent: 80\n  growMode: sideways\n".into(),
                "growMode must be one of percent, absolute",
            ),
            (
                "region: r\ndefaultPolicy:\n  usageThresholdPercent: 80\n  growMode: percent\n  growPercent: 0\n".into(),
                "growPercent must be greater than 0",
            ),
            (
                "region: r\ndefaultPolicy:\n  usageThresholdPercent: 80\n  growMode: absolute\n  growAmount: 10GB\n".into(),
                "invalid growAmount",
            ),
            (
                "region: r\ndefaultPolicy:\n  usageThresholdPercent: 80\n  growMode: percent\n  maxVolumeSizeGiB: 0\n".into(),
                "maxVolumeSizeGiB must be greater than 0",
            ),
            (format!("{MINIMAL}alertmanager:\n  notifyOn: never\n"), "alertmanager.notifyOn must be one of"),
            (format!("{MINIMAL}alertmanager:\n  enabled: true\n"), "alertmanager.url is required"),
            (format!("{MINIMAL}alertmanager:\n  timeout: bogus\n"), "invalid alertmanager.timeout"),
            (format!("{MINIMAL}grafanaAnnotation:\n  annotateOn: never\n"), "grafanaAnnotation.annotateOn must be one of"),
            (format!("{MINIMAL}grafanaAnnotation:\n  enabled: true\n"), "grafanaAnnotation.url is required"),
            (format!("{MINIMAL}grafanaAnnotation:\n  timeout: bogus\n"), "invalid grafanaAnnotation.timeout"),
            (format!("{MINIMAL}throughputRecommendation:\n  enabled: true\n"), "throughputRecommendation.prometheusUrl is required"),
            (format!("{MINIMAL}throughputRecommendation:\n  interval: 0s\n"), "throughputRecommendation.interval must be greater than 0"),
            (format!("{MINIMAL}throughputRecommendation:\n  interval: soon\n"), "invalid throughputRecommendation.interval"),
            (format!("{MINIMAL}throughputRecommendation:\n  metricNodeNameLabel: \"no-de\"\n"), "invalid throughputRecommendation.metricNodeNameLabel"),
            (format!("{MINIMAL}throughputRecommendation:\n  lookbackWindow: 1.5h\n"), "invalid throughputRecommendation.lookbackWindow"),
            (format!("{MINIMAL}tagFilters: \"Env\"\n"), "invalid tag filter"),
            (format!("{MINIMAL}ssmCommandTimeout: x\n"), "invalid ssmCommandTimeout"),
            (format!("{MINIMAL}volumeModifyTimeout: x\n"), "invalid volumeModifyTimeout"),
        ];
        for (raw, want) in cases {
            let err = parse(&raw, &env()).unwrap_err().to_string();
            assert!(err.contains(want), "{raw}\nwant {want:?}\ngot {err}");
        }
    }

    #[test]
    fn grafana_enabled_without_token() {
        let raw = format!("{MINIMAL}grafanaAnnotation:\n  enabled: true\n  url: http://g\n");
        let err = parse(&raw, &env()).unwrap_err().to_string();
        assert!(err.contains("GRAFANA_API_TOKEN is required"), "{err}");
        let e = Env {
            grafana_api_token: "t".into(),
            ..Env::default()
        };
        assert!(parse(&raw, &e).is_ok());
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
    fn example_config_loads() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.yaml");
        let c = load(&path, &env()).unwrap();
        assert_eq!(c.policies.len(), 2);
        assert!(c.dry_run);
    }

    #[test]
    fn resolve_path_prefers_flag() {
        assert_eq!(resolve_path(Some("/x.yaml")), "/x.yaml");
        // With no flag the result is either CONFIG_FILE or the default.
        let p = resolve_path(None);
        assert!(!p.is_empty());
    }
}
