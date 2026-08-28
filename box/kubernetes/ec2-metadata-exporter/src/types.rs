//! Shared domain types.

/// Ordered label set published on `ec2_metadata_instance_info`. Single source
/// of truth for both the metric encoder and the startup log.
pub const INFO_LABELS: [&str; 8] = [
    "instance_id",
    "name",
    "private_ip",
    "instance_type",
    "availability_zone",
    "state",
    "lifecycle",
    "architecture",
];

/// Subset of EC2 instance data the exporter publishes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[allow(clippy::struct_field_names)]
pub struct Instance {
    pub id: String,
    pub name: String,
    pub private_ip: String,
    pub instance_type: String,
    pub availability_zone: String,
    pub state: String,
    pub lifecycle: String,
    pub architecture: String,
    /// Unix seconds of the most recent launch. `None` when EC2 omits it.
    pub launch_time: Option<i64>,
}

impl Instance {
    /// Label values in `INFO_LABELS` order.
    pub fn info_label_values(&self) -> [&str; 8] {
        [
            &self.id,
            &self.name,
            &self.private_ip,
            &self.instance_type,
            &self.availability_zone,
            &self.state,
            &self.lifecycle,
            &self.architecture,
        ]
    }
}
