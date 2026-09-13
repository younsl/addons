//! Timestamp encoding shared by every table.

use chrono::{DateTime, SecondsFormat, TimeZone, Utc};

/// The current instant in RFC3339Nano, UTC.
pub fn now_rfc3339() -> String {
    format_time(Utc::now())
}

pub fn format_time<Tz: TimeZone>(t: DateTime<Tz>) -> String {
    let t = t.with_timezone(&Utc);
    let nanos = t.timestamp_subsec_nanos();
    if nanos == 0 {
        return t.to_rfc3339_opts(SecondsFormat::Secs, true);
    }
    let mut frac = format!("{nanos:09}");
    while frac.ends_with('0') {
        frac.pop();
    }
    format!("{}.{}Z", t.format("%Y-%m-%dT%H:%M:%S"), frac)
}

/// Formats an optional timestamp; `None` becomes SQL NULL.
pub fn format_time_opt<Tz: TimeZone>(t: Option<DateTime<Tz>>) -> Option<String> {
    t.map(format_time)
}

pub fn parse_time(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|_| DateTime::<Utc>::from_timestamp(0, 0).expect("epoch"))
}

/// Parses an optional column; `None` and the empty string map to `None`.
pub fn parse_time_opt(s: Option<&str>) -> Option<DateTime<Utc>> {
    match s {
        None | Some("") => None,
        Some(v) => Some(parse_time(v)),
    }
}

///
/// Two encodings reach this function and both must answer true. Rows read back from the
/// database use the Unix epoch, because [`parse_time`] maps an empty or unparseable NOT NULL
/// column to it.
pub fn is_zero(t: DateTime<Utc>) -> bool {
    t.timestamp() <= 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn format_trims_trailing_fraction_zeros_like_go() {
        let base = NaiveDate::from_ymd_opt(2024, 3, 5)
            .unwrap()
            .and_hms_opt(7, 8, 9)
            .unwrap();
        let t = Utc.from_utc_datetime(&base);
        assert_eq!(format_time(t), "2024-03-05T07:08:09Z");
        let t = Utc.from_utc_datetime(&base.with_nanosecond(120_000_000).unwrap());
        assert_eq!(format_time(t), "2024-03-05T07:08:09.12Z");
        let t = Utc.from_utc_datetime(&base.with_nanosecond(123_456_789).unwrap());
        assert_eq!(format_time(t), "2024-03-05T07:08:09.123456789Z");
    }

    #[test]
    fn parse_roundtrips_and_accepts_offsets() {
        let s = "2024-03-05T07:08:09.5Z";
        assert_eq!(format_time(parse_time(s)), s);
        let t = parse_time("2024-03-05T16:08:09+09:00");
        assert_eq!(format_time(t), "2024-03-05T07:08:09Z");
        assert!(is_zero(parse_time("garbage")));
        assert!(is_zero(
            DateTime::parse_from_rfc3339("0001-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        ));
        assert!(!is_zero(parse_time("2024-03-05T07:08:09Z")));
        assert!(parse_time_opt(None).is_none());
        assert!(parse_time_opt(Some("")).is_none());
        assert!(parse_time_opt(Some(s)).is_some());
    }

    use chrono::Timelike;
}
