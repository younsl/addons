//! The traffic distribution of one connection during its maintenance window,
//! and where the present moment sits inside it.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

use super::client::Sample;

// History shaping constants. They are not configuration because they are
// properties of the question rather than of a cluster.

/// How much history the distribution is drawn from. Four weeks covers four
/// occurrences of a weekly window, so one unusual week cannot define normal.
pub const LOOKBACK: Duration = Duration::from_hours(28 * 24);
/// The resolution of the historical range. It matches `SAMPLE_WINDOW`, so a
/// historical point and the current one measure the same span.
pub const STEP: Duration = Duration::from_mins(5);
/// How far back "now" reaches. The maximum over it, not the last sample, is
/// what gets judged: a transfer that started three minutes ago is traffic the
/// replacement would interrupt.
pub const SUSTAIN: Duration = Duration::from_mins(15);
/// The smallest distribution worth a percentile. Below it the history is
/// treated as unreadable, which `onError` then decides.
pub const MIN_SAMPLES: usize = 24;
/// The target the gate falls back to once AWS is about to apply the
/// maintenance itself.
pub const URGENT_PERCENTILE: f64 = 50.0;

/// In-window samples sorted ascending, plus the same samples grouped by clock
/// time for the recommendation.
#[derive(Debug, Default)]
pub struct History {
    pub values: Vec<f64>,
    slots: BTreeMap<String, Vec<f64>>,
}

impl History {
    /// Keeps the samples that fell inside a past maintenance window.
    ///
    /// Filtering by the window is what makes the percentile answer the right
    /// question. Comparing a midday moment against a distribution that
    /// includes every night would judge business hours against sleeping hours.
    pub fn new(
        samples: &[Sample],
        in_window: Option<&(dyn Fn(DateTime<Utc>) -> bool + Send + Sync)>,
        tz: Option<Tz>,
    ) -> Self {
        let mut h = Self::default();
        for s in samples {
            if let Some(f) = in_window
                && !f(s.at)
            {
                continue;
            }
            h.values.push(s.value);
            h.slots.entry(slot_key(s.at, tz)).or_default().push(s.value);
        }
        h.values.sort_by(f64::total_cmp);
        h
    }

    /// The value at `p` percent of the distribution, interpolating between
    /// the two neighbouring samples.
    #[must_use]
    pub fn percentile(&self, p: f64) -> f64 {
        let n = self.values.len();
        if n == 0 {
            return 0.0;
        }
        if p <= 0.0 {
            return self.values[0];
        }
        if p >= 100.0 {
            return self.values[n - 1];
        }
        #[allow(clippy::cast_precision_loss)]
        let pos = (p / 100.0) * (n - 1) as f64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let lower = pos.floor() as usize;
        if lower >= n - 1 {
            return self.values[n - 1];
        }
        #[allow(clippy::cast_precision_loss)]
        let frac = pos - lower as f64;
        self.values[lower] + frac * (self.values[lower + 1] - self.values[lower])
    }

    /// What share of the distribution `v` is at or above, so a verdict can say
    /// where the moment sits rather than only whether it passed.
    #[must_use]
    pub fn rank(&self, v: f64) -> f64 {
        if self.values.is_empty() {
            return 0.0;
        }
        let below = self.values.partition_point(|x| *x < v);
        #[allow(clippy::cast_precision_loss)]
        let r = 100.0 * below as f64 / self.values.len() as f64;
        r
    }

    /// The clock time of the window's calmest slot and its median, for the
    /// recommendation shown while the gate is holding. `None` when the history
    /// is too thin to name one.
    ///
    /// The median per slot rather than the minimum: one quiet Tuesday is luck,
    /// and a recommendation an approver is meant to act on should point at a
    /// habit.
    #[must_use]
    pub fn quietest(&self) -> Option<(String, f64)> {
        let mut best: Option<(&str, f64)> = None;
        for (at, values) in &self.slots {
            // A slot seen once carries no information about what usually
            // happens then.
            if values.len() < 2 {
                continue;
            }
            let mut sorted = values.clone();
            sorted.sort_by(f64::total_cmp);
            let median = sorted[sorted.len() / 2];
            // BTreeMap iterates in key order, so a strict less-than keeps the
            // earliest clock time on a tie.
            if best.is_none_or(|(_, m)| median < m) {
                best = Some((at, median));
            }
        }
        best.map(|(at, m)| (at.to_string(), m))
    }
}

/// Truncates to the step and renders `HH:MM` in the window's timezone.
fn slot_key(at: DateTime<Utc>, tz: Option<Tz>) -> String {
    let step_secs = i64::try_from(STEP.as_secs()).unwrap_or(300);
    let truncated =
        DateTime::from_timestamp(at.timestamp() / step_secs * step_secs, 0).unwrap_or(at);
    tz.map_or_else(
        || {
            let local = chrono::Local.from_utc_datetime(&truncated.naive_utc());
            format!("{:02}:{:02}", local.hour(), local.minute())
        },
        |tz| {
            let local = truncated.with_timezone(&tz);
            format!("{:02}:{:02}", local.hour(), local.minute())
        },
    )
}

/// The highest traffic seen in the last `SUSTAIN` window, and `None` when
/// nothing was scraped in it.
///
/// Missing recent samples are not the same as an idle tunnel: a broken
/// exporter would otherwise read as perfectly quiet, which is the one wrong
/// answer that leads to a replacement during a peak.
#[must_use]
pub fn sustained_now(samples: &[Sample], now: DateTime<Utc>) -> Option<f64> {
    let cutoff = now - chrono::TimeDelta::from_std(SUSTAIN).unwrap_or_default();
    samples
        .iter()
        .filter(|s| s.at >= cutoff)
        .map(|s| s.value)
        .reduce(f64::max)
}

/// Renders a recommended slot with its timezone, since an approver reading the
/// card needs to know which clock 11:35 is on.
#[must_use]
pub fn format_clock(at: &str, tz: Option<Tz>, now: DateTime<Utc>) -> String {
    tz.map_or_else(
        || at.to_string(),
        |tz| format!("{at} {}", now.with_timezone(&tz).format("%Z")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(values: &[f64]) -> Vec<Sample> {
        let base = DateTime::from_timestamp(1_785_000_000, 0).unwrap();
        values
            .iter()
            .enumerate()
            .map(|(i, v)| Sample {
                at: base + chrono::TimeDelta::from_std(STEP).unwrap() * i32::try_from(i).unwrap(),
                value: *v,
            })
            .collect()
    }

    #[test]
    fn percentile_interpolates() {
        let h = History::new(
            &samples(&[10.0, 30.0, 20.0, 40.0, 50.0]),
            None,
            Some(chrono_tz::UTC),
        );
        assert_eq!(h.values, vec![10.0, 20.0, 30.0, 40.0, 50.0]);
        assert!((h.percentile(0.0) - 10.0).abs() < f64::EPSILON);
        assert!((h.percentile(100.0) - 50.0).abs() < f64::EPSILON);
        assert!((h.percentile(50.0) - 30.0).abs() < f64::EPSILON);
        assert!((h.percentile(25.0) - 20.0).abs() < f64::EPSILON);
        assert!((h.percentile(12.5) - 15.0).abs() < f64::EPSILON);
        assert!((h.percentile(99.99) - 49.996).abs() < 0.01);
        assert!(History::default().percentile(50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rank_reports_share_below() {
        let h = History::new(
            &samples(&[10.0, 20.0, 30.0, 40.0]),
            None,
            Some(chrono_tz::UTC),
        );
        assert!((h.rank(5.0)).abs() < f64::EPSILON);
        assert!((h.rank(25.0) - 50.0).abs() < f64::EPSILON);
        assert!((h.rank(30.0) - 50.0).abs() < f64::EPSILON);
        assert!((h.rank(100.0) - 100.0).abs() < f64::EPSILON);
        assert!(History::default().rank(1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn window_filter_and_quietest_slot() {
        // 12 samples over an hour; the window filter keeps the first half.
        let all = samples(&[
            9.0, 8.0, 7.0, 6.0, 5.0, 4.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0,
        ]);
        let cutoff = all[6].at;
        let in_window = |t: DateTime<Utc>| t < cutoff;
        let h = History::new(&all, Some(&in_window), Some(chrono_tz::UTC));
        assert_eq!(h.values.len(), 6);
        // Every slot is seen once, so no recommendation.
        assert!(h.quietest().is_none());

        // Two days of samples at the same clock times: slot medians decide.
        let mut two_days = samples(&[5.0, 1.0, 3.0]);
        let day = chrono::TimeDelta::days(1);
        two_days.extend(samples(&[5.0, 9.0, 3.0]).into_iter().map(|s| Sample {
            at: s.at + day,
            ..s
        }));
        two_days.extend(samples(&[5.0, 9.0, 3.0]).into_iter().map(|s| Sample {
            at: s.at + day * 2,
            ..s
        }));
        let h = History::new(&two_days, None, Some(chrono_tz::UTC));
        let (at, median) = h.quietest().unwrap();
        // 1_785_000_000 is 2026-07-25 17:20:00 UTC; the third slot is 17:30.
        assert_eq!(at, "17:30");
        assert!((median - 3.0).abs() < f64::EPSILON);

        // Rendered in the window's timezone.
        let h = History::new(&two_days, None, Some(chrono_tz::Asia::Seoul));
        assert_eq!(h.quietest().unwrap().0, "02:30");
        // A missing timezone falls back to the local clock without panicking.
        let _ = History::new(&two_days, None, None);
    }

    #[test]
    fn sustained_now_takes_the_recent_peak() {
        let s = samples(&[50.0, 1.0, 2.0, 9.0, 3.0]);
        let now = s[4].at;
        // 15 minutes covers the last four samples; the 50 is older.
        assert!((sustained_now(&s, now).unwrap() - 9.0).abs() < f64::EPSILON);
        assert!(sustained_now(&s, now + chrono::TimeDelta::hours(1)).is_none());
        assert!(sustained_now(&[], now).is_none());
    }

    #[test]
    fn clock_carries_zone() {
        let now = DateTime::from_timestamp(1_785_000_000, 0).unwrap();
        assert_eq!(
            format_clock("02:35", Some(chrono_tz::Asia::Seoul), now),
            "02:35 KST"
        );
        assert_eq!(format_clock("02:35", None, now), "02:35");
    }
}
