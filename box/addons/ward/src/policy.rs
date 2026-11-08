//! The wire types shared by the operator, the manager, and the agent: the
//! compiled policy the operator writes, the bundle the manager serves, and
//! the detections and heartbeats the agent sends back.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::content::YaraSource;
use crate::crd::{
    BehaviorRule, EventSource, Exclusions, FeedSpec, Indicators, Mode, WardSpec, parse_duration,
};
use crate::signature::Signature;

/// The policy file the operator writes into the policy `ConfigMap`.
pub const POLICY_FILE: &str = "policy.json";

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    pub mode: Mode,
    pub scan: ScanPolicy,
    pub behavior: BehaviorPolicy,
    pub integrity: IntegrityPolicy,
    #[serde(default)]
    pub rootcheck: RootcheckPolicy,
    pub exclusions: Exclusions,
    #[serde(default)]
    pub indicators: Indicators,
    #[serde(default)]
    pub event_source: EventSource,
    /// Publish a Kubernetes Event on the affected Pod for each detection.
    #[serde(default)]
    pub emit_events: bool,
    /// Where the signature database comes from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed: Option<FeedSpec>,
    /// Accepted `WardSignature` signatures in the namespace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_signatures: Vec<Signature>,
    /// Accepted `WardSignature` YARA rules in the namespace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_yara: Vec<YaraSource>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ScanPolicy {
    pub on_exec: bool,
    pub yara: bool,
    pub interval_secs: u64,
    pub max_file_size_bytes: u64,
    pub host_paths: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BehaviorPolicy {
    pub enabled: bool,
    pub poll_interval_ms: u64,
    pub disabled_rules: Vec<BehaviorRule>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IntegrityPolicy {
    pub enabled: bool,
    pub paths: Vec<String>,
    pub interval_secs: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RootcheckPolicy {
    pub enabled: bool,
    pub interval_secs: u64,
}

fn millis(s: &str, fallback: Duration) -> u64 {
    u64::try_from(parse_duration(s).unwrap_or(fallback).as_millis()).unwrap_or(u64::MAX)
}

fn secs(s: &str, fallback: Duration) -> u64 {
    parse_duration(s).unwrap_or(fallback).as_secs().max(1)
}

impl Policy {
    pub fn from_spec(spec: &WardSpec) -> Self {
        Self {
            mode: spec.mode,
            scan: ScanPolicy {
                on_exec: spec.scan.on_exec,
                yara: spec.scan.yara,
                interval_secs: secs(&spec.scan.interval, Duration::from_hours(6)),
                max_file_size_bytes: u64::from(spec.scan.max_file_size_mib) * 1024 * 1024,
                host_paths: spec.scan.host_paths.clone(),
            },
            behavior: BehaviorPolicy {
                enabled: spec.behavior.enabled,
                poll_interval_ms: millis(&spec.behavior.poll_interval, Duration::from_secs(1))
                    .max(100),
                disabled_rules: spec.behavior.disabled_rules.clone(),
            },
            integrity: IntegrityPolicy {
                enabled: spec.integrity.enabled,
                paths: spec.integrity.paths.clone(),
                interval_secs: secs(&spec.integrity.interval, Duration::from_mins(5)),
            },
            rootcheck: RootcheckPolicy {
                enabled: spec.rootcheck.enabled,
                interval_secs: secs(&spec.rootcheck.interval, Duration::from_hours(12)),
            },
            exclusions: spec.exclusions.clone(),
            indicators: spec.indicators.clone(),
            event_source: spec.agent.event_source,
            emit_events: spec.manager.emit_events,
            feed: spec.feed.clone(),
            custom_signatures: Vec::new(),
            custom_yara: Vec::new(),
        }
    }

    pub const fn protect(&self) -> bool {
        matches!(self.mode, Mode::Protect)
    }

    /// Whether `path` falls under an excluded prefix.
    pub fn path_excluded(&self, path: &str) -> bool {
        self.exclusions
            .paths
            .iter()
            .any(|p| !p.is_empty() && path.starts_with(p.as_str()))
    }

    pub fn namespace_excluded(&self, namespace: &str) -> bool {
        self.exclusions.namespaces.iter().any(|n| n == namespace)
    }

    /// Whether behavior rules skip the process at `exe`. Only an exact,
    /// absolute executable path matches, never `comm` or a basename, which
    /// any process can set.
    pub fn process_excluded(&self, exe: Option<&str>) -> bool {
        exe.is_some_and(|exe| {
            exe.starts_with('/') && self.exclusions.processes.iter().any(|p| p == exe)
        })
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// A short content hash, stable across key order.
    pub fn revision(&self) -> String {
        revision_of(&[self.to_json().as_bytes()])
    }
}

/// The first 12 hex digits of the SHA-256 over `parts`.
pub fn revision_of(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
        h.update([0]);
    }
    format!("{:x}", h.finalize())[..12].to_string()
}

#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Serialize,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    schemars::JsonSchema,
)]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::Critical => "Critical",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "critical" => Some(Self::Critical),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub enum Engine {
    Signature,
    Behavior,
    Integrity,
    Rootcheck,
}

impl Engine {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Signature => "Signature",
            Self::Behavior => "Behavior",
            Self::Integrity => "Integrity",
            Self::Rootcheck => "Rootcheck",
        }
    }
}

/// A response the agent carried out.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub enum Response {
    Alert,
    Kill,
    Quarantine,
    /// The BPF LSM denied the exec.
    Block,
}

impl Response {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Alert => "Alert",
            Self::Kill => "Kill",
            Self::Quarantine => "Quarantine",
            Self::Block => "Block",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProcessInfo {
    pub pid: i32,
    pub ppid: i32,
    pub comm: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    pub cmdline: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    /// Ancestors from the parent up, as `pid:comm`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub storyline: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Workload {
    pub namespace: String,
    pub pod: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_uid: Option<String>,
    pub container: String,
    pub container_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Detection {
    pub id: String,
    pub time: DateTime<Utc>,
    pub node: String,
    pub engine: Engine,
    pub rule: String,
    pub severity: Severity,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcessInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FileInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<Workload>,
    #[serde(default)]
    pub actions: Vec<Response>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_error: Option<String>,
}

impl Detection {
    pub fn new(
        node: &str,
        engine: Engine,
        rule: &str,
        severity: Severity,
        description: String,
    ) -> Self {
        Self {
            id: random_hex(8),
            time: Utc::now(),
            node: node.to_string(),
            engine,
            rule: rule.to_string(),
            severity,
            description,
            process: None,
            file: None,
            workload: None,
            actions: vec![Response::Alert],
            action_error: None,
        }
    }
}

/// `n` random bytes as lowercase hex.
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    if getrandom::fill(&mut buf).is_err() {
        let nanos = Utc::now().timestamp_nanos_opt().unwrap_or_default();
        buf = nanos
            .to_le_bytes()
            .iter()
            .copied()
            .cycle()
            .take(n)
            .collect();
    }
    buf.iter().fold(String::with_capacity(n * 2), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_compiles_from_spec() {
        let mut spec: WardSpec = serde_json::from_str("{}").unwrap();
        spec.mode = Mode::Protect;
        spec.exclusions = Exclusions {
            namespaces: vec!["kube-system".into()],
            paths: vec!["/usr/share/".into(), String::new()],
            processes: vec!["/usr/bin/containerd".into(), "containerd".into()],
        };
        let p = Policy::from_spec(&spec);
        assert!(p.protect());
        assert_eq!(p.scan.interval_secs, 21_600);
        assert_eq!(p.scan.max_file_size_bytes, 32 * 1024 * 1024);
        assert_eq!(p.behavior.poll_interval_ms, 1_000);
        assert_eq!(p.integrity.interval_secs, 300);
        assert!(p.rootcheck.enabled);
        assert_eq!(p.rootcheck.interval_secs, 12 * 3600);
        assert!(p.path_excluded("/usr/share/doc/x"));
        assert!(!p.path_excluded("/usr/bin/x"));
        assert!(p.namespace_excluded("kube-system"));
        assert!(!p.namespace_excluded("default"));
        assert!(p.process_excluded(Some("/usr/bin/containerd")));
        assert!(
            !p.process_excluded(Some("/tmp/containerd")),
            "basename never matches"
        );
        assert!(
            !p.process_excluded(Some("containerd")),
            "relative paths never match"
        );
        assert!(!p.process_excluded(None));
        assert!(!p.process_excluded(Some("/bin/bash")));

        let back: Policy = serde_json::from_str(&p.to_json()).unwrap();
        assert_eq!(back, p);
        assert_eq!(p.revision().len(), 12);
        assert_eq!(p.revision(), back.revision());
        spec.mode = Mode::Detect;
        assert_ne!(Policy::from_spec(&spec).revision(), p.revision());
    }

    #[test]
    fn bad_durations_fall_back() {
        let mut spec = WardSpec::default();
        spec.behavior.poll_interval = "1ms".into();
        let p = Policy::from_spec(&spec);
        assert_eq!(p.behavior.poll_interval_ms, 100);
        assert_eq!(p.scan.interval_secs, 21_600);
        assert_eq!(p.integrity.interval_secs, 300);
    }

    #[test]
    fn enums_round_trip() {
        for s in [
            Severity::Low,
            Severity::Medium,
            Severity::High,
            Severity::Critical,
        ] {
            assert_eq!(Severity::parse(s.as_str()), Some(s));
        }
        assert_eq!(Severity::parse("nope"), None);
        assert!(Severity::Critical > Severity::High);
        assert_eq!(Engine::Integrity.as_str(), "Integrity");
        assert_eq!(Engine::Rootcheck.as_str(), "Rootcheck");
        assert_eq!(Engine::Signature.as_str(), "Signature");
        assert_eq!(Engine::Behavior.as_str(), "Behavior");
        assert_eq!(Response::Kill.as_str(), "Kill");
        assert_eq!(Response::Quarantine.as_str(), "Quarantine");
        assert_eq!(Response::Alert.as_str(), "Alert");
        assert_eq!(Response::Block.as_str(), "Block");
    }

    #[test]
    fn detections_serialize() {
        let d = Detection::new(
            "node-a",
            Engine::Behavior,
            "reverse-shell",
            Severity::Critical,
            "x".into(),
        );
        assert_eq!(d.id.len(), 16);
        assert_eq!(d.actions, vec![Response::Alert]);
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("\"engine\":\"Behavior\""));
        assert!(!json.contains("process"));
        let back: Detection = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
        assert_ne!(random_hex(4), random_hex(4));
        assert_eq!(revision_of(&[b"a"]).len(), 12);
    }
}
