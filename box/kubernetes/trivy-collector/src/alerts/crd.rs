//! The `AlertRule` custom resource in the `trivy-collector.security.io` API
//! group.
//!
//! Rules used to be JSON blobs packed into one ConfigMap, which meant no
//! schema validation, no `kubectl get`, and RBAC that could only be granted
//! over the whole collection. A CRD fixes all three and gives GitOps a shape
//! it can own.
//!
//! The custom resource is camelCase, as Kubernetes objects are; the HTTP API
//! keeps the snake_case shape its clients already speak. This module owns the
//! conversion in both directions so neither convention leaks into the other.

use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::types::{
    AlertRule as ApiAlertRule, Matchers as ApiMatchers, Receiver as ApiReceiver,
    SlackReceiver as ApiSlackReceiver,
};

pub const API_GROUP: &str = "trivy-collector.security.io";
pub const API_VERSION: &str = "v1alpha1";
pub const KIND: &str = "AlertRule";
pub const PLURAL: &str = "alertrules";

/// Attribution for a rule with no recorded author, which is the normal case
/// for one applied from a Git repository rather than authored in the UI.
pub const UNKNOWN_AUTHOR: &str = "unknown";

/// Field manager used for the server-side apply that backs every write.
pub const FIELD_MANAGER: &str = "trivy-collector";

/// An alert rule: which SBOM component to watch for, and where to send the
/// notification when one lands in a workload's SBOM.
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "trivy-collector.security.io",
    version = "v1alpha1",
    kind = "AlertRule",
    plural = "alertrules",
    singular = "alertrule",
    shortname = "tcalert",
    category = "trivy-collector",
    namespaced,
    status = "AlertRuleStatus",
    doc = "An alert rule matching SBOM components across collected clusters, and the receivers a match is delivered to.",
    printcolumn = r#"{"name":"Package","type":"string","jsonPath":".spec.matchers.packageName"}"#,
    printcolumn = r#"{"name":"Version","type":"string","jsonPath":".spec.matchers.versionExpr"}"#,
    printcolumn = r#"{"name":"Enabled","type":"boolean","jsonPath":".spec.enabled"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"Fired","type":"integer","jsonPath":".status.firedCount"}"#,
    printcolumn = r#"{"name":"Last-Fired","type":"date","jsonPath":".status.lastFiredAt"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct AlertRuleSpec {
    /// Free-text note for whoever reads the rule next.
    #[serde(default)]
    pub description: String,
    /// A disabled rule is stored and listed but never evaluated.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// What has to be true of an SBOM component for the rule to fire.
    pub matchers: MatchersSpec,
    /// Alertmanager-style labels attached to the outbound notification. These
    /// are the rule author's labels, not the object's `metadata.labels`.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Alertmanager-style annotations attached to the outbound notification.
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    /// Where a firing rule delivers to. At least one is required.
    pub receivers: Vec<ReceiverSpec>,
    /// Minimum gap between two notifications for the same rule and target.
    /// Absent falls back to the evaluator's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_seconds: Option<u64>,
}

/// Condition type reported when the evaluator has accepted a rule and will
/// act on it. `False` means the rule is stored but inert, and the condition's
/// reason says why.
pub const CONDITION_READY: &str = "Ready";

/// Condition type reporting the outcome of the most recent dispatch attempt.
/// Absent until the rule has fired at least once.
pub const CONDITION_DELIVERED: &str = "Delivered";

/// What the collector has observed about a rule.
///
/// The scraper writes the evaluation fields, and only when something changed:
/// a firing, or a `Ready` transition. Writing on every ingested report would
/// mean an API call per rule per report. The server writes the audit fields,
/// once per create or edit.
///
/// Authorship lives here rather than in annotations. An annotation is part of
/// the spec object, so the server recording "bob edited this" reads as drift
/// to whatever GitOps controller owns the manifest and gets reverted on the
/// next sync. A status subresource is exempt from that by design. A rule
/// applied straight from Git has no recorded author, which is the truth:
/// nobody authored it through the API.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AlertRuleStatus {
    /// `metadata.generation` the evaluator last acted on. A value behind the
    /// current generation means the edit has not been evaluated yet, which is
    /// normal until the next SBOM report arrives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// Who created the rule through the API. Absent for one applied from a Git
    /// repository, which reads back as `unknown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    /// When the rule was last edited through the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// Who last edited it through the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
    /// When the rule last dispatched a notification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<String>,
    /// `cluster/namespace/name` of the workload the last notification was
    /// about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_workload: Option<String>,
    /// How many distinct components the last notification carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_finding_count: Option<u32>,
    /// Distinct workloads matching this rule at the last firing, the fired one
    /// included. This is the fleet-wide blast radius, not a per-report count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matching_workloads: Option<u32>,
    /// Notifications dispatched over the rule's lifetime. Cooldown suppression
    /// is not counted, because nothing was sent.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fired_count: u64,
    /// Standard Kubernetes conditions. See `CONDITION_READY` and
    /// `CONDITION_DELIVERED`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<AlertRuleCondition>,
}

/// A condition, shaped like `metav1.Condition`.
///
/// `k8s-openapi`'s own `Condition` is not reused because deriving the CRD
/// schema needs `JsonSchema`, which that crate only implements behind a
/// feature this build does not enable.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AlertRuleCondition {
    /// `Ready` or `Delivered`.
    #[serde(rename = "type")]
    pub condition_type: String,
    /// `True`, `False`, or `Unknown`.
    pub status: String,
    /// CamelCase machine-readable cause.
    pub reason: String,
    /// Human-readable detail. Empty when the reason says everything.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// When `status` last changed, not when it was last confirmed.
    pub last_transition_time: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl AlertRuleStatus {
    /// The audit half of a status, as a merge patch.
    ///
    /// A merge patch leaves keys it does not mention alone, so an edit records
    /// `updatedAt` and `updatedBy` without disturbing `createdBy` or the
    /// evaluator's counters. Returns `None` when there is nothing to record,
    /// which is the case for a rule the API server defaulted rather than a
    /// person authoring one.
    pub fn audit_patch(rule: &ApiAlertRule) -> Option<serde_json::Value> {
        let mut status = serde_json::Map::new();
        // On a create the handler leaves `updated_by` unset, and that is the
        // only moment `createdBy` should be claimed. Re-sending it on every
        // edit would let an editor overwrite the original author.
        if rule.updated_by.is_none() && rule.created_by != UNKNOWN_AUTHOR {
            status.insert(
                "createdBy".to_string(),
                serde_json::Value::String(rule.created_by.clone()),
            );
        }
        if let Some(at) = &rule.updated_at {
            status.insert(
                "updatedAt".to_string(),
                serde_json::Value::String(at.clone()),
            );
        }
        if let Some(by) = &rule.updated_by {
            status.insert(
                "updatedBy".to_string(),
                serde_json::Value::String(by.clone()),
            );
        }
        (!status.is_empty()).then(|| serde_json::json!({ "status": status }))
    }

    /// Insert or update a condition, preserving `lastTransitionTime` when the
    /// status has not actually changed.
    ///
    /// That preservation is the whole point of the field: an operator reading
    /// it wants to know how long the rule has been in this state, not when the
    /// evaluator last confirmed it.
    pub fn set_condition(
        &mut self,
        condition_type: &str,
        status: bool,
        reason: &str,
        message: &str,
        generation: Option<i64>,
    ) {
        let status = if status { "True" } else { "False" };
        let now = chrono::Utc::now().to_rfc3339();
        match self
            .conditions
            .iter_mut()
            .find(|c| c.condition_type == condition_type)
        {
            Some(existing) => {
                if existing.status != status {
                    existing.last_transition_time = now;
                }
                existing.status = status.to_string();
                existing.reason = reason.to_string();
                existing.message = message.to_string();
                existing.observed_generation = generation;
            }
            None => self.conditions.push(AlertRuleCondition {
                condition_type: condition_type.to_string(),
                status: status.to_string(),
                reason: reason.to_string(),
                message: message.to_string(),
                last_transition_time: now,
                observed_generation: generation,
            }),
        }
    }

    pub fn condition(&self, condition_type: &str) -> Option<&AlertRuleCondition> {
        self.conditions
            .iter()
            .find(|c| c.condition_type == condition_type)
    }

    /// Whether a condition currently reads `True`.
    pub fn is_condition_true(&self, condition_type: &str) -> bool {
        self.condition(condition_type)
            .is_some_and(|c| c.status == "True")
    }
}

/// Scope: SBOM component detection only. CVE and severity matching is out of
/// scope for this subsystem, handled by registry scanning elsewhere.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MatchersSpec {
    /// Component name to match, case-insensitively. Absent matches every
    /// component, which is rarely what an author means.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_name: Option<String>,
    /// Version constraint, e.g. `<2.17.0` or `>=1.0.0,<2.0.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_expr: Option<String>,
    /// Clusters to restrict the rule to. Empty matches any cluster.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clusters: Vec<String>,
    /// Workload namespace to restrict the rule to. Absent matches any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReceiverSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slack: Option<SlackReceiverSpec>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SlackReceiverSpec {
    /// Incoming-webhook URL. Must be on `https://hooks.slack.com/`; see
    /// `types::validate_webhook_url` for why the host is pinned.
    pub webhook_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

fn default_true() -> bool {
    true
}

/// The CustomResourceDefinition for this type, as pretty JSON.
///
/// The schema is derived from the Rust type the collector actually reads, so
/// the definition cannot drift from the code the way a hand-written manifest
/// does. `kubectl apply -f -` accepts JSON, and `make crd` renders this into
/// the Helm chart.
pub fn definition_json() -> Result<String, serde_json::Error> {
    use kube::CustomResourceExt;
    serde_json::to_string_pretty(&AlertRule::crd())
}

impl AlertRule {
    /// Render the custom resource in the HTTP API's rule shape.
    ///
    /// `created_at` comes from `metadata.creationTimestamp` so the API server
    /// owns it rather than a client-supplied string, and the remaining audit
    /// fields come from annotations.
    pub fn to_api(&self) -> ApiAlertRule {
        let status = self.status.as_ref();
        ApiAlertRule {
            name: self.metadata.name.clone().unwrap_or_default(),
            description: self.spec.description.clone(),
            enabled: self.spec.enabled,
            matchers: ApiMatchers {
                package_name: self.spec.matchers.package_name.clone(),
                version_expr: self.spec.matchers.version_expr.clone(),
                clusters: self.spec.matchers.clusters.clone(),
                namespace: self.spec.matchers.namespace.clone(),
            },
            labels: self.spec.labels.clone(),
            annotations: self.spec.annotations.clone(),
            receivers: self
                .spec
                .receivers
                .iter()
                .map(|r| ApiReceiver {
                    name: r.name.clone(),
                    slack: r.slack.as_ref().map(|s| ApiSlackReceiver {
                        webhook_url: s.webhook_url.clone(),
                        channel: s.channel.clone(),
                        title: s.title.clone(),
                    }),
                })
                .collect(),
            cooldown_secs: self.spec.cooldown_seconds,
            // `creationTimestamp` is second-precision UTC, and jiff renders a
            // `Timestamp` as RFC 3339, so this is the same string Kubernetes
            // itself serializes.
            created_at: self
                .metadata
                .creation_timestamp
                .as_ref()
                .map(|t| t.0.to_string())
                .unwrap_or_default(),
            created_by: status
                .and_then(|s| s.created_by.clone())
                .unwrap_or_else(|| UNKNOWN_AUTHOR.to_string()),
            updated_at: status.and_then(|s| s.updated_at.clone()),
            updated_by: status.and_then(|s| s.updated_by.clone()),
            generation: self.metadata.generation,
            status: self.status.clone(),
        }
    }

    /// Build the custom resource to apply for an HTTP API rule.
    ///
    /// Only the spec crosses over. `creationTimestamp` is assigned by the API
    /// server on the first apply and preserved on every later one, so a client
    /// cannot backdate a rule by replaying an old payload, and the audit and
    /// evaluation fields belong to the status subresource.
    pub fn from_api(rule: &ApiAlertRule, namespace: &str) -> Self {
        Self {
            metadata: ObjectMeta {
                name: Some(rule.name.clone()),
                namespace: Some(namespace.to_string()),
                ..Default::default()
            },
            spec: AlertRuleSpec {
                description: rule.description.clone(),
                enabled: rule.enabled,
                matchers: MatchersSpec {
                    package_name: rule.matchers.package_name.clone(),
                    version_expr: rule.matchers.version_expr.clone(),
                    clusters: rule.matchers.clusters.clone(),
                    namespace: rule.matchers.namespace.clone(),
                },
                labels: rule.labels.clone(),
                annotations: rule.annotations.clone(),
                receivers: rule
                    .receivers
                    .iter()
                    .map(|r| ReceiverSpec {
                        name: r.name.clone(),
                        slack: r.slack.as_ref().map(|s| SlackReceiverSpec {
                            webhook_url: s.webhook_url.clone(),
                            channel: s.channel.clone(),
                            title: s.title.clone(),
                        }),
                    })
                    .collect(),
                cooldown_seconds: rule.cooldown_secs,
            },
            // A status is never sent on the way in. It is a subresource, so
            // the main-resource apply would ignore it anyway, and the
            // evaluator is its only writer.
            status: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    use kube::{Resource, ResourceExt};

    fn api_rule() -> ApiAlertRule {
        ApiAlertRule {
            name: "log4j-critical".to_string(),
            description: "Log4Shell".to_string(),
            enabled: true,
            matchers: ApiMatchers {
                package_name: Some("log4j-core".to_string()),
                version_expr: Some("<2.17.0".to_string()),
                clusters: vec!["prod".to_string()],
                namespace: Some("payments".to_string()),
            },
            labels: BTreeMap::from([("severity".to_string(), "critical".to_string())]),
            annotations: BTreeMap::from([("runbook".to_string(), "https://wiki".to_string())]),
            receivers: vec![ApiReceiver {
                name: "sec".to_string(),
                slack: Some(ApiSlackReceiver {
                    webhook_url: "https://hooks.slack.com/services/T0/B0/x".to_string(),
                    channel: Some("#sec".to_string()),
                    title: Some("Log4Shell".to_string()),
                }),
            }],
            cooldown_secs: Some(600),
            created_at: "2026-01-02T03:04:05Z".to_string(),
            created_by: "alice@example.com".to_string(),
            updated_at: Some("2026-02-03T04:05:06+00:00".to_string()),
            updated_by: Some("bob@example.com".to_string()),
            generation: Some(2),
            status: None,
        }
    }

    /// The generated definition is what the chart ships, so a schema the API
    /// server would reject has to fail here rather than at install time.
    #[test]
    fn the_generated_definition_describes_this_type() {
        let json: serde_json::Value =
            serde_json::from_str(&definition_json().unwrap()).expect("valid JSON");
        assert_eq!(json["kind"], "CustomResourceDefinition");
        assert_eq!(
            json["metadata"]["name"],
            format!("{PLURAL}.{API_GROUP}"),
            "a CRD is named plural.group"
        );
        assert_eq!(json["spec"]["group"], API_GROUP);
        assert_eq!(json["spec"]["scope"], "Namespaced");
        assert_eq!(json["spec"]["names"]["kind"], KIND);
        assert_eq!(json["spec"]["names"]["plural"], PLURAL);

        let version = &json["spec"]["versions"][0];
        assert_eq!(version["name"], API_VERSION);
        assert_eq!(version["served"], true);
        assert_eq!(version["storage"], true);

        // The structural schema is the point of the migration: the API server
        // rejects a malformed rule before the evaluator ever reads it.
        let spec_schema = &version["schema"]["openAPIV3Schema"]["properties"]["spec"];
        let props = &spec_schema["properties"];
        assert!(props["matchers"]["properties"]["packageName"].is_object());
        assert!(props["receivers"]["items"]["properties"]["slack"].is_object());
        assert_eq!(props["cooldownSeconds"]["format"], "uint64");
        let required: Vec<&str> = spec_schema["required"]
            .as_array()
            .expect("spec has required fields")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"matchers"));
        assert!(required.contains(&"receivers"));
    }

    /// The API group is the contract the chart's CRD and every operator's
    /// `kubectl` invocation are written against, so pin it here rather than
    /// discovering a rename during an upgrade.
    #[test]
    fn the_resource_identity_is_pinned() {
        assert_eq!(AlertRule::group(&()), API_GROUP);
        assert_eq!(AlertRule::version(&()), API_VERSION);
        assert_eq!(AlertRule::kind(&()), KIND);
        assert_eq!(AlertRule::plural(&()), PLURAL);
        assert_eq!(
            AlertRule::api_version(&()),
            format!("{API_GROUP}/{API_VERSION}")
        );
    }

    #[test]
    fn an_api_rule_round_trips_through_the_custom_resource() {
        let original = api_rule();
        let mut cr = AlertRule::from_api(&original, "trivy-system");
        // Simulate what the two-call write leaves behind: the API server owns
        // creationTimestamp and generation, and the audit fields land on the
        // status subresource in a second request.
        cr.metadata.creation_timestamp = Some(Time(original.created_at.parse().expect("RFC 3339")));
        cr.metadata.generation = original.generation;
        cr.status = Some(AlertRuleStatus {
            created_by: Some(original.created_by.clone()),
            updated_at: original.updated_at.clone(),
            updated_by: original.updated_by.clone(),
            ..Default::default()
        });

        let back = cr.to_api();
        assert_eq!(back.name, original.name);
        assert_eq!(back.description, original.description);
        assert_eq!(back.enabled, original.enabled);
        assert_eq!(back.matchers.package_name, original.matchers.package_name);
        assert_eq!(back.matchers.version_expr, original.matchers.version_expr);
        assert_eq!(back.matchers.clusters, original.matchers.clusters);
        assert_eq!(back.matchers.namespace, original.matchers.namespace);
        assert_eq!(back.labels, original.labels);
        assert_eq!(back.annotations, original.annotations);
        assert_eq!(back.receivers.len(), 1);
        assert_eq!(
            back.receivers[0].slack.as_ref().unwrap().webhook_url,
            "https://hooks.slack.com/services/T0/B0/x"
        );
        assert_eq!(back.cooldown_secs, original.cooldown_secs);
        assert_eq!(back.created_at, original.created_at);
        assert_eq!(back.created_by, original.created_by);
        assert_eq!(back.updated_at, original.updated_at);
        assert_eq!(back.updated_by, original.updated_by);
        assert_eq!(back.generation, original.generation);
    }

    #[test]
    fn the_name_becomes_the_object_name_not_a_spec_field() {
        let cr = AlertRule::from_api(&api_rule(), "trivy-system");
        assert_eq!(cr.name_any(), "log4j-critical");
        let json = serde_json::to_value(&cr).unwrap();
        assert!(
            json["spec"].get("name").is_none(),
            "the rule name is metadata.name, never duplicated into the spec"
        );
        assert_eq!(json["apiVersion"], format!("{API_GROUP}/{API_VERSION}"));
        assert_eq!(json["kind"], KIND);
    }

    /// Kubernetes objects are camelCase. The HTTP API is snake_case. A drift
    /// here would silently produce a rule the API server accepts and the
    /// evaluator reads back empty.
    #[test]
    fn the_stored_spec_is_camel_case() {
        let cr = AlertRule::from_api(&api_rule(), "trivy-system");
        let json = serde_json::to_value(&cr).unwrap();
        assert_eq!(json["spec"]["matchers"]["packageName"], "log4j-core");
        assert_eq!(json["spec"]["matchers"]["versionExpr"], "<2.17.0");
        assert_eq!(json["spec"]["cooldownSeconds"], 600);
        assert_eq!(
            json["spec"]["receivers"][0]["slack"]["webhookUrl"],
            "https://hooks.slack.com/services/T0/B0/x"
        );
    }

    #[test]
    fn a_rule_applied_without_annotations_reads_back_as_unknown() {
        // The GitOps case: a plain manifest with no audit annotations.
        let cr: AlertRule = serde_json::from_value(serde_json::json!({
            "apiVersion": format!("{API_GROUP}/{API_VERSION}"),
            "kind": KIND,
            "metadata": {"name": "axios", "namespace": "trivy-system"},
            "spec": {
                "matchers": {"packageName": "axios"},
                "receivers": [{"name": "sec"}],
            }
        }))
        .unwrap();

        let api = cr.to_api();
        assert_eq!(api.name, "axios");
        assert_eq!(api.created_by, UNKNOWN_AUTHOR);
        assert_eq!(api.created_at, "", "no creationTimestamp yet");
        assert!(api.updated_at.is_none());
        assert!(api.updated_by.is_none());
        // `enabled` defaults on, otherwise a hand-written rule would be
        // stored and never evaluated.
        assert!(api.enabled);
    }

    #[test]
    fn optional_matcher_fields_are_omitted_rather_than_written_as_null() {
        let mut rule = api_rule();
        rule.matchers.version_expr = None;
        rule.matchers.clusters.clear();
        rule.matchers.namespace = None;
        rule.cooldown_secs = None;
        rule.updated_at = None;
        rule.updated_by = None;

        let cr = AlertRule::from_api(&rule, "trivy-system");
        let json = serde_json::to_value(&cr).unwrap();
        let matchers = &json["spec"]["matchers"];
        assert!(matchers.get("versionExpr").is_none());
        assert!(matchers.get("clusters").is_none());
        assert!(matchers.get("namespace").is_none());
        assert!(json["spec"].get("cooldownSeconds").is_none());

        // Nothing but the spec crosses over. Authorship is a status write,
        // so an apply must not carry an annotation the GitOps owner would
        // then see as drift.
        assert!(
            json["metadata"].get("annotations").is_none(),
            "an apply carries no audit annotations"
        );
        assert!(json.get("status").is_none(), "status is never applied");
    }

    /// A create claims `createdBy`; an edit records only the editor, so it
    /// cannot overwrite who created the rule.
    #[test]
    fn the_audit_patch_separates_a_create_from_an_edit() {
        let mut rule = api_rule();
        rule.updated_at = None;
        rule.updated_by = None;
        let create = AlertRuleStatus::audit_patch(&rule).expect("a create records its author");
        assert_eq!(create["status"]["createdBy"], "alice@example.com");
        assert!(create["status"].get("updatedBy").is_none());

        let edit = AlertRuleStatus::audit_patch(&api_rule()).expect("an edit records its editor");
        assert!(
            edit["status"].get("createdBy").is_none(),
            "an edit must not reclaim authorship"
        );
        assert_eq!(edit["status"]["updatedBy"], "bob@example.com");
        assert_eq!(edit["status"]["updatedAt"], "2026-02-03T04:05:06+00:00");
    }

    /// A rule applied from Git has no author to record, so there is nothing
    /// to write and no reason to spend an API call.
    #[test]
    fn an_unattributed_create_produces_no_patch() {
        let mut rule = api_rule();
        rule.updated_at = None;
        rule.updated_by = None;
        rule.created_by = UNKNOWN_AUTHOR.to_string();
        assert!(AlertRuleStatus::audit_patch(&rule).is_none());
    }

    /// `lastTransitionTime` answers "how long has it been like this", so it
    /// must not move when the evaluator merely re-confirms the same status.
    #[test]
    fn a_condition_keeps_its_transition_time_until_the_status_flips() {
        let mut status = AlertRuleStatus::default();
        status.set_condition(CONDITION_READY, true, "Validated", "", Some(1));
        let first = status.condition(CONDITION_READY).unwrap().clone();
        assert_eq!(first.status, "True");

        status.set_condition(CONDITION_READY, true, "Validated", "", Some(2));
        let confirmed = status.condition(CONDITION_READY).unwrap();
        assert_eq!(
            confirmed.last_transition_time, first.last_transition_time,
            "re-confirming the same status is not a transition"
        );
        assert_eq!(confirmed.observed_generation, Some(2));

        status.set_condition(CONDITION_READY, false, "InvalidVersionExpr", "bad", Some(3));
        let flipped = status.condition(CONDITION_READY).unwrap();
        assert_ne!(flipped.last_transition_time, first.last_transition_time);
        assert_eq!(flipped.reason, "InvalidVersionExpr");
        assert_eq!(flipped.message, "bad");
        assert!(!status.is_condition_true(CONDITION_READY));
    }

    /// The status is what `kubectl get` columns and the UI read, so its wire
    /// names have to stay camelCase like the spec.
    #[test]
    fn the_status_serializes_camel_case_and_omits_what_never_happened() {
        let empty = serde_json::to_value(AlertRuleStatus::default()).unwrap();
        assert_eq!(
            empty.as_object().unwrap().len(),
            0,
            "a rule that has never been evaluated carries an empty status"
        );

        let status = AlertRuleStatus {
            observed_generation: Some(4),
            last_fired_at: Some("2026-09-02T09:00:00Z".to_string()),
            last_fired_workload: Some("prod/payments/web".to_string()),
            last_finding_count: Some(3),
            matching_workloads: Some(7),
            fired_count: 12,
            ..Default::default()
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["observedGeneration"], 4);
        assert_eq!(json["lastFiredAt"], "2026-09-02T09:00:00Z");
        assert_eq!(json["lastFiredWorkload"], "prod/payments/web");
        assert_eq!(json["lastFindingCount"], 3);
        assert_eq!(json["matchingWorkloads"], 7);
        assert_eq!(json["firedCount"], 12);
    }

    /// Audit fields round-trip through the status, not through annotations.
    #[test]
    fn the_api_reads_authorship_off_the_status() {
        let mut cr = AlertRule::from_api(&api_rule(), "trivy-system");
        cr.status = Some(AlertRuleStatus {
            created_by: Some("alice@example.com".to_string()),
            updated_at: Some("2026-02-03T04:05:06+00:00".to_string()),
            updated_by: Some("bob@example.com".to_string()),
            fired_count: 2,
            ..Default::default()
        });
        cr.metadata.generation = Some(5);

        let api = cr.to_api();
        assert_eq!(api.created_by, "alice@example.com");
        assert_eq!(api.updated_by.as_deref(), Some("bob@example.com"));
        assert_eq!(api.generation, Some(5));
        assert_eq!(api.status.unwrap().fired_count, 2);
    }
}
