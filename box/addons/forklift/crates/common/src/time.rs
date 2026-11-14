//! Timestamp encoding shared by the log lines and every table.

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
