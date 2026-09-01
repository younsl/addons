//! Reduces an Argo CD Application document to the fields the gate reasons
//! about. The document arrives inside `AdmissionReview.request.object`, so no
//! Application is ever read from the API.

use serde_json::Value;
use thiserror::Error;

use crate::gate::AppSnapshot;

/// Why an Application document could not be reduced to a snapshot.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SnapshotError {
    #[error("application has no metadata.name")]
    MissingName,
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
    fn nested_helpers_walk_and_default() {
        let obj = json!({"a": {"b": "c"}});
        assert_eq!(nested_str(&obj, &["a", "b"]), "c");
        assert_eq!(nested_str(&obj, &["a", "x"]), "");
        assert!(nested(&obj, &["a"]).is_some());
        assert!(nested(&obj, &["z", "b"]).is_none());
    }
}
