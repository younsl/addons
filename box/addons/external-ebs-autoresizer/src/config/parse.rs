//! The low-level value parsers used by `load`: sizes, durations, tag filters,
//! and Prometheus durations. They are deliberately free of any `Config`
//! knowledge so each parser is testable in isolation.

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use super::{ConfigError, TagFilter};

/// The largest EBS volume size AWS supports (64 TiB, io2 Block Express). No
/// grow amount can meaningfully exceed it.
const MAX_EBS_VOLUME_GIB: i64 = 64 * 1024;

/// Parses an absolute growth value with a MiB or GiB unit (e.g. `10GiB`,
/// `5120MiB`) into whole GiB. EBS volumes are sized in GiB, so a MiB value is
/// rounded up to the next whole GiB to guarantee at least the requested
/// growth. The unit is required and case-insensitive; the shorthand forms `Gi`
/// and `Mi` are also accepted.
pub fn parse_grow_amount(raw: &str) -> Result<i32, ConfigError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(ConfigError::Invalid(
            "empty value, expected a number with a MiB or GiB unit such as 10GiB".into(),
        ));
    }
    let lower = s.to_ascii_lowercase();
    let (num, to_gib): (&str, fn(i64) -> i64) = if let Some(n) = lower.strip_suffix("gib") {
        (n, |n| n)
    } else if let Some(n) = lower.strip_suffix("mib") {
        (n, mib_to_gib)
    } else if let Some(n) = lower.strip_suffix("gi") {
        (n, |n| n)
    } else if let Some(n) = lower.strip_suffix("mi") {
        (n, mib_to_gib)
    } else {
        return Err(ConfigError::Invalid(format!(
            "value {raw:?} must end with a MiB or GiB unit such as 10GiB or 5120MiB"
        )));
    };
    let num = num.trim();
    let n: i64 = num.parse().map_err(|err| {
        ConfigError::Invalid(format!(
            "value {raw:?} has an invalid number {num:?}: {err}"
        ))
    })?;
    if n <= 0 {
        return Err(ConfigError::Invalid(format!(
            "value {raw:?} must be greater than 0"
        )));
    }
    let gib = to_gib(n);
    if gib > MAX_EBS_VOLUME_GIB {
        return Err(ConfigError::Invalid(format!(
            "value {raw:?} exceeds the EBS maximum volume size of 64TiB"
        )));
    }
    Ok(i32::try_from(gib).unwrap_or(i32::MAX))
}

/// Converts MiB to GiB, rounding up so the resulting whole GiB is never less
/// than the requested MiB. The caller guarantees `mib > 0`.
const fn mib_to_gib(mib: i64) -> i64 {
    (mib - 1) / 1024 + 1
}

/// Parses `Key=Value,Key2=Value2` into tag filters.
pub fn parse_tag_filters(raw: &str) -> Result<Vec<TagFilter>, ConfigError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for pair in raw.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let Some((key, value)) = pair.split_once('=') else {
            return Err(ConfigError::Invalid(format!(
                "invalid tag filter {pair:?}, expected Key=Value"
            )));
        };
        let (key, value) = (key.trim(), value.trim());
        if key.is_empty() || value.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "invalid tag filter {pair:?}, expected Key=Value"
            )));
        }
        out.push(TagFilter {
            key: key.to_string(),
            value: value.to_string(),
        });
    }
    Ok(out)
}

/// Parses a Go duration string (`30s`, `5m`, `1h30m`, `1.5h`, `500ms`). An
/// invalid value (including empty) is a hard error so misconfiguration
/// (`1hour`, `5min`, or a unitless `300`) fails at startup instead of running
/// with a surprising value.
pub fn parse_duration(name: &str, raw: &str) -> Result<Duration, ConfigError> {
    parse_go_duration(raw.trim()).ok_or_else(|| {
        ConfigError::Invalid(format!(
            "invalid {name} {raw:?}: must be a Go duration such as 30s, 5m, 1h, 1h30m"
        ))
    })
}

/// The `time.ParseDuration` grammar: a sequence of decimal numbers, each with
/// an optional fraction and a mandatory unit suffix. A bare `0` is accepted.
pub fn parse_go_duration(s: &str) -> Option<Duration> {
    if s == "0" {
        return Some(Duration::ZERO);
    }
    let s = s.strip_prefix('+').unwrap_or(s);
    if s.is_empty() || s.starts_with('-') {
        return None;
    }
    let mut rest = s;
    let mut total_nanos: f64 = 0.0;
    while !rest.is_empty() {
        let num_end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let num = &rest[..num_end];
        if num.is_empty() || num == "." {
            return None;
        }
        let value: f64 = num.parse().ok()?;
        rest = &rest[num_end..];
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let unit = &rest[..unit_end];
        let scale: f64 = match unit {
            "ns" => 1.0,
            "us" | "µs" | "μs" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3600e9,
            _ => return None,
        };
        total_nanos = value.mul_add(scale, total_nanos);
        rest = &rest[unit_end..];
    }
    if !total_nanos.is_finite() || total_nanos < 0.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(Duration::from_nanos(total_nanos.round() as u64))
}

/// The Prometheus duration grammar: at least one unit-suffixed integer, units
/// in descending order, no sign and no fraction. Validating rather than
/// parsing keeps these values as the strings `PromQL` needs, and because they
/// are interpolated into a query, the pattern is also what stops an
/// operator-supplied value from injecting arbitrary `PromQL`.
static PROM_DURATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([0-9]+y)?([0-9]+w)?([0-9]+d)?([0-9]+h)?([0-9]+m)?([0-9]+s)?([0-9]+ms)?$")
        .expect("valid regex")
});

/// Each Prometheus duration unit and its length, in descending order (the
/// order of the capture groups above).
const PROM_UNITS: [(&str, u64); 7] = [
    ("y", 365 * 24 * 3600 * 1000),
    ("w", 7 * 24 * 3600 * 1000),
    ("d", 24 * 3600 * 1000),
    ("h", 3600 * 1000),
    ("m", 60 * 1000),
    ("s", 1000),
    ("ms", 1),
];

/// Validates a Prometheus duration string and returns it trimmed alongside its
/// length. Both forms are needed: the string goes into the query verbatim,
/// while the length is what the confidence gate uses to work out how many data
/// points a full window holds.
pub fn parse_prom_duration(name: &str, raw: &str) -> Result<(String, Duration), ConfigError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "{name} is required: a Prometheus duration such as 7d, 12h, 30s"
        )));
    }
    let Some(groups) = PROM_DURATION.captures(s) else {
        return Err(ConfigError::Invalid(format!(
            "invalid {name} {s:?}: must be a Prometheus duration such as 7d, 12h, 30s, with units in descending order"
        )));
    };
    let mut total_ms: u64 = 0;
    for (i, (suffix, len)) in PROM_UNITS.iter().enumerate() {
        let Some(group) = groups.get(i + 1) else {
            continue;
        };
        let digits = group.as_str().trim_end_matches(suffix);
        let n: u64 = digits
            .parse()
            .map_err(|err| ConfigError::Invalid(format!("invalid {name} {s:?}: {err}")))?;
        total_ms = total_ms.saturating_add(n.saturating_mul(*len));
    }
    if total_ms == 0 {
        return Err(ConfigError::Invalid(format!(
            "invalid {name} {s:?}: must be greater than 0"
        )));
    }
    Ok((s.to_string(), Duration::from_millis(total_ms)))
}

/// The Prometheus label name grammar. A configured node label is interpolated
/// into a `sum by (...)` clause, where it cannot be quoted, so it must be
/// checked against the grammar rather than escaped.
static LABEL_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z_][a-zA-Z0-9_]*$").expect("valid regex"));

/// Reports whether `s` is a valid Prometheus label name.
#[must_use]
pub fn is_label_name(s: &str) -> bool {
    LABEL_NAME.is_match(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grow_amount_units_and_rounding() {
        assert_eq!(parse_grow_amount("10GiB").unwrap(), 10);
        assert_eq!(parse_grow_amount(" 10gib ").unwrap(), 10);
        assert_eq!(parse_grow_amount("10Gi").unwrap(), 10);
        assert_eq!(parse_grow_amount("5120MiB").unwrap(), 5);
        assert_eq!(parse_grow_amount("5121MiB").unwrap(), 6, "rounds up");
        assert_eq!(parse_grow_amount("1Mi").unwrap(), 1);
        assert_eq!(parse_grow_amount("1024mi").unwrap(), 1);
        assert_eq!(parse_grow_amount("65536GiB").unwrap(), 65536);
        for bad in [
            "", "10", "10GB", "10 TB", "-1GiB", "0GiB", "xGiB", "65537GiB", "1.5GiB",
        ] {
            assert!(parse_grow_amount(bad).is_err(), "{bad}");
        }
        assert!(
            parse_grow_amount("10GB")
                .unwrap_err()
                .to_string()
                .contains("must end with a MiB or GiB unit")
        );
    }

    #[test]
    fn tag_filters() {
        assert!(parse_tag_filters("").unwrap().is_empty());
        assert!(parse_tag_filters("  ,  ").unwrap().is_empty());
        let f = parse_tag_filters("Env=prod, Team = infra ,").unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].key, "Env");
        assert_eq!(f[0].value, "prod");
        assert_eq!(f[1].key, "Team");
        assert_eq!(f[1].value, "infra");
        for bad in ["Env", "=prod", "Env=", "Env=prod,bad"] {
            assert!(parse_tag_filters(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn go_durations() {
        assert_eq!(parse_go_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_go_duration("5m").unwrap(), Duration::from_mins(5));
        assert_eq!(parse_go_duration("1h30m").unwrap(), Duration::from_mins(90));
        assert_eq!(parse_go_duration("1.5h").unwrap(), Duration::from_mins(90));
        assert_eq!(
            parse_go_duration("500ms").unwrap(),
            Duration::from_millis(500)
        );
        assert_eq!(parse_go_duration("2us").unwrap(), Duration::from_micros(2));
        assert_eq!(parse_go_duration("3µs").unwrap(), Duration::from_micros(3));
        assert_eq!(parse_go_duration("7ns").unwrap(), Duration::from_nanos(7));
        assert_eq!(parse_go_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_go_duration("+1s").unwrap(), Duration::from_secs(1));
        assert_eq!(
            parse_go_duration("1h0m0s").unwrap(),
            Duration::from_hours(1)
        );
        for bad in ["", "300", "5min", "1hour", "-1s", "s", ".s", "1d", "1h-1m"] {
            assert!(parse_go_duration(bad).is_none(), "{bad}");
        }
        assert!(parse_duration("reconcileInterval", " 5m ").is_ok());
        let err = parse_duration("reconcileInterval", "5min")
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid reconcileInterval \"5min\""), "{err}");
    }

    #[test]
    fn prom_durations() {
        let (s, d) = parse_prom_duration("w", " 7d ").unwrap();
        assert_eq!(s, "7d");
        assert_eq!(d, Duration::from_hours(168));
        assert_eq!(
            parse_prom_duration("w", "1w2d3h4m5s6ms").unwrap().1,
            Duration::from_millis(
                (7 * 24 * 3600 + 2 * 24 * 3600 + 3 * 3600 + 4 * 60 + 5) * 1000 + 6
            )
        );
        assert_eq!(
            parse_prom_duration("w", "1y").unwrap().1,
            Duration::from_hours(8760)
        );
        for bad in ["", "0", "0d", "7", "5min", "1.5h", "1h1d", "-1d", "1d;drop"] {
            assert!(parse_prom_duration("w", bad).is_err(), "{bad}");
        }
        assert!(
            parse_prom_duration("w", "")
                .unwrap_err()
                .to_string()
                .contains("is required")
        );
    }

    #[test]
    fn label_names() {
        assert!(is_label_name("node"));
        assert!(is_label_name("_x9"));
        assert!(!is_label_name(""));
        assert!(!is_label_name("9node"));
        assert!(!is_label_name("node)"));
        assert!(!is_label_name("no-de"));
    }
}
