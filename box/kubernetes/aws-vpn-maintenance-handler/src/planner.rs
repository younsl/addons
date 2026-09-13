//! Decides which tunnel, if any, may be replaced right now. `ReplaceVpnTunnel`
//! is irreversible, so every safety rule is checked here before anything is
//! proposed. No AWS calls, no state.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::aws::types::since;
use crate::aws::{Connection, Maintenance, Tunnel, TunnelStatus};
use crate::humanize;

/// A machine-readable reason a tunnel was not proposed. The values are stable
/// because they appear as a Prometheus metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reason {
    /// `EnableTunnelLifecycleControl` is off, so AWS never offers its
    /// maintenance for early application. Permanent until someone enables it.
    LifecycleControlDisabled,
    /// Window shut, or too little of it left to verify in.
    WindowClosed,
    /// AWS has nothing queued. The normal case.
    NoPendingMaintenance,
    /// Connection state is not "available".
    ConnectionUnavailable,
    /// Not exactly two tunnels, so nothing to fail over to.
    TunnelCount,
    /// The surviving tunnel is DOWN.
    PeerDown,
    /// The surviving tunnel came UP too recently to trust.
    PeerUnstable,
    /// Peer is UP but carries too few routes, so traffic would blackhole.
    PeerNoRoutes,
    /// This connection had a tunnel replaced too recently.
    Cooldown,
    /// Replacements are serialized, one is running.
    ReplacementInFlight,
    /// An approval is already outstanding in Slack.
    AwaitingApproval,
    /// The metric store says the tunnel is not quiet enough right now.
    /// Evaluated outside the planner, since it needs a network call.
    TrafficHigh,
}

impl Reason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LifecycleControlDisabled => "lifecycle_control_disabled",
            Self::WindowClosed => "window_closed",
            Self::NoPendingMaintenance => "no_pending_maintenance",
            Self::ConnectionUnavailable => "connection_unavailable",
            Self::TunnelCount => "tunnel_count",
            Self::PeerDown => "peer_down",
            Self::PeerUnstable => "peer_unstable",
            Self::PeerNoRoutes => "peer_no_routes",
            Self::Cooldown => "cooldown",
            Self::ReplacementInFlight => "replacement_in_flight",
            Self::AwaitingApproval => "awaiting_approval",
            Self::TrafficHigh => "traffic_high",
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A tunnel that was considered and rejected, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    pub connection_id: String,
    /// The Name tag, or empty. Carried because a rejection can be reported to a
    /// human, who recognizes the name and not the ID.
    pub connection_name: String,
    pub tunnel_ip: String,
    pub reason: Reason,
    /// The human-readable explanation for logs and Slack.
    pub detail: String,
    /// Separates "nothing to do" from "held back by a rule".
    pub pending_maintenance: bool,
    /// The same identity a candidate for this tunnel would carry, so a tunnel
    /// held back and later proposed is recognizably one maintenance cycle.
    pub request_id: String,
    /// When AWS applies the maintenance itself, `None` when unpublished.
    pub deadline: Option<DateTime<Utc>>,
    pub deadline_in: Duration,
}

/// A tunnel cleared by every preflight check and ready to be proposed for
/// approval.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Candidate {
    pub connection: Connection,
    pub tunnel: Tunnel,
    pub peer: Tunnel,
    pub maintenance: Maintenance,
    /// Derived from connection, tunnel, and deadline: stable across restarts
    /// within one maintenance cycle, new when AWS queues new work.
    pub request_id: String,
    /// The AWS auto-apply deadline is close.
    pub escalate: bool,
    /// The time left before AWS applies the maintenance itself.
    pub deadline_in: Duration,
    /// Reached by continuing from an earlier replacement on the same connection
    /// rather than by a fresh proposal.
    pub chained: bool,
    /// When the previous tunnel was replaced, set only when chained.
    pub sibling_replaced_at: Option<DateTime<Utc>>,
    /// The connection's other tunnels that also have maintenance pending, in
    /// replacement order. Approving this candidate authorizes the whole queue.
    ///
    /// Their peer checks are deliberately not evaluated here: the tunnel that
    /// will carry traffic for the next step is the one being replaced now, so
    /// each queued tunnel is re-checked in full at its own turn.
    pub queue: Vec<String>,
}

impl Candidate {
    /// Renders the candidate for logs and notifications.
    #[must_use]
    pub fn label(&self) -> String {
        format!(
            "{} tunnel {}",
            self.connection.label(),
            self.tunnel.outside_ip
        )
    }
}

/// The result of one evaluation pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Eligible tunnels, nearest AWS deadline first. Only the first is acted on
    /// per pass, since replacements are serialized.
    pub candidates: Vec<Candidate>,
    /// Every tunnel that was considered and rejected.
    pub blocked: Vec<Blocked>,
}

impl Plan {
    /// The blocked entries worth an operator's attention: tunnels with queued
    /// maintenance being held back, and tunnels permanently ineligible because
    /// lifecycle control is off. Tunnels with nothing queued are noise.
    #[must_use]
    pub fn held(&self) -> Vec<&Blocked> {
        self.blocked
            .iter()
            .filter(|b| b.pending_maintenance || b.reason == Reason::LifecycleControlDisabled)
            .collect()
    }
}

/// The safety limits applied to every candidate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Thresholds {
    pub peer_min_stable_for: Duration,
    pub peer_min_accepted_routes: i32,
    pub per_connection_cooldown: Duration,
    pub chain_sibling_tunnel: bool,
    pub escalate_before: Duration,
}

/// The persisted per-connection history the planner consults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_field_names)]
pub struct ConnectionState {
    /// Starts the cooldown. A failed attempt sets it too.
    pub last_replacement_at: Option<DateTime<Utc>>,
    /// The tunnel that was replaced, which is what distinguishes the sibling
    /// tunnel from a repeat attempt on the same one.
    pub last_tunnel_ip: String,
    /// Whether that replacement ended healthy. Only a healthy one may be
    /// chained from.
    pub last_succeeded: bool,
}

/// Everything one evaluation pass needs.
#[derive(Debug, Clone, Default)]
pub struct Input {
    pub now: DateTime<Utc>,
    pub connections: Vec<Connection>,
    /// Connection ID to per-tunnel maintenance state.
    pub statuses: HashMap<String, Vec<TunnelStatus>>,
    pub window_open: bool,
    /// Explains a closed window, for the blocked reason.
    pub window_detail: String,
    pub replacement_in_flight: bool,
    /// Request IDs with an outstanding Slack approval.
    pub awaiting_approval: HashSet<String>,
    /// Per-connection replacement history, keyed by connection ID.
    pub history: HashMap<String, ConnectionState>,
    pub thresholds: Thresholds,
}

/// Walks every tunnel and returns the eligible candidates plus the reason each
/// other tunnel was rejected. Checks run most-common-first so the reported
/// reason is the informative one, not "window closed" for a tunnel with no
/// work.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn evaluate(input: &Input) -> Plan {
    let mut out = Plan::default();
    let th = &input.thresholds;

    for conn in &input.connections {
        let Some(statuses) = input.statuses.get(&conn.id) else {
            continue;
        };
        for st in statuses {
            let request_id = request_id(&conn.id, &st.tunnel.outside_ip, &st.maintenance);
            let deadline_in = st.maintenance.deadline_in(input.now);
            let mut block = |reason: Reason, detail: String| {
                out.blocked.push(Blocked {
                    connection_id: conn.id.clone(),
                    connection_name: conn.name.clone(),
                    tunnel_ip: st.tunnel.outside_ip.clone(),
                    reason,
                    detail,
                    pending_maintenance: st.maintenance.pending,
                    request_id: request_id.clone(),
                    deadline: st.maintenance.auto_applied_after,
                    deadline_in,
                });
            };

            // Checked before pending maintenance: with lifecycle control off,
            // AWS never reports maintenance as available, so this would
            // otherwise look like "nothing to do" and hide a configuration gap.
            if !st.tunnel.lifecycle_control {
                block(
                    Reason::LifecycleControlDisabled,
                    "tunnel endpoint lifecycle control is disabled, so AWS applies maintenance on its own schedule and it cannot be triggered early; enable it with ModifyVpnTunnelOptions (EnableTunnelLifecycleControl) to bring this tunnel under control".to_string(),
                );
                continue;
            }
            if !st.maintenance.pending {
                block(
                    Reason::NoPendingMaintenance,
                    "no pending tunnel endpoint maintenance".to_string(),
                );
                continue;
            }
            if conn.state != "available" {
                block(
                    Reason::ConnectionUnavailable,
                    format!(
                        "vpn connection state is {:?}, not \"available\"",
                        conn.state
                    ),
                );
                continue;
            }
            let Some(peer) = conn.peer(&st.tunnel.outside_ip) else {
                block(
                    Reason::TunnelCount,
                    format!(
                        "connection reports {} tunnel(s); a replacement needs exactly 2 so traffic can fail over",
                        conn.tunnels.len()
                    ),
                );
                continue;
            };
            if !peer.up {
                block(
                    Reason::PeerDown,
                    format!(
                        "peer tunnel {} is DOWN ({}); replacing this tunnel would drop the whole connection",
                        peer.outside_ip,
                        peer_status_message(peer)
                    ),
                );
                continue;
            }
            let stable = peer.stable_for(input.now);
            if stable < th.peer_min_stable_for {
                block(
                    Reason::PeerUnstable,
                    format!(
                        "peer tunnel {} has only been stable for {}, {} required (possible flapping)",
                        peer.outside_ip,
                        humanize::go_duration(humanize::round_to_second(stable)),
                        humanize::go_duration(th.peer_min_stable_for)
                    ),
                );
                continue;
            }
            // Static-routes-only connections never report routes, so UP is the
            // only signal there.
            if !conn.static_routes_only && peer.accepted_routes < th.peer_min_accepted_routes {
                block(
                    Reason::PeerNoRoutes,
                    format!(
                        "peer tunnel {} is UP but accepts {} BGP route(s), {} required; traffic would blackhole",
                        peer.outside_ip, peer.accepted_routes, th.peer_min_accepted_routes
                    ),
                );
                continue;
            }
            // Chaining the sibling tunnel is what lets both tunnels of a
            // connection be finished in one window. It is safe only because the
            // peer checks above already ran against the just-replaced tunnel.
            let mut chained = false;
            let hist = input.history.get(&conn.id);
            if let Some(hist) = hist
                && let Some(last) = hist.last_replacement_at
            {
                let since_last = since(input.now, last);
                if since_last < th.per_connection_cooldown {
                    let sibling = !hist.last_tunnel_ip.is_empty()
                        && hist.last_tunnel_ip != st.tunnel.outside_ip;
                    if !th.chain_sibling_tunnel || !sibling || !hist.last_succeeded {
                        block(
                            Reason::Cooldown,
                            cooldown_detail(hist, &st.tunnel.outside_ip, since_last, th),
                        );
                        continue;
                    }
                    chained = true;
                }
            }

            if input.awaiting_approval.contains(&request_id) {
                block(
                    Reason::AwaitingApproval,
                    "an approval request is already outstanding in Slack".to_string(),
                );
                continue;
            }
            if input.replacement_in_flight {
                block(
                    Reason::ReplacementInFlight,
                    "another tunnel replacement is already running; replacements are serialized"
                        .to_string(),
                );
                continue;
            }
            if !input.window_open {
                block(Reason::WindowClosed, input.window_detail.clone());
                continue;
            }

            out.candidates.push(Candidate {
                connection: conn.clone(),
                tunnel: st.tunnel.clone(),
                peer: peer.clone(),
                maintenance: st.maintenance.clone(),
                request_id,
                // Zero means AWS published no deadline, which is not urgent.
                escalate: !deadline_in.is_zero() && deadline_in <= th.escalate_before,
                deadline_in,
                chained,
                sibling_replaced_at: hist.and_then(|h| h.last_replacement_at),
                queue: queued_siblings(statuses, &st.tunnel.outside_ip),
            });
        }
    }

    sort_by_urgency(&mut out.candidates);
    out
}

/// Puts the nearest AWS deadline first, so the tunnel most likely to be
/// replaced at an uncontrolled time gets the window. No deadline sorts last;
/// ties break on connection ID then tunnel IP for determinism.
fn sort_by_urgency(cs: &mut [Candidate]) {
    let key = |c: &Candidate| {
        if c.deadline_in.is_zero() {
            Duration::MAX
        } else {
            c.deadline_in
        }
    };
    cs.sort_by(|a, b| {
        key(a)
            .cmp(&key(b))
            .then_with(|| a.connection.id.cmp(&b.connection.id))
            .then_with(|| a.tunnel.outside_ip.cmp(&b.tunnel.outside_ip))
    });
}

/// Lists the connection's other tunnels with maintenance pending, so one
/// approval can carry the whole connection. Only lifecycle control is required
/// here; everything else is re-evaluated when the tunnel's turn comes.
fn queued_siblings(statuses: &[TunnelStatus], current_ip: &str) -> Vec<String> {
    statuses
        .iter()
        .filter(|st| {
            st.tunnel.outside_ip != current_ip
                && st.maintenance.pending
                && st.tunnel.lifecycle_control
        })
        .map(|st| st.tunnel.outside_ip.clone())
        .collect()
}

/// Explains a cooldown block, naming the reason chaining did not apply so an
/// operator can tell "waiting out a failure" from "chaining is switched off".
fn cooldown_detail(
    hist: &ConnectionState,
    tunnel_ip: &str,
    since_last: Duration,
    th: &Thresholds,
) -> String {
    let base = format!(
        "a tunnel of this connection was replaced {} ago; cooldown is {}",
        humanize::go_duration(humanize::round_to_minute(since_last)),
        humanize::go_duration(th.per_connection_cooldown)
    );
    if hist.last_tunnel_ip == tunnel_ip {
        format!("{base}, and this is the same tunnel that was just replaced")
    } else if !hist.last_succeeded {
        format!("{base}, and that replacement did not end healthy, so its sibling is not chained")
    } else if !th.chain_sibling_tunnel {
        format!("{base} (sibling chaining is disabled)")
    } else {
        base
    }
}

/// The stable approval identity of one proposed replacement. Including the
/// deadline scopes it to a single maintenance cycle, so a restart reuses the
/// outstanding request and an old approval cannot authorize new work.
#[must_use]
pub fn request_id(connection_id: &str, tunnel_ip: &str, m: &Maintenance) -> String {
    let deadline = m.auto_applied_after.map_or(0, |t| t.timestamp());
    format!("{connection_id}|{tunnel_ip}|{deadline}")
}

/// Recovers the connection ID and tunnel IP from a request ID, so the state
/// `ConfigMap` only has to store the ID itself.
#[must_use]
pub fn split_request_id(request_id: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = request_id.split('|').collect();
    if parts.len() != 3 || parts[0].is_empty() || parts[1].is_empty() {
        return None;
    }
    Some((parts[0].to_string(), parts[1].to_string()))
}

/// Whether a request ID refers to the given connection and tunnel, ignoring
/// the deadline, which may move between proposal and execution.
#[must_use]
pub fn request_id_matches(request_id: &str, connection_id: &str, tunnel_ip: &str) -> bool {
    let prefix = format!("{connection_id}|{tunnel_ip}|");
    request_id.len() > prefix.len() && request_id.starts_with(&prefix)
}

fn peer_status_message(t: &Tunnel) -> &str {
    if t.status_message.is_empty() {
        "no status message"
    } else {
        &t.status_message
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    pub const NOW_UNIX: i64 = 1_785_000_000;

    #[must_use]
    pub fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(NOW_UNIX, 0).unwrap()
    }

    #[must_use]
    pub fn tunnel(ip: &str, up: bool, routes: i32, stable_secs: i64) -> Tunnel {
        Tunnel {
            outside_ip: ip.into(),
            up,
            status_message: if up { String::new() } else { "IKE down".into() },
            accepted_routes: routes,
            last_status_change: Some(now() - chrono::TimeDelta::seconds(stable_secs)),
            lifecycle_control: true,
        }
    }

    #[must_use]
    pub fn connection(id: &str) -> Connection {
        Connection {
            id: id.into(),
            name: "prod".into(),
            state: "available".into(),
            transit_gateway_id: "tgw-1".into(),
            customer_gateway_id: "cgw-1".into(),
            tunnels: vec![
                tunnel("1.1.1.1", true, 4, 3600),
                tunnel("2.2.2.2", true, 4, 3600),
            ],
            ..Connection::default()
        }
    }

    #[must_use]
    pub fn pending(deadline_hours: i64) -> Maintenance {
        Maintenance {
            pending: true,
            auto_applied_after: Some(now() + chrono::TimeDelta::hours(deadline_hours)),
            last_applied: None,
        }
    }

    #[must_use]
    pub fn statuses(conn: &Connection, m: &[Maintenance]) -> Vec<TunnelStatus> {
        conn.tunnels
            .iter()
            .zip(m)
            .map(|(t, m)| TunnelStatus {
                tunnel: t.clone(),
                maintenance: m.clone(),
            })
            .collect()
    }

    #[must_use]
    pub fn thresholds() -> Thresholds {
        Thresholds {
            peer_min_stable_for: Duration::from_secs(300),
            peer_min_accepted_routes: 1,
            per_connection_cooldown: Duration::from_secs(24 * 3600),
            chain_sibling_tunnel: true,
            escalate_before: Duration::from_secs(168 * 3600),
        }
    }

    #[must_use]
    pub fn input(conn: Connection, statuses: Vec<TunnelStatus>) -> Input {
        let mut map = HashMap::new();
        map.insert(conn.id.clone(), statuses);
        Input {
            now: now(),
            connections: vec![conn],
            statuses: map,
            window_open: true,
            window_detail: String::new(),
            replacement_in_flight: false,
            awaiting_approval: HashSet::new(),
            history: HashMap::new(),
            thresholds: thresholds(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn reasons(plan: &Plan) -> Vec<Reason> {
        plan.blocked.iter().map(|b| b.reason).collect()
    }

    #[test]
    fn proposes_the_pending_tunnel_with_a_healthy_peer() {
        let conn = connection("vpn-1");
        let st = statuses(&conn, &[pending(48), Maintenance::default()]);
        let plan = evaluate(&input(conn, st));
        assert_eq!(plan.candidates.len(), 1);
        let c = &plan.candidates[0];
        assert_eq!(c.tunnel.outside_ip, "1.1.1.1");
        assert_eq!(c.peer.outside_ip, "2.2.2.2");
        assert_eq!(
            c.request_id,
            format!("vpn-1|1.1.1.1|{}", NOW_UNIX + 48 * 3600)
        );
        assert!(c.escalate, "48h is inside the 168h horizon");
        assert!(c.queue.is_empty());
        assert!(!c.chained);
        assert_eq!(c.label(), "prod (vpn-1) tunnel 1.1.1.1");
        assert_eq!(reasons(&plan), vec![Reason::NoPendingMaintenance]);
        assert!(plan.held().is_empty(), "nothing queued is noise");
    }

    #[test]
    fn queues_the_sibling_when_both_are_pending() {
        let conn = connection("vpn-1");
        let st = statuses(&conn, &[pending(400), pending(400)]);
        let plan = evaluate(&input(conn, st));
        assert_eq!(plan.candidates.len(), 2);
        assert_eq!(plan.candidates[0].queue, vec!["2.2.2.2".to_string()]);
        assert_eq!(plan.candidates[1].queue, vec!["1.1.1.1".to_string()]);
        assert!(!plan.candidates[0].escalate, "400h is past the horizon");
    }

    #[test]
    fn lifecycle_control_off_is_reported_before_anything_else() {
        let mut conn = connection("vpn-1");
        conn.tunnels[0].lifecycle_control = false;
        let st = statuses(&conn, &[Maintenance::default(), pending(48)]);
        let plan = evaluate(&input(conn, st));
        assert_eq!(plan.blocked[0].reason, Reason::LifecycleControlDisabled);
        assert!(plan.blocked[0].detail.contains("ModifyVpnTunnelOptions"));
        assert_eq!(
            plan.held().len(),
            1,
            "unmanageable tunnels are worth attention"
        );
        // The sibling is not queued behind the candidate either.
        assert_eq!(plan.candidates.len(), 1);
        assert!(plan.candidates[0].queue.is_empty());
    }

    #[test]
    fn peer_checks_block_in_order() {
        let base = connection("vpn-1");
        let cases: Vec<(Box<dyn Fn(&mut Connection)>, Reason, &str)> = vec![
            (
                Box::new(|c| c.state = "pending".into()),
                Reason::ConnectionUnavailable,
                "not \"available\"",
            ),
            (
                Box::new(|c| c.tunnels.truncate(1)),
                Reason::TunnelCount,
                "reports 1 tunnel(s)",
            ),
            (
                Box::new(|c| {
                    c.tunnels[1].up = false;
                    c.tunnels[1].status_message = "IKE down".into();
                }),
                Reason::PeerDown,
                "IKE down",
            ),
            (
                Box::new(|c| c.tunnels[1] = tunnel("2.2.2.2", true, 4, 30)),
                Reason::PeerUnstable,
                "stable for 30s, 5m0s required",
            ),
            (
                Box::new(|c| c.tunnels[1].accepted_routes = 0),
                Reason::PeerNoRoutes,
                "accepts 0 BGP route(s), 1 required",
            ),
        ];
        for (mutate, want, detail) in cases {
            let mut conn = base.clone();
            mutate(&mut conn);
            let st = statuses(&conn, &[pending(48), Maintenance::default()]);
            let plan = evaluate(&input(conn, st));
            assert!(plan.candidates.is_empty(), "{want:?}");
            assert_eq!(plan.blocked[0].reason, want);
            assert!(
                plan.blocked[0].detail.contains(detail),
                "{want:?}: {}",
                plan.blocked[0].detail
            );
            assert!(plan.blocked[0].pending_maintenance);
        }
    }

    #[test]
    fn peer_down_without_message_says_so() {
        let mut conn = connection("vpn-1");
        conn.tunnels[1].up = false;
        conn.tunnels[1].status_message.clear();
        let st = statuses(&conn, &[pending(48), Maintenance::default()]);
        let plan = evaluate(&input(conn, st));
        assert!(plan.blocked[0].detail.contains("no status message"));
    }

    #[test]
    fn static_routes_skip_the_route_check() {
        let mut conn = connection("vpn-1");
        conn.static_routes_only = true;
        conn.tunnels[1].accepted_routes = 0;
        let st = statuses(&conn, &[pending(48), Maintenance::default()]);
        assert_eq!(evaluate(&input(conn, st)).candidates.len(), 1);
    }

    #[test]
    fn later_checks_apply_after_the_peer_is_proven() {
        let conn = connection("vpn-1");
        let st = statuses(&conn, &[pending(48), Maintenance::default()]);
        let rid = format!("vpn-1|1.1.1.1|{}", NOW_UNIX + 48 * 3600);

        let mut inp = input(conn.clone(), st.clone());
        inp.awaiting_approval.insert(rid);
        assert_eq!(evaluate(&inp).blocked[0].reason, Reason::AwaitingApproval);

        let mut inp = input(conn.clone(), st.clone());
        inp.replacement_in_flight = true;
        assert_eq!(
            evaluate(&inp).blocked[0].reason,
            Reason::ReplacementInFlight
        );

        let mut inp = input(conn, st);
        inp.window_open = false;
        inp.window_detail = "outside window".into();
        let plan = evaluate(&inp);
        assert_eq!(plan.blocked[0].reason, Reason::WindowClosed);
        assert_eq!(plan.blocked[0].detail, "outside window");
        assert_eq!(plan.held().len(), 1);
    }

    #[test]
    fn cooldown_and_chaining() {
        let conn = connection("vpn-1");
        let st = statuses(&conn, &[pending(48), pending(48)]);
        let recent = |ip: &str, ok: bool| ConnectionState {
            last_replacement_at: Some(now() - chrono::TimeDelta::minutes(40)),
            last_tunnel_ip: ip.into(),
            last_succeeded: ok,
        };

        // Sibling of a healthy replacement chains.
        let mut inp = input(conn.clone(), st.clone());
        inp.history.insert("vpn-1".into(), recent("2.2.2.2", true));
        let plan = evaluate(&inp);
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].tunnel.outside_ip, "1.1.1.1");
        assert!(plan.candidates[0].chained);
        assert!(plan.candidates[0].sibling_replaced_at.is_some());
        let same = plan
            .blocked
            .iter()
            .find(|b| b.tunnel_ip == "2.2.2.2")
            .unwrap();
        assert_eq!(same.reason, Reason::Cooldown);
        assert!(
            same.detail
                .contains("40m0s ago; cooldown is 24h0m0s, and this is the same tunnel"),
            "{}",
            same.detail
        );

        // A failed replacement is never chained from.
        let mut inp = input(conn.clone(), st.clone());
        inp.history.insert("vpn-1".into(), recent("2.2.2.2", false));
        let plan = evaluate(&inp);
        assert!(plan.candidates.is_empty());
        assert!(plan.blocked[0].detail.contains("did not end healthy"));

        // Chaining switched off.
        let mut inp = input(conn.clone(), st.clone());
        inp.history.insert("vpn-1".into(), recent("2.2.2.2", true));
        inp.thresholds.chain_sibling_tunnel = false;
        let plan = evaluate(&inp);
        assert!(plan.candidates.is_empty());
        assert!(
            plan.blocked[0]
                .detail
                .contains("(sibling chaining is disabled)")
        );

        // Unknown last tunnel: plain cooldown wording.
        let mut inp = input(conn.clone(), st.clone());
        inp.history.insert("vpn-1".into(), recent("", true));
        let plan = evaluate(&inp);
        assert!(
            plan.blocked[0].detail.ends_with("cooldown is 24h0m0s"),
            "{}",
            plan.blocked[0].detail
        );

        // Cooldown elapsed: both eligible again.
        let mut inp = input(conn, st);
        inp.history.insert(
            "vpn-1".into(),
            ConnectionState {
                last_replacement_at: Some(now() - chrono::TimeDelta::hours(30)),
                last_tunnel_ip: "2.2.2.2".into(),
                last_succeeded: true,
            },
        );
        assert_eq!(evaluate(&inp).candidates.len(), 2);
    }

    #[test]
    fn sorts_by_deadline_then_id() {
        let a = connection("vpn-a");
        let b = connection("vpn-b");
        let mut inp = input(
            a.clone(),
            statuses(&a, &[Maintenance::default(), pending(72)]),
        );
        inp.connections.push(b.clone());
        inp.statuses.insert(
            "vpn-b".into(),
            statuses(
                &b,
                &[
                    Maintenance {
                        pending: true,
                        ..Maintenance::default()
                    },
                    pending(24),
                ],
            ),
        );
        let plan = evaluate(&inp);
        let order: Vec<String> = plan.candidates.iter().map(Candidate::label).collect();
        assert_eq!(
            order,
            vec![
                "prod (vpn-b) tunnel 2.2.2.2",
                "prod (vpn-a) tunnel 2.2.2.2",
                "prod (vpn-b) tunnel 1.1.1.1",
            ]
        );
        assert!(!plan.candidates[2].escalate, "no deadline is not urgent");
    }

    #[test]
    fn connections_without_statuses_are_skipped() {
        let conn = connection("vpn-1");
        let mut inp = input(conn, Vec::new());
        inp.statuses.clear();
        assert_eq!(evaluate(&inp), Plan::default());
    }

    #[test]
    fn request_id_helpers() {
        let rid = request_id("vpn-1", "1.1.1.1", &pending(1));
        assert_eq!(
            split_request_id(&rid),
            Some(("vpn-1".into(), "1.1.1.1".into()))
        );
        assert!(request_id_matches(&rid, "vpn-1", "1.1.1.1"));
        assert!(!request_id_matches(&rid, "vpn-1", "2.2.2.2"));
        assert!(!request_id_matches("vpn-1|1.1.1.1|", "vpn-1", "1.1.1.1"));
        assert_eq!(
            request_id("vpn-1", "1.1.1.1", &Maintenance::default()),
            "vpn-1|1.1.1.1|0"
        );
        assert!(split_request_id("nope").is_none());
        assert!(split_request_id("|1.1.1.1|0").is_none());
        assert_eq!(Reason::PeerDown.to_string(), "peer_down");
        assert_eq!(Reason::TrafficHigh.as_str(), "traffic_high");
    }
}
