//! Decides whether a volume modification should carry the throughput
//! recommender's latest increase recommendation along with the size change.
//! EC2 allows one modification per volume per 6 hours, so a size expansion
//! is the only free ride a throughput change ever gets; a volume whose disk
//! never fills simply keeps its recommendation as an annotation.

use std::time::Duration;

use super::Resizer;
use crate::recstore::{ACTION_INCREASE, Entry};

/// Scales the recommender's interval into the maximum age a recommendation
/// may have and still be applied. Two intervals tolerate one missed pass;
/// anything older means the recommender has stopped and its last output no
/// longer describes the present.
const STALE_FACTOR: u32 = 2;

/// Outcomes of one piggyback decision. Attempts and skips are separate
/// metrics, mirroring the `resize_total` / `skip_total` split: an attempt
/// sent a combined request to EC2, a skip never did.
pub const APPLY_RESULT_APPLIED: &str = "applied";
pub const APPLY_RESULT_FALLBACK: &str = "fallback_size_only";
/// The recommender has never evaluated this volume.
pub const APPLY_SKIP_NO_RECOMMENDATION: &str = "no_recommendation";
/// A recommendation exists but is older than the freshness bound. The one
/// skip worth alerting on.
pub const APPLY_SKIP_STALE: &str = "stale";
/// A fresh recommendation exists and asks for no raise. The healthy steady
/// state.
pub const APPLY_SKIP_NOT_INCREASE: &str = "not_increase";

/// The freshness bound derived from the recommender's interval. Public so
/// the startup log can report the same number the apply gate enforces.
#[must_use]
pub fn recommendation_max_age(interval: Duration) -> Duration {
    interval * STALE_FACTOR
}

impl Resizer {
    /// Returns the recommendation to fold into a volume modification, plus
    /// the skip reason when there is none (empty when the feature is off
    /// entirely, which is a configuration state rather than a per-volume
    /// outcome). It only ever returns an increase: applying a decrease as a
    /// side effect of a size expansion would cut bandwidth at the exact
    /// moment the instance is busy enough to be filling its disk.
    pub(super) fn throughput_piggyback(&self, volume_id: &str) -> (Option<Entry>, &'static str) {
        let tr = &self.cfg.throughput_recommendation;
        let Some(recs) = &self.recs else {
            return (None, "");
        };
        if !tr.apply_on_resize {
            return (None, "");
        }
        let Some(rec) = recs.lookup(volume_id, recommendation_max_age(tr.interval)) else {
            // node_ref ignores entry age, so it distinguishes "the recommender
            // has never seen this volume" from "it has, but its output has
            // gone stale".
            return if recs.node_ref(volume_id).is_some() {
                (None, APPLY_SKIP_STALE)
            } else {
                (None, APPLY_SKIP_NO_RECOMMENDATION)
            };
        };
        // The direction is re-checked against the provisioning observed with
        // the recommendation, so a malformed entry can never lower a volume's
        // throughput even if a bug upstream mislabels its action.
        if rec.action != ACTION_INCREASE || rec.throughput_mibps <= rec.current_mibps {
            return (None, APPLY_SKIP_NOT_INCREASE);
        }
        (Some(rec), "")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_age_is_two_intervals() {
        assert_eq!(
            recommendation_max_age(Duration::from_mins(30)),
            Duration::from_hours(1)
        );
    }
}
