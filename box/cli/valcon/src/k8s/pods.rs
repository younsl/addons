use k8s_openapi::api::core::v1::Pod;
use kube::api::ListParams;
use kube::{Api, Client};

use super::error::K8sError;
use crate::types::PodRef;

const DEFAULT_CONTAINER_ANNOTATION: &str = "kubectl.kubernetes.io/default-container";
const HASH_LABELS: [&str; 2] = ["pod-template-hash", "rollouts-pod-template-hash"];

/// Which namespaces to scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    All,
    Namespaces(Vec<String>),
}

pub async fn list_running(
    client: &Client,
    scope: &Scope,
    selector: Option<&str>,
) -> Result<Vec<PodRef>, K8sError> {
    let mut params = ListParams::default().fields("status.phase=Running");
    if let Some(sel) = selector {
        params = params.labels(sel);
    }
    let apis: Vec<Api<Pod>> = match scope {
        Scope::All => vec![Api::all(client.clone())],
        Scope::Namespaces(list) => list
            .iter()
            .map(|ns| Api::namespaced(client.clone(), ns))
            .collect(),
    };
    let mut pods = Vec::new();
    for api in apis {
        pods.extend(api.list(&params).await?.items.iter().filter_map(to_ref));
    }
    pods.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Ok(pods)
}

fn to_ref(pod: &Pod) -> Option<PodRef> {
    let name = pod.metadata.name.clone()?;
    Some(PodRef {
        namespace: pod.metadata.namespace.clone().unwrap_or_default(),
        app: app_name(pod, &name),
        deleting: pod.metadata.deletion_timestamp.is_some(),
        containers: container_order(pod),
        probe: super::probe::running(pod).is_some(),
        name,
    })
}

// Containers share the pod network namespace, so any container that has `cat` sees every socket.
fn container_order(pod: &Pod) -> Vec<String> {
    let mut names: Vec<String> = pod
        .spec
        .as_ref()
        .map_or_default(|s| s.containers.iter().map(|c| c.name.clone()).collect());
    let preferred = pod
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(DEFAULT_CONTAINER_ANNOTATION));
    if let Some(pos) = preferred.and_then(|p| names.iter().position(|n| n == p)) {
        let first = names.remove(pos);
        names.insert(0, first);
    }
    names
}

fn app_name(pod: &Pod, pod_name: &str) -> String {
    let owner = pod
        .metadata
        .owner_references
        .as_ref()
        .and_then(|refs| refs.iter().find(|r| r.controller == Some(true)));

    let Some(owner) = owner else {
        return strip_random_suffix(pod_name).to_owned();
    };
    if owner.kind != "ReplicaSet" {
        return owner.name.clone();
    }
    let labels = pod.metadata.labels.as_ref();
    HASH_LABELS
        .iter()
        .filter_map(|key| labels.and_then(|l| l.get(*key)))
        .find_map(|hash| owner.name.strip_suffix(&format!("-{hash}")))
        .map_or_else(
            || strip_random_suffix(&owner.name).to_owned(),
            str::to_owned,
        )
}

fn strip_random_suffix(name: &str) -> &str {
    match name.rsplit_once('-') {
        Some((base, suffix))
            if !base.is_empty()
                && (5..=10).contains(&suffix.len())
                && suffix
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) =>
        {
            base
        }
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;

    use super::*;

    fn pod(name: &str, owner: Option<(&str, &str)>, hash: Option<&str>) -> Pod {
        let mut p = Pod::default();
        p.metadata.name = Some(name.into());
        p.metadata.owner_references = owner.map(|(kind, n)| {
            vec![OwnerReference {
                kind: kind.into(),
                name: n.into(),
                controller: Some(true),
                ..Default::default()
            }]
        });
        p.metadata.labels =
            hash.map(|h| BTreeMap::from([("pod-template-hash".to_owned(), h.to_owned())]));
        p
    }

    #[test]
    fn replicaset_uses_template_hash() {
        let p = pod(
            "checkout-1-6499448f66-krggx",
            Some(("ReplicaSet", "checkout-1-6499448f66")),
            Some("6499448f66"),
        );
        assert_eq!(app_name(&p, "checkout-1-6499448f66-krggx"), "checkout-1");
    }

    #[test]
    fn statefulset_uses_owner_name() {
        let p = pod(
            "ci-controller-0",
            Some(("StatefulSet", "ci-controller")),
            None,
        );
        assert_eq!(app_name(&p, "ci-controller-0"), "ci-controller");
    }

    #[test]
    fn default_container_goes_first() {
        use k8s_openapi::api::core::v1::{Container, PodSpec};
        let mut p = pod("x", None, None);
        p.spec = Some(PodSpec {
            containers: ["sidecar", "app"]
                .iter()
                .map(|n| Container {
                    name: (*n).into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        });
        p.metadata.annotations = Some(BTreeMap::from([(
            DEFAULT_CONTAINER_ANNOTATION.to_owned(),
            "app".to_owned(),
        )]));
        p.metadata.namespace = Some("shop".into());
        let t = to_ref(&p).unwrap();
        assert_eq!(t.namespace, "shop");
        assert_eq!(t.containers, ["app", "sidecar"]);
        assert!(!t.deleting);
        assert!(to_ref(&Pod::default()).is_none());
    }

    #[test]
    fn replicaset_without_hash_label_strips_suffix() {
        let p = pod(
            "web-5d8f7c9b6d-abcde",
            Some(("ReplicaSet", "web-5d8f7c9b6d")),
            None,
        );
        assert_eq!(app_name(&p, "web-5d8f7c9b6d-abcde"), "web");
    }

    #[test]
    fn bare_pod_strips_random_suffix() {
        let p = pod("ci-agent-3b76m", None, None);
        assert_eq!(app_name(&p, "ci-agent-3b76m"), "ci-agent");
        let p = pod("debug", None, None);
        assert_eq!(app_name(&p, "debug"), "debug");
    }
}
