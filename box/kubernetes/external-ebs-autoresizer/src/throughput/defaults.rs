//! Fixed policy and the operator-facing configuration of the recommender.
//! The constants were configuration once: every one of them is either an AWS
//! limit, a value derived from how node exporter works, or a judgement call
//! that does not vary per cluster. Exposing them as settings only created
//! ways to configure the recommender into producing nothing.

use std::time::Duration;

use super::decide::{GP3_MAX_THROUGHPUT_MIBPS, GP3_MIN_THROUGHPUT_MIBPS, Settings};
use super::query::Query;

/// The prefix of every annotation key written on a Node. Re-exported from
/// the shared annotations module, which owns it because the unused volume
/// scanner writes under the same prefix on other objects.
pub const ANNOTATION_PREFIX: &str = crate::annotations::PREFIX;

/// Selects which block devices count toward a node's throughput. It covers
/// every device naming AWS produces (`NVMe` on Nitro, xvd and sd elsewhere)
/// and excludes dm-*, loop*, and md*, whose IO is already counted on the
/// underlying device.
const DEVICE_REGEX: &str = "nvme[0-9]+n[0-9]+|xvd[a-z]+|sd[a-z]+";

/// The range passed to `rate()`. It must span at least two scrapes, and 1m
/// covers every common scrape interval (15s, 30s, 60s).
const RATE_WINDOW: &str = "1m";

/// The subquery resolution. Below the scrape interval it adds no information
/// and multiplies query cost; above it, bursts start averaging away.
const QUERY_STEP: &str = "1m";

/// The quantile of per-step throughput taken as the peak. 1.0 would let a
/// single spike set the recommendation for the whole window.
const QUANTILE: f64 = 0.99;

/// Added on top of the observed peak.
const HEADROOM_PERCENT: i32 = 30;

/// Quantizes the recommendation, and is the hysteresis that keeps it from
/// flapping. 125 is the gp3 baseline throughput, so every recommendation is
/// a whole number of baselines.
const STEP_MIBPS: i32 = 125;

/// The fraction of the observation window that must actually hold data
/// before a recommendation is trusted. A fraction rather than a sample count
/// because the two are not independent: a 7d window at a 1m step holds 10080
/// points, but a 12h window holds 720.
const MIN_SAMPLE_COVERAGE: f64 = 0.3;

/// Bounds each query. Well above the other sinks' timeouts because a
/// multi-day subquery over every node is genuinely expensive.
pub const QUERY_TIMEOUT: Duration = Duration::from_mins(1);

/// The recommender's operator-facing configuration: only the values a
/// cluster genuinely differs on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// The metric label carrying the Kubernetes node name.
    pub metric_node_name_label: String,
    /// How far back the observation window reaches, as a Prometheus
    /// duration.
    pub lookback: String,
    /// `lookback` parsed.
    pub lookback_duration: Duration,
    /// Computes and reports recommendations without writing any annotation.
    pub dry_run: bool,
}

impl Config {
    /// Builds the `PromQL` for this configuration.
    #[must_use]
    pub fn query(&self) -> Query {
        Query {
            node_label: self.metric_node_name_label.clone(),
            device_regex: DEVICE_REGEX.into(),
            rate_window: RATE_WINDOW.into(),
            lookback: self.lookback.clone(),
            step: QUERY_STEP.into(),
            quantile: QUANTILE,
        }
    }

    /// Builds the decision tunables, deriving the minimum sample count from
    /// the window length so the confidence gate scales with whatever
    /// lookback the operator chose.
    #[must_use]
    pub fn settings(&self) -> Settings {
        Settings {
            headroom_percent: HEADROOM_PERCENT,
            step_mibps: STEP_MIBPS,
            min_throughput_mibps: GP3_MIN_THROUGHPUT_MIBPS,
            max_throughput_mibps: GP3_MAX_THROUGHPUT_MIBPS,
            min_samples: min_samples(self.lookback_duration),
        }
    }

    /// How old a Node must be before it is worth querying at all. Derived
    /// from the same fraction as the sample gate: a node younger than this
    /// cannot possibly hold enough data points, so querying it can only ever
    /// produce `insufficient_samples`.
    #[must_use]
    pub fn min_node_age(&self) -> Duration {
        self.lookback_duration.mul_f64(MIN_SAMPLE_COVERAGE)
    }

    /// The human-readable observation window, written to the annotation so a
    /// reader knows which lookback and quantile produced the numbers.
    #[must_use]
    pub fn window(&self) -> String {
        format!("{}/p99", self.lookback)
    }
}

/// How many data points the window must hold to be trusted. Never below 2,
/// since a single point cannot describe a peak.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn min_samples(lookback: Duration) -> usize {
    if lookback.is_zero() {
        return 2;
    }
    let full = lookback.as_secs_f64() / 60.0;
    ((full * MIN_SAMPLE_COVERAGE).ceil() as usize).max(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(lookback: &str, secs: u64) -> Config {
        Config {
            metric_node_name_label: "node".into(),
            lookback: lookback.into(),
            lookback_duration: Duration::from_secs(secs),
            dry_run: false,
        }
    }

    #[test]
    fn settings_scale_with_the_window() {
        let week = cfg("7d", 7 * 24 * 3600);
        assert_eq!(week.settings().min_samples, 3024);
        assert_eq!(cfg("12h", 12 * 3600).settings().min_samples, 216);
        assert_eq!(cfg("1m", 60).settings().min_samples, 2);
        assert_eq!(cfg("", 0).settings().min_samples, 2);
        assert_eq!(week.window(), "7d/p99");
        assert_eq!(week.query().node_label, "node");
        assert_eq!(week.query().lookback, "7d");
        assert!((week.query().quantile - 0.99).abs() < f64::EPSILON);
    }

    #[test]
    fn min_node_age_matches_the_sample_gate() {
        let c = cfg("7d", 7 * 24 * 3600);
        let age = c.min_node_age();
        // A node exactly this old holds exactly min_samples minutes of data.
        let minutes = age.as_secs_f64() / 60.0;
        assert!((minutes - 3024.0).abs() < 0.001, "{minutes}");
    }
}
