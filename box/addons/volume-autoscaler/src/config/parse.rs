//! The low-level value parsers used by `load`: durations
//! and Prometheus durations. They are deliberately free of any `Config`
//! knowledge so each parser is testable in isolation.

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use super::ConfigError;

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
