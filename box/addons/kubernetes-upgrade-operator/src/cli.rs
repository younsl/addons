//! Command-line flags, each backed by an environment variable so the Helm
//! chart configures the operator through `env`.

use clap::Parser;

#[derive(Parser, Debug, Default)]
#[command(
    name = "kuo",
    version,
    about = "Kubernetes Upgrade Operator for EKS clusters"
)]
pub struct Cli {
    /// Log level (`debug`, `info`, `warn`, `error`). `RUST_LOG` overrides it.
    #[arg(long, env = "LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    /// Log format (`json` or `text`).
    #[arg(long, env = "LOG_FORMAT", default_value = "json")]
    pub log_format: String,

    /// Slack incoming webhook URL. Notifications are disabled when empty.
    #[arg(long, env = "SLACK_WEBHOOK_URL", hide_env_values = true)]
    pub slack_webhook_url: Option<String>,

    /// Serve the MCP endpoint for kagent.
    #[arg(long, env = "MCP_ENABLED")]
    pub mcp_enabled: Option<String>,

    /// MCP listener port.
    #[arg(long, env = "MCP_PORT")]
    pub mcp_port: Option<String>,

    /// TTL of the MCP AWS read cache, in seconds.
    #[arg(long, env = "MCP_CACHE_TTL_SECONDS")]
    pub mcp_cache_ttl_seconds: Option<String>,

    /// Path of the mounted MCP bearer token file.
    #[arg(long, env = "MCP_TOKEN_FILE")]
    pub mcp_token_file: Option<String>,

    /// Post upgrade phase annotations to Grafana.
    #[arg(long, env = "GRAFANA_ANNOTATION_ENABLED")]
    pub grafana_annotation_enabled: Option<String>,

    /// Grafana base URL.
    #[arg(long, env = "GRAFANA_URL")]
    pub grafana_url: Option<String>,

    /// Grafana service account token.
    #[arg(long, env = "GRAFANA_API_TOKEN", hide_env_values = true)]
    pub grafana_api_token: Option<String>,

    /// Comma-separated base tags merged into every annotation.
    #[arg(long, env = "GRAFANA_ANNOTATION_TAGS")]
    pub grafana_annotation_tags: Option<String>,

    /// Which runs to annotate (`all`, `upgrade`, `dryRun`).
    #[arg(long, env = "GRAFANA_ANNOTATE_ON")]
    pub grafana_annotate_on: Option<String>,
}

impl Cli {
    /// Resolve a setting by its environment variable name, trimmed, with an
    /// empty value treated as unset.
    pub fn lookup(&self, key: &str) -> Option<String> {
        let value = match key {
            "SLACK_WEBHOOK_URL" => &self.slack_webhook_url,
            "MCP_ENABLED" => &self.mcp_enabled,
            "MCP_PORT" => &self.mcp_port,
            "MCP_CACHE_TTL_SECONDS" => &self.mcp_cache_ttl_seconds,
            "MCP_TOKEN_FILE" => &self.mcp_token_file,
            "GRAFANA_ANNOTATION_ENABLED" => &self.grafana_annotation_enabled,
            "GRAFANA_URL" => &self.grafana_url,
            "GRAFANA_API_TOKEN" => &self.grafana_api_token,
            "GRAFANA_ANNOTATION_TAGS" => &self.grafana_annotation_tags,
            "GRAFANA_ANNOTATE_ON" => &self.grafana_annotate_on,
            _ => return None,
        };
        value
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(String::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn test_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn test_defaults() {
        let cli = Cli::try_parse_from(["kuo"]).unwrap();
        assert_eq!(cli.log_level, "info");
        assert_eq!(cli.log_format, "json");
    }

    #[test]
    fn test_flags_parse() {
        let cli = Cli::try_parse_from([
            "kuo",
            "--log-level",
            "debug",
            "--log-format",
            "text",
            "--mcp-enabled",
            "true",
            "--mcp-port",
            "9090",
            "--grafana-annotate-on",
            "dryRun",
        ])
        .unwrap();
        assert_eq!(cli.log_level, "debug");
        assert_eq!(cli.log_format, "text");
        assert_eq!(cli.lookup("MCP_ENABLED").as_deref(), Some("true"));
        assert_eq!(cli.lookup("MCP_PORT").as_deref(), Some("9090"));
        assert_eq!(cli.lookup("GRAFANA_ANNOTATE_ON").as_deref(), Some("dryRun"));
    }

    #[test]
    fn test_lookup_trims_and_drops_empty() {
        let cli = Cli {
            grafana_url: Some("  https://grafana.example.com  ".to_string()),
            grafana_api_token: Some("   ".to_string()),
            ..Default::default()
        };
        assert_eq!(
            cli.lookup("GRAFANA_URL").as_deref(),
            Some("https://grafana.example.com")
        );
        assert_eq!(cli.lookup("GRAFANA_API_TOKEN"), None);
        assert_eq!(cli.lookup("SLACK_WEBHOOK_URL"), None);
        assert_eq!(cli.lookup("UNKNOWN"), None);
    }
}
