//! Turns a report into the input of the insights alarm.

use crate::notify;

use super::model::{CheckStatus, Report};

/// The alarm input for a report. Acknowledged findings are left out.
pub fn alarm_report(report: &Report) -> notify::InsightsReport {
    notify::InsightsReport {
        score: report.score,
        grade: report.grade.clone(),
        critical: report.counts.critical,
        high: report.counts.high,
        medium: report.counts.medium,
        low: report.counts.low,
        info: report.counts.info,
        checks_passed: report
            .checks
            .iter()
            .filter(|c| c.status == CheckStatus::Ok)
            .count() as i64,
        checks_total: report.checks.len() as i64,
        findings: report
            .findings
            .iter()
            .filter(|f| !f.acknowledged)
            .map(|f| notify::InsightsFindingLine {
                id: f.id.clone(),
                title: format!("[{}] {}", f.code, f.title),
                severity: f.severity.label().to_string(),
                flagged: f.total,
            })
            .collect(),
    }
}
