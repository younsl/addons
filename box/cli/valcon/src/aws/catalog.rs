use aws_sdk_elasticache::error::DisplayErrorContext;
use aws_sdk_elasticache::types::{ClusterMode, ReplicationGroup};

use super::error::AwsError;
use crate::source::CatalogSource;
use crate::types::{CacheGroup, CacheNode, Role};

pub struct ElastiCacheCatalog {
    client: aws_sdk_elasticache::Client,
}

impl ElastiCacheCatalog {
    pub const fn new(client: aws_sdk_elasticache::Client) -> Self {
        Self { client }
    }
}

impl CatalogSource for ElastiCacheCatalog {
    type Error = AwsError;

    async fn groups(&self) -> Result<Vec<CacheGroup>, AwsError> {
        let mut stream = self
            .client
            .describe_replication_groups()
            .into_paginator()
            .items()
            .send();
        let mut groups = Vec::new();
        while let Some(item) = stream.next().await {
            let rg =
                item.map_err(|e| AwsError::DescribeGroups(DisplayErrorContext(e).to_string()))?;
            groups.extend(to_group(&rg));
        }
        groups.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(groups)
    }
}

/// Node groups carry roles and zones. Cluster-mode groups may omit members there, so any
/// member cluster not covered falls back to an unknown role.
pub fn to_group(rg: &ReplicationGroup) -> Option<CacheGroup> {
    let cluster_mode = rg.cluster_enabled().unwrap_or(false)
        || matches!(rg.cluster_mode(), Some(ClusterMode::Enabled));
    let mut nodes: Vec<CacheNode> = rg
        .node_groups()
        .iter()
        .flat_map(|ng| {
            let shard = ng.node_group_id().unwrap_or("-").to_owned();
            ng.node_group_members().iter().filter_map(move |m| {
                Some(CacheNode {
                    id: m.cache_cluster_id()?.to_owned(),
                    shard: shard.clone(),
                    role: Role::parse(m.current_role()),
                    az: m.preferred_availability_zone().unwrap_or("-").to_owned(),
                })
            })
        })
        .collect();
    for member in rg.member_clusters() {
        if !nodes.iter().any(|n| &n.id == member) {
            nodes.push(CacheNode {
                id: member.clone(),
                shard: "-".into(),
                role: Role::Unknown,
                az: "-".into(),
            });
        }
    }
    nodes.sort_by(|a, b| (&a.shard, &a.id).cmp(&(&b.shard, &b.id)));
    Some(CacheGroup {
        id: rg.replication_group_id()?.to_owned(),
        description: rg.description().unwrap_or_default().to_owned(),
        engine: rg.engine().unwrap_or("redis").to_owned(),
        node_type: rg.cache_node_type().unwrap_or("-").to_owned(),
        status: rg.status().unwrap_or("-").to_owned(),
        cluster_mode,
        nodes,
    })
}

#[cfg(test)]
mod tests {
    use aws_sdk_elasticache::types::{NodeGroup, NodeGroupMember};

    use super::*;

    #[test]
    fn maps_nodes_with_roles() {
        let member = |id: &str, role: &str, az: &str| {
            NodeGroupMember::builder()
                .cache_cluster_id(id)
                .current_role(role)
                .preferred_availability_zone(az)
                .build()
        };
        let rg = ReplicationGroup::builder()
            .replication_group_id("orders-cache")
            .description("orders")
            .engine("valkey")
            .cache_node_type("cache.m7g.large")
            .status("available")
            .cluster_enabled(false)
            .node_groups(
                NodeGroup::builder()
                    .node_group_id("0001")
                    .node_group_members(member("orders-cache-002", "replica", "us-east-1c"))
                    .node_group_members(member("orders-cache-001", "primary", "us-east-1a"))
                    .build(),
            )
            .member_clusters("orders-cache-001")
            .member_clusters("orders-cache-002")
            .build();
        let g = to_group(&rg).unwrap();
        assert_eq!(g.engine, "valkey");
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(
            (g.nodes[0].id.as_str(), g.nodes[0].role),
            ("orders-cache-001", Role::Primary)
        );
        assert_eq!(g.nodes[1].az, "us-east-1c");
        assert!(!g.cluster_mode);
    }

    #[test]
    fn falls_back_to_member_clusters() {
        let rg = ReplicationGroup::builder()
            .replication_group_id("legacy")
            .cluster_mode(ClusterMode::Enabled)
            .member_clusters("legacy-0001-001")
            .build();
        let g = to_group(&rg).unwrap();
        assert_eq!(
            (g.engine.as_str(), g.node_type.as_str(), g.status.as_str()),
            ("redis", "-", "-")
        );
        assert!(g.cluster_mode);
        assert_eq!(g.nodes[0].role, Role::Unknown);
        assert!(to_group(&ReplicationGroup::builder().build()).is_none());
    }
}
