use std::path::PathBuf;

use clap::{Parser, ValueEnum};

const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (commit ",
    env!("BUILD_COMMIT"),
    ", built ",
    env!("BUILD_DATE"),
    ")"
);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    #[default]
    Json,
    Text,
}

/// Terminal UI for `ElastiCache` (Valkey and Redis OSS).
///
/// Lists every replication group the AWS profile can see with item counts, connections, and
/// load, shows which Kubernetes pods hold connections to each node, and browses keys read-only
/// through a pod that already reaches the cache.
#[derive(Debug, Default, Parser)]
#[command(version, long_version = LONG_VERSION)]
pub struct Cli {
    /// Target name from the config file (default: `default_target`)
    #[arg(env = "VALCON_TARGET")]
    pub target: Option<String>,

    /// Config file (default: `$XDG_CONFIG_HOME/valcon/config.toml`)
    #[arg(long, env = "VALCON_CONFIG")]
    pub config: Option<PathBuf>,

    /// Print the targets in the config file and exit
    #[arg(long, env = "VALCON_LIST_TARGETS")]
    pub list_targets: bool,

    /// Kubeconfig context (default: current-context)
    #[arg(short = 'c', long, env = "VALCON_CONTEXT")]
    pub context: Option<String>,

    /// Namespaces whose pods are scanned, comma separated (default: the context namespace)
    #[arg(
        short = 'n',
        long = "namespace",
        env = "VALCON_NAMESPACES",
        value_delimiter = ','
    )]
    pub namespaces: Option<Vec<String>>,

    /// Scan pods in every namespace
    #[arg(short = 'A', long, env = "VALCON_ALL_NAMESPACES")]
    pub all_namespaces: bool,

    /// Pod label selector
    #[arg(short = 'l', long, env = "VALCON_SELECTOR")]
    pub selector: Option<String>,

    /// Engine port [default: 6379]
    #[arg(long, env = "VALCON_PORT")]
    pub port: Option<u16>,

    /// AWS profile for the `ElastiCache`, EC2, and `CloudWatch` APIs
    #[arg(short = 'p', long, env = "AWS_PROFILE")]
    pub aws_profile: Option<String>,

    /// AWS region
    #[arg(long, env = "AWS_REGION")]
    pub aws_region: Option<String>,

    /// Seconds between refreshes [default: 15]
    #[arg(short = 'i', long, env = "VALCON_INTERVAL")]
    pub interval: Option<u64>,

    /// Concurrent pod execs per refresh [default: 10]
    #[arg(short = 'P', long, env = "VALCON_PARALLEL")]
    pub parallel: Option<usize>,

    /// Image of the ephemeral probe container used for database queries (needs `nc`)
    #[arg(long, env = "VALCON_PROBE_IMAGE")]
    pub probe_image: Option<String>,

    /// Seconds to wait for a new probe container to start [default: 60]
    #[arg(long, env = "VALCON_PROBE_TIMEOUT")]
    pub probe_timeout: Option<u64>,

    /// Per-container exec timeout in seconds [default: 10]
    #[arg(long, env = "VALCON_EXEC_TIMEOUT")]
    pub exec_timeout: Option<u64>,

    /// Write logs to this file (the TUI owns the terminal, so nothing is logged without it)
    #[arg(long, env = "VALCON_LOG_FILE")]
    pub log_file: Option<PathBuf>,

    /// Log level for this crate, overridden by `RUST_LOG`
    #[arg(long, env = "LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    /// Log format
    #[arg(long, env = "LOG_FORMAT", value_enum, default_value_t = LogFormat::Json)]
    pub log_format: LogFormat,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn command_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_target_and_flags() {
        let cli = Cli::try_parse_from([
            "valcon",
            "prod",
            "-n",
            "shop,pay",
            "-i",
            "3",
            "--log-format",
            "text",
        ])
        .unwrap();
        assert_eq!(cli.target.as_deref(), Some("prod"));
        assert_eq!(cli.namespaces, Some(vec!["shop".into(), "pay".into()]));
        assert_eq!(cli.interval, Some(3));
        assert_eq!(cli.log_format, LogFormat::Text);
        assert!(
            Cli::try_parse_from(["valcon", "-A"])
                .unwrap()
                .all_namespaces
        );
    }
}
