//! Leader election loop. Publishes leadership on a `watch` channel that gates
//! the poller.

use std::time::Duration;

use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior, interval, timeout};
use tracing::{info, warn};

use crate::k8s::lease::LeaseLock;
use crate::observability::metrics::Metrics;

/// How long shutdown waits for the poller to finish its cycle before releasing.
const POLLER_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ElectorCfg {
    pub retry_period: Duration,
    pub renew_deadline: Duration,
}

pub struct Elector {
    lock: LeaseLock,
    cfg: ElectorCfg,
    leader: watch::Sender<bool>,
    metrics: Metrics,
}

impl Elector {
    pub const fn new(
        lock: LeaseLock,
        cfg: ElectorCfg,
        leader: watch::Sender<bool>,
        metrics: Metrics,
    ) -> Self {
        Self {
            lock,
            cfg,
            leader,
            metrics,
        }
    }

    /// Runs until `shutdown`, then waits for `poller_done` and releases the
    /// Lease so a standby takes over without waiting for expiry.
    pub async fn run(
        self,
        mut shutdown: watch::Receiver<bool>,
        mut poller_done: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let mut ticker = interval(self.cfg.retry_period);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_renew: Option<Instant> = None;
        info!(identity = self.lock.identity(), "leader election started");

        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => break,
            }
            last_renew = self.step(last_renew).await;
        }

        let _ = timeout(POLLER_DRAIN_TIMEOUT, poller_done.wait_for(|done| *done)).await;
        if *self.leader.borrow() {
            match self.lock.release().await {
                Ok(()) => info!("released leader lease"),
                Err(e) => warn!(error = %e, "failed to release leader lease"),
            }
        }
        self.set_leader(false);
        Ok(())
    }

    async fn step(&self, last_renew: Option<Instant>) -> Option<Instant> {
        match self.lock.try_acquire_or_renew().await {
            Ok(true) => {
                self.set_leader(true);
                Some(Instant::now())
            }
            Ok(false) => {
                self.set_leader(false);
                None
            }
            Err(e) => {
                warn!(error = %e, "leader lease acquire or renew failed");
                if last_renew.is_some_and(|t| t.elapsed() > self.cfg.renew_deadline) {
                    warn!("renew deadline exceeded; stepping down");
                    self.set_leader(false);
                    return None;
                }
                last_renew
            }
        }
    }

    fn set_leader(&self, is_leader: bool) {
        let changed = self.leader.send_if_modified(|current| {
            let changed = *current != is_leader;
            *current = is_leader;
            changed
        });
        if changed {
            self.metrics.set_leader(is_leader);
            if is_leader {
                info!(identity = self.lock.identity(), "acquired leadership");
            } else {
                info!(identity = self.lock.identity(), "lost leadership");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::fake::FakeApiServer;

    fn elector(
        server: &FakeApiServer,
        identity: &str,
    ) -> (Elector, watch::Receiver<bool>, Metrics) {
        let (tx, rx) = watch::channel(false);
        let metrics = Metrics::new();
        let lock = LeaseLock::new(
            server.client(),
            "ns",
            "leader",
            identity,
            Duration::from_secs(15),
        );
        let cfg = ElectorCfg {
            retry_period: Duration::from_millis(20),
            renew_deadline: Duration::from_millis(100),
        };
        (Elector::new(lock, cfg, tx, metrics.clone()), rx, metrics)
    }

    #[tokio::test]
    async fn step_tracks_lease_ownership() {
        let server = FakeApiServer::start().await;
        let (a, rx_a, metrics) = elector(&server, "a");
        let (b, rx_b, _) = elector(&server, "b");

        assert!(a.step(None).await.is_some());
        assert!(*rx_a.borrow());
        assert!(b.step(None).await.is_none());
        assert!(!*rx_b.borrow());
        assert!(metrics.render().contains("aws_health_event_leader 1"));
    }

    #[tokio::test]
    async fn api_errors_step_down_after_renew_deadline() {
        let server = FakeApiServer::start().await;
        let (a, rx, _) = elector(&server, "a");
        let renewed = a.step(None).await;
        server.fail_all();

        assert_eq!(a.step(renewed).await, renewed);
        assert!(*rx.borrow());

        let stale = Instant::now().checked_sub(Duration::from_secs(1));
        assert!(a.step(stale).await.is_none());
        assert!(!*rx.borrow());
    }

    #[tokio::test]
    async fn shutdown_releases_lease_to_standby() {
        let server = FakeApiServer::start().await;
        let (a, rx_a, _) = elector(&server, "a");
        let (b, _, _) = elector(&server, "b");
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (done_tx, done_rx) = watch::channel(false);

        let handle = tokio::spawn(a.run(shutdown_rx, done_rx));
        let mut rx = rx_a.clone();
        rx.wait_for(|leader| *leader).await.unwrap();

        shutdown_tx.send(true).unwrap();
        done_tx.send(true).unwrap();
        handle.await.unwrap().unwrap();

        assert!(!*rx_a.borrow());
        assert!(b.step(None).await.is_some());
    }
}
