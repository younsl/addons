mod app;
mod aws;
mod collector;
mod config;
mod db;
mod error;
mod k8s;
mod logging;
mod procnet;
mod resp;
mod source;
mod state;
mod types;
mod ui;

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    app::run(config::Cli::parse()).await
}
