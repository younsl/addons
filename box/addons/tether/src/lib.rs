//! tether keeps dotfiles symlinked into a home directory and regenerates
//! package manager files from what is installed. It is laid out like a
//! kube-rs controller: a reconcile loop over a desired state, with shared
//! state the web server reads.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("read {path}: {error}")]
    ConfigRead { path: PathBuf, error: io::Error },

    #[error("parse {path}: {error}")]
    ConfigParse {
        path: PathBuf,
        error: Box<toml::de::Error>,
    },

    #[error("{field} must be absolute or start with ~/, got {value:?}")]
    NotAbsolute { field: &'static str, value: String },

    #[error("target {0} is declared more than once")]
    DuplicateTarget(PathBuf),

    #[error("{op} {path}: {error}")]
    Io {
        op: &'static str,
        path: PathBuf,
        error: io::Error,
    },

    #[error("reconcile interval must be at least 1s, got {0:?}")]
    IntervalTooShort(Duration),

    #[error("home must be an absolute path, got {0}")]
    HomeNotAbsolute(PathBuf),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn io(op: &'static str, path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> Self {
        let path = path.into();
        move |error| Self::Io { op, path, error }
    }

    /// Low-cardinality label for the failure metric.
    pub const fn metric_label(&self) -> &'static str {
        match self {
            Self::ConfigRead { .. } => "config_read",
            Self::ConfigParse { .. } => "config_parse",
            Self::NotAbsolute { .. } | Self::DuplicateTarget(_) => "config_invalid",
            Self::Io { .. } => "io",
            Self::IntervalTooShort(_) | Self::HomeNotAbsolute(_) => "settings",
        }
    }
}

pub mod banner;
pub mod config;
pub mod controller;
pub mod files;
pub mod linker;
pub mod metrics;
pub mod packages;
pub mod spec;
pub mod telemetry;
pub mod web;

pub use controller::{Context, State};
pub use metrics::Metrics;

#[cfg(test)]
pub mod fixtures;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_labels_are_stable() {
        let io = Error::io("link", "/x")(io::Error::other("boom"));
        assert_eq!(io.to_string(), "link /x: boom");
        assert_eq!(io.metric_label(), "io");
        assert_eq!(
            Error::DuplicateTarget("/x".into()).metric_label(),
            "config_invalid"
        );
        assert_eq!(
            Error::IntervalTooShort(Duration::ZERO).metric_label(),
            "settings"
        );
    }
}
