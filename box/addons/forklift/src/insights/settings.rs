//! Console-edited settings: when the analysis runs and when its alarm goes out.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::meta::InsightSettingsRow;

use super::model::{Report, Severity};

pub const DEFAULT_REPORT_CRON: &str = "0 10 * * 1-5";
pub const DEFAULT_TIMEZONE: &str = "UTC";
pub const DEFAULT_MIN_SEVERITY: Severity = Severity::High;

/// Bounds on the analysis interval. Each run reads every artifact row, so the
/// floor stays at minutes.
pub const MIN_INTERVAL_MINUTES: i64 = 5;
pub const MAX_INTERVAL_MINUTES: i64 = 24 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub auto_analyze_enabled: bool,
    pub interval_minutes: i64,
    pub report_enabled: bool,
    /// The notification receiver the scheduled alarm goes to, by name.
    pub receiver: String,
    pub report_cron: String,
    pub timezone: String,
    /// The least severe open finding that makes the scheduled alarm go out.
    pub min_severity: Severity,
    /// Holds the scheduled alarm back while nothing new has turned up since
    /// the last one.
    pub skip_when_unchanged: bool,
    pub updated_by: String,
    pub updated_at: Option<DateTime<Utc>>,
}

impl Settings {
    /// The settings before anybody saves: analysis every `interval` (from
    /// FORKLIFT_INSIGHTS_INTERVAL), no scheduled alarm.
    pub fn defaults(interval: Duration) -> Self {
        let minutes = i64::try_from(interval.as_secs() / 60).unwrap_or(MAX_INTERVAL_MINUTES);
        Self {
            auto_analyze_enabled: !interval.is_zero(),
            interval_minutes: minutes.clamp(MIN_INTERVAL_MINUTES, MAX_INTERVAL_MINUTES),
            report_enabled: false,
            receiver: String::new(),
            report_cron: DEFAULT_REPORT_CRON.to_string(),
            timezone: DEFAULT_TIMEZONE.to_string(),
            min_severity: DEFAULT_MIN_SEVERITY,
            skip_when_unchanged: false,
            updated_by: String::new(),
            updated_at: None,
        }
    }

    pub fn from_row(row: InsightSettingsRow) -> Self {
        Self {
            auto_analyze_enabled: row.auto_analyze_enabled,
            interval_minutes: row
                .interval_minutes
                .clamp(MIN_INTERVAL_MINUTES, MAX_INTERVAL_MINUTES),
            report_enabled: row.report_enabled,
            receiver: row.receiver,
            report_cron: row.report_cron,
            timezone: row.timezone,
            min_severity: parse_severity(&row.min_severity).unwrap_or(DEFAULT_MIN_SEVERITY),
            skip_when_unchanged: row.skip_when_unchanged,
            updated_by: row.updated_by,
            updated_at: Some(row.updated_at),
        }
    }

    pub fn to_row(&self) -> InsightSettingsRow {
        InsightSettingsRow {
            auto_analyze_enabled: self.auto_analyze_enabled,
            interval_minutes: self.interval_minutes,
            report_enabled: self.report_enabled,
            receiver: self.receiver.clone(),
            report_cron: self.report_cron.clone(),
            timezone: self.timezone.clone(),
            min_severity: self.min_severity.label().to_string(),
            skip_when_unchanged: self.skip_when_unchanged,
            updated_by: self.updated_by.clone(),
            updated_at: self.updated_at.unwrap_or_else(Utc::now),
        }
    }

    pub const fn interval(&self) -> chrono::TimeDelta {
        chrono::TimeDelta::minutes(self.interval_minutes)
    }
}

pub fn parse_severity(s: &str) -> Option<Severity> {
    [
        Severity::Critical,
        Severity::High,
        Severity::Medium,
        Severity::Low,
        Severity::Info,
    ]
    .into_iter()
    .find(|v| v.label() == s)
}

/// The open findings at or above `min`, as check id to flagged count. These are
/// what decide whether the scheduled alarm goes out.
pub(crate) fn open_at_or_above(report: &Report, min: Severity) -> HashMap<String, i64> {
    report
        .findings
        .iter()
        .filter(|f| !f.acknowledged && f.severity >= min)
        .map(|f| (f.id.clone(), f.total))
        .collect()
}

/// Reports whether `now` holds something the last alarm did not: a check it
/// did not list, or one that flags more items than it did then.
pub(crate) fn has_news(last: &HashMap<String, i64>, now: &HashMap<String, i64>) -> bool {
    now.iter()
        .any(|(id, flagged)| last.get(id).is_none_or(|before| flagged > before))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_the_configured_interval() {
        let s = Settings::defaults(Duration::from_mins(30));
        assert!(s.auto_analyze_enabled);
        assert_eq!(s.interval_minutes, 30);
        assert!(!s.report_enabled);
        assert_eq!(s.min_severity, Severity::High);

        let off = Settings::defaults(Duration::ZERO);
        assert!(!off.auto_analyze_enabled);
        assert_eq!(off.interval_minutes, MIN_INTERVAL_MINUTES);
    }

    #[test]
    fn rows_round_trip() {
        let mut s = Settings::defaults(Duration::from_hours(1));
        s.report_enabled = true;
        s.receiver = "security-alerts".to_string();
        s.min_severity = Severity::Medium;
        s.skip_when_unchanged = true;
        s.updated_at = Some(Utc::now());
        let back = Settings::from_row(s.to_row());
        assert_eq!(back.receiver, "security-alerts");
        assert_eq!(back.min_severity, Severity::Medium);
        assert!(back.report_enabled && back.skip_when_unchanged);
    }

    #[test]
    fn news_is_a_new_check_or_more_items() {
        let last: HashMap<String, i64> = [("a".to_string(), 2)].into();
        let same: HashMap<String, i64> = [("a".to_string(), 2)].into();
        let fewer: HashMap<String, i64> = [("a".to_string(), 1)].into();
        let more: HashMap<String, i64> = [("a".to_string(), 3)].into();
        let other: HashMap<String, i64> = [("a".to_string(), 2), ("b".to_string(), 1)].into();
        assert!(!has_news(&last, &same));
        assert!(!has_news(&last, &fewer));
        assert!(!has_news(&last, &HashMap::new()));
        assert!(has_news(&last, &more));
        assert!(has_news(&last, &other));
    }

    #[test]
    fn severity_parses_its_labels() {
        assert_eq!(parse_severity("critical"), Some(Severity::Critical));
        assert_eq!(parse_severity("info"), Some(Severity::Info));
        assert_eq!(parse_severity("urgent"), None);
    }
}
