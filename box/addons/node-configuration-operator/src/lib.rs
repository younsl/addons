//! node-configuration-operator keeps the OS configuration of EKS managed
//! node group nodes in line with `NodeConfiguration` resources.
//!
//! The controller turns each `NodeConfiguration` resource into a `DaemonSet`, and the
//! agent in every pod keeps the host files in line with the resource's rules.

mod agent;
mod app;
mod cli;
mod controller;
pub mod crd;
mod payload;
mod telemetry;

pub use app::start;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const COMMIT: &str = env!("BUILD_COMMIT");
pub const BUILD_DATE: &str = env!("BUILD_DATE");
pub const RUSTC_VERSION: &str = env!("BUILD_RUSTC_VERSION");
