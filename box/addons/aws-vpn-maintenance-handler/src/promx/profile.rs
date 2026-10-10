//! Known exporter conventions for Site-to-Site VPN tunnel traffic.

use std::fmt;

use super::client::Client;
use super::gate::Vars;

/// One known convention for exporting VPN tunnel traffic to Prometheus.
/// Exporters name the same `CloudWatch` metric differently and disagree on
/// whether it lands as a counter or a gauge, so both belong in the profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// Identifies the profile in logs.
    pub name: &'static str,
    /// The metric name carrying tunnel egress bytes.
    pub metric: &'static str,
    /// The ingress counterpart. Both directions count, because replacing a
    /// tunnel interrupts traffic either way. Empty when the exporter publishes
    /// only one direction.
    pub metric_in: &'static str,
    /// Carries the VPN connection ID.
    pub vpn_label: &'static str,
    /// Carries the tunnel's outside IP. Unused by the query, which judges the
    /// connection total: replacing a tunnel moves its traffic onto the peer.
    pub tunnel_label: &'static str,
    /// Selects `rate()` over a monotonic counter. `CloudWatch` exporters that
    /// publish the period Sum as a gauge need `avg_over_time` instead.
    pub counter: bool,
}

/// Tried in order. The list covers the exporters that actually publish
/// AWS/VPN tunnel metrics; the first one with data for the connection wins.
pub const PROFILES: &[Profile] = &[
    Profile {
        name: "yet-another-cloudwatch-exporter",
        metric: "aws_ec2_vpn_tunnel_data_out_sum",
        metric_in: "aws_ec2_vpn_tunnel_data_in_sum",
        vpn_label: "dimension_VpnId",
        tunnel_label: "dimension_TunnelIpAddress",
        counter: false,
    },
    Profile {
        name: "yet-another-cloudwatch-exporter (vpn namespace)",
        metric: "aws_vpn_tunnel_data_out_sum",
        metric_in: "aws_vpn_tunnel_data_in_sum",
        vpn_label: "dimension_VpnId",
        tunnel_label: "dimension_TunnelIpAddress",
        counter: false,
    },
    Profile {
        name: "prometheus cloudwatch_exporter",
        metric: "aws_ec2_tunnel_data_out_sum",
        metric_in: "aws_ec2_tunnel_data_in_sum",
        vpn_label: "dimension_VpnId",
        tunnel_label: "dimension_TunnelIpAddress",
        counter: false,
    },
    Profile {
        name: "cloudwatch_exporter (TunnelDataOut)",
        metric: "aws_vpn_tunnel_data_out_average",
        metric_in: "aws_vpn_tunnel_data_in_average",
        vpn_label: "dimension_VpnId",
        tunnel_label: "dimension_TunnelIpAddress",
        counter: false,
    },
    Profile {
        name: "otel awscloudwatchmetrics receiver",
        metric: "amazonaws_com_AWS_VPN_TunnelDataOut",
        metric_in: "amazonaws_com_AWS_VPN_TunnelDataIn",
        vpn_label: "VpnId",
        tunnel_label: "TunnelIpAddress",
        counter: true,
    },
];

/// How much traffic counts as one point, both for "now" and for every
/// historical point it is compared against. Deliberately not configurable: it
/// has to match the exporter's `CloudWatch` period to mean anything.
pub const SAMPLE_WINDOW: &str = "5m";

/// Finds the profile that has data for the given VPN connection.
///
/// Probing beats asking an operator for `PromQL`: the query has to match
/// whichever exporter that cluster happens to run, and a query written once by
/// hand silently stops matching when the exporter is swapped.
pub async fn detect(client: &Client, vpn_connection_id: &str) -> Result<Profile, String> {
    let mut tried = Vec::with_capacity(PROFILES.len());
    for p in PROFILES {
        let probe = format!(
            "count({}{{{}=\"{}\"}})",
            p.metric, p.vpn_label, vpn_connection_id
        );
        if let Ok(v) = client.query(&probe).await
            && v > 0.0
        {
            return Ok(p.clone());
        }
        tried.push(p.metric);
    }
    Err(format!(
        "no known VPN traffic metric found for {vpn_connection_id} (tried {})",
        tried.join(", ")
    ))
}

impl Profile {
    /// Builds the one expression the gate reads, used for both the recent
    /// samples and the historical distribution.
    ///
    /// One expression rather than a current/baseline pair is the point: the
    /// verdict is where today's value falls within its own history, so the two
    /// must be the same measurement by construction.
    #[must_use]
    pub fn traffic_query(&self, v: &Vars) -> String {
        let out = format!("({} or vector(0))", self.direction_expr(self.metric, v));
        if self.metric_in.is_empty() {
            return out;
        }
        // `or vector(0)` on each side, because a missing direction would
        // otherwise empty the whole expression and read as "no data" rather
        // than "no traffic that way".
        format!(
            "{out} + ({} or vector(0))",
            self.direction_expr(self.metric_in, v)
        )
    }

    fn direction_expr(&self, metric: &str, v: &Vars) -> String {
        let selector = format!("{metric}{{{}=\"{}\"}}", self.vpn_label, v.vpn_connection_id);
        if self.counter {
            format!("sum(rate({selector}[{SAMPLE_WINDOW}]))")
        } else {
            // The exporter publishes the CloudWatch period Sum as a gauge, so
            // averaging the recent samples is the closest thing to a rate.
            format!("sum(avg_over_time({selector}[{SAMPLE_WINDOW}]))")
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = if self.counter { "counter" } else { "gauge" };
        write!(
            f,
            "{} ({}, {kind} by {})",
            self.name, self.metric, self.vpn_label
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        Vars {
            vpn_connection_id: "vpn-1".into(),
            ..Vars::default()
        }
    }

    #[test]
    fn gauge_profile_averages_both_directions() {
        let q = PROFILES[0].traffic_query(&vars());
        assert_eq!(
            q,
            "(sum(avg_over_time(aws_ec2_vpn_tunnel_data_out_sum{dimension_VpnId=\"vpn-1\"}[5m])) or vector(0)) + (sum(avg_over_time(aws_ec2_vpn_tunnel_data_in_sum{dimension_VpnId=\"vpn-1\"}[5m])) or vector(0))"
        );
        assert_eq!(
            PROFILES[0].to_string(),
            "yet-another-cloudwatch-exporter (aws_ec2_vpn_tunnel_data_out_sum, gauge by dimension_VpnId)"
        );
    }

    #[test]
    fn counter_profile_uses_rate() {
        let p = &PROFILES[4];
        let q = p.traffic_query(&vars());
        assert!(q.starts_with("(sum(rate(amazonaws_com_AWS_VPN_TunnelDataOut{VpnId=\"vpn-1\"}[5m])) or vector(0)) + "), "{q}");
        assert!(p.to_string().contains("counter by VpnId"));
    }

    #[test]
    fn single_direction_profile_has_no_sum() {
        let p = Profile {
            metric_in: "",
            ..PROFILES[0].clone()
        };
        assert_eq!(
            p.traffic_query(&vars()),
            "(sum(avg_over_time(aws_ec2_vpn_tunnel_data_out_sum{dimension_VpnId=\"vpn-1\"}[5m])) or vector(0))"
        );
    }
}
