//! volume-autoscaler grows the root EBS volume and filesystem of EC2
//! instances (`InstanceVolumeAutoscaler`) and the `PersistentVolumeClaims`
//! of workloads (`PersistentVolumeClaimAutoscaler`) when usage crosses a
//! threshold.

// Nursery lints that only add noise here: every module but the entry points
// is private, so `pub(crate)` is never load-bearing, and the mutex guards in
// question are held for a few instructions.
#![allow(
    clippy::redundant_pub_crate,
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee
)]

mod alertmanager;
mod annotations;
pub mod autoscaler;
mod awsx;
pub mod cli;
pub mod config;
mod controller;
pub mod daemon;
mod grafana;
mod humanize;
mod k8s;
mod observability;
mod promql;
mod pvscan;
mod recstore;
mod scripts;
mod sinks;
mod throughput;
