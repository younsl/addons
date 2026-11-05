//! Reads Argo Rollout resources straight from the Kubernetes API.
//!
//! Everything the verdict needs (`status.stableRS`, `status.currentPodHash`,
//! `status.pauseConditions`, `status.abort`) is state the Rollout controller
//! already writes back to the CR, so no Argo Rollouts API is involved.

use async_trait::async_trait;
use kube::api::{Api, ApiResource, DynamicObject, GroupVersionKind, ListParams};
use serde_json::Value;
use thiserror::Error;

use super::application::{nested, nested_str};
use crate::gate::RolloutSnapshot;

/// Why a read against the Kubernetes API failed.
#[derive(Debug, Error)]
pub enum ReadError {
    #[error("list rollouts with selector {selector}: {source}")]
    List {
        selector: String,
        #[source]
        source: kube::Error,
    },
}

/// Lists Rollout snapshots. The Kubernetes implementation is the only one in
/// production. The trait exists so the engine and the HTTP handler can be
/// exercised without a cluster.
#[async_trait]
pub trait RolloutReader: Send + Sync {
    /// Returns every Rollout matching `label_selector`. An empty `namespace`
    /// searches the whole cluster.
    async fn list(
        &self,
        namespace: &str,
        label_selector: &str,
    ) -> Result<Vec<RolloutSnapshot>, ReadError>;
}

/// The Argo Rollout resource.
#[must_use]
pub fn rollout_resource() -> ApiResource {
    ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk("argoproj.io", "v1alpha1", "Rollout"),
        "rollouts",
    )
}

/// The Kubernetes-backed [`RolloutReader`].
pub struct KubeRolloutReader {
    client: kube::Client,
}

impl KubeRolloutReader {
    #[must_use]
    pub const fn new(client: kube::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl RolloutReader for KubeRolloutReader {
    async fn list(
        &self,
        namespace: &str,
        label_selector: &str,
    ) -> Result<Vec<RolloutSnapshot>, ReadError> {
        let api: Api<DynamicObject> = if namespace.is_empty() {
            Api::all_with(self.client.clone(), &rollout_resource())
        } else {
            Api::namespaced_with(self.client.clone(), namespace, &rollout_resource())
        };
        let params = ListParams::default().labels(label_selector);
        let list = api.list(&params).await.map_err(|source| ReadError::List {
            selector: label_selector.to_string(),
            source,
        })?;
        Ok(list
            .items
            .iter()
            .filter_map(|obj| {
                let value = serde_json::to_value(obj).ok()?;
                // One malformed Rollout must not hide the rest.
                rollout_from_value(&value)
            })
            .collect())
    }
}

/// Reduces a Rollout document to the fields the gate reasons about, or `None`
/// when the document has no name.
#[must_use]
pub fn rollout_from_value(obj: &Value) -> Option<RolloutSnapshot> {
    let name = nested_str(obj, &["metadata", "name"]);
    if name.is_empty() {
        return None;
    }

    let (strategy, total_steps) = strategy_of(obj);

    Some(RolloutSnapshot {
        name: name.to_string(),
        namespace: nested_str(obj, &["metadata", "namespace"]).to_string(),
        strategy,
        phase: nested_str(obj, &["status", "phase"]).to_string(),
        stable_hash: nested_str(obj, &["status", "stableRS"]).to_string(),
        current_hash: nested_str(obj, &["status", "currentPodHash"]).to_string(),
        current_step: nested(obj, &["status", "currentStepIndex"]).and_then(Value::as_i64),
        total_steps,
        paused: nested(obj, &["status", "pauseConditions"])
            .and_then(Value::as_array)
            .is_some_and(|conditions| !conditions.is_empty()),
        aborted: nested(obj, &["status", "abort"])
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// The strategy name and, for a canary, how many steps it declares.
fn strategy_of(obj: &Value) -> (String, usize) {
    if let Some(canary) = nested(obj, &["spec", "strategy", "canary"]) {
        let steps = canary
            .get("steps")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        return ("canary".to_string(), steps);
    }
    if nested(obj, &["spec", "strategy", "blueGreen"]).is_some() {
        return ("blueGreen".to_string(), 0);
    }
    (String::new(), 0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn rollout_reads_every_field() {
        let obj = json!({
            "metadata": {"name": "payment-api", "namespace": "payments"},
            "spec": {"strategy": {"canary": {"steps": [
                {"setWeight": 10}, {"pause": {}}, {"setWeight": 50}, {"pause": {}}
            ]}}},
            "status": {
                "phase": "Paused",
                "stableRS": "aaa",
                "currentPodHash": "bbb",
                "currentStepIndex": 1,
                "pauseConditions": [{"reason": "CanaryPauseStep"}],
                "abort": false
            }
        });
        let snap = rollout_from_value(&obj).unwrap();
        assert_eq!(snap.name, "payment-api");
        assert_eq!(snap.namespace, "payments");
        assert_eq!(snap.strategy, "canary");
        assert_eq!(snap.phase, "Paused");
        assert_eq!(snap.stable_hash, "aaa");
        assert_eq!(snap.current_hash, "bbb");
        assert_eq!(snap.current_step, Some(1));
        assert_eq!(snap.total_steps, 4);
        assert!(snap.paused);
        assert!(!snap.aborted);
        assert!(snap.in_progress());
    }

    #[test]
    fn rollout_defaults_and_strategies() {
        assert!(rollout_from_value(&json!({"spec": {}})).is_none());

        let bare = rollout_from_value(&json!({"metadata": {"name": "r"}})).unwrap();
        assert_eq!(bare.strategy, "");
        assert_eq!(bare.total_steps, 0);
        assert!(bare.current_step.is_none());
        assert!(!bare.paused);
        assert!(!bare.aborted);
        assert!(!bare.in_progress());

        let blue_green = rollout_from_value(&json!({
            "metadata": {"name": "r"},
            "spec": {"strategy": {"blueGreen": {"activeService": "svc"}}},
            "status": {"abort": true}
        }))
        .unwrap();
        assert_eq!(blue_green.strategy, "blueGreen");
        assert!(blue_green.aborted);

        let empty_pause = rollout_from_value(&json!({
            "metadata": {"name": "r"},
            "spec": {"strategy": {"canary": {}}},
            "status": {"pauseConditions": []}
        }))
        .unwrap();
        assert!(!empty_pause.paused);
        assert_eq!(empty_pause.total_steps, 0);
    }

    #[test]
    fn rollout_resource_targets_argoproj() {
        let ar = rollout_resource();
        assert_eq!(ar.group, "argoproj.io");
        assert_eq!(ar.version, "v1alpha1");
        assert_eq!(ar.plural, "rollouts");
        assert_eq!(ar.kind, "Rollout");
    }

    #[test]
    fn read_error_names_the_selector() {
        let err = ReadError::List {
            selector: "argocd.argoproj.io/instance=prd-api".to_string(),
            source: kube::Error::Api(Box::new(kube::core::Status::failure(
                "forbidden",
                "Forbidden",
            ))),
        };
        assert!(err.to_string().contains("instance=prd-api"));
    }
}
