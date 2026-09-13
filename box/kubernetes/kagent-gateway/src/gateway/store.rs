//! In-memory state a gateway replica keeps between webhooks and mentions.

use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;

/// Remembers which alert groups were already analysed so a group that
/// Alertmanager resends every `repeat_interval` does not pay for a new agent
/// run each time. It is intentionally in-memory: an entry only has to outlive
/// the resend interval, and a restart losing the history costs one extra
/// analysis, not correctness.
#[derive(Debug)]
pub struct Store {
    seen: HashMap<String, Instant>,
    ttl: Duration,
}

impl Store {
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            seen: HashMap::new(),
            ttl,
        }
    }

    /// Reports whether `key` has not been seen within the TTL, and records it
    /// when it has not. A zero TTL disables deduplication entirely.
    pub fn allow(&mut self, key: &str, now: Instant) -> bool {
        if self.ttl.is_zero() {
            return true;
        }
        self.gc(now);
        if let Some(seen_at) = self.seen.get(key)
            && now.duration_since(*seen_at) < self.ttl
        {
            return false;
        }
        self.seen.insert(key.to_string(), now);
        true
    }

    /// Drops a key so a failed analysis can be retried on the next resend
    /// instead of staying suppressed for the whole TTL.
    pub fn forget(&mut self, key: &str) {
        self.seen.remove(key);
    }

    /// How many alert groups are currently suppressed, which is what the
    /// dedupe gauge publishes. Expired entries linger until the next `allow`
    /// sweeps them, so the count can read slightly high between webhooks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Removes expired entries. Sweeping the whole map on every insert is fine
    /// because the map only ever holds the alert groups seen within one TTL
    /// window.
    fn gc(&mut self, now: Instant) {
        let ttl = self.ttl;
        self.seen
            .retain(|_, seen_at| now.duration_since(*seen_at) < ttl);
    }
}

/// Maps a Slack thread to the A2A `contextId` its last turn returned, so a
/// follow-up mention continues the same agent session instead of starting from
/// nothing. It makes the same trade as [`Store`]: an entry that falls out
/// costs one cold turn, not correctness, and a restart drops the whole map.
#[derive(Debug)]
pub struct SessionStore {
    entries: HashMap<String, (String, Instant)>,
    ttl: Duration,
}

impl SessionStore {
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            ttl,
        }
    }

    /// The `contextId` a thread carries between turns, `None` when it has
    /// nothing or has been idle for longer than the TTL.
    pub fn get(&mut self, key: &str, now: Instant) -> Option<String> {
        if self.ttl.is_zero() {
            return None;
        }
        self.gc(now);
        self.entries.get(key).map(|(id, _)| id.clone())
    }

    /// Records what a turn produced and refreshes the idle timer, so a thread
    /// that keeps being used keeps its session.
    pub fn put(&mut self, key: &str, context_id: &str, now: Instant) {
        if self.ttl.is_zero() || context_id.is_empty() {
            return;
        }
        self.gc(now);
        self.entries
            .insert(key.to_string(), (context_id.to_string(), now));
    }

    /// How many threads currently hold a session, which is what the session
    /// gauge publishes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    fn gc(&mut self, now: Instant) {
        let ttl = self.ttl;
        self.entries
            .retain(|_, (_, used_at)| now.duration_since(*used_at) < ttl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_suppresses_within_ttl() {
        let mut s = Store::new(Duration::from_secs(60));
        let t0 = Instant::now();
        assert!(s.allow("a", t0));
        assert!(!s.allow("a", t0 + Duration::from_secs(30)));
        assert!(s.allow("b", t0));
        assert_eq!(s.len(), 2);
        assert!(s.allow("a", t0 + Duration::from_secs(60)));
        // b expired during the sweep above, a was refreshed.
        assert_eq!(s.len(), 1);
        s.forget("a");
        assert_eq!(s.len(), 0);
        assert!(s.allow("a", t0 + Duration::from_secs(61)));
    }

    #[test]
    fn zero_ttl_disables_dedupe() {
        let mut s = Store::new(Duration::ZERO);
        let now = Instant::now();
        assert!(s.allow("a", now));
        assert!(s.allow("a", now));
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn sessions_expire_when_idle() {
        let mut s = SessionStore::new(Duration::from_secs(100));
        let t0 = Instant::now();
        assert_eq!(s.get("t", t0), None);
        s.put("t", "", t0);
        assert_eq!(s.len(), 0);
        s.put("t", "ctx", t0);
        assert_eq!(s.get("t", t0 + Duration::from_secs(50)), Some("ctx".into()));
        assert_eq!(s.get("t", t0 + Duration::from_secs(100)), None);
        assert_eq!(s.len(), 0);

        let mut off = SessionStore::new(Duration::ZERO);
        off.put("t", "ctx", t0);
        assert_eq!(off.get("t", t0), None);
    }
}
