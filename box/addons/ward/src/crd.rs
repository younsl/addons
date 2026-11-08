//! The `Ward` custom resource. One object deploys a manager `StatefulSet`
//! and an agent `DaemonSet` in its namespace and carries the detection and
//! response policy they share.

pub mod chart;

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::CustomResourceExt as _;

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Affinity, PodDNSConfig, ResourceRequirements, Toleration, TopologySpreadConstraint,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const GROUP: &str = "security.ward.sh";
pub const KIND: &str = "Ward";

/// Deploys the Ward manager and node agents and sets their policy.
#[derive(CustomResource, KubeSchema, Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[kube(
    group = "security.ward.sh",
    version = "v1alpha1",
    kind = "Ward",
    plural = "wards",
    singular = "ward",
    shortname = "wd",
    namespaced,
    status = "WardStatus",
    derive = "PartialEq",
    doc = "Deploys the Ward manager StatefulSet and agent DaemonSet and sets the detection and response policy they share.",
    printcolumn = r#"{"name":"Mode","type":"string","jsonPath":".spec.mode"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Agents","type":"string","jsonPath":".status.agents"}"#,
    printcolumn = r#"{"name":"Revision","type":"string","jsonPath":".status.policyRevision"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
    validation = Rule::new("self.metadata.name.size() <= 40").message("name must be at most 40 characters so derived object names stay valid"),
    validation = Rule::new("!has(self.spec) || !has(self.spec.postgresql) || !has(self.spec.postgresql.enabled) || !self.spec.postgresql.enabled || !has(self.spec.manager) || !has(self.spec.manager.enabled) || self.spec.manager.enabled").message("postgresql stores the manager's detections, so it needs manager.enabled")
)]
#[serde(rename_all = "camelCase")]
pub struct WardSpec {
    /// Image of the manager and agent. Empty means the operator's own image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Pull policy of the manager and agent image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_pull_policy: Option<PullPolicy>,
    /// Secrets the manager and agent Pods pull their image with.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_pull_secrets: Vec<LocalObjectRef>,
    /// `Detect` only reports. `Protect` also kills malicious processes and
    /// quarantines malicious files on the node.
    #[serde(default)]
    pub mode: Mode,
    /// The manager `StatefulSet`.
    #[serde(default)]
    pub manager: ManagerSpec,
    /// Keep the manager's detections in PostgreSQL, an embedded server the
    /// operator deploys or an existing one.
    #[serde(default)]
    pub postgresql: PostgresqlSpec,
    /// The agent `DaemonSet`.
    #[serde(default)]
    pub agent: AgentSpec,
    /// Signature (anti-malware) scanning.
    #[serde(default)]
    pub scan: ScanSpec,
    /// Behavioral detection on process activity.
    #[serde(default)]
    pub behavior: BehaviorSpec,
    /// File integrity monitoring of host files.
    #[serde(default)]
    pub integrity: IntegritySpec,
    /// Rootkit anomaly checks on the host: hidden processes, files in
    /// `/dev`, and setuid files in temporary directories.
    #[serde(default)]
    pub rootcheck: RootcheckSpec,
    /// What every engine skips.
    #[serde(default)]
    pub exclusions: Exclusions,
    /// A signed signature database the manager pulls from an OCI registry
    /// and rolls out to agents in rings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed: Option<FeedSpec>,
    /// File hashes that override every signature source: `allow` wins over
    /// `block`, which wins over the feed and the built-in set.
    #[serde(default)]
    pub indicators: Indicators,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
pub enum Mode {
    #[default]
    Detect,
    Protect,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
pub enum PullPolicy {
    Always,
    IfNotPresent,
    Never,
}

impl PullPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Always => "Always",
            Self::IfNotPresent => "IfNotPresent",
            Self::Never => "Never",
        }
    }
}

#[derive(KubeSchema, Clone, Debug, Deserialize, Serialize, PartialEq)]
#[x_kube(
    validation = Rule::new("!has(self.dnsPolicy) || self.dnsPolicy != 'None' || (has(self.dnsConfig) && has(self.dnsConfig.nameservers) && size(self.dnsConfig.nameservers) > 0)").message("dnsPolicy None needs at least one dnsConfig.nameservers entry")
)]
#[serde(rename_all = "camelCase")]
pub struct ManagerSpec {
    /// Run the manager. Off, agents pull the signature database from the
    /// registry themselves and detections only go to logs and Events.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// The detection store and feed cache volume.
    #[serde(default)]
    pub storage: StorageSpec,
    /// Detections the manager keeps. The oldest are dropped first.
    #[serde(default = "default_retention")]
    #[schemars(range(min = 100, max = 1_000_000))]
    pub retention: u32,
    /// Agents publish a Kubernetes `Event` on the affected Pod for every
    /// detection, with or without the manager.
    #[serde(default = "default_true")]
    pub emit_events: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub node_selector: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tolerations: Vec<Toleration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity: Option<Affinity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topology_spread_constraints: Vec<TopologySpreadConstraint>,
    /// DNS policy of the Pods. Unset is the Kubernetes default
    /// (`ClusterFirst`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns_policy: Option<DnsPolicy>,
    /// Extra DNS settings merged into the Pods' resolver configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns_config: Option<PodDNSConfig>,
    /// A Gateway API `HTTPRoute` that exposes the admin API (`/v1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_route: Option<HttpRouteSpec>,
}

#[derive(KubeSchema, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[x_kube(
    validation = Rule::new("!(has(self.enabled) && self.enabled) || (has(self.parentRefs) && size(self.parentRefs) > 0)").message("an enabled httpRoute needs at least one parentRefs entry")
)]
#[serde(rename_all = "camelCase")]
pub struct HttpRouteSpec {
    #[serde(default)]
    pub enabled: bool,
    /// Gateways the route attaches to. Each must allow routes from the
    /// `Ward` namespace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32))]
    pub parent_refs: Vec<ParentRef>,
    /// Host names the route matches. Empty matches every host name of the
    /// listener.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 16), inner(length(min = 1, max = 253)))]
    pub hostnames: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

/// A `Gateway` (or other parent) the route attaches to. `namespace` is
/// required so the target is never implied.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParentRef {
    #[serde(default = "default_gateway_group")]
    #[schemars(length(max = 253))]
    pub group: String,
    #[serde(default = "default_gateway_kind")]
    #[schemars(length(min = 1, max = 63))]
    pub kind: String,
    #[schemars(length(min = 1, max = 253))]
    pub name: String,
    #[schemars(length(min = 1, max = 63))]
    pub namespace: String,
    /// Listener name on the parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 253))]
    pub section_name: Option<String>,
    /// Listener port on the parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub port: Option<u16>,
}

fn default_gateway_group() -> String {
    "gateway.networking.k8s.io".to_string()
}

fn default_gateway_kind() -> String {
    "Gateway".to_string()
}

impl Default for ManagerSpec {
    fn default() -> Self {
        Self {
            enabled: true,
            storage: StorageSpec::default(),
            retention: default_retention(),
            emit_events: true,
            resources: None,
            node_selector: BTreeMap::new(),
            tolerations: Vec::new(),
            affinity: None,
            topology_spread_constraints: Vec::new(),
            dns_policy: None,
            dns_config: None,
            http_route: None,
        }
    }
}

#[derive(KubeSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[x_kube(
    validation = Rule::new("self == oldSelf").message("storage is immutable, a StatefulSet cannot change its volume claims")
)]
#[serde(rename_all = "camelCase")]
pub struct StorageSpec {
    /// Size of the `PersistentVolumeClaim`.
    #[serde(default = "default_storage_size")]
    #[schemars(length(max = 32))]
    #[x_kube(validation = Rule::new("isQuantity(self) && quantity(self).compareTo(quantity('64Mi')) >= 0").message("size must be a quantity of at least 64Mi"))]
    pub size: String,
    /// Storage class of the claim. Empty means the cluster default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 253))]
    pub storage_class_name: Option<String>,
}

impl Default for StorageSpec {
    fn default() -> Self {
        Self {
            size: default_storage_size(),
            storage_class_name: None,
        }
    }
}

#[derive(KubeSchema, Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[x_kube(
    validation = Rule::new("!(has(self.external) && has(self.embedded))").message("set postgresql.external or postgresql.embedded, not both")
)]
#[serde(rename_all = "camelCase")]
pub struct PostgresqlSpec {
    /// Keep detections in PostgreSQL instead of a file on the manager
    /// volume. Needs the manager.
    #[serde(default)]
    pub enabled: bool,
    /// An existing server. Unset, the operator deploys an embedded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<ExternalPostgresql>,
    /// Settings of the embedded server, used when `external` is unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedded: Option<EmbeddedPostgresql>,
}

impl PostgresqlSpec {
    /// The embedded server to deploy, when there is one.
    pub fn embedded(&self) -> Option<EmbeddedPostgresql> {
        (self.enabled && self.external.is_none()).then(|| self.embedded.clone().unwrap_or_default())
    }
}

#[derive(KubeSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[x_kube(
    validation = Rule::new("(has(self.sslMode) && self.sslMode == 'Disable') || has(self.caSecretRef)").message("sslMode VerifyFull needs caSecretRef")
)]
#[serde(rename_all = "camelCase")]
pub struct ExternalPostgresql {
    #[schemars(length(min = 1, max = 253))]
    pub host: String,
    #[serde(default = "default_postgresql_port")]
    #[schemars(range(min = 1))]
    pub port: u16,
    #[serde(default = "default_postgresql_name")]
    #[schemars(length(min = 1, max = 63))]
    pub database: String,
    #[serde(default = "default_postgresql_name")]
    #[schemars(length(min = 1, max = 63))]
    pub username: String,
    /// Secret key in the `Ward` namespace holding the password.
    pub password_secret_ref: SecretKeyRef,
    /// `VerifyFull` encrypts and checks the server certificate and host
    /// name against `caSecretRef`. `Disable` sends everything in clear.
    #[serde(default)]
    pub ssl_mode: SslMode,
    /// Secret key in the `Ward` namespace holding the PEM CA bundle the
    /// server certificate chains to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_secret_ref: Option<SecretKeyRef>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
pub enum SslMode {
    Disable,
    #[default]
    VerifyFull,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretKeyRef {
    #[schemars(length(min = 1, max = 253))]
    pub name: String,
    #[schemars(length(min = 1, max = 253))]
    pub key: String,
}

#[derive(KubeSchema, Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedPostgresql {
    /// Image of the server. Empty means the operator's PostgreSQL image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 512))]
    pub image: Option<String>,
    /// The data volume. Disabling the embedded server deletes it with its
    /// data.
    #[serde(default = "default_postgresql_storage")]
    pub storage: StorageSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub node_selector: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tolerations: Vec<Toleration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity: Option<Affinity>,
}

impl Default for EmbeddedPostgresql {
    fn default() -> Self {
        Self {
            image: None,
            storage: default_postgresql_storage(),
            resources: None,
            node_selector: BTreeMap::new(),
            tolerations: Vec::new(),
            affinity: None,
        }
    }
}

#[derive(KubeSchema, Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[x_kube(
    validation = Rule::new("!has(self.dnsPolicy) || self.dnsPolicy != 'None' || (has(self.dnsConfig) && has(self.dnsConfig.nameservers) && size(self.dnsConfig.nameservers) > 0)").message("dnsPolicy None needs at least one dnsConfig.nameservers entry")
)]
#[serde(rename_all = "camelCase")]
pub struct AgentSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub node_selector: BTreeMap<String, String>,
    /// Tolerations of the agent. Unset tolerates every taint, so every node
    /// is covered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tolerations: Option<Vec<Toleration>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity: Option<Affinity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_class_name: Option<String>,
    /// How agent Pods are replaced when the `DaemonSet` changes. Unset rolls
    /// out with `maxUnavailable` 10 percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_strategy: Option<AgentUpdateStrategy>,
    /// DNS policy of the Pods. Unset is the Kubernetes default
    /// (`ClusterFirst`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns_policy: Option<DnsPolicy>,
    /// Extra DNS settings merged into the Pods' resolver configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns_config: Option<PodDNSConfig>,
    /// Run the agent privileged. Needed where `AppArmor` or `SELinux` blocks
    /// reading other containers through `/proc`, or loading eBPF.
    #[serde(default)]
    pub privileged: bool,
    /// Where process events come from. `Auto` tries eBPF (with the BPF LSM
    /// when the kernel enables it) and falls back to polling `/proc`.
    #[serde(default)]
    pub event_source: EventSource,
}

#[derive(KubeSchema, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[x_kube(
    validation = Rule::new("!has(self.rollingUpdate) || self.type == 'RollingUpdate'").message("rollingUpdate is only allowed with type RollingUpdate")
)]
#[serde(rename_all = "camelCase")]
pub struct AgentUpdateStrategy {
    /// `RollingUpdate` replaces agents gradually, `OnDelete` only when a
    /// Pod is deleted.
    #[serde(default, rename = "type")]
    pub kind: UpdateStrategyType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolling_update: Option<AgentRollingUpdate>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
pub enum DnsPolicy {
    ClusterFirst,
    ClusterFirstWithHostNet,
    Default,
    None,
}

impl DnsPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClusterFirst => "ClusterFirst",
            Self::ClusterFirstWithHostNet => "ClusterFirstWithHostNet",
            Self::Default => "Default",
            Self::None => "None",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
pub enum UpdateStrategyType {
    #[default]
    RollingUpdate,
    OnDelete,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentRollingUpdate {
    /// Agents that may be unavailable during the update, a count or a
    /// percentage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_unavailable: Option<IntOrString>,
    /// Agents started on a node before the old one stops, a count or a
    /// percentage. Above zero there is no detection gap on the node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_surge: Option<IntOrString>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
pub enum EventSource {
    #[default]
    Auto,
    Ebpf,
    Procfs,
}

impl EventSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Ebpf => "Ebpf",
            Self::Procfs => "Procfs",
        }
    }
}

#[derive(KubeSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ScanSpec {
    /// Scan the executable of every new process.
    #[serde(default = "default_true")]
    pub on_exec: bool,
    /// Match YARA rules from the feed and `WardSignatures`. Rules cost
    /// agent memory, so large rule sets may need a higher memory limit.
    #[serde(default = "default_true")]
    pub yara: bool,
    /// Period of the full scan of every container filesystem and of
    /// `hostPaths`.
    #[serde(default = "default_scan_interval")]
    #[schemars(length(max = 32), regex(pattern = r"^([0-9]+(ms|s|m|h))+$"))]
    #[x_kube(validation = Rule::new("duration(self) >= duration('1m')").message("interval must be at least 1m"))]
    pub interval: String,
    /// Files larger than this are skipped.
    #[serde(default = "default_max_file_size_mib")]
    #[schemars(range(min = 1, max = 1024))]
    pub max_file_size_mib: u32,
    /// Host directories the full scan also walks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64), inner(length(max = 4096)))]
    #[x_kube(validation = Rule::new("self.all(p, p.startsWith('/'))").message("hostPaths must be absolute"))]
    pub host_paths: Vec<String>,
}

impl Default for ScanSpec {
    fn default() -> Self {
        Self {
            on_exec: true,
            yara: true,
            interval: default_scan_interval(),
            max_file_size_mib: default_max_file_size_mib(),
            host_paths: Vec::new(),
        }
    }
}

#[derive(KubeSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BehaviorSpec {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// How often the agent samples the process table.
    #[serde(default = "default_poll_interval")]
    #[schemars(length(max = 32), regex(pattern = r"^([0-9]+(ms|s|m|h))+$"))]
    #[x_kube(validation = Rule::new("duration(self) >= duration('100ms') && duration(self) <= duration('1m')").message("pollInterval must be between 100ms and 1m"))]
    pub poll_interval: String,
    /// Built-in rule IDs to turn off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(extend("x-kubernetes-list-type" = "set"))]
    pub disabled_rules: Vec<BehaviorRule>,
}

impl Default for BehaviorSpec {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval: default_poll_interval(),
            disabled_rules: Vec::new(),
        }
    }
}

/// A built-in behavior rule.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum BehaviorRule {
    ReverseShell,
    ShellNetworkConnect,
    FilelessExec,
    ExecFromTmp,
    CryptoMiner,
    WebserverShell,
    PipeToShell,
    ReconTool,
    SaTokenRead,
}

impl BehaviorRule {
    pub const ALL: [Self; 9] = [
        Self::ReverseShell,
        Self::ShellNetworkConnect,
        Self::FilelessExec,
        Self::ExecFromTmp,
        Self::CryptoMiner,
        Self::WebserverShell,
        Self::PipeToShell,
        Self::ReconTool,
        Self::SaTokenRead,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::ReverseShell => "reverse-shell",
            Self::ShellNetworkConnect => "shell-network-connect",
            Self::FilelessExec => "fileless-exec",
            Self::ExecFromTmp => "exec-from-tmp",
            Self::CryptoMiner => "crypto-miner",
            Self::WebserverShell => "webserver-shell",
            Self::PipeToShell => "pipe-to-shell",
            Self::ReconTool => "recon-tool",
            Self::SaTokenRead => "sa-token-read",
        }
    }
}

#[derive(KubeSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IntegritySpec {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Host files and directories to baseline and watch. Directories are
    /// walked down to `recursionLevel`.
    #[serde(default = "default_integrity_paths")]
    #[schemars(length(max = 256), inner(length(max = 4096)))]
    #[x_kube(validation = Rule::new("self.all(p, p.startsWith('/'))").message("integrity paths must be absolute"))]
    pub paths: Vec<String>,
    #[serde(default = "default_integrity_interval")]
    #[schemars(length(max = 32), regex(pattern = r"^([0-9]+(ms|s|m|h))+$"))]
    #[x_kube(validation = Rule::new("duration(self) >= duration('10s')").message("interval must be at least 10s"))]
    pub interval: String,
    /// Report changes as they happen with inotify, as Wazuh `realtime`.
    /// The periodic check stays on as a safety net.
    #[serde(default = "default_true")]
    pub realtime: bool,
    /// Record the process and user behind each change with BPF LSM hooks,
    /// as Wazuh `whodata`. Needs the `ebpf-lsm` event source.
    #[serde(default = "default_true")]
    pub whodata: bool,
    /// Keep copies of watched text files up to 256 KiB and report a
    /// modification as a unified diff, as Wazuh `report_changes`.
    #[serde(default = "default_true")]
    pub report_changes: bool,
    /// Files and directories whose content never goes into a diff or a
    /// copy on the node, as Wazuh `nodiff`.
    #[serde(default = "default_no_diff")]
    #[schemars(length(max = 256), inner(length(max = 4096)))]
    #[x_kube(validation = Rule::new("self.all(p, p.startsWith('/'))").message("noDiff paths must be absolute"))]
    pub no_diff: Vec<String>,
    /// Directory levels below a watched directory, as Wazuh
    /// `recursion_level`.
    #[serde(default = "default_recursion_level")]
    #[schemars(range(max = 320))]
    pub recursion_level: u32,
    /// Files the baseline holds at most, as Wazuh `file_limit`.
    #[serde(default = "default_file_limit")]
    #[schemars(range(min = 1, max = 1_000_000))]
    pub file_limit: u32,
}

#[derive(KubeSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RootcheckSpec {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_rootcheck_interval")]
    #[schemars(length(max = 32), regex(pattern = r"^([0-9]+(ms|s|m|h))+$"))]
    #[x_kube(validation = Rule::new("duration(self) >= duration('10m')").message("rootcheck interval must be at least 10m"))]
    pub interval: String,
}

impl Default for RootcheckSpec {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: default_rootcheck_interval(),
        }
    }
}

impl Default for IntegritySpec {
    fn default() -> Self {
        Self {
            enabled: true,
            paths: default_integrity_paths(),
            interval: default_integrity_interval(),
            realtime: true,
            whodata: true,
            report_changes: true,
            no_diff: default_no_diff(),
            recursion_level: default_recursion_level(),
            file_limit: default_file_limit(),
        }
    }
}

#[derive(KubeSchema, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Exclusions {
    /// Namespaces whose Pods are never scanned or acted on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespaces: Vec<String>,
    /// Path prefixes inside a container or on the host that are never scanned.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256), inner(length(max = 4096)))]
    #[x_kube(validation = Rule::new("self.all(p, p.startsWith('/'))").message("exclusion paths must be absolute"))]
    pub paths: Vec<String>,
    /// Absolute executable paths, matched exactly against
    /// `/proc/<pid>/exe`, that behavior rules skip. Signature scanning and
    /// its response still apply, so a renamed or relocated binary cannot
    /// hide malware behind an entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64), inner(length(max = 4096)))]
    #[x_kube(validation = Rule::new("self.all(p, p.startsWith('/'))").message("exclusion processes must be absolute executable paths"))]
    pub processes: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Indicators {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 10_000), extend("x-kubernetes-list-type" = "map", "x-kubernetes-list-map-keys" = ["sha256"]))]
    pub allow: Vec<Indicator>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 10_000), extend("x-kubernetes-list-type" = "map", "x-kubernetes-list-map-keys" = ["sha256"]))]
    pub block: Vec<Indicator>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Indicator {
    /// Lowercase SHA-256 of the file.
    #[schemars(regex(pattern = r"^[a-f0-9]{64}$"))]
    pub sha256: String,
    /// Why the hash is listed. Becomes the detection name for `block`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub comment: String,
}

#[derive(KubeSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[x_kube(
    validation = Rule::new("has(self.publicKey) || self.insecureSkipVerify").message("feed needs publicKey unless insecureSkipVerify is true")
)]
#[serde(rename_all = "camelCase")]
pub struct FeedSpec {
    /// OCI reference of the database, by tag (`ghcr.io/org/ward-db:latest`)
    /// or digest (`...@sha256:...`) to pin a revision.
    #[schemars(
        length(min = 1, max = 512),
        regex(
            pattern = r"^[a-z0-9.-]+(:[0-9]+)?/[a-z0-9._/-]+(:[A-Za-z0-9._-]{1,128}|@sha256:[a-f0-9]{64})$"
        )
    )]
    pub image: String,
    /// PEM ECDSA P-256 public key the cosign signature must verify against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    /// Accept an unsigned database. For development only.
    #[serde(default)]
    pub insecure_skip_verify: bool,
    /// How often the manager checks the registry.
    #[serde(default = "default_feed_interval")]
    #[schemars(length(max = 32), regex(pattern = r"^([0-9]+(ms|s|m|h))+$"))]
    #[x_kube(validation = Rule::new("duration(self) >= duration('5m')").message("feed interval must be at least 5m"))]
    pub interval: String,
    /// A `kubernetes.io/dockerconfigjson` Secret for a private registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_secret_ref: Option<LocalObjectRef>,
    #[serde(default)]
    pub rollout: RolloutSpec,
}

#[derive(KubeSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RolloutSpec {
    /// Share of agents that get a new revision first. Nodes labeled
    /// `ward.sh/ring=canary` are always in the canary ring.
    #[serde(default = "default_canary_percent")]
    #[schemars(range(min = 0, max = 100))]
    pub canary_percent: u32,
    /// How long the canary ring runs a revision before it is promoted.
    #[serde(default = "default_soak")]
    #[schemars(length(max = 32), regex(pattern = r"^([0-9]+(ms|s|m|h))+$"))]
    #[x_kube(validation = Rule::new("duration(self) >= duration('1m')").message("soak must be at least 1m"))]
    pub soak: String,
    /// Halt when the canary ring's detections during the soak exceed this
    /// many times the previous revision's over the same span.
    #[serde(default = "default_max_detection_ratio")]
    #[schemars(range(min = 1, max = 1000))]
    pub max_detection_ratio: u32,
    /// Detections below this count never halt a rollout.
    #[serde(default = "default_min_detections")]
    #[schemars(range(min = 1, max = 100_000))]
    pub min_detections: u32,
}

impl Default for RolloutSpec {
    fn default() -> Self {
        Self {
            canary_percent: default_canary_percent(),
            soak: default_soak(),
            max_detection_ratio: default_max_detection_ratio(),
            min_detections: default_min_detections(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
pub struct LocalObjectRef {
    #[schemars(length(min = 1, max = 253))]
    pub name: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WardStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// `Progressing`, `Ready`, or `Degraded`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Ready over desired agents, as `3/3`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<String>,
    #[serde(default)]
    pub manager_ready: bool,
    /// Hash of the policy the agents pull from the manager.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_revision: Option<String>,
    /// Signature database rollout, as the manager reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentStatus>,
    /// Agents per event source mode (`ebpf-lsm`, `ebpf`, `procfs`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub event_sources: BTreeMap<String, i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContentStatus {
    /// The revision every ring runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable: Option<String>,
    /// The revision being rolled out, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// `Idle`, `Canary`, or `Halted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Active revisions and the agents running each.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub revisions: BTreeMap<String, i32>,
}

/// Custom signatures and YARA rules for the `Ward` in the same namespace.
#[derive(
    CustomResource, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema,
)]
#[kube(
    group = "security.ward.sh",
    version = "v1alpha1",
    kind = "WardSignature",
    plural = "wardsignatures",
    singular = "wardsignature",
    shortname = "wsig",
    namespaced,
    status = "WardSignatureStatus",
    derive = "PartialEq",
    doc = "Custom signatures and YARA rules merged into every Ward in the same namespace.",
    printcolumn = r#"{"name":"Signatures","type":"integer","jsonPath":".status.signatures"}"#,
    printcolumn = r#"{"name":"Accepted","type":"string","jsonPath":".status.conditions[?(@.type=='Accepted')].status"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct WardSignatureSpec {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 1000), extend("x-kubernetes-list-type" = "map", "x-kubernetes-list-map-keys" = ["name"]))]
    pub signatures: Vec<crate::signature::Signature>,
    /// YARA rule source, compiled with YARA-X.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 262_144))]
    pub yara: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WardSignatureStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// Signatures plus YARA rules accepted.
    #[serde(default)]
    pub signatures: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

/// Every CRD the chart ships.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrdKind {
    Ward,
    WardSignature,
}

impl CrdKind {
    pub const ALL: [Self; 2] = [Self::Ward, Self::WardSignature];

    pub fn crd(self) -> CustomResourceDefinition {
        match self {
            Self::Ward => Ward::crd(),
            Self::WardSignature => WardSignature::crd(),
        }
    }
}

fn default_feed_interval() -> String {
    "6h".to_string()
}

const fn default_canary_percent() -> u32 {
    10
}

fn default_soak() -> String {
    "1h".to_string()
}

const fn default_max_detection_ratio() -> u32 {
    5
}

const fn default_min_detections() -> u32 {
    20
}

const fn default_true() -> bool {
    true
}

const fn default_retention() -> u32 {
    10_000
}

fn default_storage_size() -> String {
    "1Gi".to_string()
}

fn default_scan_interval() -> String {
    "6h".to_string()
}

const fn default_max_file_size_mib() -> u32 {
    32
}

fn default_poll_interval() -> String {
    "1s".to_string()
}

fn default_integrity_interval() -> String {
    "5m".to_string()
}

const fn default_postgresql_port() -> u16 {
    5432
}

fn default_postgresql_name() -> String {
    "ward".to_string()
}

fn default_postgresql_storage() -> StorageSpec {
    StorageSpec {
        size: "8Gi".to_string(),
        storage_class_name: None,
    }
}

fn default_rootcheck_interval() -> String {
    "12h".to_string()
}

const fn default_recursion_level() -> u32 {
    256
}

const fn default_file_limit() -> u32 {
    100_000
}

/// Secrets whose content must never land in a log, an Event, or the
/// manager as a diff.
fn default_no_diff() -> Vec<String> {
    ["/etc/shadow", "/etc/gshadow", "/etc/security/opasswd"]
        .map(String::from)
        .to_vec()
}

fn default_integrity_paths() -> Vec<String> {
    [
        "/etc/passwd",
        "/etc/shadow",
        "/etc/group",
        "/etc/sudoers",
        "/etc/sudoers.d",
        "/etc/ssh/sshd_config",
        "/root/.ssh/authorized_keys",
        "/etc/ld.so.preload",
        "/etc/crontab",
        "/etc/cron.d",
        "/etc/kubernetes/manifests",
    ]
    .map(String::from)
    .to_vec()
}

/// Parses `1h30m`, `500ms`, `6h` into a duration.
pub fn parse_duration(s: &str) -> Option<std::time::Duration> {
    if s.is_empty() {
        return None;
    }
    let mut total = std::time::Duration::ZERO;
    let mut rest = s;
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit())?;
        if digits == 0 {
            return None;
        }
        let n: u64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let (unit, len) = if rest.starts_with("ms") {
            (std::time::Duration::from_millis(n), 2)
        } else if rest.starts_with('s') {
            (std::time::Duration::from_secs(n), 1)
        } else if rest.starts_with('m') {
            (std::time::Duration::from_secs(n.checked_mul(60)?), 1)
        } else if rest.starts_with('h') {
            (std::time::Duration::from_secs(n.checked_mul(3600)?), 1)
        } else {
            return None;
        };
        total = total.checked_add(unit)?;
        rest = &rest[len..];
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("6h"), Some(Duration::from_secs(21_600)));
        assert_eq!(parse_duration("1h30m"), Some(Duration::from_mins(90)));
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("1m5s"), Some(Duration::from_secs(65)));
        for bad in ["", "h", "5", "5d", "-1s", "1.5s"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
    }

    #[test]
    fn empty_spec_takes_defaults() {
        let spec: WardSpec = serde_json::from_str("{}").unwrap();
        assert_eq!(spec.mode, Mode::Detect);
        assert_eq!(spec.manager.retention, 10_000);
        assert!(spec.manager.emit_events);
        assert_eq!(spec.manager.storage.size, "1Gi");
        assert!(spec.scan.on_exec);
        assert!(spec.scan.yara);
        assert!(spec.manager.enabled);
        assert_eq!(spec.scan.interval, "6h");
        assert!(spec.behavior.enabled);
        assert!(spec.integrity.paths.contains(&"/etc/shadow".to_string()));
        assert!(spec.integrity.realtime && spec.integrity.whodata && spec.integrity.report_changes);
        assert!(spec.integrity.no_diff.contains(&"/etc/shadow".to_string()));
        assert_eq!(spec.integrity.recursion_level, 256);
        assert_eq!(spec.integrity.file_limit, 100_000);
        for d in [
            &spec.scan.interval,
            &spec.behavior.poll_interval,
            &spec.integrity.interval,
            &spec.rootcheck.interval,
        ] {
            assert!(parse_duration(d).is_some(), "{d}");
        }
        assert_eq!(spec, spec.clone());
    }

    #[test]
    fn crd_carries_validations() {
        let crd = serde_json::to_string(&Ward::crd()).unwrap();
        assert!(crd.contains("wards.security.ward.sh"));
        assert!(crd.contains("interval must be at least 1m"));
        assert!(crd.contains("rootcheck interval must be at least 10m"));
        assert!(crd.contains("integrity paths must be absolute"));
        assert_eq!(PullPolicy::Never.as_str(), "Never");
        assert_eq!(PullPolicy::Always.as_str(), "Always");
        assert_eq!(PullPolicy::IfNotPresent.as_str(), "IfNotPresent");
        assert!(crd.contains("feed needs publicKey unless insecureSkipVerify is true"));
        for msg in [
            "storage is immutable",
            "hostPaths must be absolute",
            "exclusion paths must be absolute",
            "name must be at most 40 characters",
            "x-kubernetes-list-map-keys",
            "shell-network-connect",
            "rollingUpdate is only allowed with type RollingUpdate",
            "dnsPolicy None needs at least one dnsConfig.nameservers entry",
        ] {
            assert!(crd.contains(msg), "{msg}");
        }
        assert_eq!(BehaviorRule::ALL.len(), 9);
        for p in [
            DnsPolicy::ClusterFirst,
            DnsPolicy::ClusterFirstWithHostNet,
            DnsPolicy::Default,
            DnsPolicy::None,
        ] {
            assert_eq!(serde_json::to_value(p).unwrap(), p.as_str());
        }
        for r in BehaviorRule::ALL {
            assert_eq!(serde_json::to_value(r).unwrap(), r.id());
        }
        let sig = serde_json::to_string(&CrdKind::WardSignature.crd()).unwrap();
        assert!(sig.contains("wardsignatures.security.ward.sh"));
        assert!(sig.contains("needs sha256, strings, or hex"), "{sig}");
        assert_eq!(CrdKind::ALL.len(), 2);
        for e in [EventSource::Auto, EventSource::Ebpf, EventSource::Procfs] {
            assert_ne!(e.as_str(), "");
        }
        let r = RolloutSpec::default();
        assert_eq!(r.canary_percent, 10);
        assert!(parse_duration(&r.soak).is_some());
        assert!(parse_duration(&default_feed_interval()).is_some());
    }
}
