//! A five-field cron parser evaluated in a named IANA location.

use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

use crate::coverage::{Error, Res};

/// Schedule is a parsed five-field cron expression (minute hour day-of-month
/// month day-of-week) evaluated in a named IANA location.
///
/// Forklift parses cron itself rather than taking a dependency, because the two
/// things the scheduler needs are small: "does this expression fire in the
/// minute that just started" for the tick, and "when does it fire next" for the
/// settings preview. Each field is a bitset, so a match is five bit tests.
#[derive(Debug, Clone)]
pub struct Schedule {
    expr: String,
    loc: Tz,
    /// bits 0..59
    minute: u64,
    /// bits 0..23
    hour: u64,
    /// bits 1..31
    dom: u64,
    /// bits 1..12
    month: u64,
    /// bits 0..6, Sunday = 0
    dow: u64,
    /// Records whether the day-of-month field was written as "*". Cron's day
    /// fields are a union when both are restricted and an intersection
    /// otherwise, which cannot be recovered from the bitsets alone (a fully
    /// enumerated field has the same bits as "*").
    dom_restricted: bool,
    /// Records whether the day-of-week field was written as "*".
    dow_restricted: bool,
}

/// DefaultCron runs the scan on weekdays at 10:00 in the configured timezone.
pub const DEFAULT_CRON: &str = "0 10 * * 1-5";

/// DefaultTimezone is used when the settings name no location.
pub const DEFAULT_TIMEZONE: &str = "UTC";

struct CronField {
    name: &'static str,
    min: i64,
    max: i64,
}

const CRON_FIELDS: [CronField; 5] = [
    CronField {
        name: "minute",
        min: 0,
        max: 59,
    },
    CronField {
        name: "hour",
        min: 0,
        max: 23,
    },
    CronField {
        name: "day of month",
        min: 1,
        max: 31,
    },
    CronField {
        name: "month",
        min: 1,
        max: 12,
    },
    CronField {
        name: "day of week",
        min: 0,
        max: 6,
    },
];

/// ParseSchedule parses a five-field cron expression against an IANA timezone
/// name. An empty timezone is UTC. Supported per field: "*", a number, "a-b",
/// a comma-separated list of either, and a "/step" suffix on any of those.
pub fn parse_schedule(expr: &str, timezone: &str) -> Res<Schedule> {
    let timezone = if timezone.trim().is_empty() {
        DEFAULT_TIMEZONE
    } else {
        timezone
    };
    let loc: Tz = timezone
        .parse()
        .map_err(|_| Error::Cron(format!("unknown timezone {timezone:?}")))?;
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(Error::Cron(format!(
            "cron expression must have 5 fields, got {}",
            parts.len()
        )));
    }
    let mut bits = [0u64; 5];
    for (i, part) in parts.iter().enumerate() {
        bits[i] = parse_cron_field(part, &CRON_FIELDS[i])?;
    }
    Ok(Schedule {
        expr: parts.join(" "),
        loc,
        minute: bits[0],
        hour: bits[1],
        dom: bits[2],
        month: bits[3],
        dow: bits[4],
        dom_restricted: parts[2] != "*",
        dow_restricted: parts[4] != "*",
    })
}

/// parse_cron_field turns one field into a bitset over `[f.min, f.max]`.
fn parse_cron_field(spec: &str, f: &CronField) -> Res<u64> {
    if spec.is_empty() {
        return Err(Error::Cron(format!("empty {} field", f.name)));
    }
    let mut set = 0u64;
    for term in spec.split(',') {
        let (range_part, step_part, has_step) = match term.split_once('/') {
            Some((a, b)) => (a, b, true),
            None => (term, "", false),
        };
        let mut step = 1i64;
        if has_step {
            match step_part.parse::<i64>() {
                Ok(n) if n > 0 => step = n,
                _ => {
                    return Err(Error::Cron(format!(
                        "invalid step {step_part:?} in {} field",
                        f.name
                    )));
                }
            }
        }

        let (mut lo, mut hi) = (f.min, f.max);
        if range_part == "*" {
            // Full range; a bare "*" with no step covers everything.
        } else if range_part.contains('-') {
            let (a, b) = range_part.split_once('-').unwrap_or((range_part, ""));
            lo = a
                .parse::<i64>()
                .map_err(|_| Error::Cron(format!("invalid {} field {term:?}", f.name)))?;
            hi = b
                .parse::<i64>()
                .map_err(|_| Error::Cron(format!("invalid {} field {term:?}", f.name)))?;
        } else {
            let n = range_part
                .parse::<i64>()
                .map_err(|_| Error::Cron(format!("invalid {} field {term:?}", f.name)))?;
            // A bare number with a step means "from n to the end of the range",
            // which is how "*/n" and "n/m" are conventionally read.
            lo = n;
            hi = if has_step { f.max } else { n };
        }
        if lo < f.min || hi > f.max || lo > hi {
            return Err(Error::Cron(format!(
                "{} field {term:?} is out of range {}-{}",
                f.name, f.min, f.max
            )));
        }
        let mut v = lo;
        while v <= hi {
            set |= 1u64 << (v as u32);
            v += step;
        }
    }
    Ok(set)
}

/// nextRunSearchDays bounds the [`Schedule::next`] scan. A schedule that does
/// not fire within four years (e.g. "0 0 30 2 *", February 30th) has no next run
/// at all, and the bound keeps that from becoming an unbounded loop.
const NEXT_RUN_SEARCH_DAYS: i64 = 366 * 4;

impl Schedule {
    /// The normalised expression.
    pub fn string(&self) -> &str {
        &self.expr
    }

    /// The timezone the expression is evaluated in.
    pub fn location(&self) -> Tz {
        self.loc
    }

    /// Matches reports whether the expression fires during `t`'s minute,
    /// evaluated in the schedule's timezone.
    pub fn matches(&self, t: DateTime<Utc>) -> bool {
        let t = t.with_timezone(&self.loc);
        if self.minute & (1u64 << t.minute()) == 0 {
            return false;
        }
        if self.hour & (1u64 << t.hour()) == 0 {
            return false;
        }
        if self.month & (1u64 << t.month()) == 0 {
            return false;
        }
        self.matches_day(t)
    }

    /// matches_day applies cron's day rule: when both day fields are restricted
    /// the expression fires if either matches, otherwise the restricted one
    /// decides.
    fn matches_day(&self, t: DateTime<Tz>) -> bool {
        let dom_hit = self.dom & (1u64 << t.day()) != 0;
        let dow_hit = self.dow & (1u64 << t.weekday().num_days_from_sunday()) != 0;
        if self.dom_restricted && self.dow_restricted {
            return dom_hit || dow_hit;
        }
        dom_hit && dow_hit
    }

    /// Next returns the first firing strictly after `t`, or `None` when the
    /// expression can never fire.
    pub fn next(&self, t: DateTime<Utc>) -> Option<DateTime<Utc>> {
        // Start at the top of the following minute: a schedule firing "now" has
        // already fired, so the next run is a later one.
        let truncated = t
            - Duration::nanoseconds(i64::from(t.timestamp_subsec_nanos()))
            - Duration::seconds(i64::from(t.second()));
        let mut cur = (truncated + Duration::minutes(1)).with_timezone(&self.loc);
        for _ in 0..=NEXT_RUN_SEARCH_DAYS {
            if self.month & (1u64 << cur.month()) == 0 || !self.matches_day(cur) {
                cur = start_of_next_day(cur);
                continue;
            }
            loop {
                if self.hour & (1u64 << cur.hour()) != 0
                    && self.minute & (1u64 << cur.minute()) != 0
                {
                    return Some(cur.with_timezone(&Utc));
                }
                let next = cur + Duration::minutes(1);
                // A DST jump can move the clock backwards past this day; treat it
                // as the day being finished rather than looping on the same minute.
                if next.day() != cur.day() {
                    cur = next;
                    break;
                }
                cur = next;
            }
            if cur.hour() != 0 || cur.minute() != 0 {
                cur = start_of_next_day(cur);
            }
        }
        None
    }
}

/// Midnight on the day after `t`, in `t`'s own location.
///
/// An ambiguous midnight (a fall-back transition) takes the earlier instant, which is what
/// `time.Date` also returns.
fn start_of_next_day(t: DateTime<Tz>) -> DateTime<Tz> {
    let loc = t.timezone();
    let next_day = t.date_naive().succ_opt().unwrap_or(t.date_naive());
    for hour in 0..24 {
        let naive = match next_day.and_hms_opt(hour, 0, 0) {
            Some(n) => n,
            None => continue,
        };
        if let Some(resolved) = loc.from_local_datetime(&naive).earliest() {
            return resolved;
        }
    }
    // Every hour of the day is unrepresentable, which no real zone produces.
    // Fall back to a whole day of absolute time so the search still advances.
    t + Duration::days(1)
}

#[cfg(test)]
pub(crate) mod tests {
    use chrono::{DateTime, TimeZone, Utc};
    use chrono_tz::Tz;

    use crate::coverage::{DEFAULT_CRON, parse_schedule};

    fn at(tz: &str, y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        let zone: Tz = tz.parse().expect("known timezone");
        zone.with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("unambiguous local time")
            .with_timezone(&Utc)
    }

    #[test]
    fn parse_schedule_rejects_bad_input() {
        for (name, expr, tz) in [
            ("too few fields", "0 10 * *", "UTC"),
            ("too many fields", "0 10 * * 1-5 2026", "UTC"),
            ("minute out of range", "60 10 * * *", "UTC"),
            ("hour out of range", "0 24 * * *", "UTC"),
            ("day of week out of range", "0 10 * * 7", "UTC"),
            ("month zero", "0 10 * 0 *", "UTC"),
            ("inverted range", "0 10-5 * * *", "UTC"),
            ("zero step", "*/0 * * * *", "UTC"),
            ("non-numeric", "abc * * * *", "UTC"),
            ("unknown timezone", "0 10 * * *", "Mars/Olympus"),
        ] {
            assert!(
                parse_schedule(expr, tz).is_err(),
                "{name}: parse_schedule({expr:?}, {tz:?}) accepted an invalid expression"
            );
        }
    }

    #[test]
    fn schedule_matches() {
        struct Case {
            name: &'static str,
            expr: &'static str,
            tz: &'static str,
            at: DateTime<Utc>,
            want: bool,
        }
        let cases = vec![
            // The default schedule, checked in the timezone it is written for: the
            // same instant is 10:00 in Seoul and 01:00 UTC, and only the Seoul
            // reading fires.
            Case {
                name: "weekday morning in the configured zone",
                expr: DEFAULT_CRON,
                tz: "Asia/Seoul",
                at: at("Asia/Seoul", 2026, 8, 20, 10, 0), // Thursday
                want: true,
            },
            Case {
                name: "same instant read as UTC does not fire",
                expr: DEFAULT_CRON,
                tz: "UTC",
                at: at("Asia/Seoul", 2026, 8, 20, 10, 0),
                want: false,
            },
            Case {
                name: "weekend is excluded by the day-of-week range",
                expr: DEFAULT_CRON,
                tz: "Asia/Seoul",
                at: at("Asia/Seoul", 2026, 8, 22, 10, 0), // Saturday
                want: false,
            },
            Case {
                name: "a minute later does not fire",
                expr: DEFAULT_CRON,
                tz: "Asia/Seoul",
                at: at("Asia/Seoul", 2026, 8, 20, 10, 1),
                want: false,
            },
            Case {
                name: "step matches every fifteenth minute",
                expr: "*/15 * * * *",
                tz: "UTC",
                at: at("UTC", 2026, 8, 20, 3, 30),
                want: true,
            },
            Case {
                name: "step skips the minutes between",
                expr: "*/15 * * * *",
                tz: "UTC",
                at: at("UTC", 2026, 8, 20, 3, 31),
                want: false,
            },
            Case {
                name: "comma list matches either hour",
                expr: "0 9,18 * * *",
                tz: "UTC",
                at: at("UTC", 2026, 8, 20, 18, 0),
                want: true,
            },
            // Cron's day fields are a union when both are restricted: the 1st of the
            // month fires even though it is not a Monday.
            Case {
                name: "restricted day fields union on day of month",
                expr: "0 0 1 * 1",
                tz: "UTC",
                at: at("UTC", 2026, 9, 1, 0, 0), // Tuesday
                want: true,
            },
            Case {
                name: "restricted day fields union on day of week",
                expr: "0 0 1 * 1",
                tz: "UTC",
                at: at("UTC", 2026, 9, 7, 0, 0), // Monday the 7th
                want: true,
            },
            Case {
                name: "restricted day fields match neither",
                expr: "0 0 1 * 1",
                tz: "UTC",
                at: at("UTC", 2026, 9, 8, 0, 0), // Tuesday the 8th
                want: false,
            },
        ];
        for c in cases {
            let s = parse_schedule(c.expr, c.tz).expect("parse_schedule");
            assert_eq!(s.matches(c.at), c.want, "{}", c.name);
        }
    }

    #[test]
    fn schedule_next_skips_the_weekend() {
        let s = parse_schedule(DEFAULT_CRON, "Asia/Seoul").expect("parse_schedule");
        // Friday just after the firing: the next one is Monday, not Saturday.
        let from = at("Asia/Seoul", 2026, 8, 21, 10, 0);
        let want = at("Asia/Seoul", 2026, 8, 24, 10, 0);
        assert_eq!(s.next(from), Some(want));
    }

    #[test]
    fn schedule_next_is_strictly_after_the_given_time() {
        let s = parse_schedule("*/10 * * * *", "UTC").expect("parse_schedule");
        let from = at("UTC", 2026, 8, 20, 3, 30);
        let want = at("UTC", 2026, 8, 20, 3, 40);
        assert_eq!(s.next(from), Some(want));
    }

    #[test]
    fn schedule_next_rolls_over_into_the_next_day() {
        let s = parse_schedule("0 2 * * *", "UTC").expect("parse_schedule");
        let from = at("UTC", 2026, 8, 20, 3, 0);
        let want = at("UTC", 2026, 8, 21, 2, 0);
        assert_eq!(s.next(from), Some(want));
    }

    #[test]
    fn schedule_next_an_expression_that_can_never_fire_returns_none() {
        // February the 30th does not exist, so the search runs out rather than
        // looping forever.
        let s = parse_schedule("0 0 30 2 *", "UTC").expect("parse_schedule");
        assert_eq!(s.next(at("UTC", 2026, 1, 1, 0, 0)), None);
    }
}
