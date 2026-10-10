//! Wraps the AWS SDK with the narrow EC2 and SSM operations the resizer needs,
//! and centralizes credential resolution. The SDK surface sits behind small
//! traits so the discovery, polling, and caching logic is testable with fakes.

pub mod ebs;
pub mod ec2;
pub mod ssm;
pub mod types;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use aws_sdk_ec2::operation::describe_instance_types::DescribeInstanceTypesOutput;
use aws_sdk_ec2::operation::describe_instances::DescribeInstancesOutput;
use aws_sdk_ec2::operation::describe_volumes::DescribeVolumesOutput;
use aws_sdk_ec2::operation::describe_volumes_modifications::DescribeVolumesModificationsOutput;
use aws_sdk_ec2::types::{Filter, InstanceType};
use aws_sdk_ssm::operation::get_command_invocation::GetCommandInvocationOutput;
use aws_sdk_ssm::operation::send_command::SendCommandOutput;
use thiserror::Error;

pub use types::*;

/// An AWS call that failed. `code` carries the service error code when the
/// service answered with one, so callers can special-case known codes.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("{message}")]
pub struct ApiError {
    pub code: Option<String>,
    pub message: String,
}

impl ApiError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            code: None,
            message: message.into(),
        }
    }

    /// Reduces an SDK error to its code and rendered message.
    pub fn from_sdk<E, R>(err: &aws_sdk_ec2::error::SdkError<E, R>) -> Self
    where
        E: std::error::Error + aws_sdk_ec2::error::ProvideErrorMetadata + 'static,
        R: std::fmt::Debug,
    {
        use aws_sdk_ec2::error::ProvideErrorMetadata as _;
        Self {
            code: err.code().map(str::to_string),
            message: aws_sdk_ec2::error::DisplayErrorContext(err).to_string(),
        }
    }
}

/// The subset of the EC2 SDK client used here. The concrete client satisfies
/// it; tests provide fakes. Pagination is driven by the caller through
/// `next_token`, so a fake can hand back several pages.
#[async_trait]
pub trait Ec2Sdk: Send + Sync {
    async fn describe_instances(
        &self,
        filters: Vec<Filter>,
        next_token: Option<String>,
    ) -> Result<DescribeInstancesOutput, ApiError>;
    async fn describe_volumes(
        &self,
        volume_ids: Vec<String>,
        filters: Vec<Filter>,
        next_token: Option<String>,
    ) -> Result<DescribeVolumesOutput, ApiError>;
    async fn describe_instance_types(
        &self,
        instance_types: Vec<InstanceType>,
        next_token: Option<String>,
    ) -> Result<DescribeInstanceTypesOutput, ApiError>;
    async fn modify_volume(
        &self,
        volume_id: &str,
        size_gib: i32,
        throughput: Option<i32>,
        iops: Option<i32>,
    ) -> Result<(), ApiError>;
    async fn describe_volumes_modifications(
        &self,
        volume_id: &str,
    ) -> Result<DescribeVolumesModificationsOutput, ApiError>;
}

/// The subset of the SSM SDK client used here.
#[async_trait]
pub trait SsmSdk: Send + Sync {
    async fn send_command(
        &self,
        instance_id: &str,
        document: &str,
        timeout_seconds: i32,
        script: &str,
    ) -> Result<SendCommandOutput, ApiError>;
    async fn get_command_invocation(
        &self,
        command_id: &str,
        instance_id: &str,
    ) -> Result<GetCommandInvocationOutput, ApiError>;
}

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Bundles the AWS service clients used by the resizer.
pub struct Clients {
    pub ec2: Box<dyn Ec2Sdk>,
    pub ssm: Box<dyn SsmSdk>,
    /// The delay between status polls for volume modifications and SSM
    /// command invocations.
    pub poll_interval: Duration,
    /// Caches instance-type EBS bandwidth caps for the process lifetime. The
    /// data is static AWS catalog information, and reconcile passes run
    /// concurrently, so access is mutex-guarded.
    ebs_caps: Mutex<HashMap<String, EbsCaps>>,
}

impl Clients {
    /// Builds AWS service clients for the given region. Credentials resolve
    /// via the default chain, which in-cluster is the Pod's own IRSA or Pod
    /// Identity role; the Pod always operates under its own identity.
    pub async fn new(region: &str) -> Self {
        let cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region.to_string()))
            .load()
            .await;
        Self::from_parts(
            Box::new(aws_sdk_ec2::Client::new(&cfg)),
            Box::new(aws_sdk_ssm::Client::new(&cfg)),
        )
    }

    /// Builds a client bundle over the given SDK surfaces, for tests.
    #[must_use]
    pub fn from_parts(ec2: Box<dyn Ec2Sdk>, ssm: Box<dyn SsmSdk>) -> Self {
        Self {
            ec2,
            ssm,
            poll_interval: DEFAULT_POLL_INTERVAL,
            ebs_caps: Mutex::new(HashMap::new()),
        }
    }

    const fn poll_interval(&self) -> Duration {
        if self.poll_interval.is_zero() {
            DEFAULT_POLL_INTERVAL
        } else {
            self.poll_interval
        }
    }
}

#[async_trait]
impl Ec2Sdk for aws_sdk_ec2::Client {
    async fn describe_instances(
        &self,
        filters: Vec<Filter>,
        next_token: Option<String>,
    ) -> Result<DescribeInstancesOutput, ApiError> {
        self.describe_instances()
            .set_filters(Some(filters))
            .set_next_token(next_token)
            .send()
            .await
            .map_err(|e| ApiError::from_sdk(&e))
    }

    async fn describe_volumes(
        &self,
        volume_ids: Vec<String>,
        filters: Vec<Filter>,
        next_token: Option<String>,
    ) -> Result<DescribeVolumesOutput, ApiError> {
        let mut req = self.describe_volumes().set_next_token(next_token);
        if !volume_ids.is_empty() {
            req = req.set_volume_ids(Some(volume_ids));
        }
        if !filters.is_empty() {
            req = req.set_filters(Some(filters));
        }
        req.send().await.map_err(|e| ApiError::from_sdk(&e))
    }

    async fn describe_instance_types(
        &self,
        instance_types: Vec<InstanceType>,
        next_token: Option<String>,
    ) -> Result<DescribeInstanceTypesOutput, ApiError> {
        self.describe_instance_types()
            .set_instance_types(Some(instance_types))
            .set_next_token(next_token)
            .send()
            .await
            .map_err(|e| ApiError::from_sdk(&e))
    }

    async fn modify_volume(
        &self,
        volume_id: &str,
        size_gib: i32,
        throughput: Option<i32>,
        iops: Option<i32>,
    ) -> Result<(), ApiError> {
        self.modify_volume()
            .volume_id(volume_id)
            .size(size_gib)
            .set_throughput(throughput)
            .set_iops(iops)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| ApiError::from_sdk(&e))
    }

    async fn describe_volumes_modifications(
        &self,
        volume_id: &str,
    ) -> Result<DescribeVolumesModificationsOutput, ApiError> {
        self.describe_volumes_modifications()
            .volume_ids(volume_id)
            .send()
            .await
            .map_err(|e| ApiError::from_sdk(&e))
    }
}

#[async_trait]
impl SsmSdk for aws_sdk_ssm::Client {
    async fn send_command(
        &self,
        instance_id: &str,
        document: &str,
        timeout_seconds: i32,
        script: &str,
    ) -> Result<SendCommandOutput, ApiError> {
        self.send_command()
            .instance_ids(instance_id)
            .document_name(document)
            .timeout_seconds(timeout_seconds)
            .parameters("commands", vec![script.to_string()])
            .send()
            .await
            .map_err(|e| ApiError::new(aws_sdk_ssm::error::DisplayErrorContext(&e).to_string()))
    }

    async fn get_command_invocation(
        &self,
        command_id: &str,
        instance_id: &str,
    ) -> Result<GetCommandInvocationOutput, ApiError> {
        self.get_command_invocation()
            .command_id(command_id)
            .instance_id(instance_id)
            .send()
            .await
            .map_err(|e| ApiError::new(aws_sdk_ssm::error::DisplayErrorContext(&e).to_string()))
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! In-memory SDK surfaces for tests.

    use std::sync::Mutex;

    use aws_sdk_ec2::types::{
        EbsInfo, EbsInstanceBlockDevice, EbsOptimizedInfo, Instance, InstanceBlockDeviceMapping,
        InstanceTypeInfo, Reservation, Tag, Volume, VolumeAttachment, VolumeModification,
        VolumeModificationState, VolumeType,
    };
    use aws_sdk_ssm::types::{Command, CommandInvocationStatus};

    use super::*;

    /// `(volume_id, size_gib, throughput, iops)` of one `ModifyVolume` call.
    pub type ModifyCall = (String, i32, Option<i32>, Option<i32>);

    #[derive(Default)]
    pub struct FakeEc2 {
        /// Pages of instances; each inner Vec is one page.
        pub instance_pages: Mutex<Vec<Vec<Instance>>>,
        pub volumes: Mutex<Vec<Volume>>,
        pub instance_types: Mutex<Vec<InstanceTypeInfo>>,
        pub modifications: Mutex<Vec<VolumeModification>>,
        pub modification_error: Mutex<Option<ApiError>>,
        pub describe_error: Mutex<Option<ApiError>>,
        pub modify_error: Mutex<Option<ApiError>>,
        pub modify_calls: Mutex<Vec<ModifyCall>>,
        pub describe_volume_calls: Mutex<Vec<(Vec<String>, Vec<Filter>)>>,
        pub describe_type_calls: Mutex<Vec<Vec<String>>>,
        /// Each call pops the next modification list; when empty the last
        /// entry of `modifications` is served. Lets a wait test progress
        /// through states.
        pub modification_sequence: Mutex<Vec<Vec<VolumeModification>>>,
    }

    pub fn tag(k: &str, v: &str) -> Tag {
        Tag::builder().key(k).value(v).build()
    }

    pub fn instance(id: &str, name: &str, root: &str, volume: &str, extra: &[Tag]) -> Instance {
        let mut tags = vec![tag("Name", name)];
        tags.extend_from_slice(extra);
        Instance::builder()
            .instance_id(id)
            .root_device_name(root)
            .set_tags(Some(tags))
            .block_device_mappings(
                InstanceBlockDeviceMapping::builder()
                    .device_name(root)
                    .ebs(EbsInstanceBlockDevice::builder().volume_id(volume).build())
                    .build(),
            )
            .build()
    }

    pub fn volume(id: &str, size: i32) -> Volume {
        Volume::builder().volume_id(id).size(size).build()
    }

    pub fn attached_volume(
        id: &str,
        vtype: VolumeType,
        size: i32,
        throughput: i32,
        iops: i32,
        attachments: &[(&str, &str)],
    ) -> Volume {
        let mut b = Volume::builder()
            .volume_id(id)
            .volume_type(vtype)
            .size(size)
            .throughput(throughput)
            .iops(iops);
        for (inst, dev) in attachments {
            b = b.attachments(
                VolumeAttachment::builder()
                    .instance_id(*inst)
                    .device(*dev)
                    .build(),
            );
        }
        b.build()
    }

    pub fn instance_type(
        name: &str,
        baseline: Option<f64>,
        maximum: Option<f64>,
    ) -> InstanceTypeInfo {
        let mut info = EbsOptimizedInfo::builder();
        if let Some(b) = baseline {
            info = info.baseline_throughput_in_m_bps(b);
        }
        if let Some(m) = maximum {
            info = info.maximum_throughput_in_m_bps(m);
        }
        InstanceTypeInfo::builder()
            .instance_type(InstanceType::from(name))
            .ebs_info(EbsInfo::builder().ebs_optimized_info(info.build()).build())
            .build()
    }

    pub fn modification(
        state: VolumeModificationState,
        start_secs: i64,
        target: i32,
    ) -> VolumeModification {
        VolumeModification::builder()
            .modification_state(state)
            .start_time(aws_smithy_types::DateTime::from_secs(start_secs))
            .target_size(target)
            .build()
    }

    #[async_trait]
    impl Ec2Sdk for FakeEc2 {
        async fn describe_instances(
            &self,
            _filters: Vec<Filter>,
            next_token: Option<String>,
        ) -> Result<DescribeInstancesOutput, ApiError> {
            if let Some(err) = self.describe_error.lock().unwrap().clone() {
                return Err(err);
            }
            let pages = self.instance_pages.lock().unwrap();
            let idx: usize = next_token.as_deref().map_or(0, |t| t.parse().unwrap());
            let page = pages.get(idx).cloned().unwrap_or_default();
            let next = if idx + 1 < pages.len() {
                Some((idx + 1).to_string())
            } else {
                None
            };
            Ok(DescribeInstancesOutput::builder()
                .reservations(Reservation::builder().set_instances(Some(page)).build())
                .set_next_token(next)
                .build())
        }

        async fn describe_volumes(
            &self,
            volume_ids: Vec<String>,
            filters: Vec<Filter>,
            _next_token: Option<String>,
        ) -> Result<DescribeVolumesOutput, ApiError> {
            if let Some(err) = self.describe_error.lock().unwrap().clone() {
                return Err(err);
            }
            self.describe_volume_calls
                .lock()
                .unwrap()
                .push((volume_ids.clone(), filters.clone()));
            let instance_filter: Vec<String> = filters
                .iter()
                .find(|f| f.name() == Some("attachment.instance-id"))
                .map(|f| f.values().to_vec())
                .unwrap_or_default();
            let vols: Vec<Volume> = self
                .volumes
                .lock()
                .unwrap()
                .iter()
                .filter(|v| {
                    if !volume_ids.is_empty() {
                        return volume_ids
                            .iter()
                            .any(|id| Some(id.as_str()) == v.volume_id());
                    }
                    if instance_filter.is_empty() {
                        return true;
                    }
                    v.attachments().iter().any(|a| {
                        a.instance_id()
                            .is_some_and(|i| instance_filter.iter().any(|f| f == i))
                    })
                })
                .cloned()
                .collect();
            Ok(DescribeVolumesOutput::builder()
                .set_volumes(Some(vols))
                .build())
        }

        async fn describe_instance_types(
            &self,
            instance_types: Vec<InstanceType>,
            _next_token: Option<String>,
        ) -> Result<DescribeInstanceTypesOutput, ApiError> {
            if let Some(err) = self.describe_error.lock().unwrap().clone() {
                return Err(err);
            }
            let names: Vec<String> = instance_types
                .iter()
                .map(|t| t.as_str().to_string())
                .collect();
            self.describe_type_calls.lock().unwrap().push(names.clone());
            let out: Vec<InstanceTypeInfo> = self
                .instance_types
                .lock()
                .unwrap()
                .iter()
                .filter(|t| {
                    t.instance_type()
                        .is_some_and(|it| names.iter().any(|n| n == it.as_str()))
                })
                .cloned()
                .collect();
            Ok(DescribeInstanceTypesOutput::builder()
                .set_instance_types(Some(out))
                .build())
        }

        async fn modify_volume(
            &self,
            volume_id: &str,
            size_gib: i32,
            throughput: Option<i32>,
            iops: Option<i32>,
        ) -> Result<(), ApiError> {
            self.modify_calls
                .lock()
                .unwrap()
                .push((volume_id.into(), size_gib, throughput, iops));
            self.modify_error
                .lock()
                .unwrap()
                .clone()
                .map_or(Ok(()), Err)
        }

        async fn describe_volumes_modifications(
            &self,
            _volume_id: &str,
        ) -> Result<DescribeVolumesModificationsOutput, ApiError> {
            if let Some(err) = self.modification_error.lock().unwrap().clone() {
                return Err(err);
            }
            let mut seq = self.modification_sequence.lock().unwrap();
            let mods = if seq.is_empty() {
                self.modifications.lock().unwrap().clone()
            } else {
                seq.remove(0)
            };
            Ok(DescribeVolumesModificationsOutput::builder()
                .set_volumes_modifications(Some(mods))
                .build())
        }
    }

    #[derive(Default)]
    pub struct FakeSsm {
        pub send_error: Mutex<Option<ApiError>>,
        pub sent: Mutex<Vec<(String, String, i32, String)>>,
        /// Each poll returns the next invocation; the last one repeats.
        pub invocations: Mutex<Vec<GetCommandInvocationOutput>>,
        pub invocation_error: Mutex<Option<ApiError>>,
    }

    pub fn invocation(
        status: CommandInvocationStatus,
        code: i32,
        stdout: &str,
        stderr: &str,
    ) -> GetCommandInvocationOutput {
        GetCommandInvocationOutput::builder()
            .status(status)
            .response_code(code)
            .standard_output_content(stdout)
            .standard_error_content(stderr)
            .build()
    }

    #[async_trait]
    impl SsmSdk for FakeSsm {
        async fn send_command(
            &self,
            instance_id: &str,
            document: &str,
            timeout_seconds: i32,
            script: &str,
        ) -> Result<SendCommandOutput, ApiError> {
            if let Some(err) = self.send_error.lock().unwrap().clone() {
                return Err(err);
            }
            self.sent.lock().unwrap().push((
                instance_id.into(),
                document.into(),
                timeout_seconds,
                script.into(),
            ));
            Ok(SendCommandOutput::builder()
                .command(Command::builder().command_id("cmd-1").build())
                .build())
        }

        async fn get_command_invocation(
            &self,
            _command_id: &str,
            _instance_id: &str,
        ) -> Result<GetCommandInvocationOutput, ApiError> {
            if let Some(err) = self.invocation_error.lock().unwrap().clone() {
                return Err(err);
            }
            let mut inv = self.invocations.lock().unwrap();
            if inv.len() > 1 {
                Ok(inv.remove(0))
            } else {
                inv.first()
                    .cloned()
                    .ok_or_else(|| ApiError::new("InvocationDoesNotExist"))
            }
        }
    }

    #[async_trait]
    impl<T: Ec2Sdk> Ec2Sdk for std::sync::Arc<T> {
        async fn describe_instances(
            &self,
            filters: Vec<Filter>,
            next_token: Option<String>,
        ) -> Result<DescribeInstancesOutput, ApiError> {
            (**self).describe_instances(filters, next_token).await
        }
        async fn describe_volumes(
            &self,
            volume_ids: Vec<String>,
            filters: Vec<Filter>,
            next_token: Option<String>,
        ) -> Result<DescribeVolumesOutput, ApiError> {
            (**self)
                .describe_volumes(volume_ids, filters, next_token)
                .await
        }
        async fn describe_instance_types(
            &self,
            instance_types: Vec<InstanceType>,
            next_token: Option<String>,
        ) -> Result<DescribeInstanceTypesOutput, ApiError> {
            (**self)
                .describe_instance_types(instance_types, next_token)
                .await
        }
        async fn modify_volume(
            &self,
            volume_id: &str,
            size_gib: i32,
            throughput: Option<i32>,
            iops: Option<i32>,
        ) -> Result<(), ApiError> {
            (**self)
                .modify_volume(volume_id, size_gib, throughput, iops)
                .await
        }
        async fn describe_volumes_modifications(
            &self,
            volume_id: &str,
        ) -> Result<DescribeVolumesModificationsOutput, ApiError> {
            (**self).describe_volumes_modifications(volume_id).await
        }
    }

    #[async_trait]
    impl<T: SsmSdk> SsmSdk for std::sync::Arc<T> {
        async fn send_command(
            &self,
            instance_id: &str,
            document: &str,
            timeout_seconds: i32,
            script: &str,
        ) -> Result<SendCommandOutput, ApiError> {
            (**self)
                .send_command(instance_id, document, timeout_seconds, script)
                .await
        }
        async fn get_command_invocation(
            &self,
            command_id: &str,
            instance_id: &str,
        ) -> Result<GetCommandInvocationOutput, ApiError> {
            (**self)
                .get_command_invocation(command_id, instance_id)
                .await
        }
    }

    /// Builds a client bundle over shared fakes, so a test keeps handles to
    /// inspect the calls made.
    pub fn clients(
        ec2: FakeEc2,
        ssm: FakeSsm,
    ) -> (Clients, std::sync::Arc<FakeEc2>, std::sync::Arc<FakeSsm>) {
        let ec2 = std::sync::Arc::new(ec2);
        let ssm = std::sync::Arc::new(ssm);
        let mut c = Clients::from_parts(Box::new(ec2.clone()), Box::new(ssm.clone()));
        c.poll_interval = Duration::from_millis(1);
        (c, ec2, ssm)
    }
}
