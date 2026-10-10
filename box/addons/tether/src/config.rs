//! Runtime settings. Every flag also reads an environment variable.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

use crate::error::ConfigError;

const BUILD_COMMIT: &str = env!("BUILD_COMMIT");
const BUILD_DATE: &str = env!("BUILD_DATE");
const BUILD_RUSTC_VERSION: &str = env!("BUILD_RUSTC_VERSION");

/// Build metadata baked in by `build.rs`.
#[derive(Debug, Clone, Copy)]
pub struct BuildInfo {
    pub version: &'static str,
    pub commit: &'static str,
    pub date: &'static str,
    pub rustc: &'static str,
}

impl BuildInfo {
    pub const CURRENT: Self = Self {
        version: env!("CARGO_PKG_VERSION"),
        commit: BUILD_COMMIT,
        date: BUILD_DATE,
        rustc: BUILD_RUSTC_VERSION,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LogFormat {
    Json,
    Text,
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "tether",
    version = const_format(),
    about = "Server that keeps dotfiles symlinked into a home directory"
)]
pub struct Config {
    /// Link spec file mapping dotfiles sources to home targets.
    #[arg(long, env = "LINKS_FILE", default_value = "/etc/tether/links.toml")]
    pub links_file: PathBuf,

    /// Home directory that `~` in the spec expands to. In a container, mount
    /// it at the same path as on the host so the links resolve there too.
    #[arg(long, env = "HOME", value_parser = parse_home)]
    pub home: PathBuf,

    /// Reconcile interval (e.g. 30s, 5m). Minimum 1s.
    #[arg(long, env = "RECONCILE_INTERVAL", default_value = "5m", value_parser = parse_duration)]
    pub reconcile_interval: Duration,

    /// Report planned changes without touching the file system.
    #[arg(long, env = "DRY_RUN")]
    pub dry_run: bool,

    /// Port serving /status, /reconcile, /healthz, /readyz, and /metrics.
    #[arg(long, env = "PORT", default_value_t = 8080, value_parser = clap::value_parser!(u16).range(1..))]
    pub port: u16,

    /// Log level: trace, debug, info, warn, error.
    #[arg(long, env = "LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    /// Log output format.
    #[arg(
        long,
        env = "LOG_FORMAT",
        default_value = "json",
        value_enum,
        ignore_case = true
    )]
    pub log_format: LogFormat,
}

const fn const_format() -> &'static str {
    concat!(
        env!("CARGO_PKG_VERSION"),
        " (commit ",
        env!("BUILD_COMMIT"),
        ", built ",
        env!("BUILD_DATE"),
        ", rustc ",
        env!("BUILD_RUSTC_VERSION"),
        ")"
    )
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let d = humantime::parse_duration(s).map_err(|e| format!("invalid duration {s:?}: {e}"))?;
    if d < Duration::from_secs(1) {
        return Err(ConfigError::IntervalTooShort(d).to_string());
    }
    Ok(d)
}

fn parse_home(s: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(s);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(ConfigError::HomeNotAbsolute(path).to_string())
    }
}

impl Config {
    /// Parse from the process arguments and environment.
    pub fn load() -> Self {
        Self::parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Config, clap::Error> {
        Config::try_parse_from(std::iter::once("tether").chain(args.iter().copied()))
    }

    #[test]
    fn defaults() {
        let cfg = parse(&["--home", "/home/dev"]).expect("defaults parse");
        assert_eq!(cfg.links_file, PathBuf::from("/etc/tether/links.toml"));
        assert_eq!(cfg.home, PathBuf::from("/home/dev"));
        assert_eq!(cfg.reconcile_interval, Duration::from_secs(300));
        assert!(!cfg.dry_run);
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.log_level, "info");
        assert_eq!(cfg.log_format, LogFormat::Json);
    }

    #[test]
    fn overrides() {
        let cfg = parse(&[
            "--links-file",
            "/tmp/links.toml",
            "--home",
            "/Users/dev",
            "--reconcile-interval",
            "1m30s",
            "--dry-run",
            "--port",
            "9000",
            "--log-level",
            "debug",
            "--log-format",
            "TEXT",
        ])
        .expect("overrides parse");
        assert_eq!(cfg.links_file, PathBuf::from("/tmp/links.toml"));
        assert_eq!(cfg.reconcile_interval, Duration::from_secs(90));
        assert!(cfg.dry_run);
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(cfg.log_format, LogFormat::Text);
    }

    #[test]
    fn rejects_relative_home() {
        let err = parse(&["--home", "home/dev"]).expect_err("relative home");
        assert!(err.to_string().contains("absolute"), "{err}");
    }

    #[test]
    fn rejects_short_interval() {
        let err = parse(&["--home", "/h", "--reconcile-interval", "500ms"]).expect_err("short");
        assert!(err.to_string().contains("at least 1s"), "{err}");
        assert!(parse(&["--home", "/h", "--reconcile-interval", "soon"]).is_err());
    }

    #[test]
    fn build_info_populated() {
        let info = BuildInfo::CURRENT;
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert_ne!(info.commit, "");
        assert_ne!(info.rustc, "");
        assert!(const_format().starts_with(info.version));
    }
}
