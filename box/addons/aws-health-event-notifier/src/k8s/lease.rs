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

use crate::k8s::error::{K8sError, K8sResult, conflict_or};

pub struct LeaseLock {
    api: Api<Lease>,
    name: String,
    identity: String,
    duration: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcquireReason {
    /// No Lease existed.
    Created,
    /// The previous holder gave it up on shutdown.
    Released,
    /// The previous holder stopped renewing.
    Expired,
}

impl AcquireReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Released => "released",
            Self::Expired => "expired",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum LeaseState {
    Renewed,
    Acquired {
        previous: Option<String>,
        reason: AcquireReason,
    },
    /// Another replica holds the Lease. `None` when it just won a concurrent
    /// write and is not known yet.
    HeldBy(Option<String>),
}

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Renew,
    Take {
        previous: Option<String>,
        reason: AcquireReason,
    },
    Busy(String),
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

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Acquires the Lease or renews it, reporting who holds it otherwise.
    pub async fn try_acquire_or_renew(&self) -> K8sResult<LeaseState> {
        let now = Timestamp::now();
        let Some(current) = self.api.get_opt(&self.name).await? else {
            return self.create(now).await;
        };
        let spec = current.spec.clone().unwrap_or_default();
        let (next, state) = match decide(&spec, &self.identity, now) {
            Decision::Busy(holder) => return Ok(LeaseState::HeldBy(Some(holder))),
            Decision::Renew => (
                LeaseSpec {
                    renew_time: Some(MicroTime(now)),
                    lease_duration_seconds: Some(self.duration_secs()),
                    ..spec
                },
                LeaseState::Renewed,
            ),
            Decision::Take { previous, reason } => (
                LeaseSpec {
                    holder_identity: Some(self.identity.clone()),
                    acquire_time: Some(MicroTime(now)),
                    renew_time: Some(MicroTime(now)),
                    lease_duration_seconds: Some(self.duration_secs()),
                    lease_transitions: Some(spec.lease_transitions.unwrap_or(0) + 1),
                    ..spec
                },
                LeaseState::Acquired { previous, reason },
            ),
        };
        self.replace(current, next, state).await
    }

    /// Gives the Lease up so a standby takes over without waiting for expiry.
    /// The holder stays recorded so the next leader can log who released it.
    pub async fn release(&self) -> K8sResult<()> {
        let Some(current) = self.api.get_opt(&self.name).await? else {
            return Ok(());
        };
        let spec = current.spec.clone().unwrap_or_default();
        if spec.holder_identity.as_deref() != Some(self.identity.as_str()) {
            return Ok(());
        }
        let next = LeaseSpec {
            acquire_time: None,
            renew_time: None,
            ..spec
        };
        self.replace(current, next, LeaseState::Renewed)
            .await
            .map(|_| ())
    }

    async fn create(&self, now: Timestamp) -> K8sResult<LeaseState> {
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
            Ok(_) => Ok(LeaseState::Acquired {
                previous: None,
                reason: AcquireReason::Created,
            }),
            Err(e) => lost_race(conflict_or(e, &self.name)),
        }
    }

    async fn replace(
        &self,
        mut current: Lease,
        spec: LeaseSpec,
        on_success: LeaseState,
    ) -> K8sResult<LeaseState> {
        current.spec = Some(spec);
        match self
            .api
            .replace(&self.name, &PostParams::default(), &current)
            .await
        {
            Ok(_) => Ok(on_success),
            Err(e) => lost_race(conflict_or(e, &self.name)),
        }
    }

    fn duration_secs(&self) -> i32 {
        i32::try_from(self.duration.as_secs()).unwrap_or(i32::MAX)
    }
}

fn lost_race(e: K8sError) -> K8sResult<LeaseState> {
    match e {
        K8sError::Conflict(_) => Ok(LeaseState::HeldBy(None)),
        other @ K8sError::Api(_) => Err(other),
    }
}

fn decide(spec: &LeaseSpec, identity: &str, now: Timestamp) -> Decision {
    match spec.holder_identity.as_deref() {
        Some(holder) if holder == identity => Decision::Renew,
        None | Some("") => Decision::Take {
            previous: None,
            reason: AcquireReason::Released,
        },
        Some(holder) => {
            let previous = Some(holder.to_string());
            let Some(renewed) = spec.renew_time.as_ref() else {
                return Decision::Take {
                    previous,
                    reason: AcquireReason::Released,
                };
            };
            let duration =
                SignedDuration::from_secs(i64::from(spec.lease_duration_seconds.unwrap_or(0)));
            if renewed.0 + duration < now {
                Decision::Take {
                    previous,
                    reason: AcquireReason::Expired,
                }
            } else {
                Decision::Busy(holder.to_string())
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

    fn take(previous: Option<&str>, reason: AcquireReason) -> Decision {
        Decision::Take {
            previous: previous.map(str::to_string),
            reason,
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
            Decision::Busy("b".into())
        );
    }

    #[test]
    fn expired_foreign_lease_is_taken() {
        assert_eq!(
            decide(&spec(Some("b"), 30, 15), "a", Timestamp::now()),
            take(Some("b"), AcquireReason::Expired)
        );
    }

    #[test]
    fn released_lease_is_taken() {
        assert_eq!(
            decide(&spec(None, 0, 15), "a", Timestamp::now()),
            take(None, AcquireReason::Released)
        );
        assert_eq!(
            decide(&spec(Some(""), 0, 15), "a", Timestamp::now()),
            take(None, AcquireReason::Released)
        );
    }

    #[test]
    fn lease_without_renew_time_is_released_by_its_holder() {
        let s = LeaseSpec {
            holder_identity: Some("b".into()),
            ..Default::default()
        };
        assert_eq!(
            decide(&s, "a", Timestamp::now()),
            take(Some("b"), AcquireReason::Released)
        );
    }

    #[test]
    fn reasons_render_lowercase() {
        assert_eq!(AcquireReason::Created.as_str(), "created");
        assert_eq!(AcquireReason::Released.as_str(), "released");
        assert_eq!(AcquireReason::Expired.as_str(), "expired");
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

    fn acquired(previous: Option<&str>, reason: AcquireReason) -> LeaseState {
        LeaseState::Acquired {
            previous: previous.map(str::to_string),
            reason,
        }
    }

    #[tokio::test]
    async fn first_replica_creates_and_second_waits() {
        let server = FakeApiServer::start().await;
        let a = lock(&server, "a");
        let b = lock(&server, "b");
        assert_eq!(a.identity(), "a");
        assert_eq!(a.name(), "leader");
        assert_eq!(
            a.try_acquire_or_renew().await.unwrap(),
            acquired(None, AcquireReason::Created)
        );
        assert_eq!(
            b.try_acquire_or_renew().await.unwrap(),
            LeaseState::HeldBy(Some("a".into()))
        );
        assert_eq!(a.try_acquire_or_renew().await.unwrap(), LeaseState::Renewed);
    }

    #[tokio::test]
    async fn release_hands_over_with_previous_holder() {
        let server = FakeApiServer::start().await;
        let a = lock(&server, "a");
        let b = lock(&server, "b");
        a.try_acquire_or_renew().await.unwrap();
        b.release().await.unwrap();
        assert!(matches!(
            b.try_acquire_or_renew().await.unwrap(),
            LeaseState::HeldBy(_)
        ));
        a.release().await.unwrap();
        assert_eq!(
            b.try_acquire_or_renew().await.unwrap(),
            acquired(Some("a"), AcquireReason::Released)
        );
        assert_eq!(
            a.try_acquire_or_renew().await.unwrap(),
            LeaseState::HeldBy(Some("b".into()))
        );
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
        let won = |s: &LeaseState| matches!(s, LeaseState::Acquired { .. });
        assert!(won(&ra.unwrap()) ^ won(&rb.unwrap()));
    }

    #[tokio::test]
    async fn api_error_is_surfaced() {
        let server = FakeApiServer::start().await;
        server.fail_all();
        assert!(lock(&server, "a").try_acquire_or_renew().await.is_err());
    }
}
