//! Owns AWS Site-to-Site VPN tunnel endpoint maintenance.
//!
//! AWS queues endpoint replacements and, past a published deadline, applies
//! them at a time of its choosing. This controller applies them earlier: in a
//! maintenance window, one tunnel at a time, after a Slack approval, and only
//! while the other tunnel is verifiably carrying traffic.

mod approval;
mod aws;
mod commands;
mod config;
mod controller;
mod daemon;
mod executor;
mod humanize;
mod k8s;
mod observability;
mod planner;
mod promx;
mod slack;
mod window;

use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing::error;

const LONG_ABOUT: &str = "Applies pending Site-to-Site VPN tunnel endpoint maintenance on your schedule instead of AWS's:\ninside a maintenance window, one tunnel at a time, after a Slack approval, and only while the\nconnection's other tunnel is verifiably carrying traffic.";

#[derive(Parser)]
#[command(
    name = "aws-vpn-maintenance-handler",
    about = "Own AWS Site-to-Site VPN tunnel endpoint maintenance",
    long_about = LONG_ABOUT,
    version = concat!(env!("CARGO_PKG_VERSION"), " (commit ", env!("BUILD_COMMIT"), ")")
)]
struct Cli {
    /// Path to the config file (defaults to `$CONFIG_FILE`, then
    /// /etc/aws-vpn-maintenance-handler/config.yaml)
    #[arg(long, global = true)]
    config: Option<String>,
    /// Verbose output (debug logging)
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Validate the config file and print the effective settings
    Validate,
    /// Print tunnel telemetry and pending maintenance for the managed VPN
    /// connections (read-only)
    Status,
}

type Subscriber = Box<dyn tracing::Subscriber + Send + Sync>;

/// Builds the tracing subscriber for a level and format.
fn subscriber(level: &str, format: &str) -> Subscriber {
    let filter = tracing_subscriber::EnvFilter::new(match level.to_ascii_lowercase().as_str() {
        "debug" => "debug",
        "warn" => "warn",
        "error" => "error",
        _ => "info",
    });
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);
    if format.eq_ignore_ascii_case("text") {
        Box::new(builder.finish())
    } else {
        Box::new(builder.json().flatten_event(true).finish())
    }
}

fn main() -> ExitCode {
    // Both `ring` (reqwest) and `aws-lc-rs` (AWS SDK, kube) are linked, so
    // rustls cannot pick a default provider on its own.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = Cli::parse();
    let env = config::Env::from_process();
    let path = config::resolve_path(cli.config.as_deref());
    // A failure before the config is read still has to look like every other
    // line in the Pod log, so it is reported through a default JSON logger
    // scoped to this thread. The configured logger is installed globally once
    // the level and format are known: the runtime's worker threads only see a
    // global subscriber, and the Socket Mode loop and the replacement worker
    // both log from them.
    let loaded = tracing::subscriber::with_default(subscriber("info", "json"), || {
        config::load(Path::new(&path), &env).map_err(|err| {
            error!(file = %path, error = %err, "configuration error");
        })
    });
    let Ok(cfg) = loaded else {
        return ExitCode::FAILURE;
    };
    let level = if cli.verbose { "debug" } else { &cfg.log_level };
    if tracing::subscriber::set_global_default(subscriber(level, &cfg.log_format)).is_err() {
        eprintln!("a global tracing subscriber was already installed");
        return ExitCode::FAILURE;
    }

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            error!(error = %err, "failed to start the async runtime");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async {
        match cli.command {
            Some(Command::Validate) => commands::validate(&cfg, &mut std::io::stdout()),
            Some(Command::Status) => {
                let client = aws::Client::new(&cfg.region).await;
                commands::status(&cfg, &client, &mut std::io::stdout()).await
            }
            None => daemon::run(cfg).await,
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(
                error = format!("{err:#}"),
                "aws-vpn-maintenance-handler failed"
            );
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_parses() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from(["x", "--config", "/tmp/c.yaml", "validate"]).unwrap();
        assert_eq!(cli.config.as_deref(), Some("/tmp/c.yaml"));
        assert!(matches!(cli.command, Some(Command::Validate)));
        let cli = Cli::try_parse_from(["x", "-v", "status"]).unwrap();
        assert!(cli.verbose);
        assert!(matches!(cli.command, Some(Command::Status)));
        let cli = Cli::try_parse_from(["x"]).unwrap();
        assert!(cli.command.is_none());
        let version = Cli::command().render_version();
        assert!(version.contains("(commit "), "{version}");
    }

    #[test]
    fn subscribers_build_for_both_formats() {
        for (level, format) in [
            ("debug", "text"),
            ("nonsense", "json"),
            ("warn", "JSON"),
            ("error", "text"),
        ] {
            let s = subscriber(level, format);
            tracing::subscriber::with_default(s, || tracing::info!("probe"));
        }
    }
}
