//! node-configuration-agent, the agent that `node-configuration-operator`
//! runs on every targeted node: applies each `NodeConfigurationBinding` of its
//! node to the host and reports the result.

mod app;
mod cli;
mod hostfs;
mod metrics;
mod reconcile;
mod rules;
mod script;

use node_configuration_common::boot::Build;

const BUILD: Build = Build {
    name: env!("CARGO_PKG_NAME"),
    version: env!("CARGO_PKG_VERSION"),
    commit: env!("BUILD_COMMIT"),
    build_date: env!("BUILD_DATE"),
    rustc_version: env!("BUILD_RUSTC_VERSION"),
};

#[tokio::main]
async fn main() {
    app::start().await;
}
