use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("parsing {path}: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },

    #[error("target {name} not found in config (available: {available})")]
    UnknownTarget { name: String, available: String },

    #[error("--{0} must be greater than 0")]
    ZeroValue(&'static str),
}
