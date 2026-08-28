//! Evaluates the maintenance window that gates when a replacement may start.
//! It keeps replacements out of business hours and refuses to start work it
//! cannot finish verifying before the window closes.
//!
//! The window is a cron schedule plus a duration: each firing of the schedule
//! opens the window for that long. Cron alone only names instants, so the
//! duration is what turns those instants into a window.

use std::fmt;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use chrono_tz::Tz;
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use thiserror::Error;

use crate::humanize;

/// A window definition that could not be compiled.
#[derive(Debug, Error)]
pub enum WindowError {
    #[error("timezone {0:?}: not a valid IANA name")]
    Timezone(String),
    #[error("cron schedule is required")]
    EmptySchedule,
    #[error("invalid cron schedule {spec:?}: {reason}")]
    Schedule { spec: String, reason: String },
    #[error("window duration must be positive, got {}", humanize::go_duration(*.0))]
    Duration(Duration),
}

/// The uncompiled window definition.
#[derive(Debug, Clone)]
pub struct WindowConfig {
    /// An IANA name; the schedule is evaluated in it.
    pub timezone: String,
    /// A standard 5-field expression (minute hour dom month dow).
    pub cron_schedule: String,
    /// How long the window stays open after each firing.
    pub duration: Duration,
    /// The minimum window left for a replacement to start.
    pub min_remaining: Duration,
}

/// A recurring window compiled from a cron schedule and a duration.
#[derive(Debug, Clone)]
pub struct Window {
    tz: Tz,
    spec: String,
    schedule: Cron,
    duration: Duration,
    /// How much window must be left for `open` to report true.
    min_remaining: Duration,
}

/// Compiles a cron expression, for validating configuration without building
/// a whole [`Window`].
pub fn parse(spec: &str) -> Result<Cron, WindowError> {
    if spec.trim().is_empty() {
        return Err(WindowError::EmptySchedule);
    }
    CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .build()
        .parse(spec)
        .map_err(|err| WindowError::Schedule {
            spec: spec.to_string(),
            reason: err.to_string(),
        })
}

/// Resolves an IANA timezone name.
pub fn timezone(name: &str) -> Result<Tz, WindowError> {
    name.parse::<Tz>()
        .map_err(|_| WindowError::Timezone(name.to_string()))
}

impl Window {
    /// Compiles a config, failing on an unknown timezone, a malformed cron
    /// expression, or a non-positive duration.
    pub fn new(cfg: &WindowConfig) -> Result<Self, WindowError> {
        let tz = timezone(&cfg.timezone)?;
        let schedule = parse(&cfg.cron_schedule)?;
        if cfg.duration.is_zero() {
            return Err(WindowError::Duration(cfg.duration));
        }
        Ok(Self {
            tz,
            spec: cfg.cron_schedule.clone(),
            schedule,
            duration: cfg.duration,
            min_remaining: cfg.min_remaining,
        })
    }

    /// Reports whether a replacement may start at `t`, and why not otherwise.
    /// Being inside the window is not enough: `min_remaining` must be left.
    #[must_use]
    pub fn open(&self, t: DateTime<Utc>) -> (bool, String) {
        let Some(opened) = self.opened_at(t) else {
            let next = self.next_open(t).map_or_else(
                || "never".to_string(),
                |n| humanize::clock(&n.with_timezone(&self.tz)),
            );
            return (
                false,
                format!(
                    "outside window: schedule {:?} ({}) next opens at {next}",
                    self.spec,
                    self.tz.name()
                ),
            );
        };
        let left = remaining_from(opened, self.duration, t);
        if left < self.min_remaining {
            return (
                false,
                format!(
                    "too little window left: {} remaining, {} required",
                    humanize::go_duration(humanize::round_to_minute(left)),
                    humanize::go_duration(self.min_remaining)
                ),
            );
        }
        (true, String::new())
    }

    /// Reports whether `t` fell inside a window, ignoring `min_remaining`.
    ///
    /// Unlike `open`, this is asked about the past: it selects which historical
    /// samples are comparable to the present moment. `min_remaining` is
    /// deliberately not applied, because it decides whether work may start, not
    /// whether that instant was a window instant.
    #[must_use]
    pub fn contains(&self, t: DateTime<Utc>) -> bool {
        self.opened_at(t).is_some()
    }

    /// Returns how long the window stays open from `t`, or zero when closed.
    #[must_use]
    pub fn remaining(&self, t: DateTime<Utc>) -> Duration {
        self.opened_at(t).map_or(Duration::ZERO, |opened| {
            remaining_from(opened, self.duration, t)
        })
    }

    /// Returns how much longer a replacement may still be started at `t`, which
    /// is `remaining` less `min_remaining`, and zero once no start is permitted.
    ///
    /// `remaining` answers when the window closes; this answers when it stops
    /// accepting new work, which is the deadline anything waiting for
    /// permission to start is really up against.
    #[must_use]
    pub fn start_budget(&self, t: DateTime<Utc>) -> Duration {
        let left = self.remaining(t);
        if left <= self.min_remaining {
            return Duration::ZERO;
        }
        left.saturating_sub(self.min_remaining)
    }

    /// Returns the next time the window opens after `t`, or `None` for a
    /// schedule that never fires again (a day that does not exist, such as
    /// `0 0 30 2 *`).
    #[must_use]
    pub fn next_open(&self, t: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let local = t.with_timezone(&self.tz);
        self.schedule
            .find_next_occurrence(&local, false)
            .ok()
            .map(|n| n.with_timezone(&Utc))
    }

    /// The timezone the schedule is evaluated in.
    #[must_use]
    pub const fn timezone(&self) -> Tz {
        self.tz
    }

    /// Returns the firing whose window covers `t`.
    ///
    /// The search starts one duration back: any firing whose window still
    /// covers `t` must lie in `(t - duration, t]`. Walking forward from there
    /// and keeping the last firing at or before `t` picks the most recent one,
    /// which matters when the schedule fires more than once per duration.
    fn opened_at(&self, t: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let local = t.with_timezone(&self.tz);
        let span = TimeDelta::from_std(self.duration).ok()?;
        let mut cursor = local - span;
        let mut opened = None;
        loop {
            let next = self.schedule.find_next_occurrence(&cursor, false).ok()?;
            if next > local {
                break;
            }
            opened = Some(next.with_timezone(&Utc));
            cursor = next;
        }
        opened
    }
}

fn remaining_from(opened: DateTime<Utc>, duration: Duration, t: DateTime<Utc>) -> Duration {
    let close = opened + TimeDelta::from_std(duration).unwrap_or(TimeDelta::MAX);
    (close - t).to_std().unwrap_or(Duration::ZERO)
}

impl fmt::Display for Window {
    /// Renders the window for startup logs and Slack messages.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} for {} ({}), min remaining {}",
            self.spec,
            humanize::go_duration(self.duration),
            self.tz.name(),
            humanize::go_duration(self.min_remaining)
        )
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn window(spec: &str, tz: &str, duration: Duration, min_remaining: Duration) -> Window {
        Window::new(&WindowConfig {
            timezone: tz.to_string(),
            cron_schedule: spec.to_string(),
            duration,
            min_remaining,
        })
        .unwrap()
    }

    fn seoul(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        chrono_tz::Asia::Seoul
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn rejects_bad_config() {
        assert!(matches!(
            Window::new(&WindowConfig {
                timezone: "Mars/Olympus".into(),
                cron_schedule: "0 2 * * *".into(),
                duration: Duration::from_secs(3600),
                min_remaining: Duration::ZERO,
            }),
            Err(WindowError::Timezone(_))
        ));
        assert!(matches!(parse(""), Err(WindowError::EmptySchedule)));
        assert!(matches!(
            parse("0 2 * *"),
            Err(WindowError::Schedule { .. })
        ));
        assert!(matches!(
            parse("0 0 2 * * *"),
            Err(WindowError::Schedule { .. })
        ));
        assert!(matches!(
            Window::new(&WindowConfig {
                timezone: "UTC".into(),
                cron_schedule: "0 2 * * *".into(),
                duration: Duration::ZERO,
                min_remaining: Duration::ZERO,
            }),
            Err(WindowError::Duration(_))
        ));
    }

    #[test]
    fn opens_for_the_duration_after_each_firing() {
        let w = window(
            "0 2 * * 2,3,4",
            "Asia/Seoul",
            Duration::from_secs(3 * 3600),
            Duration::from_secs(30 * 60),
        );
        // 2026-07-28 is a Tuesday.
        let (open, detail) = w.open(seoul(2026, 7, 28, 2, 0));
        assert!(open, "{detail}");
        assert!(w.open(seoul(2026, 7, 28, 4, 29)).0);
        // Under min remaining.
        let (open, detail) = w.open(seoul(2026, 7, 28, 4, 31));
        assert!(!open);
        assert!(detail.contains("too little window left"), "{detail}");
        assert!(detail.contains("30m0s required"), "{detail}");
        // After close.
        let (open, detail) = w.open(seoul(2026, 7, 28, 5, 0));
        assert!(!open);
        assert!(detail.contains("outside window"), "{detail}");
        assert!(
            detail.contains("next opens at 2026-07-29 02:00 KST"),
            "{detail}"
        );
        // Monday is not in the schedule.
        assert!(!w.open(seoul(2026, 7, 27, 2, 30)).0);
        assert!(!w.contains(seoul(2026, 7, 27, 2, 30)));
        assert!(w.contains(seoul(2026, 7, 28, 4, 59)));
    }

    #[test]
    fn remaining_and_budget() {
        let w = window(
            "0 2 * * *",
            "UTC",
            Duration::from_secs(3 * 3600),
            Duration::from_secs(30 * 60),
        );
        let t = Utc.with_ymd_and_hms(2026, 7, 28, 3, 0, 0).unwrap();
        assert_eq!(w.remaining(t), Duration::from_secs(2 * 3600));
        assert_eq!(w.start_budget(t), Duration::from_secs(90 * 60));
        let late = Utc.with_ymd_and_hms(2026, 7, 28, 4, 45, 0).unwrap();
        assert_eq!(w.start_budget(late), Duration::ZERO);
        let closed = Utc.with_ymd_and_hms(2026, 7, 28, 12, 0, 0).unwrap();
        assert_eq!(w.remaining(closed), Duration::ZERO);
        assert_eq!(w.start_budget(closed), Duration::ZERO);
        assert_eq!(
            w.next_open(closed),
            Some(Utc.with_ymd_and_hms(2026, 7, 29, 2, 0, 0).unwrap())
        );
        let never = window(
            "0 0 30 2 *",
            "UTC",
            Duration::from_secs(3600),
            Duration::ZERO,
        );
        assert_eq!(never.next_open(closed), None);
        assert!(never.open(closed).1.ends_with("next opens at never"));
    }

    #[test]
    fn picks_the_most_recent_firing_when_windows_overlap() {
        let w = window(
            "*/30 * * * *",
            "UTC",
            Duration::from_secs(3600),
            Duration::ZERO,
        );
        let t = Utc.with_ymd_and_hms(2026, 7, 28, 3, 40, 0).unwrap();
        // Fired at 03:30; 50 minutes remain, not 20.
        assert_eq!(w.remaining(t), Duration::from_secs(50 * 60));
    }

    #[test]
    fn renders_for_logs() {
        let w = window(
            "0 2 * * *",
            "Asia/Seoul",
            Duration::from_secs(3 * 3600),
            Duration::from_secs(30 * 60),
        );
        assert_eq!(
            w.to_string(),
            "\"0 2 * * *\" for 3h0m0s (Asia/Seoul), min remaining 30m0s"
        );
        assert_eq!(w.timezone().name(), "Asia/Seoul");
    }
}
