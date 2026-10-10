//! The periodic reconcile loop for the long-running process.

use std::future::Future;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::humanize::go_duration;

/// Runs `reconcile` immediately, then on every `interval` tick, until
/// `shutdown` fires. Per-pass errors are logged and do not stop the loop.
/// `reconcile` returns the number of objects processed.
pub async fn run<F, Fut>(
    interval: Duration,
    shutdown: CancellationToken,
    reconcile: F,
    loop_name: &'static str,
) where
    F: Fn() -> Fut + Send + Sync,
    Fut: Future<Output = Result<usize, anyhow::Error>> + Send,
{
    let pass = || async {
        let start = Instant::now();
        match reconcile().await {
            Ok(n) => info!(
                r#loop = loop_name,
                instances = n,
                elapsed = %go_duration(start.elapsed()),
                "reconcile pass completed"
            ),
            Err(err) => error!(
                r#loop = loop_name,
                error = format!("{err:#}"),
                elapsed = %go_duration(start.elapsed()),
                "reconcile pass failed"
            ),
        }
    };

    pass().await;
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires immediately; the pass above already covered it.
    ticker.tick().await;
    loop {
        tokio::select! {
            () = shutdown.cancelled() => {
                info!(r#loop = loop_name, "controller shutting down");
                return;
            }
            _ = ticker.tick() => pass().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn reconciles_immediately_then_on_ticks_until_cancelled() {
        let calls = Arc::new(AtomicUsize::new(0));
        let shutdown = CancellationToken::new();
        let c = calls.clone();
        let task = tokio::spawn(run(
            Duration::from_secs(10),
            shutdown.clone(),
            move || {
                let c = c.clone();
                async move {
                    let n = c.fetch_add(1, Ordering::SeqCst);
                    if n == 1 {
                        Err(anyhow::anyhow!("boom"))
                    } else {
                        Ok(n)
                    }
                }
            },
            "test",
        ));
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "immediate pass");
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "error does not stop the loop"
        );
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn stops_on_an_already_cancelled_token() {
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        run(
            Duration::from_hours(1),
            shutdown,
            move || {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(0)
                }
            },
            "test",
        )
        .await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the immediate pass still runs"
        );
    }
}
