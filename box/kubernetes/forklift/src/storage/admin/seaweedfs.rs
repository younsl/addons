//! SeaweedFS master HTTP API (port 9333). It has no credentials, only an IP
//! whitelist. The master reports volume slots, not bytes, so disk capacity is
//! read from each volume server's `/status`.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use super::{
    AdminConfig, ClusterAdmin, ClusterInfo, ProviderSpec, base_url, decode, fetch, http_client,
};
use crate::storage::{Error, Result};

pub const SEAWEEDFS: ProviderSpec = ProviderSpec {
    id: "seaweedfs",
    display_name: "SeaweedFS",
    admin: Some(|cfg| Ok(Arc::new(SeaweedAdmin::new(cfg)?))),
};

const OP: &str = "seaweedfs master";

pub struct SeaweedAdmin {
    master: String,
    client: reqwest::Client,
}

impl SeaweedAdmin {
    pub fn new(cfg: &AdminConfig) -> Result<SeaweedAdmin> {
        if cfg.admin_endpoint.trim().is_empty() {
            return Err(Error::AdminUnavailable(
                "seaweedfs needs the master endpoint (port 9333)".into(),
            ));
        }
        Ok(SeaweedAdmin {
            master: base_url(&cfg.admin_endpoint)?,
            client: http_client()?,
        })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, url: &str, op: &'static str) -> Result<T> {
        let req = self.client.get(url).build().map_err(|e| Error::s3(op, e))?;
        decode(&fetch(&self.client, req, op).await?, op)
    }
}

#[async_trait]
impl ClusterAdmin for SeaweedAdmin {
    async fn cluster_info(&self) -> Result<ClusterInfo> {
        let dir: DirStatus = self.get(&format!("{}/dir/status", self.master), OP).await?;
        let vols: VolStatus = self.get(&format!("{}/vol/status", self.master), OP).await?;

        let nodes: Vec<&DataNode> = dir
            .topology
            .data_centers
            .iter()
            .flat_map(|dc| &dc.racks)
            .flat_map(|r| &r.data_nodes)
            .collect();
        let mut out = ClusterInfo {
            servers: nodes.len() as i64,
            version: dir
                .version
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string(),
            logical_used_bytes: Some(logical_bytes(&vols)),
            ..ClusterInfo::default()
        };

        let (mut total, mut used, mut free) = (0u64, 0u64, 0u64);
        for node in nodes {
            let url = format!("{}/status", base_url(&node.url)?);
            match self
                .get::<VolumeServerStatus>(&url, "seaweedfs volume")
                .await
            {
                Ok(status) => {
                    for d in &status.disk_statuses {
                        out.online_drives += 1;
                        total += d.all;
                        used += d.used;
                        free += d.free;
                    }
                }
                Err(err) => {
                    tracing::debug!(node = %node.url, err = %err, "seaweedfs volume status");
                    out.offline_drives += 1;
                }
            }
        }
        out.set_capacity(total, used, free);
        Ok(out)
    }
}

/// `/vol/status` lists every replica, so each volume's live bytes are
/// divided by its copy count.
fn logical_bytes(vols: &VolStatus) -> i64 {
    let mut bytes = 0f64;
    for racks in vols.volumes.data_centers.values() {
        for nodes in racks.values() {
            for volumes in nodes.values() {
                for v in volumes {
                    let copies = 1
                        + v.replica_placement.node
                        + v.replica_placement.rack
                        + v.replica_placement.dc;
                    bytes += v.size.saturating_sub(v.deleted_byte_count) as f64 / copies as f64;
                }
            }
        }
    }
    bytes.round() as i64
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DirStatus {
    #[serde(default)]
    version: String,
    #[serde(default)]
    topology: Topology,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Topology {
    #[serde(default)]
    data_centers: Vec<DataCenter>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DataCenter {
    #[serde(default)]
    racks: Vec<Rack>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Rack {
    #[serde(default)]
    data_nodes: Vec<DataNode>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DataNode {
    url: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct VolStatus {
    #[serde(default)]
    volumes: VolumeTopology,
}

/// Data center, rack and node are map keys.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct VolumeTopology {
    #[serde(default)]
    data_centers: BTreeMap<String, BTreeMap<String, BTreeMap<String, Vec<Volume>>>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Volume {
    #[serde(default)]
    size: u64,
    #[serde(default)]
    deleted_byte_count: u64,
    #[serde(default)]
    replica_placement: ReplicaPlacement,
}

#[derive(Debug, Default, Deserialize)]
struct ReplicaPlacement {
    #[serde(default)]
    node: u64,
    #[serde(default)]
    rack: u64,
    #[serde(default)]
    dc: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct VolumeServerStatus {
    #[serde(default)]
    disk_statuses: Vec<DiskStatus>,
}

#[derive(Debug, Default, Deserialize)]
struct DiskStatus {
    #[serde(default)]
    all: u64,
    #[serde(default)]
    used: u64,
    #[serde(default)]
    free: u64,
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::storage::admin::seaweedfs::*;
    use crate::storage::admin::tests::{config, install_crypto_provider};

    fn admin(endpoint: &str) -> SeaweedAdmin {
        SeaweedAdmin::new(&AdminConfig {
            admin_endpoint: endpoint.to_string(),
            ..config()
        })
        .unwrap()
    }

    async fn mount(server: &MockServer, p: &str, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    #[test]
    fn requires_master_endpoint() {
        install_crypto_provider();
        let err = SeaweedAdmin::new(&config()).map(|_| ()).unwrap_err();
        assert!(
            matches!(&err, Error::AdminUnavailable(m) if m.contains("master")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn reads_topology_volumes_and_disks() {
        install_crypto_provider();
        let master = MockServer::start().await;
        let volume = MockServer::start().await;
        let volume_addr = volume.address().to_string();
        mount(
            &master,
            "/dir/status",
            json!({"Version": "30GB 4.48 abc1234", "Topology": {"Max": 16, "Free": 9,
            "DataCenters": [{"Id": "dc1", "Racks": [{"Id": "r1", "DataNodes": [
                {"Url": volume_addr, "Volumes": 2, "Max": 8},
                {"Url": "127.0.0.1:1", "Volumes": 0, "Max": 8}
            ]}]}]}}),
        )
        .await;
        mount(
            &master,
            "/vol/status",
            json!({"Version": "30GB 4.48", "Volumes": {"Max": 16, "Free": 9, "DataCenters": {
            "dc1": {"r1": {volume_addr.clone(): [
                {"Id": 1, "Size": 1000, "DeletedByteCount": 200, "ReplicaPlacement": {}},
                {"Id": 2, "Size": 600, "DeletedByteCount": 0, "ReplicaPlacement": {"node": 1}},
                {"Id": 2, "Size": 600, "DeletedByteCount": 0, "ReplicaPlacement": {"node": 1}}
            ]}}}}}),
        )
        .await;
        mount(
            &volume,
            "/status",
            json!({"Version": "30GB 4.48", "DiskStatuses": [
                {"dir": "/data", "all": 10000, "used": 2500, "free": 7500, "disk_type": "hdd"}
            ]}),
        )
        .await;

        let info = admin(&master.uri()).cluster_info().await.unwrap();
        assert_eq!(info.version, "4.48");
        assert_eq!(info.servers, 2);
        assert_eq!(
            (info.online_drives, info.offline_drives),
            (1, 1),
            "unreachable node"
        );
        assert_eq!(info.total_capacity_bytes, 10000);
        assert_eq!(info.used_bytes, 2500);
        assert_eq!(info.available_bytes, 7500);
        assert_eq!(info.usage_ratio, 0.25);
        assert_eq!(info.logical_used_bytes, Some(1400), "replicas counted once");
        assert_eq!(info.object_count, None);
        assert_eq!(info.bucket_count, None);
    }

    #[tokio::test]
    async fn master_failure_is_an_error() {
        install_crypto_provider();
        let master = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&master)
            .await;
        let err = admin(&master.uri())
            .cluster_info()
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("seaweedfs master: 503"), "err = {err}");
    }
}
