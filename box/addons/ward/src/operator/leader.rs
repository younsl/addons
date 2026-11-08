//! Lease-based leader election for operator replicas. The leader renews
//! its Lease, and a replica that loses it shuts down so its replacement
//! starts as a follower.

use std::time::Duration;

use chrono::Utc;
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, Patch, PatchParams, PostParams};
use tokio::sync::watch;

use crate::k8s::{from_micro_time, micro_time};

pub const LEASE_NAME: &str = "ward-operator-leader";
const LEASE_SECONDS: i32 = 15;
const RETRY: Duration = Duration::from_secs(5);

/// Whether `me` may take or keep `lease` at `now`.
pub fn can_hold(lease: &Lease, me: &str, now: chrono::DateTime<Utc>) -> bool {
    let Some(spec) = &lease.spec else { return true };
    let holder = spec.holder_identity.as_deref().unwrap_or_default();
    if holder.is_empty() || holder == me {
        return true;
    }
    let renewed = spec.renew_time.as_ref().and_then(from_micro_time);
    let duration = i64::from(spec.lease_duration_seconds.unwrap_or(LEASE_SECONDS));
    renewed.is_none_or(|t| (now - t).num_seconds() > duration)
}

fn lease(me: &str, now: chrono::DateTime<Utc>) -> Lease {
    Lease {
        metadata: ObjectMeta {
            name: Some(LEASE_NAME.into()),
            ..ObjectMeta::default()
        },
        spec: Some(LeaseSpec {
            holder_identity: Some(me.into()),
            lease_duration_seconds: Some(LEASE_SECONDS),
            renew_time: Some(micro_time(now)),
            ..LeaseSpec::default()
        }),
    }
}

async fn try_hold(api: &Api<Lease>, me: &str) -> Result<bool, kube::Error> {
    let now = Utc::now();
    match api.get_opt(LEASE_NAME).await? {
        None => match api.create(&PostParams::default(), &lease(me, now)).await {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
            Err(e) => Err(e),
        },
        Some(current) if can_hold(&current, me, now) => {
            let mut next = lease(me, now);
            next.metadata.resource_version = current.metadata.resource_version;
            api.replace(LEASE_NAME, &PostParams::default(), &next)
                .await
                .map(|_| true)
                .or_else(|e| match e {
                    kube::Error::Api(ref a) if a.code == 409 => Ok(false),
                    e => Err(e),
                })
        }
        Some(_) => Ok(false),
    }
}

/// Waits until this replica leads, then renews in the background. Losing
/// the Lease ends the process.
pub async fn acquire(
    client: &kube::Client,
    namespace: &str,
    me: &str,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), kube::Error> {
    let api: Api<Lease> = Api::namespaced(client.clone(), namespace);
    loop {
        if try_hold(&api, me).await? {
            break;
        }
        tokio::select! {
            () = tokio::time::sleep(RETRY) => {}
            _ = shutdown.changed() => return Ok(()),
        }
    }
    tracing::info!(identity = me, "acquired operator leadership");
    let me = me.to_string();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = tokio::time::sleep(RETRY) => {}
                _ = shutdown.changed() => {
                    let _ = api.patch(LEASE_NAME, &PatchParams::default(), &Patch::Merge(serde_json::json!({"spec": {"holderIdentity": null}}))).await;
                    return;
                }
            }
            if !matches!(try_hold(&api, &me).await, Ok(true)) {
                tracing::error!("lost operator leadership, exiting");
                std::process::exit(1);
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takeover_rules() {
        let now = Utc::now();
        assert!(can_hold(&Lease::default(), "a", now));
        let held = lease("b", now);
        assert!(can_hold(&held, "b", now));
        assert!(!can_hold(&held, "a", now));
        assert!(can_hold(&held, "a", now + chrono::Duration::seconds(30)));
        let mut empty = held;
        empty.spec.as_mut().unwrap().holder_identity = None;
        assert!(can_hold(&empty, "a", now));
    }
}
