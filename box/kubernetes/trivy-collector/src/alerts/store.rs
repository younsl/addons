//! CRUD for alert rules, backed by one `AlertRule` custom resource per rule.
//!
//! Reads and writes go through the API server, not a cache, so both pods see
//! the same rules without coordinating: the scraper reloads on every
//! evaluation pass and the server reads on request. Writes are server-side
//! applies, which makes create and update the same call and lets the API
//! server reject a malformed spec against the CRD schema before it is ever
//! stored.

use kube::{
    Client,
    api::{Api, DeleteParams, Patch, PatchParams},
};
use thiserror::Error;
use tracing::{debug, info, warn};

use super::crd::{self, AlertRuleStatus, FIELD_MANAGER};
use super::types::{AlertRule, validate_rule_name, validate_webhook_url};

#[derive(Debug, Error)]
pub enum AlertStoreError {
    #[error("invalid rule: {0}")]
    Invalid(String),
    #[error("rule not found: {0}")]
    NotFound(String),
    #[error(
        "the {kind}.{group} CRD is not installed in this cluster; apply it (the Helm chart ships it under crds.install) before managing alert rules",
        kind = crd::PLURAL,
        group = crd::API_GROUP,
    )]
    CrdMissing,
    #[error("kube API error: {0}")]
    Kube(#[from] kube::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

#[derive(Clone)]
pub struct AlertStore {
    client: Client,
    namespace: String,
}

impl AlertStore {
    pub fn new(client: Client, namespace: String) -> Self {
        Self { client, namespace }
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// `group/version` of the resource the rules are stored as, so the API and
    /// the UI can name what they are reading rather than hardcoding it.
    pub fn api_version(&self) -> String {
        format!("{}/{}", crd::API_GROUP, crd::API_VERSION)
    }

    pub fn resource(&self) -> &'static str {
        crd::PLURAL
    }

    /// The typed API for the rules. Public so `alerts::readiness` can watch
    /// them without being handed a second client.
    pub fn api(&self) -> Api<crd::AlertRule> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// Confirm the CRD is registered. A missing CRD is an install-time
    /// mistake, not a runtime condition, and it is worth one clear line at
    /// startup rather than a 404 on the operator's first write.
    pub async fn preflight(&self) -> Result<(), AlertStoreError> {
        self.api()
            .list_metadata(&Default::default())
            .await
            .map(|_| ())
            .map_err(collection_error)
    }

    pub async fn list(&self) -> Result<Vec<AlertRule>, AlertStoreError> {
        let list = self
            .api()
            .list(&Default::default())
            .await
            .map_err(collection_error)?;
        let mut rules: Vec<AlertRule> = list.items.iter().map(crd::AlertRule::to_api).collect();
        rules.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(rules)
    }

    pub async fn get(&self, name: &str) -> Result<AlertRule, AlertStoreError> {
        // A missing CRD and a missing rule are both a 404 here and cannot be
        // told apart, so this reports the rule. `preflight` is what surfaces
        // the install-time mistake.
        match self.api().get_opt(name).await? {
            Some(cr) => Ok(cr.to_api()),
            None => Err(AlertStoreError::NotFound(name.to_string())),
        }
    }

    /// Create or replace a rule and return what the API server stored, so the
    /// caller reports server truth (`creationTimestamp`, defaulted fields)
    /// rather than the payload it sent.
    pub async fn upsert(&self, rule: &AlertRule) -> Result<AlertRule, AlertStoreError> {
        validate(rule)?;
        let cr = crd::AlertRule::from_api(rule, &self.namespace);
        // Server-side apply: one call for both create and update, and `force`
        // takes ownership of fields a previous manager (a `kubectl apply`, a
        // GitOps controller) still claims, which would otherwise make the
        // write a conflict instead of an edit.
        let params = PatchParams::apply(FIELD_MANAGER).force();
        match self
            .api()
            .patch(&rule.name, &params, &Patch::Apply(&cr))
            .await
        {
            Ok(stored) => {
                debug!(rule = %rule.name, "Applied AlertRule");
                let mut api = stored.to_api();
                // Authorship goes onto the status subresource, which is a
                // second call. A failure there is not a failed write: the rule
                // exists and will be evaluated, only the audit trail is short,
                // so it is logged and the create still succeeds.
                if let Some(patch) = AlertRuleStatus::audit_patch(rule) {
                    match self.patch_status(&rule.name, &patch).await {
                        Ok(updated) => api = updated.to_api(),
                        Err(e) => {
                            warn!(rule = %rule.name, error = %e, "Failed to record alert rule authorship")
                        }
                    }
                }
                Ok(api)
            }
            // An apply creates the object when it is absent, so a 404 here is
            // never a missing rule — only a missing resource type.
            Err(kube::Error::Api(s)) if s.is_not_found() => Err(AlertStoreError::CrdMissing),
            // The API server validates the spec against the CRD schema. That
            // is the caller's mistake, not ours, so keep it a 400.
            Err(kube::Error::Api(s)) if s.is_invalid() || s.code == 400 => {
                Err(AlertStoreError::Invalid(s.message.clone()))
            }
            Err(e) => Err(AlertStoreError::Kube(e)),
        }
    }

    /// Merge-patch the status subresource.
    ///
    /// Status is a subresource, so this cannot touch the spec no matter what
    /// the patch says, and a GitOps controller that owns the spec sees no
    /// drift from it.
    pub async fn patch_status(
        &self,
        name: &str,
        patch: &serde_json::Value,
    ) -> Result<crd::AlertRule, AlertStoreError> {
        match self
            .api()
            .patch_status(name, &PatchParams::default(), &Patch::Merge(patch))
            .await
        {
            Ok(updated) => Ok(updated),
            Err(kube::Error::Api(s)) if s.is_not_found() => {
                Err(AlertStoreError::NotFound(name.to_string()))
            }
            Err(e) => Err(AlertStoreError::Kube(e)),
        }
    }

    /// Record that a rule fired.
    ///
    /// `previous` is the status the evaluator already read on this pass, so
    /// the lifetime counter increments without a re-read. The scraper is a
    /// single replica by design, which is what makes that safe.
    pub async fn record_firing(
        &self,
        name: &str,
        previous: Option<&AlertRuleStatus>,
        outcome: FiringOutcome<'_>,
    ) -> Result<(), AlertStoreError> {
        let mut status = previous.cloned().unwrap_or_default();
        status.observed_generation = outcome.generation;
        status.last_fired_at = Some(chrono::Utc::now().to_rfc3339());
        status.last_fired_workload = Some(outcome.workload.to_string());
        status.last_finding_count = Some(outcome.finding_count);
        status.matching_workloads = Some(outcome.matching_workloads);
        status.fired_count = status.fired_count.saturating_add(1);
        match outcome.failure {
            None => status.set_condition(
                crd::CONDITION_DELIVERED,
                true,
                "Delivered",
                "",
                outcome.generation,
            ),
            Some(msg) => status.set_condition(
                crd::CONDITION_DELIVERED,
                false,
                "DeliveryFailed",
                msg,
                outcome.generation,
            ),
        }
        // A rule that fired is by definition one the evaluator accepted.
        status.set_condition(
            crd::CONDITION_READY,
            true,
            "Validated",
            "",
            outcome.generation,
        );
        self.patch_status(name, &serde_json::json!({ "status": status }))
            .await?;
        Ok(())
    }

    /// Record that the evaluator will act on a rule.
    ///
    /// Separate from `record_firing` on purpose: readiness is true as soon as
    /// the rule is well formed, and most rules never fire. Folding it into the
    /// firing path left every correct rule with an empty `Ready`.
    pub async fn record_ready(
        &self,
        name: &str,
        previous: Option<&AlertRuleStatus>,
        generation: Option<i64>,
    ) -> Result<(), AlertStoreError> {
        let mut status = previous.cloned().unwrap_or_default();
        status.observed_generation = generation;
        status.set_condition(crd::CONDITION_READY, true, "Validated", "", generation);
        self.patch_status(name, &serde_json::json!({ "status": status }))
            .await?;
        Ok(())
    }

    /// Record that the evaluator cannot act on a rule, with the reason.
    ///
    /// Without this an unparseable version expression makes a rule silently
    /// inert: it is listed, it looks enabled, and it never fires.
    pub async fn record_not_ready(
        &self,
        name: &str,
        previous: Option<&AlertRuleStatus>,
        generation: Option<i64>,
        reason: &str,
        message: &str,
    ) -> Result<(), AlertStoreError> {
        let mut status = previous.cloned().unwrap_or_default();
        status.observed_generation = generation;
        status.set_condition(crd::CONDITION_READY, false, reason, message, generation);
        self.patch_status(name, &serde_json::json!({ "status": status }))
            .await?;
        Ok(())
    }

    pub async fn delete(&self, name: &str) -> Result<(), AlertStoreError> {
        match self.api().delete(name, &DeleteParams::default()).await {
            Ok(_) => {
                info!(rule = %name, "Deleted AlertRule");
                Ok(())
            }
            Err(kube::Error::Api(s)) if s.is_not_found() => {
                Err(AlertStoreError::NotFound(name.to_string()))
            }
            Err(e) => Err(AlertStoreError::Kube(e)),
        }
    }
}

/// What a firing recorded onto a rule's status.
pub struct FiringOutcome<'a> {
    /// `cluster/namespace/name` of the workload that triggered it.
    pub workload: &'a str,
    /// Distinct components the notification carried.
    pub finding_count: u32,
    /// Distinct workloads matching the rule, the fired one included.
    pub matching_workloads: u32,
    /// `metadata.generation` the evaluator acted on.
    pub generation: Option<i64>,
    /// Delivery error, when every receiver failed.
    pub failure: Option<&'a str>,
}

/// Validate what the CRD schema cannot express.
///
/// The schema pins field names, types, and which are required. It cannot parse
/// a version expression or pin the webhook host, so those stay here. The name
/// check is duplicated with the API server's own object-name validation on
/// purpose: rejecting it locally returns a message about a rule rather than
/// about a Kubernetes object.
fn validate(rule: &AlertRule) -> Result<(), AlertStoreError> {
    validate_rule_name(&rule.name).map_err(|e| AlertStoreError::Invalid(e.to_string()))?;
    if rule.receivers.is_empty() {
        return Err(AlertStoreError::Invalid(
            "at least one receiver is required".into(),
        ));
    }
    for r in &rule.receivers {
        if let Some(slack) = &r.slack
            && let Err(msg) = validate_webhook_url(&slack.webhook_url)
        {
            return Err(AlertStoreError::Invalid(format!(
                "receiver '{}' slack {}",
                r.name, msg
            )));
        }
    }
    if let Some(expr) = &rule.matchers.version_expr {
        super::expr::VersionExpr::parse(expr)
            .map_err(|e| AlertStoreError::Invalid(format!("version_expr: {}", e)))?;
    }
    Ok(())
}

/// Map an error from a *collection* request. Listing a registered CRD returns
/// an empty list, never a 404, so a 404 on a collection means the API server
/// has never heard of the resource type and the CRD is not installed.
fn collection_error(err: kube::Error) -> AlertStoreError {
    match err {
        kube::Error::Api(s) if s.is_not_found() => AlertStoreError::CrdMissing,
        e => AlertStoreError::Kube(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::types::{Matchers, Receiver, SlackReceiver};

    fn rule(name: &str) -> AlertRule {
        AlertRule {
            name: name.to_string(),
            description: String::new(),
            enabled: true,
            matchers: Matchers {
                package_name: Some("log4j-core".to_string()),
                version_expr: None,
                clusters: vec![],
                namespace: None,
            },
            labels: Default::default(),
            annotations: Default::default(),
            receivers: vec![Receiver {
                name: "sec".to_string(),
                slack: Some(SlackReceiver {
                    webhook_url: "https://hooks.slack.com/services/T0/B0/x".to_string(),
                    channel: None,
                    title: None,
                }),
            }],
            cooldown_secs: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            created_by: "alice".to_string(),
            updated_at: None,
            updated_by: None,
            generation: None,
            status: None,
        }
    }

    #[test]
    fn a_well_formed_rule_validates() {
        assert!(validate(&rule("log4j")).is_ok());
    }

    #[test]
    fn a_rule_with_no_receivers_is_rejected() {
        let mut r = rule("log4j");
        r.receivers.clear();
        let err = validate(&r).expect_err("must reject");
        assert!(matches!(err, AlertStoreError::Invalid(_)));
        assert!(err.to_string().contains("receiver"));
    }

    /// Pinning the webhook host is the SSRF guard; it has to hold on the
    /// storage path too, not only in the HTTP handler.
    #[test]
    fn a_non_slack_webhook_is_rejected_and_names_the_receiver() {
        let mut r = rule("log4j");
        r.receivers[0].slack.as_mut().unwrap().webhook_url =
            "http://169.254.169.254/latest/meta-data/".to_string();
        let err = validate(&r).expect_err("must reject");
        assert!(err.to_string().contains("receiver 'sec'"));
        assert!(err.to_string().contains("hooks.slack.com"));
    }

    #[test]
    fn an_unparseable_version_expression_is_rejected() {
        let mut r = rule("log4j");
        r.matchers.version_expr = Some(">=".to_string());
        let err = validate(&r).expect_err("must reject");
        assert!(err.to_string().contains("version_expr"));
    }

    #[test]
    fn an_invalid_rule_name_is_rejected() {
        let err = validate(&rule("has spaces")).expect_err("must reject");
        assert!(err.to_string().contains("rule name"));
    }

    /// The message is what an operator sees when the chart was installed with
    /// `crds.install=false` and nothing applied the CRD, so it has to say what
    /// to do rather than just fail.
    #[test]
    fn the_missing_crd_error_names_the_resource_and_the_fix() {
        let msg = AlertStoreError::CrdMissing.to_string();
        assert!(msg.contains("alertrules.trivy-collector.security.io"));
        assert!(msg.contains("crds.install"));
    }

    fn api_error(code: u16, message: &str, reason: &str) -> kube::Error {
        kube::Error::Api(
            kube::core::Status::failure(message, reason)
                .with_code(code)
                .boxed(),
        )
    }

    #[test]
    fn a_404_on_a_collection_is_read_as_a_missing_crd() {
        let err = collection_error(api_error(
            404,
            "the server could not find the requested resource",
            "NotFound",
        ));
        assert!(matches!(err, AlertStoreError::CrdMissing));
    }

    #[test]
    fn other_collection_failures_stay_kube_errors() {
        let err = collection_error(api_error(403, "forbidden", "Forbidden"));
        assert!(matches!(err, AlertStoreError::Kube(_)));
    }
}
