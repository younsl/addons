//! kuo - Kubernetes Upgrade Operator for EKS clusters.
//!
//! Watches `EKSUpgrade` CRD resources and performs declarative EKS cluster upgrades
//! with sequential control plane upgrades, add-on updates, and managed node group
//! rolling updates.

mod annotate;
mod app;
mod aws;
mod cli;
mod controller;
pub mod crd;
mod eks;
mod error;
mod k8s;
mod mcp;
mod notify;
mod phases;
mod render;
mod status;
mod telemetry;

pub use app::start;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const COMMIT: &str = env!("BUILD_COMMIT");
pub const BUILD_DATE: &str = env!("BUILD_DATE");
pub const RUSTC_VERSION: &str = env!("BUILD_RUSTC_VERSION");
pub const ARCH: &str = env!("BUILD_ARCH");

fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
