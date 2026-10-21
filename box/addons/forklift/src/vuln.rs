//! Looks up known vulnerabilities for a package coordinate (ecosystem, name,
//! version) against an advisory database (OSV). It performs coordinate
//! matching only: the directly requested version is checked, not its
//! transitive dependencies and not the artifact bytes.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

mod affected;
mod cvss;
mod osv;

pub use affected::{Affected, Event};
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
    /// The scanner has no package-level advisory lookup.
    #[error("advisory lookup not supported")]
    Unsupported,
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
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
            Self::None => "none",
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    pub const fn count(&self, sev: Severity) -> i64 {
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

/// One advisory affecting a package, with the versions it affects, as served
/// to `npm audit`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PackageAdvisory {
    /// The source id (GHSA for GitHub-reviewed npm advisories).
    pub id: String,
    pub aliases: Vec<String>,
    pub title: String,
    pub severity: Severity,
    /// The CVSS base score, when the source gives a usable one.
    pub score: Option<f64>,
    /// The CVSS vector string, empty when the source gives a numeric score only.
    pub vector: String,
    pub cwe: Vec<String>,
    pub affected: Affected,
}

impl PackageAdvisory {
    /// Returns the GHSA id among the id and its aliases.
    pub fn ghsa(&self) -> Option<&str> {
        self.ids().find(|id| id.starts_with("GHSA-"))
    }

    /// Iterates the id followed by every alias.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.id.as_str()).chain(self.aliases.iter().map(String::as_str))
    }
}

/// Advisories per package name, as returned by [`Scanner::advisories`].
pub type PackageAdvisories = HashMap<String, Arc<Vec<PackageAdvisory>>>;

/// Queries an advisory source for vulnerabilities affecting a coordinate. An
/// empty version is a package-level query: the result covers every advisory
/// affecting the package across all versions. `source` names the advisory data
/// source (e.g. "OSV"), recorded on each scan so the report attributes its
/// data and stays meaningful as more sources are added.
#[async_trait]
pub trait Scanner: Send + Sync {
    async fn query(&self, ecosystem: &str, pkg: &str, version: &str) -> Result<Finding>;
    fn source(&self) -> &str;

    /// Returns every advisory affecting each package in `packages`, across all
    /// versions, with the affected version ranges. Packages without an advisory
    /// may be absent from the result.
    async fn advisories(
        &self,
        _ecosystem: &str,
        _packages: &[String],
    ) -> Result<PackageAdvisories> {
        Err(Error::Unsupported)
    }
}
