//! Rows of every list screen, derived from state. Rendering and navigation both read them, so
//! the selected index always points at the row on screen.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::net::IpAddr;

use chrono::{DateTime, Local};

use super::backend::DbTarget;
use super::nav::{GroupTab, Load, Screen, Ui};
use crate::db::model::{DbStat, KeyInfo, printable};
use crate::state::{AppState, Event};
use crate::types::{CacheGroup, CacheNode, GroupStats, NodeMetrics, PodRef};

pub struct GroupRow<'a> {
    pub group: &'a CacheGroup,
    pub stats: GroupStats,
}

pub struct NodeRow<'a> {
    pub node: &'a CacheNode,
    pub ip: Option<IpAddr>,
    pub metrics: NodeMetrics,
    pub pod_conns: u32,
}

pub struct ClientRow {
    pub namespace: String,
    pub app: String,
    pub pods: usize,
    pub conns: u32,
    pub last_seen: Option<DateTime<Local>>,
}

pub struct PodRow<'a> {
    pub pod: &'a PodRef,
    /// Node or group id to sockets, depending on the screen.
    pub links: BTreeMap<String, u32>,
    pub conns: u32,
    pub error: Option<&'a str>,
    pub last_seen: Option<DateTime<Local>>,
}

fn hit(filter: &str, fields: &[&str]) -> bool {
    let f = filter.to_lowercase();
    f.is_empty() || fields.iter().any(|s| s.to_lowercase().contains(&f))
}

pub fn groups<'a>(state: &'a AppState, filter: &str) -> Vec<GroupRow<'a>> {
    let Some(snap) = &state.latest else {
        return Vec::new();
    };
    snap.groups
        .iter()
        .filter(|g| g.matches(filter))
        .map(|group| GroupRow {
            group,
            stats: snap.stats(group, &state.topo),
        })
        .collect()
}

pub fn nodes<'a>(state: &'a AppState, group: &str, filter: &str) -> Vec<NodeRow<'a>> {
    let Some(snap) = &state.latest else {
        return Vec::new();
    };
    let Some(g) = snap.group(group) else {
        return Vec::new();
    };
    let mut pod_conns: BTreeMap<String, u32> = BTreeMap::new();
    for scan in &snap.pods {
        for (node, n) in state.topo.per_node(scan) {
            *pod_conns.entry(node).or_default() += n;
        }
    }
    g.nodes
        .iter()
        .filter(|n| hit(filter, &[&n.id, n.role.label(), &n.az]))
        .map(|node| NodeRow {
            node,
            ip: snap.ips.get(&node.id).copied(),
            metrics: snap.metrics.get(&node.id).copied().unwrap_or_default(),
            pod_conns: pod_conns.get(&node.id).copied().unwrap_or(0),
        })
        .collect()
}

/// Pods of one group with sockets per node, optionally one app only. Busiest first.
pub fn group_pods<'a>(
    state: &'a AppState,
    group: &str,
    app: Option<(&str, &str)>,
    filter: &str,
) -> Vec<PodRow<'a>> {
    let Some(snap) = &state.latest else {
        return Vec::new();
    };
    let mut rows: Vec<PodRow<'a>> = snap
        .pods
        .iter()
        .filter(|s| app.is_none_or(|(ns, a)| s.pod.namespace == ns && s.pod.app == a))
        .filter(|s| hit(filter, &[&s.pod.name, &s.pod.namespace, &s.pod.app]))
        .filter_map(|s| {
            let links: BTreeMap<String, u32> = state
                .topo
                .per_node(s)
                .into_iter()
                .filter(|(node, _)| state.topo.group_of(node) == Some(group))
                .collect();
            if links.is_empty() {
                return None;
            }
            Some(PodRow {
                pod: &s.pod,
                conns: links.values().sum(),
                links,
                error: None,
                last_seen: state
                    .last_seen
                    .get(&(s.pod.key(), group.to_owned()))
                    .copied(),
            })
        })
        .collect();
    rows.sort_by_key(|r| (Reverse(r.conns), r.pod.key()));
    rows
}

pub fn clients(state: &AppState, group: &str, filter: &str) -> Vec<ClientRow> {
    let mut by_app: BTreeMap<(String, String), ClientRow> = BTreeMap::new();
    for r in group_pods(state, group, None, "") {
        let key = (r.pod.namespace.clone(), r.pod.app.clone());
        let row = by_app.entry(key).or_insert_with(|| ClientRow {
            namespace: r.pod.namespace.clone(),
            app: r.pod.app.clone(),
            pods: 0,
            conns: 0,
            last_seen: None,
        });
        row.pods += 1;
        row.conns += r.conns;
        row.last_seen = row.last_seen.max(r.last_seen);
    }
    let mut rows: Vec<ClientRow> = by_app
        .into_values()
        .filter(|r| hit(filter, &[&r.namespace, &r.app]))
        .collect();
    rows.sort_by_key(|r| (Reverse(r.conns), r.namespace.clone(), r.app.clone()));
    rows
}

/// Every scanned pod with sockets per group. Pods with links or errors first.
pub fn all_pods<'a>(state: &'a AppState, filter: &str) -> Vec<PodRow<'a>> {
    let Some(snap) = &state.latest else {
        return Vec::new();
    };
    let mut rows: Vec<PodRow<'a>> = snap
        .pods
        .iter()
        .filter(|s| hit(filter, &[&s.pod.name, &s.pod.namespace, &s.pod.app]))
        .map(|s| {
            let links = state.topo.per_group(s);
            PodRow {
                pod: &s.pod,
                conns: links.values().sum(),
                links,
                error: s.result.as_ref().err().map(String::as_str),
                last_seen: None,
            }
        })
        .collect();
    rows.sort_by_key(|r| (Reverse(r.conns), r.error.is_none(), r.pod.key()));
    rows
}

pub fn events<'a>(state: &'a AppState, filter: &str) -> Vec<&'a Event> {
    state
        .events
        .iter()
        .filter(|e| hit(filter, &[&e.text]))
        .collect()
}

pub fn keyspace(ui: &Ui) -> &[DbStat] {
    match &ui.db.info {
        Load::Ready(info) => &info.dbs,
        _ => &[],
    }
}

pub fn keys(ui: &Ui) -> &[KeyInfo] {
    match &ui.db.keys {
        Load::Ready(page) => &page.keys,
        _ => &[],
    }
}

/// Row count of the current screen.
pub fn len(ui: &Ui) -> usize {
    let (s, f) = (&ui.state, ui.filter());
    match ui.screen() {
        Screen::Groups => groups(s, f).len(),
        Screen::Group {
            id,
            tab: GroupTab::Nodes,
        } => nodes(s, id, f).len(),
        Screen::Group {
            id,
            tab: GroupTab::Clients,
        } => clients(s, id, f).len(),
        Screen::Group {
            id,
            tab: GroupTab::Pods,
        } => group_pods(s, id, None, f).len(),
        Screen::Clients {
            group,
            namespace,
            app,
        } => group_pods(s, group, Some((namespace, app)), f).len(),
        Screen::Node { .. } => keyspace(ui).len(),
        Screen::Keys { .. } => keys(ui).len(),
        Screen::Pods => all_pods(s, f).len(),
        Screen::Events => events(s, f).len(),
        Screen::Value { .. } | Screen::Pod { .. } => 0,
    }
}

/// The screen `Enter` opens from row `i` of the current screen.
pub fn target(ui: &Ui, i: usize) -> Option<Screen> {
    let (s, f) = (&ui.state, ui.filter());
    match ui.screen().clone() {
        Screen::Groups => groups(s, f).get(i).map(|r| Screen::Group {
            id: r.group.id.clone(),
            tab: GroupTab::Nodes,
        }),
        Screen::Group {
            id,
            tab: GroupTab::Nodes,
        } => nodes(s, &id, f).get(i).map(|r| Screen::Node {
            group: id.clone(),
            node: r.node.id.clone(),
        }),
        Screen::Group {
            id,
            tab: GroupTab::Clients,
        } => clients(s, &id, f).get(i).map(|r| Screen::Clients {
            group: id.clone(),
            namespace: r.namespace.clone(),
            app: r.app.clone(),
        }),
        Screen::Group {
            id,
            tab: GroupTab::Pods,
        } => group_pods(s, &id, None, f)
            .get(i)
            .map(|r| Screen::Pod { key: r.pod.key() }),
        Screen::Clients {
            group,
            namespace,
            app,
        } => group_pods(s, &group, Some((&namespace, &app)), f)
            .get(i)
            .map(|r| Screen::Pod { key: r.pod.key() }),
        Screen::Pods => all_pods(s, f)
            .get(i)
            .map(|r| Screen::Pod { key: r.pod.key() }),
        Screen::Node { group, node } => keyspace(ui).get(i).map(|d| Screen::Keys {
            group,
            node,
            db: d.db,
        }),
        Screen::Keys { group, node, db } => keys(ui).get(i).map(|k| Screen::Value {
            group,
            node,
            db,
            key: k.key.clone(),
            kind: k.kind.clone(),
        }),
        Screen::Value { .. } | Screen::Pod { .. } | Screen::Events => None,
    }
}

/// The node IP and the pods that can reach it. Pods that already run a probe come first so
/// queries add as few ephemeral containers as possible, then pods on that node, then other
/// pods of the group, which share its security group in practice.
pub fn db_target(state: &AppState, group: &str, node: &str) -> Result<DbTarget, String> {
    let snap = state
        .latest
        .as_ref()
        .ok_or("waiting for the first refresh")?;
    let host = *snap
        .ips
        .get(node)
        .ok_or_else(|| format!("no ENI IP for {node} (ec2:DescribeNetworkInterfaces)"))?;
    let mut candidates: Vec<(bool, u32, u32, &PodRef)> = snap
        .pods
        .iter()
        .filter_map(|s| {
            let per_node = state.topo.per_node(s);
            let on_node = per_node.get(node).copied().unwrap_or(0);
            let on_group: u32 = per_node
                .iter()
                .filter(|(n, _)| state.topo.group_of(n) == Some(group))
                .map(|(_, c)| *c)
                .sum();
            (on_group > 0).then_some((s.pod.probe, on_node, on_group, &s.pod))
        })
        .collect();
    if candidates.is_empty() {
        return Err(format!(
            "no scanned pod is connected to {group}, so nothing can reach it. Check that the kube context and namespaces (-c, -n, -A) belong to the same account as the AWS profile"
        ));
    }
    candidates.sort_by_key(|(probe, node, group, p)| {
        (Reverse(*probe), Reverse(*node), Reverse(*group), p.key())
    });
    Ok(DbTarget {
        host,
        gateways: candidates
            .into_iter()
            .map(|(_, _, _, p)| p.clone())
            .collect(),
    })
}

pub fn key_label(k: &KeyInfo) -> String {
    printable(&k.key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::state;

    #[test]
    fn groups_and_nodes() {
        let s = state();
        let g = groups(&s, "");
        assert_eq!(
            g.iter().map(|r| r.group.id.as_str()).collect::<Vec<_>>(),
            ["billing", "orders"]
        );
        assert_eq!(g[1].stats.pods, 3);
        let n = nodes(&s, "orders", "");
        assert_eq!(n.len(), 2);
        assert_eq!((n[0].pod_conns, n[1].pod_conns), (4, 3));
        assert_eq!(n[0].ip, Some("10.0.0.1".parse().unwrap()));
        assert_eq!(nodes(&s, "orders", "replica").len(), 1);
        assert!(nodes(&s, "nope", "").is_empty());
    }

    #[test]
    fn clients_and_pods_of_a_group() {
        let s = state();
        let c = clients(&s, "orders", "");
        assert_eq!(
            c.iter()
                .map(|r| (r.app.as_str(), r.pods, r.conns))
                .collect::<Vec<_>>(),
            [("api", 2, 6), ("ledger", 1, 1)]
        );
        assert!(c[0].last_seen.is_some());
        assert_eq!(clients(&s, "orders", "pay").len(), 1);
        let p = group_pods(&s, "orders", Some(("shop", "api")), "");
        assert_eq!(
            p.iter().map(|r| r.pod.name.as_str()).collect::<Vec<_>>(),
            ["api-1", "api-2"]
        );
        assert_eq!(p[0].links.len(), 2);
        assert_eq!(group_pods(&s, "billing", None, "").len(), 1);
    }

    #[test]
    fn all_pods_include_errors() {
        let s = state();
        let rows = all_pods(&s, "");
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].pod.name, "api-1");
        let names: Vec<_> = rows.iter().map(|r| r.pod.name.as_str()).collect();
        assert_eq!(names, ["api-1", "ledger-1", "api-2", "batch-1"]);
        assert_eq!(rows[3].error, Some("cat: not found"));
        assert_eq!(all_pods(&s, "ledger").len(), 1);
        assert_eq!(events(&s, "start").len(), 1);
    }

    #[test]
    fn db_target_orders_gateways() {
        let s = state();
        let t = db_target(&s, "orders", "orders-002").unwrap();
        let names: Vec<_> = t.gateways.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["api-1", "api-2", "ledger-1"]);
        let mut s = s;
        if let Some(snap) = s.latest.as_mut() {
            snap.pods[2].pod.probe = true;
        }
        let t = db_target(&s, "orders", "orders-002").unwrap();
        assert_eq!(t.gateways[0].name, "ledger-1", "existing probe first");
        assert!(
            db_target(&s, "orders", "missing-001")
                .unwrap_err()
                .contains("no ENI IP")
        );
        assert!(
            db_target(&s, "search", "orders-001")
                .unwrap_err()
                .contains("kube context")
        );
        assert!(db_target(&AppState::new(), "orders", "orders-001").is_err());
    }

    #[test]
    fn empty_state_has_no_rows() {
        let s = AppState::new();
        assert!(groups(&s, "").is_empty());
        assert!(nodes(&s, "x", "").is_empty());
        assert!(group_pods(&s, "x", None, "").is_empty());
        assert!(all_pods(&s, "").is_empty());
    }
}
