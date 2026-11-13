//! The `NodeConfigurationBinding` custom resource: one revision of one
//! `NodeConfiguration` assigned to one node, the way a `Pod` is one replica of
//! a `Deployment`.

use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Rules;
use crate::payload::fnv1a;

pub const CONFIGURATION_LABEL: &str = "node-configuration-operator.sh/configuration";
pub const NODE_LABEL: &str = "node-configuration-operator.sh/node";

/// The revision of a `NodeConfiguration` that one node applies. The controller
/// creates and updates it, and the agent on the node reports the result in the
/// status. Deleting it makes the controller assign the current revision again.
#[derive(CustomResource, KubeSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[kube(
    group = "node-configuration-operator.sh",
    version = "v1alpha1",
    kind = "NodeConfigurationBinding",
    plural = "nodeconfigurationbindings",
    singular = "nodeconfigurationbinding",
    shortname = "ncb",
    status = "NodeConfigurationBindingStatus",
    derive = "PartialEq",
    doc = "One revision of a NodeConfiguration assigned to one node. Managed by the controller.",
    printcolumn = r#"{"name":"Configuration","type":"string","jsonPath":".spec.configuration"}"#,
    printcolumn = r#"{"name":"Node","type":"string","jsonPath":".spec.nodeName"}"#,
    printcolumn = r#"{"name":"Revision","type":"string","jsonPath":".spec.revision"}"#,
    printcolumn = r#"{"name":"Applied","type":"string","jsonPath":".status.observedRevision"}"#,
    printcolumn = r#"{"name":"Ready","type":"boolean","jsonPath":".status.ready"}"#,
    printcolumn = r#"{"name":"Failed","type":"integer","jsonPath":".status.failedRules"}"#,
    printcolumn = r#"{"name":"Status Age","type":"date","jsonPath":".status.lastTransitionTime"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct NodeConfigurationBindingSpec {
    /// Name of the `NodeConfiguration` this revision comes from.
    pub configuration: String,
    /// Name of the node that applies it.
    pub node_name: String,
    /// The revision hash of everything below it.
    pub revision: String,
    /// The group whose bindings run in order on the node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Order inside the group, highest first.
    #[serde(default)]
    pub priority: i32,
    /// Report the changes without writing them.
    #[serde(default)]
    pub dry_run: bool,
    /// Seconds between two runs on the node.
    pub interval_seconds: u32,
    /// The rules of this revision, run in list order.
    #[schemars(
        length(max = 256),
        extend("x-kubernetes-list-type" = "map", "x-kubernetes-list-map-keys" = ["name"])
    )]
    pub rules: Rules,
}

/// What the agent on the node reports.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeConfigurationBindingStatus {
    /// The revision of the last run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_revision: Option<String>,
    /// The last run applied every rule without a failure.
    #[serde(default)]
    pub ready: bool,
    /// Rules that failed in the last run.
    #[serde(default)]
    pub failed_rules: i32,
    /// The first failure of the last run, the binding this one waits for, or
    /// the number of changes a dry run found.
    #[serde(default)]
    pub message: Option<String>,
    /// The result of every rule in the last run, in rule order.
    #[serde(default)]
    #[schemars(extend("x-kubernetes-list-type" = "map", "x-kubernetes-list-map-keys" = ["name"]))]
    pub rules: Vec<RuleStatus>,
    /// When `observedRevision` or `ready` last changed, in RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<String>,
}

/// What the last run did with one rule.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuleStatus {
    /// Name of the rule.
    pub name: String,
    /// `unchanged` when the node was in line, `changed` when the rule
    /// corrected it, `pending` when a dry run found a difference, `skipped`
    /// when the target does not exist on the node, or `failed`.
    pub outcome: String,
    /// What changed, is pending, or failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The node name as a label value, hashed when it is too long for one.
pub fn node_key(node_name: &str) -> String {
    if node_name.len() <= 63 {
        node_name.to_string()
    } else {
        format!("node-{:016x}", fnv1a(node_name.as_bytes()))
    }
}

pub fn name(configuration: &str, node_name: &str) -> String {
    format!("{configuration}.{}", node_key(node_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_stay_within_label_and_object_limits() {
        let short = "ip-10-0-1-2.ap-northeast-2.compute.internal";
        assert_eq!(node_key(short), short);
        assert_eq!(name("os-hardening", short), format!("os-hardening.{short}"));

        let long = "n".repeat(64);
        let key = node_key(&long);
        assert_eq!(key.len(), 21);
        assert!(key.starts_with("node-"));
        assert_eq!(node_key(&long), key);
        assert_ne!(node_key(&"m".repeat(64)), key);
    }
}
