//! The annotations written on each Node.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::decide::{ACTION_DECREASE, ACTION_INCREASE, ACTION_NONE, Decision};
use super::defaults::ANNOTATION_PREFIX;
use super::observation::Observation;
use crate::pvscan::annotations::observed_at_is_stale;

/// Annotation key suffixes written on each Node, joined to the prefix as
/// `<prefix>/<suffix>`. Keys are stable identifiers.
pub const KEY_VOLUME_ID: &str = "volume-id";
pub const KEY_CURRENT_MIBPS: &str = "throughput-current-mibps";
pub const KEY_PEAK_MIBPS: &str = "throughput-observed-peak-mibps";
pub const KEY_UTILIZATION: &str = "throughput-utilization-percent";
pub const KEY_RECOMMEND_MIBPS: &str = "throughput-recommended-mibps";
pub const KEY_CURRENT_IOPS: &str = "iops-current";
pub const KEY_RECOMMEND_IOPS: &str = "iops-recommended";
pub const KEY_RECOMMENDATION: &str = "throughput-recommendation";
pub const KEY_REASON: &str = "throughput-recommendation-reason";
pub const KEY_WINDOW: &str = "throughput-observation-window";
pub const KEY_SAMPLES: &str = "throughput-observation-samples";
pub const KEY_OBSERVED_AT: &str = "throughput-observed-at";

/// Every key except throughput-observed-at, in a fixed order.
const DATA_KEYS: [&str; 11] = [
    KEY_VOLUME_ID,
    KEY_CURRENT_MIBPS,
    KEY_PEAK_MIBPS,
    KEY_UTILIZATION,
    KEY_RECOMMEND_MIBPS,
    KEY_CURRENT_IOPS,
    KEY_RECOMMEND_IOPS,
    KEY_RECOMMENDATION,
    KEY_REASON,
    KEY_WINDOW,
    KEY_SAMPLES,
];

/// How stale throughput-observed-at may get before the annotations are
/// rewritten even though nothing changed.
const REFRESH_INTERVAL: Duration = Duration::from_hours(24);

/// Joins the prefix and a key suffix.
#[must_use]
pub fn key(suffix: &str) -> String {
    format!("{ANNOTATION_PREFIX}/{suffix}")
}

/// One Node's desired annotations: values to write and keys to remove.
/// Removal matters when a Node drops from a full recommendation to unknown:
/// leaving the last numbers behind would present a stale recommendation as a
/// current one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnnotationSet {
    pub set: BTreeMap<String, String>,
    pub remove: Vec<String>,
}

/// Renders one node's decision into annotation values. Numeric keys are only
/// written when the underlying value is actually known; every other key is
/// queued for removal so no stale value survives.
#[must_use]
pub fn build_annotations(obs: &Observation, d: &Decision, window: &str) -> AnnotationSet {
    let mut set = BTreeMap::from([
        (key(KEY_RECOMMENDATION), d.action.clone()),
        (key(KEY_REASON), d.reason.clone()),
        (key(KEY_WINDOW), window.to_string()),
    ]);
    if !obs.volume.id.is_empty() {
        set.insert(key(KEY_VOLUME_ID), obs.volume.id.clone());
        set.insert(
            key(KEY_CURRENT_MIBPS),
            obs.volume.throughput_mibps.to_string(),
        );
        set.insert(key(KEY_CURRENT_IOPS), obs.volume.iops.to_string());
    }
    if obs.has_metrics {
        set.insert(key(KEY_PEAK_MIBPS), format!("{:.1}", obs.input.peak_mibps));
        set.insert(key(KEY_SAMPLES), obs.input.samples.to_string());
    }
    // Utilization is derivable from the two keys above, but only outside
    // kubectl: custom-columns cannot divide. It needs a volume with a
    // provisioned throughput to divide by and a finite peak.
    if !obs.volume.id.is_empty()
        && obs.has_metrics
        && obs.volume.throughput_mibps > 0
        && obs.input.peak_mibps.is_finite()
    {
        let utilization = obs.input.peak_mibps / f64::from(obs.volume.throughput_mibps) * 100.0;
        set.insert(key(KEY_UTILIZATION), format!("{utilization:.1}"));
    }
    if [ACTION_INCREASE, ACTION_DECREASE, ACTION_NONE].contains(&d.action.as_str()) {
        set.insert(
            key(KEY_RECOMMEND_MIBPS),
            d.recommended_throughput_mibps.to_string(),
        );
        set.insert(key(KEY_RECOMMEND_IOPS), d.recommended_iops.to_string());
    }
    let remove = DATA_KEYS
        .iter()
        .map(|s| key(s))
        .filter(|k| !set.contains_key(k))
        .collect();
    AnnotationSet { set, remove }
}

impl AnnotationSet {
    /// Reports whether the Node's annotations have to be patched: any data
    /// value differs, a key queued for removal is still present, or
    /// throughput-observed-at has gone stale past the refresh interval.
    #[must_use]
    pub fn needs_write(&self, existing: &BTreeMap<String, String>, now: DateTime<Utc>) -> bool {
        if self.set.iter().any(|(k, v)| existing.get(k) != Some(v)) {
            return true;
        }
        if self.remove.iter().any(|k| existing.contains_key(k)) {
            return true;
        }
        observed_at_is_stale(existing.get(&key(KEY_OBSERVED_AT)), now, REFRESH_INTERVAL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::awsx::Volume;
    use crate::k8s::nodes::Node;
    use crate::throughput::decide::{ACTION_UNKNOWN, Input, REASON_NO_METRICS};

    fn obs(volume: bool, metrics: bool, peak: f64) -> Observation {
        Observation {
            node: Node::default(),
            volume: if volume {
                Volume {
                    id: "vol-1".into(),
                    throughput_mibps: 125,
                    iops: 3000,
                    ..Volume::default()
                }
            } else {
                Volume::default()
            },
            input: Input {
                peak_mibps: peak,
                samples: 4000,
                ..Input::default()
            },
            has_metrics: metrics,
            blocked: String::new(),
        }
    }

    #[test]
    fn full_recommendation_writes_every_key() {
        let d = Decision {
            action: ACTION_INCREASE.into(),
            reason: "observed_peak_above_provisioned".into(),
            recommended_throughput_mibps: 250,
            recommended_iops: 3000,
            capped: false,
        };
        let a = build_annotations(&obs(true, true, 100.25), &d, "7d/p99");
        assert_eq!(a.set[&key(KEY_RECOMMENDATION)], "increase");
        assert_eq!(a.set[&key(KEY_VOLUME_ID)], "vol-1");
        assert_eq!(a.set[&key(KEY_CURRENT_MIBPS)], "125");
        assert_eq!(a.set[&key(KEY_CURRENT_IOPS)], "3000");
        assert_eq!(a.set[&key(KEY_PEAK_MIBPS)], "100.2");
        assert_eq!(a.set[&key(KEY_SAMPLES)], "4000");
        assert_eq!(a.set[&key(KEY_UTILIZATION)], "80.2");
        assert_eq!(a.set[&key(KEY_RECOMMEND_MIBPS)], "250");
        assert_eq!(a.set[&key(KEY_RECOMMEND_IOPS)], "3000");
        assert_eq!(a.set[&key(KEY_WINDOW)], "7d/p99");
        assert!(a.remove.is_empty());
    }

    #[test]
    fn unknown_without_metrics_removes_numbers() {
        let d = Decision {
            action: ACTION_UNKNOWN.into(),
            reason: REASON_NO_METRICS.into(),
            ..Decision::default()
        };
        let a = build_annotations(&obs(true, false, 0.0), &d, "7d/p99");
        assert_eq!(a.set.len(), 6);
        assert!(a.remove.contains(&key(KEY_PEAK_MIBPS)));
        assert!(a.remove.contains(&key(KEY_UTILIZATION)));
        assert!(a.remove.contains(&key(KEY_RECOMMEND_MIBPS)));
        let a = build_annotations(&obs(false, false, 0.0), &d, "7d/p99");
        assert_eq!(a.set.len(), 3);
        assert_eq!(a.remove.len(), 8);
        // A NaN peak omits utilization but still writes the peak.
        let a = build_annotations(&obs(true, true, f64::NAN), &d, "7d/p99");
        assert!(!a.set.contains_key(&key(KEY_UTILIZATION)));
        assert_eq!(a.set[&key(KEY_PEAK_MIBPS)], "NaN");
    }

    #[test]
    fn needs_write_cases() {
        let now = Utc::now();
        let d = Decision {
            action: ACTION_NONE.into(),
            reason: "observed_peak_within_provisioned".into(),
            recommended_throughput_mibps: 125,
            recommended_iops: 3000,
            capped: false,
        };
        let a = build_annotations(&obs(true, true, 50.0), &d, "7d/p99");
        let mut existing = a.set.clone();
        existing.insert(
            key(KEY_OBSERVED_AT),
            (now - chrono::TimeDelta::hours(2)).to_rfc3339(),
        );
        assert!(!a.needs_write(&existing, now));
        existing.insert(
            key(KEY_OBSERVED_AT),
            (now - chrono::TimeDelta::hours(30)).to_rfc3339(),
        );
        assert!(a.needs_write(&existing, now));
        existing.insert(key(KEY_OBSERVED_AT), now.to_rfc3339());
        existing.insert(key(KEY_REASON), "other".into());
        assert!(a.needs_write(&existing, now));
        let d2 = Decision {
            action: ACTION_UNKNOWN.into(),
            reason: REASON_NO_METRICS.into(),
            ..Decision::default()
        };
        let b = build_annotations(&obs(true, false, 0.0), &d2, "7d/p99");
        let mut existing = b.set.clone();
        existing.insert(key(KEY_OBSERVED_AT), now.to_rfc3339());
        existing.insert(key(KEY_PEAK_MIBPS), "stale".into());
        assert!(b.needs_write(&existing, now), "stale number still present");
        assert!(b.needs_write(&b.set, now), "missing observed-at");
    }
}
