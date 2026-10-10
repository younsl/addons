//! Runs cleanup cycles on the configured schedule.
//!
//! Scheduling is kept apart from the cleanup work itself so that a new mode
//! (a cron expression, a usage-triggered wake-up) only touches this module.

use std::time::Duration;

use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::config::CleanupMode;

/// Drives `cycle` according to `mode` until it finishes (`once`) or
/// `shutdown` is cancelled (`interval`). The first cycle always runs
/// immediately, later cycles fire every `period`.
pub async fn run<F>(mode: CleanupMode, period: Duration, shutdown: CancellationToken, mut cycle: F)
where
    F: FnMut(),
{
    match mode {
        CleanupMode::Once => {
            info!("Running in 'once' mode - single cleanup execution");
            cycle();
            info!("Cleanup completed, exiting");
        }
        CleanupMode::Interval => {
            info!(
                interval_minutes = period.as_secs() / 60,
                "Running in 'interval' mode - periodic cleanup"
            );
            cycle();

            let mut ticker = time::interval(period);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            // The first tick completes immediately; the cycle above already covered it.
            ticker.tick().await;

            loop {
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => {
                        info!("Cleaner stopped");
                        return;
                    }
                    _ = ticker.tick() => cycle(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;

    use super::run;
    use crate::config::CleanupMode;

    const PERIOD: Duration = Duration::from_secs(60);

    #[tokio::test]
    async fn once_runs_exactly_one_cycle() {
        let count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&count);
        run(
            CleanupMode::Once,
            PERIOD,
            CancellationToken::new(),
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn interval_runs_immediately_then_every_period() {
        let count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&count);
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(run(
            CleanupMode::Interval,
            PERIOD,
            shutdown.clone(),
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
            },
        ));

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "first cycle runs immediately"
        );

        tokio::time::advance(PERIOD).await;
        tokio::task::yield_now().await;
        assert_eq!(count.load(Ordering::SeqCst), 2);

        tokio::time::advance(PERIOD).await;
        tokio::task::yield_now().await;
        assert_eq!(count.load(Ordering::SeqCst), 3);

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("run() did not exit after cancellation")
            .expect("task");
        assert_eq!(count.load(Ordering::SeqCst), 3, "no cycle after shutdown");
    }

    #[tokio::test]
    async fn interval_returns_when_cancelled_before_start() {
        let count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&count);
        let shutdown = CancellationToken::new();
        shutdown.cancel();

        tokio::time::timeout(
            Duration::from_secs(5),
            run(CleanupMode::Interval, PERIOD, shutdown, move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .await
        .expect("run() did not exit after cancellation");

        // The initial cycle still runs; cancellation is observed at the loop.
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
