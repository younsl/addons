//! Single-flight coalescing for identical proxy fetches.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use prometheus::core::{Collector, Desc};
use prometheus::proto::MetricFamily;
use tokio::sync::watch;

use super::FetchOutcome;
use crate::metrics::{desc, gauge_family};

/// In-flight calls by key, with the instant each one started.
type Calls = Arc<Mutex<HashMap<String, (watch::Receiver<Option<FetchOutcome>>, Instant)>>>;

/// Which side of a coalesced call this caller is on.
enum Role {
    Leader(watch::Sender<Option<FetchOutcome>>),
    Waiter(watch::Receiver<Option<FetchOutcome>>),
}

/// Coalesces concurrent work for the same key so that only one task performs it while the rest
/// wait and observe the same outcome.
pub(crate) struct Flight {
    calls: Calls,
}

/// Owned by the spawned fetch task. Dropping it, on completion or during a
/// panic unwind, deregisters the key and publishes the outcome (an explicit
/// error when none was recorded), so the key never outlives its fetch.
struct Publish {
    calls: Calls,
    key: String,
    tx: watch::Sender<Option<FetchOutcome>>,
    out: Option<FetchOutcome>,
}

impl Drop for Publish {
    fn drop(&mut self) {
        self.calls.lock().remove(&self.key);
        // Waiters must not observe the zero value: its kind is `Stored`, which
        // would make them serve a nonexistent cached artifact.
        let out = self.out.take().unwrap_or_else(FetchOutcome::error);
        let _ = self.tx.send(Some(out));
    }
}

impl Flight {
    pub(crate) fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Runs `f` for `key` unless an identical call is already in progress, in
    /// which case it waits for that call and returns its result. The shared
    /// result is then applied independently by each caller to its own response.
    ///
    /// `f` runs on a detached task that deregisters the key itself, so
    /// dropping the leader's future (its client disconnected) neither aborts
    /// the fetch the waiters depend on nor leaves the key registered.
    pub(crate) async fn do_call<F, Fut>(&self, key: &str, f: F) -> FetchOutcome
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = FetchOutcome> + Send + 'static,
    {
        let role = {
            let mut calls = self.calls.lock();
            if let Some((rx, _)) = calls.get(key) {
                Role::Waiter(rx.clone())
            } else {
                let (tx, rx) = watch::channel(None);
                calls.insert(key.to_string(), (rx, Instant::now()));
                Role::Leader(tx)
            }
        };

        match role {
            Role::Waiter(mut rx) => loop {
                let value = rx.borrow_and_update().clone();
                if let Some(out) = value {
                    return out;
                }
                if rx.changed().await.is_err() {
                    // The fetch task was dropped without publishing anything
                    // (runtime shutdown): report the same explicit error a
                    // panicking fetch publishes.
                    return FetchOutcome::error();
                }
            },
            Role::Leader(tx) => {
                let fut = f();
                let mut publish = Publish {
                    calls: Arc::clone(&self.calls),
                    key: key.to_string(),
                    tx,
                    out: None,
                };
                let result = tokio::spawn(async move {
                    let out = fut.await;
                    publish.out = Some(out.clone());
                    drop(publish);
                    out
                })
                .await;
                match result {
                    Ok(out) => out,
                    Err(e) => {
                        // Resume a panic on this task so the caller's own catch
                        // (HTTP middleware) still handles it.
                        if e.is_panic() {
                            std::panic::resume_unwind(e.into_panic());
                        }
                        FetchOutcome::error()
                    }
                }
            }
        }
    }

    /// A scrape-time collector over this flight's in-flight calls.
    pub(crate) fn collector(&self) -> FlightCollector {
        FlightCollector {
            calls: Arc::clone(&self.calls),
            inflight: desc(
                "forklift_upstream_fetches_inflight",
                "Coalesced upstream fetches currently in flight.",
                &[],
            ),
            oldest: desc(
                "forklift_upstream_fetch_oldest_seconds",
                "Age of the oldest in-flight upstream fetch, 0 when none; beyond the 60s upstream timeout a fetch is stuck.",
                &[],
            ),
        }
    }
}

/// Exports in-flight upstream fetch backlog and age, so a stuck or piling-up
/// fetch is visible before clients report failures.
pub(crate) struct FlightCollector {
    calls: Calls,
    inflight: Desc,
    oldest: Desc,
}

impl Collector for FlightCollector {
    fn desc(&self) -> Vec<&Desc> {
        vec![&self.inflight, &self.oldest]
    }

    fn collect(&self) -> Vec<MetricFamily> {
        let (n, oldest) = {
            let calls = self.calls.lock();
            let oldest = calls
                .values()
                .map(|(_, started)| started.elapsed().as_secs_f64())
                .fold(0.0, f64::max);
            (calls.len(), oldest)
        };
        vec![
            gauge_family(&self.inflight, vec![(vec![], n as f64)]),
            gauge_family(&self.oldest, vec![(vec![], oldest)]),
        ]
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::time::Duration;

    use chrono::{TimeZone, Utc};
    use http::StatusCode;

    use crate::repo::{
        DEFAULT_UPSTREAM_COOLDOWN, FetchKind, FetchOutcome, Flight, MAX_UPSTREAM_COOLDOWN,
        NegCache, parse_retry_after, retry_after_seconds,
    };
    use crate::testing::repo::new_test_manager;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn flight_coalesces_concurrent_calls() {
        let f = Arc::new(Flight::new());
        let calls = Arc::new(AtomicI64::new(0));
        let (release_tx, release_rx) = tokio::sync::watch::channel(false);

        const WAITERS: usize = 20;
        let mut handles = Vec::with_capacity(WAITERS);
        for _ in 0..WAITERS {
            let f = Arc::clone(&f);
            let calls = Arc::clone(&calls);
            let mut rx = release_rx.clone();
            handles.push(tokio::spawn(async move {
                f.do_call("same-key", move || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    // Hold the in-flight call so the rest coalesce behind it.
                    while !*rx.borrow_and_update() {
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                    FetchOutcome::stored()
                })
                .await
            }));
        }

        // Give the tasks time to pile up behind the first call, then release.
        tokio::time::sleep(Duration::from_millis(100)).await;
        release_tx.send(true).expect("release");
        for h in handles {
            h.await.expect("waiter finished");
        }

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "coalesced fn ran more than once"
        );
    }

    /// Verifies that a panic in the in-flight fn does not leave the key registered
    /// with waiters blocked forever: cleanup runs before the unwind resumes,
    /// coalesced waiters receive an explicit `FetchKind::Error` (not the misleading
    /// default `Stored`), and a subsequent call for the same key proceeds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn flight_panic_does_not_wedge_key() {
        let f = Arc::new(Flight::new());
        let (in_flight_tx, in_flight_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::watch::channel(false);

        // Leader: holds the in-flight call open, then panics.
        let leader = {
            let f = Arc::clone(&f);
            let mut rx = release_rx.clone();
            tokio::spawn(async move {
                f.do_call("boom", move || async move {
                    let _ = in_flight_tx.send(());
                    while !*rx.borrow_and_update() {
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                    panic!("fetch blew up");
                })
                .await
            })
        };

        // The leader is inside fn; a call for "boom" is now registered.
        in_flight_rx.await.expect("leader entered fn");
        let waiter = {
            let f = Arc::clone(&f);
            tokio::spawn(async move {
                f.do_call("boom", || async {
                    panic!(
                        "coalesced waiter ran its own fn; it should have joined the in-flight call"
                    )
                })
                .await
            })
        };
        // Let the waiter park on the shared result, then let the leader panic.
        tokio::time::sleep(Duration::from_millis(100)).await;
        release_tx.send(true).expect("release");

        let out = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("coalesced waiter wedged after the in-flight call panicked")
            .expect("waiter task finished");
        assert_eq!(out.kind, FetchKind::Error, "coalesced waiter kind");

        let leader = tokio::time::timeout(Duration::from_secs(2), leader)
            .await
            .expect("leader finished");
        assert!(
            leader.is_err_and(|e| e.is_panic()),
            "panic did not propagate out of do_call on the leader task"
        );

        // The key must be deregistered; a fresh call runs its fn and returns.
        let done = tokio::time::timeout(
            Duration::from_secs(2),
            f.do_call("boom", || async { FetchOutcome::stored() }),
        )
        .await
        .expect("do_call wedged after a prior panic on the same key");
        assert_eq!(done.kind, FetchKind::Stored);
    }

    /// Verifies that dropping the leader's `do_call` future (the client that
    /// started the fetch disconnected) neither fails the coalesced waiters nor
    /// leaves the key registered: the fetch runs to completion, waiters observe
    /// its outcome, and a later call for the same key runs its own fn.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn flight_cancelled_leader_does_not_wedge_key() {
        let f = Arc::new(Flight::new());
        let (in_flight_tx, in_flight_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::watch::channel(false);

        let leader = {
            let f = Arc::clone(&f);
            let mut rx = release_rx.clone();
            tokio::spawn(async move {
                f.do_call("gone", move || async move {
                    let _ = in_flight_tx.send(());
                    while !*rx.borrow_and_update() {
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                    FetchOutcome::stored()
                })
                .await
            })
        };

        in_flight_rx.await.expect("leader entered fn");
        let waiter = {
            let f = Arc::clone(&f);
            tokio::spawn(async move {
                f.do_call("gone", || async {
                    panic!(
                        "coalesced waiter ran its own fn; it should have joined the in-flight call"
                    )
                })
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        leader.abort();
        assert!(
            leader.await.is_err_and(|e| e.is_cancelled()),
            "leader task was not cancelled"
        );
        release_tx.send(true).expect("release");

        let out = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("coalesced waiter wedged after the leader was cancelled")
            .expect("waiter task finished");
        assert_eq!(
            out.kind,
            FetchKind::Stored,
            "coalesced waiter did not observe the detached fetch outcome"
        );

        let ran = Arc::new(AtomicI64::new(0));
        let done = {
            let ran = Arc::clone(&ran);
            tokio::time::timeout(
                Duration::from_secs(2),
                f.do_call("gone", move || async move {
                    ran.fetch_add(1, Ordering::SeqCst);
                    FetchOutcome::stored()
                }),
            )
            .await
            .expect("do_call wedged after the leader was cancelled")
        };
        assert_eq!(done.kind, FetchKind::Stored);
        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "key stayed registered after the leader was cancelled"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn flight_collector_reports_inflight_backlog() {
        let f = Arc::new(Flight::new());
        let collector = f.collector();
        let sample = || {
            prometheus::core::Collector::collect(&collector)
                .iter()
                .map(|mf| mf.get_metric()[0].get_gauge().get_value())
                .collect::<Vec<_>>()
        };
        assert_eq!(sample(), vec![0.0, 0.0], "idle");

        let (in_flight_tx, in_flight_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::watch::channel(false);
        let call = {
            let f = Arc::clone(&f);
            let mut rx = release_rx.clone();
            tokio::spawn(async move {
                f.do_call("slow", move || async move {
                    let _ = in_flight_tx.send(());
                    while !*rx.borrow_and_update() {
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                    FetchOutcome::stored()
                })
                .await
            })
        };
        in_flight_rx.await.expect("call entered fn");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let busy = sample();
        assert_eq!(busy[0], 1.0, "inflight");
        assert!(busy[1] >= 0.05, "oldest age = {}", busy[1]);

        release_tx.send(true).expect("release");
        call.await.expect("call finished");
        assert_eq!(sample(), vec![0.0, 0.0], "drained");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn flight_distinct_keys_do_not_coalesce() {
        let f = Arc::new(Flight::new());
        let calls = Arc::new(AtomicI64::new(0));
        let mut handles = Vec::new();
        for key in ["a", "b", "c"] {
            let f = Arc::clone(&f);
            let calls = Arc::clone(&calls);
            handles.push(tokio::spawn(async move {
                f.do_call(key, move || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    FetchOutcome::stored()
                })
                .await
            }));
        }
        for h in handles {
            h.await.expect("call finished");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3, "distinct keys ran fn");
    }

    #[test]
    fn parse_retry_after_cases() {
        let now = Utc
            .with_ymd_and_hms(2026, 6, 30, 12, 0, 0)
            .single()
            .expect("valid instant");
        let http_date = httpdate::fmt_http_date((now + chrono::TimeDelta::seconds(90)).into());
        let cases: Vec<(&str, &str, Duration)> = vec![
            ("empty falls back to default", "", DEFAULT_UPSTREAM_COOLDOWN),
            (
                "small seconds clamped up to default",
                "2",
                DEFAULT_UPSTREAM_COOLDOWN,
            ),
            ("seconds honored", "60", Duration::from_secs(60)),
            ("capped at max", "100000", MAX_UPSTREAM_COOLDOWN),
            (
                "garbage falls back to default",
                "soon",
                DEFAULT_UPSTREAM_COOLDOWN,
            ),
            ("http-date honored", &http_date, Duration::from_secs(90)),
        ];
        for (name, hdr, want) in cases {
            assert_eq!(
                parse_retry_after(hdr, now),
                want,
                "parse_retry_after({name})"
            );
        }
    }

    #[test]
    fn retry_after_seconds_rounds_and_floors() {
        assert_eq!(retry_after_seconds(Duration::from_secs(15)), "15");
        assert_eq!(
            retry_after_seconds(Duration::from_millis(100)),
            "1",
            "floor of 1"
        );
    }

    #[tokio::test]
    async fn write_retry() {
        let tm = new_test_manager().await;
        // 429 with a hint is relayed verbatim.
        let resp = tm.engine.write_retry(StatusCode::TOO_MANY_REQUESTS, "30");
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(resp.headers().get(http::header::RETRY_AFTER).unwrap(), "30");
        // A non-retry status is normalized to 503.
        let resp = tm.engine.write_retry(StatusCode::BAD_GATEWAY, "");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn neg_cache_remaining() {
        let mut c = NegCache::new();
        let fixed = Utc
            .with_ymd_and_hms(2026, 6, 30, 12, 0, 0)
            .single()
            .expect("valid instant");
        let clock = Arc::new(parking_lot::Mutex::new(fixed));
        let handle = Arc::clone(&clock);
        c.set_now(Arc::new(move || *handle.lock()));

        assert!(
            c.remaining("missing").is_none(),
            "remaining on missing key should report not live"
        );
        c.set("k", Duration::from_secs(30));
        assert_eq!(c.remaining("k"), Some(Duration::from_secs(30)));
        // Advance past expiry.
        *clock.lock() = fixed + chrono::TimeDelta::minutes(1);
        assert!(
            c.remaining("k").is_none(),
            "remaining after expiry should report not live"
        );
    }
}
