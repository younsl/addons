//! Loads and validates runtime configuration from a single YAML file. The
//! Slack tokens (Secret) and Pod identity (downward API) come from the
//! environment instead, since neither belongs in a plain `ConfigMap`.

pub mod validate;

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::Duration as StdDuration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::humanize;
use crate::promx::gate::{DEFAULT_PERCENTILE, OnError};

/// The config file path used when `CONFIG_FILE` is unset.
pub const DEFAULT_CONFIG_FILE: &str = "/etc/aws-vpn-maintenance-handler/config.yaml";

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
    /// Every validation problem, one per line, so a broken file is fixed in one
    /// round trip rather than one error at a time.
    #[error("{}", .0.join("\n"))]
    Invalid(Vec<String>),
}

/// A `std::time::Duration` that unmarshals from a YAML string like `"5m"`, so
/// durations are written with units.
///
/// A null or empty value leaves the default in place. The Helm chart declares
/// the tunable durations with no value so they are visible in values.yaml
/// without being set, and that renders as null; treating it as "unset" is what
/// makes the listing and the default agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Duration(pub StdDuration);

impl Duration {
    #[must_use]
    pub const fn secs(s: u64) -> Self {
        Self(StdDuration::from_secs(s))
    }

    #[must_use]
    pub const fn get(self) -> StdDuration {
        self.0
    }

    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0.is_zero()
    }
}

impl From<StdDuration> for Duration {
    fn from(d: StdDuration) -> Self {
        Self(d)
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&humanize::go_duration(self.0))
    }
}

impl Serialize for Duration {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

/// Deserializes over the default: an absent, null, or empty value leaves the
/// field alone. Implemented as a field-level helper because serde has no
/// "null means default" for a newtype on its own.
fn duration_or_default<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
    let raw: Option<serde_yaml::Value> = Option::deserialize(d)?;
    match raw {
        None | Some(serde_yaml::Value::Null) => Ok(None),
        Some(serde_yaml::Value::String(s)) if s.is_empty() => Ok(None),
        Some(serde_yaml::Value::String(s)) => humantime::parse_duration(&s)
            .map(|d| Some(Duration(d)))
            .map_err(|err| serde::de::Error::custom(format!("invalid duration {s:?}: {err}"))),
        Some(other) => Err(serde::de::Error::custom(format!(
            "duration must be a quoted string like \"5m\", got {other:?}"
        ))),
    }
}

impl<'de> Deserialize<'de> for Duration {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        duration_or_default(d).map(Option::unwrap_or_default)
    }
}

/// A single EC2 tag key/value pair used to scope target VPN connections. An
/// empty value matches any value for that key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TagFilter {
    pub key: String,
    #[serde(default)]
    pub value: String,
}

/// Selects which VPN connections this controller owns. Opting in by tag is
/// deliberate: a new VPN is never eligible until someone tags it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Targets {
    /// All must match (AND). Required and non-empty.
    #[serde(default)]
    pub tag_filters: Vec<TagFilter>,
    /// Drops specific connections even if their tags match.
    #[serde(default, rename = "excludeConnectionIDs")]
    pub exclude_connection_ids: Vec<String>,
}

/// The maintenance window during which replacements may start. Each firing of
/// `cron_schedule` opens the window for `duration`; cron alone only names
/// instants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase", default)]
pub struct Window {
    /// An IANA name (e.g. Asia/Seoul), so the window follows DST.
    pub timezone: String,
    /// A standard 5-field expression: minute hour dom month dow.
    pub cron_schedule: String,
    /// How long the window stays open after each firing.
    pub duration: Duration,
    /// Refuses to start with less than this much window left, so verification
    /// finishes before it closes. Left unset it becomes `safety.verifyTimeout`,
    /// which is the only value that makes the guarantee it exists for.
    pub min_remaining: Duration,
}

/// The preflight and verification thresholds that make an irreversible
/// `ReplaceVpnTunnel` call safe to automate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase", default)]
pub struct Safety {
    /// Requires the surviving tunnel to have held UP this long, since a peer
    /// that just came up may be flapping. Also the wait the sibling tunnel
    /// serves before it may be chained.
    pub peer_min_stable_for: Duration,
    /// The minimum BGP route count on the surviving tunnel. Skipped on
    /// static-routes-only connections.
    pub peer_min_accepted_routes: i32,
    /// Blocks a second replacement on the same connection.
    pub per_connection_cooldown: Duration,
    /// Lets the connection's other tunnel skip the cooldown once the first was
    /// replaced successfully, so both tunnels finish in one window. The peer
    /// checks still gate it. A replacement that ended badly is never chained
    /// from.
    pub chain_sibling_tunnel: bool,
    /// Bounds the wait for the tunnel to come back UP.
    pub verify_timeout: Duration,
    /// The delay between telemetry polls while verifying.
    pub verify_poll_interval: Duration,
    /// Raises severity once the AWS auto-apply deadline is nearer than this.
    pub escalate_before: Duration,
}

/// The human gate in front of every replacement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase", default)]
pub struct Approval {
    /// Receive the DM and may approve. Required; user IDs (Uxxxxxxxx), not
    /// display names.
    #[serde(rename = "slackUserIDs")]
    pub slack_user_ids: Vec<String>,
    /// Expires an unanswered request; the tunnel is left alone.
    pub timeout: Duration,
    /// How often to post a "still waiting" thread update while verifying.
    pub progress_heartbeat: Duration,
}

/// Consults Prometheus or Mimir so a replacement only runs while the tunnel is
/// actually quiet. Only the window and one percentile are configured;
/// everything else is measured rather than declared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase", default)]
pub struct TrafficGate {
    pub enabled: bool,
    /// The query API base URL, the part before `/api/v1/query`.
    pub endpoint: String,
    /// Sent with every query, for tenant selectors like `X-Scope-OrgID`.
    pub headers: BTreeMap<String, String>,
    /// Bounds a single query.
    pub timeout: Duration,
    /// The share of this connection's own traffic during past maintenance
    /// windows that counts as quiet.
    pub quiet_percentile: f64,
    /// `block` or `allow`: what an unreadable metric source means.
    pub on_error: String,
}

/// All runtime settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase", default)]
pub struct Config {
    /// The AWS region to operate in. Required.
    pub region: String,
    /// How often to poll VPN telemetry and maintenance status.
    pub reconcile_interval: Duration,
    /// Asks for approval as usual, but sends the AWS `DryRun` flag.
    pub dry_run: bool,

    pub targets: Targets,
    pub maintenance_window: Window,
    pub safety: Safety,
    pub approval: Approval,
    pub traffic_gate: TrafficGate,

    /// Reconciles only on the Lease holder. Required above one replica.
    pub leader_elect: bool,
    pub lease_name: String,
    /// Holds in-flight and cooldown state, so a restart neither loses a running
    /// replacement nor re-proposes one it just made.
    pub state_config_map_name: String,

    pub health_port: u16,
    pub metrics_port: u16,
    pub log_level: String,
    pub log_format: String,

    // Runtime-injected, not part of the YAML file.
    /// `xoxb-` token, from `SLACK_BOT_TOKEN`.
    #[serde(skip)]
    pub slack_bot_token: String,
    /// `xapp-` token that opens Socket Mode, from `SLACK_APP_TOKEN`.
    #[serde(skip)]
    pub slack_app_token: String,
    #[serde(skip)]
    pub pod_name: String,
    #[serde(skip)]
    pub pod_namespace: String,
    #[serde(skip)]
    pub pod_uid: String,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            timezone: "UTC".into(),
            cron_schedule: "0 2 * * *".into(),
            duration: Duration::secs(3 * 3600),
            min_remaining: Duration::default(),
        }
    }
}

impl Default for Safety {
    fn default() -> Self {
        Self {
            peer_min_stable_for: Duration::secs(5 * 60),
            peer_min_accepted_routes: 1,
            per_connection_cooldown: Duration::secs(24 * 3600),
            chain_sibling_tunnel: true,
            verify_timeout: Duration::secs(30 * 60),
            verify_poll_interval: Duration::secs(10),
            // A week, because the useful signal is "this needs a window soon"
            // rather than "this is about to happen".
            escalate_before: Duration::secs(168 * 3600),
        }
    }
}

impl Default for Approval {
    fn default() -> Self {
        Self {
            slack_user_ids: Vec::new(),
            timeout: Duration::secs(3600),
            progress_heartbeat: Duration::secs(5 * 60),
        }
    }
}

impl Default for TrafficGate {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: String::new(),
            headers: BTreeMap::new(),
            timeout: Duration::secs(10),
            quiet_percentile: DEFAULT_PERCENTILE,
            // Unreadable metrics mean no evidence the tunnel is quiet.
            on_error: OnError::Block.as_str().to_string(),
        }
    }
}

/// Fails closed on purpose: dry run on, a 24h cooldown, and a 5m peer
/// stability requirement.
impl Default for Config {
    fn default() -> Self {
        Self {
            region: String::new(),
            reconcile_interval: Duration::secs(5 * 60),
            dry_run: true,
            targets: Targets::default(),
            maintenance_window: Window::default(),
            safety: Safety::default(),
            approval: Approval::default(),
            traffic_gate: TrafficGate::default(),
            leader_elect: true,
            lease_name: "aws-vpn-maintenance-handler".into(),
            state_config_map_name: "aws-vpn-maintenance-handler-state".into(),
            health_port: 8081,
            metrics_port: 9090,
            log_level: "info".into(),
            log_format: "json".into(),
            slack_bot_token: String::new(),
            slack_app_token: String::new(),
            pod_name: String::new(),
            pod_namespace: String::new(),
            pod_uid: String::new(),
        }
    }
}

/// Runtime values that come from the environment rather than the file.
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub slack_bot_token: String,
    pub slack_app_token: String,
    pub pod_name: String,
    pub pod_namespace: String,
    pub pod_uid: String,
    pub log_level: Option<String>,
    pub log_format: Option<String>,
    pub aws_region: Option<String>,
}

impl Env {
    /// Reads the process environment.
    #[must_use]
    pub fn from_process() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self {
            slack_bot_token: get("SLACK_BOT_TOKEN").unwrap_or_default(),
            slack_app_token: get("SLACK_APP_TOKEN").unwrap_or_default(),
            pod_name: get("POD_NAME").unwrap_or_default(),
            pod_namespace: get("POD_NAMESPACE").unwrap_or_default(),
            pod_uid: get("POD_UID").unwrap_or_default(),
            log_level: get("LOG_LEVEL"),
            log_format: get("LOG_FORMAT"),
            aws_region: get("AWS_REGION"),
        }
    }
}

/// Resolves the config file path: the flag, then `CONFIG_FILE`, then the
/// default.
#[must_use]
pub fn resolve_path(flag: Option<&str>) -> String {
    flag.filter(|p| !p.is_empty()).map_or_else(
        || {
            std::env::var("CONFIG_FILE")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_CONFIG_FILE.to_string())
        },
        str::to_string,
    )
}

/// Reads the YAML file at `path` over the defaults, applies the env-injected
/// values, and validates the result.
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

/// Parses YAML text over the defaults. Strict, so a typo in a safety threshold
/// is a startup error rather than a silently ignored field.
pub fn parse(raw: &str, env: &Env) -> Result<Config, ConfigError> {
    let parse_err = |source| ConfigError::Parse {
        path: String::new(),
        source,
    };
    let mut cfg: Config = if raw.trim().is_empty() {
        Config::default()
    } else {
        // Null and empty values are dropped before typed decoding so they leave
        // the default in place, the way the Helm chart's unset tunables expect.
        let value: serde_yaml::Value = serde_yaml::from_str(raw).map_err(parse_err)?;
        serde_yaml::from_value(prune_unset(value)).map_err(parse_err)?
    };

    cfg.slack_bot_token.clone_from(&env.slack_bot_token);
    cfg.slack_app_token.clone_from(&env.slack_app_token);
    cfg.pod_name.clone_from(&env.pod_name);
    cfg.pod_namespace.clone_from(&env.pod_namespace);
    cfg.pod_uid.clone_from(&env.pod_uid);
    if let Some(v) = &env.log_level {
        cfg.log_level.clone_from(v);
    }
    if let Some(v) = &env.log_format {
        cfg.log_format.clone_from(v);
    }
    if cfg.region.is_empty()
        && let Some(v) = &env.aws_region
    {
        cfg.region.clone_from(v);
    }

    // Derived rather than defaulted: a fixed number here would silently
    // disagree with a verifyTimeout somebody tuned.
    if cfg.maintenance_window.min_remaining.is_zero() {
        cfg.maintenance_window.min_remaining = cfg.safety.verify_timeout;
    }

    cfg.validate()?;
    Ok(cfg)
}

/// Removes mapping entries whose value is null or an empty string, recursively.
fn prune_unset(value: serde_yaml::Value) -> serde_yaml::Value {
    match value {
        serde_yaml::Value::Mapping(map) => serde_yaml::Value::Mapping(
            map.into_iter()
                .filter(|(_, v)| !matches!(v, serde_yaml::Value::Null) && v.as_str() != Some(""))
                .map(|(k, v)| (k, prune_unset(v)))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
pub fn test_env() -> Env {
    Env {
        slack_bot_token: "xoxb-test".into(),
        slack_app_token: "xapp-test".into(),
        pod_name: "pod-0".into(),
        pod_namespace: "kube-system".into(),
        pod_uid: "uid".into(),
        ..Env::default()
    }
}

#[cfg(test)]
pub const MINIMAL_YAML: &str = r#"
region: ap-northeast-2
targets:
  tagFilters:
    - key: managed
      value: "true"
approval:
  slackUserIDs: [U0123456789]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fail_closed() {
        let cfg = parse(MINIMAL_YAML, &test_env()).unwrap();
        assert!(cfg.dry_run);
        assert!(cfg.leader_elect);
        assert_eq!(
            cfg.safety.per_connection_cooldown,
            Duration::secs(24 * 3600)
        );
        assert_eq!(cfg.safety.peer_min_stable_for, Duration::secs(300));
        assert_eq!(
            cfg.maintenance_window.min_remaining,
            cfg.safety.verify_timeout
        );
        assert_eq!(cfg.traffic_gate.on_error, "block");
        assert!((cfg.traffic_gate.quiet_percentile - 20.0).abs() < f64::EPSILON);
        assert_eq!(cfg.health_port, 8081);
        assert_eq!(cfg.slack_bot_token, "xoxb-test");
    }

    #[test]
    fn parses_durations_and_env_overrides() {
        let yaml = format!(
            "{MINIMAL_YAML}\nreconcileInterval: \"1m\"\nsafety:\n  verifyTimeout: \"1h30m\"\n  verifyPollInterval: null\nmaintenanceWindow:\n  duration: \"2h\"\n  minRemaining: \"\"\n"
        );
        let env = Env {
            log_level: Some("debug".into()),
            log_format: Some("text".into()),
            ..test_env()
        };
        let cfg = parse(&yaml, &env).unwrap();
        assert_eq!(cfg.reconcile_interval, Duration::secs(60));
        assert_eq!(cfg.safety.verify_timeout, Duration::secs(90 * 60));
        assert_eq!(cfg.safety.verify_poll_interval, Duration::secs(10));
        assert_eq!(
            cfg.maintenance_window.min_remaining,
            Duration::secs(90 * 60)
        );
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(cfg.log_format, "text");
        assert_eq!(cfg.reconcile_interval.to_string(), "1m0s");
    }

    #[test]
    fn region_falls_back_to_aws_region() {
        let yaml = MINIMAL_YAML.replace("region: ap-northeast-2\n", "");
        let env = Env {
            aws_region: Some("us-east-1".into()),
            ..test_env()
        };
        assert_eq!(parse(&yaml, &env).unwrap().region, "us-east-1");
    }

    #[test]
    fn rejects_unknown_fields_and_bad_durations() {
        let err = parse(
            &format!("{MINIMAL_YAML}\nsafety:\n  peerMinStabelFor: \"5m\"\n"),
            &test_env(),
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        let err = parse(
            &format!("{MINIMAL_YAML}\nreconcileInterval: 300\n"),
            &test_env(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("quoted string"), "{err}");
        let err = parse(
            &format!("{MINIMAL_YAML}\nreconcileInterval: \"5 parsecs\"\n"),
            &test_env(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid duration"), "{err}");
    }

    #[test]
    fn load_reads_file_and_reports_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, MINIMAL_YAML).unwrap();
        assert!(load(&path, &test_env()).is_ok());
        let err = load(&dir.path().join("missing.yaml"), &test_env()).unwrap_err();
        assert!(matches!(err, ConfigError::Read { .. }));
        std::fs::write(&path, "region: [").unwrap();
        let err = load(&path, &test_env()).unwrap_err();
        assert!(err.to_string().contains("config.yaml"), "{err}");
    }

    #[test]
    fn resolve_path_prefers_flag() {
        assert_eq!(resolve_path(Some("/tmp/x.yaml")), "/tmp/x.yaml");
        // With no flag the answer is either the env var or the default, both
        // non-empty.
        assert!(!resolve_path(None).is_empty());
        assert!(!resolve_path(Some("")).is_empty());
    }

    #[test]
    fn duration_round_trips_through_serde() {
        let d: Duration = serde_yaml::from_str("\"90s\"").unwrap();
        assert_eq!(d, Duration::secs(90));
        assert_eq!(serde_yaml::to_string(&d).unwrap().trim(), "1m30s");
        let d: Duration = serde_yaml::from_str("null").unwrap();
        assert!(d.is_zero());
        assert_eq!(
            Duration::from(StdDuration::from_secs(3)).get(),
            StdDuration::from_secs(3)
        );
    }
}
