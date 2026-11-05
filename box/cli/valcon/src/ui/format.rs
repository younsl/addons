use chrono::{DateTime, Local};

/// Compact elapsed time such as `42s`, `3m05s`, `2h10m`, or `-` when never.
pub fn ago(t: Option<DateTime<Local>>, now: DateTime<Local>) -> String {
    let Some(t) = t else {
        return "-".into();
    };
    duration((now - t).num_seconds().max(0))
}

fn duration(secs: i64) -> String {
    match secs {
        ..60 => format!("{secs}s"),
        60..3600 => format!("{}m{:02}s", secs / 60, secs % 60),
        3600..86_400 => format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60),
        _ => format!("{}d{:02}h", secs / 86_400, (secs % 86_400) / 3600),
    }
}

/// `PTTL` in milliseconds: `-1` never expires, `-2` gone.
pub fn ttl(ms: i64) -> String {
    match ms {
        -1 => "never".into(),
        ..0 => "gone".into(),
        _ => duration(ms / 1000),
    }
}

/// Large counts as `950`, `12.3k`, `4.5M`, `-` when unknown.
pub fn count(v: Option<f64>) -> String {
    let Some(v) = v else {
        return "-".into();
    };
    match v.abs() {
        x if x >= 1e9 => format!("{:.1}G", v / 1e9),
        x if x >= 1e6 => format!("{:.1}M", v / 1e6),
        x if x >= 1e4 => format!("{:.1}k", v / 1e3),
        _ => format!("{v:.0}"),
    }
}

pub fn pct(v: Option<f64>) -> String {
    v.map_or_else(|| "-".into(), |v| format!("{v:.1}%"))
}

#[allow(clippy::cast_precision_loss)]
pub fn bytes(v: Option<i64>) -> String {
    let Some(v) = v else {
        return "-".into();
    };
    let f = v as f64;
    if v < 1024 {
        format!("{v}B")
    } else if v < 1_048_576 {
        format!("{:.1}K", f / 1024.0)
    } else {
        format!("{:.1}M", f / 1_048_576.0)
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    #[test]
    fn formats_each_range() {
        let now = Local::now();
        assert_eq!(ago(None, now), "-");
        assert_eq!(ago(Some(now - Duration::seconds(42)), now), "42s");
        assert_eq!(ago(Some(now - Duration::seconds(185)), now), "3m05s");
        assert_eq!(ago(Some(now - Duration::seconds(7800)), now), "2h10m");
        assert_eq!(ago(Some(now - Duration::seconds(90_000)), now), "1d01h");
        assert_eq!(ago(Some(now + Duration::seconds(5)), now), "0s");
    }

    #[test]
    fn ttl_count_pct_bytes() {
        assert_eq!(
            (ttl(-1), ttl(-2), ttl(61_000)),
            ("never".into(), "gone".into(), "1m01s".into())
        );
        assert_eq!(count(None), "-");
        assert_eq!(count(Some(950.0)), "950");
        assert_eq!(count(Some(12_345.0)), "12.3k");
        assert_eq!(count(Some(4_500_000.0)), "4.5M");
        assert_eq!(count(Some(2e9)), "2.0G");
        assert_eq!((pct(None), pct(Some(12.34))), ("-".into(), "12.3%".into()));
        assert_eq!(
            (
                bytes(None),
                bytes(Some(56)),
                bytes(Some(2048)),
                bytes(Some(3_145_728))
            ),
            ("-".into(), "56B".into(), "2.0K".into(), "3.0M".into())
        );
    }
}
