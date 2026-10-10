//! Garage admin API v2, served on its own port (3903) with a bearer token.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use super::{
    AdminConfig, ClusterAdmin, ClusterInfo, ProviderSpec, base_url, decode, fetch, http_client,
};
use crate::storage::{Error, Result};

pub const GARAGE: ProviderSpec = ProviderSpec {
    id: "garage",
    display_name: "Garage",
    admin: Some(|cfg| Ok(Arc::new(GarageAdmin::new(cfg)?))),
};

const OP: &str = "garage admin";

pub struct GarageAdmin {
    base: String,
    token: String,
    client: reqwest::Client,
}

impl GarageAdmin {
    pub fn new(cfg: &AdminConfig) -> Result<Self> {
        if cfg.admin_endpoint.trim().is_empty() {
            return Err(Error::AdminUnavailable(
                "garage needs the admin endpoint (port 3903)".into(),
            ));
        }
        if cfg.admin_token.is_empty() {
            return Err(Error::AdminUnavailable(
                "garage needs an admin token".into(),
            ));
        }
        Ok(Self {
            base: base_url(&cfg.admin_endpoint)?,
            token: cfg.admin_token.clone(),
            client: http_client()?,
        })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path_and_query: &str) -> Result<T> {
        let req = self
            .client
            .get(format!("{}{path_and_query}", self.base))
            .bearer_auth(&self.token)
            .build()
            .map_err(|e| Error::s3(OP, e))?;
        decode(&fetch(&self.client, req, OP).await?, OP)
    }
}

#[async_trait]
impl ClusterAdmin for GarageAdmin {
    async fn cluster_info(&self) -> Result<ClusterInfo> {
        let status: ClusterStatus = self.get("/v2/GetClusterStatus").await?;
        let stats: ClusterStatistics = self.get("/v2/GetClusterStatistics").await?;
        let mut out = summarize(status);
        out.bucket_count = stats.bucket_count;
        out.object_count = stats.total_object_count;
        out.logical_used_bytes = stats.total_object_bytes;
        Ok(out)
    }
}

/// Only nodes with a storage role hold data; gateways are counted as servers
/// but not as storage units.
fn summarize(status: ClusterStatus) -> ClusterInfo {
    let mut out = ClusterInfo {
        servers: status.nodes.len() as i64,
        ..ClusterInfo::default()
    };
    let (mut total, mut avail) = (0u64, 0u64);
    for n in &status.nodes {
        if out.version.is_empty()
            && let Some(v) = &n.garage_version
        {
            out.version.clone_from(v);
        }
        let is_storage = n
            .role
            .as_ref()
            .is_some_and(|r| r.capacity.is_some_and(|c| c > 0));
        if !is_storage {
            continue;
        }
        if n.is_up {
            out.online_drives += 1;
        } else {
            out.offline_drives += 1;
        }
        if let Some(p) = &n.data_partition {
            total += p.total;
            avail += p.available;
        }
    }
    out.set_capacity(total, total.saturating_sub(avail), avail);
    out
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClusterStatus {
    #[serde(default)]
    nodes: Vec<Node>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Node {
    #[serde(default)]
    is_up: bool,
    #[serde(default)]
    garage_version: Option<String>,
    #[serde(default)]
    role: Option<Role>,
    #[serde(default)]
    data_partition: Option<Partition>,
}

#[derive(Debug, Default, Deserialize)]
struct Role {
    #[serde(default)]
    capacity: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct Partition {
    #[serde(default)]
    available: u64,
    #[serde(default)]
    total: u64,
}

/// Object totals are omitted once a cluster holds 1000 or more buckets.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClusterStatistics {
    #[serde(default)]
    bucket_count: Option<i64>,
    #[serde(default)]
    total_object_count: Option<i64>,
    #[serde(default)]
    total_object_bytes: Option<i64>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::storage::admin::garage::*;
    use crate::storage::admin::tests::{config, install_crypto_provider};

    fn admin(endpoint: &str) -> GarageAdmin {
        GarageAdmin::new(&AdminConfig {
            admin_endpoint: endpoint.to_string(),
            admin_token: "tok".into(),
            ..config()
        })
        .unwrap()
    }

    async fn mount(server: &MockServer, p: &str, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(p))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    #[test]
    fn requires_endpoint_and_token() {
        install_crypto_provider();
        let err = GarageAdmin::new(&AdminConfig {
            admin_token: "t".into(),
            ..config()
        })
        .map(|_| ())
        .unwrap_err();
        assert!(
            matches!(&err, Error::AdminUnavailable(m) if m.contains("endpoint")),
            "{err:?}"
        );
        let err = GarageAdmin::new(&AdminConfig {
            admin_endpoint: "http://garage:3903".into(),
            ..config()
        })
        .map(|_| ())
        .unwrap_err();
        assert!(
            matches!(&err, Error::AdminUnavailable(m) if m.contains("token")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn reads_nodes_and_bucket_totals() {
        install_crypto_provider();
        let server = MockServer::start().await;
        mount(
            &server,
            "/v2/GetClusterStatus",
            json!({"layoutVersion": 1, "nodes": [
                {"id": "a", "isUp": true, "garageVersion": "v2.4.1",
                 "role": {"zone": "z1", "capacity": 1000, "tags": []},
                 "dataPartition": {"available": 600, "total": 1000}},
                {"id": "b", "isUp": false, "garageVersion": "v2.4.1",
                 "role": {"zone": "z1", "capacity": 1000, "tags": []},
                 "dataPartition": {"available": 1000, "total": 1000}},
                {"id": "gw", "isUp": true, "role": {"zone": "z1", "capacity": null, "tags": []}}
            ]}),
        )
        .await;
        mount(
            &server,
            "/v2/GetClusterStatistics",
            json!({"freeform": "", "dataAvail": 1600, "bucketCount": 2,
                   "totalObjectCount": 5, "totalObjectBytes": 50}),
        )
        .await;

        let info = admin(&server.uri()).cluster_info().await.unwrap();
        assert_eq!(info.servers, 3);
        assert_eq!(
            (info.online_drives, info.offline_drives),
            (1, 1),
            "gateway excluded"
        );
        assert_eq!(info.total_capacity_bytes, 2000);
        assert_eq!(info.available_bytes, 1600);
        assert_eq!(info.used_bytes, 400);
        assert_eq!(info.usage_ratio, 0.2);
        assert_eq!(info.bucket_count, Some(2));
        assert_eq!(info.object_count, Some(5));
        assert_eq!(info.logical_used_bytes, Some(50));
        assert_eq!(info.version, "v2.4.1");
    }

    #[tokio::test]
    async fn omitted_totals_stay_unknown() {
        install_crypto_provider();
        let server = MockServer::start().await;
        mount(&server, "/v2/GetClusterStatus", json!({"nodes": []})).await;
        mount(&server, "/v2/GetClusterStatistics", json!({"freeform": ""})).await;

        let info = admin(&server.uri()).cluster_info().await.unwrap();
        assert_eq!(info.bucket_count, None);
        assert_eq!(info.object_count, None);
        assert_eq!(info.logical_used_bytes, None);
        assert_eq!(info.total_capacity_bytes, 0);
    }

    #[tokio::test]
    async fn unauthorized_is_an_error() {
        install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad token"))
            .mount(&server)
            .await;
        let err = admin(&server.uri())
            .cluster_info()
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("garage admin: 401"), "err = {err}");
    }
}
