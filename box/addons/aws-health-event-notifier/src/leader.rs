//! Leader election loop. Publishes leadership on a `watch` channel that gates
//! the poller.

use std::time::Duration;

use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior, interval, timeout};
use tracing::{info, warn};

use crate::k8s::lease::{LeaseLock, LeaseState};
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
        let mut tracker = Tracker::default();
        info!(
            identity = self.lock.identity(),
            lease = self.lock.name(),
            "leader election started"
        );

        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => break,
            }
            self.step(&mut tracker).await;
        }

        let _ = timeout(POLLER_DRAIN_TIMEOUT, poller_done.wait_for(|done| *done)).await;
        if *self.leader.borrow() {
            match self.lock.release().await {
                Ok(()) => info!(identity = self.lock.identity(), "released leader lease"),
                Err(e) => warn!(error = %e, "failed to release leader lease"),
            }
        }
        self.set_leader(false);
        Ok(())
    }

    async fn step(&self, t: &mut Tracker) {
        match self.lock.try_acquire_or_renew().await {
            Ok(LeaseState::Renewed) => {
                t.last_renew = Some(Instant::now());
                t.announced = None;
                if self.set_leader(true) {
                    info!(
                        identity = self.lock.identity(),
                        previous_leader = self.lock.identity(),
                        reason = "renewed",
                        "acquired leadership"
                    );
                }
            }
            Ok(LeaseState::Acquired { previous, reason }) => {
                t.last_renew = Some(Instant::now());
                t.announced = None;
                self.set_leader(true);
                info!(
                    identity = self.lock.identity(),
                    previous_leader = previous.as_deref(),
                    reason = reason.as_str(),
                    "acquired leadership"
                );
            }
            Ok(LeaseState::HeldBy(holder)) => {
                t.last_renew = None;
                let lost = self.step_down();
                self.announce_standby(t, holder, lost);
            }
            Err(e) => {
                warn!(error = %e, "leader lease acquire or renew failed");
                if t.last_renew
                    .is_some_and(|at| at.elapsed() > self.cfg.renew_deadline)
                {
                    warn!("renew deadline exceeded; stepping down");
                    t.last_renew = None;
                    if self.step_down() {
                        self.announce_standby(t, None, true);
                    }
                }
            }
        }
    }

    /// Logs a standby line on the first sighting of a holder and on every
    /// change, never on each retry tick.
    fn announce_standby(&self, t: &mut Tracker, holder: Option<String>, force: bool) {
        let changed = holder.is_some() && t.announced != holder;
        if force || changed {
            info!(
                identity = self.lock.identity(),
                leader = holder.as_deref(),
                "standing by"
            );
            t.announced = holder;
        }
    }

    fn step_down(&self) -> bool {
        let lost = self.set_leader(false);
        if lost {
            info!(identity = self.lock.identity(), "lost leadership");
        }
        lost
    }

    /// Returns true when leadership changed.
    fn set_leader(&self, is_leader: bool) -> bool {
        let changed = self.leader.send_if_modified(|current| {
            let changed = *current != is_leader;
            *current = is_leader;
            changed
        });
        if changed {
            self.metrics.set_leader(is_leader);
        }
        changed
    }
}

#[derive(Default)]
struct Tracker {
    last_renew: Option<Instant>,
    /// Holder named in the last standby line, `None` when unknown or none yet.
    announced: Option<String>,
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
    async fn step_tracks_lease_ownership_and_announces_once() {
        let server = FakeApiServer::start().await;
        let (a, rx_a, metrics) = elector(&server, "a");
        let (b, rx_b, _) = elector(&server, "b");
        let (mut ta, mut tb) = (Tracker::default(), Tracker::default());

        a.step(&mut ta).await;
        assert!(*rx_a.borrow());
        assert!(ta.last_renew.is_some());

        b.step(&mut tb).await;
        assert!(!*rx_b.borrow());
        assert_eq!(tb.announced, Some("a".into()));
        b.step(&mut tb).await;
        assert_eq!(tb.announced, Some("a".into()));

        a.step(&mut ta).await;
        assert!(*rx_a.borrow());
        assert!(metrics.render().contains("aws_health_event_leader 1"));
    }

    #[tokio::test]
    async fn unknown_holder_is_not_announced_until_known() {
        let server = FakeApiServer::start().await;
        let (a, _, _) = elector(&server, "a");
        let mut t = Tracker::default();
        a.announce_standby(&mut t, None, false);
        assert_eq!(t.announced, None);
        a.announce_standby(&mut t, Some("b".into()), false);
        assert_eq!(t.announced, Some("b".into()));
        a.announce_standby(&mut t, Some("c".into()), false);
        assert_eq!(t.announced, Some("c".into()));
    }

    #[tokio::test]
    async fn api_errors_step_down_after_renew_deadline() {
        let server = FakeApiServer::start().await;
        let (a, rx, _) = elector(&server, "a");
        let mut t = Tracker::default();
        a.step(&mut t).await;
        server.fail_all();

        let renewed = t.last_renew;
        a.step(&mut t).await;
        assert_eq!(t.last_renew, renewed);
        assert!(*rx.borrow());

        t.last_renew = Instant::now().checked_sub(Duration::from_secs(1));
        a.step(&mut t).await;
        assert!(t.last_renew.is_none());
        assert!(!*rx.borrow());
        assert_eq!(t.announced, None);
    }

    #[tokio::test]
    async fn renew_after_step_down_resumes_leadership() {
        let server = FakeApiServer::start().await;
        let (a, rx, _) = elector(&server, "a");
        let mut t = Tracker::default();
        a.step(&mut t).await;
        assert!(a.step_down());
        assert!(!*rx.borrow());
        a.step(&mut t).await;
        assert!(*rx.borrow());
    }

    #[tokio::test]
    async fn shutdown_releases_lease_to_standby() {
        let server = FakeApiServer::start().await;
        let (a, rx_a, _) = elector(&server, "a");
        let (b, rx_b, _) = elector(&server, "b");
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (done_tx, done_rx) = watch::channel(false);

        let handle = tokio::spawn(a.run(shutdown_rx, done_rx));
        let mut rx = rx_a.clone();
        rx.wait_for(|leader| *leader).await.unwrap();

        shutdown_tx.send(true).unwrap();
        done_tx.send(true).unwrap();
        handle.await.unwrap().unwrap();

        assert!(!*rx_a.borrow());
        b.step(&mut Tracker::default()).await;
        assert!(*rx_b.borrow());
    }
}
