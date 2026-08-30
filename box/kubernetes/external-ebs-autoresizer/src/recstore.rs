//! The in-memory hand-off point between the throughput recommender and the
//! resizer. The recommender publishes its latest decision per volume; the
//! resizer looks a volume up right before modifying it, so an increase
//! recommendation can ride along on the same `ModifyVolume` call (and the same
//! once-per-6h modification slot) as a size expansion.
//!
//! The store is deliberately process-local. Both loops run in the same binary
//! under the same leader, so shared memory is the whole persistence this needs:
//! a restart empties the store and the next recommender pass refills it, and
//! until then the resizer simply falls back to size-only modifications.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};

/// Mirrors `throughput::ACTION_INCREASE` without importing the module: it is
/// the only action a consumer of this store acts on, and this module must stay
/// a leaf both subsystems can use.
pub const ACTION_INCREASE: &str = "increase";

/// One volume's most recent throughput recommendation, as published by the
/// recommender.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    /// The Kubernetes Node the volume was attached to when the recommendation
    /// was made, so the consumer can publish Events against the Node.
    pub node_name: String,
    pub node_uid: String,
    /// The recommendation action (increase, decrease, none, unknown). The
    /// resizer only ever consumes increase; the rest are stored so a stale
    /// increase is overwritten rather than lingering after demand drops.
    pub action: String,
    /// The recommended values. IOPS already carries the gp3 throughput-to-IOPS
    /// ratio bump computed by the recommender.
    pub throughput_mibps: i32,
    pub iops: i32,
    /// The provisioned values at observation time, kept so the consumer can
    /// re-check the direction of the change and describe it in reporting.
    pub current_mibps: i32,
    pub current_iops: i32,
    /// When the recommendation was computed. `lookup` uses it to refuse
    /// entries older than the caller's freshness bound.
    pub observed_at: Option<DateTime<Utc>>,
}

/// Holds the latest [`Entry`] per volume ID.
pub struct Store {
    entries: Mutex<HashMap<String, Entry>>,
    /// Injectable so tests control the staleness clock.
    now: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    /// Returns an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(Utc::now)
    }

    /// Returns an empty store whose staleness clock is `now`.
    pub fn with_clock(now: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            now: Box::new(now),
        }
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records the latest recommendation for a volume, replacing any previous
    /// one.
    pub fn publish(&self, volume_id: &str, e: Entry) {
        if volume_id.is_empty() {
            return;
        }
        self.entries().insert(volume_id.to_string(), e);
    }

    /// Removes a volume's entry. The recommender calls it when a volume can no
    /// longer be evaluated (detached, metrics gone), so a recommendation from a
    /// past life never outlives the conditions that produced it.
    pub fn delete(&self, volume_id: &str) {
        self.entries().remove(volume_id);
    }

    /// Drops every entry whose volume ID is not in `keep`. The recommender calls
    /// it at the end of each successful pass with the volumes it saw, which is
    /// what keeps the store from growing without bound under node churn.
    pub fn retain(&self, keep: &HashSet<String>) {
        self.entries().retain(|id, _| keep.contains(id));
    }

    /// Returns the entry for a volume when one exists and its `observed_at` is
    /// within `max_age` of now. A stale entry is reported as absent rather than
    /// returned with a flag: a stale recommendation must never be applied.
    #[must_use]
    pub fn lookup(&self, volume_id: &str, max_age: Duration) -> Option<Entry> {
        let entries = self.entries();
        let e = entries.get(volume_id)?;
        let observed = e.observed_at?;
        let age = (self.now)() - observed;
        let max = chrono::TimeDelta::from_std(max_age).unwrap_or(chrono::TimeDelta::MAX);
        if age > max {
            return None;
        }
        Some(e.clone())
    }

    /// Returns the Kubernetes Node `(name, uid)` a volume was last seen attached
    /// to. Unlike `lookup` it ignores the entry's age: node identity is used to
    /// address Events, not to apply a recommendation.
    #[must_use]
    pub fn node_ref(&self, volume_id: &str) -> Option<(String, String)> {
        let entries = self.entries();
        let e = entries.get(volume_id)?;
        if e.node_name.is_empty() {
            return None;
        }
        Some((e.node_name.clone(), e.node_uid.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(node: &str, observed: DateTime<Utc>) -> Entry {
        Entry {
            node_name: node.into(),
            node_uid: "uid".into(),
            action: ACTION_INCREASE.into(),
            throughput_mibps: 250,
            iops: 3000,
            current_mibps: 125,
            current_iops: 3000,
            observed_at: Some(observed),
        }
    }

    #[test]
    fn publish_overwrites_and_ignores_empty_id() {
        let now = Utc::now();
        let s = Store::with_clock(move || now);
        s.publish("", entry("n", now));
        assert!(s.lookup("", Duration::from_mins(1)).is_none());
        s.publish("vol-1", entry("a", now));
        s.publish("vol-1", entry("b", now));
        assert_eq!(
            s.lookup("vol-1", Duration::from_mins(1)).unwrap().node_name,
            "b"
        );
    }

    #[test]
    fn lookup_fresh_and_stale() {
        let now = Utc::now();
        let s = Store::with_clock(move || now);
        s.publish("fresh", entry("n", now - chrono::TimeDelta::minutes(30)));
        s.publish("stale", entry("n", now - chrono::TimeDelta::minutes(90)));
        s.publish("boundary", entry("n", now - chrono::TimeDelta::hours(1)));
        let hour = Duration::from_hours(1);
        assert!(s.lookup("fresh", hour).is_some());
        assert!(s.lookup("stale", hour).is_none());
        assert!(
            s.lookup("boundary", hour).is_some(),
            "exact boundary is fresh"
        );
        assert!(s.lookup("missing", hour).is_none());
        let mut no_time = entry("n", now);
        no_time.observed_at = None;
        s.publish("no-time", no_time);
        assert!(s.lookup("no-time", hour).is_none());
    }

    #[test]
    fn node_ref_ignores_age_and_absent_cases() {
        let now = Utc::now();
        let s = Store::with_clock(move || now);
        s.publish("stale", entry("node-a", now - chrono::TimeDelta::days(5)));
        assert_eq!(s.node_ref("stale"), Some(("node-a".into(), "uid".into())));
        assert!(s.node_ref("missing").is_none());
        s.publish("no-node", entry("", now));
        assert!(s.node_ref("no-node").is_none());
    }

    #[test]
    fn delete_and_retain() {
        let now = Utc::now();
        let s = Store::with_clock(move || now);
        for id in ["a", "b", "c"] {
            s.publish(id, entry("n", now));
        }
        s.delete("a");
        assert!(s.lookup("a", Duration::from_secs(1)).is_none());
        s.retain(&HashSet::from(["b".to_string()]));
        assert!(s.lookup("b", Duration::from_secs(1)).is_some());
        assert!(s.lookup("c", Duration::from_secs(1)).is_none());
        s.retain(&HashSet::new());
        assert!(s.lookup("b", Duration::from_secs(1)).is_none());
    }
}
