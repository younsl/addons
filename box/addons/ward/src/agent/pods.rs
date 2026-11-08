//! Maps container IDs to the Pods on this node, so detections name the
//! workload and Events land on the right Pod.

use futures::StreamExt as _;
use k8s_openapi::api::core::v1::Pod;
use kube::api::Api;
use kube::runtime::WatchStreamExt as _;
use kube::runtime::reflector::{self, Store};
use kube::runtime::watcher;

use crate::policy::Workload;

#[derive(Clone)]
pub struct PodIndex {
    store: Option<Store<Pod>>,
}

/// The workload a container ID belongs to among `pods`.
pub fn find<'a>(pods: impl IntoIterator<Item = &'a Pod>, container_id: &str) -> Option<Workload> {
    for pod in pods {
        let Some(status) = &pod.status else { continue };
        let all = status
            .container_statuses
            .iter()
            .flatten()
            .chain(status.init_container_statuses.iter().flatten())
            .chain(status.ephemeral_container_statuses.iter().flatten());
        for c in all {
            if c.container_id
                .as_deref()
                .is_some_and(|id| id.rsplit("://").next() == Some(container_id))
            {
                return Some(Workload {
                    namespace: pod.metadata.namespace.clone().unwrap_or_default(),
                    pod: pod.metadata.name.clone().unwrap_or_default(),
                    pod_uid: pod.metadata.uid.clone(),
                    container: c.name.clone(),
                    container_id: container_id.to_string(),
                    image: Some(c.image.clone()),
                });
            }
        }
    }
    None
}

impl PodIndex {
    pub const fn empty() -> Self {
        Self { store: None }
    }

    /// Watches the Pods scheduled to `node`.
    pub fn start(client: kube::Client, node: &str) -> Self {
        let api: Api<Pod> = Api::all(client);
        let cfg = watcher::Config::default().fields(&format!("spec.nodeName={node}"));
        let (reader, writer) = reflector::store();
        let stream = reflector::reflector(writer, watcher(api, cfg))
            .default_backoff()
            .applied_objects();
        tokio::spawn(async move {
            futures::pin_mut!(stream);
            while let Some(item) = stream.next().await {
                if let Err(e) = item {
                    tracing::debug!(error = %e, "pod watch");
                }
            }
        });
        Self {
            store: Some(reader),
        }
    }

    pub fn lookup(&self, container_id: &str) -> Option<Workload> {
        let store = self.store.as_ref()?;
        let pods = store.state();
        find(pods.iter().map(AsRef::as_ref), container_id)
    }
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::core::v1::{ContainerStatus, PodStatus};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;

    #[test]
    fn finds_containers_by_runtime_id() {
        let pod = Pod {
            metadata: ObjectMeta {
                name: Some("web-0".into()),
                namespace: Some("shop".into()),
                uid: Some("u1".into()),
                ..ObjectMeta::default()
            },
            status: Some(PodStatus {
                container_statuses: Some(vec![ContainerStatus {
                    name: "app".into(),
                    image: "nginx:1".into(),
                    container_id: Some("containerd://abc".into()),
                    ..ContainerStatus::default()
                }]),
                ..PodStatus::default()
            }),
            ..Pod::default()
        };
        let w = find([&pod], "abc").unwrap();
        assert_eq!(
            (w.namespace.as_str(), w.pod.as_str(), w.container.as_str()),
            ("shop", "web-0", "app")
        );
        assert_eq!(w.pod_uid.as_deref(), Some("u1"));
        assert!(find([&pod], "zzz").is_none());
        assert!(find([&Pod::default()], "abc").is_none());
        assert!(PodIndex::empty().lookup("abc").is_none());
    }
}
