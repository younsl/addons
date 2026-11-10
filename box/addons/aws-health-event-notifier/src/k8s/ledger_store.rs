//! `ConfigMap` persistence for the delivery ledger.
//!
//! The payload is opaque JSON to this layer. Writes are compare-and-swap on
//! `resourceVersion`, which is what fences a stale leader out.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::PostParams;
use kube::{Api, Client};

use crate::k8s::error::{K8sResult, conflict_or};

const KEY: &str = "ledger.json";

pub struct Stored {
    pub payload: String,
    pub resource_version: String,
}

pub struct LedgerStore {
    api: Api<ConfigMap>,
    name: String,
}

impl LedgerStore {
    pub fn new(client: Client, namespace: &str, name: impl Into<String>) -> Self {
        Self {
            api: Api::namespaced(client, namespace),
            name: name.into(),
        }
    }

    pub async fn load(&self) -> K8sResult<Option<Stored>> {
        let Some(cm) = self.api.get_opt(&self.name).await? else {
            return Ok(None);
        };
        let payload = cm
            .data
            .as_ref()
            .and_then(|d| d.get(KEY).cloned())
            .unwrap_or_default();
        Ok(Some(Stored {
            payload,
            resource_version: cm.metadata.resource_version.unwrap_or_default(),
        }))
    }

    /// Creates the `ConfigMap`. Fails with `Conflict` if it already exists.
    pub async fn create(&self, payload: &str) -> K8sResult<String> {
        let cm = self.object(payload, None);
        self.api
            .create(&PostParams::default(), &cm)
            .await
            .map(version_of)
            .map_err(|e| conflict_or(e, &self.name))
    }

    /// Replaces the payload only if the `ConfigMap` is still at `resource_version`.
    pub async fn replace(&self, payload: &str, resource_version: &str) -> K8sResult<String> {
        let cm = self.object(payload, Some(resource_version.to_string()));
        self.api
            .replace(&self.name, &PostParams::default(), &cm)
            .await
            .map(version_of)
            .map_err(|e| conflict_or(e, &self.name))
    }

    fn object(&self, payload: &str, resource_version: Option<String>) -> ConfigMap {
        ConfigMap {
            metadata: ObjectMeta {
                name: Some(self.name.clone()),
                resource_version,
                labels: Some(BTreeMap::from([(
                    "app.kubernetes.io/component".to_string(),
                    "ledger".to_string(),
                )])),
                ..Default::default()
            },
            data: Some(BTreeMap::from([(KEY.to_string(), payload.to_string())])),
            ..Default::default()
        }
    }
}

fn version_of(cm: ConfigMap) -> String {
    cm.metadata.resource_version.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::error::K8sError;
    use crate::k8s::fake::FakeApiServer;

    #[tokio::test]
    async fn create_load_replace_round_trip() {
        let server = FakeApiServer::start().await;
        let store = LedgerStore::new(server.client(), "ns", "ledger");
        assert!(store.load().await.unwrap().is_none());

        let rv = store.create("{\"term\":1}").await.unwrap();
        let loaded = store.load().await.unwrap().unwrap();
        assert_eq!(loaded.payload, "{\"term\":1}");
        assert_eq!(loaded.resource_version, rv);

        let rv2 = store.replace("{\"term\":2}", &rv).await.unwrap();
        assert_ne!(rv, rv2);
        assert_eq!(store.load().await.unwrap().unwrap().payload, "{\"term\":2}");
    }

    #[tokio::test]
    async fn stale_replace_conflicts() {
        let server = FakeApiServer::start().await;
        let store = LedgerStore::new(server.client(), "ns", "ledger");
        let rv = store.create("a").await.unwrap();
        store.replace("b", &rv).await.unwrap();
        let err = store.replace("c", &rv).await.unwrap_err();
        assert!(matches!(err, K8sError::Conflict(_)));
    }

    #[tokio::test]
    async fn duplicate_create_conflicts() {
        let server = FakeApiServer::start().await;
        let store = LedgerStore::new(server.client(), "ns", "ledger");
        store.create("a").await.unwrap();
        assert!(matches!(
            store.create("a").await.unwrap_err(),
            K8sError::Conflict(_)
        ));
    }

    #[tokio::test]
    async fn api_failure_is_not_a_conflict() {
        let server = FakeApiServer::start().await;
        let store = LedgerStore::new(server.client(), "ns", "ledger");
        server.fail_all();
        assert!(matches!(
            store.create("a").await.unwrap_err(),
            K8sError::Api(_)
        ));
        assert!(store.load().await.is_err());
    }
}
