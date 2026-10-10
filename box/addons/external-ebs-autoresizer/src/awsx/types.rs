use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

/// A discovered EC2 instance and its root EBS volume.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Instance {
    pub id: String,
    pub name: String,
    pub tags: BTreeMap<String, String>,
    pub root_device_name: String,
    pub root_volume_id: String,
    pub root_volume_size_gib: i32,
}

/// An attached EBS volume and its performance configuration. Throughput and
/// IOPS are only configurable on gp3, io1, and io2; on other volume types EC2
/// reports throughput as 0 and the recommender treats them as out of scope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Volume {
    pub id: String,
    /// The EBS volume type (gp3, gp2, io2, ...).
    pub kind: String,
    /// The guest device name the volume is attached as (e.g. /dev/xvda).
    pub device: String,
    pub instance_id: String,
    pub size_gib: i32,
    /// The provisioned throughput in MiB/s.
    pub throughput_mibps: i32,
    pub iops: i32,
}

/// The EBS bandwidth an instance type can drive, independent of how much the
/// attached volumes provision. Both values are in MB/s (decimal), the unit
/// `DescribeInstanceTypes` reports; converting to the MiB/s unit gp3
/// throughput is configured in is the caller's job.
///
/// `baseline_mbps` is the rate the instance sustains indefinitely.
/// `maximum_mbps` is the burst rate, which on burstable-bandwidth instance
/// types is credit-limited. On non-burstable types the two are equal.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EbsCaps {
    pub baseline_mbps: f64,
    pub maximum_mbps: f64,
}

/// One `ModifyVolume` request. `size_gib` is always sent; the throughput and
/// IOPS fields are omitted from the request when zero, leaving those
/// dimensions of the volume untouched. Bundling them into the size call
/// matters because EC2 allows one modification per volume per 6 hours.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModifySpec {
    pub size_gib: i32,
    pub throughput_mibps: i32,
    pub iops: i32,
}

/// The most recent EBS volume modification.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VolumeModification {
    pub state: String,
    pub start_time: Option<DateTime<Utc>>,
    pub target_gib: i32,
}

/// The terminal outcome of an SSM `RunShellScript` invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandResult {
    pub status: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// A single EC2 tag key/value pair used to scope discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagFilter {
    pub key: String,
    pub value: String,
}
