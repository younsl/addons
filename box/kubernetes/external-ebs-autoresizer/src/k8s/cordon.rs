//! Applies and lifts the protective cordon: `spec.unschedulable` on a Node
//! whose root filesystem is filling up, so the scheduler stops placing new
//! Pods on it while the volume grows. The addon marks every cordon it applies
//! with an annotation and only ever lifts a cordon carrying that mark, so a
//! cordon set by an operator, a drain, or another controller is never undone.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use k8s_openapi::api::core::v1::Node as K8sNode;
use kube::api::{ListParams, Patch, PatchParams};

use super::nodes::{PAGE_SIZE, instance_id_from_provider_id};

/// The annotation suffix that marks a cordon as the addon's own. Its value is
/// the RFC 3339 instant the cordon was applied.
pub const ANNOTATION_SUFFIX: &str = "protective-cordon";

/// The cordon state of one EC2-backed Node.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CordonNode {
    pub name: String,
    pub uid: String,
    /// `spec.unschedulable`, whoever set it.
    pub unschedulable: bool,
    /// The Node carries the addon's protective cordon mark.
    pub protective: bool,
}

/// The subset of Node operations the protective cordon depends on.
#[async_trait]
pub trait CordonApi: Send + Sync {
    /// Lists every EC2-backed Node keyed by its instance ID.
    async fn list(&self) -> Result<HashMap<String, CordonNode>, String>;
    /// Marks the Node unschedulable and records the protective mark.
    async fn cordon(&self, name: &str, at: DateTime<Utc>) -> Result<(), String>;
    /// Makes the Node schedulable again and drops the protective mark.
    async fn uncordon(&self, name: &str) -> Result<(), String>;
    /// Drops the protective mark only, for a Node someone else already
    /// uncordoned.
    async fn forget(&self, name: &str) -> Result<(), String>;
}

/// The kube-backed protective cordon client.
pub struct KubeCordon(kube::Api<K8sNode>);

impl KubeCordon {
    #[must_use]
    pub fn new(client: kube::Client) -> Self {
        Self(kube::Api::all(client))
    }

    async fn patch(&self, name: &str, patch: serde_json::Value) -> Result<(), String> {
        self.0
            .patch(name, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .map(|_| ())
            .map_err(|e| format!("patch node {name}: {e}"))
    }
}

#[async_trait]
impl CordonApi for KubeCordon {
    async fn list(&self) -> Result<HashMap<String, CordonNode>, String> {
        let mut out = HashMap::new();
        let mut params = ListParams::default().limit(PAGE_SIZE);
        loop {
            let page = self
                .0
                .list(&params)
                .await
                .map_err(|e| format!("list nodes: {e}"))?;
            out.extend(page.items.iter().filter_map(from_k8s_node));
            match page.metadata.continue_ {
                Some(token) if !token.is_empty() => params = params.continue_token(&token),
                _ => return Ok(out),
            }
        }
    }

    async fn cordon(&self, name: &str, at: DateTime<Utc>) -> Result<(), String> {
        self.patch(name, cordon_patch(at)).await
    }

    async fn uncordon(&self, name: &str) -> Result<(), String> {
        self.patch(name, uncordon_patch()).await
    }

    async fn forget(&self, name: &str) -> Result<(), String> {
        self.patch(name, forget_patch()).await
    }
}

/// The full annotation key of the protective cordon mark.
#[must_use]
pub fn annotation_key() -> String {
    crate::annotations::key(ANNOTATION_SUFFIX)
}

fn cordon_patch(at: DateTime<Utc>) -> serde_json::Value {
    serde_json::json!({
        "metadata": {"annotations": {annotation_key(): at.to_rfc3339_opts(SecondsFormat::Secs, true)}},
        "spec": {"unschedulable": true},
    })
}

fn uncordon_patch() -> serde_json::Value {
    serde_json::json!({
        "metadata": {"annotations": {annotation_key(): null}},
        "spec": {"unschedulable": null},
    })
}

fn forget_patch() -> serde_json::Value {
    serde_json::json!({"metadata": {"annotations": {annotation_key(): null}}})
}

/// Reduces a Node to its cordon state, or `None` when it is not EC2-backed
/// and so can never match a discovered instance.
fn from_k8s_node(n: &K8sNode) -> Option<(String, CordonNode)> {
    let spec = n.spec.as_ref();
    let instance_id = instance_id_from_provider_id(
        spec.and_then(|s| s.provider_id.as_deref())
            .unwrap_or_default(),
    );
    if instance_id.is_empty() {
        return None;
    }
    let node = CordonNode {
        name: n.metadata.name.clone().unwrap_or_default(),
        uid: n.metadata.uid.clone().unwrap_or_default(),
        unschedulable: spec.and_then(|s| s.unschedulable).unwrap_or(false),
        protective: n
            .metadata
            .annotations
            .as_ref()
            .is_some_and(|a| a.contains_key(&annotation_key())),
    };
    Some((instance_id, node))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use k8s_openapi::api::core::v1::NodeSpec;
    use kube::api::ObjectMeta;

    use super::*;

    fn node(provider_id: &str, unschedulable: Option<bool>, marked: bool) -> K8sNode {
        K8sNode {
            metadata: ObjectMeta {
                name: Some("ip-10-0-1-5".into()),
                uid: Some("u".into()),
                annotations: marked.then(|| {
                    BTreeMap::from([(annotation_key(), "2026-10-09T00:00:00Z".to_string())])
                }),
                ..ObjectMeta::default()
            },
            spec: Some(NodeSpec {
                provider_id: Some(provider_id.into()),
                unschedulable,
                ..NodeSpec::default()
            }),
            status: None,
        }
    }

    #[test]
    fn node_reduction() {
        let (id, n) = from_k8s_node(&node("aws:///ap-northeast-2a/i-1", Some(true), true)).unwrap();
        assert_eq!(id, "i-1");
        assert_eq!(
            n,
            CordonNode {
                name: "ip-10-0-1-5".into(),
                uid: "u".into(),
                unschedulable: true,
                protective: true,
            }
        );
        let (_, n) = from_k8s_node(&node("aws:///zone/i-2", None, false)).unwrap();
        assert!(!n.unschedulable);
        assert!(!n.protective);
        assert!(from_k8s_node(&node("aws:///zone/fargate-x", None, false)).is_none());
        assert!(from_k8s_node(&K8sNode::default()).is_none());
    }

    #[test]
    fn patch_shapes() {
        let at = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        assert_eq!(
            cordon_patch(at),
            serde_json::json!({
                "metadata": {"annotations": {"external-ebs-autoresizer/protective-cordon": "2023-11-14T22:13:20Z"}},
                "spec": {"unschedulable": true},
            })
        );
        assert_eq!(
            uncordon_patch(),
            serde_json::json!({
                "metadata": {"annotations": {"external-ebs-autoresizer/protective-cordon": null}},
                "spec": {"unschedulable": null},
            })
        );
        assert_eq!(
            forget_patch(),
            serde_json::json!({"metadata": {"annotations": {"external-ebs-autoresizer/protective-cordon": null}}})
        );
    }
}
