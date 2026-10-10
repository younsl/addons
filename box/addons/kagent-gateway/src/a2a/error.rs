//! Failures here have two audiences that want opposite things. The log wants
//! the whole chain, down to the controller's own words, or an incident has
//! nothing to work from. Slack wants one line a responder can read at a
//! glance, and every stage name, retry count, and response body dumped into a
//! thread buries the part that says what to do next.
//!
//! [`Error`] carries both: its `Display` keeps the chain for the log, and
//! [`Error::user_message`] is what the gateway posts.

use std::fmt;

/// Pairs an internal error with the single line describing it to a human.
#[derive(Debug)]
pub struct Error {
    summary: String,
    source: anyhow::Error,
}

impl Error {
    /// Wraps `source` with the summary a human should see instead of it.
    pub fn new(summary: impl Into<String>, source: impl Into<anyhow::Error>) -> Self {
        Self {
            summary: summary.into(),
            source: source.into(),
        }
    }

    /// The one-line summary meant for Slack.
    #[must_use]
    pub fn user_message(&self) -> &str {
        &self.summary
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#}", self.source)
    }
}

impl std::error::Error for Error {}

/// Shorthand used throughout the client.
pub fn fail(summary: impl Into<String>, source: impl Into<anyhow::Error>) -> Error {
    Error::new(summary, source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_both_audiences_apart() {
        let inner = anyhow::anyhow!("decode response").context("submit analysis");
        let err = fail("the controller reply was not valid JSON", inner);
        assert_eq!(
            err.user_message(),
            "the controller reply was not valid JSON"
        );
        assert_eq!(err.to_string(), "submit analysis: decode response");
    }
}
