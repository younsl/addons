//! Registry of metadata references whose blob bytes are missing.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;

use super::{Manager, NowFn};

/// Bounds the registry. Dangling references are a fault condition, not a
/// workload, so a small cap is plenty; if something goes very wrong the oldest
/// entries are dropped rather than growing memory without limit.
pub(crate) const MAX_DANGLING_TRACKED: usize = 4096;

/// A metadata reference whose blob bytes were found missing from the blob store.
/// It is what the Artifacts view needs to warn a user that an artifact cannot be
/// served: which path is affected, which digest is gone, and when it was first
/// observed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DanglingRef {
    #[serde(skip)]
    pub repo_id: i64,
    pub path: String,
    pub sha256: String,
    pub role: String,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub hits: i64,
    /// Counts the HTTP status codes the failure produced, keyed by code. The
    /// same broken blob surfaces differently depending on what touched it: a
    /// fetch answers 500, a publish or a delete answers 503. Showing which one a
    /// user actually hit turns "this artifact is broken" into something that
    /// matches the error in their build log.
    pub statuses: HashMap<i64, i64>,
    /// The code the most recent failure returned. The histogram says what has
    /// failed over time; this says what just happened, which is what a user
    /// compares against the error still on their screen.
    pub last_status: i64,
}

/// The registry key: one entry per (repository, artifact path).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DanglingKey {
    pub(crate) repo_id: i64,
    pub(crate) path: String,
}

/// Remembers references observed to be missing their bytes.
///
/// It is populated by the serving and publish paths as they hit the failure, so
/// it costs nothing on the happy path and needs no periodic scan: the entries
/// are exactly the artifacts that have already failed a real request. An
/// integrity scan can fill it ahead of first failure by calling
/// [`DanglingRegistry::record`] for what it finds.
///
/// Entries live in memory on the leader only. That is deliberate: they describe
/// the blob store as observed through one metadata state, and a restart
/// re-derives them from real requests rather than carrying a stale claim
/// forward.
pub(crate) struct DanglingRegistry {
    pub(crate) entries: Mutex<HashMap<DanglingKey, DanglingRef>>,
    now: NowFn,
}

impl DanglingRegistry {
    pub(crate) fn new(now: NowFn) -> DanglingRegistry {
        DanglingRegistry {
            entries: Mutex::new(HashMap::new()),
            now,
        }
    }

    /// Notes a missing blob for (repo_id, path). Repeated observations keep the
    /// original `first_seen` so the UI can show how long the artifact has been
    /// broken.
    pub(crate) fn record(&self, repo_id: i64, path: &str, sha: &str, role: &str, status: i64) {
        if path.is_empty() || sha.is_empty() {
            return;
        }
        // UTC like every other stored timestamp, so a view can render the
        // recorded offset as-is instead of converting into the reader's zone.
        let now = (self.now)();
        let mut entries = self.entries.lock();
        let key = DanglingKey {
            repo_id,
            path: path.to_string(),
        };
        if let Some(existing) = entries.get_mut(&key) {
            existing.last_seen = now;
            existing.hits += 1;
            // A changed digest means the artifact was rewritten; restart the
            // clock and track the new digest rather than reporting the old one.
            if existing.sha256 != sha {
                existing.sha256 = sha.to_string();
                existing.first_seen = now;
                existing.hits = 1;
                existing.statuses = HashMap::new();
            }
            if status != 0 {
                *existing.statuses.entry(status).or_insert(0) += 1;
                existing.last_status = status;
            }
            return;
        }
        if entries.len() >= MAX_DANGLING_TRACKED {
            evict_oldest_locked(&mut entries);
        }
        let mut statuses = HashMap::new();
        if status != 0 {
            statuses.insert(status, 1);
        }
        entries.insert(
            key,
            DanglingRef {
                repo_id,
                path: path.to_string(),
                sha256: sha.to_string(),
                role: role.to_string(),
                first_seen: now,
                last_seen: now,
                hits: 1,
                statuses,
                last_status: status,
            },
        );
    }

    /// Removes an entry, used when the reference is observed to be healthy
    /// again (the artifact now points at a digest whose bytes exist).
    pub(crate) fn forget(&self, repo_id: i64, path: &str) {
        self.entries.lock().remove(&DanglingKey {
            repo_id,
            path: path.to_string(),
        });
    }

    /// The tracked references for one repository, keyed by artifact path, for
    /// annotating an artifact listing.
    pub(crate) fn by_repo(&self, repo_id: i64) -> HashMap<String, DanglingRef> {
        self.entries
            .lock()
            .iter()
            .filter(|(key, _)| key.repo_id == repo_id)
            .map(|(key, r)| (key.path.clone(), r.clone()))
            .collect()
    }

    /// Every tracked reference. Newest observation first is not guaranteed;
    /// callers that display them should sort.
    pub(crate) fn all(&self) -> Vec<DanglingRef> {
        self.entries.lock().values().cloned().collect()
    }
}

/// Drops the least recently observed entry. The caller holds the lock.
fn evict_oldest_locked(entries: &mut HashMap<DanglingKey, DanglingRef>) {
    let oldest = entries
        .iter()
        .min_by_key(|(_, r)| r.last_seen)
        .map(|(k, _)| k.clone());
    if let Some(key) = oldest {
        entries.remove(&key);
    }
}

impl Manager {
    /// Exposes the registry to the API layer, keyed by artifact path. A caller
    /// comparing each artifact's current digest against the returned `sha256`
    /// can tell a still-broken reference from a stale entry left behind by a
    /// republish.
    pub fn dangling_refs_for_repo(&self, repo_id: i64) -> HashMap<String, DanglingRef> {
        self.engine.dangling.by_repo(repo_id)
    }

    /// Every tracked reference.
    pub fn dangling_refs(&self) -> Vec<DanglingRef> {
        self.engine.dangling.all()
    }

    /// Registers a reference whose bytes are known to be missing, for callers
    /// that discover the condition outside the serving path (an integrity scan,
    /// which can flag artifacts before anyone requests them).
    pub fn record_dangling_ref(
        &self,
        repo_id: i64,
        path: &str,
        sha: &str,
        role: &str,
        status: i64,
    ) {
        self.engine
            .dangling
            .record(repo_id, path, sha, role, status);
    }

    /// Reports whether a tracked reference is still broken, forgetting it when
    /// the bytes are back.
    ///
    /// Comparing digests is not enough on its own: bytes can be restored in
    /// place at the same digest (an operator putting the object back), which
    /// leaves the digest unchanged and would otherwise keep the artifact flagged
    /// forever. This costs one blob store existence check, and only for
    /// artifacts already flagged, so a healthy listing does no extra work.
    pub async fn still_missing(&self, repo_id: i64, path: &str, sha: &str) -> bool {
        match self.engine.blobs.exists(sha).await {
            // Undecidable: keep the warning rather than silently clearing it.
            Err(_) => true,
            Ok(true) => {
                self.engine.dangling.forget(repo_id, path);
                false
            }
            Ok(false) => true,
        }
    }

    /// Reports whether the blob store definitively does not hold these bytes. It
    /// is the gate for destructive repair (forced artifact deletion), and so it
    /// is strict where [`Manager::still_missing`] is lenient: `still_missing`
    /// keeps a warning visible when the blob store cannot be reached, which is
    /// the safe answer for a warning but the dangerous one for a delete. A
    /// storage outage must not open a path to deleting healthy artifacts, so an
    /// error here answers "not confirmed" and the caller refuses.
    pub async fn confirmed_missing(&self, sha: &str) -> Result<bool, crate::storage::Error> {
        Ok(!self.engine.blobs.exists(sha).await?)
    }

    /// Drops a tracked reference. The API calls it once an artifact has been
    /// observed healthy again so a fixed artifact stops being flagged.
    pub fn forget_dangling_ref(&self, repo_id: i64, path: &str) {
        self.engine.dangling.forget(repo_id, path);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use chrono::{DateTime, TimeZone, Utc};
    use parking_lot::Mutex;

    use crate::repo::dangling::MAX_DANGLING_TRACKED;
    use crate::repo::{DanglingRegistry, itoa};

    fn at(y: i32, mo: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, 0, 0)
            .single()
            .expect("valid instant")
    }

    fn registry(start: DateTime<Utc>) -> (DanglingRegistry, Arc<Mutex<DateTime<Utc>>>) {
        let now = Arc::new(Mutex::new(start));
        let handle = Arc::clone(&now);
        (DanglingRegistry::new(Arc::new(move || *handle.lock())), now)
    }

    #[test]
    fn dangling_registry_records_and_keeps_first_seen() {
        let start = at(2026, 8, 7, 10);
        let (d, clock) = registry(start);
        d.record(7, "@msp/toolkit", "sha-a", "index", 500);

        *clock.lock() = start + chrono::TimeDelta::hours(1);
        d.record(7, "@msp/toolkit", "sha-a", "index", 500);

        let refs = d.by_repo(7);
        let r = refs.get("@msp/toolkit").expect("reference tracked");
        assert_eq!(r.hits, 2);
        assert_eq!(
            r.first_seen, start,
            "first seen must be the original observation"
        );
        assert_eq!(r.last_seen, *clock.lock());
        assert!(
            d.by_repo(8).is_empty(),
            "reference leaked into another repository"
        );
    }

    /// Covers a republish: the path is broken again but with different bytes, so
    /// the age of the previous failure must not be reported against the new digest.
    #[test]
    fn dangling_registry_restarts_on_new_digest() {
        let start = at(2026, 8, 7, 10);
        let (d, clock) = registry(start);
        d.record(7, "pkg", "sha-old", "index", 500);

        *clock.lock() = start + chrono::TimeDelta::hours(2);
        d.record(7, "pkg", "sha-new", "index", 500);

        let refs = d.by_repo(7);
        let r = refs.get("pkg").expect("reference tracked");
        assert_eq!(r.sha256, "sha-new");
        assert_eq!(
            r.first_seen,
            *clock.lock(),
            "clock not restarted for the new digest"
        );
        assert_eq!(r.hits, 1);
    }

    #[test]
    fn dangling_registry_forget() {
        let (d, _clock) = registry(at(2026, 8, 7, 10));
        d.record(7, "pkg", "sha", "index", 500);
        d.forget(7, "pkg");
        assert!(
            d.by_repo(7).is_empty(),
            "forget left the reference in place"
        );
    }

    /// The registry cannot grow without limit, and eviction drops the least
    /// recently observed entry.
    #[test]
    fn dangling_registry_bounded() {
        let start = at(2026, 8, 7, 10);
        let (d, clock) = registry(start);
        for i in 0..MAX_DANGLING_TRACKED {
            d.record(1, &format!("pkg-{}", itoa(i as i64)), "sha", "primary", 500);
            *clock.lock() += chrono::TimeDelta::seconds(1);
        }
        assert_eq!(d.entries.lock().len(), MAX_DANGLING_TRACKED);
        d.record(1, "pkg-overflow", "sha", "primary", 500);
        assert_eq!(
            d.entries.lock().len(),
            MAX_DANGLING_TRACKED,
            "entries after overflow"
        );
        let refs = d.by_repo(1);
        assert!(
            refs.contains_key("pkg-overflow"),
            "newest reference was not tracked"
        );
        assert!(
            !refs.contains_key("pkg-0"),
            "eviction did not drop the least recently observed reference"
        );
    }

    /// Keeps junk out of the view: a record with no path or digest cannot be
    /// matched against an artifact row.
    #[test]
    fn dangling_registry_ignores_incomplete_records() {
        let (d, _clock) = registry(at(2026, 8, 7, 10));
        d.record(7, "", "sha", "index", 500);
        d.record(7, "pkg", "", "index", 500);
        assert!(d.all().is_empty(), "tracked an incomplete record");
    }

    /// The observed response codes are counted per reference. The same broken blob
    /// answers 500 to a fetch and 503 to a publish, and telling them apart is what
    /// lets an operator match a row against the error someone actually reported.
    #[test]
    fn dangling_registry_tracks_status_codes() {
        let (d, _clock) = registry(at(2026, 8, 7, 10));
        d.record(7, "@msp/toolkit", "sha-a", "index", 500);
        d.record(7, "@msp/toolkit", "sha-a", "index", 500);
        d.record(7, "@msp/toolkit", "sha-a", "index", 503);

        let mut r = d
            .by_repo(7)
            .remove("@msp/toolkit")
            .expect("reference tracked");
        assert_eq!(r.statuses.get(&500).copied(), Some(2));
        assert_eq!(r.statuses.get(&503).copied(), Some(1));
        assert_eq!(r.hits, 3);

        // The returned map must be a copy: mutating it cannot corrupt the registry.
        r.statuses.insert(500, 99);
        let again = d
            .by_repo(7)
            .remove("@msp/toolkit")
            .expect("reference tracked");
        assert_eq!(
            again.statuses.get(&500).copied(),
            Some(2),
            "registry state mutated through the returned map"
        );

        // A rewritten artifact starts a fresh histogram.
        d.record(7, "@msp/toolkit", "sha-b", "index", 503);
        let after = d
            .by_repo(7)
            .remove("@msp/toolkit")
            .expect("reference tracked");
        assert_eq!(after.statuses.get(&500).copied(), None);
        assert_eq!(after.statuses.get(&503).copied(), Some(1));
    }

    mod dangling_registry {
        use std::sync::Arc;

        use chrono::{DateTime, TimeZone, Utc};
        use parking_lot::Mutex;

        use crate::repo::DanglingRegistry;
        use crate::repo::dangling::{DanglingKey, MAX_DANGLING_TRACKED};
        use crate::testing::repo::{body, new_test_manager};

        fn at(y: i32, mo: u32, d: u32, h: u32) -> DateTime<Utc> {
            Utc.with_ymd_and_hms(y, mo, d, h, 0, 0)
                .single()
                .expect("valid instant")
        }

        fn registry(start: DateTime<Utc>) -> (DanglingRegistry, Arc<Mutex<DateTime<Utc>>>) {
            let now = Arc::new(Mutex::new(start));
            let handle = Arc::clone(&now);
            (DanglingRegistry::new(Arc::new(move || *handle.lock())), now)
        }

        /// The registry is what the console's broken-artifact warning is built from, so
        /// its bookkeeping is the contract: repeated failures keep the first observation
        /// (that is the "broken since" a reader acts on) while counting hits and the
        /// status codes users actually saw, and a changed digest restarts the clock
        /// because it is a different artifact at the same path.
        #[test]
        fn dangling_registry_record_accumulates() {
            let start = at(2026, 8, 20, 10);
            let (reg, clock) = registry(start);

            // A record with no path or no digest names nothing actionable; it is dropped.
            reg.record(1, "", "sha-a", "primary", 500);
            reg.record(1, "a.jar", "", "primary", 500);
            assert!(reg.all().is_empty(), "incomplete records tracked");

            reg.record(1, "a.jar", "sha-a", "primary", 500);
            *clock.lock() += chrono::TimeDelta::minutes(1);
            reg.record(1, "a.jar", "sha-a", "primary", 503);
            *clock.lock() += chrono::TimeDelta::minutes(1);
            reg.record(1, "a.jar", "sha-a", "primary", 503);

            let mut r = reg.by_repo(1).remove("a.jar").expect("a.jar tracked");
            assert_eq!(r.hits, 3);
            assert_eq!(
                r.first_seen, start,
                "first seen must be the first observation"
            );
            assert_eq!(
                r.last_seen,
                *clock.lock(),
                "last seen must be the latest observation"
            );
            assert_eq!(r.statuses.get(&500).copied(), Some(1));
            assert_eq!(r.statuses.get(&503).copied(), Some(2));
            assert_eq!(r.last_status, 503);
            // The returned histogram is a copy: a caller must not be able to mutate
            // registry state through it.
            r.statuses.insert(500, 99);
            let again = reg.by_repo(1).remove("a.jar").expect("a.jar tracked");
            assert_eq!(
                again.statuses.get(&500).copied(),
                Some(1),
                "registry state mutated through a returned reference"
            );

            // A different digest at the same path is a republish: new digest, clock and
            // counters, and the old status histogram is gone rather than merged.
            *clock.lock() += chrono::TimeDelta::minutes(1);
            reg.record(1, "a.jar", "sha-b", "primary", 500);
            let r = reg.by_repo(1).remove("a.jar").expect("a.jar tracked");
            assert_eq!(r.sha256, "sha-b");
            assert_eq!(r.hits, 1);
            assert_eq!(r.first_seen, *clock.lock());
            assert_eq!(r.statuses.get(&503).copied(), None, "statuses carried over");
            assert_eq!(r.statuses.get(&500).copied(), Some(1));

            // Entries are per repository: by_repo must not leak another repository's rows.
            reg.record(2, "b.tgz", "sha-c", "index", 500);
            assert_eq!(reg.by_repo(1).len(), 1, "repo 1 refs must be only its own");
            assert_eq!(reg.all().len(), 2, "all refs must cover both repositories");
            reg.forget(1, "a.jar");
            assert!(
                !reg.by_repo(1).contains_key("a.jar"),
                "forgotten reference still tracked"
            );
        }

        /// The cap is a memory bound on a fault condition, and it has to drop the
        /// least-recently-observed entry: the newest failures are the ones an operator
        /// is looking at.
        #[test]
        fn dangling_registry_evicts_oldest() {
            let start = at(2026, 8, 20, 10);
            let (reg, clock) = registry(start);
            for i in 0..MAX_DANGLING_TRACKED {
                reg.record(1, &format!("p{i}"), "sha", "primary", 500);
                *clock.lock() += chrono::TimeDelta::seconds(1);
            }
            assert_eq!(
                reg.entries.lock().len(),
                MAX_DANGLING_TRACKED,
                "entries at the cap"
            );
            reg.record(1, "newest", "sha", "primary", 500);
            assert_eq!(
                reg.entries.lock().len(),
                MAX_DANGLING_TRACKED,
                "entries after eviction"
            );
            assert!(
                !reg.entries.lock().contains_key(&DanglingKey {
                    repo_id: 1,
                    path: "p0".into()
                }),
                "oldest entry survived the cap"
            );
            assert!(
                reg.entries.lock().contains_key(&DanglingKey {
                    repo_id: 1,
                    path: "newest".into()
                }),
                "newest entry was not recorded"
            );
        }

        /// `still_missing` and `confirmed_missing` deliberately disagree about an
        /// unreadable blob store, and the difference is a safety property: a warning is
        /// kept when the answer is unknown, while the destructive repair path refuses
        /// unless absence is proven. Restored bytes at the same digest clear the
        /// warning, which is the case a digest comparison alone cannot see.
        #[tokio::test]
        async fn still_missing_and_confirmed_missing() {
            let tm = new_test_manager().await;
            let (digest, _size) = tm
                .engine
                .blobs
                .put(body("present"))
                .await
                .expect("put blob");

            tm.manager
                .record_dangling_ref(1, "gone.jar", "sha-absent", "primary", 500);
            tm.manager
                .record_dangling_ref(1, "back.jar", &digest, "primary", 500);
            assert_eq!(tm.manager.dangling_refs().len(), 2, "tracked refs");

            assert!(
                tm.manager.still_missing(1, "gone.jar", "sha-absent").await,
                "absent bytes reported as present"
            );
            assert!(
                !tm.manager.still_missing(1, "back.jar", &digest).await,
                "restored bytes still reported as missing"
            );
            // Clearing is not just an answer: the entry is dropped, so the row stops
            // being flagged on the next listing.
            assert!(
                !tm.manager
                    .dangling_refs_for_repo(1)
                    .contains_key("back.jar"),
                "restored reference kept in the registry"
            );

            assert!(
                tm.manager
                    .confirmed_missing("sha-absent")
                    .await
                    .expect("confirmed_missing(absent)"),
                "absent bytes not confirmed missing"
            );
            assert!(
                !tm.manager
                    .confirmed_missing(&digest)
                    .await
                    .expect("confirmed_missing(present)"),
                "present bytes confirmed missing"
            );

            tm.manager.forget_dangling_ref(1, "gone.jar");
            assert!(tm.manager.dangling_refs().is_empty(), "refs after forget");
        }
    }
}
