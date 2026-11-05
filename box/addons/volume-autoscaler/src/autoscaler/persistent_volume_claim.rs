//! `PersistentVolumeClaimAutoscaler`: grows the `PersistentVolumeClaims` of a
//! `StatefulSet`, or one claim, when space or inode utilization reported by
//! `kubelet_volume_stats_*` crosses the target.

pub mod api;
pub mod cache;
pub mod crd;
pub mod metrics;
pub mod policy;
pub mod reconciler;
pub mod status;
