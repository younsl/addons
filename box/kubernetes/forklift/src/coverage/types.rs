//! The domain types the coverage measurement is expressed in: one project's
//! verdict, the headline counts, the live progress of a running scan and the
//! settings an administrator edits.

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::coverage::cron::{DEFAULT_CRON, DEFAULT_TIMEZONE};

pub(crate) fn zero_time() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(1, 1, 1, 0, 0, 0)
        .single()
        .expect("year 1 is representable")
}

pub(crate) mod go_time {
    use super::*;

    pub fn serialize<S: Serializer>(t: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&crate::meta::time::format_time(*t))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        let raw = String::deserialize(d)?;
        Ok(DateTime::parse_from_rfc3339(&raw)
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or_else(|_| zero_time()))
    }
}

pub(crate) mod go_time_opt {
    use super::*;

    pub fn serialize<S: Serializer>(t: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => s.serialize_str(&crate::meta::time::format_time(*t)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
        let raw = Option::<String>::deserialize(d)?;
        Ok(raw.map(|raw| {
            DateTime::parse_from_rfc3339(&raw)
                .map(|t| t.with_timezone(&Utc))
                .unwrap_or_else(|_| zero_time())
        }))
    }
}

/// AppliedState is one project's verdict.
///
/// Applied means both halves of the wiring are present: the CI pipeline
/// references forklift (or its token) and a package-manager or image-build file
/// pins the registry. Partial means only one of the two does, which builds
/// through forklift in some paths and around it in others. NotApplied means the
/// project has GitLab CI but no forklift wiring anywhere that was looked at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum AppliedState {
    /// "yes"
    Applied,
    /// "partial"
    Partial,
    /// "no"
    #[default]
    NotApplied,
    /// "error"
    Error,
}

pub const STATE_APPLIED: AppliedState = AppliedState::Applied;
pub const STATE_PARTIAL: AppliedState = AppliedState::Partial;
pub const STATE_NOT_APPLIED: AppliedState = AppliedState::NotApplied;
pub const STATE_ERROR: AppliedState = AppliedState::Error;

impl AppliedState {
    pub fn as_str(self) -> &'static str {
        match self {
            AppliedState::Applied => "yes",
            AppliedState::Partial => "partial",
            AppliedState::NotApplied => "no",
            AppliedState::Error => "error",
        }
    }
}

impl std::fmt::Display for AppliedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for AppliedState {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AppliedState {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Ok(match raw.as_str() {
            "yes" => AppliedState::Applied,
            "partial" => AppliedState::Partial,
            "error" => AppliedState::Error,
            _ => AppliedState::NotApplied,
        })
    }
}

/// ExcludeMuted marks a project silenced from the console.
///
/// Why a project is out of scope. The two sources stay distinct so the console
/// can say who took it out: muting is the operator's decision and is reversed in
/// the console, while a topic is the repository opting itself out.
pub const EXCLUDE_MUTED: &str = "muted";
/// ExcludeTopicPrefix is followed by the GitLab topic that matched.
pub const EXCLUDE_TOPIC_PREFIX: &str = "topic:";

/// Project is one scanned GitLab project and its verdict.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: i64,
    /// group/name path with namespace
    pub path: String,
    pub group: String,
    pub name: String,
    pub web_url: String,
    pub default_branch: String,
    pub topics: Vec<String>,
    pub applied: AppliedState,
    /// Branch the wiring was found on; empty when nothing was found.
    pub branch: String,
    /// OnDefault reports whether Branch is the default branch. None when no
    /// wiring was found at all, which is not the same as "found, not on
    /// default".
    pub on_default: Option<bool>,
    /// Format is the repository format derived from the forklift URL path, e.g.
    /// "npm/maven". Empty when it could not be determined.
    pub format: String,
    pub ci_wired: bool,
    pub registry_pinned: bool,
    /// MutedScopes lists the checks an operator has waived on this project, so
    /// the console can say which half of the verdict is not being required.
    pub muted_scopes: Vec<String>,
    /// Evidence lists the files that matched, as the trail behind the verdict.
    pub evidence: Vec<String>,
    /// Note carries a scan remark such as "branches_truncated:10/28" or an API
    /// error.
    pub note: String,
    /// Skipped marks a project with no GitLab CI file at all. Such projects were
    /// never integration candidates, so they are counted apart and kept out of
    /// the coverage denominator.
    pub skipped: bool,
    #[serde(with = "go_time")]
    pub last_activity_at: DateTime<Utc>,
    /// ExcludeReason is why the project is out of scope, or empty when it
    /// counts. It is kept on the scanned record rather than moving the project
    /// to another list, so toggling the opt-out preserves the verdict
    /// underneath.
    pub exclude_reason: String,
}

/// ExcludedProject is a project dropped before it was scanned, so it has a
/// reason but no verdict.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExcludedProject {
    pub id: i64,
    pub path: String,
    pub group: String,
    pub name: String,
    pub web_url: String,
    pub default_branch: String,
    pub topics: Vec<String>,
    pub reason: String,
    #[serde(with = "go_time")]
    pub last_activity_at: DateTime<Utc>,
}

/// Summary is the headline count set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    /// Target is the coverage denominator: in-scope projects that have GitLab
    /// CI.
    pub target: i64,
    pub applied: i64,
    pub partial: i64,
    pub not_applied: i64,
    pub errored: i64,
    /// Skipped counts projects without GitLab CI, which are out of scope.
    pub skipped: i64,
    pub excluded: i64,
    pub percent: i64,
}

/// Progress is the live state of a running scan, so the console fills in as the
/// scan proceeds instead of sitting on the previous result for minutes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    /// Phase is "listing" while the project list is still being paged in, then
    /// "scanning".
    pub phase: String,
    /// Done counts finished projects; zero during the listing phase.
    pub done: i64,
    /// Total is the number to scan; zero until listing completes.
    pub total: i64,
    pub excluded: i64,
    #[serde(with = "go_time")]
    pub started_at: DateTime<Utc>,
    /// Rolling verdict counts.
    pub applied: i64,
    pub partial: i64,
    pub not_applied: i64,
    pub errored: i64,
    pub skipped: i64,
}

impl Default for Progress {
    fn default() -> Self {
        Progress {
            phase: String::new(),
            done: 0,
            total: 0,
            excluded: 0,
            started_at: zero_time(),
            applied: 0,
            partial: 0,
            not_applied: 0,
            errored: 0,
            skipped: 0,
        }
    }
}

/// Phase values for [`Progress`].
pub const PHASE_LISTING: &str = "listing";
pub const PHASE_SCANNING: &str = "scanning";

/// GroupCoverage aggregates a summary per GitLab group, worst first in the
/// console.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupCoverage {
    pub group: String,
    pub target: i64,
    pub applied: i64,
    pub partial: i64,
    pub not_applied: i64,
    pub percent: i64,
}

/// HistoryRetentionDays is how long a coverage reading is kept, and therefore
/// the widest window the trend can show.
///
/// Two weeks, because of what the trend is read for: whether the rollout moved
/// this sprint, and whether it moved backwards. Neither question is asked of a
/// year-old reading, and keeping one only makes the chart harder to read, since
/// every point competes for the same axis. The store prunes to this on each
/// scan, so it is a retention policy and not only a default window.
pub const HISTORY_RETENTION_DAYS: i64 = 14;

/// Snapshot is one historical coverage reading, written after each scan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub target: i64,
    pub applied: i64,
    pub partial: i64,
    pub not_applied: i64,
    pub skipped: i64,
    pub percent: i64,
    #[serde(with = "go_time")]
    pub scanned_at: DateTime<Utc>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Snapshot {
            target: 0,
            applied: 0,
            partial: 0,
            not_applied: 0,
            skipped: 0,
            percent: 0,
            scanned_at: zero_time(),
        }
    }
}

/// LastCommit is the tip commit of the branch a verdict came from. It is fetched
/// on demand for the project detail view rather than during the scan, so it
/// costs one request instead of one per project on every scan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastCommit {
    #[serde(rename = "ref")]
    pub ref_: String,
    pub short_id: String,
    pub title: String,
    pub author_name: String,
    #[serde(with = "go_time")]
    pub committed_at: DateTime<Utc>,
    pub web_url: String,
}

// `ref` is a Rust keyword, so the field is named `ref_` and renamed on the wire.
// Done by hand rather than with `r#ref` so the struct reads the same everywhere.
impl LastCommit {
    pub fn new() -> LastCommit {
        LastCommit {
            ref_: String::new(),
            short_id: String::new(),
            title: String::new(),
            author_name: String::new(),
            committed_at: zero_time(),
            web_url: String::new(),
        }
    }
}

impl Default for LastCommit {
    fn default() -> Self {
        LastCommit::new()
    }
}

/// PipelineFile is one CI definition returned to the pipeline viewer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PipelineFile {
    pub path: String,
    /// Content is the file body, truncated at the per-file cap.
    pub content: String,
    pub truncated: bool,
    /// MatchesForklift marks a file that references the forklift host, so the
    /// viewer can highlight what the verdict was based on.
    pub matches_forklift: bool,
}

/// Pipeline is the set of CI definitions on one ref.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Pipeline {
    pub project_path: String,
    #[serde(rename = "ref")]
    pub ref_: String,
    pub files: Vec<PipelineFile>,
}

/// Settings is the runtime configuration an administrator edits in the console.
///
/// The GitLab base URL and token are deliberately absent: they come from the
/// environment so the credential never reaches the metadata database, its
/// snapshots, or a settings response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// ForkliftHost is the external domain a project's configuration must
    /// reference to count as wired, e.g. "forklift.example.com".
    ///
    /// It defaults to the host of FORKLIFT_EXTERNAL_URL, which is what forklift
    /// already knows itself by, so a normal deployment never has to set it. It
    /// stays editable because the name builds resolve through is not always the
    /// one the console is served on, and because a migration can leave
    /// repositories pointing at a different domain than the current default.
    pub forklift_host: String,
    /// Group narrows the scan to one GitLab group and its subgroups. Empty scans
    /// every project the token is a member of.
    pub group: String,
    /// ExcludePaths drops projects by full path or bare name.
    pub exclude_paths: Vec<String>,
    /// ExcludeTopics drops projects carrying any of these GitLab topics, so a
    /// repository can opt itself out without a console change.
    pub exclude_topics: Vec<String>,
    pub scan_cron: String,
    pub timezone: String,
    pub auto_scan_enabled: bool,
    /// ReportEnabled turns the scheduled report on. Kept apart from Receiver the
    /// same way the scan switch is kept apart from its cron: turning the report
    /// off should not make an operator re-pick where it goes.
    pub report_enabled: bool,
    /// Receiver names the notification receiver the report is sent to.
    pub receiver: String,
    /// SkipWhenFullCoverage drops the scheduled report when every target project
    /// is applied, since a report nobody has to act on is noise. A manual send
    /// ignores it.
    pub skip_when_full_coverage: bool,
    /// How deep the crawl looks. These shape what is scanned, not how fast: the
    /// request rate is discovered at run time rather than configured.
    ///
    /// MaxBranches caps how many non-default branches are searched per project.
    pub max_branches: i64,
    /// SinceDays ignores branches whose last commit is older than this.
    pub since_days: i64,
    /// UseSearch tries GitLab's blob-search endpoint first, which answers for
    /// the default branch in one call but runs on a much lower rate limit.
    pub use_search: bool,
    pub updated_by: String,
    #[serde(with = "go_time")]
    pub updated_at: DateTime<Utc>,
}

impl Default for Settings {
    /// Not the same as [`default_settings`].
    fn default() -> Self {
        Settings {
            forklift_host: String::new(),
            group: String::new(),
            exclude_paths: Vec::new(),
            exclude_topics: Vec::new(),
            scan_cron: String::new(),
            timezone: String::new(),
            auto_scan_enabled: false,
            report_enabled: false,
            receiver: String::new(),
            skip_when_full_coverage: false,
            max_branches: 0,
            since_days: 0,
            use_search: false,
            updated_by: String::new(),
            updated_at: zero_time(),
        }
    }
}

/// DefaultSettings are what a forklift that has never had its coverage settings
/// saved runs with.
pub fn default_settings() -> Settings {
    Settings {
        exclude_topics: vec!["forklift.excluded".to_string()],
        scan_cron: DEFAULT_CRON.to_string(),
        timezone: DEFAULT_TIMEZONE.to_string(),
        auto_scan_enabled: true,
        report_enabled: true,
        max_branches: 10,
        since_days: 180,
        ..Settings::default()
    }
}
