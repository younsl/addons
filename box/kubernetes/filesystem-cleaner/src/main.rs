//! filesystem-cleaner monitors disk usage of target paths and deletes matching
//! files when usage exceeds a threshold. It runs as a Kubernetes init container
//! (`once` mode) or sidecar (`interval` mode).

mod bytesize;
mod cleaner;
mod config;
mod disk;
mod matcher;
mod scanner;

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::cleaner::Cleaner;
use crate::config::{Args, Config, LogLevel};

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let config = match Config::resolve(args, |key| std::env::var(key).ok()) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    init_logging(config.log_level);

    info!(
        version = env!("CARGO_PKG_VERSION"),
        commit = env!("BUILD_COMMIT"),
        "Starting filesystem-cleaner"
    );
    info!(
        target_paths = ?config.target_paths,
        usage_threshold_percent = config.usage_threshold_percent,
        cleanup_mode = %config.cleanup_mode,
        include_patterns = ?config.include_patterns,
        exclude_patterns = ?config.exclude_patterns,
        dry_run = config.dry_run,
        log_level = %config.log_level,
        check_interval_minutes = config.check_interval_minutes,
        "Configuration loaded"
    );
    if config.dry_run {
        warn!("Running in DRY-RUN mode - no files will be deleted");
    }

    let shutdown = CancellationToken::new();
    let cleaner = match Cleaner::new(config, shutdown.clone()) {
        Ok(cleaner) => cleaner,
        Err(e) => {
            error!(error = %e, "Failed to create cleaner");
            return ExitCode::from(1);
        }
    };

    tokio::spawn(async move {
        wait_for_signal().await;
        info!("Received shutdown signal, stopping cleaner");
        shutdown.cancel();
    });

    cleaner.run().await;
    ExitCode::SUCCESS
}

fn init_logging(level: LogLevel) {
    tracing_subscriber::fmt()
        .with_max_level(level.as_tracing())
        .with_target(false)
        .with_ansi(std::io::stdout().is_terminal())
        .init();
}

async fn wait_for_signal() {
    let ctrl_c = async {
        if let Err(e) = signal::ctrl_c().await {
            error!(error = %e, "Failed to listen for SIGINT");
            std::future::pending::<()>().await;
        }
    };
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                error!(error = %e, "Failed to listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
