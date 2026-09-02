//! Match an incoming SBOM report against alert rules and dispatch to
//! receivers. Rules are re-listed from the API server on each evaluation
//! pass, and a per-rule, per-target cooldown prevents Slack flooding when
//! reports arrive frequently. Matches from a single report are collected per
//! rule and dispatched as one grouped message to avoid per-finding fan-out.
//!
//! What happened is written back to each rule's status subresource, but only
//! when it changed: a firing, or a `Ready` transition. A write per rule per
//! report would put the API server on the ingest path.
//!
//! Scope: SBOM component detection only. Vulnerability/CVE-based alerting
//! is out of scope for this subsystem.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tracing::{debug, error};

use super::expr::VersionExpr;
use super::notifier::{AlertContext, SlackNotifier, TestDeliveryResult};
use super::store::{AlertStore, FiringOutcome};
use super::types::AlertRule;
use crate::collector::types::{ReportPayload, SbomReportData};
use crate::storage::{ReportStore, SbomComponentMatch};

const DEFAULT_COOLDOWN_SECS: u64 = 3600;

#[derive(Clone)]
pub struct AlertEvaluator {
    store: AlertStore,
    notifier: SlackNotifier,
    cooldown: Arc<Mutex<HashMap<String, Instant>>>,
}

impl AlertEvaluator {
    pub fn new(store: AlertStore) -> Self {
        Self::with_external_url(store, None)
    }

    pub fn with_external_url(store: AlertStore, external_url: Option<String>) -> Self {
        Self {
            store,
            notifier: SlackNotifier::with_external_url(external_url),
            cooldown: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn store(&self) -> &AlertStore {
        &self.store
    }

    /// Send a test alert built from real SBOM reports already stored in the
    /// DB. The message is indistinguishable from a production firing so the
    /// operator sees what an actual alert will look like rather than mock
    /// placeholders. A test-mode footer is appended noting how many other
    /// workloads would also fire if real ingestion happened, so the
    /// operator can gauge fleet-wide impact without flooding the channel.
    /// Returns `Err(TestRunError::NoMatches)` when no current report
    /// matches the rule's matchers — surfaced as 422 so the operator can
    /// adjust matchers or wait for a matching report.
    pub async fn test_with_rule(
        &self,
        rule: AlertRule,
        db: &dyn ReportStore,
    ) -> Result<Vec<TestDeliveryResult>, TestRunError> {
        let (contexts, other_workloads) = build_test_contexts(db, &rule).await?;
        Ok(self
            .notifier
            .send_test(&rule, &contexts, other_workloads)
            .await)
    }

    /// Evaluate a freshly received report against all enabled rules. Only
    /// SBOM reports are considered — vulnerability reports are ignored even
    /// if delivered, since this subsystem only fires on package presence.
    /// `db` is used to compute a fleet-wide "also matches N workloads"
    /// scope hint that gets surfaced on the outbound message.
    pub async fn evaluate(
        &self,
        payload: &ReportPayload,
        prev_data_json: Option<&str>,
        db: &dyn ReportStore,
    ) {
        if !payload.report_type.eq_ignore_ascii_case("sbomreport") {
            return;
        }
        let rules = match self.store.list().await {
            Ok(r) => r,
            Err(e) => {
                error!(error = %e, "Failed to load alert rules");
                return;
            }
        };
        if rules.is_empty() {
            return;
        }
        // Compute "already seen" component keys from the previous report
        // once per evaluation pass. Empty set when prev is missing or
        // unparseable; that case treats every match as new.
        let prev_keys = prev_data_json.map(extract_finding_keys).unwrap_or_default();
        for rule in &rules {
            // Readiness is a property of the rule, not of the report in hand,
            // so it is reconciled before the cluster and namespace narrowing
            // below. Otherwise a rule scoped to other clusters would never say
            // anything about itself.
            self.reconcile_readiness(rule).await;
        }
        for rule in rules.iter().filter(|r| r.enabled) {
            if !rule.matchers.clusters.is_empty()
                && !rule.matchers.clusters.contains(&payload.cluster)
            {
                continue;
            }
            if let Some(ns) = &rule.matchers.namespace
                && ns != &payload.namespace
            {
                continue;
            }
            self.evaluate_sbom(rule, payload, &prev_keys, db).await;
        }
    }

    /// Make the rule's `Ready` condition say whether the evaluator will act on
    /// it, writing only when that answer has changed.
    ///
    /// Readiness used to be recorded only as a side effect of firing, which
    /// left every correct-but-unfired rule with no condition at all. That is
    /// the state most rules live in, and a blank `READY` column was
    /// indistinguishable from a rule the scraper had never looked at.
    ///
    /// The transition check is what keeps this off the hot path: after the
    /// first pass the answer is already stored, so no further writes happen no
    /// matter how many reports arrive. The status was read from the same list
    /// call that produced `rule`, so the check costs no extra request.
    async fn reconcile_readiness(&self, rule: &AlertRule) {
        let Readiness {
            ready,
            reason,
            message,
        } = readiness_for(rule);
        if readiness_already_recorded(rule, ready, reason, &message) {
            return;
        }

        let result = if ready {
            self.store
                .record_ready(&rule.name, rule.status.as_ref(), rule.generation)
                .await
        } else {
            self.store
                .record_not_ready(
                    &rule.name,
                    rule.status.as_ref(),
                    rule.generation,
                    reason,
                    &message,
                )
                .await
        };
        if let Err(e) = result {
            debug!(rule = %rule.name, error = %e, "Failed to record alert rule readiness");
        }
    }

    async fn evaluate_sbom(
        &self,
        rule: &AlertRule,
        payload: &ReportPayload,
        prev_keys: &HashSet<String>,
        db: &dyn ReportStore,
    ) {
        let parsed: serde_json::Result<SbomReportEnvelope> =
            serde_json::from_str(&payload.data_json);
        let components = match parsed {
            Ok(env) => env.report.components.components,
            Err(e) => {
                debug!(error = %e, "Failed to parse SBOM payload");
                return;
            }
        };
        // An unparseable expression is already reported as Ready=False by
        // `reconcile_readiness`, so this only has to decline to match.
        let expr = match rule.matchers.version_expr.as_deref() {
            Some(s) => match VersionExpr::parse(s) {
                Ok(e) => Some(e),
                Err(_) => return,
            },
            None => None,
        };

        let mut contexts: Vec<AlertContext> = Vec::new();
        // Dedup by (name, version) within this single SBOM evaluation pass.
        // Trivy can list the same package twice with different `bom-ref`
        // values when an image has multiple binaries that each link the
        // same library — surfacing both as separate alert rows is noise.
        let mut seen: HashSet<String> = HashSet::new();
        for c in components {
            if let Some(name) = &rule.matchers.package_name
                && !name.eq_ignore_ascii_case(&c.name)
            {
                continue;
            }
            if let Some(ref e) = expr
                && !e.matches(&c.version)
            {
                continue;
            }
            let finding_key = format!("{}|{}", c.name, c.version);
            if !seen.insert(finding_key.clone()) {
                continue;
            }
            // Diff-aware: skip components already present in prior SBOM.
            // Key shape must match `extract_finding_keys`.
            if prev_keys.contains(&finding_key) {
                continue;
            }
            let key = format!(
                "{}|{}|{}|{}|{}|{}",
                rule.name, payload.cluster, payload.namespace, payload.name, c.name, c.version
            );
            if !self.try_acquire(&key, rule.cooldown_secs).await {
                continue;
            }
            let pkg_type = (!c.component_type.is_empty()).then(|| c.component_type.clone());
            contexts.push(AlertContext {
                cluster: payload.cluster.clone(),
                namespace: payload.namespace.clone(),
                name: payload.name.clone(),
                report_type: payload.report_type.clone(),
                package: c.name,
                version: c.version,
                pkg_type,
            });
        }
        if contexts.is_empty() {
            return;
        }
        // Count other workloads in the DB that would also match this rule.
        // Surfaced on the message so a recipient can immediately tell
        // whether this is an isolated finding or fleet-wide.
        let other_workloads = count_other_matching_workloads(
            db,
            rule,
            &payload.cluster,
            &payload.namespace,
            &payload.name,
        )
        .await
        .unwrap_or(0);
        let outcome = self.notifier.fire(rule, &contexts, other_workloads).await;
        self.record_firing(rule, payload, contexts.len(), other_workloads, &outcome)
            .await;
    }

    /// Put the firing on the rule's status subresource. Best-effort: the
    /// notification has already gone out, and losing the record of it must not
    /// look like a failed alert.
    async fn record_firing(
        &self,
        rule: &AlertRule,
        payload: &ReportPayload,
        finding_count: usize,
        other_workloads: usize,
        outcome: &super::notifier::FireOutcome,
    ) {
        let workload = format!("{}/{}/{}", payload.cluster, payload.namespace, payload.name);
        let result = self
            .store
            .record_firing(
                &rule.name,
                rule.status.as_ref(),
                FiringOutcome {
                    workload: &workload,
                    finding_count: finding_count as u32,
                    // The scope hint counts the others; the blast radius
                    // includes the workload that just fired.
                    matching_workloads: other_workloads.saturating_add(1) as u32,
                    generation: rule.generation,
                    failure: outcome
                        .all_failed()
                        .then_some(())
                        .and(outcome.last_error.as_deref()),
                },
            )
            .await;
        if let Err(e) = result {
            debug!(rule = %rule.name, error = %e, "Failed to record alert firing on the rule status");
        }
    }

    async fn try_acquire(&self, key: &str, cooldown_secs: Option<u64>) -> bool {
        let cd = Duration::from_secs(cooldown_secs.unwrap_or(DEFAULT_COOLDOWN_SECS));
        let now = Instant::now();
        let mut map = self.cooldown.lock().await;
        if let Some(prev) = map.get(key)
            && now.duration_since(*prev) < cd
        {
            return false;
        }
        map.insert(key.to_string(), now);
        if map.len() > 4096 {
            map.retain(|_, t| now.duration_since(*t) < cd);
        }
        true
    }
}

/// What a rule's `Ready` condition should say.
struct Readiness {
    ready: bool,
    reason: &'static str,
    message: String,
}

/// Decide whether the evaluator will act on a rule.
///
/// Readiness does not depend on whether the rule has ever matched anything: a
/// rule watching a package nobody runs is correct and ready, it simply has
/// nothing to fire on.
fn readiness_for(rule: &AlertRule) -> Readiness {
    if !rule.enabled {
        return Readiness {
            ready: false,
            reason: "Disabled",
            message: String::new(),
        };
    }
    match rule.matchers.version_expr.as_deref() {
        Some(expr) => match VersionExpr::parse(expr) {
            Ok(_) => Readiness {
                ready: true,
                reason: "Validated",
                message: String::new(),
            },
            Err(message) => Readiness {
                ready: false,
                reason: "InvalidVersionExpr",
                message,
            },
        },
        None => Readiness {
            ready: true,
            reason: "Validated",
            message: String::new(),
        },
    }
}

/// Whether the rule's stored condition already says exactly this, in which
/// case there is nothing to write. `observedGeneration` is part of the
/// comparison so an edit is re-acknowledged even when the verdict is
/// unchanged.
fn readiness_already_recorded(rule: &AlertRule, ready: bool, reason: &str, message: &str) -> bool {
    rule.status
        .as_ref()
        .and_then(|s| s.condition(super::crd::CONDITION_READY))
        .is_some_and(|c| {
            c.status == if ready { "True" } else { "False" }
                && c.reason == reason
                && c.message == message
                && c.observed_generation == rule.generation
        })
}

#[derive(serde::Deserialize)]
struct SbomReportEnvelope {
    #[serde(default)]
    report: SbomReportData,
}

const TEST_MAX_FINDINGS: usize = 10;

#[derive(Debug, thiserror::Error)]
pub enum TestRunError {
    #[error("storage error: {0}")]
    Storage(String),
    #[error("no current reports match the rule's matchers")]
    NoMatches,
    #[error("invalid version expression: {0}")]
    InvalidExpr(String),
}

/// Pull every SBOM component row matching the rule's row-level filters
/// (cluster, namespace, package_name) at SQL level, then layer the
/// `version_expr` filter on top in Rust. Replaces the prior approach of
/// loading the 200 most-recent reports and parsing each one — that cap
/// silently dropped components living in older SBOMs from the rule
/// editor's preview, test, and fleet-count surfaces.
async fn fetch_component_matches(
    db: &dyn ReportStore,
    rule: &AlertRule,
) -> Result<(Vec<SbomComponentMatch>, Option<VersionExpr>), TestRunError> {
    let expr = match rule.matchers.version_expr.as_deref() {
        Some(s) => Some(VersionExpr::parse(s).map_err(TestRunError::InvalidExpr)?),
        None => None,
    };
    let rows = db
        .list_sbom_component_matches(
            &rule.matchers.clusters,
            rule.matchers.namespace.as_deref(),
            rule.matchers.package_name.as_deref(),
        )
        .await
        .map_err(|e| TestRunError::Storage(e.to_string()))?;
    Ok((rows, expr))
}

/// Pick the most recent workload that has at least one component
/// satisfying the rule and return up to `TEST_MAX_FINDINGS` deduped real
/// `AlertContext`s from it. Mirrors the production evaluation shape (one
/// report → grouped findings) so a test message is indistinguishable from
/// a real firing. Returns (contexts of the first matching workload, number
/// of OTHER workloads that would also fire on real ingestion). Workload
/// recency is determined by the SQL `ORDER BY r.updated_at DESC` in
/// `list_sbom_component_matches`; we lock onto the first workload key we
/// encounter and treat the rest as "would also match".
async fn build_test_contexts(
    db: &dyn ReportStore,
    rule: &AlertRule,
) -> Result<(Vec<AlertContext>, usize), TestRunError> {
    let (rows, expr) = fetch_component_matches(db, rule).await?;

    let mut first_key: Option<(String, String, String)> = None;
    let mut first_contexts: Vec<AlertContext> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut other_workloads: HashSet<(String, String, String)> = HashSet::new();

    for row in rows {
        if let Some(ref e) = expr
            && !e.matches(&row.version)
        {
            continue;
        }
        let key = (
            row.cluster.clone(),
            row.namespace.clone(),
            row.workload_name.clone(),
        );
        match &first_key {
            None => {
                first_key = Some(key.clone());
                let dedup = format!("{}|{}", row.package_name, row.version);
                if seen.insert(dedup) && first_contexts.len() < TEST_MAX_FINDINGS {
                    let pkg_type =
                        (!row.component_type.is_empty()).then(|| row.component_type.clone());
                    first_contexts.push(AlertContext {
                        cluster: row.cluster,
                        namespace: row.namespace,
                        name: row.workload_name,
                        report_type: "sbomreport".to_string(),
                        package: row.package_name,
                        version: row.version,
                        pkg_type,
                    });
                }
            }
            Some(k) if k == &key => {
                let dedup = format!("{}|{}", row.package_name, row.version);
                if seen.insert(dedup) && first_contexts.len() < TEST_MAX_FINDINGS {
                    let pkg_type =
                        (!row.component_type.is_empty()).then(|| row.component_type.clone());
                    first_contexts.push(AlertContext {
                        cluster: row.cluster,
                        namespace: row.namespace,
                        name: row.workload_name,
                        report_type: "sbomreport".to_string(),
                        package: row.package_name,
                        version: row.version,
                        pkg_type,
                    });
                }
            }
            Some(_) => {
                other_workloads.insert(key);
            }
        }
    }

    if first_contexts.is_empty() {
        return Err(TestRunError::NoMatches);
    }
    Ok((first_contexts, other_workloads.len()))
}

/// Count distinct workloads (other than the firing one) whose SBOM has at
/// least one component matching the rule. Used to render the "also
/// matches N other workload(s)" scope hint on production firings.
///
/// Errors are swallowed (caller defaults to 0) — the scope hint is best-
/// effort context, not a hard requirement of dispatch.
async fn count_other_matching_workloads(
    db: &dyn ReportStore,
    rule: &AlertRule,
    exclude_cluster: &str,
    exclude_namespace: &str,
    exclude_name: &str,
) -> Option<usize> {
    let expr = match rule.matchers.version_expr.as_deref() {
        Some(s) => Some(VersionExpr::parse(s).ok()?),
        None => None,
    };
    let rows = db
        .list_sbom_component_matches(
            &rule.matchers.clusters,
            rule.matchers.namespace.as_deref(),
            rule.matchers.package_name.as_deref(),
        )
        .await
        .ok()?;
    let mut workloads: HashSet<(String, String, String)> = HashSet::new();
    for row in rows {
        if row.cluster == exclude_cluster
            && row.namespace == exclude_namespace
            && row.workload_name == exclude_name
        {
            continue;
        }
        if let Some(ref e) = expr
            && !e.matches(&row.version)
        {
            continue;
        }
        workloads.insert((row.cluster, row.namespace, row.workload_name));
    }
    Some(workloads.len())
}

/// Extract a set of stable component keys (`name|version`) from a stored
/// SBOM report's `data_json`. Used to diff a freshly received report
/// against the previously stored one so the evaluator fires only on
/// net-new components.
pub(crate) fn extract_finding_keys(data_json: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    if let Ok(env) = serde_json::from_str::<SbomReportEnvelope>(data_json) {
        for c in env.report.components.components {
            out.insert(format!("{}|{}", c.name, c.version));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;

    #[test]
    fn extract_sbom_keys_roundtrip() {
        let json = r#"{"report":{"components":{"components":[
            {"name":"openssl","version":"3.0.7"},
            {"name":"glibc","version":"2.36"}
        ]}}}"#;
        let keys = extract_finding_keys(json);
        assert!(keys.contains("openssl|3.0.7"));
        assert!(keys.contains("glibc|2.36"));
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn extract_keys_invalid_json_returns_empty() {
        let keys = extract_finding_keys("not-json");
        assert!(keys.is_empty());
    }

    use crate::alerts::types::{Matchers, Receiver};
    use crate::collector::types::ReportPayload;
    use serde_json::json;

    fn sbom_payload(
        cluster: &str,
        namespace: &str,
        name: &str,
        components: serde_json::Value,
    ) -> ReportPayload {
        ReportPayload {
            cluster: cluster.to_string(),
            namespace: namespace.to_string(),
            name: name.to_string(),
            report_type: "sbomreport".to_string(),
            data_json: json!({
                "metadata": {"labels": {}},
                "report": {
                    "artifact": {"repository": "app", "tag": "v1"},
                    "registry": {"server": "ghcr.io"},
                    "summary": {"componentsCount": 1},
                    "components": {
                        "bomFormat": "CycloneDX",
                        "specVersion": "1.5",
                        "components": components,
                    }
                }
            })
            .to_string(),
            received_at: chrono::Utc::now(),
        }
    }

    fn rule_for_axios(version_expr: Option<&str>) -> AlertRule {
        AlertRule {
            name: "axios-rule".to_string(),
            description: String::new(),
            enabled: true,
            matchers: Matchers {
                package_name: Some("axios".to_string()),
                version_expr: version_expr.map(|s| s.to_string()),
                clusters: vec![],
                namespace: None,
            },
            labels: Default::default(),
            annotations: Default::default(),
            receivers: vec![Receiver {
                name: "noop".to_string(),
                slack: None,
            }],
            cooldown_secs: None,
            created_at: "2026-04-27T00:00:00Z".to_string(),
            created_by: "test".to_string(),
            updated_at: None,
            updated_by: None,
            generation: None,
            status: None,
        }
    }

    async fn seed_db_with_axios_fleet() -> Database {
        let db = Database::new(":memory:")
            .await
            .expect("Failed to create database");
        // Workload A: two axios versions (dedup target — only one row per
        // (name, version) should reach the test contexts).
        db.upsert_report(&sbom_payload(
            "prod",
            "default",
            "node-app-a",
            json!([
                {"type": "library", "name": "axios", "version": "1.6.0"},
                {"type": "library", "name": "axios", "version": "1.6.0"},
                {"type": "library", "name": "axios", "version": "0.27.2"},
            ]),
        ))
        .await
        .unwrap();
        // Workload B: also has axios — should count toward "other workloads".
        db.upsert_report(&sbom_payload(
            "prod",
            "team-b",
            "node-app-b",
            json!([{"type": "library", "name": "axios", "version": "1.6.0"}]),
        ))
        .await
        .unwrap();
        // Workload C: no axios — must not contribute to either count.
        db.upsert_report(&sbom_payload(
            "prod",
            "team-c",
            "no-axios",
            json!([{"type": "library", "name": "lodash", "version": "4.17.21"}]),
        ))
        .await
        .unwrap();
        db
    }

    /// The bug this covers: readiness used to be written only as a side effect
    /// of firing, so a correct rule watching a package nobody runs stayed
    /// blank forever. That is the state most rules live in.
    #[test]
    fn a_correct_rule_is_ready_before_it_has_ever_fired() {
        let rule = rule_for_axios(Some("<2.17.0"));
        assert!(rule.status.is_none(), "never evaluated yet");

        let r = readiness_for(&rule);
        assert!(r.ready);
        assert_eq!(r.reason, "Validated");
        assert!(r.message.is_empty());
    }

    #[test]
    fn a_rule_without_a_version_expression_is_ready() {
        let r = readiness_for(&rule_for_axios(None));
        assert!(r.ready);
        assert_eq!(r.reason, "Validated");
    }

    #[test]
    fn an_unparseable_version_expression_is_not_ready_and_carries_the_error() {
        let r = readiness_for(&rule_for_axios(Some(">=")));
        assert!(!r.ready);
        assert_eq!(r.reason, "InvalidVersionExpr");
        assert!(!r.message.is_empty(), "the parse error is the useful part");
    }

    /// A disabled rule is stored and listed but never evaluated, so it has to
    /// say that rather than leave the column blank.
    #[test]
    fn a_disabled_rule_reports_why_it_is_not_ready() {
        let mut rule = rule_for_axios(None);
        rule.enabled = false;
        let r = readiness_for(&rule);
        assert!(!r.ready);
        assert_eq!(r.reason, "Disabled");
    }

    /// The transition check is what keeps readiness off the hot path: after
    /// the first pass no further writes happen however many reports arrive.
    #[test]
    fn readiness_is_written_once_and_then_left_alone() {
        let mut rule = rule_for_axios(None);
        rule.generation = Some(3);
        let r = readiness_for(&rule);
        assert!(
            !readiness_already_recorded(&rule, r.ready, r.reason, &r.message),
            "nothing stored yet, so the first pass must write"
        );

        let mut status = crate::alerts::crd::AlertRuleStatus::default();
        status.set_condition(
            crate::alerts::crd::CONDITION_READY,
            r.ready,
            r.reason,
            &r.message,
            rule.generation,
        );
        rule.status = Some(status);
        assert!(
            readiness_already_recorded(&rule, r.ready, r.reason, &r.message),
            "the stored condition already says this, so no second write"
        );
    }

    /// An edit bumps the generation, and the acknowledgement has to follow it
    /// even when the verdict is unchanged, or observedGeneration would sit
    /// permanently behind.
    #[test]
    fn an_edit_is_re_acknowledged_even_when_the_verdict_is_unchanged() {
        let mut rule = rule_for_axios(None);
        rule.generation = Some(1);
        let r = readiness_for(&rule);
        let mut status = crate::alerts::crd::AlertRuleStatus::default();
        status.set_condition(
            crate::alerts::crd::CONDITION_READY,
            r.ready,
            r.reason,
            &r.message,
            Some(1),
        );
        rule.status = Some(status);
        assert!(readiness_already_recorded(
            &rule, r.ready, r.reason, &r.message
        ));

        rule.generation = Some(2);
        assert!(
            !readiness_already_recorded(&rule, r.ready, r.reason, &r.message),
            "a newer generation must be acknowledged"
        );
    }

    /// A rule that goes from broken to fixed has to flip, not stay stuck on
    /// the stored failure.
    #[test]
    fn a_fixed_expression_flips_the_stored_verdict() {
        let mut rule = rule_for_axios(Some(">="));
        let bad = readiness_for(&rule);
        let mut status = crate::alerts::crd::AlertRuleStatus::default();
        status.set_condition(
            crate::alerts::crd::CONDITION_READY,
            bad.ready,
            bad.reason,
            &bad.message,
            rule.generation,
        );
        rule.status = Some(status);

        rule.matchers.version_expr = Some("<2.17.0".to_string());
        let good = readiness_for(&rule);
        assert!(good.ready);
        assert!(
            !readiness_already_recorded(&rule, good.ready, good.reason, &good.message),
            "the stored condition still says InvalidVersionExpr, so this must write"
        );
    }

    #[tokio::test]
    async fn build_test_contexts_groups_first_workload_and_counts_others() {
        let db = seed_db_with_axios_fleet().await;
        let rule = rule_for_axios(None);
        let (contexts, other_workloads) = build_test_contexts(&db, &rule).await.unwrap();

        // The first workload (most-recent updated_at — workload C is newest
        // but doesn't match; rows iterate in updated_at DESC) contributes
        // deduped contexts; whichever workload it is, it must have exactly
        // its distinct (name, version) pairs.
        assert!(!contexts.is_empty());
        let workload_keys: std::collections::HashSet<_> = contexts
            .iter()
            .map(|c| (c.cluster.clone(), c.namespace.clone(), c.name.clone()))
            .collect();
        assert_eq!(
            workload_keys.len(),
            1,
            "all contexts must come from a single workload"
        );

        // All contexts should be axios.
        assert!(contexts.iter().all(|c| c.package == "axios"));
        // Other workloads count = matching workloads minus the chosen one.
        // 2 workloads match axios → other = 1.
        assert_eq!(other_workloads, 1);
    }

    #[tokio::test]
    async fn build_test_contexts_dedups_by_name_version() {
        let db = Database::new(":memory:").await.unwrap();
        db.upsert_report(&sbom_payload(
            "prod",
            "default",
            "dup-app",
            json!([
                {"type": "library", "name": "axios", "version": "1.6.0"},
                {"type": "library", "name": "axios", "version": "1.6.0"},
                {"type": "library", "name": "axios", "version": "1.6.0"},
            ]),
        ))
        .await
        .unwrap();
        let rule = rule_for_axios(None);
        let (contexts, _) = build_test_contexts(&db, &rule).await.unwrap();
        assert_eq!(contexts.len(), 1);
    }

    #[tokio::test]
    async fn build_test_contexts_returns_no_matches_when_empty() {
        let db = Database::new(":memory:").await.unwrap();
        db.upsert_report(&sbom_payload(
            "prod",
            "default",
            "no-axios",
            json!([{"type": "library", "name": "lodash", "version": "4.17.21"}]),
        ))
        .await
        .unwrap();
        let rule = rule_for_axios(None);
        let err = build_test_contexts(&db, &rule).await.unwrap_err();
        matches!(err, TestRunError::NoMatches);
    }

    #[tokio::test]
    async fn build_test_contexts_invalid_expr_surfaces() {
        let db = Database::new(":memory:").await.unwrap();
        let rule = rule_for_axios(Some(">="));
        let err = build_test_contexts(&db, &rule).await.unwrap_err();
        matches!(err, TestRunError::InvalidExpr(_));
    }

    #[tokio::test]
    async fn count_other_matching_workloads_excludes_firing_one() {
        let db = seed_db_with_axios_fleet().await;
        let rule = rule_for_axios(None);
        // Pretend node-app-a fired — count the other matching workloads.
        let n = count_other_matching_workloads(&db, &rule, "prod", "default", "node-app-a")
            .await
            .unwrap();
        assert_eq!(n, 1);

        // If we exclude a non-existent workload the firing-one filter is a
        // no-op and we get the full matching count (2).
        let n_all = count_other_matching_workloads(&db, &rule, "prod", "ghost", "ghost")
            .await
            .unwrap();
        assert_eq!(n_all, 2);
    }

    #[tokio::test]
    async fn count_other_matching_workloads_invalid_expr_returns_zero_via_none() {
        let db = seed_db_with_axios_fleet().await;
        let rule = rule_for_axios(Some(">="));
        // Invalid expr → None propagates from `?`. Caller treats None as 0,
        // documented in the function-level comment.
        let result =
            count_other_matching_workloads(&db, &rule, "prod", "default", "node-app-a").await;
        assert!(result.is_none());
    }
}
