//! backstage-mcp exposes a Backstage instance to AI agents over the Model
//! Context Protocol. Every tool is a read: the catalog, search, TechDocs and
//! the in-house plugins are queried through the Backstage backend REST API
//! and nothing is ever written back.

mod backstage;
mod config;
mod html;
mod server;
mod tools;

use std::sync::Arc;

use anyhow::{Context as _, Result};
use clap::Parser;
use rmcp::ServiceExt as _;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::config::{Config, Transport};
use crate::server::{BackstageMcp, HttpOptions};

const BUILD_COMMIT: &str = env!("BUILD_COMMIT");
const BUILD_DATE: &str = env!("BUILD_DATE");
const RUSTC_VERSION: &str = env!("BUILD_RUSTC_VERSION");

/// Every setting is read from the environment; the flags only cover what a
/// shell needs to ask directly.
#[derive(Parser)]
#[command(name = "backstage-mcp", version, about)]
struct Cli {
    /// Enable debug logging regardless of `LOG_LEVEL`.
    #[arg(short, long)]
    verbose: bool,
    /// Serve over stdin and stdout instead of HTTP (same as `MCP_TRANSPORT=stdio`).
    #[arg(long)]
    stdio: bool,
    /// Print the registered tool names and exit.
    #[arg(long)]
    list_tools: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = match Config::load() {
        Ok(cfg) => cfg,
        Err(err) => {
            init_tracing("info", "json", true);
            error!(error = %err, "configuration error");
            std::process::exit(1);
        }
    };
    if cli.stdio {
        cfg.transport = Transport::Stdio;
    }
    // stdio mode owns stdout for the protocol, so logs always go to stderr.
    init_tracing(
        if cli.verbose { "debug" } else { &cfg.log_level },
        &cfg.log_format,
        true,
    );

    let client = Arc::new(backstage::Client::new(
        &cfg.backstage_url,
        &cfg.backstage_token,
        cfg.request_timeout,
        &format!("backstage-mcp/{}", env!("CARGO_PKG_VERSION")),
    )?);
    let handler = BackstageMcp::new(client, cfg.max_result_chars);

    if cli.list_tools {
        for name in handler.tool_names() {
            println!("{name}");
        }
        return Ok(());
    }

    info!(
        version = env!("CARGO_PKG_VERSION"),
        commit = BUILD_COMMIT,
        built = BUILD_DATE,
        rustc = RUSTC_VERSION,
        backstage_url = cfg.backstage_url,
        transport = cfg.transport.as_str(),
        tools = handler.tool_names().len(),
        "starting backstage-mcp"
    );
    if cfg.backstage_token.is_empty() {
        warn!(
            hint = "set BACKSTAGE_TOKEN to a backend.auth.externalAccess static token",
            "requests to Backstage are unauthenticated"
        );
    }

    let shutdown = CancellationToken::new();
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            wait_for_signal().await;
            info!("shutdown signal received");
            shutdown.cancel();
        }
    });

    match cfg.transport {
        Transport::Stdio => {
            let service = handler
                .serve(rmcp::transport::stdio())
                .await
                .context("stdio transport")?;
            tokio::select! {
                result = service.waiting() => {
                    result.context("stdio session")?;
                }
                () = shutdown.cancelled() => {}
            }
            Ok(())
        }
        Transport::Http => {
            if cfg.mcp_bearer_token.is_empty() {
                warn!(
                    hint = "set MCP_BEARER_TOKEN to require a bearer token",
                    "MCP endpoint authentication disabled"
                );
            }
            let options = HttpOptions {
                mcp_path: cfg.mcp_path.clone(),
                bearer_token: cfg.mcp_bearer_token.clone(),
            };
            let router = server::app(handler, &options, shutdown.clone());
            let listener = server::bind(cfg.listen_port).await?;
            info!(
                path = cfg.mcp_path,
                port = cfg.listen_port,
                "serving MCP over streamable HTTP"
            );
            server::serve(listener, router, shutdown).await
        }
    }
}

fn init_tracing(level: &str, format: &str, to_stderr: bool) {
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter);
    let writer = move || -> Box<dyn std::io::Write + Send> {
        if to_stderr {
            Box::new(std::io::stderr())
        } else {
            Box::new(std::io::stdout())
        }
    };
    if format == "text" {
        registry.with(fmt::layer().with_writer(writer)).init();
    } else {
        registry
            .with(fmt::layer().json().flatten_event(true).with_writer(writer))
            .init();
    }
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
