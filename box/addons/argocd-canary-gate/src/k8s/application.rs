//! Reduces an Argo CD Application document to the fields the gate reasons
//! about.
//!
//! The webhook gets the document inside `AdmissionReview.request.object`. The
//! UI extension preview has only a name, so [`KubeAppReader`] fetches the
//! Application from the API and both paths share the same parser, which is
//! what keeps the panel and the webhook from disagreeing about what an
//! Application says.

use async_trait::async_trait;
use kube::api::{Api, ApiResource, DynamicObject, GroupVersionKind};
use serde_json::Value;
use thiserror::Error;

use crate::gate::AppSnapshot;

/// Why an Application document could not be reduced to a snapshot.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SnapshotError {
    #[error("application has no metadata.name")]
    MissingName,
}

/// Why a read against the Kubernetes API failed.
#[derive(Debug, Error)]
pub enum AppReadError {
    #[error("get application {name}: {source}")]
    Get {
        name: String,
        #[source]
        source: kube::Error,
    },
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
}

/// Fetches one Application snapshot by name. The Kubernetes implementation is
/// the only one in production. The trait exists so the extension API can be
/// exercised without a cluster.
#[async_trait]
pub trait AppReader: Send + Sync {
    /// Returns `Ok(None)` when the Application does not exist.
    async fn get(&self, name: &str) -> Result<Option<AppSnapshot>, AppReadError>;
}

/// The Argo CD Application resource.
#[must_use]
pub fn application_resource() -> ApiResource {
    ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk("argoproj.io", "v1alpha1", "Application"),
        "applications",
    )
}

/// The Kubernetes-backed [`AppReader`].
pub struct KubeAppReader {
    api: Api<DynamicObject>,
    skip_annotation: String,
}

impl KubeAppReader {
    /// Scopes a reader to the namespace holding the Applications.
    #[must_use]
    pub fn new(client: kube::Client, namespace: &str, skip_annotation: &str) -> Self {
        Self {
            api: Api::namespaced_with(client, namespace, &application_resource()),
            skip_annotation: skip_annotation.to_string(),
        }
    }
}

#[async_trait]
impl AppReader for KubeAppReader {
    async fn get(&self, name: &str) -> Result<Option<AppSnapshot>, AppReadError> {
        match self.api.get(name).await {
            Ok(obj) => {
                let value = serde_json::to_value(&obj).unwrap_or(Value::Null);
                Ok(Some(snapshot_from_value(&value, &self.skip_annotation)?))
            }
            Err(kube::Error::Api(status)) if status.code == 404 => Ok(None),
            Err(source) => Err(AppReadError::Get {
                name: name.to_string(),
                source,
            }),
        }
    }
}

/// Reduces an Application document to a snapshot.
pub fn snapshot_from_value(
    obj: &Value,
    skip_annotation: &str,
) -> Result<AppSnapshot, SnapshotError> {
    let name = nested_str(obj, &["metadata", "name"]);
    if name.is_empty() {
        return Err(SnapshotError::MissingName);
    }

    Ok(AppSnapshot {
        name: name.to_string(),
        dest_namespace: nested_str(obj, &["spec", "destination", "namespace"]).to_string(),
        skip_requested: skip_requested(obj, skip_annotation),
    })
}

fn skip_requested(obj: &Value, annotation: &str) -> bool {
    nested(obj, &["metadata", "annotations", annotation])
        .and_then(Value::as_str)
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

/// Walks a decoded document and returns the value at `path`.
pub fn nested<'a>(obj: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(obj, |current, key| current.get(key))
}

/// Walks a decoded document and returns the string at `path`, or `""`.
pub fn nested_str<'a>(obj: &'a Value, path: &[&str]) -> &'a str {
    nested(obj, path)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const SKIP: &str = "canary-gate.younsl.github.io/skip";

    #[test]
    fn snapshot_reads_every_field() {
        let obj = json!({
            "metadata": {"name": "prd-api", "annotations": {SKIP: " TRUE "}},
            "spec": {"destination": {"namespace": "payments"}}
        });
        let snap = snapshot_from_value(&obj, SKIP).unwrap();
        assert_eq!(snap.name, "prd-api");
        assert_eq!(snap.dest_namespace, "payments");
        assert!(snap.skip_requested);
    }

    #[test]
    fn snapshot_defaults_and_errors() {
        let err = snapshot_from_value(&json!({"spec": {}}), SKIP).unwrap_err();
        assert_eq!(err, SnapshotError::MissingName);

        let snap = snapshot_from_value(&json!({"metadata": {"name": "api"}}), SKIP).unwrap();
        assert_eq!(snap.dest_namespace, "");
        assert!(!snap.skip_requested);

        let obj = json!({"metadata": {"name": "api", "annotations": {SKIP: "yes"}}});
        assert!(!snapshot_from_value(&obj, SKIP).unwrap().skip_requested);
    }

    #[test]
    fn application_resource_targets_argoproj() {
        let ar = application_resource();
        assert_eq!(ar.group, "argoproj.io");
        assert_eq!(ar.version, "v1alpha1");
        assert_eq!(ar.plural, "applications");
        assert_eq!(ar.kind, "Application");
    }

    #[test]
    fn read_error_messages_name_the_operation() {
        let err: AppReadError = SnapshotError::MissingName.into();
        assert_eq!(err.to_string(), "application has no metadata.name");
    }

    #[test]
    fn nested_helpers_walk_and_default() {
        let obj = json!({"a": {"b": "c"}});
        assert_eq!(nested_str(&obj, &["a", "b"]), "c");
        assert_eq!(nested_str(&obj, &["a", "x"]), "");
        assert!(nested(&obj, &["a"]).is_some());
        assert!(nested(&obj, &["z", "b"]).is_none());
    }
}
