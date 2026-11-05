//! Instance discovery and the mutating volume operations the resizer needs.

use std::time::{Duration, Instant};

use aws_sdk_ec2::types::{Filter, Instance as SdkInstance};
use chrono::{DateTime, Utc};

use super::{ApiError, Clients, Instance, ModifySpec, VolumeModification};

/// Volume IDs per `DescribeVolumes` call.
const VOLUME_BATCH: usize = 100;

impl Clients {
    /// Returns every running instance in the account/region, resolving each
    /// instance's root EBS volume ID and current size. Which of them are
    /// resized is decided by the `InstanceVolumeAutoscalers`.
    pub async fn describe_target_instances(&self) -> Result<Vec<Instance>, ApiError> {
        let ec2_filters = vec![
            Filter::builder()
                .name("instance-state-name")
                .values("running")
                .build(),
        ];

        let mut instances = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page = self
                .ec2
                .describe_instances(ec2_filters.clone(), token.take())
                .await
                .map_err(|e| ApiError {
                    code: e.code,
                    message: format!("describe instances: {}", e.message),
                })?;
            for res in page.reservations() {
                for inst in res.instances() {
                    instances.push(new_instance(inst));
                }
            }
            token = page.next_token().map(str::to_string);
            if token.is_none() {
                break;
            }
        }

        let ids: Vec<String> = instances
            .iter()
            .filter(|i| !i.root_volume_id.is_empty())
            .map(|i| i.root_volume_id.clone())
            .collect();
        let sizes = self.volume_sizes(&ids).await?;
        for inst in &mut instances {
            if inst.root_volume_id.is_empty() {
                continue;
            }
            inst.root_volume_size_gib = *sizes.get(&inst.root_volume_id).ok_or_else(|| {
                ApiError::new(format!("volume {} not found", inst.root_volume_id))
            })?;
        }
        Ok(instances)
    }

    /// The size of each volume in GiB, read in batches rather than one call
    /// per volume.
    async fn volume_sizes(
        &self,
        volume_ids: &[String],
    ) -> Result<std::collections::HashMap<String, i32>, ApiError> {
        let mut sizes = std::collections::HashMap::with_capacity(volume_ids.len());
        for chunk in volume_ids.chunks(VOLUME_BATCH) {
            let mut token: Option<String> = None;
            loop {
                let page = self
                    .ec2
                    .describe_volumes(chunk.to_vec(), Vec::new(), token.take())
                    .await
                    .map_err(|e| ApiError {
                        code: e.code,
                        message: format!("describe volumes: {}", e.message),
                    })?;
                for v in page.volumes() {
                    if let Some(id) = v.volume_id() {
                        sizes.insert(id.to_string(), v.size().unwrap_or(0));
                    }
                }
                token = page.next_token().map(str::to_string);
                if token.is_none() {
                    break;
                }
            }
        }
        Ok(sizes)
    }

    /// Requests the changes in `spec` for the given EBS volume in a single
    /// EC2 call. Zero-valued spec fields are omitted from the request, so a
    /// size-only spec behaves exactly as a plain size change.
    pub async fn modify_volume(&self, volume_id: &str, spec: ModifySpec) -> Result<(), ApiError> {
        let throughput = (spec.throughput_mibps > 0).then_some(spec.throughput_mibps);
        let iops = (spec.iops > 0).then_some(spec.iops);
        self.ec2
            .modify_volume(volume_id, spec.size_gib, throughput, iops)
            .await
            .map_err(|e| ApiError {
                code: e.code,
                message: format!(
                    "modify volume {volume_id} to {} GiB: {}",
                    spec.size_gib, e.message
                ),
            })
    }

    /// Returns the most recent modification for a volume, or `None` if the
    /// volume has never been modified.
    pub async fn describe_last_modification(
        &self,
        volume_id: &str,
    ) -> Result<Option<VolumeModification>, ApiError> {
        let out = match self.ec2.describe_volumes_modifications(volume_id).await {
            Ok(out) => out,
            // A volume that has never been modified has no modification
            // history, and EC2 signals this with an
            // InvalidVolumeModification.NotFound error rather than an empty
            // result. Treat it as "no modification" so a never-resized volume
            // is not stuck failing the cooldown check.
            Err(e) if e.code.as_deref() == Some("InvalidVolumeModification.NotFound") => {
                return Ok(None);
            }
            Err(e) => {
                return Err(ApiError {
                    code: e.code,
                    message: format!("describe volume modifications {volume_id}: {}", e.message),
                });
            }
        };
        let Some(m) = out.volumes_modifications().first() else {
            return Ok(None);
        };
        Ok(Some(VolumeModification {
            state: m
                .modification_state()
                .map(|s| s.as_str().to_string())
                .unwrap_or_default(),
            start_time: m.start_time().and_then(to_chrono),
            target_gib: m.target_size().unwrap_or(0),
        }))
    }

    /// Polls until the volume modification reaches `optimizing` or
    /// `completed` (filesystem extension is safe from optimizing onward), or
    /// the timeout elapses.
    pub async fn wait_for_modification(
        &self,
        volume_id: &str,
        timeout: Duration,
    ) -> Result<(), ApiError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(m) = self.describe_last_modification(volume_id).await? {
                match m.state.as_str() {
                    "optimizing" | "completed" => return Ok(()),
                    "failed" => {
                        return Err(ApiError::new(format!(
                            "volume {volume_id} modification failed"
                        )));
                    }
                    _ => {}
                }
            }
            if Instant::now() > deadline {
                return Err(ApiError::new(format!(
                    "volume {volume_id} modification did not reach optimizing within {}: modification still in progress",
                    crate::humanize::go_duration(timeout)
                )));
            }
            tokio::time::sleep(self.poll_interval()).await;
        }
    }
}

/// Converts an SDK timestamp to chrono.
pub(super) fn to_chrono(t: &aws_smithy_types::DateTime) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(t.secs(), t.subsec_nanos())
}

fn new_instance(inst: &SdkInstance) -> Instance {
    let mut out = Instance {
        id: inst.instance_id().unwrap_or_default().to_string(),
        root_device_name: inst.root_device_name().unwrap_or_default().to_string(),
        ..Instance::default()
    };
    for tag in inst.tags() {
        out.tags.insert(
            tag.key().unwrap_or_default().to_string(),
            tag.value().unwrap_or_default().to_string(),
        );
    }
    out.name = out.tags.get("Name").cloned().unwrap_or_default();
    for bdm in inst.block_device_mappings() {
        if bdm.device_name() == Some(out.root_device_name.as_str())
            && let Some(ebs) = bdm.ebs()
        {
            out.root_volume_id = ebs.volume_id().unwrap_or_default().to_string();
        }
    }
    out
}

#[cfg(test)]
#[allow(unused_variables)]
mod tests {
    use aws_sdk_ec2::types::VolumeModificationState;

    use super::super::fake::{self, FakeEc2, FakeSsm, clients};
    use super::*;

    #[tokio::test]
    async fn describe_target_instances_paginates_and_resolves_sizes() {
        let ec2 = FakeEc2::default();
        *ec2.instance_pages.lock().unwrap() = vec![
            vec![fake::instance("i-1", "web", "/dev/xvda", "vol-1", &[])],
            vec![
                fake::instance(
                    "i-2",
                    "db",
                    "/dev/nvme0n1",
                    "vol-2",
                    &[fake::tag("Role", "db")],
                ),
                aws_sdk_ec2::types::Instance::builder()
                    .instance_id("i-3")
                    .root_device_name("/dev/xvda")
                    .build(),
            ],
        ];
        *ec2.volumes.lock().unwrap() = vec![fake::volume("vol-1", 30), fake::volume("vol-2", 100)];
        let (c, ec2, _) = clients(ec2, FakeSsm::default());
        let got = c.describe_target_instances().await.unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].id, "i-1");
        assert_eq!(got[0].name, "web");
        assert_eq!(got[0].root_volume_id, "vol-1");
        assert_eq!(got[0].root_volume_size_gib, 30);
        assert_eq!(got[1].root_volume_size_gib, 100);
        assert_eq!(got[1].tags.get("Role").unwrap(), "db");
        assert!(got[2].root_volume_id.is_empty(), "no block device mapping");
        assert_eq!(got[2].root_volume_size_gib, 0);
        let calls = ec2.describe_volume_calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "sizes resolved in one batched call");
        assert_eq!(calls[0].0, vec!["vol-1".to_string(), "vol-2".to_string()]);
    }

    #[tokio::test]
    async fn describe_target_instances_errors() {
        let ec2 = FakeEc2::default();
        *ec2.describe_error.lock().unwrap() = Some(ApiError::new("denied"));
        let (c, ec2, _) = clients(ec2, FakeSsm::default());
        let err = c.describe_target_instances().await.unwrap_err();
        assert!(err.message.contains("describe instances: denied"), "{err}");

        let ec2 = FakeEc2::default();
        *ec2.instance_pages.lock().unwrap() =
            vec![vec![fake::instance("i-1", "w", "/dev/xvda", "vol-x", &[])]];
        let (c, ec2, _) = clients(ec2, FakeSsm::default());
        let err = c.describe_target_instances().await.unwrap_err();
        assert!(err.message.contains("volume vol-x not found"), "{err}");
    }

    #[tokio::test]
    async fn modify_volume_omits_zero_fields() {
        let ec2 = FakeEc2::default();
        let (c, ec2, _) = clients(ec2, FakeSsm::default());
        c.modify_volume(
            "vol-1",
            ModifySpec {
                size_gib: 110,
                ..ModifySpec::default()
            },
        )
        .await
        .unwrap();
        c.modify_volume(
            "vol-1",
            ModifySpec {
                size_gib: 120,
                throughput_mibps: 250,
                iops: 4000,
            },
        )
        .await
        .unwrap();
        let calls = ec2.modify_calls.lock().unwrap().clone();
        assert_eq!(calls[0], ("vol-1".into(), 110, None, None));
        assert_eq!(calls[1], ("vol-1".into(), 120, Some(250), Some(4000)));
    }

    #[tokio::test]
    async fn modify_volume_wraps_errors() {
        let ec2 = FakeEc2::default();
        *ec2.modify_error.lock().unwrap() = Some(ApiError::new("RateExceeded"));
        let (c, ec2, _) = clients(ec2, FakeSsm::default());
        let err = c
            .modify_volume(
                "vol-1",
                ModifySpec {
                    size_gib: 110,
                    ..ModifySpec::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.message, "modify volume vol-1 to 110 GiB: RateExceeded");
    }

    #[tokio::test]
    async fn describe_last_modification_cases() {
        let ec2 = FakeEc2::default();
        let (c, ec2, _) = clients(ec2, FakeSsm::default());
        assert!(
            c.describe_last_modification("vol-1")
                .await
                .unwrap()
                .is_none()
        );

        let ec2 = FakeEc2::default();
        *ec2.modifications.lock().unwrap() = vec![fake::modification(
            VolumeModificationState::Completed,
            1_700_000_000,
            120,
        )];
        let (c, ec2, _) = clients(ec2, FakeSsm::default());
        let m = c
            .describe_last_modification("vol-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.state, "completed");
        assert_eq!(m.target_gib, 120);
        assert_eq!(m.start_time.unwrap().timestamp(), 1_700_000_000);

        let ec2 = FakeEc2::default();
        *ec2.modification_error.lock().unwrap() = Some(ApiError {
            code: Some("InvalidVolumeModification.NotFound".into()),
            message: "nope".into(),
        });
        let (c, ec2, _) = clients(ec2, FakeSsm::default());
        assert!(
            c.describe_last_modification("vol-1")
                .await
                .unwrap()
                .is_none()
        );

        let ec2 = FakeEc2::default();
        *ec2.modification_error.lock().unwrap() = Some(ApiError::new("boom"));
        let (c, ec2, _) = clients(ec2, FakeSsm::default());
        let err = c.describe_last_modification("vol-1").await.unwrap_err();
        assert!(
            err.message
                .contains("describe volume modifications vol-1: boom")
        );
    }

    #[tokio::test]
    async fn wait_for_modification_progresses_and_fails() {
        let ec2 = FakeEc2::default();
        *ec2.modification_sequence.lock().unwrap() = vec![
            vec![fake::modification(VolumeModificationState::Modifying, 0, 1)],
            vec![],
            vec![fake::modification(
                VolumeModificationState::Optimizing,
                0,
                1,
            )],
        ];
        let (c, ec2, _) = clients(ec2, FakeSsm::default());
        c.wait_for_modification("vol-1", Duration::from_secs(5))
            .await
            .unwrap();

        let ec2 = FakeEc2::default();
        *ec2.modifications.lock().unwrap() =
            vec![fake::modification(VolumeModificationState::Failed, 0, 1)];
        let (c, ec2, _) = clients(ec2, FakeSsm::default());
        let err = c
            .wait_for_modification("vol-1", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.message.contains("modification failed"));

        let ec2 = FakeEc2::default();
        *ec2.modifications.lock().unwrap() =
            vec![fake::modification(VolumeModificationState::Modifying, 0, 1)];
        let (c, ec2, _) = clients(ec2, FakeSsm::default());
        let err = c
            .wait_for_modification("vol-1", Duration::from_millis(5))
            .await
            .unwrap_err();
        assert!(
            err.message.contains("did not reach optimizing within 5ms"),
            "{err}"
        );

        let ec2 = FakeEc2::default();
        *ec2.modification_error.lock().unwrap() = Some(ApiError::new("boom"));
        let (c, ec2, _) = clients(ec2, FakeSsm::default());
        assert!(
            c.wait_for_modification("vol-1", Duration::from_secs(1))
                .await
                .is_err()
        );
    }

    #[test]
    fn to_chrono_converts() {
        let t = aws_smithy_types::DateTime::from_secs(42);
        assert_eq!(to_chrono(&t).unwrap().timestamp(), 42);
    }
}
