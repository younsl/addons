//! Keep each rule's `Ready` condition current, by watching the rules.
//!
//! Readiness is a property of the rule, not of any report: whether the
//! evaluator will act on a rule depends only on the rule being enabled and its
//! version expression parsing. It therefore has no business on the ingest path.
//!
//! It was there at first, and the consequence was that readiness inherited the
//! ingest path's cadence. Evaluation is suppressed until hydration completes,
//! and after that a report only arrives when Trivy Operator rescans, which on
//! a quiet fleet is hours. A rule created in the UI would sit with a blank
//! `READY` until the next rescan, which is exactly when an operator is looking
//! at it.
//!
//! Watching the rules instead means a new or edited rule is answered within a
//! watch event, and a rule nothing ever matches still reports for itself.
//!
//! The write is guarded by a transition check, so a steady state costs no API
//! calls. That guard is also what stops the obvious feedback loop: this task's
//! own status patch comes back as another event, but a status write does not
//! bump `metadata.generation`, so the second pass sees its own verdict already
//! recorded and writes nothing.

use futures::StreamExt;
use kube::runtime::watcher::{Config as WatcherConfig, Event, watcher};
use tracing::{debug, error, info, warn};

use super::crd::{self};
use super::expr::VersionExpr;
use super::store::AlertStore;
use super::types::AlertRule;

/// What a rule's `Ready` condition should say.
#[derive(Debug, PartialEq, Eq)]
pub struct Readiness {
    pub ready: bool,
    pub reason: &'static str,
    pub message: String,
}

/// Decide whether the evaluator will act on a rule.
///
/// Deliberately independent of whether the rule has ever matched anything: a
/// rule watching a package nobody runs is correct and ready, it simply has
/// nothing to fire on. Reporting that as not-ready would be wrong, and
/// reporting nothing at all is what this replaced.
pub fn readiness_for(rule: &AlertRule) -> Readiness {
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
/// case there is nothing to write.
///
/// `observedGeneration` is part of the comparison so an edit is
/// re-acknowledged even when the verdict is unchanged, and left out of it
/// nothing would ever move `observedGeneration` forward for a rule that stays
/// valid across edits.
pub fn already_recorded(rule: &AlertRule, readiness: &Readiness) -> bool {
    rule.status
        .as_ref()
        .and_then(|s| s.condition(crd::CONDITION_READY))
        .is_some_and(|c| {
            c.status == if readiness.ready { "True" } else { "False" }
                && c.reason == readiness.reason
                && c.message == readiness.message
                && c.observed_generation == rule.generation
        })
}

/// Watch the rules and keep their `Ready` conditions current.
///
/// Runs on the scraper only. The scraper is the process that evaluates rules,
/// so it is the one entitled to say whether it will act on them, and keeping
/// it single-writer avoids two pods disagreeing about the same condition.
pub async fn run_watch(store: AlertStore, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut stream = watcher(store.api(), WatcherConfig::default()).boxed();

    info!(
        namespace = %store.namespace(),
        resource = %format!("{}.{}", crd::PLURAL, crd::API_GROUP),
        "Alert rule readiness watcher started"
    );

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                info!("Alert rule readiness watcher shutting down");
                break;
            }
            ev = stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(cr))) | Some(Ok(Event::InitApply(cr))) => {
                        reconcile(&store, &cr.to_api()).await;
                    }
                    // A deleted rule has nothing left to report on.
                    Some(Ok(Event::Delete(_))) => {}
                    Some(Ok(Event::Init)) | Some(Ok(Event::InitDone)) => {}
                    Some(Err(e)) => error!(error = %e, "Alert rule readiness watcher error"),
                    None => {
                        warn!("Alert rule readiness watcher stream ended");
                        break;
                    }
                }
            }
        }
    }
}

/// Bring one rule's `Ready` condition in line with what the evaluator would
/// do, writing only when that answer has changed.
///
/// A failure here is logged rather than surfaced: the rule is still evaluated
/// normally, only its self-report is stale, and a watch event will come round
/// again.
async fn reconcile(store: &AlertStore, rule: &AlertRule) {
    let readiness = readiness_for(rule);
    if already_recorded(rule, &readiness) {
        return;
    }

    let result = if readiness.ready {
        store
            .record_ready(rule.name.as_str(), rule.status.as_ref(), rule.generation)
            .await
    } else {
        store
            .record_not_ready(
                rule.name.as_str(),
                rule.status.as_ref(),
                rule.generation,
                readiness.reason,
                &readiness.message,
            )
            .await
    };
    match result {
        Ok(()) => debug!(
            rule = %rule.name,
            ready = readiness.ready,
            reason = readiness.reason,
            "Recorded alert rule readiness"
        ),
        Err(e) => debug!(rule = %rule.name, error = %e, "Failed to record alert rule readiness"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::crd::AlertRuleStatus;
    use crate::alerts::types::{Matchers, Receiver};

    fn rule(version_expr: Option<&str>) -> AlertRule {
        AlertRule {
            name: "axios-rule".to_string(),
            description: String::new(),
            enabled: true,
            matchers: Matchers {
                package_name: Some("axios".to_string()),
                version_expr: version_expr.map(str::to_string),
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
            created_at: "2026-01-01T00:00:00Z".to_string(),
            created_by: "test".to_string(),
            updated_at: None,
            updated_by: None,
            generation: Some(1),
            status: None,
        }
    }

    fn with_recorded(
        mut r: AlertRule,
        readiness: &Readiness,
        generation: Option<i64>,
    ) -> AlertRule {
        let mut status = r.status.take().unwrap_or_default();
        status.set_condition(
            crd::CONDITION_READY,
            readiness.ready,
            readiness.reason,
            &readiness.message,
            generation,
        );
        r.status = Some(status);
        r
    }

    /// The defect this module exists for: readiness used to be a side effect
    /// of firing, so a correct rule watching a package nobody runs reported
    /// nothing at all.
    #[test]
    fn a_correct_rule_is_ready_before_it_has_ever_fired() {
        let r = rule(Some("<2.17.0"));
        assert!(r.status.is_none(), "never evaluated");
        let readiness = readiness_for(&r);
        assert!(readiness.ready);
        assert_eq!(readiness.reason, "Validated");
        assert!(readiness.message.is_empty());
    }

    #[test]
    fn a_rule_without_a_version_expression_is_ready() {
        assert!(readiness_for(&rule(None)).ready);
    }

    #[test]
    fn an_unparseable_version_expression_is_not_ready_and_carries_the_error() {
        let readiness = readiness_for(&rule(Some(">=")));
        assert!(!readiness.ready);
        assert_eq!(readiness.reason, "InvalidVersionExpr");
        assert!(
            !readiness.message.is_empty(),
            "the parse error is the useful half"
        );
    }

    /// A disabled rule is stored and listed but never evaluated, so it has to
    /// say that rather than leave the column blank.
    #[test]
    fn a_disabled_rule_reports_why_it_is_not_ready() {
        let mut r = rule(None);
        r.enabled = false;
        let readiness = readiness_for(&r);
        assert!(!readiness.ready);
        assert_eq!(readiness.reason, "Disabled");
    }

    /// A disabled rule's expression is not even looked at: it cannot fire
    /// either way, and reporting a parse error would bury the real reason.
    #[test]
    fn disabled_takes_precedence_over_an_invalid_expression() {
        let mut r = rule(Some(">="));
        r.enabled = false;
        assert_eq!(readiness_for(&r).reason, "Disabled");
    }

    /// The transition check is the whole cost control, and it is also what
    /// stops this watcher from answering its own writes forever.
    #[test]
    fn a_recorded_verdict_is_not_written_again() {
        let r = rule(None);
        let readiness = readiness_for(&r);
        assert!(
            !already_recorded(&r, &readiness),
            "nothing stored, so the first event must write"
        );

        let r = with_recorded(r, &readiness, Some(1));
        assert!(
            already_recorded(&r, &readiness),
            "the status patch this produced must not trigger another write"
        );
    }

    /// A status write does not bump `metadata.generation`, which is what makes
    /// the loop terminate. Pin that assumption: if a status patch ever did
    /// bump it, this watcher would write forever.
    #[test]
    fn a_status_write_that_bumped_the_generation_would_loop() {
        let r = rule(None);
        let readiness = readiness_for(&r);
        let recorded = with_recorded(rule(None), &readiness, Some(1));
        assert!(already_recorded(&recorded, &readiness));

        let mut bumped = recorded;
        bumped.generation = Some(2);
        assert!(
            !already_recorded(&bumped, &readiness),
            "a generation bump is treated as new work, so status must never cause one"
        );
        let _ = r;
    }

    /// An edit bumps the generation, and the acknowledgement has to follow it
    /// even when the verdict is unchanged, or `observedGeneration` would sit
    /// permanently behind.
    #[test]
    fn an_edit_is_re_acknowledged_even_when_the_verdict_is_unchanged() {
        let readiness = readiness_for(&rule(None));
        let mut r = with_recorded(rule(None), &readiness, Some(1));
        assert!(already_recorded(&r, &readiness));

        r.generation = Some(2);
        assert!(!already_recorded(&r, &readiness));
    }

    /// A rule that goes from broken to fixed has to flip rather than stay
    /// stuck on the stored failure.
    #[test]
    fn a_fixed_expression_flips_the_stored_verdict() {
        let broken = rule(Some(">="));
        let bad = readiness_for(&broken);
        let mut r = with_recorded(broken, &bad, Some(1));

        r.matchers.version_expr = Some("<2.17.0".to_string());
        let good = readiness_for(&r);
        assert!(good.ready);
        assert!(
            !already_recorded(&r, &good),
            "the stored condition still says InvalidVersionExpr"
        );
    }

    /// Enabling a rule flips it back without an edit to the expression.
    #[test]
    fn re_enabling_a_rule_flips_it_back_to_ready() {
        let mut disabled = rule(None);
        disabled.enabled = false;
        let off = readiness_for(&disabled);
        let mut r = with_recorded(disabled, &off, Some(1));

        r.enabled = true;
        let on = readiness_for(&r);
        assert!(on.ready);
        assert!(!already_recorded(&r, &on));
    }

    /// `AlertRuleStatus` is what gets patched, so the condition this module
    /// writes has to land under the type the CRD schema describes.
    #[test]
    fn the_recorded_condition_serializes_under_the_status_schema() {
        let readiness = readiness_for(&rule(None));
        let mut status = AlertRuleStatus::default();
        status.set_condition(
            crd::CONDITION_READY,
            readiness.ready,
            readiness.reason,
            &readiness.message,
            Some(4),
        );
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["conditions"][0]["type"], "Ready");
        assert_eq!(json["conditions"][0]["status"], "True");
        assert_eq!(json["conditions"][0]["reason"], "Validated");
        assert_eq!(json["conditions"][0]["observedGeneration"], 4);
    }
}
