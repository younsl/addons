//! What the console reads: the headline counts, the per-group breakdown and one
//! project's detail, all derived from the picture the scanner holds.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::coverage::scan::RawProject;
use crate::coverage::types::{
    EXCLUDE_MUTED, ExcludedProject, GroupCoverage, HISTORY_RETENTION_DAYS, Progress, Project,
    STATE_APPLIED, STATE_ERROR, STATE_NOT_APPLIED, STATE_PARTIAL, Summary, go_time_opt,
};
use crate::coverage::{Error, Res, Scanner};

/// pathEscape percent-encodes a value for a single GitLab path segment. GitLab
/// takes a project path or a ref as one URL-encoded segment, slashes included.
///
/// `percent_encoding`'s sets differ on `~` and `*`, so the escape is spelled out rather than
/// composed from one.
pub(crate) fn path_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Nothing outside the tests calls it, which is why it is test-only here.
#[cfg(test)]
pub(crate) fn url_query_unescape(s: &str) -> Res<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err(Error::Msg(format!("invalid URL escape {:?}", &s[i..])));
                }
                let hex = &s[i + 1..i + 3];
                let v = u8::from_str_radix(hex, 16)
                    .map_err(|_| Error::Msg(format!("invalid URL escape \"%{hex}\"")))?;
                out.push(v);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|e| Error::Msg(e.to_string()))
}

/// splitPath separates "group/subgroup/name" into its group and project name.
pub(crate) fn split_path(full: &str) -> (String, String) {
    match full.rfind('/') {
        Some(i) => (full[..i].to_string(), full[i + 1..].to_string()),
        None => (String::new(), full.to_string()),
    }
}

/// nonNil keeps a slice out of JSON as `[]` rather than `null`, so the console never has to
/// guard for a missing array.
pub(crate) fn non_nil(v: Vec<String>) -> Vec<String> {
    v
}

/// parseGitLabTime reads GitLab's ISO-8601 timestamps; an unparseable value
/// becomes the zero time rather than failing the scan.
pub(crate) fn parse_gitlab_time(s: &str) -> DateTime<Utc> {
    if s.is_empty() {
        return crate::coverage::types::zero_time();
    }
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|_| crate::coverage::types::zero_time())
}

/// inScope is a project that counts toward coverage: it has CI and no exclusion.
pub(crate) fn in_scope(p: &Project) -> bool {
    !p.skipped && p.exclude_reason.is_empty()
}

pub(crate) fn summarize(projects: &[Project], excluded_projects: &[ExcludedProject]) -> Summary {
    let mut out = Summary::default();
    for p in projects {
        if p.skipped {
            out.skipped += 1;
            continue;
        }
        if !p.exclude_reason.is_empty() {
            out.excluded += 1;
            continue;
        }
        out.target += 1;
        match p.applied {
            STATE_APPLIED => out.applied += 1,
            STATE_PARTIAL => out.partial += 1,
            STATE_ERROR => out.errored += 1,
            _ => out.not_applied += 1,
        }
    }
    out.excluded += excluded_projects.len() as i64;
    if out.target > 0 {
        out.percent = (out.applied as f64 / out.target as f64 * 100.0 + 0.5) as i64;
    }
    out
}

/// Overview is everything the coverage dashboard reads in one call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Overview {
    #[serde(flatten)]
    pub summary: Summary,
    pub projects: Vec<Project>,
    pub excluded_projects: Vec<ExcludedProject>,
    #[serde(with = "go_time_opt")]
    pub last_scanned_at: Option<DateTime<Utc>>,
    pub last_scan_duration_ms: i64,
    /// LastScanTriggeredBy is a username for a manual run, or "schedule" /
    /// "startup" for the automatic ones.
    pub last_scan_triggered_by: String,
    /// LastScanError is the message of the last scan that failed, cleared once a
    /// scan succeeds.
    pub last_scan_error: String,
    pub scanning: bool,
    pub scan_progress: Option<Progress>,
    pub gitlab_url: String,
    /// ForkliftHost is the name a project must reference to count: what was
    /// saved, or the host derived from FORKLIFT_EXTERNAL_URL.
    pub forklift_host: String,
    /// Configured is false until both a GitLab instance and a forklift host are
    /// known, which is what sends the console to the settings page.
    pub configured: bool,
    /// Enabled is the effective state: switched on, with a GitLab URL and token.
    pub enabled: bool,
    /// CredentialsPresent separates "turned off" from "never set up", so the
    /// console can tell an operator which of the two to fix.
    pub credentials_present: bool,
    pub scan_cron: String,
    pub timezone: String,
    pub auto_scan_enabled: bool,
    #[serde(with = "go_time_opt")]
    pub next_run_at: Option<DateTime<Utc>>,
    pub alarm_configured: bool,
    /// HistoryRetentionDays is how long a trend reading is kept, so the chart can
    /// state the window it is showing instead of the reader inferring it from
    /// where the line happens to start.
    pub history_retention_days: i64,
}

/// ProjectDetail is one project plus whether the last scan actually produced a
/// verdict for it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectDetail {
    #[serde(flatten)]
    pub project: Project,
    /// Scanned is false when the last scan produced no verdict, either because
    /// the project was excluded at the time or because it was created since. The
    /// console then says so rather than showing a verdict it does not have.
    pub scanned: bool,
}

impl Scanner {
    /// Summary reports the headline counts over the current picture.
    pub fn summary(&self) -> Summary {
        let state = self.state.read();
        summarize(&state.projects, &state.excluded_projects)
    }

    /// Overview assembles the dashboard payload.
    pub fn overview(&self) -> Overview {
        let (cfg, mut out) = {
            let state = self.state.read();
            let out = Overview {
                summary: summarize(&state.projects, &state.excluded_projects),
                projects: state.projects.clone(),
                excluded_projects: state.excluded_projects.clone(),
                last_scanned_at: state.last_scanned_at,
                last_scan_duration_ms: state.last_duration_ms,
                last_scan_triggered_by: state.last_triggered_by.clone(),
                last_scan_error: state.last_scan_error.clone(),
                scanning: state.scanning,
                scan_progress: state.progress.clone(),
                gitlab_url: String::new(),
                forklift_host: String::new(),
                configured: false,
                enabled: false,
                credentials_present: false,
                scan_cron: String::new(),
                timezone: String::new(),
                auto_scan_enabled: false,
                next_run_at: None,
                alarm_configured: false,
                history_retention_days: 0,
            };
            (state.settings.clone(), out)
        };

        out.gitlab_url = self.gitlab_url();
        out.forklift_host = self.match_host();
        out.enabled = self.enabled();
        out.credentials_present = self.credentials_present();
        out.configured = out.enabled && self.configured();
        out.scan_cron = cfg.scan_cron;
        out.timezone = cfg.timezone;
        out.auto_scan_enabled = cfg.auto_scan_enabled;
        out.alarm_configured = cfg.report_enabled && !cfg.receiver.is_empty();
        out.history_retention_days = HISTORY_RETENTION_DAYS;
        out.next_run_at = self.next_run_at(Utc::now());
        out
    }

    /// GroupCoverage aggregates the in-scope projects per group, worst coverage
    /// first: the console reads it as a work queue, not an alphabetical index.
    pub fn group_coverage(&self) -> Vec<GroupCoverage> {
        let projects = self.state.read().projects.clone();

        let mut by_group: HashMap<String, GroupCoverage> = HashMap::new();
        for p in &projects {
            if !in_scope(p) {
                continue;
            }
            let key = if p.group.is_empty() {
                "(root)".to_string()
            } else {
                p.group.clone()
            };
            let entry = by_group
                .entry(key.clone())
                .or_insert_with(|| GroupCoverage {
                    group: key,
                    ..GroupCoverage::default()
                });
            entry.target += 1;
            match p.applied {
                STATE_APPLIED => entry.applied += 1,
                STATE_PARTIAL => entry.partial += 1,
                STATE_NOT_APPLIED => entry.not_applied += 1,
                _ => {}
            }
        }

        let mut out: Vec<GroupCoverage> = by_group
            .into_values()
            .map(|mut entry| {
                if entry.target > 0 {
                    entry.percent =
                        (entry.applied as f64 / entry.target as f64 * 100.0 + 0.5) as i64;
                }
                entry
            })
            .collect();
        out.sort_by(|a, b| {
            a.percent
                .cmp(&b.percent)
                .then_with(|| a.group.cmp(&b.group))
        });
        out
    }

    /// Project returns one scanned project by path.
    pub fn project(&self, path: &str) -> Option<Project> {
        self.state
            .read()
            .projects
            .iter()
            .find(|p| p.path == path)
            .cloned()
    }

    /// ProjectDetail resolves one project for the detail view.
    ///
    /// It falls back to a GitLab lookup so a valid path never dead-ends on a
    /// 404: a project can be missing from the last scan because it was excluded
    /// at the time, or because it was created since. Without that, an opt-out
    /// would be a one-way door, since the detail page is where it gets toggled
    /// back on.
    pub async fn project_detail(&self, path: &str) -> Res<ProjectDetail> {
        if let Some(p) = self.project(path) {
            return Ok(ProjectDetail {
                project: p,
                scanned: true,
            });
        }

        let (excluded, muted) = {
            let state = self.state.read();
            let excluded = state
                .excluded_projects
                .iter()
                .find(|p| p.path == path)
                .cloned();
            let muted = state.muted.get(path).copied().unwrap_or_default();
            (excluded, muted)
        };

        if let Some(excluded) = excluded {
            return Ok(ProjectDetail {
                project: Project {
                    id: excluded.id,
                    path: excluded.path,
                    group: excluded.group,
                    name: excluded.name,
                    web_url: excluded.web_url,
                    default_branch: excluded.default_branch,
                    topics: non_nil(excluded.topics),
                    applied: STATE_NOT_APPLIED,
                    evidence: Vec::new(),
                    muted_scopes: muted.list(),
                    last_activity_at: excluded.last_activity_at,
                    exclude_reason: excluded.reason,
                    ..Project::default()
                },
                scanned: false,
            });
        }

        if !self.enabled() {
            return Err(Error::NotFound);
        }
        let raw: RawProject = self
            .new_client()
            .get_json(&format!("projects/{}", path_escape(path)))
            .await?;
        let (group, name) = split_path(&raw.path_with_namespace);
        let reason = if muted.all() {
            EXCLUDE_MUTED.to_string()
        } else {
            String::new()
        };
        Ok(ProjectDetail {
            project: Project {
                id: raw.id,
                path: raw.path_with_namespace,
                group,
                name,
                web_url: raw.web_url,
                default_branch: raw.default_branch,
                topics: non_nil(raw.topics),
                applied: STATE_NOT_APPLIED,
                evidence: Vec::new(),
                muted_scopes: muted.list(),
                last_activity_at: parse_gitlab_time(&raw.last_activity_at),
                exclude_reason: reason,
                ..Project::default()
            },
            scanned: false,
        })
    }

    /// NotApplied lists the in-scope projects that are not fully wired, worst
    /// first. This is what the alarm reports and what the console's default
    /// filter shows.
    pub fn not_applied(&self) -> Vec<Project> {
        let projects = self.state.read().projects.clone();

        let mut out: Vec<Project> = projects
            .into_iter()
            .filter(|p| {
                in_scope(p) && (p.applied == STATE_NOT_APPLIED || p.applied == STATE_PARTIAL)
            })
            .collect();
        out.sort_by(|a, b| {
            // Not applied at all before partially applied: a project with no
            // wiring is further from done than one with half of it.
            let (li, lj) = (
                a.applied != STATE_NOT_APPLIED,
                b.applied != STATE_NOT_APPLIED,
            );
            li.cmp(&lj).then_with(|| a.path.cmp(&b.path))
        });
        out
    }
}
