//! A small in-memory negative (404) cache.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use super::NowFn;

/// A small in-memory negative (404) cache keyed by repo-relative path. It
/// bounds upstream round-trips for missing artifacts. Entries are not shared
/// across replicas, which is acceptable: a stale negative entry only causes one
/// extra upstream fetch after failover.
pub(crate) struct NegCache {
    entries: Mutex<HashMap<String, DateTime<Utc>>>,
    now: NowFn,
}

impl NegCache {
    pub(crate) fn new() -> NegCache {
        NegCache {
            entries: Mutex::new(HashMap::new()),
            now: super::system_clock(),
        }
    }

    /// Replaces the clock. Tests inject a fixed one; the engine keeps the
    /// wall clock so a frozen engine clock cannot freeze cache expiry.
    #[cfg(test)]
    pub(crate) fn set_now(&mut self, now: NowFn) {
        self.now = now;
    }

    pub(crate) fn has(&self, key: &str) -> bool {
        let mut entries = self.entries.lock();
        let Some(exp) = entries.get(key).copied() else {
            return false;
        };
        if (self.now)() > exp {
            entries.remove(key);
            return false;
        }
        true
    }

    /// The time left before `key` expires, and whether `key` is still live.
    /// Used by the upstream cooldown to set a Retry-After hint for clients.
    pub(crate) fn remaining(&self, key: &str) -> Option<Duration> {
        let mut entries = self.entries.lock();
        let exp = entries.get(key).copied()?;
        let d = exp - (self.now)();
        if d <= chrono::TimeDelta::zero() {
            entries.remove(key);
            return None;
        }
        d.to_std().ok()
    }

    pub(crate) fn set(&self, key: &str, ttl: Duration) {
        if ttl.is_zero() {
            return;
        }
        let Ok(delta) = chrono::TimeDelta::from_std(ttl) else {
            return;
        };
        self.entries
            .lock()
            .insert(key.to_string(), (self.now)() + delta);
    }

    pub(crate) fn clear(&self, key: &str) {
        self.entries.lock().remove(key);
    }
}
