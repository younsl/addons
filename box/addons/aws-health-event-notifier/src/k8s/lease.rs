//! Leader election lock on a `coordination.k8s.io/v1` Lease.
//!
//! Every write is an optimistic replace on `resourceVersion`, so two replicas
//! racing for an expired Lease cannot both win. Expiry is judged against the
//! local clock, which only affects how fast a dead leader is replaced: the
//! ledger fencing term, not the Lease, is what keeps a stale leader from posting.

use std::time::Duration;

use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use k8s_openapi::jiff::{SignedDuration, Timestamp};
use kube::api::PostParams;
use kube::{Api, Client};

use crate::k8s::error::{K8sResult, conflict_or};

pub struct LeaseLock {
    api: Api<Lease>,
    name: String,
    identity: String,
    duration: Duration,
}

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Renew,
    Take,
    Busy,
}

impl LeaseLock {
    pub fn new(
        client: Client,
        namespace: &str,
        name: impl Into<String>,
        identity: impl Into<String>,
        duration: Duration,
    ) -> Self {
        Self {
            api: Api::namespaced(client, namespace),
            name: name.into(),
            identity: identity.into(),
            duration,
        }
    }

    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Acquires the Lease or renews it. `Ok(false)` means another replica
    /// holds it or won a concurrent write.
    pub async fn try_acquire_or_renew(&self) -> K8sResult<bool> {
        let now = Timestamp::now();
        let Some(current) = self.api.get_opt(&self.name).await? else {
            return self.create(now).await;
        };
        let spec = current.spec.clone().unwrap_or_default();
        let next = match decide(&spec, &self.identity, now) {
            Decision::Busy => return Ok(false),
            Decision::Renew => LeaseSpec {
                renew_time: Some(MicroTime(now)),
                lease_duration_seconds: Some(self.duration_secs()),
                ..spec
            },
            Decision::Take => LeaseSpec {
                holder_identity: Some(self.identity.clone()),
                acquire_time: Some(MicroTime(now)),
                renew_time: Some(MicroTime(now)),
                lease_duration_seconds: Some(self.duration_secs()),
                lease_transitions: Some(spec.lease_transitions.unwrap_or(0) + 1),
                ..spec
            },
        };
        self.replace(current, next).await
    }

    /// Gives the Lease up so a standby takes over without waiting for expiry.
    pub async fn release(&self) -> K8sResult<()> {
        let Some(current) = self.api.get_opt(&self.name).await? else {
            return Ok(());
        };
        let spec = current.spec.clone().unwrap_or_default();
        if spec.holder_identity.as_deref() != Some(self.identity.as_str()) {
            return Ok(());
        }
        let next = LeaseSpec {
            holder_identity: None,
            acquire_time: None,
            renew_time: None,
            ..spec
        };
        self.replace(current, next).await.map(|_| ())
    }

    async fn create(&self, now: Timestamp) -> K8sResult<bool> {
        let lease = Lease {
            metadata: ObjectMeta {
                name: Some(self.name.clone()),
                ..Default::default()
            },
            spec: Some(LeaseSpec {
                holder_identity: Some(self.identity.clone()),
                acquire_time: Some(MicroTime(now)),
                renew_time: Some(MicroTime(now)),
                lease_duration_seconds: Some(self.duration_secs()),
                lease_transitions: Some(0),
                ..Default::default()
            }),
        };
        match self.api.create(&PostParams::default(), &lease).await {
            Ok(_) => Ok(true),
            Err(e) => lost_race(conflict_or(e, &self.name)),
        }
    }

    async fn replace(&self, mut current: Lease, spec: LeaseSpec) -> K8sResult<bool> {
        current.spec = Some(spec);
        match self
            .api
            .replace(&self.name, &PostParams::default(), &current)
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => lost_race(conflict_or(e, &self.name)),
        }
    }

    fn duration_secs(&self) -> i32 {
        i32::try_from(self.duration.as_secs()).unwrap_or(i32::MAX)
    }
}

fn lost_race(e: crate::k8s::error::K8sError) -> K8sResult<bool> {
    match e {
        crate::k8s::error::K8sError::Conflict(_) => Ok(false),
        other @ crate::k8s::error::K8sError::Api(_) => Err(other),
    }
}

fn decide(spec: &LeaseSpec, identity: &str, now: Timestamp) -> Decision {
    match spec.holder_identity.as_deref() {
        Some(holder) if holder == identity => Decision::Renew,
        None | Some("") => Decision::Take,
        Some(_) => {
            let duration =
                SignedDuration::from_secs(i64::from(spec.lease_duration_seconds.unwrap_or(0)));
            let expired = spec
                .renew_time
                .as_ref()
                .is_none_or(|renewed| renewed.0 + duration < now);
            if expired {
                Decision::Take
            } else {
                Decision::Busy
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::fake::FakeApiServer;

    fn spec(holder: Option<&str>, renewed_secs_ago: i64, duration: i32) -> LeaseSpec {
        LeaseSpec {
            holder_identity: holder.map(str::to_string),
            renew_time: Some(MicroTime(
                Timestamp::now() - SignedDuration::from_secs(renewed_secs_ago),
            )),
            lease_duration_seconds: Some(duration),
            ..Default::default()
        }
    }

    #[test]
    fn own_lease_is_renewed() {
        assert_eq!(
            decide(&spec(Some("a"), 0, 15), "a", Timestamp::now()),
            Decision::Renew
        );
    }

    #[test]
    fn live_foreign_lease_is_busy() {
        assert_eq!(
            decide(&spec(Some("b"), 5, 15), "a", Timestamp::now()),
            Decision::Busy
        );
    }

    #[test]
    fn expired_foreign_lease_is_taken() {
        assert_eq!(
            decide(&spec(Some("b"), 30, 15), "a", Timestamp::now()),
            Decision::Take
        );
    }

    #[test]
    fn released_lease_is_taken() {
        assert_eq!(
            decide(&spec(None, 0, 15), "a", Timestamp::now()),
            Decision::Take
        );
        assert_eq!(
            decide(&spec(Some(""), 0, 15), "a", Timestamp::now()),
            Decision::Take
        );
    }

    #[test]
    fn lease_without_renew_time_is_taken() {
        let s = LeaseSpec {
            holder_identity: Some("b".into()),
            ..Default::default()
        };
        assert_eq!(decide(&s, "a", Timestamp::now()), Decision::Take);
    }

    fn lock(server: &FakeApiServer, identity: &str) -> LeaseLock {
        LeaseLock::new(
            server.client(),
            "ns",
            "leader",
            identity,
            Duration::from_secs(15),
        )
    }

    #[tokio::test]
    async fn first_replica_creates_and_second_waits() {
        let server = FakeApiServer::start().await;
        let a = lock(&server, "a");
        let b = lock(&server, "b");
        assert_eq!(a.identity(), "a");
        assert!(a.try_acquire_or_renew().await.unwrap());
        assert!(!b.try_acquire_or_renew().await.unwrap());
        assert!(a.try_acquire_or_renew().await.unwrap());
    }

    #[tokio::test]
    async fn release_hands_over_immediately() {
        let server = FakeApiServer::start().await;
        let a = lock(&server, "a");
        let b = lock(&server, "b");
        assert!(a.try_acquire_or_renew().await.unwrap());
        b.release().await.unwrap();
        assert!(!b.try_acquire_or_renew().await.unwrap());
        a.release().await.unwrap();
        assert!(b.try_acquire_or_renew().await.unwrap());
        assert!(!a.try_acquire_or_renew().await.unwrap());
    }

    #[tokio::test]
    async fn release_without_lease_is_noop() {
        let server = FakeApiServer::start().await;
        lock(&server, "a").release().await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_create_has_one_winner() {
        let server = FakeApiServer::start().await;
        let a = lock(&server, "a");
        let b = lock(&server, "b");
        let (ra, rb) = tokio::join!(a.try_acquire_or_renew(), b.try_acquire_or_renew());
        assert!(ra.unwrap() ^ rb.unwrap());
    }

    #[tokio::test]
    async fn api_error_is_surfaced() {
        let server = FakeApiServer::start().await;
        server.fail_all();
        assert!(lock(&server, "a").try_acquire_or_renew().await.is_err());
    }
}
