//! Kubernetes access: the in-cluster client, Lease-based leader election,
//! Event publishing, and the Node client of the throughput recommender.

pub mod events;
pub mod leader;
pub mod nodes;

use anyhow::{Context as _, Result};

/// Builds a client from the in-cluster config. It fails outside a cluster,
/// which every caller treats as "disable this feature" rather than a fatal
/// error, so the resize loop still runs from a laptop.
pub fn in_cluster_client() -> Result<kube::Client> {
    let config = kube::Config::incluster().context("in-cluster config")?;
    kube::Client::try_from(config).context("kubernetes client")
}

/// Renders a chrono instant as a k8s-openapi `Time`.
pub(crate) fn k8s_time(
    t: chrono::DateTime<chrono::Utc>,
) -> k8s_openapi::apimachinery::pkg::apis::meta::v1::Time {
    k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
        k8s_openapi::jiff::Timestamp::from_millisecond(t.timestamp_millis()).unwrap_or_default(),
    )
}

/// Converts a k8s-openapi `Time` to chrono.
pub(crate) fn from_k8s_time(
    t: &k8s_openapi::apimachinery::pkg::apis::meta::v1::Time,
) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp_millis(t.0.as_millisecond())
}

/// Builds the merge patch that sets and removes annotations in one request.
/// A merge patch deletes a key by mapping it to JSON null. Returns `None`
/// when there is nothing to write.
pub(crate) fn annotation_patch(
    set: &std::collections::BTreeMap<String, String>,
    remove: &[String],
) -> Option<serde_json::Value> {
    if set.is_empty() && remove.is_empty() {
        return None;
    }
    let mut values = serde_json::Map::new();
    for (k, v) in set {
        values.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    for k in remove {
        values.insert(k.clone(), serde_json::Value::Null);
    }
    Some(serde_json::json!({"metadata": {"annotations": values}}))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[tokio::test]
    async fn in_cluster_client_fails_outside_a_cluster() {
        // The test process has no service account token mounted.
        if std::env::var("KUBERNETES_SERVICE_HOST").is_ok() {
            return;
        }
        let err = in_cluster_client().err().expect("no cluster");
        assert!(err.to_string().contains("in-cluster config"), "{err}");
    }

    #[test]
    fn time_round_trips() {
        let now = chrono::DateTime::from_timestamp_millis(1_700_000_000_123).unwrap();
        assert_eq!(from_k8s_time(&k8s_time(now)).unwrap(), now);
    }

    #[test]
    fn annotation_patch_shapes() {
        assert!(annotation_patch(&BTreeMap::new(), &[]).is_none());
        let p = annotation_patch(
            &BTreeMap::from([("a/x".to_string(), "1".to_string())]),
            &["a/y".to_string()],
        )
        .unwrap();
        assert_eq!(
            p,
            serde_json::json!({"metadata": {"annotations": {"a/x": "1", "a/y": null}}})
        );
    }
}
