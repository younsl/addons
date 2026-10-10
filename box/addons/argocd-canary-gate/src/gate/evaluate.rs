//! The pure rules. Facts in, verdict out, no I/O.
//!
//! Every message is written for the person who sees it in the Argo CD error
//! toast or in `kubectl describe application`, so it names the Rollout, where
//! the update stands, and what to do about it.

use crate::config::{Config, Mode, OnError};
use crate::gate::types::{AppSnapshot, Code, Decision, RolloutSnapshot, RolloutState};

/// The verdict when the Rollout list itself failed.
///
/// Reporting a failed read as "no Rollouts" would silently open the gate, so
/// the failure stays visible and the `onError` policy decides.
#[must_use]
pub fn lookup_failed(app: &AppSnapshot, error: &str, cfg: &Config) -> Decision {
    let allowed = cfg.on_error == OnError::Allow;
    let outcome = if allowed {
        "allowed because onError is set to allow"
    } else {
        "blocked because onError is set to deny"
    };
    let message = format!(
        "Sync of {} is {outcome}. The gate could not list the Argo Rollouts \
         this Application manages, so it cannot tell whether a canary is in \
         progress: {error}",
        app.name
    );
    let warnings = if allowed {
        vec![message.clone()]
    } else {
        Vec::new()
    };
    Decision {
        app: app.name.clone(),
        namespace: app.dest_namespace.clone(),
        allowed,
        code: Code::LookupFailed,
        message,
        warnings,
        rollouts: Vec::new(),
    }
}

/// The verdict for one Application given the Rollouts found under its
/// tracking label.
#[must_use]
pub fn decide(app: &AppSnapshot, rollouts: &[RolloutSnapshot], cfg: &Config) -> Decision {
    let mut verdict = Decision {
        app: app.name.clone(),
        namespace: app.dest_namespace.clone(),
        allowed: true,
        code: Code::Passed,
        message: String::new(),
        warnings: Vec::new(),
        rollouts: Vec::new(),
    };

    if app.skip_requested {
        verdict.code = Code::Exempt;
        verdict.message = format!(
            "Sync of {} is allowed. The Application carries the {} annotation, \
             so no Rollout state was checked.",
            app.name, cfg.exempt.annotation
        );
        return verdict;
    }

    let watched: Vec<&RolloutSnapshot> = rollouts
        .iter()
        .filter(|r| cfg.watches_strategy(&r.strategy))
        .collect();

    if watched.is_empty() {
        verdict.code = Code::NoRollouts;
        verdict.message = format!(
            "Sync of {} is allowed. No Rollout using a watched strategy \
             ({}) carries the label {}={}{}.",
            app.name,
            cfg.rollouts.strategies.join(", "),
            cfg.rollouts.tracking_label,
            app.name,
            where_clause(&app.dest_namespace)
        );
        return verdict;
    }

    let mut states: Vec<RolloutState> = watched.iter().map(|r| RolloutState::of(r)).collect();
    // In-progress ones first, so the log line and the message lead with what
    // blocked the sync.
    states.sort_by_key(|s| (!s.in_progress, s.name.clone()));
    let active: Vec<&RolloutState> = states.iter().filter(|s| s.in_progress).collect();

    if active.is_empty() {
        verdict.message = format!(
            "Sync of {} is allowed. {} Rollout{} checked, none has an update in progress.",
            app.name,
            states.len(),
            plural(states.len())
        );
        verdict.rollouts = states;
        return verdict;
    }

    let detail = active
        .iter()
        .map(|s| describe(s))
        .collect::<Vec<_>>()
        .join(" ");
    let advice = format!(
        "Wait for the rollout to finish, promote it, or abort it, then sync. \
         To bypass this gate once, annotate the Application with {}=true.",
        cfg.exempt.annotation
    );

    verdict.code = Code::CanaryInProgress;
    if cfg.mode == Mode::Enforce {
        verdict.allowed = false;
        verdict.message = format!(
            "Sync of {} is blocked. {detail} Syncing now would hand the Rollout \
             new desired state mid-update and restart the progression. {advice}",
            app.name
        );
    } else {
        verdict.allowed = true;
        verdict.message = format!(
            "Sync of {} proceeded while an update was in progress because mode \
             is set to warn. {detail} {advice}",
            app.name
        );
        verdict.warnings.push(verdict.message.clone());
    }
    verdict.rollouts = states;
    verdict
}

/// One in-flight Rollout as a sentence.
fn describe(state: &RolloutState) -> String {
    let mut condition = Vec::new();
    if !state.step.is_empty() {
        condition.push(state.step.clone());
    }
    if state.aborted {
        condition.push("aborting back to stable".to_string());
    } else if state.paused {
        condition.push("paused".to_string());
    }
    let condition = if condition.is_empty() {
        String::new()
    } else {
        format!(" at {}", condition.join(", "))
    };
    format!(
        "Rollout {} in namespace {} has a {} update in progress{condition}.",
        state.name, state.namespace, state.strategy
    )
}

fn where_clause(namespace: &str) -> String {
    if namespace.is_empty() {
        " in any namespace".to_string()
    } else {
        format!(" in namespace {namespace}")
    }
}

const fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str) -> AppSnapshot {
        AppSnapshot {
            name: name.to_string(),
            dest_namespace: "payments".to_string(),
            skip_requested: false,
        }
    }

    fn rollout(name: &str, strategy: &str, stable: &str, current: &str) -> RolloutSnapshot {
        RolloutSnapshot {
            name: name.to_string(),
            namespace: "payments".to_string(),
            strategy: strategy.to_string(),
            stable_hash: stable.to_string(),
            current_hash: current.to_string(),
            ..RolloutSnapshot::default()
        }
    }

    #[test]
    fn skip_annotation_short_circuits() {
        let cfg = Config::default();
        let mut app = app("prd-api");
        app.skip_requested = true;
        let verdict = decide(&app, &[rollout("api", "canary", "a", "b")], &cfg);
        assert_eq!(verdict.code, Code::Exempt);
        assert!(verdict.allowed);
        assert!(
            verdict
                .message
                .contains("canary-gate.younsl.github.io/skip")
        );
        assert!(verdict.rollouts.is_empty());
    }

    #[test]
    fn no_watched_rollouts_is_allowed() {
        let cfg = Config::default();
        let verdict = decide(&app("prd-api"), &[], &cfg);
        assert_eq!(verdict.code, Code::NoRollouts);
        assert!(verdict.allowed);
        assert!(verdict.message.contains("in namespace payments"));

        // A blueGreen Rollout is invisible under the default strategy list.
        let verdict = decide(
            &app("prd-api"),
            &[rollout("api", "blueGreen", "a", "b")],
            &cfg,
        );
        assert_eq!(verdict.code, Code::NoRollouts);

        let mut cluster_wide = app("prd-api");
        cluster_wide.dest_namespace = String::new();
        let verdict = decide(&cluster_wide, &[], &cfg);
        assert!(verdict.message.contains("in any namespace"));
    }

    #[test]
    fn settled_rollouts_pass() {
        let cfg = Config::default();
        let verdict = decide(
            &app("prd-api"),
            &[
                rollout("api", "canary", "aaa", "aaa"),
                rollout("worker", "canary", "bbb", "bbb"),
            ],
            &cfg,
        );
        assert_eq!(verdict.code, Code::Passed);
        assert!(verdict.allowed);
        assert!(verdict.message.contains("2 Rollouts checked"));
        assert_eq!(verdict.rollouts.len(), 2);
        assert!(verdict.warnings.is_empty());
    }

    #[test]
    fn one_rollout_prints_singular() {
        let cfg = Config::default();
        let verdict = decide(&app("prd-api"), &[rollout("api", "canary", "a", "a")], &cfg);
        assert!(
            verdict.message.contains("1 Rollout checked"),
            "{}",
            verdict.message
        );
    }

    #[test]
    fn in_progress_canary_denies_in_enforce_mode() {
        let cfg = Config::default();
        let mut mid = rollout("api", "canary", "aaa", "bbb");
        mid.current_step = Some(3);
        mid.total_steps = 8;
        mid.paused = true;
        let verdict = decide(
            &app("prd-api"),
            &[rollout("worker", "canary", "ccc", "ccc"), mid],
            &cfg,
        );
        assert_eq!(verdict.code, Code::CanaryInProgress);
        assert!(!verdict.allowed);
        assert!(
            verdict
                .message
                .contains("Rollout api in namespace payments")
        );
        assert!(verdict.message.contains("step 3/8, paused"));
        assert!(verdict.message.contains("annotate the Application"));
        assert_eq!(verdict.rollouts[0].name, "api", "in-progress sorts first");
        assert!(verdict.warnings.is_empty());
    }

    #[test]
    fn warn_mode_allows_with_a_warning() {
        let cfg = Config::parse("mode: warn\n").unwrap();
        let verdict = decide(&app("prd-api"), &[rollout("api", "canary", "a", "b")], &cfg);
        assert_eq!(verdict.code, Code::CanaryInProgress);
        assert!(verdict.allowed);
        assert_eq!(verdict.warnings.len(), 1);
        assert!(verdict.warnings[0].contains("mode is set to warn"));
    }

    #[test]
    fn abort_is_described_as_aborting() {
        let cfg = Config::default();
        let mut aborting = rollout("api", "canary", "aaa", "bbb");
        aborting.aborted = true;
        let verdict = decide(&app("prd-api"), &[aborting], &cfg);
        assert!(!verdict.allowed);
        assert!(verdict.message.contains("aborting back to stable"));
        assert!(!verdict.message.contains("at step"));
    }

    #[test]
    fn hash_disagreement_alone_reads_without_a_condition() {
        let cfg = Config::default();
        let verdict = decide(&app("prd-api"), &[rollout("api", "canary", "a", "b")], &cfg);
        assert!(
            verdict.message.contains("update in progress."),
            "{}",
            verdict.message
        );
    }

    #[test]
    fn blue_green_is_gated_when_configured() {
        let cfg = Config::parse("rollouts:\n  strategies: [blueGreen]\n").unwrap();
        let verdict = decide(
            &app("prd-api"),
            &[
                rollout("api", "blueGreen", "a", "b"),
                rollout("legacy", "canary", "c", "d"),
            ],
            &cfg,
        );
        assert!(!verdict.allowed);
        assert_eq!(verdict.rollouts.len(), 1, "the canary one is not watched");
        assert!(verdict.message.contains("blueGreen update"));
    }

    #[test]
    fn lookup_failure_follows_on_error() {
        let deny = Config::default();
        let verdict = lookup_failed(&app("prd-api"), "boom", &deny);
        assert_eq!(verdict.code, Code::LookupFailed);
        assert!(!verdict.allowed);
        assert!(verdict.message.contains("boom"));
        assert!(verdict.warnings.is_empty());

        let allow = Config::parse("onError: allow\n").unwrap();
        let verdict = lookup_failed(&app("prd-api"), "boom", &allow);
        assert!(verdict.allowed);
        assert_eq!(verdict.warnings.len(), 1);
        assert!(verdict.warnings[0].contains("onError is set to allow"));
    }
}
