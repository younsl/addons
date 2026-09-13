//! Renders durations and sizes the way the Go version printed them, so log
//! lines, CLI tables, and annotation values stay byte-for-byte comparable.

use std::fmt::Write as _;
use std::time::Duration;

/// Renders a duration in Go's `time.Duration` syntax (`1h30m0s`, `10s`,
/// `500ms`), which is what the config file uses and what every log line
/// printed before the port.
#[must_use]
pub fn go_duration(d: Duration) -> String {
    if d.is_zero() {
        return "0s".to_string();
    }
    if d < Duration::from_secs(1) {
        // Go picks the largest unit that keeps a non-zero integer part and
        // prints the remainder as a trimmed fraction: 436.421432ms, 1.5µs.
        let nanos = d.as_nanos();
        let (unit, scale) = if nanos >= 1_000_000 {
            ("ms", 1_000_000)
        } else if nanos >= 1_000 {
            ("µs", 1_000)
        } else {
            return format!("{nanos}ns");
        };
        let whole = nanos / scale;
        let frac = nanos % scale;
        if frac == 0 {
            return format!("{whole}{unit}");
        }
        let width = if scale == 1_000_000 { 6 } else { 3 };
        let mut f = format!("{frac:0width$}");
        while f.ends_with('0') {
            f.pop();
        }
        return format!("{whole}.{f}{unit}");
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

/// Rounds to the nearest multiple of `unit`, like Go's `Duration.Round`.
#[must_use]
pub fn round_duration(d: Duration, unit: Duration) -> Duration {
    if unit.is_zero() {
        return d;
    }
    let (n, u) = (d.as_nanos(), unit.as_nanos());
    let rem = n % u;
    let base = n - rem;
    let rounded = if rem * 2 >= u { base + u } else { base };
    Duration::from_nanos(u64::try_from(rounded).unwrap_or(u64::MAX))
}

/// Renders a byte count in the binary units Kubernetes capacities are written
/// in, so a column lines up with what kubectl shows.
#[must_use]
pub fn human_bytes(b: i64) -> String {
    const UNIT: i64 = 1024;
    if b < UNIT {
        return format!("{b}B");
    }
    let mut div = UNIT;
    let mut exp = 0usize;
    let mut n = b / UNIT;
    while n >= UNIT && exp < 4 {
        div *= UNIT;
        exp += 1;
        n /= UNIT;
    }
    #[allow(clippy::cast_precision_loss)]
    let value = b as f64 / div as f64;
    format!("{value:.1}{}i", ['K', 'M', 'G', 'T', 'P'][exp])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_duration_matches_go() {
        for (d, want) in [
            (Duration::ZERO, "0s"),
            (Duration::from_millis(500), "500ms"),
            (Duration::from_micros(1500), "1.5ms"),
            (Duration::from_nanos(436_421_432), "436.421432ms"),
            (Duration::from_nanos(1_500), "1.5µs"),
            (Duration::from_nanos(7), "7ns"),
            (Duration::from_secs(1), "1s"),
            (Duration::from_millis(1500), "1.5s"),
            (Duration::from_secs(90), "1m30s"),
            (Duration::from_hours(1), "1h0m0s"),
            (Duration::from_mins(90), "1h30m0s"),
            (Duration::from_hours(24), "24h0m0s"),
            (Duration::from_hours(512), "512h0m0s"),
        ] {
            assert_eq!(go_duration(d), want);
        }
    }

    #[test]
    fn round_duration_rounds_half_up() {
        let minute = Duration::from_mins(1);
        assert_eq!(round_duration(Duration::from_secs(89), minute), minute);
        assert_eq!(
            round_duration(Duration::from_secs(90), minute),
            Duration::from_mins(2)
        );
        assert_eq!(
            round_duration(Duration::from_millis(1499), Duration::from_millis(1)),
            Duration::from_millis(1499)
        );
        assert_eq!(
            round_duration(Duration::from_secs(5), Duration::ZERO),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn human_bytes_uses_binary_units() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(1023), "1023B");
        assert_eq!(human_bytes(1024), "1.0Ki");
        assert_eq!(human_bytes(20 * 1024 * 1024 * 1024), "20.0Gi");
        assert_eq!(human_bytes(1536 * 1024 * 1024), "1.5Gi");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024 * 1024), "5.0Ti");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024 * 1024 * 1024), "3.0Pi");
    }
}
