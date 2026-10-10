//! Where a run has got to, shared between the controller that advances it and
//! the reporter that renders it.

use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::humanize;
use crate::slack::Level;

/// One leg of replacing a single tunnel, in the order they happen.
///
/// The values are counters, not identifiers: a percentage over them is only
/// meaningful because every tunnel of a run passes through the same four in
/// the same order. Deliberately not `state::Phase`, which is the narrower
/// question a restart asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RunPhase {
    /// Re-applies the preflight rules to the tunnel that is next.
    Checking = 1,
    /// The in-flight record is written and the AWS call outstanding.
    Replacing = 2,
    /// AWS has accepted, with the tunnel being watched back to health.
    Verifying = 3,
    /// The outcome is persisted and the step closed out.
    Recorded = 4,
}

/// How much of a run one tunnel accounts for.
const PHASES_PER_TUNNEL: usize = RunPhase::Recorded as usize;

#[derive(Debug, Default, Clone)]
struct Inner {
    /// How many tunnels this approval covers. Zero until the run starts, which
    /// is what keeps the footer off the approval card's own replies.
    tunnels: usize,
    /// How many tunnels are completely finished, so `phase` describes tunnel
    /// number `done + 1`.
    done: usize,
    phase: Option<RunPhase>,
    /// When the run began, which after a restart is earlier than this process.
    started_at: Option<DateTime<Utc>>,
    /// Tunnels whose outcome was recorded, across restarts.
    finished: usize,
    /// Those that did not end healthy.
    unhealthy: usize,
}

/// A run is one approval, which covers every tunnel of one connection. The
/// tunnel count is fixed when the run starts, so the percentage only ever
/// moves forward.
#[derive(Debug, Default)]
pub struct Progress {
    inner: Mutex<Inner>,
}

impl Progress {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Fixes the size of the run and puts it at the first phase of its first
    /// tunnel.
    pub fn start(&self, tunnels: usize) {
        let mut p = self.lock();
        p.tunnels = tunnels;
        p.done = 0;
        p.phase = Some(RunPhase::Checking);
        if p.started_at.is_none() {
            p.started_at = Some(Utc::now());
        }
    }

    /// Starts a run that a previous process already got part-way through.
    pub fn resume(
        &self,
        tunnels: usize,
        done: usize,
        phase: RunPhase,
        started_at: Option<DateTime<Utc>>,
    ) {
        let mut p = self.lock();
        p.tunnels = tunnels;
        p.done = done;
        p.phase = Some(phase);
        p.finished = done;
        p.started_at = Some(started_at.unwrap_or_else(Utc::now));
    }

    /// Moves the run to a phase of the tunnel after `done` finished ones.
    pub fn at(&self, done: usize, phase: RunPhase) {
        let mut p = self.lock();
        p.done = done;
        p.phase = Some(phase);
    }

    /// Marks the tunnel after `done` finished ones as finished itself.
    pub fn record(&self, done: usize, healthy: bool) {
        let mut p = self.lock();
        p.done = done;
        p.phase = Some(RunPhase::Recorded);
        p.finished = done + 1;
        if !healthy {
            p.unhealthy += 1;
        }
    }

    /// When the run began, or now if it has not started.
    #[must_use]
    pub fn started_at(&self) -> DateTime<Utc> {
        self.lock().started_at.unwrap_or_else(Utc::now)
    }

    /// The footer, or empty when there is no run to report on.
    #[must_use]
    pub fn line(&self) -> String {
        let p = self.lock().clone();
        if p.tunnels == 0 {
            return String::new();
        }
        let total = p.tunnels * PHASES_PER_TUNNEL;
        let current = (p.done * PHASES_PER_TUNNEL + p.phase.map_or(0, |ph| ph as usize)).min(total);
        // Rounded rather than truncated, so the last phase of a run reads as
        // 100% and the first of many does not read as 0%.
        format!(
            "Progress: {current}/{total} ({}%)",
            (current * 200 + total) / (total * 2)
        )
    }

    /// Closes out a run with what it achieved and how long the whole thing
    /// took, or `None` when there is nothing to close out.
    ///
    /// Withheld for a single-tunnel run. There, the run and its one replacement
    /// differ only by the re-check, and restating a duration already posted one
    /// line above reads as a second measurement of something else.
    #[must_use]
    pub fn report(&self, elapsed: Duration) -> Option<(Level, String)> {
        let p = self.lock().clone();
        if p.tunnels < 2 || p.finished == 0 {
            return None;
        }
        let took = humanize::elapsed(elapsed);
        Some(if p.finished < p.tunnels {
            (
                Level::Warn,
                format!(
                    "*Run ended early.* {} of {} tunnel(s) replaced, and the whole run took {took}. The rest keep their queued maintenance and are proposed again in a later window.",
                    p.finished, p.tunnels
                ),
            )
        } else if p.unhealthy > 0 {
            (
                Level::Warn,
                format!(
                    "*Run finished.* All {} tunnel(s) were replaced in {took}, but {} did not end healthy. Read the steps above before treating this connection as done.",
                    p.tunnels, p.unhealthy
                ),
            )
        } else {
            (
                Level::Success,
                format!(
                    "*Run complete.* All {} tunnel(s) of this connection are done. The whole run took {took}.",
                    p.tunnels
                ),
            )
        })
    }

    /// Puts the footer on its own line under a message. Messages posted when
    /// no run is under way are returned untouched.
    #[must_use]
    pub fn with_progress(&self, msg: &str) -> String {
        let line = self.line();
        if line.is_empty() {
            msg.to_string()
        } else {
            format!("{msg}\n{line}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footer_moves_forward_through_phases() {
        let p = Progress::default();
        assert_eq!(p.line(), "");
        assert_eq!(p.with_progress("hi"), "hi");
        p.start(2);
        assert_eq!(p.line(), "Progress: 1/8 (13%)");
        p.at(0, RunPhase::Replacing);
        assert_eq!(p.line(), "Progress: 2/8 (25%)");
        p.at(0, RunPhase::Verifying);
        assert_eq!(p.with_progress("step"), "step\nProgress: 3/8 (38%)");
        p.record(0, true);
        assert_eq!(p.line(), "Progress: 4/8 (50%)");
        p.at(1, RunPhase::Checking);
        assert_eq!(p.line(), "Progress: 5/8 (63%)");
        p.record(1, false);
        assert_eq!(p.line(), "Progress: 8/8 (100%)");
        // Over-counting is clamped.
        p.at(5, RunPhase::Recorded);
        assert_eq!(p.line(), "Progress: 8/8 (100%)");
    }

    #[test]
    fn report_forms() {
        let p = Progress::default();
        assert!(p.report(Duration::from_secs(10)).is_none(), "no run");
        p.start(1);
        p.record(0, true);
        assert!(p.report(Duration::from_secs(10)).is_none(), "single tunnel");

        let p = Progress::default();
        p.start(2);
        assert!(
            p.report(Duration::from_secs(10)).is_none(),
            "nothing finished"
        );
        p.record(0, true);
        let (level, text) = p.report(Duration::from_secs(600)).unwrap();
        assert_eq!(level, Level::Warn);
        assert!(
            text.starts_with(
                "*Run ended early.* 1 of 2 tunnel(s) replaced, and the whole run took 10m 00s."
            ),
            "{text}"
        );
        p.record(1, false);
        let (level, text) = p.report(Duration::from_mins(20)).unwrap();
        assert_eq!(level, Level::Warn);
        assert!(text.contains("but 1 did not end healthy"), "{text}");

        let p = Progress::default();
        p.start(2);
        p.record(0, true);
        p.record(1, true);
        let (level, text) = p.report(Duration::from_mins(20)).unwrap();
        assert_eq!(level, Level::Success);
        assert!(
            text.starts_with("*Run complete.* All 2 tunnel(s)"),
            "{text}"
        );
    }

    #[test]
    fn resume_keeps_earlier_tunnels_and_start_time() {
        let p = Progress::default();
        let started = DateTime::from_timestamp(1_785_000_000, 0).unwrap();
        p.resume(3, 1, RunPhase::Verifying, Some(started));
        assert_eq!(p.line(), "Progress: 7/12 (58%)");
        assert_eq!(p.started_at(), started);
        p.record(1, true);
        assert_eq!(p.report(Duration::from_secs(1)).unwrap().0, Level::Warn);
        let p = Progress::default();
        p.resume(2, 0, RunPhase::Checking, None);
        assert!(p.started_at() <= Utc::now());
        // start keeps an earlier start.
        let p = Progress::default();
        p.resume(2, 0, RunPhase::Checking, Some(started));
        p.start(2);
        assert_eq!(p.started_at(), started);
    }
}
