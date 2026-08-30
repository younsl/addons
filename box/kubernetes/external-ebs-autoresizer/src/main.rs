//! external-ebs-autoresizer continuously watches tagged standalone EC2
//! instances and grows their root EBS volume and filesystem (ext2/3/4 or
//! XFS) when usage crosses a threshold.

// Nursery lints that only add noise in a binary crate: every module is
// private, so `pub(crate)` is never load-bearing, and the mutex guards in
// question are held for a few instructions.
#![allow(
    clippy::redundant_pub_crate,
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee
)]

mod alertmanager;
mod annotations;
mod awsx;
mod cli;
mod config;
mod controller;
mod daemon;
mod grafana;
mod humanize;
mod k8s;
mod observability;
mod policy;
mod promql;
mod pvscan;
mod recstore;
mod resizer;
mod scripts;
mod sinks;
mod throughput;

use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing::error;

#[derive(Parser)]
#[command(
    name = "external-ebs-autoresizer",
    about = "Grow standalone EC2 root volumes when disk usage crosses a threshold",
    version = concat!(env!("CARGO_PKG_VERSION"), " (commit ", env!("BUILD_COMMIT"), ")")
)]
struct Cli {
    /// Path to the config file (`$CONFIG_FILE`, else the mounted default)
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
    /// Run the controller (the default when no subcommand is given)
    Run,
    /// Load and validate the config file, then exit
    Validate,
    /// Print the resolved resize policies and their effective settings
    Policies {
        /// Discover instances via AWS and add a MATCHED column with the count
        /// each policy identifies
        #[arg(long)]
        count: bool,
    },
    /// List discovered instances grouped by the policy each matches (calls AWS)
    Instances,
    /// List unused `PersistentVolumeClaims` and `PersistentVolumes` (reads the
    /// Kubernetes API, writes nothing)
    Unused {
        /// Include objects that have not yet been unused for
        /// unusedVolumeScan.minUnusedAge
        #[arg(long)]
        all: bool,
    },
}

type Subscriber = Box<dyn tracing::Subscriber + Send + Sync>;

/// Builds the tracing subscriber for a level and format. Every log line goes
/// through it, including the Kubernetes client's, so a lease renewal failure
/// lands in the same JSON format as the addon's own lines.
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

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("failed to start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    let result = match cli.command {
        Some(Command::Validate) => {
            cli::run_validate(Path::new(&path), &env, &mut std::io::stdout())
        }
        Some(Command::Policies { count }) => runtime.block_on(cli::run_policies(
            Path::new(&path),
            &env,
            count,
            &mut std::io::stdout(),
        )),
        Some(Command::Instances) => runtime.block_on(cli::run_instances(
            Path::new(&path),
            &env,
            &mut std::io::stdout(),
        )),
        Some(Command::Unused { all }) => runtime.block_on(cli::run_unused(
            Path::new(&path),
            &env,
            all,
            &mut std::io::stdout(),
        )),
        Some(Command::Run) | None => {
            // A failure before the config is read still has to look like
            // every other line in the Pod log, so it is reported through a
            // default JSON logger. The configured logger is installed
            // globally once the level and format are known.
            let loaded = tracing::subscriber::with_default(subscriber("info", "json"), || {
                config::load(Path::new(&path), &env).map_err(|err| {
                    error!(file = %path, error = %err, "configuration error");
                    anyhow::Error::new(err)
                })
            });
            let Ok(cfg) = loaded else {
                return ExitCode::FAILURE;
            };
            let level = if cli.verbose { "debug" } else { &cfg.log_level };
            if tracing::subscriber::set_global_default(subscriber(level, &cfg.log_format)).is_err()
            {
                eprintln!("a global tracing subscriber was already installed");
                return ExitCode::FAILURE;
            }
            runtime.block_on(daemon::run(cfg))
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err:#}");
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
        let cli = Cli::try_parse_from(["x", "policies", "--count"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Policies { count: true })
        ));
        let cli = Cli::try_parse_from(["x", "unused", "--all", "--config", "c.yaml"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Unused { all: true })));
        assert_eq!(cli.config.as_deref(), Some("c.yaml"));
        let cli = Cli::try_parse_from(["x", "instances"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Instances)));
        let cli = Cli::try_parse_from(["x", "-v", "run"]).unwrap();
        assert!(cli.verbose);
        assert!(matches!(cli.command, Some(Command::Run)));
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
