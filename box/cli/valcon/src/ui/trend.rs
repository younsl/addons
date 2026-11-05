//! k9s-style change markers: a table value that rose or fell on the latest refresh shows an
//! arrow next to it for a few seconds.

use std::collections::HashMap;

use chrono::{DateTime, Local, TimeDelta};

use super::format::{count, pct};
use super::rows;
use crate::state::AppState;

const HOLD: TimeDelta = TimeDelta::seconds(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
}

impl Dir {
    pub const fn arrow(self) -> &'static str {
        match self {
            Self::Up => "↑",
            Self::Down => "↓",
        }
    }
}

struct Seen {
    raw: f64,
    shown: String,
    moved: Option<(Dir, DateTime<Local>)>,
}

#[derive(Default)]
pub struct Trends {
    seen: HashMap<String, Seen>,
}

impl Trends {
    /// Marks values whose displayed text changed since the last refresh, so rounding noise
    /// never shows an arrow. Keys missing from this round are forgotten.
    pub fn observe(&mut self, at: DateTime<Local>, values: Vec<(String, f64, String)>) {
        let mut next = HashMap::with_capacity(values.len());
        for (key, raw, shown) in values {
            let moved = match self.seen.remove(&key) {
                Some(old) if old.shown != shown => dir(old.raw, raw).map(|d| (d, at)).or(old.moved),
                Some(old) => old.moved,
                None => None,
            };
            next.insert(key, Seen { raw, shown, moved });
        }
        self.seen = next;
    }

    pub fn at(&self, key: &str, now: DateTime<Local>) -> Option<Dir> {
        let (d, when) = self.seen.get(key)?.moved?;
        (now - when < HOLD).then_some(d)
    }
}

fn dir(old: f64, new: f64) -> Option<Dir> {
    if new > old {
        Some(Dir::Up)
    } else if new < old {
        Some(Dir::Down)
    } else {
        None
    }
}

pub fn group(id: &str, column: &str) -> String {
    format!("group/{id}/{column}")
}

pub fn node(id: &str, column: &str) -> String {
    format!("node/{id}/{column}")
}

pub fn client(group: &str, namespace: &str, app: &str, column: &str) -> String {
    format!("client/{group}/{namespace}/{app}/{column}")
}

pub fn group_pod(group: &str, pod: &str) -> String {
    format!("group-pod/{group}/{pod}")
}

pub fn pod(pod: &str) -> String {
    format!("pod/{pod}")
}

fn num(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

/// Every numeric table cell of the list screens, keyed the way the renderers look them up.
pub fn values(state: &AppState) -> Vec<(String, f64, String)> {
    let mut out = Vec::new();
    let mut put = |key: String, raw: Option<f64>, shown: String| {
        if let Some(raw) = raw {
            out.push((key, raw, shown));
        }
    };
    for r in rows::groups(state, "") {
        let (id, s) = (r.group.id.as_str(), r.stats);
        put(group(id, "items"), s.items, count(s.items));
        put(group(id, "conns"), s.connections, count(s.connections));
        put(group(id, "cpu"), s.cpu, pct(s.cpu));
        put(group(id, "mem"), s.memory, pct(s.memory));
        put(group(id, "pods"), Some(num(s.pods)), s.pods.to_string());
        for n in rows::nodes(state, id, "") {
            let (nid, m) = (n.node.id.as_str(), n.metrics);
            put(node(nid, "items"), m.items, count(m.items));
            put(node(nid, "conns"), m.connections, count(m.connections));
            put(node(nid, "cpu"), m.cpu, pct(m.cpu));
            put(node(nid, "mem"), m.memory, pct(m.memory));
            put(
                node(nid, "pod-conns"),
                Some(f64::from(n.pod_conns)),
                n.pod_conns.to_string(),
            );
        }
        for c in rows::clients(state, id, "") {
            put(
                client(id, &c.namespace, &c.app, "pods"),
                Some(num(c.pods)),
                c.pods.to_string(),
            );
            put(
                client(id, &c.namespace, &c.app, "conns"),
                Some(f64::from(c.conns)),
                c.conns.to_string(),
            );
        }
        for p in rows::group_pods(state, id, None, "") {
            put(
                group_pod(id, &p.pod.key()),
                Some(f64::from(p.conns)),
                p.conns.to_string(),
            );
        }
    }
    for p in rows::all_pods(state, "") {
        put(
            pod(&p.pod.key()),
            Some(f64::from(p.conns)),
            p.conns.to_string(),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::state;

    fn v(key: &str, raw: f64, shown: &str) -> (String, f64, String) {
        (key.into(), raw, shown.into())
    }

    #[test]
    fn arrows_follow_display_changes_and_expire() {
        let t0 = Local::now();
        let mut t = Trends::default();
        t.observe(
            t0,
            vec![v("a", 10.0, "10"), v("b", 5.0, "5"), v("c", 1.0, "1.0%")],
        );
        assert_eq!(t.at("a", t0), None, "first round has nothing to compare");

        let t1 = t0 + TimeDelta::seconds(15);
        t.observe(
            t1,
            vec![v("a", 12.0, "12"), v("b", 4.0, "4"), v("c", 1.01, "1.0%")],
        );
        assert_eq!(t.at("a", t1), Some(Dir::Up));
        assert_eq!(t.at("b", t1), Some(Dir::Down));
        assert_eq!(t.at("c", t1), None, "same display text");
        assert_eq!(t.at("a", t1 + TimeDelta::seconds(2)), Some(Dir::Up));
        assert_eq!(t.at("a", t1 + HOLD), None);

        t.observe(t1 + TimeDelta::seconds(1), vec![v("a", 12.0, "12")]);
        assert_eq!(t.at("a", t1 + TimeDelta::seconds(1)), Some(Dir::Up));
        assert_eq!(t.at("b", t1), None, "dropped keys are forgotten");
        assert_eq!((Dir::Up.arrow(), Dir::Down.arrow()), ("↑", "↓"));
    }

    #[test]
    fn values_cover_every_list() {
        let keys: Vec<String> = values(&state()).into_iter().map(|(k, _, _)| k).collect();
        for expected in [
            group("orders", "conns"),
            group("orders", "pods"),
            node("orders-001", "pod-conns"),
            client("orders", "shop", "api", "conns"),
            group_pod("orders", "shop/api-1"),
            pod("shop/api-1"),
        ] {
            assert!(keys.contains(&expected), "{expected}: {keys:?}");
        }
        assert_eq!(values(&AppState::new()).len(), 0);
    }
}
