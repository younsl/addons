//! volume-autoscaler grows the root EBS volume and filesystem of EC2
//! instances (`InstanceVolumeAutoscaler`) and the `PersistentVolumeClaims`
//! of workloads (`PersistentVolumeClaimAutoscaler`) when usage crosses a
//! threshold.

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
mod autoscaler;
mod awsx;
mod cli;
mod config;
mod controller;
mod daemon;
mod grafana;
mod humanize;
mod k8s;
mod observability;
mod promql;
mod pvscan;
mod recstore;
mod scripts;
mod sinks;
mod throughput;

use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing::error;

#[derive(Parser)]
#[command(
    name = "volume-autoscaler",
    about = "Grow EC2 root volumes and PersistentVolumeClaims when usage crosses a threshold",
    version = concat!(
        env!("CARGO_PKG_VERSION"),
        " (commit ",
        env!("BUILD_COMMIT"),
        ", rustc ",
        env!("BUILD_RUSTC_VERSION"),
        ")"
    )
)]
struct Cli {
    /// Path to the config file (else the mounted default)
    #[arg(long, env = "CONFIG_FILE", global = true)]
    config: Option<String>,
    /// Verbose output (debug logging)
    #[arg(short, long, env = "VERBOSE", global = true)]
    verbose: bool,
    /// Controllers to run, comma separated
    #[arg(
        long,
        env = "CONTROLLERS",
        value_enum,
        value_delimiter = ',',
        default_value = "instance-volume-autoscaler,persistent-volume-claim-autoscaler,unused-volume-scan",
        global = true
    )]
    controllers: Vec<daemon::ControllerName>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the controller (the default when no subcommand is given)
    Run,
    /// Load and validate the config file, then exit
    Validate,
    #[command(
        about = "Print the InstanceVolumeAutoscalers and their effective settings (reads the Kubernetes API)"
    )]
    Autoscalers {
        /// Discover instances via AWS and add a MATCHED column with the count
        /// each autoscaler targets
        #[arg(long, env = "AUTOSCALERS_COUNT")]
        count: bool,
    },
    /// List discovered instances with the autoscaler that targets each (calls
    /// AWS and the Kubernetes API)
    Instances,
    #[command(about = "Print a CustomResourceDefinition as JSON")]
    Crd {
        /// The custom resource to print
        #[arg(long, value_enum, default_value = "instance-volume-autoscaler")]
        kind: cli::CrdKind,
    },
    /// List unused `PersistentVolumeClaims` and `PersistentVolumes` (reads the
    /// Kubernetes API, writes nothing)
    Unused {
        /// Include objects that have not yet been unused for
        /// unusedVolumeScan.minUnusedAge
        #[arg(long, env = "UNUSED_ALL")]
        all: bool,
    },
}

type Subscriber = Box<dyn tracing::Subscriber + Send + Sync>;

const WORKER_THREADS: usize = 2;

/// Builds the tracing subscriber for a level and format. Every log line goes
/// through it, including the Kubernetes client's, so a lease renewal failure
/// lands in the same JSON format as the addon's own lines.
fn subscriber(level: &str, format: &str) -> Subscriber {
    let level = match level.to_ascii_lowercase().as_str() {
        "debug" => "debug",
        "warn" => "warn",
        "error" => "error",
        _ => "info",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
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
    // Installed explicitly so every rustls client shares aws-lc-rs even if a
    // dependency later links a second provider.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = Cli::parse();
    let env = config::Env::from_process();
    let path = config::resolve_path(cli.config.as_deref());

    // Every controller waits on the network, so two workers keep up while a
    // worker per node core only adds threads and allocator arenas.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS)
        .enable_all()
        .build()
    {
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
        Some(Command::Crd { kind }) => cli::run_crd(kind, &mut std::io::stdout()),
        Some(Command::Autoscalers { count }) => runtime.block_on(cli::run_autoscalers(
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
            runtime.block_on(daemon::run(cfg, daemon::Controllers::of(&cli.controllers)))
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
        let cli = Cli::try_parse_from(["x", "autoscalers", "--count"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Autoscalers { count: true })
        ));
        let cli = Cli::try_parse_from(["x", "crd"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Crd {
                kind: cli::CrdKind::InstanceVolumeAutoscaler
            })
        ));
        let cli = Cli::try_parse_from(["x", "crd", "--kind", "persistent-volume-claim-autoscaler"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Crd {
                kind: cli::CrdKind::PersistentVolumeClaimAutoscaler
            })
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
        assert_eq!(cli.controllers.len(), 3, "every controller by default");
        let cli = Cli::try_parse_from(["x", "--controllers", "persistent-volume-claim-autoscaler"])
            .unwrap();
        let c = daemon::Controllers::of(&cli.controllers);
        assert!(c.persistent_volume_claim_autoscaler);
        assert!(!c.instance_volume_autoscaler && !c.unused_volume_scan);
        assert!(Cli::try_parse_from(["x", "--controllers", "pvca"]).is_err());
        let version = Cli::command().render_version();
        assert!(version.contains("(commit "), "{version}");
        assert!(version.contains(", rustc "), "{version}");
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
