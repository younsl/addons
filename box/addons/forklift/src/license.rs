//! Resolves the declared license(s) of a package coordinate (system, name,
//! version) against an external metadata source (deps.dev). It performs
//! coordinate matching only: the directly requested version is resolved, not
//! its transitive dependencies and not the artifact bytes.

use async_trait::async_trait;

mod depsdev;

pub use depsdev::{DepsDev, DepsDevResolver};

/// Errors returned by a [`Resolver`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The metadata source could not be reached or the request failed in
    /// transport.
    #[error("{0}")]
    Http(#[from] reqwest::Error),
    /// The metadata source answered with a body that is not the expected JSON.
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    /// The metadata source answered with a non-2xx status other than 404.
    #[error("deps.dev query: status {0}")]
    Status(u16),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, Error>;

/// The resolved license information for one coordinate: the SPDX license
/// expressions reported by the source. `licenses` is empty when the source
/// reports no license (unknown / unlicensed).
///
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LicenseResult {
    pub licenses: Vec<String>,
}

/// Looks up the declared licenses for a coordinate. `source` names the data
/// source (e.g. "deps.dev"), recorded on each resolution so the report
/// attributes its data and stays meaningful as more sources are added.
#[async_trait]
pub trait Resolver: Send + Sync {
    async fn resolve(&self, system: &str, pkg: &str, version: &str) -> Result<LicenseResult>;
    fn source(&self) -> &str;
}
