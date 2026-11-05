//! The read-only EBS and instance-type lookups the throughput recommender
//! needs. Nothing here mutates AWS state: the recommender only ever publishes
//! a recommendation, so it must not be able to change a volume even by
//! accident.

use std::collections::HashMap;

use aws_sdk_ec2::types::{Filter, InstanceType, InstanceTypeInfo, Volume as SdkVolume};

use super::{ApiError, Clients, EbsCaps, Volume};

/// Bounds how many instance IDs go into a single `DescribeVolumes` filter.
/// EC2 caps filter values per request, and a long value list also risks
/// exceeding the request size limit.
const FILTER_BATCH: usize = 100;
/// The `DescribeInstanceTypes` per-request limit on the `InstanceTypes`
/// parameter.
const INSTANCE_TYPE_BATCH: usize = 100;

impl Clients {
    /// Returns the attached EBS volumes of each given instance, keyed by
    /// instance ID. Instances with no attached EBS volume are absent from the
    /// result. An empty `instance_ids` returns an empty map without calling
    /// AWS.
    pub async fn describe_attached_volumes(
        &self,
        instance_ids: &[String],
    ) -> Result<HashMap<String, Vec<Volume>>, ApiError> {
        let mut out: HashMap<String, Vec<Volume>> = HashMap::new();
        for chunk in instance_ids.chunks(FILTER_BATCH) {
            let filters = vec![
                Filter::builder()
                    .name("attachment.instance-id")
                    .set_values(Some(chunk.to_vec()))
                    .build(),
                Filter::builder()
                    .name("attachment.status")
                    .values("attached")
                    .build(),
            ];
            let mut token: Option<String> = None;
            loop {
                let page = self
                    .ec2
                    .describe_volumes(Vec::new(), filters.clone(), token.take())
                    .await
                    .map_err(|e| ApiError {
                        code: e.code,
                        message: format!("describe attached volumes: {}", e.message),
                    })?;
                for v in page.volumes() {
                    for vol in new_volumes(v) {
                        out.entry(vol.instance_id.clone()).or_default().push(vol);
                    }
                }
                token = page.next_token().map(str::to_string);
                if token.is_none() {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Returns the EBS bandwidth caps of each given instance type, keyed by
    /// instance type name. Results are cached for the process lifetime:
    /// instance type capabilities are static AWS catalog data, so a cluster
    /// with a handful of instance types settles into zero API calls after the
    /// first pass. Types AWS does not report EBS-optimized info for are absent
    /// from the result.
    pub async fn describe_instance_type_ebs_caps(
        &self,
        instance_types: &[String],
    ) -> Result<HashMap<String, EbsCaps>, ApiError> {
        let mut out = HashMap::new();
        let missing = self.cached_ebs_caps(instance_types, &mut out);
        if missing.is_empty() {
            return Ok(out);
        }

        let mut fetched: HashMap<String, EbsCaps> = HashMap::new();
        for chunk in missing.chunks(INSTANCE_TYPE_BATCH) {
            let types: Vec<InstanceType> = chunk
                .iter()
                .map(|t| InstanceType::from(t.as_str()))
                .collect();
            let mut token: Option<String> = None;
            loop {
                let page = self
                    .ec2
                    .describe_instance_types(types.clone(), token.take())
                    .await
                    .map_err(|e| ApiError {
                        code: e.code,
                        message: format!("describe instance types: {}", e.message),
                    })?;
                for it in page.instance_types() {
                    if let Some(caps) = new_ebs_caps(it) {
                        fetched.insert(
                            it.instance_type()
                                .map(|t| t.as_str().to_string())
                                .unwrap_or_default(),
                            caps,
                        );
                    }
                }
                token = page.next_token().map(str::to_string);
                if token.is_none() {
                    break;
                }
            }
        }

        self.store_ebs_caps(&fetched);
        out.extend(fetched);
        Ok(out)
    }

    /// Copies every already-cached entry into `dst` and returns the instance
    /// types still to fetch, deduplicated.
    fn cached_ebs_caps(
        &self,
        instance_types: &[String],
        dst: &mut HashMap<String, EbsCaps>,
    ) -> Vec<String> {
        let cache = self
            .ebs_caps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut missing = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for t in instance_types {
            if t.is_empty() || !seen.insert(t.as_str()) {
                continue;
            }
            match cache.get(t) {
                Some(caps) => {
                    dst.insert(t.clone(), *caps);
                }
                None => missing.push(t.clone()),
            }
        }
        missing
    }

    fn store_ebs_caps(&self, caps: &HashMap<String, EbsCaps>) {
        let mut cache = self
            .ebs_caps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (k, v) in caps {
            cache.insert(k.clone(), *v);
        }
    }
}

/// Flattens one EC2 volume into one `Volume` per attachment. A volume is
/// normally attached to a single instance; Multi-Attach io2 volumes report
/// several attachments and yield one entry each, so every owning instance
/// sees it.
fn new_volumes(v: &SdkVolume) -> Vec<Volume> {
    let base = Volume {
        id: v.volume_id().unwrap_or_default().to_string(),
        kind: v
            .volume_type()
            .map(|t| t.as_str().to_string())
            .unwrap_or_default(),
        size_gib: v.size().unwrap_or(0),
        throughput_mibps: v.throughput().unwrap_or(0),
        iops: v.iops().unwrap_or(0),
        ..Volume::default()
    };
    v.attachments()
        .iter()
        .map(|att| Volume {
            device: att.device().unwrap_or_default().to_string(),
            instance_id: att.instance_id().unwrap_or_default().to_string(),
            ..base.clone()
        })
        .collect()
}

/// Extracts the EBS bandwidth caps from one instance type. It reports `None`
/// when AWS publishes no EBS-optimized bandwidth for the type, which is the
/// case for older non-EBS-optimizable families.
fn new_ebs_caps(it: &InstanceTypeInfo) -> Option<EbsCaps> {
    let info = it.ebs_info()?.ebs_optimized_info()?;
    let mut caps = EbsCaps {
        baseline_mbps: info.baseline_throughput_in_m_bps().unwrap_or(0.0),
        maximum_mbps: info.maximum_throughput_in_m_bps().unwrap_or(0.0),
    };
    if caps.maximum_mbps <= 0.0 {
        return None;
    }
    // A non-burstable type reports only the maximum; treat it as the
    // sustainable rate too, so callers never see a zero baseline they have to
    // special-case.
    if caps.baseline_mbps <= 0.0 {
        caps.baseline_mbps = caps.maximum_mbps;
    }
    Some(caps)
}

#[cfg(test)]
#[allow(unused_variables)]
mod tests {
    use aws_sdk_ec2::types::VolumeType;

    use super::super::fake::{self, FakeEc2, FakeSsm, clients};
    use super::*;

    #[tokio::test]
    async fn attached_volumes_keyed_by_instance_with_multi_attach() {
        let ec2 = FakeEc2::default();
        *ec2.volumes.lock().unwrap() = vec![
            fake::attached_volume(
                "vol-a",
                VolumeType::Gp3,
                100,
                125,
                3000,
                &[("i-1", "/dev/xvda")],
            ),
            fake::attached_volume(
                "vol-b",
                VolumeType::Io2,
                50,
                0,
                10000,
                &[("i-1", "/dev/xvdf"), ("i-2", "/dev/xvdf")],
            ),
            fake::attached_volume("vol-c", VolumeType::Gp2, 10, 0, 0, &[("i-9", "/dev/xvda")]),
        ];
        let (c, ec2, _) = clients(ec2, FakeSsm::default());
        let got = c
            .describe_attached_volumes(&["i-1".to_string(), "i-2".to_string(), "i-3".to_string()])
            .await
            .unwrap();
        assert_eq!(got["i-1"].len(), 2);
        assert_eq!(got["i-1"][0].id, "vol-a");
        assert_eq!(got["i-1"][0].kind, "gp3");
        assert_eq!(got["i-1"][0].device, "/dev/xvda");
        assert_eq!(got["i-1"][0].throughput_mibps, 125);
        assert_eq!(got["i-1"][0].iops, 3000);
        assert_eq!(got["i-1"][0].size_gib, 100);
        assert_eq!(
            got["i-2"].len(),
            1,
            "multi-attach yields one entry per owner"
        );
        assert!(!got.contains_key("i-3"), "no volume, absent");
        assert!(!got.contains_key("i-9"));
    }

    #[tokio::test]
    async fn attached_volumes_chunks_and_skips_empty() {
        let ec2 = FakeEc2::default();
        let (c, ec2, _) = clients(ec2, FakeSsm::default());
        assert!(c.describe_attached_volumes(&[]).await.unwrap().is_empty());
        assert!(
            ec2.describe_volume_calls.lock().unwrap().is_empty(),
            "no call for an empty set"
        );
        let ids: Vec<String> = (0..250).map(|i| format!("i-{i}")).collect();
        c.describe_attached_volumes(&ids).await.unwrap();
        assert_eq!(
            ec2.describe_volume_calls.lock().unwrap().len(),
            3,
            "100 + 100 + 50"
        );
    }

    #[tokio::test]
    async fn attached_volumes_error() {
        let ec2 = FakeEc2::default();
        *ec2.describe_error.lock().unwrap() = Some(ApiError::new("denied"));
        let (c, ec2, _) = clients(ec2, FakeSsm::default());
        let err = c
            .describe_attached_volumes(&["i-1".to_string()])
            .await
            .unwrap_err();
        assert!(err.message.contains("describe attached volumes: denied"));
    }

    #[tokio::test]
    async fn instance_type_caps_cache_and_skip() {
        let ec2 = FakeEc2::default();
        *ec2.instance_types.lock().unwrap() = vec![
            fake::instance_type("m5.large", Some(81.25), Some(593.75)),
            fake::instance_type("c5.xlarge", None, Some(1187.5)),
            fake::instance_type("t2.micro", None, None),
        ];
        let (c, ec2, _) = clients(ec2, FakeSsm::default());
        let got = c
            .describe_instance_type_ebs_caps(&[
                "m5.large".into(),
                "m5.large".into(),
                String::new(),
                "c5.xlarge".into(),
                "t2.micro".into(),
                "unknown.type".into(),
            ])
            .await
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(
            got["m5.large"],
            EbsCaps {
                baseline_mbps: 81.25,
                maximum_mbps: 593.75
            }
        );
        assert_eq!(
            got["c5.xlarge"],
            EbsCaps {
                baseline_mbps: 1187.5,
                maximum_mbps: 1187.5
            },
            "baseline falls back to maximum"
        );
        assert_eq!(
            ec2.describe_type_calls.lock().unwrap()[0],
            vec!["m5.large", "c5.xlarge", "t2.micro", "unknown.type"],
            "deduplicated, empty skipped"
        );

        // Second call: cached types are not fetched again.
        let got = c
            .describe_instance_type_ebs_caps(&["m5.large".into(), "c5.xlarge".into()])
            .await
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(
            ec2.describe_type_calls.lock().unwrap().len(),
            1,
            "served from cache"
        );

        assert!(
            c.describe_instance_type_ebs_caps(&[])
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(ec2.describe_type_calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn instance_type_caps_error() {
        let ec2 = FakeEc2::default();
        *ec2.describe_error.lock().unwrap() = Some(ApiError::new("denied"));
        let (c, ec2, _) = clients(ec2, FakeSsm::default());
        let err = c
            .describe_instance_type_ebs_caps(&["m5.large".into()])
            .await
            .unwrap_err();
        assert!(err.message.contains("describe instance types: denied"));
    }

    #[test]
    fn new_ebs_caps_without_info() {
        let it = InstanceTypeInfo::builder().build();
        assert!(new_ebs_caps(&it).is_none());
    }
}
