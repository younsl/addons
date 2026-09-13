//! Error types shared across modules.

/// Configuration that failed validation.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("environment variable {key}: {reason}")]
    Env { key: &'static str, reason: String },
    #[error("usage-threshold-percent must be between 0 and 100, got {0}")]
    ThresholdOutOfRange(i64),
    #[error("check-interval-minutes must be at least 1, got {0}")]
    IntervalTooSmall(i64),
    #[error("target-paths must not be empty")]
    EmptyTargetPaths,
}

/// A glob pattern that failed to compile.
#[derive(Debug, thiserror::Error)]
pub enum PatternError {
    #[error("invalid include pattern: {0}")]
    Include(#[source] globset::Error),
    #[error("invalid exclude pattern: {0}")]
    Exclude(#[source] globset::Error),
}
