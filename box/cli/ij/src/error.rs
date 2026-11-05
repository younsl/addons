//! Custom error types for better error handling.

/// Application-specific errors.
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum Error {
    /// AWS SDK error.
    #[error("AWS error: {0}")]
    Aws(String),
    /// PTY operation failed.
    #[error("PTY error: {0}")]
    Pty(String),
    /// Session connection failed.
    #[error("Session error: {0}")]
    Session(String),
    /// Configuration error.
    #[error("Config error: {0}")]
    Config(String),
    /// User cancelled operation.
    #[error("Operation cancelled")]
    Cancelled,
    /// No instances found.
    #[error("No instances found")]
    NoInstances,
    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// Other errors.
    #[error(transparent)]
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl From<serde_yaml::Error> for Error {
    fn from(e: serde_yaml::Error) -> Self {
        Self::Config(e.to_string())
    }
}

/// Result type alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as StdError;

    // --- Display tests ---

    #[test]
    fn display_aws_error() {
        let e = Error::Aws("timeout".into());
        assert_eq!(e.to_string(), "AWS error: timeout");
    }

    #[test]
    fn display_pty_error() {
        let e = Error::Pty("alloc failed".into());
        assert_eq!(e.to_string(), "PTY error: alloc failed");
    }

    #[test]
    fn display_session_error() {
        let e = Error::Session("refused".into());
        assert_eq!(e.to_string(), "Session error: refused");
    }

    #[test]
    fn display_config_error() {
        let e = Error::Config("missing field".into());
        assert_eq!(e.to_string(), "Config error: missing field");
    }

    #[test]
    fn display_cancelled() {
        assert_eq!(Error::Cancelled.to_string(), "Operation cancelled");
    }

    #[test]
    fn display_no_instances() {
        assert_eq!(Error::NoInstances.to_string(), "No instances found");
    }

    #[test]
    fn display_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let e = Error::Io(io_err);
        assert_eq!(e.to_string(), "IO error: file missing");
    }

    #[test]
    fn display_other_error() {
        let e = Error::Other("boom".into());
        assert_eq!(e.to_string(), "boom");
    }

    // --- From conversion tests ---

    #[test]
    fn from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let e: Error = io_err.into();
        assert!(matches!(e, Error::Io(_)));
        assert_eq!(e.to_string(), "IO error: denied");
    }

    #[test]
    fn other_from_anyhow_error() {
        let anyhow_err = anyhow::anyhow!("something went wrong");
        let e = Error::Other(anyhow_err.into());
        assert_eq!(e.to_string(), "something went wrong");
    }

    #[test]
    fn from_serde_yaml_error() {
        let yaml_err = serde_yaml::from_str::<serde_yaml::Value>("{{invalid").unwrap_err();
        let msg = yaml_err.to_string();
        let e: Error = yaml_err.into();
        assert!(matches!(e, Error::Config(_)));
        assert!(
            e.to_string()
                .contains(&msg.split('\n').next().unwrap()[..20])
        );
    }

    // --- source() tests ---

    #[test]
    fn source_returns_some_for_io() {
        let io_err = std::io::Error::other("inner");
        let e = Error::Io(io_err);
        assert!(e.source().is_some());
    }

    #[test]
    fn source_other_delegates() {
        let e = Error::Other(Box::new(std::io::Error::other("no source")));
        assert!(e.source().is_none());
    }

    #[test]
    fn source_returns_none_for_simple_variants() {
        assert!(Error::Aws("x".into()).source().is_none());
        assert!(Error::Pty("x".into()).source().is_none());
        assert!(Error::Session("x".into()).source().is_none());
        assert!(Error::Config("x".into()).source().is_none());
        assert!(Error::Cancelled.source().is_none());
        assert!(Error::NoInstances.source().is_none());
    }
}
