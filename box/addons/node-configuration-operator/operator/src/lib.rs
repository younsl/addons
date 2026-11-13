//! node-configuration-operator, the controller: rolls each `NodeConfiguration`
//! out to the nodes it targets and creates the agent `DaemonSet` that
//! `node-configuration-agent` runs in.

mod app;
pub mod chart;
mod cli;
mod controller;
mod metrics;

pub use app::start;

use node_configuration_common::boot::Build;

pub const BUILD: Build = Build {
    name: env!("CARGO_PKG_NAME"),
    version: env!("CARGO_PKG_VERSION"),
    commit: env!("BUILD_COMMIT"),
    build_date: env!("BUILD_DATE"),
    rustc_version: env!("BUILD_RUSTC_VERSION"),
};
