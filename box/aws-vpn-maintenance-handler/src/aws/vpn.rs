//! Discovery and conversion of VPN connections.

use aws_sdk_ec2::types::{Filter, TelemetryStatus, VpnConnection};
use chrono::{DateTime, Utc};

use super::client::{ApiError, Client};
use super::types::{Connection, DiscoverInput, Tunnel};

/// Converts an SDK timestamp, dropping anything outside chrono's range.
pub(super) fn to_chrono(t: &aws_smithy_types::DateTime) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(t.secs(), t.subsec_nanos())
}

impl Client {
    /// Returns the managed connections in state "available", sorted by ID for
    /// stable output. `DescribeVpnConnections` is not paginated.
    pub async fn discover(&self, input: &DiscoverInput) -> Result<Vec<Connection>, ApiError> {
        // Deleted connections linger in the API; this keeps them out entirely.
        let mut filters = vec![Filter::builder().name("state").values("available").build()];
        for f in &input.tag_filters {
            let filter = if f.value.is_empty() {
                Filter::builder().name("tag-key").values(&f.key).build()
            } else {
                Filter::builder()
                    .name(format!("tag:{}", f.key))
                    .values(&f.value)
                    .build()
            };
            filters.push(filter);
        }

        let out = self
            .api
            .describe_vpn_connections(filters, Vec::new())
            .await
            .map_err(|e| e.with_context("describe vpn connections"))?;

        let mut conns: Vec<Connection> = out
            .vpn_connections()
            .iter()
            .filter(|v| {
                v.vpn_connection_id()
                    .is_some_and(|id| !id.is_empty() && !input.exclude_ids.iter().any(|x| x == id))
            })
            .map(convert_connection)
            .collect();
        conns.sort_by(|a, b| a.id.cmp(&b.id));
        self.resolve_gateway_names(&mut conns).await;
        Ok(conns)
    }

    /// Returns one connection by ID. Verification uses it to keep the poll
    /// cheap and avoid re-evaluating tags mid-replacement.
    pub async fn describe(&self, id: &str) -> Result<Connection, ApiError> {
        let out = self
            .api
            .describe_vpn_connections(Vec::new(), vec![id.to_string()])
            .await
            .map_err(|e| e.with_context(&format!("describe vpn connection {id}")))?;
        let Some(first) = out.vpn_connections().first() else {
            return Err(ApiError::Rejected(format!("vpn connection {id} not found")));
        };
        let mut single = vec![convert_connection(first)];
        // Cached after the first pass, so re-describing during verification
        // costs nothing extra.
        self.resolve_gateway_names(&mut single).await;
        Ok(single.pop().unwrap_or_default())
    }
}

impl ApiError {
    /// Prefixes the message, keeping the classification.
    pub(super) fn with_context(self, ctx: &str) -> Self {
        match self {
            Self::Rejected(s) => Self::Rejected(format!("{ctx}: {s}")),
            Self::Uncertain(s) => Self::Uncertain(format!("{ctx}: {s}")),
            Self::DryRunSucceeded => Self::DryRunSucceeded,
        }
    }
}

pub(super) fn convert_connection(v: &VpnConnection) -> Connection {
    let mut conn = Connection {
        id: v.vpn_connection_id().unwrap_or_default().to_string(),
        state: v
            .state()
            .map(|s| s.as_str().to_string())
            .unwrap_or_default(),
        customer_gateway_id: v.customer_gateway_id().unwrap_or_default().to_string(),
        transit_gateway_id: v.transit_gateway_id().unwrap_or_default().to_string(),
        vpn_gateway_id: v.vpn_gateway_id().unwrap_or_default().to_string(),
        ..Connection::default()
    };
    // Tunnel endpoint lifecycle control is a per-tunnel option, so it is keyed
    // by outside IP and merged into the telemetry below.
    let mut lifecycle = std::collections::HashMap::new();
    if let Some(opts) = v.options() {
        conn.static_routes_only = opts.static_routes_only().unwrap_or(false);
        for opt in opts.tunnel_options() {
            if let Some(ip) = opt.outside_ip_address().filter(|ip| !ip.is_empty()) {
                lifecycle.insert(
                    ip.to_string(),
                    opt.enable_tunnel_lifecycle_control().unwrap_or(false),
                );
            }
        }
    }
    if let Some(name) = v
        .tags()
        .iter()
        .find(|t| t.key() == Some("Name"))
        .and_then(|t| t.value())
    {
        conn.name = name.to_string();
    }

    conn.tunnels = v
        .vgw_telemetry()
        .iter()
        .map(|tel| {
            let ip = tel.outside_ip_address().unwrap_or_default().to_string();
            Tunnel {
                up: tel.status() == Some(&TelemetryStatus::Up),
                status_message: tel.status_message().unwrap_or_default().to_string(),
                accepted_routes: tel.accepted_route_count().unwrap_or(0),
                last_status_change: tel.last_status_change().and_then(to_chrono),
                lifecycle_control: lifecycle.get(&ip).copied().unwrap_or(false),
                outside_ip: ip,
            }
        })
        .collect();
    // Stable order for logs, metric labels, and Slack messages.
    conn.tunnels.sort_by(|a, b| a.outside_ip.cmp(&b.outside_ip));
    conn
}

#[cfg(test)]
mod tests {
    use aws_sdk_ec2::types::{VpnConnection, VpnState};

    use super::super::client::fake::*;
    use super::*;
    use crate::aws::TagFilter;

    fn client(conns: Vec<VpnConnection>) -> (Client, std::sync::Arc<FakeEc2>) {
        let fake = std::sync::Arc::new(FakeEc2::default());
        *fake.connections.lock().unwrap() = conns;
        let c = Client::from_parts(Box::new(fake.clone()), None, None);
        (c, fake)
    }

    #[tokio::test]
    async fn discover_sorts_filters_and_converts() {
        let (client, _) = client(vec![
            vpn_connection("vpn-b", "", [true, true]),
            vpn_connection("vpn-a", "prod", [true, false]),
            vpn_connection("vpn-x", "excluded", [true, true]),
            VpnConnection::builder().state(VpnState::Available).build(),
        ]);
        let conns = client
            .discover(&DiscoverInput {
                tag_filters: vec![
                    TagFilter {
                        key: "managed".into(),
                        value: "true".into(),
                    },
                    TagFilter {
                        key: "any".into(),
                        value: String::new(),
                    },
                ],
                exclude_ids: vec!["vpn-x".into()],
            })
            .await
            .unwrap();
        assert_eq!(conns.len(), 2);
        assert_eq!(conns[0].id, "vpn-a");
        assert_eq!(conns[0].name, "prod");
        assert_eq!(conns[0].state, "available");
        assert_eq!(conns[0].transit_gateway_id, "tgw-1");
        assert!(!conns[0].static_routes_only);
        // Tunnels sorted by IP with lifecycle merged from the options.
        assert_eq!(conns[0].tunnels[0].outside_ip, "1.1.1.1");
        assert!(conns[0].tunnels[0].lifecycle_control);
        assert!(!conns[0].tunnels[1].lifecycle_control);
        assert!(conns[0].tunnels[0].up);
        assert_eq!(conns[0].tunnels[0].accepted_routes, 3);
        assert_eq!(
            conns[0].tunnels[0].last_status_change.unwrap().timestamp(),
            1_700_000_000
        );
        assert_eq!(conns[1].id, "vpn-b");
        assert!(conns[1].name.is_empty());
    }

    #[tokio::test]
    async fn describe_returns_one_or_not_found() {
        let (client, fake) = client(vec![vpn_connection("vpn-a", "prod", [true, true])]);
        let conn = client.describe("vpn-a").await.unwrap();
        assert_eq!(conn.id, "vpn-a");
        let err = client.describe("vpn-z").await.unwrap_err();
        assert!(
            err.to_string().contains("vpn connection vpn-z not found"),
            "{err}"
        );
        *fake.describe_error.lock().unwrap() = Some("throttled".into());
        let err = client.describe("vpn-a").await.unwrap_err();
        assert_eq!(err.to_string(), "describe vpn connection vpn-a: throttled");
        let err = client
            .discover(&DiscoverInput::default())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "describe vpn connections: throttled");
        assert_eq!(
            ApiError::DryRunSucceeded.with_context("x").to_string(),
            ApiError::DryRunSucceeded.to_string()
        );
        assert_eq!(
            ApiError::Uncertain("t".into())
                .with_context("c")
                .to_string(),
            "c: t"
        );
    }

    #[test]
    fn convert_handles_missing_options_and_telemetry() {
        let v = VpnConnection::builder()
            .vpn_connection_id("vpn-1")
            .vpn_gateway_id("vgw-1")
            .vgw_telemetry(
                aws_sdk_ec2::types::VgwTelemetry::builder()
                    .outside_ip_address("1.1.1.1")
                    .status(TelemetryStatus::Down)
                    .build(),
            )
            .build();
        let conn = convert_connection(&v);
        assert_eq!(conn.vpn_gateway_id, "vgw-1");
        assert!(conn.state.is_empty());
        assert_eq!(conn.tunnels.len(), 1);
        assert!(!conn.tunnels[0].up);
        assert!(!conn.tunnels[0].lifecycle_control);
        assert!(conn.tunnels[0].last_status_change.is_none());
    }
}
