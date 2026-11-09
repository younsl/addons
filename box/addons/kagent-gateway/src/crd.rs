//! The `AgentAccessPolicy` custom resource. One object names the Slack
//! members who may invoke one kagent agent through a mention.

pub mod chart;

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::{CustomResource, CustomResourceExt as _, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const GROUP: &str = "access.kagent-gateway.sh";
pub const KIND: &str = "AgentAccessPolicy";
pub const PLURAL: &str = "agentaccesspolicies";

/// The API group and kind every `spec.agentRefs` entry must name.
pub const AGENT_GROUP: &str = "kagent.dev";
pub const AGENT_KIND: &str = "Agent";

/// Decides who may invoke an agent name through a Slack mention, and which
/// kagent Agents answer it.
#[derive(CustomResource, KubeSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[kube(
    group = "access.kagent-gateway.sh",
    version = "v1alpha1",
    kind = "AgentAccessPolicy",
    plural = "agentaccesspolicies",
    singular = "agentaccesspolicy",
    shortname = "aap",
    namespaced,
    status = "AgentAccessPolicyStatus",
    derive = "PartialEq",
    doc = "Decides which Slack members may invoke an agent name through a mention, and splits new threads across the kagent Agents in agentRefs by weight. The policy name is the agent name CHAT_AGENT and CHAT_AGENT_MAP route to. A name without a policy falls back to the gateway's CHAT_ALLOWED_USERS.",
    printcolumn = r#"{"name":"AllowAll","type":"boolean","jsonPath":".spec.chat.allowAll"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"AgentResolved","type":"string","jsonPath":".status.conditions[?(@.type==\"AgentResolved\")].status"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct AgentAccessPolicySpec {
    /// The kagent Agents a new thread is sent to, picked by weight like
    /// Gateway API `backendRefs`. A thread stays on the Agent it started on.
    #[schemars(length(min = 1, max = 16))]
    #[x_kube(validation = Rule::new("self.all(a, self.exists_one(b, b.name == a.name)) && self.exists(a, !has(a.weight) || a.weight > 0)").message("agentRefs must name each Agent once and give at least one a weight above 0"))]
    pub agent_refs: Vec<AgentRef>,
    /// Who may invoke the agent name through a Slack mention.
    pub chat: ChatAccess,
}

/// Identifies a kagent Agent by every field.
#[derive(KubeSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[x_kube(
    validation = Rule::new("self.group == 'kagent.dev' && self.kind == 'Agent'").message("agentRefs entries must name a kagent.dev Agent")
)]
#[serde(rename_all = "camelCase")]
pub struct AgentRef {
    /// API group of the referenced object. Always `kagent.dev`.
    #[schemars(length(min = 1, max = 253))]
    pub group: String,
    /// Kind of the referenced object. Always `Agent`.
    #[schemars(length(min = 1, max = 63))]
    pub kind: String,
    /// Name of the Agent.
    #[schemars(length(min = 1, max = 253))]
    pub name: String,
    /// Namespace of the Agent. Must be the namespace the gateway submits to
    /// (`KAGENT_NAMESPACE`), which is also where it reads policies from.
    #[schemars(length(min = 1, max = 63))]
    pub namespace: String,
    /// Share of new threads sent to this Agent, relative to the other refs.
    /// `0` sends none, which drains the Agent while its threads finish.
    #[serde(default = "default_weight")]
    #[schemars(range(min = 0, max = 1_000_000))]
    pub weight: i32,
}

const fn default_weight() -> i32 {
    1
}

/// Mention access to the agent. Exactly one of `allowedUsers` and `allowAll`
/// is set, so an empty list can never be read as "everyone".
#[derive(KubeSchema, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[x_kube(
    validation = Rule::new("(has(self.allowAll) && self.allowAll) != (has(self.allowedUsers) && size(self.allowedUsers) > 0)").message("set exactly one of allowedUsers (non-empty) and allowAll: true")
)]
#[serde(rename_all = "camelCase")]
pub struct ChatAccess {
    /// Slack member IDs (`U...` or `W...`) allowed to invoke the agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 1000), inner(length(min = 2, max = 32)))]
    #[x_kube(validation = Rule::new("self.all(u, u.matches('^[UW][A-Z0-9]+$'))").message("allowedUsers must be Slack member IDs such as U01234567"))]
    pub allowed_users: Vec<String>,
    /// Lets every member of the allowed channels invoke the agent, overriding
    /// a restrictive `CHAT_ALLOWED_USERS` for this agent alone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_all: bool,
}

/// What the gateway last observed about the policy.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentAccessPolicyStatus {
    /// The `metadata.generation` the conditions describe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// `Ready` reports whether the gateway enforces the policy as written.
    /// `AgentResolved` reports whether the referenced Agent exists.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

impl AgentAccessPolicy {
    /// The generated `CustomResourceDefinition`.
    pub fn definition() -> CustomResourceDefinition {
        Self::crd()
    }
}
