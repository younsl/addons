//! Domain types shared by the rules, the engine, and the admission handler.

/// One Argo CD Application reduced to the fields the gate reasons about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppSnapshot {
    /// `metadata.name`, which is also the value Argo CD stamps into its
    /// tracking label on every resource it manages.
    pub name: String,
    /// `spec.destination.namespace`, where the Application's Rollouts live.
    /// Empty widens the Rollout search to the whole cluster.
    pub dest_namespace: String,
    /// True when the app carries the skip annotation.
    pub skip_requested: bool,
}

/// One Argo Rollout reduced to the fields that say whether an update is in
/// flight.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RolloutSnapshot {
    pub name: String,
    pub namespace: String,
    /// `canary`, `blueGreen`, or empty when the strategy block is absent.
    pub strategy: String,
    /// `status.phase` as the Rollout controller last wrote it.
    pub phase: String,
    /// `status.stableRS`, the pod hash of the last fully promoted revision.
    pub stable_hash: String,
    /// `status.currentPodHash`, the pod hash of the revision being rolled out.
    pub current_hash: String,
    /// `status.currentStepIndex`. `None` when the controller has not stepped.
    pub current_step: Option<i64>,
    /// The number of canary steps declared in the spec.
    pub total_steps: usize,
    /// True when `status.pauseConditions` is non-empty, which is how a canary
    /// waits at a pause step or for manual promotion.
    pub paused: bool,
    /// True when `status.abort` is set: the update is being rolled back to
    /// stable and has not arrived yet.
    pub aborted: bool,
}

impl RolloutSnapshot {
    /// Reports whether an update is in flight.
    ///
    /// The load-bearing signal is the pod hash pair: while `stableRS` and
    /// `currentPodHash` disagree, traffic is split between two revisions and a
    /// sync landing new desired state would restart the progression. Pause
    /// conditions and an abort in flight count as in progress even on the rare
    /// snapshot where the hashes momentarily agree. A Rollout with no hashes
    /// yet is a first deploy, which Argo Rollouts promotes without running the
    /// steps, so there is nothing to protect.
    #[must_use]
    pub fn in_progress(&self) -> bool {
        let hashes_disagree = !self.stable_hash.is_empty()
            && !self.current_hash.is_empty()
            && self.stable_hash != self.current_hash;
        hashes_disagree || self.paused || self.aborted
    }

    /// The step position as prose, `step 3/8` or empty when steps do not apply.
    #[must_use]
    pub fn step_position(&self) -> String {
        match self.current_step {
            Some(step) if self.total_steps > 0 => format!("step {step}/{}", self.total_steps),
            _ => String::new(),
        }
    }
}

/// One Rollout's contribution to a verdict, kept for the log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutState {
    pub name: String,
    pub namespace: String,
    pub strategy: String,
    pub phase: String,
    /// `step 3/8` or empty.
    pub step: String,
    pub paused: bool,
    pub aborted: bool,
    pub in_progress: bool,
}

impl RolloutState {
    /// Reduces a snapshot to what the verdict keeps.
    #[must_use]
    pub fn of(rollout: &RolloutSnapshot) -> Self {
        Self {
            name: rollout.name.clone(),
            namespace: rollout.namespace.clone(),
            strategy: rollout.strategy.clone(),
            phase: rollout.phase.clone(),
            step: rollout.step_position(),
            paused: rollout.paused,
            aborted: rollout.aborted,
            in_progress: rollout.in_progress(),
        }
    }
}

/// The machine-readable reason for a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Code {
    /// The Application opted out through the skip annotation.
    Exempt,
    /// No watched Rollout carries this Application's tracking label.
    NoRollouts,
    /// At least one Rollout owned by this Application is mid-update.
    CanaryInProgress,
    /// The Rollout list itself failed.
    LookupFailed,
    /// Every watched Rollout is settled.
    Passed,
}

impl Code {
    /// The label value used in metrics and logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exempt => "Exempt",
            Self::NoRollouts => "NoRollouts",
            Self::CanaryInProgress => "CanaryInProgress",
            Self::LookupFailed => "LookupFailed",
            Self::Passed => "Passed",
        }
    }
}

impl std::fmt::Display for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The gate verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub app: String,
    /// The namespace the Rollouts were searched in. Empty means cluster-wide.
    pub namespace: String,
    pub allowed: bool,
    pub code: Code,
    pub message: String,
    pub warnings: Vec<String>,
    /// The watched Rollouts, in-progress ones first.
    pub rollouts: Vec<RolloutState>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rollout(stable: &str, current: &str) -> RolloutSnapshot {
        RolloutSnapshot {
            name: "payment-api".into(),
            namespace: "payments".into(),
            strategy: "canary".into(),
            stable_hash: stable.into(),
            current_hash: current.into(),
            ..RolloutSnapshot::default()
        }
    }

    #[test]
    fn hash_disagreement_is_in_progress() {
        assert!(rollout("aaa", "bbb").in_progress());
        assert!(!rollout("aaa", "aaa").in_progress());
        assert!(!rollout("", "bbb").in_progress(), "first deploy");
        assert!(!rollout("aaa", "").in_progress(), "no update seen yet");
    }

    #[test]
    fn pause_and_abort_count_even_with_agreeing_hashes() {
        let mut paused = rollout("aaa", "aaa");
        paused.paused = true;
        assert!(paused.in_progress());

        let mut aborted = rollout("aaa", "aaa");
        aborted.aborted = true;
        assert!(aborted.in_progress());
    }

    #[test]
    fn step_position_needs_both_numbers() {
        let mut r = rollout("a", "b");
        assert_eq!(r.step_position(), "");
        r.current_step = Some(3);
        assert_eq!(r.step_position(), "");
        r.total_steps = 8;
        assert_eq!(r.step_position(), "step 3/8");
        r.current_step = None;
        assert_eq!(r.step_position(), "");
    }

    #[test]
    fn state_carries_the_snapshot_over() {
        let mut r = rollout("a", "b");
        r.current_step = Some(2);
        r.total_steps = 4;
        r.phase = "Paused".into();
        r.paused = true;
        let state = RolloutState::of(&r);
        assert_eq!(state.name, "payment-api");
        assert_eq!(state.namespace, "payments");
        assert_eq!(state.step, "step 2/4");
        assert!(state.paused);
        assert!(!state.aborted);
        assert!(state.in_progress);
        assert_eq!(state.phase, "Paused");
        assert_eq!(state.strategy, "canary");
    }

    #[test]
    fn codes_print_their_names() {
        assert_eq!(Code::CanaryInProgress.to_string(), "CanaryInProgress");
        assert_eq!(Code::Exempt.as_str(), "Exempt");
        assert_eq!(Code::NoRollouts.as_str(), "NoRollouts");
        assert_eq!(Code::LookupFailed.as_str(), "LookupFailed");
        assert_eq!(Code::Passed.as_str(), "Passed");
    }
}
