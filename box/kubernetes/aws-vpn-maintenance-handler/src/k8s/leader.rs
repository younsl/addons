//! Single-active-instance leader election backed by a Lease.
//!
//! A correctness requirement, not an availability nicety: two active replicas
//! could each pick a different tunnel of the same connection, both pass
//! preflight against a peer the other is about to replace, and take the
//! connection down.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use kube::api::{ObjectMeta, PostParams};
use thiserror::Error;
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Lease timing, matching the common controller defaults.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub lease_duration: Duration,
    pub renew_deadline: Duration,
    pub retry_period: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            lease_duration: Duration::from_secs(15),
            renew_deadline: Duration::from_secs(10),
            retry_period: Duration::from_secs(2),
        }
    }
}

/// Parameterizes leader election.
#[derive(Debug, Clone)]
pub struct LeaderConfig {
    /// Uniquely identifies this candidate; use the Pod name.
    pub identity: String,
    /// The Lease shared by all candidates.
    pub lease_name: String,
    pub timing: Timing,
}

#[derive(Debug, Error)]
pub enum LeaderError {
    #[error("leader election requires identity, namespace, and lease name")]
    Incomplete,
}

/// The Lease operations election needs.
#[async_trait]
pub trait LeaseApi: Send + Sync {
    async fn get(&self) -> Result<Option<Lease>, String>;
    async fn create(&self, lease: Lease) -> Result<Lease, String>;
    async fn update(&self, lease: Lease) -> Result<Lease, String>;
}

/// Runs `lead` as the elected leader until `shutdown` fires. The token handed
/// to `lead` is cancelled the moment leadership is lost, so the new leader
/// resumes from persisted state instead of two controllers narrating the same
/// replacement.
pub async fn run<F, Fut>(
    api: &dyn LeaseApi,
    cfg: &LeaderConfig,
    shutdown: CancellationToken,
    lead: F,
) -> Result<(), LeaderError>
where
    F: Fn(CancellationToken) -> Fut,
    Fut: Future<Output = ()>,
{
    if cfg.identity.is_empty() || cfg.lease_name.is_empty() {
        return Err(LeaderError::Incomplete);
    }
    info!(lease_name = %cfg.lease_name, identity = %cfg.identity, "starting leader election");
    let mut observed = String::new();

    loop {
        if shutdown.is_cancelled() {
            return Ok(());
        }
        match try_acquire(api, cfg, Utc::now()).await {
            Ok(Acquire::Acquired) => {
                info!(identity = %cfg.identity, "acquired leadership, starting reconcile loop");
                let term = shutdown.child_token();
                let renew = renew_loop(api, cfg, term.clone());
                tokio::select! {
                    () = lead(term.clone()) => {}
                    () = renew => {}
                }
                term.cancel();
                info!(identity = %cfg.identity, "lost leadership, stopping reconcile loop");
                if shutdown.is_cancelled() {
                    release(api, cfg).await;
                    return Ok(());
                }
            }
            Ok(Acquire::HeldBy(other)) => {
                if other != observed {
                    info!(leader = %other, "observed leader");
                    observed = other;
                }
            }
            Err(err) => warn!(error = %err, "leader election attempt failed"),
        }
        tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            () = sleep(cfg.timing.retry_period) => {}
        }
    }
}

enum Acquire {
    Acquired,
    HeldBy(String),
}

fn micro(t: DateTime<Utc>) -> MicroTime {
    MicroTime(
        k8s_openapi::jiff::Timestamp::from_millisecond(t.timestamp_millis()).unwrap_or_default(),
    )
}

fn renew_time(lease: &Lease) -> Option<DateTime<Utc>> {
    let t = lease.spec.as_ref()?.renew_time.as_ref()?;
    DateTime::from_timestamp_millis(t.0.as_millisecond())
}

fn holder(lease: &Lease) -> String {
    lease
        .spec
        .as_ref()
        .and_then(|s| s.holder_identity.clone())
        .unwrap_or_default()
}

/// Creates the Lease, takes over an expired one, or renews our own.
async fn try_acquire(
    api: &dyn LeaseApi,
    cfg: &LeaderConfig,
    now: DateTime<Utc>,
) -> Result<Acquire, String> {
    let duration = i32::try_from(cfg.timing.lease_duration.as_secs()).unwrap_or(15);
    let Some(mut lease) = api.get().await? else {
        let lease = Lease {
            metadata: ObjectMeta {
                name: Some(cfg.lease_name.clone()),
                ..ObjectMeta::default()
            },
            spec: Some(LeaseSpec {
                holder_identity: Some(cfg.identity.clone()),
                lease_duration_seconds: Some(duration),
                acquire_time: Some(micro(now)),
                renew_time: Some(micro(now)),
                lease_transitions: Some(0),
                ..LeaseSpec::default()
            }),
        };
        api.create(lease).await?;
        return Ok(Acquire::Acquired);
    };

    let current = holder(&lease);
    let expired = renew_time(&lease).is_none_or(|t| {
        let ttl = chrono::TimeDelta::from_std(cfg.timing.lease_duration).unwrap_or_default();
        t + ttl < now
    });
    if !current.is_empty() && current != cfg.identity && !expired {
        return Ok(Acquire::HeldBy(current));
    }

    let spec = lease.spec.get_or_insert_with(LeaseSpec::default);
    if current != cfg.identity {
        spec.lease_transitions = Some(spec.lease_transitions.unwrap_or(0) + 1);
        spec.acquire_time = Some(micro(now));
    }
    spec.holder_identity = Some(cfg.identity.clone());
    spec.lease_duration_seconds = Some(duration);
    spec.renew_time = Some(micro(now));
    api.update(lease).await?;
    Ok(Acquire::Acquired)
}

/// Renews our Lease until it fails for longer than the renew deadline or the
/// term is cancelled. Returning means leadership is lost.
async fn renew_loop(api: &dyn LeaseApi, cfg: &LeaderConfig, term: CancellationToken) {
    let mut last_ok = Instant::now();
    loop {
        tokio::select! {
            () = term.cancelled() => return,
            () = sleep(cfg.timing.retry_period) => {}
        }
        match try_acquire(api, cfg, Utc::now()).await {
            Ok(Acquire::Acquired) => last_ok = Instant::now(),
            Ok(Acquire::HeldBy(other)) => {
                warn!(leader = %other, "lease was taken over by another candidate");
                return;
            }
            Err(err) => {
                warn!(error = %err, "failed to renew the leader lease");
                if last_ok.elapsed() >= cfg.timing.renew_deadline {
                    return;
                }
            }
        }
    }
}

/// Gives the Lease up on shutdown so the next candidate does not wait out the
/// full lease duration.
async fn release(api: &dyn LeaseApi, cfg: &LeaderConfig) {
    let Ok(Some(mut lease)) = api.get().await else {
        return;
    };
    if holder(&lease) != cfg.identity {
        return;
    }
    if let Some(spec) = &mut lease.spec {
        spec.holder_identity = Some(String::new());
    }
    if let Err(err) = api.update(lease).await {
        warn!(error = %err, "failed to release the leader lease on shutdown");
    }
}

/// The kube-backed Lease API.
pub struct KubeLeases {
    api: kube::Api<Lease>,
    name: String,
}

impl KubeLeases {
    #[must_use]
    pub fn new(client: kube::Client, namespace: &str, name: &str) -> Self {
        Self {
            api: kube::Api::namespaced(client, namespace),
            name: name.to_string(),
        }
    }
}

#[async_trait]
impl LeaseApi for KubeLeases {
    async fn get(&self) -> Result<Option<Lease>, String> {
        self.api
            .get_opt(&self.name)
            .await
            .map_err(|e| e.to_string())
    }
    async fn create(&self, lease: Lease) -> Result<Lease, String> {
        self.api
            .create(&PostParams::default(), &lease)
            .await
            .map_err(|e| e.to_string())
    }
    async fn update(&self, lease: Lease) -> Result<Lease, String> {
        self.api
            .replace(&self.name, &PostParams::default(), &lease)
            .await
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Default)]
    struct MemoryLease {
        lease: Mutex<Option<Lease>>,
        fail: AtomicBool,
    }

    #[async_trait]
    impl LeaseApi for MemoryLease {
        async fn get(&self) -> Result<Option<Lease>, String> {
            if self.fail.load(Ordering::SeqCst) {
                return Err("api down".into());
            }
            Ok(self.lease.lock().unwrap().clone())
        }
        async fn create(&self, lease: Lease) -> Result<Lease, String> {
            *self.lease.lock().unwrap() = Some(lease.clone());
            Ok(lease)
        }
        async fn update(&self, lease: Lease) -> Result<Lease, String> {
            if self.fail.load(Ordering::SeqCst) {
                return Err("api down".into());
            }
            *self.lease.lock().unwrap() = Some(lease.clone());
            Ok(lease)
        }
    }

    fn cfg(identity: &str) -> LeaderConfig {
        LeaderConfig {
            identity: identity.into(),
            lease_name: "lease".into(),
            timing: Timing {
                lease_duration: Duration::from_millis(300),
                renew_deadline: Duration::from_millis(200),
                retry_period: Duration::from_millis(20),
            },
        }
    }

    #[tokio::test]
    async fn rejects_incomplete_config() {
        let api = MemoryLease::default();
        let err = run(&api, &cfg(""), CancellationToken::new(), |_| async {})
            .await
            .unwrap_err();
        assert!(matches!(err, LeaderError::Incomplete));
    }

    #[tokio::test]
    async fn acquires_leads_and_releases_on_shutdown() {
        let api = Arc::new(MemoryLease::default());
        let led = Arc::new(AtomicUsize::new(0));
        let shutdown = CancellationToken::new();
        let (api2, led2, shutdown2) = (api.clone(), led.clone(), shutdown.clone());
        let task = tokio::spawn(async move {
            run(api2.as_ref(), &cfg("pod-a"), shutdown2, |term| {
                let led = led2.clone();
                async move {
                    led.fetch_add(1, Ordering::SeqCst);
                    term.cancelled().await;
                }
            })
            .await
        });
        sleep(Duration::from_millis(100)).await;
        assert_eq!(led.load(Ordering::SeqCst), 1);
        let lease = api.lease.lock().unwrap().clone().unwrap();
        assert_eq!(holder(&lease), "pod-a");
        assert_eq!(lease.spec.as_ref().unwrap().lease_transitions, Some(0));
        shutdown.cancel();
        task.await.unwrap().unwrap();
        let lease = api.lease.lock().unwrap().clone().unwrap();
        assert_eq!(holder(&lease), "", "released");
    }

    #[tokio::test]
    async fn waits_while_another_holds_then_takes_over_when_expired() {
        let api = Arc::new(MemoryLease::default());
        let now = Utc::now();
        api.create(Lease {
            metadata: ObjectMeta::default(),
            spec: Some(LeaseSpec {
                holder_identity: Some("pod-b".into()),
                renew_time: Some(micro(now)),
                lease_transitions: Some(3),
                ..LeaseSpec::default()
            }),
        })
        .await
        .unwrap();
        let led = Arc::new(AtomicUsize::new(0));
        let shutdown = CancellationToken::new();
        let (api2, led2, shutdown2) = (api.clone(), led.clone(), shutdown.clone());
        let task = tokio::spawn(async move {
            run(api2.as_ref(), &cfg("pod-a"), shutdown2, |term| {
                let led = led2.clone();
                async move {
                    led.fetch_add(1, Ordering::SeqCst);
                    term.cancelled().await;
                }
            })
            .await
        });
        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            led.load(Ordering::SeqCst),
            0,
            "pod-b still holds a fresh lease"
        );
        // The lease expires after 300ms without renewal.
        sleep(Duration::from_millis(350)).await;
        assert_eq!(led.load(Ordering::SeqCst), 1);
        let lease = api.lease.lock().unwrap().clone().unwrap();
        assert_eq!(holder(&lease), "pod-a");
        assert_eq!(lease.spec.as_ref().unwrap().lease_transitions, Some(4));
        shutdown.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn loses_leadership_when_renewal_fails_or_lease_is_taken() {
        let api = Arc::new(MemoryLease::default());
        let terms: Arc<Mutex<Vec<CancellationToken>>> = Arc::default();
        let shutdown = CancellationToken::new();
        let (api2, terms2, shutdown2) = (api.clone(), terms.clone(), shutdown.clone());
        let task = tokio::spawn(async move {
            run(api2.as_ref(), &cfg("pod-a"), shutdown2, |term| {
                let terms = terms2.clone();
                async move {
                    terms.lock().unwrap().push(term.clone());
                    term.cancelled().await;
                }
            })
            .await
        });
        sleep(Duration::from_millis(60)).await;
        assert_eq!(terms.lock().unwrap().len(), 1);
        // API outage longer than the renew deadline: the term is cancelled.
        api.fail.store(true, Ordering::SeqCst);
        sleep(Duration::from_millis(400)).await;
        assert!(terms.lock().unwrap()[0].is_cancelled());
        // Recovery: re-elected with a new term.
        api.fail.store(false, Ordering::SeqCst);
        sleep(Duration::from_millis(100)).await;
        assert_eq!(terms.lock().unwrap().len(), 2);

        // Another candidate steals the lease: the term ends too.
        let steal = |spec: &mut LeaseSpec| {
            spec.holder_identity = Some("pod-z".into());
            spec.renew_time = Some(micro(Utc::now() + chrono::TimeDelta::hours(1)));
        };
        steal(
            api.lease
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .spec
                .as_mut()
                .unwrap(),
        );
        sleep(Duration::from_millis(100)).await;
        assert!(terms.lock().unwrap()[1].is_cancelled());
        shutdown.cancel();
        task.await.unwrap().unwrap();
        // Not ours any more, so nothing was released.
        assert_eq!(holder(&api.lease.lock().unwrap().clone().unwrap()), "pod-z");
    }
}
