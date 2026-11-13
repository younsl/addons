//! The process plumbing of both binaries: the shared flags, logging, the
//! shutdown signal, and the health and metrics server around the work loop.

use std::future::Future;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use clap::Args;
use kube::Client;
use prometheus_client::registry::Registry;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::telemetry::server::{self, Health};

/// Flags the controller and the agent share, each backed by an environment
/// variable so the Helm chart configures the binary through `env`.
#[derive(Args, Debug, Clone)]
pub struct CommonArgs {
    /// Log level (`trace`, `debug`, `info`, `warn`, `error`). `RUST_LOG` overrides it.
    #[arg(long, env = "LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    /// Log format (`json` or `text`).
    #[arg(long, env = "LOG_FORMAT", default_value = "json")]
    pub log_format: String,

    /// Port of the health and metrics server.
    #[arg(long, env = "HTTP_PORT", default_value_t = 8080)]
    pub http_port: u16,
}

/// What a binary knows about its own build.
#[derive(Clone, Copy, Debug)]
pub struct Build {
    pub name: &'static str,
    pub version: &'static str,
    pub commit: &'static str,
    pub build_date: &'static str,
    pub rustc_version: &'static str,
}

impl Build {
    #[doc(hidden)]
    pub const TEST: Self = Self {
        name: "test",
        version: "0.0.0",
        commit: "test",
        build_date: "1970-01-01",
        rustc_version: "0.0.0",
    };
}

/// Runs `work` until it ends or a shutdown signal arrives, and exits the
/// process with 1 when it fails. `work` registers its metrics before the
/// server starts and returns the loop to run.
pub async fn start<F, Fut>(build: &Build, common: &CommonArgs, work: F)
where
    F: FnOnce(Client, &mut Registry, CancellationToken) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    init_tracing(&common.log_level, &common.log_format);
    let name = build.name;
    info!(
        version = build.version,
        commit = build.commit,
        build_date = build.build_date,
        rustc_version = build.rustc_version,
        arch = std::env::consts::ARCH,
        "Starting {name}"
    );

    let shutdown = CancellationToken::new();
    let signal_token = shutdown.clone();
    tokio::spawn(async move {
        wait_for_signal().await;
        info!("Shutdown signal received, stopping");
        signal_token.cancel();
    });

    if let Err(e) = run(common.http_port, work, shutdown).await {
        error!("{name} failed, {e:#}");
        std::process::exit(1);
    }
    info!("{name} stopped");
}

fn init_tracing(level: &str, format: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level(level)));
    let registry = tracing_subscriber::registry().with(filter);
    if format.eq_ignore_ascii_case("text") {
        registry.with(fmt::layer().with_target(true)).init();
    } else {
        registry.with(fmt::layer().json().with_target(true)).init();
    }
}

fn log_level(level: &str) -> &'static str {
    match level.to_ascii_lowercase().as_str() {
        "trace" => "trace",
        "debug" => "debug",
        "warn" => "warn",
        "error" => "error",
        _ => "info",
    }
}

async fn wait_for_signal() {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

async fn run<F, Fut>(http_port: u16, work: F, shutdown: CancellationToken) -> Result<()>
where
    F: FnOnce(Client, &mut Registry, CancellationToken) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let client = Client::try_default()
        .await
        .context("build the Kubernetes client")?;

    let mut registry = Registry::default();
    let work = work(client, &mut registry, shutdown.clone());
    let health = Health::default();
    let http = tokio::spawn(server::serve(
        http_port,
        health.clone(),
        Arc::new(registry),
        shutdown.clone(),
    ));

    health.set_ready(true);
    work.await?;
    health.set_ready(false);

    shutdown.cancel();
    http.await
        .context("join the HTTP server")?
        .context("serve HTTP")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_log_levels_fall_back_to_info() {
        assert_eq!(log_level("DEBUG"), "debug");
        assert_eq!(log_level("trace"), "trace");
        assert_eq!(log_level("warn"), "warn");
        assert_eq!(log_level("error"), "error");
        assert_eq!(log_level("verbose"), "info");
    }
}
