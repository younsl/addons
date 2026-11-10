use thiserror::Error;

/// Webhook delivery failure, classified by whether Slack may have posted it.
#[derive(Debug, Error)]
pub enum SlackError {
    /// Slack did not post the message, so a retry cannot duplicate it.
    #[error("slack did not post the message: {0}")]
    NotPosted(String),

    /// Slack rate-limited the request. Not posted.
    #[error("slack rate limited the request: {0}")]
    RateLimited(String),

    /// The payload itself is invalid. Not posted, and a retry would fail again.
    #[error("slack rejected the payload: {0}")]
    InvalidPayload(String),

    /// The request reached Slack but the outcome is unknown.
    #[error("slack delivery outcome unknown: {0}")]
    Ambiguous(String),
}

impl SlackError {
    pub const fn is_retry_safe(&self) -> bool {
        matches!(self, Self::NotPosted(_) | Self::RateLimited(_))
    }
}
