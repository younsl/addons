//! `InstanceVolumeAutoscaler`: grows the root EBS volume and
//! filesystem of the EC2 instances an autoscaler targets, EKS nodes included,
//! through the EC2 API and SSM Run Command.

pub mod api;
pub mod crd;
pub mod policy;
pub mod resizer;
