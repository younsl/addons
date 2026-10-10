//! Runtime settings. Every flag also reads an environment variable so the
//! Helm chart keeps injecting plain env vars.

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
    name = "ec2-metadata-exporter",
    version = const_format(),
    about = "Prometheus exporter that publishes EC2 instance metadata as Prometheus metrics"
)]
pub struct Config {
    /// AWS region to scan. Falls back to the SDK default chain when unset.
    #[arg(long, env = "AWS_REGION")]
    pub region: Option<String>,

    /// EC2 API polling interval (e.g. 60s, 5m). Minimum 1s.
    #[arg(long, env = "SCRAPE_INTERVAL", default_value = "60s", value_parser = parse_duration)]
    pub scrape_interval: Duration,

    /// Port serving /metrics.
    #[arg(long, env = "METRICS_PORT", default_value_t = 8081, value_parser = clap::value_parser!(u16).range(1..))]
    pub metrics_port: u16,

    /// Port serving /healthz and /readyz.
    #[arg(long, env = "HEALTH_PORT", default_value_t = 8080, value_parser = clap::value_parser!(u16).range(1..))]
    pub health_port: u16,

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
        return Err(ConfigError::ScrapeIntervalTooShort(d).to_string());
    }
    Ok(d)
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
        Config::try_parse_from(std::iter::once("ec2-metadata-exporter").chain(args.iter().copied()))
    }

    #[test]
    fn defaults() {
        let cfg = parse(&[]).expect("defaults parse");
        assert_eq!(cfg.scrape_interval, Duration::from_secs(60));
        assert_eq!(cfg.metrics_port, 8081);
        assert_eq!(cfg.health_port, 8080);
        assert_eq!(cfg.log_level, "info");
        assert_eq!(cfg.log_format, LogFormat::Json);
    }

    #[test]
    fn overrides() {
        let cfg = parse(&[
            "--region",
            "ap-northeast-2",
            "--scrape-interval",
            "1m30s",
            "--metrics-port",
            "9000",
            "--health-port",
            "9001",
            "--log-level",
            "debug",
            "--log-format",
            "TEXT",
        ])
        .expect("overrides parse");
        assert_eq!(cfg.region.as_deref(), Some("ap-northeast-2"));
        assert_eq!(cfg.scrape_interval, Duration::from_secs(90));
        assert_eq!(cfg.metrics_port, 9000);
        assert_eq!(cfg.health_port, 9001);
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(cfg.log_format, LogFormat::Text);
    }

    #[test]
    fn build_info_populated() {
        let info = BuildInfo::CURRENT;
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert!(!info.commit.is_empty());
        assert!(!info.rustc.is_empty());
        assert!(const_format().starts_with(info.version));
    }
}
