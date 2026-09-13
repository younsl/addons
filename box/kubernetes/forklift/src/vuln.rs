//! Looks up known vulnerabilities for a package coordinate (ecosystem, name,
//! version) against an advisory database (OSV). It performs coordinate
//! matching only: the directly requested version is checked, not its
//! transitive dependencies and not the artifact bytes.

use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;

mod cvss;
mod osv;

pub use osv::{Osv, OsvScanner};

/// Errors returned by a [`Scanner`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The advisory source could not be reached or the request failed in
    /// transport.
    #[error("{0}")]
    Http(#[from] reqwest::Error),
    /// The advisory source answered with a body that is not the expected JSON.
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    /// The advisory source answered with a non-2xx status.
    #[error("osv query: status {0}")]
    Status(u16),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, Error>;

/// An ordered vulnerability severity. Higher is worse.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    #[default]
    None = 0,
    Low = 1,
    Medium = 2,
    High = 3,
    Critical = 4,
}

/// No known advisory (the coordinate is clean).
pub const SEV_NONE: Severity = Severity::None;
/// Low severity.
pub const SEV_LOW: Severity = Severity::Low;
/// Medium severity.
pub const SEV_MEDIUM: Severity = Severity::Medium;
/// High severity.
pub const SEV_HIGH: Severity = Severity::High;
/// Critical severity.
pub const SEV_CRITICAL: Severity = Severity::Critical;

impl Severity {
    /// Returns the lowercase label used in storage and the API.
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Critical => "critical",
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
            Severity::None => "none",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Maps a stored/config label back to a [`Severity`]. Unknown labels
/// (including "") are [`Severity::None`].
pub fn parse_severity(s: &str) -> Severity {
    match s {
        "critical" => Severity::Critical,
        "high" => Severity::High,
        "medium" => Severity::Medium,
        "low" => Severity::Low,
        _ => Severity::None,
    }
}

/// One matched advisory: its id (CVE preferred over GHSA/OSV), the derived
/// severity, and the raw CVSS score string from the source when present (a
/// numeric base score or a CVSS vector; empty when the source gives none).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Advisory {
    pub id: String,
    pub severity: String,
    pub score: String,
}

/// The result of scanning one coordinate: the advisories that apply, the
/// highest severity among them, and a per-severity count. `ids` and
/// `advisories` are empty when the coordinate is clean.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Finding {
    pub ids: Vec<String>,
    pub advisories: Vec<Advisory>,
    pub max: Severity,
    /// The number of advisories at each severity, indexed by the [`Severity`]
    /// discriminant (`counts[Severity::Critical as usize]`, etc.); see
    /// [`Finding::count`].
    pub counts: [i64; 5],
}

impl Finding {
    /// Returns the number of advisories at `sev`.
    pub fn count(&self, sev: Severity) -> i64 {
        self.counts[sev as usize]
    }

    /// Returns the non-zero per-severity advisory counts keyed by the severity label (e.g.
    /// `{"critical": 2, "high": 5}`), for storage and display.
    pub fn severity_counts(&self) -> BTreeMap<String, i64> {
        let mut out = BTreeMap::new();
        for sev in [
            Severity::Low,
            Severity::Medium,
            Severity::High,
            Severity::Critical,
        ] {
            if self.counts[sev as usize] > 0 {
                out.insert(sev.as_str().to_string(), self.counts[sev as usize]);
            }
        }
        out
    }
}

/// Queries an advisory source for vulnerabilities affecting a coordinate. An
/// empty version is a package-level query: the result covers every advisory
/// affecting the package across all versions. `source` names the advisory data
/// source (e.g. "OSV"), recorded on each scan so the report attributes its
/// data and stays meaningful as more sources are added.
#[async_trait]
pub trait Scanner: Send + Sync {
    async fn query(&self, ecosystem: &str, pkg: &str, version: &str) -> Result<Finding>;
    fn source(&self) -> &str;
}
