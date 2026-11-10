//! The annotations written on each unused `PersistentVolumeClaim` and
//! `PersistentVolume`.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};

use super::types::Finding;
use crate::annotations;

/// Annotation key suffixes, joined to the shared prefix as
/// `<prefix>/<suffix>`. Keys are stable identifiers: renaming one orphans the
/// old key on every object already annotated.
pub const KEY_UNUSED: &str = "unused";
pub const KEY_UNUSED_SINCE: &str = "unused-since";
pub const KEY_UNUSED_REASON: &str = "unused-reason";
pub const KEY_UNUSED_DAYS: &str = "unused-days";
pub const KEY_VOLUME_ID: &str = "volume-id";
pub const KEY_OBSERVED_AT: &str = "unused-observed-at";
/// When orphan PVC cleanup will delete a scheduled claim. Shown to operators
/// only: the deadline that counts lives in the controller-owned state.
pub const KEY_ORPHAN_PVC_DELETE_AFTER: &str = "orphan-pvc-delete-after";
/// Set by an operator, never by the scanner: any value but `enabled` keeps a
/// claim out of orphan PVC cleanup.
pub const KEY_ORPHAN_PVC_CLEANUP: &str = "orphan-pvc-cleanup";

/// Every key except unused-observed-at, in a fixed order. It is excluded
/// because it changes on every pass and would make every comparison report a
/// difference, defeating the skip-unchanged check.
const DATA_KEYS: [&str; 6] = [
    KEY_UNUSED,
    KEY_UNUSED_SINCE,
    KEY_UNUSED_REASON,
    KEY_UNUSED_DAYS,
    KEY_VOLUME_ID,
    KEY_ORPHAN_PVC_DELETE_AFTER,
];

/// The keys 0.1.0 wrote under the legacy prefix.
pub const LEGACY_KEYS: [&str; 6] = [
    KEY_UNUSED,
    KEY_UNUSED_SINCE,
    KEY_UNUSED_REASON,
    KEY_UNUSED_DAYS,
    KEY_VOLUME_ID,
    KEY_OBSERVED_AT,
];

/// How stale unused-observed-at may get before the annotations are rewritten
/// even though nothing changed, so an operator can tell a current reading
/// from a stopped scanner.
const REFRESH_INTERVAL: Duration = Duration::from_hours(24);

/// Joins the shared prefix and a key suffix.
#[must_use]
pub fn key(suffix: &str) -> String {
    annotations::key(suffix)
}

/// One object's desired annotations: values to write and keys to remove.
/// Removal is what makes a claim that came back into use stop reading as
/// unused, which matters more here than for an advisory value: a stale
/// "unused" mark is an invitation to delete live data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnnotationSet {
    pub set: BTreeMap<String, String>,
    pub remove: Vec<String>,
}

/// Renders one finding into annotation values. An object in use sets nothing
/// and queues every key for removal.
#[must_use]
pub fn build_annotations(f: &Finding) -> AnnotationSet {
    let mut set = BTreeMap::new();
    if f.unused {
        set.insert(key(KEY_UNUSED), "true".to_string());
        set.insert(
            key(KEY_UNUSED_SINCE),
            f.unused_since
                .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true))
                .unwrap_or_default(),
        );
        set.insert(key(KEY_UNUSED_REASON), f.reason.clone());
        set.insert(key(KEY_UNUSED_DAYS), f.unused_days().to_string());
        if !f.volume_id.is_empty() {
            set.insert(key(KEY_VOLUME_ID), f.volume_id.clone());
        }
        if let Some(t) = f.delete_after {
            set.insert(
                key(KEY_ORPHAN_PVC_DELETE_AFTER),
                t.to_rfc3339_opts(SecondsFormat::Secs, true),
            );
        }
    }
    let mut remove: Vec<String> = DATA_KEYS
        .iter()
        .map(|s| key(s))
        .filter(|k| !set.contains_key(k))
        .collect();
    // unused-observed-at is not in DATA_KEYS, since it changes every pass. It
    // still has to be cleared alongside them when the object comes back into
    // use, or the object would keep a timestamp with nothing left to
    // timestamp.
    if !f.unused {
        remove.push(key(KEY_OBSERVED_AT));
    }
    remove.extend(annotations::legacy_present(&f.annotations, &LEGACY_KEYS));
    AnnotationSet { set, remove }
}

impl AnnotationSet {
    /// Reports whether the object has to be patched: any value differs, a key
    /// queued for removal is still present, or unused-observed-at has gone
    /// stale past the refresh interval. An in-use object writes nothing, so
    /// it is patched only to clear a mark left by an earlier pass.
    #[must_use]
    pub fn needs_write(&self, existing: &BTreeMap<String, String>, now: DateTime<Utc>) -> bool {
        let still_present = self.remove.iter().any(|k| existing.contains_key(k));
        if self.set.is_empty() {
            return still_present;
        }
        if self.set.iter().any(|(k, v)| existing.get(k) != Some(v)) {
            return true;
        }
        if still_present {
            return true;
        }
        observed_at_is_stale(existing.get(&key(KEY_OBSERVED_AT)), now, REFRESH_INTERVAL)
    }
}

/// Reports whether a stored RFC 3339 observed-at timestamp is missing,
/// unparseable, or older than `refresh`.
pub(crate) fn observed_at_is_stale(
    raw: Option<&String>,
    now: DateTime<Utc>,
    refresh: Duration,
) -> bool {
    let Some(last) = raw.and_then(|s| DateTime::parse_from_rfc3339(s).ok()) else {
        return true;
    };
    let age = now - last.with_timezone(&Utc);
    age >= chrono::TimeDelta::from_std(refresh).unwrap_or(chrono::TimeDelta::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unused_finding(now: DateTime<Utc>) -> Finding {
        Finding {
            unused: true,
            reason: "no_consumer_pod".into(),
            unused_since: Some(now - chrono::TimeDelta::days(3)),
            age: Duration::from_hours(72),
            volume_id: "vol-1".into(),
            ..Finding::default()
        }
    }

    #[test]
    fn build_for_unused_and_in_use() {
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let a = build_annotations(&unused_finding(now));
        assert_eq!(a.set[&key(KEY_UNUSED)], "true");
        assert_eq!(
            a.set[&key(KEY_UNUSED_SINCE)],
            (now - chrono::TimeDelta::days(3)).to_rfc3339_opts(SecondsFormat::Secs, true)
        );
        assert_eq!(a.set[&key(KEY_UNUSED_REASON)], "no_consumer_pod");
        assert_eq!(a.set[&key(KEY_UNUSED_DAYS)], "3");
        assert_eq!(a.set[&key(KEY_VOLUME_ID)], "vol-1");
        assert_eq!(a.remove, vec![key(KEY_ORPHAN_PVC_DELETE_AFTER)]);

        let mut f = unused_finding(now);
        f.volume_id = String::new();
        let a = build_annotations(&f);
        assert!(!a.set.contains_key(&key(KEY_VOLUME_ID)));
        assert_eq!(
            a.remove,
            vec![key(KEY_VOLUME_ID), key(KEY_ORPHAN_PVC_DELETE_AFTER)]
        );

        let mut f = unused_finding(now);
        f.delete_after = Some(now);
        let a = build_annotations(&f);
        assert_eq!(
            a.set[&key(KEY_ORPHAN_PVC_DELETE_AFTER)],
            now.to_rfc3339_opts(SecondsFormat::Secs, true)
        );

        let a = build_annotations(&Finding::default());
        assert!(a.set.is_empty());
        assert_eq!(a.remove.len(), 7, "every data key plus observed-at");
        assert!(a.remove.contains(&key(KEY_OBSERVED_AT)));
    }

    #[test]
    fn legacy_keys_are_removed() {
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let mut f = unused_finding(now);
        f.annotations.insert(
            annotations::legacy_key(KEY_UNUSED_SINCE),
            "2023-01-01T00:00:00Z".into(),
        );
        let a = build_annotations(&f);
        assert!(
            a.remove
                .contains(&annotations::legacy_key(KEY_UNUSED_SINCE))
        );
        assert!(a.needs_write(&f.annotations, now));
    }

    #[test]
    fn needs_write_cases() {
        let now = Utc::now();
        let desired = build_annotations(&unused_finding(now));
        let mut existing = desired.set.clone();
        existing.insert(
            key(KEY_OBSERVED_AT),
            (now - chrono::TimeDelta::hours(1)).to_rfc3339(),
        );
        assert!(!desired.needs_write(&existing, now), "unchanged and fresh");

        existing.insert(
            key(KEY_OBSERVED_AT),
            (now - chrono::TimeDelta::hours(25)).to_rfc3339(),
        );
        assert!(
            desired.needs_write(&existing, now),
            "stale observed-at refreshes"
        );

        existing.insert(key(KEY_OBSERVED_AT), "garbage".into());
        assert!(
            desired.needs_write(&existing, now),
            "unparseable observed-at"
        );

        let mut changed = desired.set.clone();
        changed.insert(key(KEY_OBSERVED_AT), now.to_rfc3339());
        changed.insert(key(KEY_UNUSED_REASON), "unbound".into());
        assert!(desired.needs_write(&changed, now), "changed reason");

        let mut f = unused_finding(now);
        f.volume_id = String::new();
        let desired = build_annotations(&f);
        let mut existing = desired.set.clone();
        existing.insert(key(KEY_OBSERVED_AT), now.to_rfc3339());
        existing.insert(key(KEY_VOLUME_ID), "vol-old".into());
        assert!(
            desired.needs_write(&existing, now),
            "key queued for removal is still present"
        );

        let in_use = build_annotations(&Finding::default());
        assert!(
            !in_use.needs_write(&BTreeMap::new(), now),
            "unmarked in-use object is skipped"
        );
        assert!(
            in_use.needs_write(
                &BTreeMap::from([(key(KEY_UNUSED), "true".to_string())]),
                now
            ),
            "stale mark is cleared"
        );
    }
}
