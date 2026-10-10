//! Age-policy evaluation shared by the serving and fetch paths.

use chrono::{DateTime, Utc};

use crate::repoconfig::{ACTION_WARN, AgePolicyConfig};

/// The outcome of an age-policy evaluation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum AgeDecision {
    /// Policy disabled, satisfied, or no release time known.
    #[default]
    Allow,
    /// Violates policy but the action is "warn".
    Warn,
    /// Violates policy and the action is "block".
    Block,
}

/// Applies a repository's age policy to an upstream release time. The primary
/// use is a cooldown window (`min_age`): freshly published versions are
/// quarantined to mitigate supply-chain attacks. If `published_at` is `None` the
/// release time is unknown and the artifact is allowed (we never block on
/// missing data, only on a known-too-new release).
pub(crate) fn evaluate_age(
    cfg: &AgePolicyConfig,
    published_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> (AgeDecision, &'static str) {
    let Some(published_at) = published_at else {
        return (AgeDecision::Allow, "");
    };
    if !cfg.enabled {
        return (AgeDecision::Allow, "");
    }
    let age = now - published_at;

    let mut violated = "";
    let min = cfg.min_age.d();
    if !min.is_zero() && age < chrono::TimeDelta::from_std(min).unwrap_or(chrono::TimeDelta::MAX) {
        violated = "release is newer than min_age cooldown";
    }
    let max = cfg.max_age.d();
    if !max.is_zero() && age > chrono::TimeDelta::from_std(max).unwrap_or(chrono::TimeDelta::MAX) {
        violated = "release is older than max_age";
    }
    if violated.is_empty() {
        return (AgeDecision::Allow, "");
    }
    if cfg.action == ACTION_WARN {
        return (AgeDecision::Warn, violated);
    }
    (AgeDecision::Block, violated)
}
