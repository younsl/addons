//! Blocks an Argo CD Application sync until the same application has been
//! promoted in the upstream environment.

mod admission;
mod app;
mod argocd;
mod cli;
mod config;
mod engine;
mod events;
mod extension;
mod gate;
mod observability;
mod servingcert;
mod uiextension;

use clap::Parser;
use tokio::signal;
use tokio::sync::watch;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::cli::{Cli, Command};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // One subcommand. It exists because argocd-server loads extension scripts
    // from its own filesystem, so an init container has to place the embedded
    // script there.
    if let Some(Command::InstallExtension {
        dest,
        extension_name,
    }) = cli.command
    {
        let path = uiextension::install(&dest, &extension_name).map_err(|err| {
            eprintln!("install-extension: {err}");
            anyhow::anyhow!("install-extension failed")
        })?;
        println!(
            "wrote {} for extension name {extension_name}",
            path.display()
        );
        return Ok(());
    }

    if cli.version {
        print!("{}", cli::version_banner());
        return Ok(());
    }

    init_tracing(&cli.log_level, &cli.log_format);
    // Both the webhook listener and the argocd-server client run on rustls, so
    // the crypto provider is pinned once here.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        wait_for_signal().await;
        let _ = shutdown_tx.send(true);
    });

    if let Err(err) = app::run(cli, shutdown_rx).await {
        tracing::error!(error = %err, "fatal");
        std::process::exit(1);
    }
    Ok(())
}

fn init_tracing(level: &str, format: &str) {
    let level = match level.to_ascii_lowercase().as_str() {
        "debug" => "debug",
        "warn" => "warn",
        "error" => "error",
        _ => "info",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let registry = tracing_subscriber::registry().with(filter);
    if format.eq_ignore_ascii_case("text") {
        registry.with(fmt::layer().with_target(false)).init();
    } else {
        registry
            .with(fmt::layer().json().flatten_event(true).with_target(false))
            .init();
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
