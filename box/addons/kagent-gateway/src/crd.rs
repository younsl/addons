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
    doc = "Routes Slack mentions and alerts to kagent Agents, like a Gateway API route, and decides which Slack members may invoke them. spec.match picks the mentions and alerts the policy takes, agentRefs the Agents that answer by weight. Anything no policy matches is dropped.",
    printcolumn = r#"{"name":"AllowAll","type":"boolean","jsonPath":".spec.chat.allowAll"}"#,
    printcolumn = r#"{"name":"Accepted","type":"string","jsonPath":".status.conditions[?(@.type==\"Accepted\")].status"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"AgentResolved","type":"string","jsonPath":".status.conditions[?(@.type==\"AgentResolved\")].status"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[x_kube(
    validation = Rule::new("!has(self.alertChannel) || has(self.match.alertLabel)").message("alertChannel needs match.alertLabel, the alerts it applies to")
)]
#[serde(rename_all = "camelCase")]
pub struct AgentAccessPolicySpec {
    /// The mentions and alerts this policy takes, like a Gateway API route
    /// match.
    #[serde(rename = "match")]
    pub route_match: RouteMatch,
    /// Slack channel, by name or conversation ID, that the alerts this policy
    /// matches thread in. Unset uses the matched label value as the channel
    /// name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 80))]
    #[x_kube(validation = Rule::new("!self.contains(' ') && !self.contains('*')").message("alertChannel must be one channel name or ID"))]
    pub alert_channel: Option<String>,
    /// The kagent Agents a new thread is sent to, picked by weight like
    /// Gateway API `backendRefs`. A thread stays on the Agent it started on.
    #[schemars(length(min = 1, max = 16))]
    #[x_kube(validation = Rule::new("self.all(a, self.exists_one(b, b.name == a.name)) && self.exists(a, !has(a.weight) || a.weight > 0) && self.map(a, a.namespace).all(n, n == self[0].namespace)").message("agentRefs must name each Agent once, give at least one a weight above 0, and share one namespace"))]
    pub agent_refs: Vec<AgentRef>,
    /// Who may invoke the agent name through a Slack mention.
    pub chat: ChatAccess,
}

/// Which mentions and alerts a policy takes. When two policies claim the
/// same channel or label value, the older one keeps it and the newer one
/// reports `Accepted=False`.
#[derive(KubeSchema, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[x_kube(
    validation = Rule::new("(has(self.slackChannels) && size(self.slackChannels) > 0) || has(self.alertLabel)").message("match needs slackChannels or alertLabel")
)]
#[serde(rename_all = "camelCase")]
pub struct RouteMatch {
    /// Slack channels, by name or conversation ID, whose mentions this
    /// policy answers. `*` takes every channel no other policy names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64), inner(length(min = 1, max = 80)))]
    #[x_kube(validation = Rule::new("self.all(c, self.exists_one(d, d == c) && !c.contains(' ') && (c == '*' || !c.contains('*')))").message("slackChannels entries must be unique channel names or IDs without spaces, or the single catch-all *"))]
    pub slack_channels: Vec<String>,
    /// The alert label whose value routes an alert to this policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert_label: Option<LabelMatch>,
}

/// Matches an alert when its label `name` holds one of `values`.
#[derive(KubeSchema, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LabelMatch {
    /// Label name, read from the labels every alert in the group shares,
    /// then from the group labels.
    #[schemars(
        length(min = 1, max = 63),
        regex(pattern = r"^[a-zA-Z_][a-zA-Z0-9_]*$")
    )]
    pub name: String,
    /// Label values that route to this policy. `*` alone takes every alert
    /// no other policy matches, whatever its labels.
    #[schemars(length(min = 1, max = 64), inner(length(min = 1, max = 128)))]
    #[x_kube(validation = Rule::new("self.all(v, self.exists_one(w, w == v) && v.trim() == v && (v == '*' || !v.contains('*'))) && (!self.exists(v, v == '*') || size(self) == 1)").message("values must be unique, carry no surrounding spaces, and use * only as the single catch-all value"))]
    pub values: Vec<String>,
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
