//! EC2 `DescribeInstances` paging and conversion into domain types.

use aws_sdk_ec2::Client;
use aws_sdk_ec2::primitives::DateTime;
use aws_sdk_ec2::types::{Filter, Instance as SdkInstance, InstanceLifecycleType, Placement, Tag};

use crate::types::Instance;

/// Instance states the exporter publishes. Terminated instances are excluded
/// server-side so they never reach the snapshot.
const PUBLISHED_STATES: [&str; 4] = ["pending", "running", "stopping", "stopped"];

/// Anything that can produce the current instance set. The collector is
/// generic over this so tests never touch the SDK.
pub trait InstanceSource: Send + Sync + 'static {
    fn describe_all(&self) -> impl Future<Output = anyhow::Result<Vec<Instance>>> + Send;
}

/// `InstanceSource` backed by the real EC2 client.
#[derive(Clone)]
pub struct Ec2Source {
    client: Client,
}

impl Ec2Source {
    pub const fn new(client: Client) -> Self {
        Self { client }
    }

    /// Build a client from the SDK default chain, overriding the region when
    /// one is configured.
    pub async fn from_env(region: Option<String>) -> Self {
        let mut loader = aws_config::from_env();
        if let Some(region) = region {
            loader = loader.region(aws_sdk_ec2::config::Region::new(region));
        }
        let cfg = loader.load().await;
        Self::new(Client::new(&cfg))
    }
}

impl InstanceSource for Ec2Source {
    /// Page through `DescribeInstances` and return every non-terminated
    /// instance that has a private IP address.
    async fn describe_all(&self) -> anyhow::Result<Vec<Instance>> {
        let filter = Filter::builder()
            .name("instance-state-name")
            .set_values(Some(
                PUBLISHED_STATES.iter().map(ToString::to_string).collect(),
            ))
            .build();

        let mut pages = self
            .client
            .describe_instances()
            .filters(filter)
            .into_paginator()
            .send();

        let mut instances = Vec::new();
        while let Some(page) = pages.next().await {
            let page = page?;
            for reservation in page.reservations() {
                instances.extend(reservation.instances().iter().filter_map(convert));
            }
        }
        Ok(instances)
    }
}

/// Map an SDK instance onto the exporter's view. Instances without a private
/// IP carry nothing useful for IP-to-name resolution and are dropped.
fn convert(inst: &SdkInstance) -> Option<Instance> {
    let private_ip = inst.private_ip_address()?.to_string();
    Some(Instance {
        id: inst.instance_id().unwrap_or_default().to_string(),
        name: name_tag(inst.tags()),
        private_ip,
        instance_type: inst
            .instance_type()
            .map(|t| t.as_str().to_string())
            .unwrap_or_default(),
        availability_zone: availability_zone(inst.placement()),
        state: inst
            .state()
            .and_then(|s| s.name())
            .map(|n| n.as_str().to_string())
            .unwrap_or_default(),
        lifecycle: lifecycle(inst.instance_lifecycle().map(InstanceLifecycleType::as_str)),
        architecture: inst
            .architecture()
            .map(|a| a.as_str().to_string())
            .unwrap_or_default(),
        launch_time: inst.launch_time().map(DateTime::secs),
    })
}

fn availability_zone(placement: Option<&Placement>) -> String {
    placement
        .and_then(Placement::availability_zone)
        .unwrap_or_default()
        .to_string()
}

/// The `InstanceLifecycle` field is absent for on-demand instances and `spot`
/// for Spot; other values (scheduled, capacity-block) pass through verbatim.
fn lifecycle(l: Option<&str>) -> String {
    match l {
        None | Some("") => "on-demand".to_string(),
        Some(other) => other.to_string(),
    }
}

fn name_tag(tags: &[Tag]) -> String {
    tags.iter()
        .find(|t| t.key() == Some("Name"))
        .and_then(Tag::value)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use aws_sdk_ec2::error::ErrorMetadata;
    use aws_sdk_ec2::operation::describe_instances::{
        DescribeInstancesError, DescribeInstancesOutput,
    };
    use aws_sdk_ec2::types::{
        ArchitectureValues, InstanceState, InstanceStateName, InstanceType, Reservation,
    };
    use aws_smithy_mocks::{RuleMode, mock, mock_client};

    use super::*;

    fn sdk_instance(id: &str, ip: Option<&str>) -> SdkInstance {
        let mut b = SdkInstance::builder()
            .instance_id(id)
            .instance_type(InstanceType::M5Large)
            .architecture(ArchitectureValues::X8664)
            .placement(
                Placement::builder()
                    .availability_zone("ap-northeast-2a")
                    .build(),
            )
            .state(
                InstanceState::builder()
                    .name(InstanceStateName::Running)
                    .build(),
            )
            .launch_time(DateTime::from_secs(1_752_994_800))
            .tags(Tag::builder().key("env").value("prod").build())
            .tags(Tag::builder().key("Name").value("web-1").build());
        if let Some(ip) = ip {
            b = b.private_ip_address(ip);
        }
        b.build()
    }

    fn page(instances: Vec<SdkInstance>, next: Option<&str>) -> DescribeInstancesOutput {
        DescribeInstancesOutput::builder()
            .reservations(
                Reservation::builder()
                    .set_instances(Some(instances))
                    .build(),
            )
            .set_next_token(next.map(str::to_string))
            .build()
    }

    #[test]
    fn convert_maps_all_fields() {
        let inst = convert(&sdk_instance("i-0abc123", Some("10.0.1.10"))).expect("has ip");
        assert_eq!(
            inst,
            Instance {
                id: "i-0abc123".into(),
                name: "web-1".into(),
                private_ip: "10.0.1.10".into(),
                instance_type: "m5.large".into(),
                availability_zone: "ap-northeast-2a".into(),
                state: "running".into(),
                lifecycle: "on-demand".into(),
                architecture: "x86_64".into(),
                launch_time: Some(1_752_994_800),
            }
        );
    }

    #[test]
    fn convert_drops_instances_without_private_ip() {
        assert!(convert(&sdk_instance("i-0abc123", None)).is_none());
    }

    #[test]
    fn convert_handles_sparse_instance() {
        let inst = convert(
            &SdkInstance::builder()
                .private_ip_address("10.0.0.1")
                .instance_lifecycle(InstanceLifecycleType::Spot)
                .build(),
        )
        .expect("has ip");
        assert_eq!(inst.id, "");
        assert_eq!(inst.name, "");
        assert_eq!(inst.availability_zone, "");
        assert_eq!(inst.state, "");
        assert_eq!(inst.lifecycle, "spot");
        assert_eq!(inst.launch_time, None);
    }

    #[test]
    fn lifecycle_defaults_to_on_demand() {
        assert_eq!(lifecycle(None), "on-demand");
        assert_eq!(lifecycle(Some("")), "on-demand");
        assert_eq!(lifecycle(Some("spot")), "spot");
        assert_eq!(lifecycle(Some("capacity-block")), "capacity-block");
    }

    #[test]
    fn name_tag_lookup() {
        assert_eq!(name_tag(&[]), "");
        let tags = [
            Tag::builder().key("Name").build(),
            Tag::builder().key("Name").value("x").build(),
        ];
        assert_eq!(name_tag(&tags), "");
    }

    #[tokio::test]
    async fn describe_all_follows_pagination() {
        let first = mock!(Client::describe_instances)
            .match_requests(|req| req.next_token().is_none())
            .then_output(|| {
                page(
                    vec![
                        sdk_instance("i-1", Some("10.0.0.1")),
                        sdk_instance("i-2", None),
                    ],
                    Some("page-2"),
                )
            });
        let second = mock!(Client::describe_instances)
            .match_requests(|req| req.next_token() == Some("page-2"))
            .then_output(|| page(vec![sdk_instance("i-3", Some("10.0.0.3"))], None));
        let client = mock_client!(aws_sdk_ec2, RuleMode::MatchAny, [&first, &second]);

        let instances = Ec2Source::new(client)
            .describe_all()
            .await
            .expect("describe succeeds");
        let ids: Vec<&str> = instances.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["i-1", "i-3"]);
        assert_eq!(first.num_calls(), 1);
        assert_eq!(second.num_calls(), 1);
    }

    #[tokio::test]
    async fn describe_all_propagates_errors() {
        let rule = mock!(Client::describe_instances).then_error(|| {
            DescribeInstancesError::generic(
                ErrorMetadata::builder()
                    .code("UnauthorizedOperation")
                    .message("not authorized")
                    .build(),
            )
        });
        let client = mock_client!(aws_sdk_ec2, [&rule]);
        let err = Ec2Source::new(client)
            .describe_all()
            .await
            .expect_err("403 must fail");
        assert!(
            format!("{err:#}").contains("UnauthorizedOperation"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn from_env_builds_client_with_region() {
        // Static credentials keep the default chain off the network.
        let src = Ec2Source::from_env(Some("us-east-1".into())).await;
        assert_eq!(
            src.client.config().region().map(AsRef::as_ref),
            Some("us-east-1")
        );
    }
}
