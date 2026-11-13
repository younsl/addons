//! The `NodeConfiguration` custom resource.

pub mod binding;
pub mod chart;
pub mod validate;

use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const GROUP: &str = "node-configuration-operator.sh";

const ABSOLUTE_PATH: &str = r"^/[^*?]+$";
/// Wildcards are confined to the last segment so a glob lists one directory.
const GLOB_PATH: &str = r"^(/[^/*?]+)*/[^/]+$";
const OCTAL_MODE: &str = r"^[0-7]{3,4}$";
pub const DNS_LABEL: &str = r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?$";

/// Keeps the OS configuration of EKS managed node group nodes in line with
/// `rules`. A change of the rules is a new revision that the controller rolls
/// out node by node under `strategy`, and the agent on each node reapplies its
/// revision on every interval.
#[derive(CustomResource, KubeSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[kube(
    group = "node-configuration-operator.sh",
    version = "v1alpha1",
    kind = "NodeConfiguration",
    plural = "nodeconfigurations",
    singular = "nodeconfiguration",
    shortname = "nc",
    status = "NodeConfigurationStatus",
    derive = "PartialEq",
    doc = "Keeps the OS configuration of EKS managed node group nodes in line with a set of declarative rules.",
    printcolumn = r#"{"name":"Desired","type":"integer","jsonPath":".status.desiredNodes"}"#,
    printcolumn = r#"{"name":"Updated","type":"integer","jsonPath":".status.updatedNodes"}"#,
    printcolumn = r#"{"name":"Configured","type":"integer","jsonPath":".status.configuredNodes"}"#,
    printcolumn = r#"{"name":"Failed","type":"integer","jsonPath":".status.failedNodes"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"Rollout","type":"string","jsonPath":".status.conditions[?(@.type==\"Progressing\")].reason"}"#,
    printcolumn = r#"{"name":"Group","type":"string","jsonPath":".spec.group","priority":1}"#,
    printcolumn = r#"{"name":"Priority","type":"integer","jsonPath":".spec.priority","priority":1}"#,
    printcolumn = r#"{"name":"Revision","type":"string","jsonPath":".status.revision","priority":1}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct NodeConfigurationSpec {
    /// The nodes to configure. Only EKS managed node group nodes are ever
    /// selected, and nodes that Karpenter owns are always left out.
    #[serde(default)]
    #[schemars(extend("default" = {}))]
    pub target: Target,
    /// Names a set of `NodeConfiguration` resources that apply in order on a
    /// node, for example `os-vulnerability-hardening`. Without a group the
    /// resource runs independently of every other one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 63), regex(pattern = DNS_LABEL))]
    pub group: Option<String>,
    /// Order inside the group. A node runs the resource with the highest
    /// priority first, and runs a lower one only after every higher one of the
    /// group is configured on that node. Resources of equal priority do not
    /// wait for each other.
    #[serde(default)]
    #[schemars(range(min = -1_000_000, max = 1_000_000))]
    pub priority: i32,
    /// Reports what a run would change without writing to the node. A change
    /// shows up as the outcome `pending`, and the node still counts as
    /// configured.
    #[serde(default)]
    pub dry_run: bool,
    /// Seconds between two runs on a node, which bounds how long a drift
    /// survives.
    #[serde(default = "default_interval_seconds")]
    #[schemars(range(min = 30, max = 86400))]
    pub interval_seconds: u32,
    /// How a new revision replaces the previous one across the nodes.
    #[serde(default)]
    #[schemars(extend("default" = {}))]
    pub strategy: RolloutStrategy,
    /// Seconds a node must stay configured before the rollout moves on.
    #[serde(default)]
    #[schemars(range(max = 3600))]
    pub min_ready_seconds: u32,
    /// Holds a new revision back. The nodes keep reapplying the revision they
    /// already run until the rollout is resumed, and a node that joins
    /// meanwhile gets that same revision.
    #[serde(default)]
    pub paused: bool,
    /// Seconds a rollout may go without progress before the `Progressing`
    /// condition turns false with the reason `ProgressDeadlineExceeded`.
    #[serde(default = "default_progress_deadline_seconds")]
    #[schemars(range(min = 60, max = 86400))]
    pub progress_deadline_seconds: u32,
    /// What every selected node must satisfy: named rules with one action
    /// each, run in list order.
    #[schemars(
        length(max = 256),
        extend("x-kubernetes-list-type" = "map", "x-kubernetes-list-map-keys" = ["name"])
    )]
    pub rules: Rules,
}

impl NodeConfigurationSpec {
    /// Whether a node is cordoned while it moves to a new revision.
    pub fn cordons(&self) -> bool {
        !self.dry_run
            && self.strategy.type_ == StrategyType::RollingUpdate
            && self
                .strategy
                .rolling_update
                .as_ref()
                .is_some_and(|r| r.cordon)
    }
}

const fn default_interval_seconds() -> u32 {
    300
}

const fn default_progress_deadline_seconds() -> u32 {
    600
}

/// How a new revision reaches the nodes.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
pub enum StrategyType {
    /// Move a few nodes at a time to the new revision. A node that fails a
    /// rule stops the rollout.
    #[default]
    RollingUpdate,
    /// Keep every node on its revision until its `NodeConfigurationBinding`
    /// is deleted.
    OnDelete,
}

/// The rollout strategy, modeled on a `Deployment`.
#[derive(KubeSchema, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[x_kube(
    validation = Rule::new("!has(self.rollingUpdate) || !has(self.type) || self.type == 'RollingUpdate'").message("rollingUpdate is allowed only with type RollingUpdate")
)]
#[serde(rename_all = "camelCase")]
pub struct RolloutStrategy {
    /// `RollingUpdate` or `OnDelete`.
    #[serde(rename = "type", default)]
    pub type_: StrategyType,
    /// Parameters of `RollingUpdate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolling_update: Option<RollingUpdate>,
}

/// Parameters of the `RollingUpdate` strategy.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RollingUpdate {
    /// Nodes that may be on a new revision without being configured yet, as a
    /// count or a percentage such as `10%`. Omitted means 1, one node at a
    /// time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_unavailable: Option<IntOrString>,
    /// Cordons a node before it moves to a new revision and uncordons it
    /// once the revision is configured there, so no new Pod is scheduled on a
    /// node whose configuration is changing. A node that was already cordoned
    /// is left as it is, and a dry run never cordons.
    #[serde(default)]
    pub cordon: bool,
}

/// Narrows the managed node group nodes the agent runs on.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    /// Managed node group names. Empty selects every managed node group.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64), inner(length(min = 1, max = 63)))]
    pub node_groups: Vec<String>,
    /// Node labels a node must also carry.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schemars(extend("maxProperties" = 16))]
    pub node_selector: BTreeMap<String, String>,
    /// Node names to leave out even though the rest of the target selects
    /// them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256), inner(length(min = 1, max = 253)))]
    pub exclude_nodes: Vec<String>,
}

/// The rules of a `NodeConfiguration`, run in list order.
pub type Rules = Vec<NamedRule>;

/// One named, idempotent action on the node.
#[derive(KubeSchema, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[x_kube(
    validation = Rule::new("(has(self.fileMode) ? 1 : 0) + (has(self.loginShell) ? 1 : 0) + (has(self.lineEdit) ? 1 : 0) + (has(self.file) ? 1 : 0) + (has(self.script) ? 1 : 0) == 1").message("exactly one of fileMode, loginShell, lineEdit, file, and script is required")
)]
#[serde(rename_all = "camelCase")]
pub struct NamedRule {
    /// What the rule is for, unique inside the resource. It names the rule in
    /// logs and in the status of a node that fails it.
    #[schemars(length(min = 1, max = 63), regex(pattern = DNS_LABEL))]
    pub name: String,
    /// Permission bits of existing files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_mode: Option<FileModeRule>,
    /// The login shell of local accounts in `/etc/passwd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_shell: Option<LoginShellRule>,
    /// A line substitution in an existing text file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_edit: Option<LineEditRule>,
    /// A file whose whole content is managed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FileRule>,
    /// A shell script run on the host, for what the other actions cannot
    /// express. It needs `agent.allowScripts` in the chart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<ScriptRule>,
}

/// The single action of a rule.
#[derive(Clone, Copy, Debug)]
pub enum RuleAction<'a> {
    FileMode(&'a FileModeRule),
    LoginShell(&'a LoginShellRule),
    LineEdit(&'a LineEditRule),
    File(&'a FileRule),
    Script(&'a ScriptRule),
}

impl NamedRule {
    fn named(name: &str) -> Self {
        Self {
            name: name.to_string(),
            ..Self::default()
        }
    }

    pub fn file_mode(name: &str, rule: FileModeRule) -> Self {
        Self {
            file_mode: Some(rule),
            ..Self::named(name)
        }
    }

    pub fn login_shell(name: &str, rule: LoginShellRule) -> Self {
        Self {
            login_shell: Some(rule),
            ..Self::named(name)
        }
    }

    pub fn line_edit(name: &str, rule: LineEditRule) -> Self {
        Self {
            line_edit: Some(rule),
            ..Self::named(name)
        }
    }

    pub fn file(name: &str, rule: FileRule) -> Self {
        Self {
            file: Some(rule),
            ..Self::named(name)
        }
    }

    pub fn script(name: &str, rule: ScriptRule) -> Self {
        Self {
            script: Some(rule),
            ..Self::named(name)
        }
    }

    /// The action of the rule, or nothing unless exactly one is set.
    pub fn action(&self) -> Option<RuleAction<'_>> {
        let mut actions = [
            self.file_mode.as_ref().map(RuleAction::FileMode),
            self.login_shell.as_ref().map(RuleAction::LoginShell),
            self.line_edit.as_ref().map(RuleAction::LineEdit),
            self.file.as_ref().map(RuleAction::File),
            self.script.as_ref().map(RuleAction::Script),
        ]
        .into_iter()
        .flatten();
        let first = actions.next()?;
        actions.next().is_none().then_some(first)
    }
}

/// Sets or clears permission bits, like `chmod`. A path that does not exist on
/// the node is skipped.
#[derive(KubeSchema, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[x_kube(
    validation = Rule::new("has(self.mode) != has(self.clear)").message("exactly one of mode and clear is required")
)]
#[serde(rename_all = "camelCase")]
pub struct FileModeRule {
    /// Absolute paths on the node. The last segment may hold `*` and `?`.
    #[schemars(
        length(min = 1, max = 64),
        inner(length(max = 4096), regex(pattern = GLOB_PATH))
    )]
    pub paths: Vec<String>,
    /// The exact octal mode to set, such as `0640` or `4750`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = OCTAL_MODE))]
    pub mode: Option<String>,
    /// Octal bits to remove from the current mode. `4000` drops setuid like
    /// `chmod u-s`, and `0001` drops execute for others like `chmod o-x`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = OCTAL_MODE))]
    pub clear: Option<String>,
}

/// Sets the login shell of local accounts, like `usermod -s`. An account that
/// does not exist on the node is skipped.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoginShellRule {
    /// Account names.
    #[schemars(
        length(min = 1, max = 64),
        inner(regex(pattern = r"^[a-z_][a-z0-9_-]{0,31}$"))
    )]
    pub users: Vec<String>,
    /// The shell to set, such as `/sbin/nologin`.
    #[schemars(length(max = 4096), regex(pattern = r"^/[^:\s]+$"))]
    pub shell: String,
}

/// Replaces the first match of `pattern` on every line, like `sed -i
/// 's/pattern/replacement/'`. A file that does not exist on the node is skipped.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LineEditRule {
    /// Absolute path of the file on the node.
    #[schemars(length(max = 4096), regex(pattern = ABSOLUTE_PATH))]
    pub path: String,
    /// A regular expression in the Rust regex syntax, matched against each
    /// line without its line break.
    #[schemars(length(min = 1, max = 1024))]
    pub pattern: String,
    /// The replacement. `$1` or `${1}` insert a capture group.
    #[schemars(length(max = 1024))]
    pub replacement: String,
}

/// Writes a file with exactly `content`, replacing whatever is at `path`, a
/// symbolic link included.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileRule {
    /// Absolute path of the file on the node. Its directory must exist.
    #[schemars(length(max = 4096), regex(pattern = ABSOLUTE_PATH))]
    pub path: String,
    /// The whole file content.
    #[schemars(length(max = 65536))]
    pub content: String,
    /// The octal mode of the file.
    #[serde(default = "default_file_mode")]
    #[schemars(regex(pattern = OCTAL_MODE))]
    pub mode: String,
}

/// Runs a shell script as root inside the host root filesystem, through the
/// `/bin/sh` of the node. The agent cannot tell what a script changed, so
/// `unless` is what keeps it idempotent.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScriptRule {
    /// The script to run. A non-zero exit status fails the rule.
    #[schemars(length(min = 1, max = 32768))]
    pub run: String,
    /// A check that must not change the node. When it exits with 0 the node
    /// is in line and `run` is skipped. Without it `run` runs on every
    /// interval. A dry run executes only this check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 32768))]
    pub unless: Option<String>,
    /// Seconds each of `unless` and `run` may take before it is killed.
    #[serde(default = "default_script_timeout_seconds")]
    #[schemars(range(min = 1, max = 600))]
    pub timeout_seconds: u32,
}

impl Default for ScriptRule {
    fn default() -> Self {
        Self {
            run: String::new(),
            unless: None,
            timeout_seconds: default_script_timeout_seconds(),
        }
    }
}

const fn default_script_timeout_seconds() -> u32 {
    60
}

fn default_file_mode() -> String {
    "0644".to_string()
}

/// What the controller observed.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeConfigurationStatus {
    /// The generation the status describes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// Nodes the agent is scheduled on.
    #[serde(default)]
    pub desired_nodes: i32,
    /// Nodes assigned the current revision.
    #[serde(default)]
    pub updated_nodes: i32,
    /// Nodes whose last run applied their assigned revision without a failed
    /// rule.
    #[serde(default)]
    pub configured_nodes: i32,
    /// Configured nodes that have stayed so for `minReadySeconds`.
    #[serde(default)]
    pub available_nodes: i32,
    /// Nodes whose last run failed at least one rule.
    #[serde(default)]
    pub failed_nodes: i32,
    /// The revision `spec` describes, a hash of the interval and the rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Per rule, how many nodes on the current revision reported each
    /// outcome in their last run.
    #[serde(default)]
    #[schemars(extend("x-kubernetes-list-type" = "map", "x-kubernetes-list-map-keys" = ["name"]))]
    pub rules: Vec<RuleSummary>,
    /// Nodes that are not configured and why, at most ten of them in node
    /// name order.
    #[serde(default)]
    pub failures: Vec<NodeFailure>,
    /// `Ready` is true once every desired node is configured on the current
    /// revision. `Progressing` follows the rollout of a new revision.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

/// The last run of one rule across the nodes.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuleSummary {
    /// Name of the rule.
    pub name: String,
    /// Nodes that were already in line.
    #[serde(default)]
    pub unchanged: i32,
    /// Nodes the rule corrected.
    #[serde(default)]
    pub changed: i32,
    /// Nodes where a dry run found a difference.
    #[serde(default)]
    pub pending: i32,
    /// Nodes where the target of the rule does not exist.
    #[serde(default)]
    pub skipped: i32,
    /// Nodes where the rule failed.
    #[serde(default)]
    pub failed: i32,
}

/// A node that is not configured.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeFailure {
    /// Name of the node.
    pub node: String,
    /// The first failed rule, or the configuration the node waits for.
    pub message: String,
}

/// A status condition in the Kubernetes convention.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    /// The condition type, `Ready` or `Progressing`.
    #[serde(rename = "type")]
    pub type_: String,
    /// `True`, `False`, or `Unknown`.
    pub status: String,
    /// A CamelCase word for the cause.
    pub reason: String,
    /// The cause in a sentence.
    pub message: String,
    /// When the condition was last updated, in RFC 3339. For `Progressing`
    /// this is the last time the rollout made progress.
    pub last_update_time: String,
    /// When `status` last changed, in RFC 3339.
    pub last_transition_time: String,
}

#[cfg(test)]
pub mod testing {
    use super::{NodeConfiguration, NodeConfigurationSpec, RolloutStrategy, Rules, Target};

    pub fn spec(rules: Rules) -> NodeConfigurationSpec {
        NodeConfigurationSpec {
            target: Target::default(),
            group: None,
            priority: 0,
            dry_run: false,
            interval_seconds: 300,
            strategy: RolloutStrategy::default(),
            min_ready_seconds: 0,
            paused: false,
            progress_deadline_seconds: 600,
            rules,
        }
    }

    pub fn resource(rules: Rules) -> NodeConfiguration {
        let mut obj = NodeConfiguration::new("mng", spec(rules));
        obj.metadata.uid = Some("uid-1".to_string());
        obj.metadata.generation = Some(2);
        obj
    }
}
