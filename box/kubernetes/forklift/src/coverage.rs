//! Measures how much of an organisation's source actually routes through this
//! forklift: it walks a GitLab instance, reads each project's CI and
//! package-manager configuration, and reports which projects point their builds
//! at the forklift host. Results feed a console dashboard and a scheduled alarm
//! listing the projects still to migrate.
//!
//! This file is the scanner itself: what it is configured with, the picture it
//! holds in memory, and how that picture is loaded and persisted. The crawl that
//! fills it in is in `scan.rs`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

pub mod adaptive;
pub mod cron;
pub mod evidence;
pub mod gitlab;
pub mod gitlabcheck;
pub mod host;
pub mod mute;
pub mod pipeline;
pub mod scan;
pub mod types;
pub mod views;

pub use cron::{DEFAULT_CRON, DEFAULT_TIMEZONE, Schedule, parse_schedule};
pub use gitlab::{GitLabClient, GitLabOptions, get_pages, new_gitlab_client};
pub use host::{
    GitLabCheck, HostCheck, host_from_url, is_external_domain, lookup_host, normalize_gitlab_url,
    normalize_host, validate_gitlab_url, validate_match_host,
};
pub use mute::{
    MUTE_SCOPE_CI, MUTE_SCOPE_REGISTRY, MuteScopes, MutedProject, parse_mute_scopes, verdict_for,
};
pub use types::*;
pub use views::{Overview, ProjectDetail};

/// Errors this package returns.
///
/// An empty repository, a branch without a tree and a project the token cannot read all arrive
/// this way.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The bare sentinel, returned where the caller has nothing to add.
    #[error("gitlab: not found")]
    NotFound,
    /// The sentinel with the request that produced it.
    #[error("gitlab: not found: {path} returned {status}")]
    NotFoundAt { path: String, status: u16 },
    /// A cron expression or timezone that does not parse.
    #[error("{0}")]
    Cron(String),
    /// A transport-level HTTP failure.
    #[error("{0}")]
    Http(String),
    /// A malformed JSON response or stored payload.
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    /// The store refused a read or a write.
    #[error("{0}")]
    Store(String),
    /// The scan was cancelled.
    #[error("context canceled")]
    Cancelled,
    #[error("{0}")]
    Msg(String),
}

impl Error {
    /// True for either not-found variant, which is what `errors.Is(err,
    /// ErrNotFound)` tested.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Error::NotFound | Error::NotFoundAt { .. })
    }
}

impl From<crate::meta::Error> for Error {
    fn from(e: crate::meta::Error) -> Self {
        Error::Store(e.to_string())
    }
}

/// The package result type.
///
/// The `Result` name belongs to a completed scan; fallible operations use `Res`.
pub type Res<T> = std::result::Result<T, Error>;

/// Store is the persistence the scanner needs. It is a trait so the scanner can
/// be exercised without a database.
///
#[async_trait]
pub trait Store: Send + Sync {
    async fn read_coverage_settings(&self) -> Res<Option<Settings>>;
    async fn read_coverage_result(&self) -> Res<Option<Result>>;
    async fn write_coverage_result(&self, r: Result) -> Res<()>;
    async fn add_coverage_snapshot(&self, s: Snapshot) -> Res<()>;
    async fn list_coverage_muted(&self) -> Res<Vec<MutedProject>>;
    async fn add_coverage_muted(&self, project_path: &str, by: &str, scopes: MuteScopes)
    -> Res<()>;
    async fn remove_coverage_muted(&self, project_path: &str) -> Res<()>;
}

/// The metadata store speaks the scanner's Store trait directly; the SQL lives
/// in `src/meta/_coverage_needs.rs`.
#[async_trait]
impl Store for crate::meta::Store {
    async fn read_coverage_settings(&self) -> Res<Option<Settings>> {
        Ok(crate::meta::Store::read_coverage_settings(self).await?)
    }
    async fn read_coverage_result(&self) -> Res<Option<Result>> {
        Ok(crate::meta::Store::read_coverage_result(self).await?)
    }
    async fn write_coverage_result(&self, r: Result) -> Res<()> {
        Ok(crate::meta::Store::write_coverage_result(self, r).await?)
    }
    async fn add_coverage_snapshot(&self, s: Snapshot) -> Res<()> {
        Ok(crate::meta::Store::add_coverage_snapshot(self, s).await?)
    }
    async fn list_coverage_muted(&self) -> Res<Vec<MutedProject>> {
        Ok(crate::meta::Store::list_coverage_muted(self).await?)
    }
    async fn add_coverage_muted(
        &self,
        project_path: &str,
        by: &str,
        scopes: MuteScopes,
    ) -> Res<()> {
        Ok(crate::meta::Store::add_coverage_muted(self, project_path, by, scopes).await?)
    }
    async fn remove_coverage_muted(&self, project_path: &str) -> Res<()> {
        Ok(crate::meta::Store::remove_coverage_muted(self, project_path).await?)
    }
}

/// Result is a completed scan as it is persisted, so a restart shows the
/// previous result instead of an empty page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Result {
    // Rows written by earlier releases store an empty list as `null`.
    #[serde(default, deserialize_with = "crate::repoconfig::null_default")]
    pub projects: Vec<Project>,
    #[serde(default, deserialize_with = "crate::repoconfig::null_default")]
    pub excluded_projects: Vec<ExcludedProject>,
    #[serde(with = "types::go_time")]
    pub scanned_at: DateTime<Utc>,
    pub duration_ms: i64,
    /// TriggeredBy names whoever asked for the scan: a username for a manual
    /// run, or "schedule" / "startup" for the two automatic ones.
    pub triggered_by: String,
    pub forklift_host: String,
}

impl Default for Result {
    fn default() -> Self {
        Result {
            projects: Vec::new(),
            excluded_projects: Vec::new(),
            scanned_at: types::zero_time(),
            duration_ms: 0,
            triggered_by: String::new(),
            forklift_host: String::new(),
        }
    }
}

/// Trigger values recorded on a result.
pub const TRIGGER_SCHEDULE: &str = "schedule";
pub const TRIGGER_STARTUP: &str = "startup";
/// The attribution a scan somebody asked for carries when no username is available.
pub const TRIGGER_MANUAL: &str = "manual";

/// ScannerOptions configures a [`Scanner`].
pub struct ScannerOptions {
    pub store: Arc<dyn Store>,
    /// Enabled is the explicit opt-in. Without it the scanner refuses to crawl
    /// even with a URL and token present, so a GitLab credential that happens to
    /// be in the environment never turns into traffic against that instance.
    pub enabled: bool,
    /// GitLabURL is the GitLab base, e.g. "https://gitlab.example.com". It comes
    /// from the environment together with the token, so the pair that reaches
    /// GitLab is one deployment decision. Empty disables scanning.
    pub gitlab_url: String,
    /// GitLabToken is a personal or group access token with read_api. It comes
    /// from the environment and is never persisted or returned.
    pub gitlab_token: String,
    /// ForkliftHost is the host builds must reference to count, derived by the
    /// caller from FORKLIFT_EXTERNAL_URL. It is not a setting: the server knows
    /// what it is called, so nobody retypes it.
    pub forklift_host: String,
}

pub(crate) struct ScannerState {
    pub(crate) settings: Settings,
    /// muted holds, per project path, which checks an operator has waived. A
    /// project with every check muted is out of the measurement entirely.
    pub(crate) muted: HashMap<String, MuteScopes>,
    pub(crate) projects: Vec<Project>,
    pub(crate) excluded_projects: Vec<ExcludedProject>,
    pub(crate) last_scanned_at: Option<DateTime<Utc>>,
    pub(crate) last_duration_ms: i64,
    pub(crate) last_triggered_by: String,
    pub(crate) last_scan_error: String,
    pub(crate) scanning: bool,
    pub(crate) progress: Option<Progress>,
    /// Scan outcomes since start, and the concurrency the last crawl settled on.
    /// Kept here rather than in a metrics module so this one stays free of a
    /// Prometheus dependency; the binary adapts them onto a collector.
    pub(crate) scans_succeeded: i64,
    pub(crate) scans_failed: i64,
    pub(crate) last_concurrency: i64,
    pub(crate) peak_concurrency: i64,
}

/// Scanner walks GitLab and holds the current coverage picture in memory,
/// persisting each completed scan through the store.
pub struct Scanner {
    pub(crate) store: Arc<dyn Store>,
    pub(crate) enabled: bool,
    pub(crate) gitlab_url: String,
    pub(crate) gitlab_token: String,
    pub(crate) forklift_host: String,
    pub(crate) state: RwLock<ScannerState>,
    /// scan_mu serialises whole scans. The `scanning` flag reports the state to
    /// the console; this is what actually prevents two overlapping crawls.
    pub(crate) scan_mu: tokio::sync::Mutex<()>,
}

impl Scanner {
    /// NewScanner builds a scanner. Call [`Scanner::load`] before serving to
    /// bring the stored settings, exclusions and last result into memory.
    pub fn new(o: ScannerOptions) -> Arc<Scanner> {
        Arc::new(Scanner {
            store: o.store,
            enabled: o.enabled,
            gitlab_url: normalize_gitlab_url(&o.gitlab_url),
            gitlab_token: o.gitlab_token,
            forklift_host: host_from_url(&o.forklift_host),
            state: RwLock::new(ScannerState {
                settings: default_settings(),
                muted: HashMap::new(),
                projects: Vec::new(),
                excluded_projects: Vec::new(),
                last_scanned_at: None,
                last_duration_ms: 0,
                last_triggered_by: String::new(),
                last_scan_error: String::new(),
                scanning: false,
                progress: None,
                scans_succeeded: 0,
                scans_failed: 0,
                last_concurrency: 0,
                peak_concurrency: 0,
            }),
            scan_mu: tokio::sync::Mutex::new(()),
        })
    }

    /// DefaultForkliftHost returns the host derived from the external URL, which
    /// is what an empty setting falls back to. The console needs it separately
    /// from the effective value so it can say whether clearing the field would
    /// leave the scan with nothing to match.
    pub fn default_forklift_host(&self) -> String {
        self.forklift_host.clone()
    }

    /// MatchHost is the name that counts as forklift: what an administrator
    /// saved, or the host derived from the external URL when they never had to.
    pub fn match_host(&self) -> String {
        let saved = self.settings().forklift_host;
        if !saved.is_empty() {
            return saved;
        }
        self.forklift_host.clone()
    }

    /// GitLabURL is the GitLab base the scan reads from, supplied by the
    /// deployment.
    pub fn gitlab_url(&self) -> String {
        self.gitlab_url.clone()
    }

    /// Enabled reports whether coverage scanning is both switched on and pointed
    /// at a GitLab instance it can authenticate to. All three are required: the
    /// switch so the crawl is deliberate, the URL and token so there is
    /// something to crawl.
    pub fn enabled(&self) -> bool {
        self.enabled && !self.gitlab_url().is_empty() && !self.gitlab_token.is_empty()
    }

    /// CredentialsPresent reports whether a GitLab URL and token are configured,
    /// regardless of the switch. It separates "turned off" from "not set up" so
    /// the console and the startup log can say which one applies.
    pub fn credentials_present(&self) -> bool {
        !self.gitlab_url().is_empty() && !self.gitlab_token.is_empty()
    }

    /// Load reads settings, the muted projects and the last stored result into
    /// memory. Called at boot and after an administrator saves.
    pub async fn load(&self) -> Res<()> {
        self.refresh_settings().await?;
        self.refresh_muted().await?;
        self.restore_last_result().await
    }

    /// RefreshSettings re-reads the stored settings. An unsaved forklift host
    /// falls back to the one derived from the external URL, so coverage works
    /// out of the box on a deployment that set FORKLIFT_EXTERNAL_URL.
    pub async fn refresh_settings(&self) -> Res<()> {
        let stored = self
            .store
            .read_coverage_settings()
            .await?
            .unwrap_or_else(default_settings);
        self.state.write().settings = stored;
        Ok(())
    }

    /// Settings returns the effective settings.
    pub fn settings(&self) -> Settings {
        self.state.read().settings.clone()
    }

    /// Configured reports whether a host is known to match against, without
    /// which no body can match and every scan would report zero coverage. It is
    /// false only when FORKLIFT_EXTERNAL_URL is unset and nothing was saved.
    pub fn configured(&self) -> bool {
        !self.match_host().is_empty()
    }

    /// Scanning reports whether a scan is in flight.
    pub fn scanning(&self) -> bool {
        self.state.read().scanning
    }

    pub fn next_run_at(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let cfg = self.settings();
        if !cfg.auto_scan_enabled {
            return None;
        }
        let sched = parse_schedule(&cfg.scan_cron, &cfg.timezone).ok()?;
        sched.next(now)
    }

    pub(crate) async fn restore_last_result(&self) -> Res<()> {
        let stored = match self.store.read_coverage_result().await? {
            Some(stored) => stored,
            None => return Ok(()),
        };
        let projects_len = stored.projects.len();
        let scanned_at = stored.scanned_at;
        {
            let mut state = self.state.write();
            state.projects = stored.projects;
            // The stored verdicts were derived under whatever was muted at the
            // time, so they are restamped against what is muted now.
            for i in 0..state.projects.len() {
                let muted = state
                    .muted
                    .get(&state.projects[i].path)
                    .copied()
                    .unwrap_or_default();
                mute::apply_muted(&mut state.projects[i], muted);
            }
            state.excluded_projects = stored.excluded_projects;
            state.last_scanned_at = Some(stored.scanned_at);
            state.last_duration_ms = stored.duration_ms;
            state.last_triggered_by = stored.triggered_by;
        }
        tracing::info!(
            projects = projects_len,
            scanned_at = %crate::meta::time::format_time(scanned_at),
            "coverage: restored last scan"
        );
        Ok(())
    }

    pub(crate) fn new_client(&self) -> GitLabClient {
        self.new_client_with(tokio_util::sync::CancellationToken::new())
    }

    /// The same client bound to a caller-owned cancellation token, which is how
    /// the crawl stops every in-flight request at once.
    pub(crate) fn new_client_with(
        &self,
        cancel: tokio_util::sync::CancellationToken,
    ) -> GitLabClient {
        new_gitlab_client(GitLabOptions {
            api_base_url: self.gitlab_url() + "/api/v4",
            token: self.gitlab_token.clone(),
            cancel,
        })
    }

    pub(crate) async fn persist(&self) -> Res<()> {
        let r = {
            let state = self.state.read();
            let scanned_at = match state.last_scanned_at {
                Some(t) => t,
                None => return Ok(()),
            };
            Result {
                projects: state.projects.clone(),
                excluded_projects: state.excluded_projects.clone(),
                scanned_at,
                duration_ms: state.last_duration_ms,
                triggered_by: state.last_triggered_by.clone(),
                forklift_host: String::new(),
            }
        };
        // MatchHost takes the read lock itself, so it is resolved outside the
        // block above rather than deadlocking on a re-entrant read.
        let r = Result {
            forklift_host: self.match_host(),
            ..r
        };
        self.store.write_coverage_result(r).await
    }

    /// ScanStats returns the scan counters and the last crawl's concurrency.
    pub fn scan_stats(&self) -> ScanStats {
        let state = self.state.read();
        ScanStats {
            succeeded: state.scans_succeeded,
            failed: state.scans_failed,
            last_concurrency: state.last_concurrency,
            peak_concurrency: state.peak_concurrency,
        }
    }
}

/// ScanStats reports how the scanner itself has been behaving, as opposed to
/// what it measured. Exported for the metrics collector, which is why this
/// module stays free of a Prometheus dependency: the binary adapts these onto it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanStats {
    /// Succeeded and Failed count completed scan attempts since process start.
    pub succeeded: i64,
    pub failed: i64,
    /// LastConcurrency and PeakConcurrency are what the adaptive limiter settled
    /// on during the last completed crawl. There is no rate setting, so this is
    /// the only way to see what rate the GitLab instance actually tolerated.
    pub last_concurrency: i64,
    pub peak_concurrency: i64,
}
