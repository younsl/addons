//! The report the analyzer produces and the API serves.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Ordered so a derived sort puts the worst first after `reverse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    /// Points one finding of this severity takes off the 100-point score.
    pub const fn penalty(self) -> i64 {
        match self {
            Self::Critical => 25,
            Self::High => 12,
            Self::Medium => 5,
            Self::Low => 2,
            Self::Info => 0,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
            Self::Info => "info",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    SupplyChain,
    Policy,
    Exposure,
    Access,
    Operations,
}

/// Who acts on a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Audience {
    Devops,
    Security,
    Developer,
}

/// One thing a finding points at. Optional fields let the console link to it:
/// a repository, an artifact path in it, an OCI tag, or a user.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Affected {
    pub kind: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<i64>,
}

/// One improvement the analyzer derived. `title`, `summary` and
/// `recommendation` are English for API clients; the console renders its own
/// translation keyed by `id`, filling `params` into it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    /// The rule's short code, for example AC-05.
    #[serde(default)]
    pub code: String,
    pub severity: Severity,
    pub category: Category,
    pub audiences: Vec<Audience>,
    pub title: String,
    pub summary: String,
    /// Why the rule matters, the same text for every run.
    pub rationale: String,
    /// When the rule fires, and at which severity.
    pub criteria: String,
    pub recommendation: String,
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    /// How many things are affected in total; `affected` lists at most
    /// [`AFFECTED_LIMIT`] of them, worst first.
    pub total: i64,
    pub affected: Vec<Affected>,
    /// Per-repository share of `total`, so a viewer limited to some
    /// repositories gets the total they can see. Empty when the finding is not
    /// about repositories.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_repo: BTreeMap<String, i64>,
    /// Shown whole to every signed-in user, because its source is readable by
    /// all of them anyway (Forklift coverage).
    #[serde(default, skip_serializing_if = "is_false")]
    pub everyone: bool,
    /// Taken out of the score, the counts and the alarms by an active
    /// acknowledgement of its check.
    #[serde(default, skip_serializing_if = "is_false")]
    pub acknowledged: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

/// Caps the rows one finding carries.
pub const AFFECTED_LIMIT: usize = 100;

/// Per-severity finding counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeverityCounts {
    pub critical: i64,
    pub high: i64,
    pub medium: i64,
    pub low: i64,
    pub info: i64,
}

impl SeverityCounts {
    pub fn of(findings: &[Finding]) -> Self {
        let mut c = Self::default();
        for f in findings.iter().filter(|f| !f.acknowledged) {
            match f.severity {
                Severity::Critical => c.critical += 1,
                Severity::High => c.high += 1,
                Severity::Medium => c.medium += 1,
                Severity::Low => c.low += 1,
                Severity::Info => c.info += 1,
            }
        }
        c
    }
}

/// What the analyzer looked at, for the header of the view.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    pub repositories: i64,
    pub artifacts: i64,
    pub oci_images: i64,
    pub scanned: i64,
    pub scannable: i64,
    pub users: i64,
    pub tokens: i64,
    /// Scanned and scannable versions per repository name, kept with the
    /// stored report so a viewer limited to some repositories gets the
    /// coverage of theirs. Never sent to the console.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scan_by_repo: BTreeMap<String, ScanCount>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanCount {
    pub scanned: i64,
    pub scannable: i64,
}

/// The trend row kept for every run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InsightSummary {
    pub generated_at: DateTime<Utc>,
    pub score: i64,
    pub grade: String,
    pub counts: SeverityCounts,
}

/// Where a rule stands in a run, after Trusted Advisor's red, yellow and green.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// A critical or high finding.
    Action,
    /// A medium, low or info finding.
    Investigate,
    /// The rule ran and found nothing.
    Ok,
    /// The rule could not run, for example coverage without a completed scan.
    Unavailable,
    /// The check failed, and an administrator or security reviewer
    /// acknowledged it.
    Acknowledged,
}

/// Where an acknowledgement stands against the latest run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AckState {
    /// In force: the check is acknowledged.
    Active,
    /// Past its expiry, so the check counts again.
    Expired,
    /// The check now flags more items than when it was acknowledged, so it
    /// counts again.
    NewItems,
}

/// Who acknowledged a check, why, and until when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Acknowledgement {
    pub note: String,
    pub acknowledged_by: String,
    pub acknowledged_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// What the check flagged when it was acknowledged.
    pub flagged: i64,
    pub state: AckState,
}

impl CheckStatus {
    pub const fn of(severity: Severity) -> Self {
        match severity {
            Severity::Critical | Severity::High => Self::Action,
            Severity::Medium | Severity::Low | Severity::Info => Self::Investigate,
        }
    }
}

/// One rule's result in a run, passing or not, with what it checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub id: String,
    #[serde(default)]
    pub code: String,
    pub category: Category,
    pub audiences: Vec<Audience>,
    pub title: String,
    pub rationale: String,
    pub criteria: String,
    pub recommendation: String,
    pub status: CheckStatus,
    /// How many items the finding flagged, 0 when it passed.
    pub flagged: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledgement: Option<Acknowledgement>,
}

/// One analyzer run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub generated_at: DateTime<Utc>,
    pub duration_ms: i64,
    pub triggered_by: String,
    pub score: i64,
    pub grade: String,
    pub counts: SeverityCounts,
    pub inventory: Inventory,
    pub findings: Vec<Finding>,
    /// Every rule, passing or not, in catalog order.
    #[serde(default)]
    pub checks: Vec<Check>,
}

impl Report {
    /// Builds a report from findings, sorting them worst first and scoring.
    pub fn new(
        generated_at: DateTime<Utc>,
        duration_ms: i64,
        triggered_by: &str,
        inventory: Inventory,
        mut findings: Vec<Finding>,
        checks: Vec<Check>,
    ) -> Self {
        findings.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then_with(|| b.total.cmp(&a.total))
                .then_with(|| a.id.cmp(&b.id))
        });
        let score = score(&findings);
        Self {
            generated_at,
            duration_ms,
            triggered_by: triggered_by.to_string(),
            score,
            grade: grade(score).to_string(),
            counts: SeverityCounts::of(&findings),
            inventory,
            findings,
            checks,
        }
    }

    pub fn summary(&self) -> InsightSummary {
        InsightSummary {
            generated_at: self.generated_at,
            score: self.score,
            grade: self.grade.clone(),
            counts: self.counts,
        }
    }

    /// Recomputes the score and counts after the findings were narrowed for a
    /// viewer.
    pub fn rescore(&mut self) {
        self.score = score(&self.findings);
        self.grade = grade(self.score).to_string();
        self.counts = SeverityCounts::of(&self.findings);
        for check in &mut self.checks {
            if check.status == CheckStatus::Unavailable {
                continue;
            }
            if let Some(f) = self.findings.iter().find(|f| f.id == check.id) {
                check.status = if f.acknowledged {
                    CheckStatus::Acknowledged
                } else {
                    CheckStatus::of(f.severity)
                };
                check.flagged = f.total;
            } else {
                check.status = CheckStatus::Ok;
                check.flagged = 0;
            }
        }
    }
}

impl Report {
    /// Applies the stored acknowledgements as of `now`. An acknowledgement is
    /// in force until it expires or its check flags more items than it did
    /// when acknowledged. Either way the check keeps it for display, with the
    /// reason it lapsed. Acknowledgements of passing checks are left out.
    pub fn apply_acknowledgements(&mut self, acks: &[crate::meta::InsightAck], now: DateTime<Utc>) {
        for check in &mut self.checks {
            let Some(ack) = acks.iter().find(|a| a.check_id == check.id) else {
                continue;
            };
            let Some(finding) = self.findings.iter_mut().find(|f| f.id == check.id) else {
                continue;
            };
            let state = if now >= ack.expires_at {
                AckState::Expired
            } else if finding.total > ack.flagged {
                AckState::NewItems
            } else {
                AckState::Active
            };
            finding.acknowledged = state == AckState::Active;
            check.acknowledgement = Some(Acknowledgement {
                note: ack.note.clone(),
                acknowledged_by: ack.acknowledged_by.clone(),
                acknowledged_at: ack.acknowledged_at,
                expires_at: ack.expires_at,
                flagged: ack.flagged,
                state,
            });
        }
        self.rescore();
    }
}

/// 100 minus the penalty of each finding still open, floored at 0.
pub fn score(findings: &[Finding]) -> i64 {
    let penalty: i64 = findings
        .iter()
        .filter(|f| !f.acknowledged)
        .map(|f| f.severity.penalty())
        .sum();
    (100 - penalty).max(0)
}

pub const fn grade(score: i64) -> &'static str {
    match score {
        90.. => "A",
        75..=89 => "B",
        60..=74 => "C",
        40..=59 => "D",
        _ => "F",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(id: &str, severity: Severity, total: i64) -> Finding {
        Finding {
            id: id.to_string(),
            code: String::new(),
            severity,
            category: Category::Policy,
            audiences: vec![Audience::Devops],
            title: String::new(),
            summary: String::new(),
            rationale: String::new(),
            criteria: String::new(),
            recommendation: String::new(),
            params: BTreeMap::new(),
            total,
            affected: Vec::new(),
            by_repo: BTreeMap::new(),
            everyone: false,
            acknowledged: false,
        }
    }

    #[test]
    fn report_sorts_and_scores() {
        let report = Report::new(
            Utc::now(),
            1,
            "schedule",
            Inventory::default(),
            vec![
                finding("low", Severity::Low, 9),
                finding("crit", Severity::Critical, 1),
                finding("high-b", Severity::High, 1),
                finding("high-a", Severity::High, 5),
            ],
            Vec::new(),
        );
        let ids: Vec<_> = report.findings.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["crit", "high-a", "high-b", "low"]);
        assert_eq!(report.score, 100 - 25 - 12 - 12 - 2);
        assert_eq!(report.grade, "D");
        assert_eq!(report.counts.high, 2);
        assert_eq!(report.summary().score, report.score);
    }

    fn check(id: &str) -> Check {
        Check {
            id: id.to_string(),
            code: String::new(),
            category: Category::Policy,
            audiences: Vec::new(),
            title: String::new(),
            rationale: String::new(),
            criteria: String::new(),
            recommendation: String::new(),
            status: CheckStatus::Ok,
            flagged: 0,
            acknowledgement: None,
        }
    }

    #[test]
    fn acknowledgements_leave_the_score_until_they_lapse() {
        let now = Utc::now();
        let mut report = Report::new(
            now,
            1,
            "schedule",
            Inventory::default(),
            vec![
                finding("active", Severity::High, 2),
                finding("grown", Severity::High, 5),
                finding("expired", Severity::Medium, 1),
            ],
            ["active", "grown", "expired", "passing"]
                .map(check)
                .to_vec(),
        );
        report.rescore();
        let ack = |id: &str, flagged: i64, days: i64| crate::meta::InsightAck {
            check_id: id.to_string(),
            note: "accepted".to_string(),
            flagged,
            acknowledged_by: "sec".to_string(),
            acknowledged_at: now,
            expires_at: now + chrono::TimeDelta::days(days),
        };
        report.apply_acknowledgements(
            &[
                ack("active", 2, 30),
                ack("grown", 3, 30),
                ack("expired", 1, -1),
                ack("passing", 0, 30),
            ],
            now,
        );
        let state = |id: &str| {
            let c = report.checks.iter().find(|c| c.id == id).expect("check");
            (c.status, c.acknowledgement.as_ref().map(|a| a.state))
        };
        assert_eq!(
            state("active"),
            (CheckStatus::Acknowledged, Some(AckState::Active))
        );
        assert_eq!(
            state("grown"),
            (CheckStatus::Action, Some(AckState::NewItems))
        );
        assert_eq!(
            state("expired"),
            (CheckStatus::Investigate, Some(AckState::Expired))
        );
        assert_eq!(
            state("passing"),
            (CheckStatus::Ok, None),
            "passing checks keep no acknowledgement"
        );
        assert_eq!(
            report.score,
            100 - 12 - 5,
            "the acknowledged high no longer counts"
        );
        assert_eq!(report.counts.high, 1);
    }

    #[test]
    fn score_floors_and_grades() {
        let many: Vec<_> = (0..5)
            .map(|i| finding(&i.to_string(), Severity::Critical, 1))
            .collect();
        assert_eq!(score(&many), 0);
        assert_eq!(grade(100), "A");
        assert_eq!(grade(80), "B");
        assert_eq!(grade(60), "C");
        assert_eq!(grade(40), "D");
        assert_eq!(grade(10), "F");
    }
}
