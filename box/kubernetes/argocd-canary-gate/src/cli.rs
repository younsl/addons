//! Command line flags. Every flag has an environment fallback so the chart can
//! set either.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use clap::Parser;

/// Blocks an Argo CD Application sync while an Argo Rollouts canary owned by
/// that Application is still in progress.
#[derive(Debug, Parser)]
#[command(name = "argocd-canary-gate", disable_version_flag = true)]
pub struct Cli {
    /// Path to the gate configuration file
    #[arg(
        long,
        env = "GATE_CONFIG",
        default_value = "/etc/argocd-canary-gate/config.yaml"
    )]
    pub config: PathBuf,

    /// Address for the HTTPS admission webhook listener
    #[arg(long, env = "WEBHOOK_ADDR", default_value = ":8443", value_parser = parse_addr)]
    pub webhook_addr: SocketAddr,

    /// Address for probes and metrics
    #[arg(long, env = "ADMIN_ADDR", default_value = ":8080", value_parser = parse_addr)]
    pub admin_addr: SocketAddr,

    /// PEM serving certificate for the webhook listener
    #[arg(
        long,
        env = "TLS_CERT_FILE",
        default_value = "/etc/argocd-canary-gate/tls/tls.crt"
    )]
    pub tls_cert_file: PathBuf,

    /// PEM private key for the webhook listener
    #[arg(
        long,
        env = "TLS_KEY_FILE",
        default_value = "/etc/argocd-canary-gate/tls/tls.key"
    )]
    pub tls_key_file: PathBuf,

    /// Path to a kubeconfig. Empty uses the in-cluster config
    #[arg(long, env = "KUBECONFIG", default_value = "")]
    pub kubeconfig: String,

    /// Log level: debug, info, warn, error
    #[arg(long, env = "LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    /// Log format: json or text
    #[arg(long, env = "LOG_FORMAT", default_value = "json")]
    pub log_format: String,

    /// Print version and exit
    #[arg(short = 'V', long)]
    pub version: bool,
}

/// Parses a listen address, accepting the Go style `:8443` shorthand for "all
/// interfaces" so the chart's arguments keep working unchanged.
fn parse_addr(raw: &str) -> Result<SocketAddr, String> {
    let raw = raw.trim();
    if let Some(port) = raw.strip_prefix(':') {
        let port: u16 = port
            .parse()
            .map_err(|e| format!("invalid port in {raw:?}: {e}"))?;
        return Ok(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)));
    }
    raw.parse()
        .map_err(|e| format!("invalid address {raw:?}: {e}"))
}

/// The multi-line version banner.
#[must_use]
pub fn version_banner() -> String {
    format!(
        "argocd-canary-gate {}\ncommit: {}\nrustc: {}\nplatform: {}/{}\n",
        env!("CARGO_PKG_VERSION"),
        env!("BUILD_COMMIT"),
        env!("BUILD_RUSTC_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_chart() {
        let cli = Cli::try_parse_from(["gate"]).unwrap();
        assert_eq!(cli.webhook_addr.port(), 8443);
        assert_eq!(cli.admin_addr.port(), 8080);
        assert!(cli.webhook_addr.ip().is_unspecified());
        assert!(!cli.version);
        assert_eq!(
            cli.config,
            PathBuf::from("/etc/argocd-canary-gate/config.yaml")
        );
    }

    #[test]
    fn accepts_go_style_and_full_addresses() {
        let cli = Cli::try_parse_from([
            "gate",
            "--webhook-addr=:9443",
            "--admin-addr",
            "127.0.0.1:9090",
            "-V",
        ])
        .unwrap();
        assert_eq!(cli.webhook_addr.port(), 9443);
        assert_eq!(cli.admin_addr.to_string(), "127.0.0.1:9090");
        assert!(cli.version);
        assert!(Cli::try_parse_from(["gate", "--admin-addr=:x"]).is_err());
        assert!(Cli::try_parse_from(["gate", "--admin-addr=nope"]).is_err());
        assert!(version_banner().starts_with("argocd-canary-gate "));
    }
}
