use anyhow::Result;
use prometheus_client::registry::Registry;
use std::sync::Arc;
use tracing::{error, info};

use trivy_collector::config::{Command, Config, Mode};
use trivy_collector::health::HealthServer;
use trivy_collector::metrics::Metrics;
use trivy_collector::{collector, logging, migrate, web};

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_args();

    if let Some(command) = config.command.clone() {
        return run_command(command, &config).await;
    }

    logging::init(&config.log_format, &config.log_level);

    info!(
        version = env!("CARGO_PKG_VERSION"),
        commit = env!("VERGEN_GIT_SHA"),
        build_date = env!("VERGEN_BUILD_TIMESTAMP"),
        rustc = env!("VERGEN_RUSTC_SEMVER"),
        mode = %config.mode,
        cluster = %config.cluster_name,
        "trivy-collector starting"
    );

    if let Err(e) = config.validate() {
        error!(error = %e, "Configuration validation failed");
        std::process::exit(1);
    }

    info!(mode = %config.mode, "Initializing Prometheus metrics registry");
    let mut registry = Registry::default();
    let metrics = Metrics::new(&mut registry, config.mode);
    let registry = Arc::new(registry);
    info!(
        mode = %config.mode,
        metrics_count = metrics.count(),
        "Prometheus metrics registered successfully"
    );

    let health_port = config.health_port;
    let health_server = HealthServer::new(registry);
    let health_server_clone = health_server.clone();

    info!(
        port = health_port,
        endpoint = "/metrics",
        "Starting Prometheus metrics endpoint"
    );
    let (health_ready_tx, health_ready_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        if let Err(e) = health_server_clone
            .serve(health_port, health_ready_tx)
            .await
        {
            error!(error = %e, "Health check server failed");
        }
    });

    health_ready_rx.await.ok();
    info!(
        port = health_port,
        endpoint = "/metrics",
        "Prometheus metrics endpoint is ready"
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let result = tokio::select! {
        result = run_mode(config, health_server, shutdown_rx, metrics) => result,
        _ = tokio::signal::ctrl_c() => {
            info!("Received shutdown signal");
            let _ = shutdown_tx.send(true);
            Ok(())
        }
    };

    if let Err(e) = result {
        error!(error = %e, "Application error");
        std::process::exit(1);
    }

    info!("Shutdown complete");
    Ok(())
}

/// One-shot subcommands, which run instead of a server and exit.
async fn run_command(command: Command, config: &Config) -> Result<()> {
    match command {
        Command::Version => {
            println!(
                "trivy-collector {}, commit: {}, build_date: {}",
                env!("CARGO_PKG_VERSION"),
                env!("VERGEN_GIT_SHA"),
                env!("VERGEN_BUILD_TIMESTAMP"),
            );
            Ok(())
        }
        Command::Crd => {
            println!("{}", trivy_collector::alerts::crd::definition_json()?);
            Ok(())
        }
        Command::ExportState {
            db_path,
            namespace,
            dry_run,
        } => {
            logging::init(&config.log_format, &config.log_level);
            migrate::export_state(migrate::ExportRequest {
                db_path,
                namespace,
                notes_configmap: config.notes_configmap.clone(),
                api_tokens_secret: config.api_tokens_secret.clone(),
                dry_run,
            })
            .await
        }
    }
}

async fn run_mode(
    config: Config,
    health_server: HealthServer,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
    metrics: Arc<Metrics>,
) -> Result<()> {
    match config.mode {
        Mode::Scraper => {
            info!(
                cluster = %config.get_cluster_name(),
                storage_path = %config.storage_path,
                hub_secret_namespace = %config.hub_secret_namespace,
                "Running in scraper mode (watchers + database owner)"
            );
            collector::run(config, health_server, shutdown_rx, metrics).await
        }
        Mode::Server => {
            info!(
                port = config.server_port,
                scraper_url = %config.scraper_url,
                "Running in server mode (UI/API only)"
            );
            web::run(config, health_server, shutdown_rx, metrics).await
        }
    }
}
