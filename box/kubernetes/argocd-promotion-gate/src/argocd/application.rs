//! Reads Application resources straight from the Kubernetes API.
//!
//! Everything the upstream check needs (`status.sync`, `status.health`,
//! `status.summary.images`) is live state the application controller already
//! writes back to the CR, so no Argo CD API call is involved and no API token
//! is required.

use async_trait::async_trait;
use kube::api::{Api, ApiResource, DynamicObject, GroupVersionKind, ListParams};
use serde_json::Value;
use thiserror::Error;

use crate::gate::{AppSnapshot, ImageRef, identity_of, parse_image};

/// Why an Application document could not be reduced to a snapshot.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SnapshotError {
    #[error("application has no metadata.name")]
    MissingName,
}

/// Why a read against the Kubernetes API failed.
#[derive(Debug, Error)]
pub enum ReadError {
    #[error("get application {name}: {source}")]
    Get {
        name: String,
        #[source]
        source: kube::Error,
    },
    #[error("list applications: {0}")]
    List(#[source] kube::Error),
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
}

/// Reads Application snapshots. The Kubernetes implementation is the only one
/// in production. The trait exists so the engine and the HTTP handlers can be
/// exercised without a cluster.
#[async_trait]
pub trait AppReader: Send + Sync {
    /// Fetches one Application. Returns `Ok(None)` when the Application does
    /// not exist, which the gate reads as "nothing upstream".
    async fn get(&self, name: &str) -> Result<Option<AppSnapshot>, ReadError>;

    /// Returns every Application in the namespace the reader is scoped to.
    async fn list(&self) -> Result<Vec<AppSnapshot>, ReadError>;
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
pub struct KubeReader {
    api: Api<DynamicObject>,
    skip_annotation: String,
}

impl KubeReader {
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
impl AppReader for KubeReader {
    async fn get(&self, name: &str) -> Result<Option<AppSnapshot>, ReadError> {
        match self.api.get(name).await {
            Ok(obj) => {
                let value = serde_json::to_value(&obj).unwrap_or(Value::Null);
                Ok(Some(snapshot_from_value(&value, &self.skip_annotation)?))
            }
            Err(kube::Error::Api(status)) if status.code == 404 => Ok(None),
            Err(source) => Err(ReadError::Get {
                name: name.to_string(),
                source,
            }),
        }
    }

    /// Used at startup to report what the exemptions currently cover. A gate
    /// whose escape hatch is invisible tends to end up with the hatch
    /// permanently open.
    async fn list(&self) -> Result<Vec<AppSnapshot>, ReadError> {
        let list = self
            .api
            .list(&ListParams::default())
            .await
            .map_err(ReadError::List)?;
        Ok(list
            .items
            .iter()
            .filter_map(|obj| {
                let value = serde_json::to_value(obj).ok()?;
                // One malformed Application must not hide the rest.
                snapshot_from_value(&value, &self.skip_annotation).ok()
            })
            .collect())
    }
}

/// Reduces an Application document to the fields the gate reasons about.
///
/// The same shape arrives from two places, the Kubernetes API and
/// `AdmissionReview.request.object`, so both share this parser and can never
/// disagree about what an Application says.
pub fn snapshot_from_value(
    obj: &Value,
    skip_annotation: &str,
) -> Result<AppSnapshot, SnapshotError> {
    let name = nested_str(obj, &["metadata", "name"]);
    if name.is_empty() {
        return Err(SnapshotError::MissingName);
    }

    let mut project = nested_str(obj, &["spec", "project"]);
    if project.is_empty() {
        project = "default";
    }

    Ok(AppSnapshot {
        name: name.to_string(),
        project: project.to_string(),
        identity: identity_of(name, project),
        sync_status: nested_str(obj, &["status", "sync", "status"]).to_string(),
        health_status: nested_str(obj, &["status", "health", "status"]).to_string(),
        live_images: parse_image_list(nested(obj, &["status", "summary", "images"])),
        skip_requested: skip_requested(obj, skip_annotation),
        pending_revision: nested_str(obj, &["operation", "sync", "revision"]).to_string(),
        current_revision: nested_str(obj, &["status", "sync", "revision"]).to_string(),
        deployed_revisions: deployed_revisions(obj),
    })
}

/// Reads `status.history`, which Argo CD keeps on the resource itself. Because
/// the Application CRD has no status subresource, an `AdmissionReview` carries
/// this too, so a rollback is recognisable from the admission request alone
/// with no extra API call.
fn deployed_revisions(obj: &Value) -> Vec<String> {
    let Some(history) = nested(obj, &["status", "history"]).and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(history.len());
    for record in history {
        if let Some(revision) = record.get("revision").and_then(Value::as_str)
            && !revision.is_empty()
        {
            out.push(revision.to_string());
        }
        // A multi-source application records one revision per source.
        if let Some(revisions) = record.get("revisions").and_then(Value::as_array) {
            out.extend(
                revisions
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|r| !r.is_empty())
                    .map(ToString::to_string),
            );
        }
    }
    out
}

fn skip_requested(obj: &Value, annotation: &str) -> bool {
    nested(obj, &["metadata", "annotations", annotation])
        .and_then(Value::as_str)
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn parse_image_list(raw: Option<&Value>) -> Vec<ImageRef> {
    raw.and_then(Value::as_array)
        .map_or_else(Vec::new, |items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|image| !image.trim().is_empty())
                .map(parse_image)
                .collect()
        })
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

    const SKIP: &str = "promotion-gate.younsl.github.io/skip";

    #[test]
    fn snapshot_reads_every_field() {
        let obj = json!({
            "metadata": {"name": "prd-api", "annotations": {SKIP: " TRUE "}},
            "spec": {"project": "prd"},
            "operation": {"sync": {"revision": "abc"}},
            "status": {
                "sync": {"status": "Synced", "revision": "def"},
                "health": {"status": "Healthy"},
                "summary": {"images": ["r/api:1", "  ", 3]},
                "history": [
                    {"revision": "abc"},
                    {"revision": "", "revisions": ["x", "", "y"]},
                    "junk"
                ]
            }
        });
        let snap = snapshot_from_value(&obj, SKIP).unwrap();
        assert_eq!(snap.name, "prd-api");
        assert_eq!(snap.project, "prd");
        assert_eq!(snap.identity, "api");
        assert_eq!(snap.sync_status, "Synced");
        assert_eq!(snap.health_status, "Healthy");
        assert_eq!(snap.live_images.len(), 1);
        assert_eq!(snap.live_images[0].tag, "1");
        assert!(snap.skip_requested);
        assert_eq!(snap.pending_revision, "abc");
        assert_eq!(snap.current_revision, "def");
        assert_eq!(snap.deployed_revisions, vec!["abc", "x", "y"]);
    }

    #[test]
    fn snapshot_defaults_and_errors() {
        let err = snapshot_from_value(&json!({"spec": {}}), SKIP).unwrap_err();
        assert_eq!(err, SnapshotError::MissingName);

        let snap = snapshot_from_value(&json!({"metadata": {"name": "api"}}), SKIP).unwrap();
        assert_eq!(snap.project, "default");
        assert_eq!(snap.identity, "api");
        assert!(!snap.skip_requested);
        assert!(snap.live_images.is_empty());
        assert!(snap.deployed_revisions.is_empty());

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
        let err: ReadError = SnapshotError::MissingName.into();
        assert_eq!(err.to_string(), "application has no metadata.name");
    }
}
