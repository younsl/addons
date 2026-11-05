mod events;
mod series;

use std::collections::{BTreeSet, HashMap};
use std::time::Instant;

use chrono::{DateTime, Local};

pub use events::{Event, EventLog, Level};
pub use series::Series;

use crate::collector::Msg;
use crate::types::{Snapshot, Topology};

const MAX_EVENTS: usize = 500;
const MAX_SERIES: usize = 120;

/// Per-group history for sparklines.
#[derive(Debug)]
pub struct GroupSeries {
    pub connections: Series,
    pub items: Series,
}

impl Default for GroupSeries {
    fn default() -> Self {
        Self {
            connections: Series::new(MAX_SERIES),
            items: Series::new(MAX_SERIES),
        }
    }
}

pub struct AppState {
    pub latest: Option<Snapshot>,
    pub topo: Topology,
    pub collecting_since: Option<Instant>,
    pub events: EventLog,
    pub series: HashMap<String, GroupSeries>,
    /// Pod key to the groups it was last seen connected to.
    links: HashMap<String, BTreeSet<String>>,
    /// `(pod key, group)` to the last time the pod held a connection to the group.
    pub last_seen: HashMap<(String, String), DateTime<Local>>,
    last_errors: Vec<String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        let mut events = EventLog::new(MAX_EVENTS);
        events.push(Local::now(), Level::Info, "started");
        Self {
            latest: None,
            topo: Topology::default(),
            collecting_since: None,
            events,
            series: HashMap::new(),
            links: HashMap::new(),
            last_seen: HashMap::new(),
            last_errors: Vec::new(),
        }
    }

    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::RoundStarted => self.collecting_since = Some(Instant::now()),
            Msg::Snapshot(snap) => {
                self.collecting_since = None;
                self.apply(*snap);
            }
        }
    }

    fn apply(&mut self, snap: Snapshot) {
        let at = snap.at;
        let topo = Topology::new(&snap);
        self.record_errors(at, &snap.errors);
        if let Some(prev) = &self.latest {
            let changes = group_changes(prev, &snap);
            for c in changes {
                self.events.push(at, Level::Change, c);
            }
        }
        if snap.pods_listed {
            self.record_links(at, &snap, &topo);
        }
        for g in &snap.groups {
            let stats = snap.stats(g, &topo);
            let s = self.series.entry(g.id.clone()).or_default();
            s.connections.push(round(stats.connections));
            s.items.push(round(stats.items));
        }
        self.topo = topo;
        self.latest = Some(snap);
    }

    fn record_errors(&mut self, at: DateTime<Local>, errors: &[String]) {
        if errors == self.last_errors.as_slice() {
            return;
        }
        for e in errors {
            self.events.push(at, Level::Warn, e.clone());
        }
        if errors.is_empty() {
            self.events.push(at, Level::Info, "errors cleared");
        }
        self.last_errors = errors.to_vec();
    }

    /// Connect and disconnect events, skipped on the first round so startup is quiet.
    fn record_links(&mut self, at: DateTime<Local>, snap: &Snapshot, topo: &Topology) {
        let first = self.latest.is_none();
        let mut now: HashMap<String, BTreeSet<String>> = HashMap::new();
        for scan in &snap.pods {
            let key = scan.pod.key();
            let groups: BTreeSet<String> = topo.per_group(scan).into_keys().collect();
            for g in &groups {
                self.last_seen.insert((key.clone(), g.clone()), at);
            }
            // A failed scan says nothing about links, so the previous set carries over.
            let groups = if scan.result.is_err() {
                self.links.get(&key).cloned().unwrap_or_default()
            } else {
                groups
            };
            now.insert(key, groups);
        }
        if !first {
            for (key, groups) in &now {
                let before = self.links.get(key).cloned().unwrap_or_default();
                for g in groups.difference(&before) {
                    self.events
                        .push(at, Level::Change, format!("{key} connected to {g}"));
                }
                for g in before.difference(groups) {
                    self.events
                        .push(at, Level::Change, format!("{key} disconnected from {g}"));
                }
            }
            for (key, before) in &self.links {
                if !now.contains_key(key) && !before.is_empty() {
                    let list: Vec<&str> = before.iter().map(String::as_str).collect();
                    self.events.push(
                        at,
                        Level::Info,
                        format!("{key} gone (was on {})", list.join(", ")),
                    );
                }
            }
        }
        self.links = now;
    }
}

fn group_changes(prev: &Snapshot, next: &Snapshot) -> Vec<String> {
    let mut out = Vec::new();
    for g in &next.groups {
        match prev.group(&g.id) {
            None => out.push(format!("{} appeared ({})", g.id, g.status)),
            Some(p) if p.status != g.status => {
                out.push(format!("{}: {} -> {}", g.id, p.status, g.status));
            }
            Some(p) => {
                for n in &g.nodes {
                    let was = p.nodes.iter().find(|o| o.id == n.id);
                    if let Some(o) = was.filter(|o| o.role != n.role) {
                        out.push(format!(
                            "{}: {} -> {}",
                            n.id,
                            o.role.label(),
                            n.role.label()
                        ));
                    }
                }
            }
        }
    }
    for g in &prev.groups {
        if next.group(&g.id).is_none() {
            out.push(format!("{} removed", g.id));
        }
    }
    for (node, ip) in &next.ips {
        if prev.ips.get(node).is_some_and(|old| old != ip) {
            out.push(format!("{node} IP changed to {ip}"));
        }
    }
    out
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn round(v: Option<f64>) -> u64 {
    v.map_or(0, |v| v.max(0.0).round() as u64)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::types::Role;
    use crate::types::tests::{scan, snapshot};

    pub fn state() -> AppState {
        let mut s = AppState::new();
        s.on_msg(Msg::Snapshot(Box::new(snapshot())));
        s
    }

    fn texts(s: &AppState) -> Vec<String> {
        s.events.iter().map(|e| e.text.clone()).collect()
    }

    #[test]
    fn first_round_is_quiet_and_records() {
        let s = state();
        assert_eq!(texts(&s), ["started"]);
        assert_eq!(s.series["orders"].connections.last(), 70);
        assert_eq!(s.series["orders"].items.last(), 1000);
        assert!(
            s.last_seen
                .contains_key(&("shop/api-1".into(), "orders".into()))
        );
        assert_eq!(s.topo.group_of("billing-001"), Some("billing"));
    }

    #[test]
    fn link_and_group_changes_become_events() {
        let mut s = state();
        s.on_msg(Msg::RoundStarted);
        assert!(s.collecting_since.is_some());
        let mut next = snapshot();
        next.pods.remove(0);
        next.pods[0] = scan("shop", "api-2", "api", &[("10.0.1.1", 1)]);
        next.groups[1].status = "modifying".into();
        next.groups[0].nodes[0].role = Role::Replica;
        next.groups.push(crate::types::tests::group("search"));
        next.ips
            .insert("orders-001".into(), "10.0.0.9".parse().unwrap());
        s.on_msg(Msg::Snapshot(Box::new(next)));
        let t = texts(&s);
        for expected in [
            "shop/api-2 connected to billing",
            "shop/api-2 disconnected from orders",
            "shop/api-1 gone (was on orders)",
            "orders: available -> modifying",
            "billing-001: primary -> replica",
            "search appeared (available)",
            "orders-001 IP changed to 10.0.0.9",
        ] {
            assert!(t.contains(&expected.to_owned()), "{expected}: {t:?}");
        }
        assert!(s.collecting_since.is_none());

        let mut gone = snapshot();
        gone.groups.retain(|g| g.id != "billing");
        s.on_msg(Msg::Snapshot(Box::new(gone)));
        assert!(texts(&s).contains(&"billing removed".to_owned()));
    }

    #[test]
    fn failed_scan_keeps_links() {
        let mut s = state();
        let mut next = snapshot();
        next.pods[0].result = Err("timeout".into());
        s.on_msg(Msg::Snapshot(Box::new(next)));
        assert!(!texts(&s).iter().any(|t| t.contains("api-1 disconnected")));
    }

    #[test]
    fn errors_logged_once_until_cleared() {
        let mut s = state();
        let mut failing = snapshot();
        failing.errors = vec!["metrics: denied".into()];
        failing.pods_listed = false;
        s.on_msg(Msg::Snapshot(Box::new(failing.clone())));
        s.on_msg(Msg::Snapshot(Box::new(failing)));
        s.on_msg(Msg::Snapshot(Box::new(snapshot())));
        let t = texts(&s);
        assert_eq!(t.iter().filter(|e| e.contains("denied")).count(), 1);
        assert_eq!(t[0], "errors cleared");
    }

    #[test]
    fn rounding_clamps() {
        assert_eq!(round(None), 0);
        assert_eq!(round(Some(-3.0)), 0);
        assert_eq!(round(Some(2.6)), 3);
        assert_eq!(AppState::default().events.len(), 1);
    }
}
