//! Active/standby high availability via Kubernetes Lease-based leader election.
//! Only the elected leader becomes Ready (so the Service routes to a single
//! active instance) and runs the background blob sweeper, which guarantees a
//! single writer to the shared SQLite database.
//!
//! [`LeaderElector`] coordinates a Kubernetes Lease, releases leadership on
//! cancellation, and uses the lease/renew/retry durations from [`HAConfig`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use kube::api::{Api, PostParams};
use tokio_util::sync::CancellationToken;

use crate::config::HAConfig;

mod podlabel;
pub use podlabel::{ROLE_LABEL, ROLE_LEADER, ROLE_STANDBY};

/// Errors returned by the cluster module.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The pod is not running inside a cluster (no service-account env/token).
    #[error("in-cluster config: {0}")]
    InClusterConfig(#[source] kube::config::InClusterError),
    /// The Kubernetes client could not be built from the in-cluster config.
    #[error("kubernetes client: {0}")]
    KubernetesClient(#[source] kube::Error),
    /// Reading the Lease failed for a reason other than "not found".
    #[error("get lease: {0}")]
    GetLease(#[source] kube::Error),
    /// Patching a pod's role label failed.
    #[error("patch pod role label: {0}")]
    PatchPodRole(#[source] kube::Error),
    /// Listing leader-labelled pods failed.
    #[error("list leader pods: {0}")]
    ListLeaderPods(#[source] kube::Error),
    #[error("{}", .0.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n"))]
    Multiple(Vec<Error>),
}

/// Result alias used throughout the module.
pub type Result<T> = std::result::Result<T, Error>;

/// Live leadership term state, mutated from the election loop and read by
/// [`Elector::step_down`].
#[derive(Default)]
pub(crate) struct TermState {
    /// Reports whether this instance currently holds leadership.
    pub(crate) leading: bool,
    /// Marks the current term as a voluntary hand-off so the loop applies a
    /// longer re-contention cooldown after it ends.
    pub(crate) stepping_down: bool,
    /// Cancels the current leadership term (releasing the Lease via
    /// `ReleaseOnCancel`). `None` between terms.
    pub(crate) term_cancel: Option<CancellationToken>,
}

/// Runs leader election against a Lease object.
pub struct Elector {
    cfg: HAConfig,
    client: kube::Client,
    pub(crate) state: parking_lot::Mutex<TermState>,
}

impl Elector {
    /// Builds an Elector using the in-cluster Kubernetes config. Outside a
    /// cluster this fails cleanly (no service-account environment or token).
    pub fn new(cfg: HAConfig) -> Result<Arc<Elector>> {
        let rest_cfg = kube::Config::incluster().map_err(Error::InClusterConfig)?;
        let client = kube::Client::try_from(rest_cfg).map_err(Error::KubernetesClient)?;
        Ok(Self::new_with_client(cfg, client))
    }

    /// Builds an Elector with an injected Kubernetes client (tests).
    pub fn new_with_client(cfg: HAConfig, client: kube::Client) -> Arc<Elector> {
        Arc::new(Elector {
            cfg,
            client,
            state: parking_lot::Mutex::new(TermState::default()),
        })
    }

    /// The Kubernetes client the elector talks through.
    pub fn client(&self) -> &kube::Client {
        &self.client
    }

    fn leases(&self) -> Api<Lease> {
        Api::namespaced(self.client.clone(), &self.cfg.lease_namespace)
    }

    /// Returns the current Lease holder's identity (the leader pod name), or
    /// `""` when the Lease does not exist or has no holder. Replication
    /// standbys use this to locate the leader pod via the headless Service.
    pub async fn leader_identity(&self) -> Result<String> {
        match self.leases().get(&self.cfg.lease_name).await {
            Ok(lease) => Ok(lease
                .spec
                .and_then(|s| s.holder_identity)
                .unwrap_or_default()),
            Err(e) if is_not_found(&e) => Ok(String::new()),
            Err(e) => Err(Error::GetLease(e)),
        }
    }

    /// Returns the Lease's transition counter, a value that increases by one
    /// every time leadership changes hands. The leader uses it as a fencing
    /// token: writes to shared storage carry this token, and a stale ("zombie")
    /// former leader (paused past its lease and superseded) carries a lower
    /// token, so the storage layer can reject its writes and prevent
    /// split-brain overwrites. Returns 0 when the Lease is absent or has no
    /// recorded transitions.
    pub async fn fencing_token(&self) -> Result<i64> {
        match self.leases().get(&self.cfg.lease_name).await {
            Ok(lease) => Ok(lease
                .spec
                .and_then(|s| s.lease_transitions)
                .map(i64::from)
                .unwrap_or(0)),
            Err(e) if is_not_found(&e) => Ok(0),
            Err(e) => Err(Error::GetLease(e)),
        }
    }

    /// Contends for leadership until `cancel` fires. `on_started_leading` is
    /// invoked with a token that is cancelled when leadership is lost;
    /// `on_stopped_leading` is invoked when this instance stops leading. The
    /// election loop re-contends after losing leadership so a demoted instance
    /// can become leader again later.
    pub async fn run(
        self: Arc<Self>,
        cancel: CancellationToken,
        on_started_leading: impl Fn(CancellationToken) + Send + Sync + 'static,
        on_stopped_leading: impl Fn() + Send + Sync + 'static,
    ) {
        let lock = LeaseLock {
            api: self.leases(),
            name: self.cfg.lease_name.clone(),
            namespace: self.cfg.lease_namespace.clone(),
            identity: self.cfg.identity.clone(),
            lease: None,
        };
        let election = LeaderElectionConfig {
            lease_duration: self.cfg.lease_duration,
            renew_deadline: self.cfg.renew_deadline,
            retry_period: self.cfg.retry_period,
            release_on_cancel: true,
        };
        if let Err(msg) = election.validate() {
            tracing::error!(err = %msg, identity = %self.cfg.identity, "leaderelection: invalid configuration");
            return;
        }
        let me = Arc::clone(&self);
        let on_started = move |c: CancellationToken| {
            me.state.lock().leading = true;
            tracing::info!(identity = %me.cfg.identity, "acquired leadership");
            on_started_leading(c);
        };
        let me = Arc::clone(&self);
        let on_stopped = move || {
            me.state.lock().leading = false;
            tracing::warn!(identity = %me.cfg.identity, "lost leadership");
            on_stopped_leading();
        };
        let mut lock = lock;
        while !cancel.is_cancelled() {
            // Each term runs under its own cancellable token so step_down can
            // end just this term (releasing the Lease) without tearing down the
            // process.
            let term = cancel.child_token();
            self.state.lock().term_cancel = Some(term.clone());

            LeaderElector::new(&mut lock, election.clone())
                .run(term.clone(), &on_started, &on_stopped)
                .await;
            term.cancel();

            // The term ends when leadership is lost or the term is cancelled.
            // After a voluntary step-down, pause longer than a standby's
            // acquisition latency (a full lease_duration plus a retry tick) so
            // the freed Lease is taken over instead of being re-grabbed by this
            // instance; otherwise back off briefly.
            let backoff = {
                let mut st = self.state.lock();
                st.term_cancel = None;
                if st.stepping_down {
                    st.stepping_down = false;
                    self.cfg.lease_duration + self.cfg.retry_period
                } else {
                    self.cfg.retry_period
                }
            };

            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = tokio::time::sleep(backoff) => {}
            }
        }
    }

    /// Voluntarily releases leadership for a controlled manual failover. It
    /// cancels the current term so the election loop releases the Lease (via
    /// `ReleaseOnCancel`) and then pauses re-contention long enough for a
    /// standby to acquire it. It is a no-op returning `false` when this
    /// instance is not currently the leader. The vacated Lease records a
    /// transition, so the new leader's fencing token strictly exceeds this one,
    /// preserving the single-writer guard.
    pub fn step_down(&self) -> bool {
        let mut st = self.state.lock();
        let Some(term_cancel) = st.term_cancel.clone() else {
            return false;
        };
        if !st.leading {
            return false;
        }
        tracing::info!(identity = %self.cfg.identity, "stepping down leadership on request");
        st.stepping_down = true;
        term_cancel.cancel();
        true
    }
}

/// True when the API server answered 404 for the resource.
fn is_not_found(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(status) if status.code == 404)
}

const JITTER_FACTOR: f64 = 1.2;

/// The subset of `leaderelection.LeaderElectionConfig` the Elector sets.
#[derive(Debug, Clone)]
struct LeaderElectionConfig {
    /// The duration that non-leader candidates will wait to force acquire
    /// leadership; measured against the time of the last observed ack.
    lease_duration: Duration,
    /// The duration that the acting master will retry refreshing leadership
    /// before giving up.
    renew_deadline: Duration,
    /// The duration the clients should wait between tries of actions.
    retry_period: Duration,
    /// Release the lease when the term is cancelled, so a new leader can be
    /// elected right away instead of waiting a full lease_duration.
    release_on_cancel: bool,
}

impl LeaderElectionConfig {
    /// The sanity checks `NewLeaderElector` applies.
    fn validate(&self) -> std::result::Result<(), &'static str> {
        if self.lease_duration <= self.renew_deadline {
            return Err("leaseDuration must be greater than renewDeadline");
        }
        if self.renew_deadline.as_secs_f64() <= self.retry_period.as_secs_f64() * JITTER_FACTOR {
            return Err("renewDeadline must be greater than retryPeriod*JitterFactor");
        }
        if self.lease_duration < Duration::from_secs(1) {
            return Err("leaseDuration must be greater than zero");
        }
        if self.renew_deadline < Duration::from_secs(1) {
            return Err("renewDeadline must be greater than zero");
        }
        if self.retry_period < Duration::from_secs(1) {
            return Err("retryPeriod must be greater than zero");
        }
        Ok(())
    }
}

/// `resourcelock.LeaderElectionRecord`: the information stored in the Lease
/// spec that contenders compare.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LeaderElectionRecord {
    holder_identity: String,
    lease_duration_seconds: i64,
    acquire_time: Option<DateTime<Utc>>,
    renew_time: Option<DateTime<Utc>>,
    leader_transitions: i64,
}

fn micro_time_to_chrono(t: &MicroTime) -> Option<DateTime<Utc>> {
    let ts = t.0;
    DateTime::<Utc>::from_timestamp(ts.as_second(), ts.subsec_nanosecond() as u32)
}

fn chrono_to_micro_time(t: DateTime<Utc>) -> Option<MicroTime> {
    k8s_openapi::jiff::Timestamp::new(t.timestamp(), t.timestamp_subsec_nanos() as i32)
        .ok()
        .map(MicroTime)
}

/// `resourcelock.LeaseSpecToLeaderElectionRecord`.
fn lease_spec_to_record(spec: Option<&LeaseSpec>) -> LeaderElectionRecord {
    let Some(spec) = spec else {
        return LeaderElectionRecord::default();
    };
    LeaderElectionRecord {
        holder_identity: spec.holder_identity.clone().unwrap_or_default(),
        lease_duration_seconds: spec.lease_duration_seconds.map(i64::from).unwrap_or(0),
        acquire_time: spec.acquire_time.as_ref().and_then(micro_time_to_chrono),
        renew_time: spec.renew_time.as_ref().and_then(micro_time_to_chrono),
        leader_transitions: spec.lease_transitions.map(i64::from).unwrap_or(0),
    }
}

/// `resourcelock.LeaderElectionRecordToLeaseSpec`.
fn record_to_lease_spec(r: &LeaderElectionRecord) -> LeaseSpec {
    LeaseSpec {
        holder_identity: Some(r.holder_identity.clone()),
        lease_duration_seconds: Some(r.lease_duration_seconds as i32),
        acquire_time: r.acquire_time.and_then(chrono_to_micro_time),
        renew_time: r.renew_time.and_then(chrono_to_micro_time),
        lease_transitions: Some(r.leader_transitions as i32),
        ..LeaseSpec::default()
    }
}

/// `resourcelock.LeaseLock`: the Lease object as a lock, caching the last
/// object seen so updates carry its `resourceVersion` (optimistic concurrency
/// is what makes two contenders' updates serialize).
struct LeaseLock {
    api: Api<Lease>,
    name: String,
    namespace: String,
    identity: String,
    lease: Option<Lease>,
}

impl LeaseLock {
    /// Returns the election record from the Lease spec.
    async fn get(&mut self) -> std::result::Result<LeaderElectionRecord, kube::Error> {
        let lease = self.api.get(&self.name).await?;
        let record = lease_spec_to_record(lease.spec.as_ref());
        self.lease = Some(lease);
        Ok(record)
    }

    /// Attempts to create a Lease holding `record`.
    async fn create(
        &mut self,
        record: &LeaderElectionRecord,
    ) -> std::result::Result<(), kube::Error> {
        let lease = Lease {
            metadata: ObjectMeta {
                name: Some(self.name.clone()),
                namespace: Some(self.namespace.clone()),
                ..ObjectMeta::default()
            },
            spec: Some(record_to_lease_spec(record)),
        };
        self.lease = Some(self.api.create(&PostParams::default(), &lease).await?);
        Ok(())
    }

    /// Updates the cached Lease with `record`.
    async fn update(&mut self, record: &LeaderElectionRecord) -> std::result::Result<(), String> {
        let Some(mut lease) = self.lease.clone() else {
            return Err("lease not initialized, call get or create first".into());
        };
        lease.spec = Some(record_to_lease_spec(record));
        let updated = self
            .api
            .replace(&self.name, &PostParams::default(), &lease)
            .await
            .map_err(|e| e.to_string())?;
        self.lease = Some(updated);
        Ok(())
    }

    /// `namespace/name`, used in log lines.
    fn describe(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }
}

/// One term of `leaderelection.LeaderElector`.
struct LeaderElector<'a> {
    lock: &'a mut LeaseLock,
    config: LeaderElectionConfig,
    /// The last record read from or written to the lock.
    observed_record: LeaderElectionRecord,
    observed_raw: Option<LeaderElectionRecord>,
    /// When `observed_record` was last set; the lease is valid for
    /// `lease_duration_seconds` past this instant.
    observed_time: Instant,
}

impl<'a> LeaderElector<'a> {
    fn new(lock: &'a mut LeaseLock, config: LeaderElectionConfig) -> Self {
        LeaderElector {
            lock,
            config,
            observed_record: LeaderElectionRecord::default(),
            observed_raw: None,
            observed_time: Instant::now(),
        }
    }

    /// `LeaderElector.Run`: acquire, then run `on_started` and renew until the lease is lost or
    /// `term` is cancelled.
    async fn run(
        mut self,
        term: CancellationToken,
        on_started: &(impl Fn(CancellationToken) + Send + Sync),
        on_stopped: &(impl Fn() + Send + Sync),
    ) {
        if self.acquire(&term).await {
            let lead = term.child_token();
            on_started(lead.clone());
            self.renew(&lead).await;
            lead.cancel();
        }
        on_stopped();
    }

    /// Loops calling `try_acquire_or_renew` (jittered every retry_period) and
    /// returns true once succeeded; returns false only when `term` is
    /// cancelled first.
    async fn acquire(&mut self, term: &CancellationToken) -> bool {
        let ctx = term.child_token();
        let desc = self.lock.describe();
        tracing::info!("attempting to acquire leader lease {desc}...");
        let mut succeeded = false;
        while !ctx.is_cancelled() {
            succeeded = tokio::select! {
                _ = ctx.cancelled() => false,
                ok = self.try_acquire_or_renew() => ok,
            };
            if !succeeded {
                tracing::debug!("failed to acquire lease {desc}");
            } else {
                tracing::info!("successfully acquired lease {desc}");
                ctx.cancel();
            }
            tokio::select! {
                _ = ctx.cancelled() => break,
                _ = tokio::time::sleep(jitter(self.config.retry_period)) => {}
            }
        }
        succeeded
    }

    /// Loops calling `try_acquire_or_renew` every retry_period (bounded by
    /// renew_deadline per attempt window) and returns when renewal fails or
    /// `lead` is cancelled; then releases the lock when `release_on_cancel`.
    async fn renew(&mut self, lead: &CancellationToken) {
        let ctx = lead.child_token();
        let desc = self.lock.describe();
        while !ctx.is_cancelled() {
            // wait.PollUntilContextTimeout(ctx, retry_period, renew_deadline,
            // immediate=true, tryAcquireOrRenew)
            let deadline = tokio::time::Instant::now() + self.config.renew_deadline;
            let err: Option<&str> = loop {
                let renewed = tokio::select! {
                    _ = ctx.cancelled() => break Some("context canceled"),
                    _ = tokio::time::sleep_until(deadline) => break Some("context deadline exceeded"),
                    ok = self.try_acquire_or_renew() => ok,
                };
                if renewed {
                    break None;
                }
                tokio::select! {
                    _ = ctx.cancelled() => break Some("context canceled"),
                    _ = tokio::time::sleep_until(deadline) => break Some("context deadline exceeded"),
                    _ = tokio::time::sleep(self.config.retry_period) => {}
                }
            };
            match err {
                None => tracing::debug!("successfully renewed lease {desc}"),
                Some(err) => {
                    tracing::info!("failed to renew lease {desc}: {err}");
                    ctx.cancel();
                }
            }
            tokio::select! {
                _ = ctx.cancelled() => break,
                _ = tokio::time::sleep(self.config.retry_period) => {}
            }
        }

        // If we hold the lease, give it up.
        if self.config.release_on_cancel {
            self.release().await;
        }
    }

    /// Attempts to release the leader lease if we have acquired it. Returns
    /// true on success (or when we never held it), false when the release
    /// failed and the Lease remains held until it expires.
    async fn release(&mut self) -> bool {
        if !self.is_leader() {
            return true;
        }
        let now = Utc::now();
        let record = LeaderElectionRecord {
            holder_identity: String::new(),
            leader_transitions: self.observed_record.leader_transitions,
            lease_duration_seconds: 1,
            renew_time: Some(now),
            acquire_time: Some(now),
        };
        let update = self.lock.update(&record);
        match tokio::time::timeout(self.config.renew_deadline, update).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!("Failed to release lock: {e}");
                return false;
            }
            Err(_) => {
                tracing::error!("Failed to release lock: context deadline exceeded");
                return false;
            }
        }
        self.set_observed_record(record);
        true
    }

    /// Tries to acquire a leader lease if it is not already acquired, or
    /// renews it when held. Returns true on success.
    async fn try_acquire_or_renew(&mut self) -> bool {
        let now = Utc::now();
        let mut record = LeaderElectionRecord {
            holder_identity: self.lock.identity.clone(),
            lease_duration_seconds: self.config.lease_duration.as_secs() as i64,
            renew_time: Some(now),
            acquire_time: Some(now),
            leader_transitions: 0,
        };

        // 1. Fast path for the leader to update optimistically, assuming the
        //    record observed last time is the current version.
        if self.is_leader() && self.is_lease_valid(Instant::now()) {
            let old = self.observed_record.clone();
            record.acquire_time = old.acquire_time;
            record.leader_transitions = old.leader_transitions;
            match self.lock.update(&record).await {
                Ok(()) => {
                    self.set_observed_record(record);
                    return true;
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to update lock optimistically: {e}, falling back to slow path"
                    );
                }
            }
        }

        // 2. Obtain or create the election record.
        let old = match self.lock.get().await {
            Ok(r) => r,
            Err(e) if is_not_found(&e) => {
                if let Err(e) = self.lock.create(&record).await {
                    tracing::error!("error initially creating leader election record: {e}");
                    return false;
                }
                self.set_observed_record(record);
                return true;
            }
            Err(e) => {
                tracing::error!(
                    "error retrieving resource lock {}: {e}",
                    self.lock.describe()
                );
                return false;
            }
        };

        // 3. Record obtained; check the identity and time.
        if self.observed_raw.as_ref() != Some(&old) {
            self.set_observed_record(old.clone());
            self.observed_raw = Some(old.clone());
        }
        if !old.holder_identity.is_empty()
            && self.is_lease_valid(Instant::now())
            && !self.is_leader()
        {
            tracing::debug!(
                "lock is held by {} and has not yet expired",
                old.holder_identity
            );
            return false;
        }

        // 4. We're going to try to update. The record is set to its default
        //    here; correct it before updating.
        if self.is_leader() {
            record.acquire_time = old.acquire_time;
            record.leader_transitions = old.leader_transitions;
        } else {
            record.leader_transitions = old.leader_transitions + 1;
        }

        // Update the lock itself.
        if let Err(e) = self.lock.update(&record).await {
            tracing::error!("Failed to update lock: {e}");
            return false;
        }
        self.set_observed_record(record);
        true
    }

    /// True if the last observed leader was this client.
    fn is_leader(&self) -> bool {
        self.observed_record.holder_identity == self.lock.identity
    }

    fn is_lease_valid(&self, now: Instant) -> bool {
        let dur = Duration::from_secs(self.observed_record.lease_duration_seconds.max(0) as u64);
        self.observed_time + dur > now
    }

    fn set_observed_record(&mut self, record: LeaderElectionRecord) {
        self.observed_record = record;
        self.observed_time = Instant::now();
    }
}

/// `wait.Jitter(duration, JITTER_FACTOR)`.
fn jitter(d: Duration) -> Duration {
    let f: f64 = rand::random_range(0.0..1.0);
    d.mul_f64(1.0 + f * JITTER_FACTOR)
}

#[cfg(test)]
pub(crate) mod tests {
    mod election {
        use std::time::Duration;

        use crate::cluster::*;

        #[test]
        fn new_outside_cluster_fails() {
            let in_cluster = std::env::var_os("KUBERNETES_SERVICE_HOST")
                .is_some_and(|v| !v.is_empty())
                && std::path::Path::new("/var/run/secrets/kubernetes.io/serviceaccount/token")
                    .exists();
            if in_cluster {
                eprintln!("running inside a cluster");
                return;
            }
            let err = Elector::new(HAConfig {
                lease_name: "forklift".into(),
                lease_namespace: "default".into(),
                identity: "pod-0".into(),
                lease_duration: Duration::from_secs(15),
                renew_deadline: Duration::from_secs(10),
                retry_period: Duration::from_secs(2),
                ..HAConfig::default()
            })
            .err()
            .expect("expected error outside a cluster");
            assert!(
                matches!(err, Error::InClusterConfig(_) | Error::KubernetesClient(_)),
                "err = {err}"
            );
        }

        #[test]
        fn leader_election_config_validation() {
            let defaults = LeaderElectionConfig {
                lease_duration: Duration::from_secs(15),
                renew_deadline: Duration::from_secs(10),
                retry_period: Duration::from_secs(2),
                release_on_cancel: true,
            };
            assert!(defaults.validate().is_ok(), "chart defaults must be valid");

            let mut bad = defaults.clone();
            bad.renew_deadline = Duration::from_secs(15);
            assert_eq!(
                bad.validate(),
                Err("leaseDuration must be greater than renewDeadline")
            );

            let mut bad = defaults.clone();
            bad.retry_period = Duration::from_secs(9);
            assert_eq!(
                bad.validate(),
                Err("renewDeadline must be greater than retryPeriod*JitterFactor")
            );

            let mut bad = defaults;
            bad.lease_duration = Duration::from_millis(500);
            bad.renew_deadline = Duration::from_millis(200);
            bad.retry_period = Duration::from_millis(100);
            assert_eq!(
                bad.validate(),
                Err("leaseDuration must be greater than zero")
            );
        }

        /// The Lease spec is the wire format of the election record: what one contender
        /// writes, another must read back unchanged, because the whole algorithm is a
        /// comparison of these fields.
        #[test]
        fn lease_spec_round_trips_the_election_record() {
            let now =
                chrono::DateTime::from_timestamp(1_700_000_000, 123_456_000).expect("timestamp");
            let record = LeaderElectionRecord {
                holder_identity: "forklift-0".into(),
                lease_duration_seconds: 15,
                acquire_time: Some(now),
                renew_time: Some(now),
                leader_transitions: 3,
            };
            let spec = record_to_lease_spec(&record);
            assert_eq!(spec.holder_identity.as_deref(), Some("forklift-0"));
            assert_eq!(spec.lease_duration_seconds, Some(15));
            assert_eq!(spec.lease_transitions, Some(3));
            assert_eq!(lease_spec_to_record(Some(&spec)), record);

            // A Lease nobody has held reads as the zero record rather than an error.
            assert_eq!(
                lease_spec_to_record(Some(&LeaseSpec::default())),
                LeaderElectionRecord::default()
            );
            assert_eq!(lease_spec_to_record(None), LeaderElectionRecord::default());
        }

        /// Retry sleeps are jittered upward only, by at most `JITTER_FACTOR`, so
        /// contenders spread out instead of hammering the API server in lockstep.
        #[test]
        fn jitter_stays_within_configured_bounds() {
            let base = Duration::from_secs(2);
            for _ in 0..64 {
                let d = jitter(base);
                assert!(d >= base, "jitter shortened the retry period: {d:?}");
                assert!(
                    d <= base.mul_f64(1.0 + JITTER_FACTOR),
                    "jitter exceeded the factor: {d:?}"
                );
            }
        }
    }
}
