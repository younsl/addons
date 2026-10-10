//! Server that keeps dotfiles symlinked into a home directory. It reconciles
//! the links declared in a spec file on an interval or on demand, and moves
//! any real file in the way into a timestamped backup directory.

mod api;
mod app;
mod config;
mod error;
mod linker;
mod observability;
mod reconciler;
mod spec;

use tokio::signal;
use tokio::sync::watch;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::config::{Config, LogFormat};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load();
    init_tracing(&cfg);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        wait_for_signal().await;
        let _ = shutdown_tx.send(true);
    });

    app::run(cfg, shutdown_rx).await
}

fn init_tracing(cfg: &Config) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(cfg.log_level.as_str()));
    let registry = tracing_subscriber::registry().with(filter);
    match cfg.log_format {
        LogFormat::Json => registry
            .with(fmt::layer().json().flatten_event(true))
            .init(),
        LogFormat::Text => registry.with(fmt::layer().with_target(false)).init(),
    }
}

async fn wait_for_signal() {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };
    let terminate = async {
        if let Ok(mut sig) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
            sig.recv().await;
        }
    };

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
