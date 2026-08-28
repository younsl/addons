//! The SDK client bundle and the trait seams tests replace.

use async_trait::async_trait;
use aws_sdk_ec2::operation::describe_customer_gateways::DescribeCustomerGatewaysOutput;
use aws_sdk_ec2::operation::describe_transit_gateways::DescribeTransitGatewaysOutput;
use aws_sdk_ec2::operation::describe_vpn_connections::DescribeVpnConnectionsOutput;
use aws_sdk_ec2::operation::describe_vpn_gateways::DescribeVpnGatewaysOutput;
use aws_sdk_ec2::operation::get_vpn_tunnel_replacement_status::GetVpnTunnelReplacementStatusOutput;
use aws_sdk_ec2::types::Filter;
use thiserror::Error;

use super::gateways::GatewayNames;

/// How a call to AWS failed, reduced to what the controller decides on.
#[derive(Debug, Error)]
pub enum ApiError {
    /// AWS answered and refused. Only a client-fault API error proves a
    /// request did not take effect: the service received it, evaluated it, and
    /// rejected it.
    #[error("{0}")]
    Rejected(String),
    /// The `DryRunOperation` code AWS returns when a dry-run request would have
    /// been permitted. Success arrives as an error and is translated here.
    #[error("dry run succeeded: the request would have been accepted")]
    DryRunSucceeded,
    /// A timeout, a connection failure, or a server-fault response. The
    /// request may have been accepted and only the answer lost.
    #[error("{0}")]
    Uncertain(String),
}

/// The subset of the EC2 API used here. The SDK client satisfies it; tests
/// provide fakes.
#[async_trait]
pub trait Ec2Api: Send + Sync {
    async fn describe_vpn_connections(
        &self,
        filters: Vec<Filter>,
        ids: Vec<String>,
    ) -> Result<DescribeVpnConnectionsOutput, ApiError>;
    async fn get_vpn_tunnel_replacement_status(
        &self,
        connection_id: &str,
        outside_ip: &str,
    ) -> Result<GetVpnTunnelReplacementStatusOutput, ApiError>;
    async fn replace_vpn_tunnel(
        &self,
        connection_id: &str,
        outside_ip: &str,
        dry_run: bool,
    ) -> Result<(), ApiError>;
}

/// Reads the Name tags of the gateways a VPN connection attaches to.
#[async_trait]
pub trait GatewayApi: Send + Sync {
    async fn describe_transit_gateways(
        &self,
        ids: Vec<String>,
    ) -> Result<DescribeTransitGatewaysOutput, ApiError>;
    async fn describe_vpn_gateways(
        &self,
        ids: Vec<String>,
    ) -> Result<DescribeVpnGatewaysOutput, ApiError>;
    async fn describe_customer_gateways(
        &self,
        ids: Vec<String>,
    ) -> Result<DescribeCustomerGatewaysOutput, ApiError>;
}

/// The identity call, which is all the STS surface used here.
#[async_trait]
pub trait StsApi: Send + Sync {
    /// Returns `(arn, account)`.
    async fn caller_identity(&self) -> Result<(String, String), ApiError>;
}

/// Provides the VPN maintenance operations.
pub struct Client {
    pub(super) api: Box<dyn Ec2Api>,
    /// Backs the startup identity check only.
    pub(super) sts: Option<Box<dyn StsApi>>,
    /// Reads gateway Name tags; `gateways` caches them. `None` in tests that
    /// do not need names, in which case notifications show gateway IDs.
    pub(super) gw_api: Option<Box<dyn GatewayApi>>,
    pub(super) gateways: GatewayNames,
}

impl Client {
    /// Builds a client for the region. Credentials resolve via the default
    /// chain, in-cluster the Pod's own IRSA or EKS Pod Identity role; no role
    /// is ever assumed.
    pub async fn new(region: &str) -> Self {
        let cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region.to_string()))
            .load()
            .await;
        let ec2 = aws_sdk_ec2::Client::new(&cfg);
        let sts = aws_sdk_sts::Client::new(&cfg);
        Self::from_parts(
            Box::new(ec2.clone()),
            Some(Box::new(sts)),
            Some(Box::new(ec2)),
        )
    }

    /// Builds a client over the given API surfaces, for tests.
    #[must_use]
    pub fn from_parts(
        api: Box<dyn Ec2Api>,
        sts: Option<Box<dyn StsApi>>,
        gw_api: Option<Box<dyn GatewayApi>>,
    ) -> Self {
        Self {
            api,
            sts,
            gw_api,
            gateways: GatewayNames::default(),
        }
    }
}

/// Classifies an SDK error. Only a service error with a 4xx status is a
/// definite rejection; everything else leaves the outcome unknown.
pub(super) fn classify<E>(err: &aws_sdk_ec2::error::SdkError<E>) -> ApiError
where
    E: std::error::Error + aws_sdk_ec2::error::ProvideErrorMetadata + 'static,
{
    use aws_sdk_ec2::error::SdkError;
    let rendered = aws_sdk_ec2::error::DisplayErrorContext(err).to_string();
    match err {
        SdkError::ServiceError(inner) => {
            if inner.err().code() == Some("DryRunOperation") {
                return ApiError::DryRunSucceeded;
            }
            let status = inner.raw().status().as_u16();
            if (400..500).contains(&status) {
                ApiError::Rejected(rendered)
            } else {
                ApiError::Uncertain(rendered)
            }
        }
        _ => ApiError::Uncertain(rendered),
    }
}

#[async_trait]
impl Ec2Api for aws_sdk_ec2::Client {
    async fn describe_vpn_connections(
        &self,
        filters: Vec<Filter>,
        ids: Vec<String>,
    ) -> Result<DescribeVpnConnectionsOutput, ApiError> {
        let mut req = self.describe_vpn_connections();
        if !filters.is_empty() {
            req = req.set_filters(Some(filters));
        }
        if !ids.is_empty() {
            req = req.set_vpn_connection_ids(Some(ids));
        }
        req.send().await.map_err(|e| classify(&e))
    }

    async fn get_vpn_tunnel_replacement_status(
        &self,
        connection_id: &str,
        outside_ip: &str,
    ) -> Result<GetVpnTunnelReplacementStatusOutput, ApiError> {
        self.get_vpn_tunnel_replacement_status()
            .vpn_connection_id(connection_id)
            .vpn_tunnel_outside_ip_address(outside_ip)
            .send()
            .await
            .map_err(|e| classify(&e))
    }

    async fn replace_vpn_tunnel(
        &self,
        connection_id: &str,
        outside_ip: &str,
        dry_run: bool,
    ) -> Result<(), ApiError> {
        self.replace_vpn_tunnel()
            .vpn_connection_id(connection_id)
            .vpn_tunnel_outside_ip_address(outside_ip)
            .apply_pending_maintenance(true)
            .dry_run(dry_run)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| classify(&e))
    }
}

#[async_trait]
impl GatewayApi for aws_sdk_ec2::Client {
    async fn describe_transit_gateways(
        &self,
        ids: Vec<String>,
    ) -> Result<DescribeTransitGatewaysOutput, ApiError> {
        self.describe_transit_gateways()
            .set_transit_gateway_ids(Some(ids))
            .send()
            .await
            .map_err(|e| classify(&e))
    }

    async fn describe_vpn_gateways(
        &self,
        ids: Vec<String>,
    ) -> Result<DescribeVpnGatewaysOutput, ApiError> {
        self.describe_vpn_gateways()
            .set_vpn_gateway_ids(Some(ids))
            .send()
            .await
            .map_err(|e| classify(&e))
    }

    async fn describe_customer_gateways(
        &self,
        ids: Vec<String>,
    ) -> Result<DescribeCustomerGatewaysOutput, ApiError> {
        self.describe_customer_gateways()
            .set_customer_gateway_ids(Some(ids))
            .send()
            .await
            .map_err(|e| classify(&e))
    }
}

#[async_trait]
impl StsApi for aws_sdk_sts::Client {
    async fn caller_identity(&self) -> Result<(String, String), ApiError> {
        let out = self.get_caller_identity().send().await.map_err(|e| {
            ApiError::Uncertain(aws_sdk_sts::error::DisplayErrorContext(&e).to_string())
        })?;
        Ok((
            out.arn().unwrap_or_default().to_string(),
            out.account().unwrap_or_default().to_string(),
        ))
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! In-memory API surfaces for tests.

    use std::sync::Mutex;

    use aws_sdk_ec2::types::{
        CustomerGateway, MaintenanceDetails, Tag, TelemetryStatus, TransitGateway, TunnelOption,
        VgwTelemetry, VpnConnection, VpnConnectionOptions, VpnGateway, VpnState,
    };
    use aws_smithy_types::DateTime as SmithyDateTime;

    use super::*;

    #[derive(Default)]
    pub struct FakeEc2 {
        pub connections: Mutex<Vec<VpnConnection>>,
        pub describe_error: Mutex<Option<String>>,
        pub status_error: Mutex<Option<String>>,
        pub statuses: Mutex<Vec<(String, String, Option<MaintenanceDetails>)>>,
        pub replace_result: Mutex<Option<ApiError>>,
        pub replace_calls: Mutex<Vec<(String, String, bool)>>,
        pub describe_calls: Mutex<usize>,
    }

    #[async_trait]
    impl Ec2Api for FakeEc2 {
        async fn describe_vpn_connections(
            &self,
            _filters: Vec<Filter>,
            ids: Vec<String>,
        ) -> Result<DescribeVpnConnectionsOutput, ApiError> {
            *self.describe_calls.lock().unwrap() += 1;
            if let Some(err) = self.describe_error.lock().unwrap().clone() {
                return Err(ApiError::Rejected(err));
            }
            let conns: Vec<VpnConnection> = self
                .connections
                .lock()
                .unwrap()
                .iter()
                .filter(|c| {
                    ids.is_empty()
                        || ids
                            .iter()
                            .any(|id| Some(id.as_str()) == c.vpn_connection_id())
                })
                .cloned()
                .collect();
            Ok(DescribeVpnConnectionsOutput::builder()
                .set_vpn_connections(Some(conns))
                .build())
        }

        async fn get_vpn_tunnel_replacement_status(
            &self,
            connection_id: &str,
            outside_ip: &str,
        ) -> Result<GetVpnTunnelReplacementStatusOutput, ApiError> {
            if let Some(err) = self.status_error.lock().unwrap().clone() {
                return Err(ApiError::Rejected(err));
            }
            let details = self
                .statuses
                .lock()
                .unwrap()
                .iter()
                .find(|(c, ip, _)| c == connection_id && ip == outside_ip)
                .and_then(|(_, _, d)| d.clone());
            Ok(GetVpnTunnelReplacementStatusOutput::builder()
                .set_maintenance_details(details)
                .build())
        }

        async fn replace_vpn_tunnel(
            &self,
            connection_id: &str,
            outside_ip: &str,
            dry_run: bool,
        ) -> Result<(), ApiError> {
            self.replace_calls.lock().unwrap().push((
                connection_id.into(),
                outside_ip.into(),
                dry_run,
            ));
            match self.replace_result.lock().unwrap().as_ref() {
                None => Ok(()),
                Some(ApiError::Rejected(s)) => Err(ApiError::Rejected(s.clone())),
                Some(ApiError::Uncertain(s)) => Err(ApiError::Uncertain(s.clone())),
                Some(ApiError::DryRunSucceeded) => Err(ApiError::DryRunSucceeded),
            }
        }
    }

    #[derive(Default)]
    pub struct FakeGateways {
        pub transit: Vec<(String, String)>,
        pub vpn: Vec<(String, String)>,
        pub customer: Vec<(String, String)>,
        pub fail: bool,
        pub calls: Mutex<usize>,
    }

    fn name_tag(name: &str) -> Vec<Tag> {
        if name.is_empty() {
            Vec::new()
        } else {
            vec![Tag::builder().key("Name").value(name).build()]
        }
    }

    #[async_trait]
    impl GatewayApi for FakeGateways {
        async fn describe_transit_gateways(
            &self,
            ids: Vec<String>,
        ) -> Result<DescribeTransitGatewaysOutput, ApiError> {
            *self.calls.lock().unwrap() += 1;
            if self.fail {
                return Err(ApiError::Rejected("UnauthorizedOperation".into()));
            }
            let out: Vec<TransitGateway> = self
                .transit
                .iter()
                .filter(|(id, _)| ids.contains(id))
                .map(|(id, name)| {
                    TransitGateway::builder()
                        .transit_gateway_id(id)
                        .set_tags(Some(name_tag(name)))
                        .build()
                })
                .collect();
            Ok(DescribeTransitGatewaysOutput::builder()
                .set_transit_gateways(Some(out))
                .build())
        }

        async fn describe_vpn_gateways(
            &self,
            ids: Vec<String>,
        ) -> Result<DescribeVpnGatewaysOutput, ApiError> {
            *self.calls.lock().unwrap() += 1;
            if self.fail {
                return Err(ApiError::Rejected("UnauthorizedOperation".into()));
            }
            let out: Vec<VpnGateway> = self
                .vpn
                .iter()
                .filter(|(id, _)| ids.contains(id))
                .map(|(id, name)| {
                    VpnGateway::builder()
                        .vpn_gateway_id(id)
                        .set_tags(Some(name_tag(name)))
                        .build()
                })
                .collect();
            Ok(DescribeVpnGatewaysOutput::builder()
                .set_vpn_gateways(Some(out))
                .build())
        }

        async fn describe_customer_gateways(
            &self,
            ids: Vec<String>,
        ) -> Result<DescribeCustomerGatewaysOutput, ApiError> {
            *self.calls.lock().unwrap() += 1;
            if self.fail {
                return Err(ApiError::Rejected("UnauthorizedOperation".into()));
            }
            let out: Vec<CustomerGateway> = self
                .customer
                .iter()
                .filter(|(id, _)| ids.contains(id))
                .map(|(id, name)| {
                    CustomerGateway::builder()
                        .customer_gateway_id(id)
                        .set_tags(Some(name_tag(name)))
                        .build()
                })
                .collect();
            Ok(DescribeCustomerGatewaysOutput::builder()
                .set_customer_gateways(Some(out))
                .build())
        }
    }

    #[async_trait]
    impl<T: Ec2Api> Ec2Api for std::sync::Arc<T> {
        async fn describe_vpn_connections(
            &self,
            filters: Vec<Filter>,
            ids: Vec<String>,
        ) -> Result<DescribeVpnConnectionsOutput, ApiError> {
            (**self).describe_vpn_connections(filters, ids).await
        }
        async fn get_vpn_tunnel_replacement_status(
            &self,
            connection_id: &str,
            outside_ip: &str,
        ) -> Result<GetVpnTunnelReplacementStatusOutput, ApiError> {
            (**self)
                .get_vpn_tunnel_replacement_status(connection_id, outside_ip)
                .await
        }
        async fn replace_vpn_tunnel(
            &self,
            connection_id: &str,
            outside_ip: &str,
            dry_run: bool,
        ) -> Result<(), ApiError> {
            (**self)
                .replace_vpn_tunnel(connection_id, outside_ip, dry_run)
                .await
        }
    }

    #[async_trait]
    impl<T: GatewayApi> GatewayApi for std::sync::Arc<T> {
        async fn describe_transit_gateways(
            &self,
            ids: Vec<String>,
        ) -> Result<DescribeTransitGatewaysOutput, ApiError> {
            (**self).describe_transit_gateways(ids).await
        }
        async fn describe_vpn_gateways(
            &self,
            ids: Vec<String>,
        ) -> Result<DescribeVpnGatewaysOutput, ApiError> {
            (**self).describe_vpn_gateways(ids).await
        }
        async fn describe_customer_gateways(
            &self,
            ids: Vec<String>,
        ) -> Result<DescribeCustomerGatewaysOutput, ApiError> {
            (**self).describe_customer_gateways(ids).await
        }
    }

    pub struct FakeSts(pub Result<(String, String), String>);

    #[async_trait]
    impl StsApi for FakeSts {
        async fn caller_identity(&self) -> Result<(String, String), ApiError> {
            self.0.clone().map_err(ApiError::Uncertain)
        }
    }

    /// A telemetry entry; `changed` is the unix time of the last status flip.
    pub fn telemetry(ip: &str, up: bool, routes: i32, changed: i64) -> VgwTelemetry {
        VgwTelemetry::builder()
            .outside_ip_address(ip)
            .status(if up {
                TelemetryStatus::Up
            } else {
                TelemetryStatus::Down
            })
            .status_message(if up { "" } else { "IPSEC IS DOWN" })
            .accepted_route_count(routes)
            .last_status_change(SmithyDateTime::from_secs(changed))
            .build()
    }

    pub fn vpn_connection(id: &str, name: &str, lifecycle: [bool; 2]) -> VpnConnection {
        VpnConnection::builder()
            .vpn_connection_id(id)
            .state(VpnState::Available)
            .transit_gateway_id("tgw-1")
            .customer_gateway_id("cgw-1")
            .set_tags(Some(name_tag(name)))
            .options(
                VpnConnectionOptions::builder()
                    .static_routes_only(false)
                    .tunnel_options(
                        TunnelOption::builder()
                            .outside_ip_address("2.2.2.2")
                            .enable_tunnel_lifecycle_control(lifecycle[1])
                            .build(),
                    )
                    .tunnel_options(
                        TunnelOption::builder()
                            .outside_ip_address("1.1.1.1")
                            .enable_tunnel_lifecycle_control(lifecycle[0])
                            .build(),
                    )
                    .build(),
            )
            .vgw_telemetry(telemetry("2.2.2.2", true, 3, 1_700_000_000))
            .vgw_telemetry(telemetry("1.1.1.1", true, 3, 1_700_000_000))
            .build()
    }

    pub fn pending(after: i64) -> MaintenanceDetails {
        MaintenanceDetails::builder()
            .pending_maintenance("AVAILABLE")
            .maintenance_auto_applied_after(SmithyDateTime::from_secs(after))
            .last_maintenance_applied(SmithyDateTime::from_secs(1_600_000_000))
            .build()
    }
}
