//! harbor-arm64 builds every image the harbor-helm chart deploys for
//! linux/arm64 from the upstream Harbor source, driving its own Makefile on a
//! native arm64 Docker host. Upstream publishes amd64-only images before 2.16.0.

mod config;
mod elf;
mod error;
mod images;
mod pipeline;
mod runner;

use std::process::ExitCode;

use clap::Parser;
use tracing::{error, info, warn};
use tracing_subscriber::filter::LevelFilter;

use crate::config::{BuildConfig, Cli, Command, HarborVersion};
use crate::images::{IMAGES, values_override};
use crate::runner::{DryRunner, Runner, SystemRunner};

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    match cli.command {
        Command::Images => {
            for image in IMAGES {
                println!("{}", image.name);
            }
            ExitCode::SUCCESS
        }
        Command::Values { registry } => match HarborVersion::default_version() {
            Ok(version) => {
                print!("{}", values_override(&registry, version.as_str()));
                ExitCode::SUCCESS
            }
            Err(e) => {
                error!(error = %e, "Invalid VERSION file");
                ExitCode::from(2)
            }
        },
        Command::HarborVersion => match HarborVersion::default_version() {
            Ok(version) => {
                println!("{version}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                error!(error = %e, "Invalid VERSION file");
                ExitCode::from(2)
            }
        },
        Command::Build(args) => {
            let cfg = match BuildConfig::resolve(args) {
                Ok(cfg) => cfg,
                Err(e) => {
                    error!(error = %e, "Invalid configuration");
                    return ExitCode::from(2);
                }
            };
            info!(
                tool_version = env!("CARGO_PKG_VERSION"),
                commit = env!("BUILD_COMMIT"),
                harbor_version = %cfg.version,
                registry = %cfg.registry,
                workdir = %cfg.workdir.display(),
                push = cfg.push,
                "Starting Harbor arm64 build"
            );
            if cfg.dry_run {
                warn!("DRY-RUN mode, commands are logged and not executed");
            }
            let runner: &dyn Runner = if cfg.dry_run {
                &DryRunner
            } else {
                &SystemRunner
            };
            match pipeline::run(&cfg, runner) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    error!(error = %e, "Build failed");
                    ExitCode::from(1)
                }
            }
        }
    }
}

/// Logs go to stderr so `images` and `harbor-version` stdout stays parseable.
fn init_logging(verbose: bool) {
    tracing_subscriber::fmt()
        .with_max_level(if verbose {
            LevelFilter::DEBUG
        } else {
            LevelFilter::INFO
        })
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
