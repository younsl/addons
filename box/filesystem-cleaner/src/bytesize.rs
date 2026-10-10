//! Human-readable byte formatting for log output.

const UNIT: u64 = 1024;
const PREFIXES: &[u8] = b"KMGTPE";

/// Formats `n` bytes using binary (1024-based) units, e.g. `1.5 MiB`.
#[expect(clippy::cast_precision_loss)]
pub fn human(n: u64) -> String {
    if n < UNIT {
        return format!("{n} B");
    }
    let mut div = UNIT;
    let mut exp = 0;
    let mut m = n / UNIT;
    while m >= UNIT {
        div *= UNIT;
        exp += 1;
        m /= UNIT;
    }
    format!(
        "{:.1} {}iB",
        n as f64 / div as f64,
        char::from(PREFIXES[exp])
    )
}

#[cfg(test)]
mod tests {
    use super::human;

    #[test]
    fn formats_bytes_below_one_kib_as_plain_bytes() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1), "1 B");
        assert_eq!(human(1023), "1023 B");
    }

    #[test]
    fn formats_binary_units_with_one_decimal() {
        assert_eq!(human(1024), "1.0 KiB");
        assert_eq!(human(1536), "1.5 KiB");
        assert_eq!(human(1024 * 1024), "1.0 MiB");
        assert_eq!(human(5 * 1024 * 1024 * 1024), "5.0 GiB");
        assert_eq!(human(1024_u64.pow(4)), "1.0 TiB");
        assert_eq!(human(1024_u64.pow(5)), "1.0 PiB");
        assert_eq!(human(u64::MAX), "16.0 EiB");
    }
}
