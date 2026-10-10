//! Maps planner and state records to the Slack display form.

use std::time::Duration;

use chrono::Utc;

use super::Controller;
use crate::aws::Connection;
use crate::k8s::InFlight;
use crate::planner::Candidate;
use crate::slack::Proposal;

impl Controller {
    /// How long a request posted now stays answerable: the configured timeout,
    /// or the window's remaining room to start a replacement when that runs out
    /// first. The card prints this figure, and the traffic gate often posts one
    /// late in the window.
    pub(super) fn approval_expiry(&self, now: chrono::DateTime<Utc>) -> Duration {
        self.cfg
            .approval
            .timeout
            .get()
            .min(self.window.start_budget(now))
    }

    /// Maps a planner candidate to its Slack display form.
    pub(super) fn proposal(&self, cand: &Candidate) -> Proposal {
        let now = Utc::now();
        Proposal {
            request_id: cand.request_id.clone(),
            connection_id: cand.connection.id.clone(),
            connection_name: cand.connection.name.clone(),
            gateway: gateway_of(&cand.connection),
            gateway_name: gateway_name_of(&cand.connection),
            customer_gateway_id: cand.connection.customer_gateway_id.clone(),
            customer_gateway_name: cand.connection.customer_gateway_name.clone(),
            region: self.cfg.region.clone(),
            tunnel_ip: cand.tunnel.outside_ip.clone(),
            queue: cand.queue.clone(),
            stable_requirement: self.cfg.safety.peer_min_stable_for.get(),
            peer_ip: cand.peer.outside_ip.clone(),
            peer_routes: cand.peer.accepted_routes,
            peer_stable_for: cand.peer.stable_for(now),
            static_routes: cand.connection.static_routes_only,
            deadline_in: cand.deadline_in,
            deadline: cand.maintenance.auto_applied_after,
            escalate: cand.escalate,
            dry_run: self.cfg.dry_run,
            approval_expiry: self.approval_expiry(now),
            window: self.window.to_string(),
            traffic_checked: false,
            traffic_detail: String::new(),
        }
    }

    /// Rebuilds the display form for a replacement recovered from persisted
    /// state, where the original candidate is gone. Only the identifying
    /// fields are needed: the card it updates was already posted in full.
    pub(super) fn proposal_from_in_flight(&self, conn: &Connection, f: &InFlight) -> Proposal {
        Proposal {
            request_id: f.request_id.clone(),
            connection_id: conn.id.clone(),
            connection_name: conn.name.clone(),
            gateway: gateway_of(conn),
            gateway_name: gateway_name_of(conn),
            customer_gateway_id: conn.customer_gateway_id.clone(),
            customer_gateway_name: conn.customer_gateway_name.clone(),
            region: self.cfg.region.clone(),
            tunnel_ip: f.tunnel_ip.clone(),
            queue: f.queue.clone(),
            stable_requirement: self.cfg.safety.peer_min_stable_for.get(),
            peer_ip: f.peer_ip.clone(),
            static_routes: conn.static_routes_only,
            dry_run: self.cfg.dry_run,
            approval_expiry: self.cfg.approval.timeout.get(),
            window: self.window.to_string(),
            ..Proposal::default()
        }
    }
}

/// Whichever gateway the connection attaches to. The two are mutually
/// exclusive.
#[must_use]
pub fn gateway_of(conn: &Connection) -> String {
    if conn.transit_gateway_id.is_empty() {
        conn.vpn_gateway_id.clone()
    } else {
        conn.transit_gateway_id.clone()
    }
}

/// The Name tag of that same gateway, empty when it has none.
#[must_use]
pub fn gateway_name_of(conn: &Connection) -> String {
    if conn.transit_gateway_id.is_empty() {
        conn.vpn_gateway_name.clone()
    } else {
        conn.transit_gateway_name.clone()
    }
}
