//! Shared domain types.

/// Ordered label set published on `ec2_metadata_instance_info`. Single source
/// of truth for both the metric encoder and the startup log.
pub const INFO_LABELS: [&str; 9] = [
    "instance_id",
    "name",
    "private_ip",
    "private_dns_name",
    "instance_type",
    "availability_zone",
    "state",
    "lifecycle",
    "architecture",
];

/// Instance Metadata Service configuration. `http_tokens` distinguishes
/// IMDSv2-only (`required`) from instances that still answer `IMDSv1`
/// (`optional`), and a hop limit of 1 stops containers from reaching IMDS at
/// all.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MetadataOptions {
    pub http_tokens: String,
    pub http_endpoint: String,
    /// `None` when EC2 omits the hop limit.
    pub hop_limit: Option<i32>,
}

impl MetadataOptions {
    /// Whether `IMDSv1` still answers on this instance. EC2 has no version
    /// field: `required` tokens mean `IMDSv2` only, `optional` means both
    /// versions answer, and a disabled endpoint answers neither.
    pub fn imdsv1_allowed(&self) -> bool {
        self.http_endpoint != "disabled" && self.http_tokens == "optional"
    }
}

/// Subset of EC2 instance data the exporter publishes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[allow(clippy::struct_field_names)]
pub struct Instance {
    pub id: String,
    pub name: String,
    pub private_ip: String,
    /// Private DNS name, which is also the Kubernetes node name on EKS
    /// clusters using the default (IP-based) naming.
    pub private_dns_name: String,
    pub instance_type: String,
    pub availability_zone: String,
    pub state: String,
    pub lifecycle: String,
    pub architecture: String,
    /// Unix seconds of the most recent launch. `None` when EC2 omits it.
    pub launch_time: Option<i64>,
    /// `None` when EC2 omits the metadata options block.
    pub metadata_options: Option<MetadataOptions>,
}

impl Instance {
    /// Label values in `INFO_LABELS` order.
    pub fn info_label_values(&self) -> [&str; 9] {
        [
            &self.id,
            &self.name,
            &self.private_ip,
            &self.private_dns_name,
            &self.instance_type,
            &self.availability_zone,
            &self.state,
            &self.lifecycle,
            &self.architecture,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(endpoint: &str, tokens: &str) -> MetadataOptions {
        MetadataOptions {
            http_tokens: tokens.into(),
            http_endpoint: endpoint.into(),
            hop_limit: Some(2),
        }
    }

    #[test]
    fn imdsv1_allowed_only_for_enabled_optional_endpoints() {
        assert!(opts("enabled", "optional").imdsv1_allowed());
        assert!(!opts("enabled", "required").imdsv1_allowed());
        assert!(!opts("disabled", "optional").imdsv1_allowed());
        assert!(!opts("disabled", "required").imdsv1_allowed());
        // EC2 omitted both fields, so nothing claims IMDSv1 is reachable.
        assert!(!MetadataOptions::default().imdsv1_allowed());
    }
}
