//! Error types for configuration, the link spec, and link operations.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reconcile interval must be at least 1s, got {0:?}")]
    IntervalTooShort(Duration),

    #[error("home must be an absolute path, got {0}")]
    HomeNotAbsolute(PathBuf),
}

#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    #[error("read {path}: {error}")]
    Read { path: PathBuf, error: io::Error },

    #[error("parse {path}: {error}")]
    Parse {
        path: PathBuf,
        error: Box<toml::de::Error>,
    },

    #[error("{field} must be absolute or start with ~/, got {value:?}")]
    NotAbsolute { field: &'static str, value: String },

    #[error("target {0} is declared more than once")]
    DuplicateTarget(PathBuf),
}

#[derive(Debug, thiserror::Error)]
#[error("{op} {path}: {error}")]
pub struct LinkError {
    pub op: &'static str,
    pub path: PathBuf,
    pub error: io::Error,
}

impl LinkError {
    pub fn io(op: &'static str, path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> Self {
        let path = path.into();
        move |error| Self { op, path, error }
    }
}
