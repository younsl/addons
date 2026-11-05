//! Structured results of a cleanup cycle.
//!
//! The cleaner returns these instead of only logging, so tests assert on
//! outcomes and a future metrics or summary output can consume the same data.

use std::path::PathBuf;
use std::time::Duration;

/// Result of one cleanup cycle over every target path.
#[derive(Debug, Clone, PartialEq)]
pub struct CycleReport {
    pub paths: Vec<PathReport>,
    pub duration: Duration,
}

/// Result for a single target path.
#[derive(Debug, Clone, PartialEq)]
pub struct PathReport {
    pub path: PathBuf,
    /// Usage observed before deciding whether to clean.
    pub usage: f64,
    pub outcome: Outcome,
}

/// What happened to a target path during a cycle.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Usage was at or below the threshold, nothing was scanned.
    BelowThreshold,
    /// The target path could not be stat'ed.
    Missing,
    /// The path was scanned and matching files were deleted (or listed in
    /// dry-run mode).
    Cleaned(CleanStats),
}

/// Counters for one cleaned path.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CleanStats {
    pub initial_usage: f64,
    pub final_usage: f64,
    /// Files matched by the include/exclude patterns.
    pub candidates: usize,
    pub candidate_bytes: u64,
    pub deleted: usize,
    pub freed_bytes: u64,
    /// Files whose deletion failed.
    pub failed: usize,
    /// The pass stopped early because shutdown was requested.
    pub interrupted: bool,
    pub dry_run: bool,
}

impl CleanStats {
    /// Percentage points of usage recovered by this pass.
    pub fn usage_reduction(&self) -> f64 {
        self.initial_usage - self.final_usage
    }
}

#[cfg(test)]
mod tests {
    use super::CleanStats;

    #[test]
    fn usage_reduction_is_initial_minus_final() {
        let stats = CleanStats {
            initial_usage: 82.5,
            final_usage: 40.0,
            ..CleanStats::default()
        };
        assert!((stats.usage_reduction() - 42.5).abs() < f64::EPSILON);
    }
}
