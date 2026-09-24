//! Domain types reduced from the EC2 API to the fields that decide whether a
//! tunnel may be replaced.

use std::time::Duration;

use chrono::{DateTime, Utc};

/// Saturating `now - then`, zero when `then` is in the future.
#[must_use]
pub fn since(now: DateTime<Utc>, then: DateTime<Utc>) -> Duration {
    (now - then).to_std().unwrap_or(Duration::ZERO)
}

/// Saturating `then - now`, zero when `then` has passed.
#[must_use]
pub fn until(now: DateTime<Utc>, then: DateTime<Utc>) -> Duration {
    (then - now).to_std().unwrap_or(Duration::ZERO)
}

/// One `IPsec` tunnel of a VPN connection, from the `VgwTelemetry` field of
/// `DescribeVpnConnections`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tunnel {
    /// The AWS-side public IP, and the tunnel's only stable identifier: every
    /// maintenance API takes it as input.
    pub outside_ip: String,
    /// `TelemetryStatus == UP` (IKE and `IPsec` established).
    pub up: bool,
    /// The AWS-provided reason when the tunnel is DOWN.
    pub status_message: String,
    /// The BGP route count from the customer gateway. Always 0 on
    /// static-routes-only connections, where it means nothing.
    pub accepted_routes: i32,
    /// When IKE, `IPsec`, or BGP status last flipped, used to reject a flapping
    /// peer.
    pub last_status_change: Option<DateTime<Utc>>,
    /// `EnableTunnelLifecycleControl` on the tunnel option. Without it AWS never
    /// offers pending maintenance for early application and `ReplaceVpnTunnel`
    /// cannot be used.
    pub lifecycle_control: bool,
}

impl Tunnel {
    /// How long the tunnel has held its status. An unreported
    /// `last_status_change` yields zero, which callers read as "not known to be
    /// stable".
    #[must_use]
    pub fn stable_for(&self, now: DateTime<Utc>) -> Duration {
        self.last_status_change
            .map_or(Duration::ZERO, |t| since(now, t))
    }
}

/// A VPN connection reduced to the fields that decide whether one of its
/// tunnels may be replaced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Connection {
    pub id: String,
    /// The Name tag, for logs and Slack. Empty when untagged.
    pub name: String,
    /// "available", "pending", "deleting", or "deleted". Only "available" is
    /// eligible.
    pub state: String,
    /// Disables the route-count check, since such connections never report
    /// accepted routes.
    pub static_routes_only: bool,
    pub customer_gateway_id: String,
    pub transit_gateway_id: String,
    pub vpn_gateway_id: String,
    /// Gateway Name tags, resolved separately because `DescribeVpnConnections`
    /// returns the IDs only. Empty when the gateway carries no Name tag or the
    /// describe call is not permitted.
    pub customer_gateway_name: String,
    pub transit_gateway_name: String,
    pub vpn_gateway_name: String,
    /// The telemetry entries, normally exactly two.
    pub tunnels: Vec<Tunnel>,
}

impl Connection {
    /// `name (id)` when a Name tag exists, otherwise the raw ID.
    #[must_use]
    pub fn label(&self) -> String {
        if self.name.is_empty() {
            self.id.clone()
        } else {
            format!("{} ({})", self.name, self.id)
        }
    }

    /// The tunnel with the given outside IP.
    #[must_use]
    pub fn tunnel(&self, outside_ip: &str) -> Option<&Tunnel> {
        self.tunnels.iter().find(|t| t.outside_ip == outside_ip)
    }

    /// The other tunnel, `None` unless there are exactly two. That case is
    /// itself a reason to refuse: nothing to fail over to.
    #[must_use]
    pub fn peer(&self, outside_ip: &str) -> Option<&Tunnel> {
        if self.tunnels.len() != 2 {
            return None;
        }
        self.tunnels.iter().find(|t| t.outside_ip != outside_ip)
    }
}

/// One tunnel's pending endpoint maintenance, from
/// `GetVpnTunnelReplacementStatus`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Maintenance {
    /// `PendingMaintenance == "AVAILABLE"`, meaning `ReplaceVpnTunnel` will
    /// actually do something.
    pub pending: bool,
    /// When AWS starts applying the maintenance itself, at a time of its
    /// choosing. Owning it means acting before this.
    pub auto_applied_after: Option<DateTime<Utc>>,
    /// When maintenance was last applied to this tunnel.
    pub last_applied: Option<DateTime<Utc>>,
}

impl Maintenance {
    /// The time left before AWS applies the maintenance itself, or zero when
    /// unknown or elapsed.
    #[must_use]
    pub fn deadline_in(&self, now: DateTime<Utc>) -> Duration {
        self.auto_applied_after
            .map_or(Duration::ZERO, |t| until(now, t))
    }
}

/// A tunnel paired with its maintenance state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TunnelStatus {
    pub tunnel: Tunnel,
    pub maintenance: Maintenance,
}

/// A tag key/value pair scoping the managed connections. An empty value
/// matches any value for that key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagFilter {
    pub key: String,
    pub value: String,
}

/// Scopes a discovery pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoverInput {
    /// `AND`ed by the EC2 API: every listed tag must be present.
    pub tag_filters: Vec<TagFilter>,
    /// Dropped after the call, since EC2 filters cannot negate.
    pub exclude_ids: Vec<String>,
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 28, 2, 0, 0).unwrap()
    }

    #[test]
    fn stable_for_and_deadline_saturate() {
        let t = Tunnel {
            last_status_change: Some(now() - chrono::TimeDelta::minutes(7)),
            ..Tunnel::default()
        };
        assert_eq!(t.stable_for(now()), Duration::from_mins(7));
        assert_eq!(Tunnel::default().stable_for(now()), Duration::ZERO);
        let future = Tunnel {
            last_status_change: Some(now() + chrono::TimeDelta::minutes(1)),
            ..Tunnel::default()
        };
        assert_eq!(future.stable_for(now()), Duration::ZERO);

        let m = Maintenance {
            pending: true,
            auto_applied_after: Some(now() + chrono::TimeDelta::hours(2)),
            last_applied: None,
        };
        assert_eq!(m.deadline_in(now()), Duration::from_hours(2));
        assert_eq!(Maintenance::default().deadline_in(now()), Duration::ZERO);
        let past = Maintenance {
            auto_applied_after: Some(now() - chrono::TimeDelta::hours(2)),
            ..Maintenance::default()
        };
        assert_eq!(past.deadline_in(now()), Duration::ZERO);
    }

    #[test]
    fn label_tunnel_and_peer() {
        let mut conn = Connection {
            id: "vpn-1".into(),
            tunnels: vec![
                Tunnel {
                    outside_ip: "1.1.1.1".into(),
                    ..Tunnel::default()
                },
                Tunnel {
                    outside_ip: "2.2.2.2".into(),
                    ..Tunnel::default()
                },
            ],
            ..Connection::default()
        };
        assert_eq!(conn.label(), "vpn-1");
        conn.name = "prod".into();
        assert_eq!(conn.label(), "prod (vpn-1)");
        assert_eq!(conn.tunnel("2.2.2.2").unwrap().outside_ip, "2.2.2.2");
        assert!(conn.tunnel("3.3.3.3").is_none());
        assert_eq!(conn.peer("1.1.1.1").unwrap().outside_ip, "2.2.2.2");
        conn.tunnels.pop();
        assert!(conn.peer("1.1.1.1").is_none());
    }
}
