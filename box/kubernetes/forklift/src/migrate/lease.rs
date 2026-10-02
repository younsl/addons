//! Keeps forklift from writing while a migration runs: the migration holds
//! the HA Lease, so a replica that starts meanwhile stays a standby, and no
//! forklift pod may be left when copying starts.
//!
//! Holding the Lease alone is not enough. A leader releases it on shutdown
//! before its final snapshot flush finishes, so only the pod being gone proves
//! the flush is done.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams};
use tokio_util::sync::CancellationToken;

use crate::cluster::Elector;
use crate::config::HAConfig;

pub struct LeaseHold {
    cancel: CancellationToken,
    lost: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl LeaseHold {
    pub async fn acquire(cfg: HAConfig, wait: Duration) -> Result<LeaseHold, String> {
        let name = cfg.lease_name.clone();
        let elector = Elector::new(cfg).map_err(|e| format!("lease {name}: {e}"))?;
        let cancel = CancellationToken::new();
        let lost = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn({
            let lost = Arc::clone(&lost);
            elector.run(
                cancel.clone(),
                move |_| {
                    let _ = tx.send(true);
                },
                move || lost.store(true, Ordering::SeqCst),
            )
        });
        let acquired = tokio::time::timeout(wait, rx.wait_for(|held| *held)).await;
        if !matches!(acquired, Ok(Ok(_))) {
            cancel.cancel();
            let _ = task.await;
            return Err(format!(
                "lease {name} still held by another instance after {}s; stop every forklift replica",
                wait.as_secs()
            ));
        }
        Ok(LeaseHold { cancel, lost, task })
    }

    pub fn held(&self) -> bool {
        !self.lost.load(Ordering::SeqCst)
    }

    pub fn watch(&self) -> Box<dyn Fn() -> bool + Send + Sync> {
        let lost = Arc::clone(&self.lost);
        Box::new(move || !lost.load(Ordering::SeqCst))
    }

    pub async fn release(self) {
        self.cancel.cancel();
        let _ = self.task.await;
    }
}

pub fn in_cluster_client() -> Result<kube::Client, String> {
    let cfg = kube::Config::incluster().map_err(|e| format!("in-cluster config: {e}"))?;
    kube::Client::try_from(cfg).map_err(|e| format!("kubernetes client: {e}"))
}

/// Waits for every pod matching `selector` to be gone, returning the ones
/// still present after `wait`.
pub async fn wait_for_no_writers(
    client: &kube::Client,
    namespace: &str,
    selector: &str,
    wait: Duration,
) -> Result<Vec<String>, String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let list = pods
            .list(&ListParams::default().labels(selector))
            .await
            .map_err(|e| format!("list pods {selector}: {e}"))?;
        let left = live_pods(&list.items);
        if left.is_empty() || tokio::time::Instant::now() >= deadline {
            return Ok(left);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Finished pods no longer run forklift; a terminating one may still be
/// flushing its final snapshot.
pub(crate) fn live_pods(pods: &[Pod]) -> Vec<String> {
    pods.iter()
        .filter(|p| {
            let phase = p.status.as_ref().and_then(|s| s.phase.as_deref());
            !matches!(phase, Some("Succeeded" | "Failed"))
        })
        .filter_map(|p| p.metadata.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::core::v1::PodStatus;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use crate::migrate::lease::*;

    fn pod(name: &str, phase: &str) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some(name.into()),
                ..Default::default()
            },
            status: Some(PodStatus {
                phase: Some(phase.into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn finished_pods_are_not_writers() {
        let pods = [
            pod("a", "Running"),
            pod("b", "Succeeded"),
            pod("c", "Pending"),
            pod("d", "Failed"),
        ];
        assert_eq!(live_pods(&pods), vec!["a", "c"]);
    }
}
