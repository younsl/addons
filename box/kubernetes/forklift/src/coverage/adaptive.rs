//! Adaptive concurrency control for the GitLab crawl.
//!
//! The scan has no business asking an operator how many requests per second the
//! GitLab instance can take. Nobody knows that number: it depends on the
//! instance's size, what else is hitting it right now, and which endpoint is
//! being called. A number picked once in a settings form is wrong in both
//! directions over a day, and the failure mode of guessing high is that forklift
//! degrades somebody else's GitLab.
//!
//! So the limit is discovered instead, with the AIMD loop Vector uses for its
//! adaptive request concurrency: increase the in-flight limit by one whenever a
//! request succeeds while the limit is saturated, and cut it multiplicatively
//! the moment the service pushes back. Back-pressure is read from two signals,
//! an explicit 429 or 5xx, and a round-trip time that has risen well above its
//! own recent average, which is what a service under load does before it starts
//! refusing outright.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::coverage::{Error, Res};

pub(crate) const INITIAL_LIMIT: i64 = 2;
/// max_limit caps the ramp. The scan is background work, so there is no reason
/// to let it grow into the instance's whole request budget even if latency never
/// degrades.
pub(crate) const MAX_LIMIT: i64 = 32;
/// min_limit keeps one request in flight, so a struggling instance still makes
/// progress rather than the scan stalling to a stop.
pub(crate) const MIN_LIMIT: i64 = 1;

/// decrease_ratio is the multiplicative cut on back-pressure. Vector's default;
/// it backs off quickly without collapsing to one in a single step.
const DECREASE_RATIO: f64 = 0.9;
/// ewma_alpha weights the newest round-trip time against the running average.
const EWMA_ALPHA: f64 = 0.4;
/// rtt_deviation_scale is how many mean deviations above the average a
/// round-trip time must be before it counts as back-pressure. Too low and
/// ordinary jitter throttles the scan; too high and it only reacts to 429s.
const RTT_DEVIATION_SCALE: f64 = 2.5;

/// requestOutcome is what one completed request tells the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestOutcome {
    Ok,
    /// An explicit refusal: 429, or a 5xx, which under load is the same message
    /// with a worse error code.
    BackPressure,
    /// A result that says nothing about capacity, such as a 404 for a file that
    /// is not there. It moves neither the limit nor the latency average.
    Ignored,
}

pub(crate) const OUTCOME_OK: RequestOutcome = RequestOutcome::Ok;
pub(crate) const OUTCOME_BACK_PRESSURE: RequestOutcome = RequestOutcome::BackPressure;
pub(crate) const OUTCOME_IGNORED: RequestOutcome = RequestOutcome::Ignored;

/// The mutable half of the controller, all of it under one lock.
#[derive(Debug)]
struct LimiterState {
    limit: i64,
    debt: i64,
    rtt_mean: f64,
    rtt_dev: f64,
    has_rtt: bool,
    /// paused_until honours a Retry-After header: the service named a time, and
    /// no amount of concurrency tuning is a substitute for waiting it out.
    paused_until: Option<Instant>,
    /// peak_limit is the highest limit reached, reported once per scan so the
    /// behaviour is observable without a setting to look at.
    peak_limit: i64,
}

/// adaptiveLimiter bounds in-flight requests and moves that bound in response to
/// how the service is behaving.
///
/// Capacity is handed out as semaphore permits, so waiting for one stays
/// cancellable. Lowering the limit cannot take a permit back from a request
/// already running, so the reduction is recorded as debt and paid off as those
/// requests finish.
#[derive(Debug)]
pub(crate) struct AdaptiveLimiter {
    tokens: Semaphore,
    state: Mutex<LimiterState>,
}

/// Builds a limiter holding [`INITIAL_LIMIT`] permits.
pub(crate) fn new_adaptive_limiter() -> Arc<AdaptiveLimiter> {
    Arc::new(AdaptiveLimiter {
        tokens: Semaphore::new(INITIAL_LIMIT as usize),
        state: Mutex::new(LimiterState {
            limit: INITIAL_LIMIT,
            debt: 0,
            rtt_mean: 0.0,
            rtt_dev: 0.0,
            has_rtt: false,
            paused_until: None,
            peak_limit: INITIAL_LIMIT,
        }),
    })
}

impl AdaptiveLimiter {
    /// acquire blocks until a slot is free and the pause, if any, has elapsed.
    pub(crate) async fn acquire(&self, cancel: &CancellationToken) -> Res<()> {
        loop {
            let wait = {
                let state = self.state.lock();
                state
                    .paused_until
                    .map(|until| until.saturating_duration_since(Instant::now()))
                    .unwrap_or_default()
            };
            if wait.is_zero() {
                break;
            }
            sleep_cancellable(cancel, wait).await?;
        }
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(Error::Cancelled),
            permit = self.tokens.acquire() => {
                // The permit travels with the request rather than with a guard:
                // release() decides whether it is handed back or retired to pay
                // off a reduction.
                permit.map_err(|_| Error::Cancelled)?.forget();
                Ok(())
            }
        }
    }

    /// release returns a slot and folds the request's outcome into the limit.
    ///
    /// `saturated` reports whether the caller had to wait for its slot, which is
    /// the evidence that the limit, rather than the workload, is what is holding
    /// the scan back. Without it a scan with only a few projects left would keep
    /// raising a limit it is not using.
    pub(crate) fn release(&self, outcome: RequestOutcome, rtt: Duration, saturated: bool) {
        let pay_debt = {
            let mut state = self.state.lock();
            match outcome {
                RequestOutcome::BackPressure => {
                    let next = (state.limit as f64 * DECREASE_RATIO) as i64;
                    set_limit_locked(&mut state, &self.tokens, next);
                }
                RequestOutcome::Ok => {
                    let seconds = rtt.as_secs_f64();
                    if state.has_rtt
                        && seconds > state.rtt_mean + RTT_DEVIATION_SCALE * state.rtt_dev
                    {
                        // Latency has climbed clear of its own recent spread, which
                        // is what a service does on the way to refusing requests.
                        // Back off before it has to.
                        let next = (state.limit as f64 * DECREASE_RATIO) as i64;
                        set_limit_locked(&mut state, &self.tokens, next);
                    } else if saturated {
                        let next = state.limit + 1;
                        set_limit_locked(&mut state, &self.tokens, next);
                    }
                    observe_rtt_locked(&mut state, seconds);
                }
                // Says nothing about capacity either way.
                RequestOutcome::Ignored => {}
            }

            // A reduction is paid out of the permits coming back, so a request
            // already in flight is never interrupted to enforce it.
            let pay = state.debt > 0;
            if pay {
                state.debt -= 1;
            }
            pay
        };

        if !pay_debt {
            self.tokens.add_permits(1);
        }
    }

    /// pause holds every caller back for `d`, honouring a Retry-After.
    pub(crate) fn pause(&self, d: Duration) {
        let mut state = self.state.lock();
        let until = Instant::now() + d;
        if state.paused_until.is_none_or(|prev| until > prev) {
            state.paused_until = Some(until);
        }
    }

    /// stats reports the current and peak limit, for the log line that closes a
    /// scan.
    pub(crate) fn stats(&self) -> (i64, i64) {
        let state = self.state.lock();
        (state.limit, state.peak_limit)
    }
}

/// set_limit_locked moves the limit within its bounds, issuing or retiring
/// permits to match.
fn set_limit_locked(state: &mut LimiterState, tokens: &Semaphore, next: i64) {
    let next = next.clamp(MIN_LIMIT, MAX_LIMIT);
    if next > state.limit {
        tokens.add_permits((next - state.limit) as usize);
    } else if next < state.limit {
        state.debt += state.limit - next;
    }
    state.limit = next;
    if next > state.peak_limit {
        state.peak_limit = next;
    }
}

/// observe_rtt_locked folds one round-trip time into the running average and its
/// mean deviation, which together define what "unusually slow" means later.
fn observe_rtt_locked(state: &mut LimiterState, seconds: f64) {
    if !state.has_rtt {
        state.rtt_mean = seconds;
        state.rtt_dev = 0.0;
        state.has_rtt = true;
        return;
    }
    let deviation = (seconds - state.rtt_mean).abs();
    state.rtt_mean += EWMA_ALPHA * (seconds - state.rtt_mean);
    state.rtt_dev += EWMA_ALPHA * (deviation - state.rtt_dev);
}

/// Sleeps for `d`, returning early when the scan is cancelled.
pub(crate) async fn sleep_cancellable(cancel: &CancellationToken, d: Duration) -> Res<()> {
    if d.is_zero() {
        return if cancel.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        };
    }
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(Error::Cancelled),
        () = tokio::time::sleep(d) => Ok(()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    use crate::coverage::adaptive::{
        INITIAL_LIMIT, MAX_LIMIT, MIN_LIMIT, OUTCOME_BACK_PRESSURE, OUTCOME_IGNORED, OUTCOME_OK,
        new_adaptive_limiter,
    };
    use crate::coverage::scan::tests::install_crypto_provider;
    use crate::coverage::{GitLabOptions, new_gitlab_client};

    fn never() -> CancellationToken {
        CancellationToken::new()
    }

    #[tokio::test]
    async fn adaptive_limiter_ramps_up_while_saturated() {
        let l = new_adaptive_limiter();
        let cancel = never();

        // Fast, successful responses while the limit is the constraint: the limit
        // should climb one step per request.
        for _ in 0..10 {
            l.acquire(&cancel).await.expect("acquire");
            l.release(OUTCOME_OK, Duration::from_millis(1), true);
        }
        let (got, peak) = l.stats();
        assert!(
            got > INITIAL_LIMIT,
            "limit = {got}, want it above the initial {INITIAL_LIMIT} after a clean run"
        );
        assert!(peak >= got, "peak {peak} is below the current limit {got}");
    }

    #[tokio::test]
    async fn adaptive_limiter_does_not_ramp_when_idle() {
        let l = new_adaptive_limiter();
        let cancel = never();

        // Not saturated: the workload, not the limit, is the constraint. Raising the
        // limit here would be inventing capacity nothing asked for.
        for _ in 0..10 {
            l.acquire(&cancel).await.expect("acquire");
            l.release(OUTCOME_OK, Duration::from_millis(1), false);
        }
        let (got, _) = l.stats();
        assert_eq!(got, INITIAL_LIMIT, "limit = {got}, want it unchanged");
    }

    #[tokio::test]
    async fn adaptive_limiter_backs_off_on_pushback() {
        let l = new_adaptive_limiter();
        let cancel = never();

        for _ in 0..40 {
            l.acquire(&cancel).await.expect("acquire");
            l.release(OUTCOME_OK, Duration::from_millis(1), true);
        }
        let (before, _) = l.stats();

        for _ in 0..20 {
            l.acquire(&cancel).await.expect("acquire");
            l.release(OUTCOME_BACK_PRESSURE, Duration::ZERO, true);
        }
        let (after, _) = l.stats();
        assert!(
            after < before,
            "limit went {before} -> {after}, want a cut on back-pressure"
        );
        assert!(
            after >= MIN_LIMIT,
            "limit = {after}, want it to never fall below {MIN_LIMIT}"
        );
    }

    #[tokio::test]
    async fn adaptive_limiter_backs_off_on_latency_alone() {
        let l = new_adaptive_limiter();
        let cancel = never();

        // Establish a baseline of fast responses.
        for _ in 0..30 {
            l.acquire(&cancel).await.expect("acquire");
            l.release(OUTCOME_OK, Duration::from_millis(10), true);
        }
        let (before, _) = l.stats();

        // Latency climbs clear of its own recent spread, which is what a service does
        // on the way to refusing requests. No error code involved.
        //
        // One sample is the whole test on purpose: what the controller detects is the
        // change, not the absolute figure. A level that persists is folded into the
        // average and becomes the new normal, which is what lets the scan recover its
        // throughput against an instance that is simply slower rather than throttling
        // itself to one request forever. An instance that is actually overloaded goes
        // on to answer 429 or 5xx, and that path cuts the limit regardless of timing.
        l.acquire(&cancel).await.expect("acquire");
        l.release(OUTCOME_OK, Duration::from_secs(5), true);

        let (after, _) = l.stats();
        assert!(
            after < before,
            "limit went {before} -> {after}, want a cut on a latency spike"
        );
    }

    /// Sustained slowness is not treated as permanent back-pressure: it becomes the
    /// baseline, so the scan does not throttle itself to a crawl against an instance
    /// that is merely slow. The explicit-refusal path is what handles overload.
    #[tokio::test]
    async fn adaptive_limiter_treats_sustained_latency_as_the_new_normal() {
        let l = new_adaptive_limiter();
        let cancel = never();
        for _ in 0..30 {
            l.acquire(&cancel).await.expect("acquire");
            l.release(OUTCOME_OK, Duration::from_millis(10), true);
        }
        for _ in 0..40 {
            l.acquire(&cancel).await.expect("acquire");
            l.release(OUTCOME_OK, Duration::from_secs(5), true);
        }
        let (got, _) = l.stats();
        assert!(
            got > MIN_LIMIT,
            "limit = {got}, want the scan to keep making progress"
        );
    }

    #[tokio::test]
    async fn adaptive_limiter_ignores_non_capacity_outcomes() {
        let l = new_adaptive_limiter();
        let cancel = never();
        for _ in 0..20 {
            l.acquire(&cancel).await.expect("acquire");
            // A 404 for a file that is not there says nothing about capacity.
            l.release(OUTCOME_IGNORED, Duration::from_millis(1), true);
        }
        let (got, _) = l.stats();
        assert_eq!(got, INITIAL_LIMIT, "limit = {got}, want it unchanged");
    }

    #[tokio::test]
    async fn adaptive_limiter_never_exceeds_its_cap() {
        let l = new_adaptive_limiter();
        let cancel = never();
        for _ in 0..(MAX_LIMIT * 4) {
            l.acquire(&cancel).await.expect("acquire");
            l.release(OUTCOME_OK, Duration::from_millis(1), true);
        }
        let (got, _) = l.stats();
        assert!(
            got <= MAX_LIMIT,
            "limit = {got}, want it capped at {MAX_LIMIT}"
        );
    }

    #[tokio::test]
    async fn adaptive_limiter_acquire_is_cancellable() {
        let l = new_adaptive_limiter();
        let cancel = never();
        // Drain every slot so the next acquire has to wait.
        for _ in 0..INITIAL_LIMIT {
            l.acquire(&cancel).await.expect("acquire");
        }
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(
            l.acquire(&cancelled).await.is_err(),
            "acquire returned a slot on a cancelled context"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn adaptive_limiter_holds_in_flight_within_the_limit() {
        let l = new_adaptive_limiter();
        let cancel = never();

        let in_flight = Arc::new(AtomicI64::new(0));
        let peak = Arc::new(parking_lot::Mutex::new(0i64));
        let mut handles = Vec::new();
        for _ in 0..64 {
            let l = Arc::clone(&l);
            let cancel = cancel.clone();
            let in_flight = Arc::clone(&in_flight);
            let peak = Arc::clone(&peak);
            handles.push(tokio::spawn(async move {
                if l.acquire(&cancel).await.is_err() {
                    return;
                }
                let n = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                {
                    let mut peak = peak.lock();
                    if n > *peak {
                        *peak = n;
                    }
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                l.release(OUTCOME_OK, Duration::from_millis(1), true);
            }));
        }
        for h in handles {
            h.await.expect("worker");
        }

        let got = *peak.lock();
        assert!(
            got <= MAX_LIMIT,
            "{got} requests were in flight at once, above the cap of {MAX_LIMIT}"
        );
    }

    /// Answers 429 for the first two calls, then recovers.
    struct FlakyUpstream {
        calls: AtomicI64,
    }

    impl Respond for FlakyUpstream {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            if self.calls.fetch_add(1, Ordering::SeqCst) < 2 {
                return ResponseTemplate::new(429).insert_header("Retry-After", "0");
            }
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "application/json")
                .set_body_raw(br#"{"ok":true}"#.to_vec(), "application/json")
        }
    }

    /// The client must not need a rate setting: a service that pushes back is
    /// answered by the limiter, and the request still succeeds on retry.
    #[tokio::test]
    async fn gitlab_client_backs_off_and_recovers_without_configuration() {
        install_crypto_provider();
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(FlakyUpstream {
                calls: AtomicI64::new(0),
            })
            .mount(&srv)
            .await;

        let c = new_gitlab_client(GitLabOptions {
            api_base_url: srv.uri(),
            ..GitLabOptions::default()
        });
        let out: HashMap<String, bool> = c.get_json("anything").await.expect("get_json");
        assert_eq!(out.get("ok"), Some(&true), "body = {out:?}");
        let (limit, _) = c.concurrency_stats();
        assert!(
            limit <= INITIAL_LIMIT,
            "limit = {limit}, want it not raised across two refusals"
        );
    }
}
