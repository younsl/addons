//! Leader election over a `Lease`, so one controller replica acts at a time.

use std::time::{Duration, Instant};

use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use k8s_openapi::jiff::Timestamp;
use kube::Client;
use kube::api::{Api, PostParams};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("the renew deadline must be shorter than the lease duration")]
    RenewDeadline,
    #[error("the retry period must be above zero and shorter than the renew deadline")]
    RetryPeriod,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub namespace: String,
    pub name: String,
    /// The holder identity of this replica, its pod name.
    pub identity: String,
    /// How long a standby waits on an unrenewed lease before it takes over.
    pub lease_duration: Duration,
    /// How long the leader keeps trying to renew before it steps down.
    pub renew_deadline: Duration,
    pub retry_period: Duration,
}

impl Settings {
    /// The leader has to give up before a standby may take over.
    pub fn check(&self) -> Result<(), Error> {
        if self.renew_deadline >= self.lease_duration {
            return Err(Error::RenewDeadline);
        }
        if self.retry_period.is_zero() || self.retry_period >= self.renew_deadline {
            return Err(Error::RetryPeriod);
        }
        Ok(())
    }
}

/// The holder and the renew time of a lease another replica holds.
type Record = (Option<String>, Option<MicroTime>);

/// The spec to write, or `None` while another replica holds a live lease.
fn next(
    current: Option<&LeaseSpec>,
    settings: &Settings,
    expired: bool,
    now: Timestamp,
) -> Option<LeaseSpec> {
    let identity = Some(settings.identity.clone());
    let seconds = i32::try_from(settings.lease_duration.as_secs()).unwrap_or(i32::MAX);
    let held = |acquire_time, transitions| LeaseSpec {
        holder_identity: identity.clone(),
        lease_duration_seconds: Some(seconds.max(1)),
        acquire_time,
        renew_time: Some(MicroTime(now)),
        lease_transitions: Some(transitions),
        ..LeaseSpec::default()
    };
    let Some(spec) = current else {
        return Some(held(Some(MicroTime(now)), 0));
    };
    let transitions = spec.lease_transitions.unwrap_or(0);
    if spec.holder_identity == identity {
        return Some(held(spec.acquire_time.clone(), transitions));
    }
    let released = spec.holder_identity.as_deref().is_none_or(str::is_empty);
    (expired || released).then(|| held(Some(MicroTime(now)), transitions.saturating_add(1)))
}

pub struct Elector {
    api: Api<Lease>,
    settings: Settings,
    /// The record of another holder and when it was first seen. The local
    /// clock times its expiry, so a skewed clock on the holder does not matter.
    observed: Option<(Record, Instant)>,
    leading: bool,
}

impl Elector {
    pub fn new(client: Client, settings: Settings) -> Self {
        Self {
            api: Api::namespaced(client, &settings.namespace),
            settings,
            observed: None,
            leading: false,
        }
    }

    // A conflict means another replica wrote the lease first.
    fn written(&mut self, result: Result<Lease, kube::Error>) -> Result<bool, kube::Error> {
        match result {
            Ok(_) => {
                self.observed = None;
                Ok(true)
            }
            Err(kube::Error::Api(status)) if status.code == 409 => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Creates, renews, or takes over the lease. True when this replica holds
    /// it afterwards.
    async fn attempt(&mut self) -> Result<bool, kube::Error> {
        let now = Timestamp::now();
        let Some(mut lease) = self.api.get_opt(&self.settings.name).await? else {
            let lease = Lease {
                metadata: ObjectMeta {
                    name: Some(self.settings.name.clone()),
                    namespace: Some(self.settings.namespace.clone()),
                    ..ObjectMeta::default()
                },
                spec: next(None, &self.settings, false, now),
            };
            let created = self.api.create(&PostParams::default(), &lease).await;
            return self.written(created);
        };

        let record: Record = lease.spec.as_ref().map_or_else(Record::default, |spec| {
            (spec.holder_identity.clone(), spec.renew_time.clone())
        });
        if record.0.as_deref() == Some(self.settings.identity.as_str()) {
            self.observed = None;
        } else if self
            .observed
            .as_ref()
            .is_none_or(|(seen, _)| *seen != record)
        {
            self.observed = Some((record, Instant::now()));
        }
        let expired = self
            .observed
            .as_ref()
            .is_some_and(|(_, since)| since.elapsed() >= self.settings.lease_duration);

        let Some(spec) = next(lease.spec.as_ref(), &self.settings, expired, now) else {
            return Ok(false);
        };
        lease.spec = Some(spec);
        lease.metadata.managed_fields = None;
        let replaced = self
            .api
            .replace(&self.settings.name, &PostParams::default(), &lease)
            .await;
        self.written(replaced)
    }

    /// Waits until this replica holds the lease. False when the shutdown
    /// came first.
    pub async fn acquire(&mut self, shutdown: &CancellationToken) -> bool {
        let (lease, identity) = (self.settings.name.clone(), self.settings.identity.clone());
        info!(lease, identity, "Waiting for the leader lease");
        loop {
            match self.attempt().await {
                Ok(true) => {
                    self.leading = true;
                    info!(lease, identity, "Became the leader");
                    return true;
                }
                Ok(false) => debug!(lease, identity, "Another replica is the leader"),
                Err(e) => warn!(
                    lease,
                    identity, "Could not read or write the leader lease, {e}"
                ),
            }
            tokio::select! {
                () = shutdown.cancelled() => return false,
                () = tokio::time::sleep(self.settings.retry_period) => {}
            }
        }
    }

    /// Renews the lease and returns once the shutdown arrives or the lease is
    /// lost.
    pub async fn hold(&mut self, shutdown: &CancellationToken) {
        let (lease, identity) = (self.settings.name.clone(), self.settings.identity.clone());
        let deadline = self.settings.renew_deadline;
        let mut renewed = Instant::now();
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(self.settings.retry_period) => {}
            }
            match tokio::time::timeout(deadline, self.attempt()).await {
                Ok(Ok(true)) => {
                    renewed = Instant::now();
                    continue;
                }
                Ok(Ok(false)) => {
                    self.leading = false;
                    warn!(lease, identity, "Another replica took the leader lease");
                    return;
                }
                Ok(Err(e)) => warn!(lease, identity, "Could not renew the leader lease, {e}"),
                Err(_) => warn!(lease, identity, "Renewing the leader lease timed out"),
            }
            if renewed.elapsed() >= deadline {
                self.leading = false;
                warn!(
                    lease,
                    identity, "The leader lease was not renewed within the deadline"
                );
                return;
            }
        }
    }

    #[cfg(test)]
    const fn leading(&self) -> bool {
        self.leading
    }

    /// Hands the lease back so a standby takes over without waiting for it to
    /// expire.
    pub async fn release(&mut self) {
        if !self.leading {
            return;
        }
        self.leading = false;
        let (lease, identity) = (self.settings.name.clone(), self.settings.identity.clone());
        match self.give_back().await {
            Ok(()) => info!(lease, identity, "Released the leader lease"),
            Err(e) => warn!(lease, identity, "Could not release the leader lease, {e}"),
        }
    }

    async fn give_back(&self) -> Result<(), kube::Error> {
        let Some(mut lease) = self.api.get_opt(&self.settings.name).await? else {
            return Ok(());
        };
        let identity = Some(self.settings.identity.as_str());
        let Some(spec) = lease
            .spec
            .as_mut()
            .filter(|spec| spec.holder_identity.as_deref() == identity)
        else {
            return Ok(());
        };
        spec.holder_identity = None;
        spec.renew_time = Some(MicroTime(Timestamp::now()));
        spec.lease_duration_seconds = Some(1);
        lease.metadata.managed_fields = None;
        self.api
            .replace(&self.settings.name, &PostParams::default(), &lease)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const LEASES: &str = "/apis/coordination.k8s.io/v1/namespaces/kube-system/leases";

    fn settings() -> Settings {
        Settings {
            namespace: "kube-system".to_string(),
            name: "nco".to_string(),
            identity: "pod-a".to_string(),
            lease_duration: Duration::from_millis(60),
            renew_deadline: Duration::from_millis(40),
            retry_period: Duration::from_millis(10),
        }
    }

    fn elector(server: &MockServer) -> Elector {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let config = kube::Config::new(server.uri().parse().unwrap());
        Elector::new(Client::try_from(config).unwrap(), settings())
    }

    fn lease(holder: Option<&str>) -> Value {
        json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": {"name": "nco", "namespace": "kube-system", "resourceVersion": "9"},
            "spec": {
                "holderIdentity": holder,
                "leaseDurationSeconds": 15,
                "acquireTime": "2026-11-13T00:00:00.000000Z",
                "renewTime": "2026-11-13T00:00:05.000000Z",
                "leaseTransitions": 3
            }
        })
    }

    async fn mock(server: &MockServer, verb: &str, at: String, code: u16, body: Value) {
        Mock::given(method(verb))
            .and(path(at))
            .respond_with(ResponseTemplate::new(code).set_body_json(body))
            .mount(server)
            .await;
    }

    fn failure(code: u16, reason: &str) -> Value {
        json!({"apiVersion": "v1", "kind": "Status", "status": "Failure", "reason": reason, "code": code})
    }

    async fn writes(server: &MockServer) -> Vec<(String, Value)> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() != "GET")
            .map(|r| {
                (
                    r.method.to_string(),
                    serde_json::from_slice(&r.body).unwrap_or_default(),
                )
            })
            .collect()
    }

    #[test]
    fn settings_keep_the_leader_ahead_of_a_standby() {
        assert_eq!(settings().check(), Ok(()));
        let mut s = settings();
        s.renew_deadline = s.lease_duration;
        assert_eq!(s.check(), Err(Error::RenewDeadline));
        let mut s = settings();
        s.retry_period = s.renew_deadline;
        assert_eq!(s.check(), Err(Error::RetryPeriod));
        s.retry_period = Duration::ZERO;
        assert_eq!(s.check(), Err(Error::RetryPeriod));
    }

    #[test]
    fn next_creates_renews_and_takes_over() {
        let now = Timestamp::from_second(1_800_000_000).unwrap();
        let created = next(None, &settings(), false, now).unwrap();
        assert_eq!(created.holder_identity.as_deref(), Some("pod-a"));
        assert_eq!(created.lease_transitions, Some(0));
        assert_eq!(created.lease_duration_seconds, Some(1));
        assert_eq!(created.acquire_time, Some(MicroTime(now)));

        let later = Timestamp::from_second(1_800_000_009).unwrap();
        let renewed = next(Some(&created), &settings(), false, later).unwrap();
        assert_eq!(renewed.acquire_time, Some(MicroTime(now)));
        assert_eq!(renewed.renew_time, Some(MicroTime(later)));
        assert_eq!(renewed.lease_transitions, Some(0));

        let mut other = created;
        other.holder_identity = Some("pod-b".to_string());
        assert_eq!(next(Some(&other), &settings(), false, later), None);
        let taken = next(Some(&other), &settings(), true, later).unwrap();
        assert_eq!(taken.holder_identity.as_deref(), Some("pod-a"));
        assert_eq!(taken.acquire_time, Some(MicroTime(later)));
        assert_eq!(taken.lease_transitions, Some(1));

        other.holder_identity = None;
        assert!(next(Some(&other), &settings(), false, later).is_some());
        other.holder_identity = Some(String::new());
        assert!(next(Some(&other), &settings(), false, later).is_some());
    }

    #[tokio::test]
    async fn a_missing_lease_is_created() {
        let server = MockServer::start().await;
        mock(
            &server,
            "GET",
            format!("{LEASES}/nco"),
            404,
            failure(404, "NotFound"),
        )
        .await;
        mock(
            &server,
            "POST",
            LEASES.to_string(),
            201,
            lease(Some("pod-a")),
        )
        .await;
        let mut elector = elector(&server);

        assert!(elector.acquire(&CancellationToken::new()).await);
        assert!(elector.leading());
        let sent = writes(&server).await;
        assert_eq!(sent[0].0, "POST");
        assert_eq!(sent[0].1["metadata"]["name"], "nco");
        assert_eq!(sent[0].1["spec"]["holderIdentity"], "pod-a");
    }

    #[tokio::test]
    async fn a_lost_race_and_an_api_failure_are_no_leadership() {
        let server = MockServer::start().await;
        mock(
            &server,
            "GET",
            format!("{LEASES}/nco"),
            404,
            failure(404, "NotFound"),
        )
        .await;
        mock(
            &server,
            "POST",
            LEASES.to_string(),
            409,
            failure(409, "AlreadyExists"),
        )
        .await;
        let mut elector = elector(&server);
        assert!(!elector.attempt().await.unwrap());

        let down = MockServer::start().await;
        mock(
            &down,
            "GET",
            format!("{LEASES}/nco"),
            500,
            failure(500, "InternalError"),
        )
        .await;
        let mut elector = self::elector(&down);
        assert!(elector.attempt().await.is_err());
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        assert!(!elector.acquire(&shutdown).await);
        assert!(!elector.leading());
    }

    #[tokio::test]
    async fn a_standby_takes_over_once_the_lease_stops_moving() {
        let server = MockServer::start().await;
        mock(
            &server,
            "GET",
            format!("{LEASES}/nco"),
            200,
            lease(Some("pod-b")),
        )
        .await;
        mock(
            &server,
            "PUT",
            format!("{LEASES}/nco"),
            200,
            lease(Some("pod-a")),
        )
        .await;
        let mut elector = elector(&server);

        assert!(!elector.attempt().await.unwrap());
        assert_eq!(writes(&server).await.len(), 0);
        assert!(elector.acquire(&CancellationToken::new()).await);
        let sent = writes(&server).await;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1["spec"]["holderIdentity"], "pod-a");
        assert_eq!(sent[0].1["spec"]["leaseTransitions"], 4);
        assert_eq!(sent[0].1["metadata"]["resourceVersion"], "9");
    }

    #[tokio::test]
    async fn the_leader_renews_until_shutdown_and_then_releases() {
        let server = MockServer::start().await;
        mock(
            &server,
            "GET",
            format!("{LEASES}/nco"),
            200,
            lease(Some("pod-a")),
        )
        .await;
        mock(
            &server,
            "PUT",
            format!("{LEASES}/nco"),
            200,
            lease(Some("pod-a")),
        )
        .await;
        let mut elector = elector(&server);
        let shutdown = CancellationToken::new();
        assert!(elector.acquire(&shutdown).await);

        let stop = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(35)).await;
            stop.cancel();
        });
        elector.hold(&shutdown).await;
        assert!(elector.leading());
        let renewals = writes(&server).await.len();
        assert!(renewals >= 2, "renewed {renewals} times");

        elector.release().await;
        assert!(!elector.leading());
        let sent = writes(&server).await;
        let released = &sent.last().unwrap().1["spec"];
        assert_eq!(released["holderIdentity"], Value::Null);
        assert_eq!(released["leaseDurationSeconds"], 1);
        elector.release().await;
        assert_eq!(writes(&server).await.len(), sent.len());
    }

    #[tokio::test]
    async fn the_leader_steps_down_when_another_replica_holds_the_lease() {
        let server = MockServer::start().await;
        mock(
            &server,
            "GET",
            format!("{LEASES}/nco"),
            200,
            lease(Some("pod-b")),
        )
        .await;
        let mut elector = elector(&server);
        elector.leading = true;

        elector.hold(&CancellationToken::new()).await;
        assert!(!elector.leading());
        elector.release().await;
        assert_eq!(writes(&server).await.len(), 0);
    }

    #[tokio::test]
    async fn the_leader_steps_down_after_the_renew_deadline() {
        let server = MockServer::start().await;
        mock(
            &server,
            "GET",
            format!("{LEASES}/nco"),
            500,
            failure(500, "InternalError"),
        )
        .await;
        let mut elector = elector(&server);
        elector.leading = true;

        let started = Instant::now();
        elector.hold(&CancellationToken::new()).await;
        assert!(!elector.leading());
        assert!(started.elapsed() >= Duration::from_millis(40));
    }

    #[tokio::test]
    async fn a_lease_someone_else_holds_is_not_released() {
        let server = MockServer::start().await;
        mock(
            &server,
            "GET",
            format!("{LEASES}/nco"),
            200,
            lease(Some("pod-b")),
        )
        .await;
        let mut elector = elector(&server);
        elector.leading = true;
        elector.release().await;
        assert_eq!(writes(&server).await.len(), 0);
    }
}
