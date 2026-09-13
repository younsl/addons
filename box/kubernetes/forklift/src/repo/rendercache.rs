//! In-process cache of rendered metadata documents.

use std::collections::HashMap;
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use super::NowFn;

/// Bounds how long a rendered document may be reused when nothing about it
/// depends on the clock. Every other input to a render is part of the cache key
/// (stored bytes, host, serving repo, config revision), so the only reason to
/// re-render an unchanged document is the age policy, and
/// [`RenderCache::put`] takes the exact instant that policy would next change
/// the output. This bound is a backstop for that calculation, matched to the
/// default metadata TTL after which the stored bytes (and so the key) turn over
/// anyway.
pub(crate) const RENDER_CACHE_TTL: Duration = Duration::from_secs(15 * 60);

/// Caps the resident rendered documents. Sized to stay well inside the container
/// memory limit alongside the request path itself; the cache is an
/// optimization, so evicting early only costs a re-render.
pub(crate) const RENDER_CACHE_BYTES: i64 = 64 << 20;

/// Holds rendered metadata documents so that repeated requests for the same
/// package skip the decode and rewrite entirely.
///
/// It is deliberately in-process rather than blob-backed like the group
/// aggregate cache (`meta::GroupMetadataCache`): traffic reaches a single pod
/// (the Service selects the leader), and a blob-backed cache would add an object
/// store write plus a serialized metadata transaction to every first render,
/// which is exactly the cold-install burst this is meant to make cheaper.
///
/// Eviction is oldest-first rather than least-recently-used. Entries live for
/// seconds and an install burst touches each key once or twice, so recency
/// carries almost no information here and FIFO keeps the hot path to a map
/// write.
pub(crate) struct RenderCache {
    state: Mutex<State>,
    limit: i64,
    ttl: Duration,
    now: NowFn,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, RenderEntry>,
    order: Vec<String>,
    bytes: i64,
}

#[derive(Clone)]
struct RenderEntry {
    body: Bytes,
    removed: i64,
    expires: DateTime<Utc>,
}

impl RenderCache {
    pub(crate) fn new(limit: i64, ttl: Duration) -> RenderCache {
        RenderCache {
            state: Mutex::new(State::default()),
            limit,
            ttl,
            now: super::system_clock(),
        }
    }

    /// Replaces the clock (tests only).
    #[cfg(test)]
    pub(crate) fn set_now(&mut self, now: NowFn) {
        self.now = now;
    }

    /// Returns the cached body and the version count the age policy removed
    /// when it was rendered, so the caller can report the same metric it would
    /// have.
    pub(crate) fn get(&self, key: &str) -> Option<(Bytes, i64)> {
        let mut state = self.state.lock();
        let entry = state.entries.get(key)?.clone();
        if entry.expires <= (self.now)() {
            drop_locked(&mut state, key);
            return None;
        }
        Some((entry.body, entry.removed))
    }

    /// Stores `body` under `key`. `until`, when set, is the instant at which the
    /// rendered output would change even though its inputs did not (a version
    /// leaving the age-policy cooldown); the entry expires then or at the TTL,
    /// whichever comes first.
    pub(crate) fn put(&self, key: &str, body: Bytes, removed: i64, until: Option<DateTime<Utc>>) {
        let size = body.len() as i64;
        if size == 0 || size > self.limit {
            return;
        }
        let mut state = self.state.lock();
        if state.entries.contains_key(key) {
            drop_locked(&mut state, key);
        }
        while state.bytes + size > self.limit && !state.order.is_empty() {
            let oldest = state.order[0].clone();
            drop_locked(&mut state, &oldest);
        }
        let mut expires =
            (self.now)() + chrono::TimeDelta::from_std(self.ttl).unwrap_or(chrono::TimeDelta::MAX);
        if let Some(until) = until
            && until < expires
        {
            expires = until;
        }
        state.entries.insert(
            key.to_string(),
            RenderEntry {
                body,
                removed,
                expires,
            },
        );
        state.order.push(key.to_string());
        state.bytes += size;
    }

    /// The resident entry count and bytes, for the scrape-time gauges.
    #[cfg(test)]
    pub(crate) fn stats(&self) -> (usize, i64) {
        let state = self.state.lock();
        (state.entries.len(), state.bytes)
    }
}

/// Removes `key` and its bytes. The caller holds the state lock.
fn drop_locked(state: &mut State, key: &str) {
    let Some(entry) = state.entries.remove(key) else {
        return;
    };
    state.bytes -= entry.body.len() as i64;
    if let Some(i) = state.order.iter().position(|k| k == key) {
        state.order.remove(i);
    }
}

/// Identifies one rendered representation. Every input the rewrite depends on is
/// part of the key: the stored bytes (digest), where the URLs must point (base,
/// serving repo) and the policy that shaped the output (config revision).
/// Anything time-dependent is covered by the TTL instead.
pub(crate) fn render_cache_key(
    repo: &str,
    path: &str,
    digest: &str,
    base: &str,
    serving_repo: &str,
    config_revision: &str,
) -> String {
    format!("{repo}\0{path}\0{digest}\0{base}\0{serving_repo}\0{config_revision}")
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use chrono::{DateTime, TimeZone, Utc};
    use parking_lot::Mutex;

    use crate::repo::{MAX_CONCURRENT_REWRITES, RenderCache};
    use crate::testing::repo::{call, mk_format_repo, mux, new_test_manager, send, spawn_upstream};

    fn fixed_cache(
        limit: i64,
        ttl: Duration,
        start: DateTime<Utc>,
    ) -> (RenderCache, Arc<Mutex<DateTime<Utc>>>) {
        let mut c = RenderCache::new(limit, ttl);
        let now = Arc::new(Mutex::new(start));
        let handle = Arc::clone(&now);
        c.set_now(Arc::new(move || *handle.lock()));
        (c, now)
    }

    fn unix(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().expect("valid instant")
    }

    #[test]
    fn render_cache_expires_and_evicts() {
        let (c, now) = fixed_cache(100, Duration::from_secs(10), unix(1000));

        c.put("a", Bytes::from_static(b"0123456789"), 3, None);
        let (body, removed) = c.get("a").expect("entry present");
        assert_eq!(&body[..], b"0123456789");
        assert_eq!(removed, 3);

        *now.lock() = unix(1011);
        assert!(c.get("a").is_none(), "expired entry served");
        assert_eq!(c.stats(), (0, 0), "expiry left entries resident");
    }

    #[test]
    fn render_cache_honours_the_byte_cap() {
        let (c, _now) = fixed_cache(30, Duration::from_secs(60), unix(1000));
        for k in ["a", "b", "c"] {
            c.put(k, Bytes::from("x".repeat(10)), 0, None);
        }
        assert_eq!(c.stats(), (3, 30), "resident after three puts");
        // The cap is reached, so the oldest entry funds the newest.
        c.put("d", Bytes::from("x".repeat(10)), 0, None);
        assert!(c.get("a").is_none(), "oldest entry survived eviction");
        assert!(c.get("d").is_some(), "newest entry was not stored");
        assert_eq!(c.stats(), (3, 30), "resident after eviction");
        // A document larger than the whole cache is not worth evicting everything for.
        c.put("big", Bytes::from("x".repeat(31)), 0, None);
        assert!(c.get("big").is_none(), "oversized document was cached");
        assert_eq!(c.stats().0, 3, "oversized put disturbed the cache");
    }

    #[test]
    fn render_cache_replaces_an_entry_without_leaking_bytes() {
        let (c, _now) = fixed_cache(100, Duration::from_secs(60), unix(1000));
        c.put("a", Bytes::from_static(b"1234567890"), 0, None);
        c.put("a", Bytes::from_static(b"12345"), 7, None);
        let (body, removed) = c.get("a").expect("entry present");
        assert_eq!(&body[..], b"12345");
        assert_eq!(removed, 7);
        assert_eq!(c.stats(), (1, 5));
    }

    /// An entry whose rendered output changes before the TTL (a version leaving the
    /// age-policy cooldown) expires at that instant, while an absent `until` leaves
    /// the TTL in charge.
    #[test]
    fn render_cache_put_honours_earlier_until() {
        let start = unix(1000);
        let (c, now) = fixed_cache(100, Duration::from_secs(600), start);

        c.put(
            "cooldown",
            Bytes::from_static(b"a"),
            1,
            Some(start + chrono::TimeDelta::seconds(30)),
        );
        c.put("plain", Bytes::from_static(b"b"), 0, None);
        c.put(
            "late",
            Bytes::from_static(b"c"),
            0,
            Some(start + chrono::TimeDelta::hours(1)),
        );

        *now.lock() = start + chrono::TimeDelta::seconds(31);
        assert!(
            c.get("cooldown").is_none(),
            "entry served past the cooldown boundary it was rendered against"
        );
        assert!(
            c.get("plain").is_some(),
            "clock-independent entry expired before its TTL"
        );
        *now.lock() = start + chrono::TimeDelta::seconds(31) + chrono::TimeDelta::minutes(10);
        assert!(
            c.get("late").is_none(),
            "TTL did not cap an until beyond it"
        );
    }

    /// A cached render is served even when every rewrite slot is held, because the
    /// rendered document needs no slot at all.
    #[tokio::test]
    async fn npm_packument_render_cache_serves_with_every_slot_held() {
        use std::sync::atomic::{AtomicI64, Ordering};

        use axum::Router;
        use axum::routing::any;
        use http::{Method, StatusCode};

        let upstream_hits = Arc::new(AtomicI64::new(0));
        let hits = Arc::clone(&upstream_hits);
        let upstream = spawn_upstream(Router::new().fallback(any(move || {
        let hits = Arc::clone(&hits);
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            r#"{"name":"pkg","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"dist":{"tarball":"http://up/pkg/-/pkg-1.0.0.tgz"}}},"time":{"1.0.0":"2020-01-01T00:00:00Z"}}"#
        }
    })))
    .await;

        let tm = new_test_manager().await;
        mk_format_repo(
            &tm.store,
            "npmjs",
            crate::meta::FORMAT_NPM,
            crate::meta::TYPE_PROXY,
            &upstream,
            crate::repoconfig::default(),
        )
        .await;
        let app = mux(&tm.manager);

        let resp = call(&app, Method::GET, "/npm/npmjs/pkg", "").await;
        assert_eq!(resp.status, StatusCode::OK, "warm request");
        let warm = resp.text();

        let mut slots = Vec::with_capacity(MAX_CONCURRENT_REWRITES);
        for _ in 0..MAX_CONCURRENT_REWRITES {
            slots.push(
                tm.engine
                    .acquire_rewrite("npmjs")
                    .await
                    .expect("could not fill the gate"),
            );
        }

        let cached = tokio::time::timeout(
            Duration::from_secs(3),
            call(&app, Method::GET, "/npm/npmjs/pkg", ""),
        )
        .await
        .expect("request queued for a rewrite slot instead of reusing the rendered document");
        assert_eq!(
            cached.status,
            StatusCode::OK,
            "cached render with the gate full"
        );
        assert_eq!(
            cached.text(),
            warm,
            "cached render differs from the first response"
        );

        assert_eq!(upstream_hits.load(Ordering::SeqCst), 1, "upstream fetches");
        assert_eq!(
            tm.engine.render_cache_ops.with_label_values(&["hit"]).get(),
            1.0,
            "render cache hits"
        );
        drop(slots);
    }

    /// Different clients reach forklift under different external base URLs, and the
    /// rewritten tarball URLs must follow the request rather than whatever the first
    /// caller happened to use.
    #[tokio::test]
    async fn npm_packument_render_cache_keyed_by_external_base() {
        use axum::Router;
        use axum::body::Body;
        use axum::routing::any;
        use http::{Method, Request, StatusCode};

        let upstream = spawn_upstream(Router::new().fallback(any(|| async {
        r#"{"name":"pkg","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"dist":{"tarball":"http://up/pkg/-/pkg-1.0.0.tgz"}}}}"#
    })))
    .await;

        let tm = new_test_manager().await;
        mk_format_repo(
            &tm.store,
            "npmjs",
            crate::meta::FORMAT_NPM,
            crate::meta::TYPE_PROXY,
            &upstream,
            crate::repoconfig::default(),
        )
        .await;
        let app = mux(&tm.manager);

        let body_for = async |host: &str| {
            let request = Request::builder()
                .method(Method::GET)
                .uri("/npm/npmjs/pkg")
                .header(http::header::HOST, host)
                .body(Body::empty())
                .expect("build request");
            let resp = send(&app, request).await;
            assert_eq!(resp.status, StatusCode::OK, "{host}: request");
            resp.text()
        };
        let first = body_for("a.example.com").await;
        let second = body_for("b.example.com").await;
        assert!(
            first.contains("a.example.com"),
            "first response does not point at its own host: {first}"
        );
        assert!(
            second.contains("b.example.com"),
            "second host served the first host's rendered document: {second}"
        );
    }
}
