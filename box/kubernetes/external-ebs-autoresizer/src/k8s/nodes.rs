//! Reads in-cluster Kubernetes Nodes and writes annotations back to them. It
//! is the only place the addon touches Node objects, and it is deliberately
//! limited to list and annotate: the recommender publishes advice on the Node
//! and never taints, drains, or otherwise changes a Node's scheduling state.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::Node as K8sNode;
use kube::api::{ListParams, Patch, PatchParams};

use super::{annotation_patch, from_k8s_time};

/// Well-known Node labels the recommender reads instead of calling AWS. Both
/// are set by the AWS cloud provider and by Karpenter.
const INSTANCE_TYPE_LABEL: &str = "node.kubernetes.io/instance-type";
const ZONE_LABEL: &str = "topology.kubernetes.io/zone";

/// The providerID scheme of an EC2-backed Node
/// (`aws:///<availability-zone>/<instance-id>`).
const AWS_PROVIDER_PREFIX: &str = "aws://";

/// The page size of every list request, so a large cluster never issues one
/// unbounded request.
pub(crate) const PAGE_SIZE: u32 = 500;

/// The subset of a Kubernetes Node the recommender needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Node {
    pub name: String,
    /// Identifies the live Node object, so an Event recorded against it is
    /// associated with this instance of the Node rather than a recycled name.
    pub uid: String,
    /// The EC2 instance ID parsed from `spec.providerID`. Empty when the Node
    /// is not EC2-backed (e.g. Fargate) or has no providerID yet.
    pub instance_id: String,
    pub instance_type: String,
    pub zone: String,
    /// The Node's creation timestamp. It bounds how much history the node can
    /// possibly have.
    pub created_at: Option<DateTime<Utc>>,
    /// The Node's current annotation set, used to skip a patch when nothing
    /// changed.
    pub annotations: BTreeMap<String, String>,
}

/// The subset of Kubernetes Node operations the recommender depends on.
#[async_trait]
pub trait NodeApi: Send + Sync {
    /// Lists every Node matching `label_selector` (empty selects all).
    async fn list(&self, label_selector: &str) -> Result<Vec<Node>, String>;
    /// Writes `set` and deletes `remove` from the Node's annotations in one
    /// request.
    async fn annotate(
        &self,
        name: &str,
        set: &BTreeMap<String, String>,
        remove: &[String],
    ) -> Result<(), String>;
}

/// The kube-backed Node client.
pub struct KubeNodes(kube::Api<K8sNode>);

impl KubeNodes {
    #[must_use]
    pub fn new(client: kube::Client) -> Self {
        Self(kube::Api::all(client))
    }
}

#[async_trait]
impl NodeApi for KubeNodes {
    async fn list(&self, label_selector: &str) -> Result<Vec<Node>, String> {
        let mut out = Vec::new();
        let mut params = ListParams::default().limit(PAGE_SIZE);
        if !label_selector.is_empty() {
            params = params.labels(label_selector);
        }
        loop {
            let page = self
                .0
                .list(&params)
                .await
                .map_err(|e| format!("list nodes: {e}"))?;
            out.extend(page.items.iter().map(from_k8s_node));
            match page.metadata.continue_ {
                Some(token) if !token.is_empty() => params = params.continue_token(&token),
                _ => return Ok(out),
            }
        }
    }

    /// A merge patch is used rather than an update so concurrent writers (the
    /// cluster autoscaler, Karpenter, CSI drivers) never lose their own
    /// annotations to a stale resourceVersion.
    async fn annotate(
        &self,
        name: &str,
        set: &BTreeMap<String, String>,
        remove: &[String],
    ) -> Result<(), String> {
        let Some(patch) = annotation_patch(set, remove) else {
            return Ok(());
        };
        self.0
            .patch(name, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .map(|_| ())
            .map_err(|e| format!("patch node {name} annotations: {e}"))
    }
}

fn from_k8s_node(n: &K8sNode) -> Node {
    let labels = n.metadata.labels.clone().unwrap_or_default();
    Node {
        name: n.metadata.name.clone().unwrap_or_default(),
        uid: n.metadata.uid.clone().unwrap_or_default(),
        instance_id: instance_id_from_provider_id(
            n.spec
                .as_ref()
                .and_then(|s| s.provider_id.as_deref())
                .unwrap_or_default(),
        ),
        instance_type: labels.get(INSTANCE_TYPE_LABEL).cloned().unwrap_or_default(),
        zone: labels.get(ZONE_LABEL).cloned().unwrap_or_default(),
        created_at: n
            .metadata
            .creation_timestamp
            .as_ref()
            .and_then(from_k8s_time),
        annotations: n.metadata.annotations.clone().unwrap_or_default(),
    }
}

/// Extracts the EC2 instance ID from a Node providerID such as
/// `aws:///ap-northeast-2a/i-0abc123`. Returns an empty string for any other
/// scheme or shape, which callers treat as "not an EC2 node".
#[must_use]
pub fn instance_id_from_provider_id(provider_id: &str) -> String {
    let Some(rest) = provider_id.strip_prefix(AWS_PROVIDER_PREFIX) else {
        return String::new();
    };
    let last = rest
        .trim_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    if last.starts_with("i-") {
        last.to_string()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::core::v1::NodeSpec;
    use kube::api::ObjectMeta;

    use super::*;

    #[test]
    fn provider_id_parsing() {
        assert_eq!(
            instance_id_from_provider_id("aws:///ap-northeast-2a/i-0abc123"),
            "i-0abc123"
        );
        assert_eq!(instance_id_from_provider_id("aws://i-1"), "i-1");
        assert_eq!(
            instance_id_from_provider_id("aws:///zone/fargate-ip-10-0-0-1"),
            ""
        );
        assert_eq!(instance_id_from_provider_id("gce://project/zone/i-1"), "");
        assert_eq!(instance_id_from_provider_id(""), "");
        assert_eq!(instance_id_from_provider_id("aws:///"), "");
    }

    #[test]
    fn node_conversion() {
        let n = K8sNode {
            metadata: ObjectMeta {
                name: Some("ip-10-0-1-5".into()),
                uid: Some("u".into()),
                labels: Some(BTreeMap::from([
                    (INSTANCE_TYPE_LABEL.to_string(), "m5.large".to_string()),
                    (ZONE_LABEL.to_string(), "ap-northeast-2a".to_string()),
                ])),
                annotations: Some(BTreeMap::from([("a".to_string(), "b".to_string())])),
                creation_timestamp: Some(super::super::k8s_time(
                    DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
                )),
                ..ObjectMeta::default()
            },
            spec: Some(NodeSpec {
                provider_id: Some("aws:///ap-northeast-2a/i-1".into()),
                ..NodeSpec::default()
            }),
            status: None,
        };
        let node = from_k8s_node(&n);
        assert_eq!(node.name, "ip-10-0-1-5");
        assert_eq!(node.uid, "u");
        assert_eq!(node.instance_id, "i-1");
        assert_eq!(node.instance_type, "m5.large");
        assert_eq!(node.zone, "ap-northeast-2a");
        assert_eq!(node.created_at.unwrap().timestamp(), 1_700_000_000);
        assert_eq!(node.annotations["a"], "b");
        let bare = from_k8s_node(&K8sNode::default());
        assert!(bare.instance_id.is_empty());
        assert!(bare.created_at.is_none());
    }
}
