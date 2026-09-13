//! Renders measured times the way an operator reads them.

use std::fmt::Write as _;
use std::time::Duration;

use chrono::{DateTime, TimeZone};

/// Renders how long something took, for a Slack thread or a log line.
///
/// Seconds are kept at every scale, unlike the minute-rounded horizons on the
/// approval card: a replacement that took 3m 07s and one that took 3m 52s are
/// different facts, and the difference is exactly what someone reading a
/// replacement report is after.
#[must_use]
pub fn elapsed(d: Duration) -> String {
    if d.is_zero() {
        return "0s".to_string();
    }
    if d < Duration::from_secs(1) {
        // Sub-second only happens on a rejected call, where "0s" would read as
        // if nothing had been measured at all.
        let ms = (d.as_secs_f64() * 1000.0).round();
        if ms >= 1000.0 {
            return "1s".to_string();
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        return format!("{}ms", ms as u64);
    }
    let total = round_secs(d);
    let h = total / 3600;
    let m = total % 3600 / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}h {m:02}m {s:02}s")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Renders a duration in Go's `time.Duration` syntax (`1h30m0s`, `10s`,
/// `500ms`), which is what the config file uses and what every log line and
/// Slack card printed before the port. Keeping the syntax keeps dashboards and
/// runbooks valid.
#[must_use]
pub fn go_duration(d: Duration) -> String {
    if d.is_zero() {
        return "0s".to_string();
    }
    if d < Duration::from_secs(1) {
        let nanos = d.as_nanos();
        if nanos.is_multiple_of(1_000_000) {
            return format!("{}ms", nanos / 1_000_000);
        }
        if nanos.is_multiple_of(1_000) {
            return format!("{}µs", nanos / 1_000);
        }
        return format!("{nanos}ns");
    }
    let total = d.as_secs();
    let hours = total / 3600;
    let mins = total % 3600 / 60;
    let secs = total % 60;
    let frac = d.subsec_nanos();
    let mut out = String::new();
    if hours > 0 {
        let _ = write!(out, "{hours}h");
    }
    if hours > 0 || mins > 0 {
        let _ = write!(out, "{mins}m");
    }
    if frac == 0 {
        let _ = write!(out, "{secs}s");
    } else {
        let mut f = format!("{frac:09}");
        while f.ends_with('0') {
            f.pop();
        }
        let _ = write!(out, "{secs}.{f}s");
    }
    out
}

/// Rounds to the nearest whole minute, the way the approval card states
/// horizons.
#[must_use]
pub const fn round_to_minute(d: Duration) -> Duration {
    Duration::from_secs((d.as_secs() + 30) / 60 * 60)
}

/// Rounds to the nearest whole second.
#[must_use]
pub const fn round_to_second(d: Duration) -> Duration {
    Duration::from_secs(round_secs(d))
}

const fn round_secs(d: Duration) -> u64 {
    let secs = d.as_secs();
    if d.subsec_nanos() >= 500_000_000 {
        secs + 1
    } else {
        secs
    }
}

/// Renders an instant as `2006-01-02 15:04 MST`, the form every card and log
/// line uses so an approver reads one clock everywhere.
#[must_use]
pub fn clock<Tz: TimeZone>(t: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    t.format("%Y-%m-%d %H:%M %Z").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_renders_every_scale() {
        for (input, want) in [
            (Duration::ZERO, "0s"),
            (Duration::from_millis(812), "812ms"),
            (Duration::from_secs(1), "1s"),
            (Duration::from_secs(59), "59s"),
            (Duration::from_secs(60), "1m 00s"),
            (Duration::from_secs(3 * 60 + 7), "3m 07s"),
            (Duration::from_secs(3600), "1h 00m 00s"),
            (Duration::from_secs(3600 + 5 * 60), "1h 05m 00s"),
            (Duration::from_secs(2 * 3600 + 34 * 60 + 5), "2h 34m 05s"),
        ] {
            assert_eq!(elapsed(input), want, "{input:?}");
        }
    }

    #[test]
    fn elapsed_rounds_sub_second_up() {
        assert_eq!(elapsed(Duration::from_micros(999_800)), "1s");
        assert_eq!(elapsed(Duration::from_millis(1499)), "1s");
        assert_eq!(elapsed(Duration::from_millis(1500)), "2s");
    }

    #[test]
    fn go_duration_matches_go_syntax() {
        for (input, want) in [
            (Duration::ZERO, "0s"),
            (Duration::from_secs(10), "10s"),
            (Duration::from_secs(5 * 60), "5m0s"),
            (Duration::from_secs(90 * 60), "1h30m0s"),
            (Duration::from_secs(168 * 3600), "168h0m0s"),
            (Duration::from_millis(500), "500ms"),
            (
                Duration::from_micros(1500),
                "1.5ms".replace("1.5ms", "1500µs").as_str(),
            ),
            (Duration::from_nanos(7), "7ns"),
            (Duration::from_millis(1500), "1.5s"),
        ] {
            assert_eq!(go_duration(input), want, "{input:?}");
        }
    }

    #[test]
    fn rounding_helpers() {
        assert_eq!(
            round_to_minute(Duration::from_secs(89)),
            Duration::from_secs(60)
        );
        assert_eq!(
            round_to_minute(Duration::from_secs(90)),
            Duration::from_secs(120)
        );
        assert_eq!(
            round_to_second(Duration::from_millis(2499)),
            Duration::from_secs(2)
        );
        assert_eq!(
            round_to_second(Duration::from_millis(2500)),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn clock_renders_zone_abbreviation() {
        let t = chrono::Utc.with_ymd_and_hms(2026, 7, 27, 2, 14, 0).unwrap();
        assert_eq!(clock(&t), "2026-07-27 02:14 UTC");
        let seoul = t.with_timezone(&chrono_tz::Asia::Seoul);
        assert_eq!(clock(&seoul), "2026-07-27 11:14 KST");
    }
}
