use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::time::Duration;

use chrono::{DateTime, Local};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Primary,
    Replica,
    Unknown,
}

impl Role {
    pub fn parse(s: Option<&str>) -> Self {
        match s.map(str::to_ascii_lowercase).as_deref() {
            Some("primary") => Self::Primary,
            Some("replica") => Self::Replica,
            _ => Self::Unknown,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Replica => "replica",
            Self::Unknown => "-",
        }
    }
}

/// One cache node (an `ElastiCache` member cluster).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheNode {
    pub id: String,
    pub shard: String,
    pub role: Role,
    pub az: String,
}

/// An `ElastiCache` replication group as listed by the API.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheGroup {
    pub id: String,
    pub description: String,
    pub engine: String,
    pub node_type: String,
    pub status: String,
    pub cluster_mode: bool,
    pub nodes: Vec<CacheNode>,
}

impl CacheGroup {
    pub fn matches(&self, needle: &str) -> bool {
        let needle = needle.to_lowercase();
        needle.is_empty()
            || self.id.to_lowercase().contains(&needle)
            || self.description.to_lowercase().contains(&needle)
            || self.engine.to_lowercase().contains(&needle)
    }

    pub fn shard_count(&self) -> usize {
        let mut shards: Vec<&str> = self.nodes.iter().map(|n| n.shard.as_str()).collect();
        shards.sort_unstable();
        shards.dedup();
        shards.len()
    }
}

/// Latest `CloudWatch` values for one node. `None` when no datapoint arrived in the window.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct NodeMetrics {
    pub items: Option<f64>,
    pub connections: Option<f64>,
    pub cpu: Option<f64>,
    pub memory: Option<f64>,
}

/// A pod and how many ESTABLISHED sockets it holds to each cache node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodRef {
    pub namespace: String,
    pub name: String,
    pub app: String,
    pub deleting: bool,
    pub containers: Vec<String>,
    /// A `valcon-probe` ephemeral container is already running in the pod.
    pub probe: bool,
}

impl PodRef {
    pub fn key(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }
}

#[derive(Debug, Clone)]
pub struct PodScan {
    pub pod: PodRef,
    /// Remote IP to socket count, engine port only.
    pub result: Result<HashMap<IpAddr, u32>, String>,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub at: DateTime<Local>,
    pub took: Duration,
    pub groups: Vec<CacheGroup>,
    pub ips: HashMap<String, IpAddr>,
    pub metrics: HashMap<String, NodeMetrics>,
    pub pods: Vec<PodScan>,
    pub pods_listed: bool,
    pub errors: Vec<String>,
}

/// Pod connections resolved to nodes, built once per snapshot.
#[derive(Debug, Clone, Default)]
pub struct Topology {
    node_group: HashMap<String, String>,
    ip_node: HashMap<IpAddr, String>,
}

impl Topology {
    pub fn new(snap: &Snapshot) -> Self {
        let node_group = snap
            .groups
            .iter()
            .flat_map(|g| g.nodes.iter().map(|n| (n.id.clone(), g.id.clone())))
            .collect();
        let ip_node = snap.ips.iter().map(|(id, ip)| (*ip, id.clone())).collect();
        Self {
            node_group,
            ip_node,
        }
    }

    pub fn group_of(&self, node: &str) -> Option<&str> {
        self.node_group.get(node).map(String::as_str)
    }

    /// Node id to socket count for one pod. Sockets to unknown IPs are dropped.
    pub fn per_node(&self, scan: &PodScan) -> BTreeMap<String, u32> {
        let mut out = BTreeMap::new();
        if let Ok(by_ip) = &scan.result {
            for (ip, n) in by_ip {
                if let Some(node) = self.ip_node.get(ip) {
                    *out.entry(node.clone()).or_default() += n;
                }
            }
        }
        out
    }

    /// Group id to socket count for one pod.
    pub fn per_group(&self, scan: &PodScan) -> BTreeMap<String, u32> {
        let mut out = BTreeMap::new();
        for (node, n) in self.per_node(scan) {
            if let Some(group) = self.group_of(&node) {
                *out.entry(group.to_owned()).or_default() += n;
            }
        }
        out
    }
}

/// Aggregates of one group for list views.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GroupStats {
    pub items: Option<f64>,
    pub connections: Option<f64>,
    pub cpu: Option<f64>,
    pub memory: Option<f64>,
    pub pods: usize,
    pub pod_conns: u32,
}

impl Snapshot {
    pub fn group(&self, id: &str) -> Option<&CacheGroup> {
        self.groups.iter().find(|g| g.id == id)
    }

    /// Items count only primaries (replicas hold the same keys), connections sum every node,
    /// CPU and memory take the busiest node.
    pub fn stats(&self, group: &CacheGroup, topo: &Topology) -> GroupStats {
        let mut s = GroupStats::default();
        let mut shard_items: BTreeMap<&str, f64> = BTreeMap::new();
        for node in &group.nodes {
            let Some(m) = self.metrics.get(&node.id) else {
                continue;
            };
            if let Some(v) = m.items {
                let slot = shard_items.entry(node.shard.as_str()).or_insert(0.0);
                if node.role == Role::Primary || *slot == 0.0 {
                    *slot = v;
                }
            }
            add(&mut s.connections, m.connections);
            max(&mut s.cpu, m.cpu);
            max(&mut s.memory, m.memory);
        }
        if !shard_items.is_empty() {
            s.items = Some(shard_items.values().sum());
        }
        for scan in &self.pods {
            if let Some(n) = topo.per_group(scan).get(&group.id) {
                s.pods += 1;
                s.pod_conns += n;
            }
        }
        s
    }
}

fn add(slot: &mut Option<f64>, v: Option<f64>) {
    if let Some(v) = v {
        *slot = Some(slot.unwrap_or(0.0) + v);
    }
}

fn max(slot: &mut Option<f64>, v: Option<f64>) {
    if let Some(v) = v {
        *slot = Some(slot.map_or(v, |s| s.max(v)));
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn node(id: &str, shard: &str, role: Role) -> CacheNode {
        CacheNode {
            id: id.into(),
            shard: shard.into(),
            role,
            az: "us-east-1a".into(),
        }
    }

    pub fn group(id: &str) -> CacheGroup {
        CacheGroup {
            id: id.into(),
            description: format!("{id} store"),
            engine: "valkey".into(),
            node_type: "cache.m7g.large".into(),
            status: "available".into(),
            cluster_mode: false,
            nodes: vec![
                node(&format!("{id}-001"), "0001", Role::Primary),
                node(&format!("{id}-002"), "0001", Role::Replica),
            ],
        }
    }

    pub fn pod(ns: &str, name: &str, app: &str) -> PodRef {
        PodRef {
            namespace: ns.into(),
            name: name.into(),
            app: app.into(),
            deleting: false,
            containers: vec!["app".into()],
            probe: false,
        }
    }

    pub fn scan(ns: &str, name: &str, app: &str, conns: &[(&str, u32)]) -> PodScan {
        PodScan {
            pod: pod(ns, name, app),
            result: Ok(conns
                .iter()
                .map(|(ip, n)| (ip.parse().unwrap(), *n))
                .collect()),
        }
    }

    /// Two groups: orders (10.0.0.1 primary, 10.0.0.2 replica) and billing (10.0.1.1, 10.0.1.2).
    pub fn snapshot() -> Snapshot {
        let metrics = |items, conns, cpu| NodeMetrics {
            items: Some(items),
            connections: Some(conns),
            cpu: Some(cpu),
            memory: Some(cpu / 2.0),
        };
        Snapshot {
            at: Local::now(),
            took: Duration::from_millis(1200),
            groups: vec![group("billing"), group("orders")],
            ips: HashMap::from([
                ("orders-001".into(), "10.0.0.1".parse().unwrap()),
                ("orders-002".into(), "10.0.0.2".parse().unwrap()),
                ("billing-001".into(), "10.0.1.1".parse().unwrap()),
                ("billing-002".into(), "10.0.1.2".parse().unwrap()),
            ]),
            metrics: HashMap::from([
                ("orders-001".into(), metrics(1000.0, 40.0, 12.0)),
                ("orders-002".into(), metrics(1000.0, 30.0, 8.0)),
                ("billing-001".into(), metrics(50.0, 6.0, 2.0)),
            ]),
            pods: vec![
                scan("shop", "api-1", "api", &[("10.0.0.1", 3), ("10.0.0.2", 2)]),
                scan("shop", "api-2", "api", &[("10.0.0.2", 1), ("9.9.9.9", 4)]),
                scan(
                    "pay",
                    "ledger-1",
                    "ledger",
                    &[("10.0.1.1", 2), ("10.0.0.1", 1)],
                ),
                PodScan {
                    pod: pod("pay", "batch-1", "batch"),
                    result: Err("cat: not found".into()),
                },
            ],
            pods_listed: true,
            errors: vec![],
        }
    }

    #[test]
    fn role_parsing_and_labels() {
        assert_eq!(Role::parse(Some("PRIMARY")), Role::Primary);
        assert_eq!(Role::parse(Some("replica")), Role::Replica);
        assert_eq!(Role::parse(None), Role::Unknown);
        assert_eq!(Role::Unknown.label(), "-");
    }

    #[test]
    fn group_search_and_shards() {
        let g = group("orders");
        assert!(g.matches("ORD") && g.matches("store") && g.matches("valk") && g.matches(""));
        assert!(!g.matches("billing"));
        assert_eq!(g.shard_count(), 1);
    }

    #[test]
    fn topology_maps_sockets_to_nodes_and_groups() {
        let snap = snapshot();
        let topo = Topology::new(&snap);
        let per_node = topo.per_node(&snap.pods[1]);
        assert_eq!(
            per_node.into_iter().collect::<Vec<_>>(),
            [("orders-002".to_owned(), 1)]
        );
        let per_group = topo.per_group(&snap.pods[2]);
        assert_eq!(per_group.get("billing"), Some(&2));
        assert_eq!(per_group.get("orders"), Some(&1));
        assert!(topo.per_group(&snap.pods[3]).is_empty());
        assert_eq!(snap.pods[0].pod.key(), "shop/api-1");
    }

    #[test]
    fn stats_aggregate_by_rule() {
        let snap = snapshot();
        let topo = Topology::new(&snap);
        let orders = snap.stats(snap.group("orders").unwrap(), &topo);
        assert_eq!(
            orders.items,
            Some(1000.0),
            "replica items not double counted"
        );
        assert_eq!(orders.connections, Some(70.0));
        assert_eq!(orders.cpu, Some(12.0));
        assert_eq!((orders.pods, orders.pod_conns), (3, 7));
        let billing = snap.stats(snap.group("billing").unwrap(), &topo);
        assert_eq!((billing.items, billing.pods), (Some(50.0), 1));
        let empty = CacheGroup::default();
        assert_eq!(snap.stats(&empty, &topo), GroupStats::default());
    }
}
