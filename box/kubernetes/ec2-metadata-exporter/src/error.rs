//! Error types.

use std::time::Duration;

/// Configuration validation failures surfaced before anything starts.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("SCRAPE_INTERVAL must be at least 1s, got {0:?}")]
    ScrapeIntervalTooShort(Duration),
}
