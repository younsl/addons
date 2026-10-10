use anyhow::{Result, anyhow};
use clap::Parser;

#[derive(Debug, Clone, Parser)]
#[command(name = "ghes-schedule-scanner", version, about)]
pub struct Config {
    // Slack Configuration
    /// Slack Bot User OAuth Token starting with xoxb-
    #[arg(
        long = "slack-token",
        env = "SLACK_TOKEN",
        value_name = "SLACK_TOKEN",
        hide_env_values = true
    )]
    pub slack_bot_token: Option<String>,
    /// Slack channel ID that holds the canvas
    #[arg(long, env = "SLACK_CHANNEL_ID")]
    pub slack_channel_id: Option<String>,
    /// Slack canvas ID to update
    #[arg(long, env = "SLACK_CANVAS_ID")]
    pub slack_canvas_id: Option<String>,

    // GitHub Configuration
    /// GitHub personal access token
    #[arg(long, env = "GITHUB_TOKEN", hide_env_values = true)]
    pub github_token: String,
    /// GitHub organization to scan
    #[arg(long = "github-org", env = "GITHUB_ORG", value_name = "GITHUB_ORG")]
    pub github_organization: String,
    /// GitHub Enterprise Server base URL without /api/v3
    #[arg(long, env = "GITHUB_BASE_URL")]
    pub github_base_url: String,

    // Application Configuration
    /// Log level: trace, debug, info, warn, error
    #[arg(long, env = "LOG_LEVEL", default_value = "INFO")]
    pub log_level: String,
    /// Log format: json or text
    #[arg(long, env = "LOG_FORMAT", default_value = "json")]
    pub log_format: String,
    /// Timeout in seconds for each GitHub API request
    #[arg(long, env = "REQUEST_TIMEOUT", default_value_t = 60)]
    pub request_timeout: u64,
    /// Number of repositories scanned in parallel
    #[arg(long, env = "CONCURRENT_SCANS", default_value_t = 10)]
    pub concurrent_scans: usize,
    /// Publisher: console or slack-canvas
    #[arg(long, env = "PUBLISHER_TYPE", default_value = "console")]
    pub publisher_type: String,

    // Connectivity Configuration
    /// Connectivity check attempts before giving up
    #[arg(long, env = "CONNECTIVITY_MAX_RETRIES", default_value_t = 3)]
    pub connectivity_max_retries: u32,
    /// Seconds between connectivity check attempts
    #[arg(long, env = "CONNECTIVITY_RETRY_INTERVAL", default_value_t = 5)]
    pub connectivity_retry_interval: u64,
    /// Timeout in seconds for each connectivity check
    #[arg(long, env = "CONNECTIVITY_TIMEOUT", default_value_t = 5)]
    pub connectivity_timeout: u64,
}

impl Config {
    pub fn load() -> Result<Self> {
        Self::parse().checked()
    }

    fn checked(self) -> Result<Self> {
        // Validate Slack token format if provided
        if let Some(ref token) = self.slack_bot_token
            && !token.starts_with("xoxb-")
        {
            return Err(anyhow!(
                "SLACK_TOKEN must start with 'xoxb-' (Bot User OAuth Token)"
            ));
        }
        Ok(self)
    }

    pub fn validate(&self) -> Result<()> {
        // Validate publisher type specific requirements
        match self.publisher_type.as_str() {
            "slack-canvas" => {
                if self.slack_bot_token.is_none() {
                    return Err(anyhow!(
                        "SLACK_TOKEN is required when using slack-canvas publisher"
                    ));
                }
                if self.slack_channel_id.is_none() {
                    return Err(anyhow!(
                        "SLACK_CHANNEL_ID is required when using slack-canvas publisher"
                    ));
                }
                if self.slack_canvas_id.is_none() {
                    return Err(anyhow!(
                        "SLACK_CANVAS_ID is required when using slack-canvas publisher"
                    ));
                }
            }
            "console" => {
                // No additional validation needed for console publisher
            }
            _ => {
                return Err(anyhow!(
                    "Invalid publisher type: {}. Supported types: console, slack-canvas",
                    self.publisher_type
                ));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
impl Config {
    pub fn new_for_test(github_token: String, github_org: String, github_base_url: String) -> Self {
        Self {
            github_token,
            github_organization: github_org,
            github_base_url,
            log_level: "INFO".to_string(),
            log_format: "json".to_string(),
            request_timeout: 60,
            concurrent_scans: 10,
            publisher_type: "console".to_string(),
            slack_bot_token: None,
            slack_channel_id: None,
            slack_canvas_id: None,
            connectivity_max_retries: 3,
            connectivity_retry_interval: 5,
            connectivity_timeout: 5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUIRED_ARGS: [&str; 7] = [
        "gss",
        "--github-token",
        "test-token",
        "--github-org",
        "test-org",
        "--github-base-url",
        "https://github.example.com",
    ];

    #[test]
    fn test_parse_flags() {
        let config = Config::try_parse_from(REQUIRED_ARGS.iter().copied().chain([
            "--log-format",
            "text",
            "--request-timeout",
            "30",
            "--concurrent-scans",
            "4",
            "--slack-token",
            "xoxb-valid-token",
        ]))
        .unwrap();

        assert_eq!(config.github_token, "test-token");
        assert_eq!(config.github_organization, "test-org");
        assert_eq!(config.log_format, "text");
        assert_eq!(config.request_timeout, 30);
        assert_eq!(config.concurrent_scans, 4);
        assert!(config.checked().is_ok());
    }

    #[test]
    fn test_parse_rejects_invalid_number() {
        let result = Config::try_parse_from(
            REQUIRED_ARGS
                .iter()
                .copied()
                .chain(["--concurrent-scans", "many"]),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_checked_rejects_non_bot_slack_token() {
        let mut config = Config::new_for_test(
            "test-token".to_string(),
            "test-org".to_string(),
            "https://github.example.com".to_string(),
        );
        config.slack_bot_token = Some("xapp-token".to_string());
        let err = config.checked().unwrap_err();
        assert!(err.to_string().contains("must start with 'xoxb-'"));
    }

    #[test]
    fn test_config_creation() {
        let config = Config::new_for_test(
            "test-token".to_string(),
            "test-org".to_string(),
            "https://github.example.com".to_string(),
        );

        assert_eq!(config.github_token, "test-token");
        assert_eq!(config.github_organization, "test-org");
        assert_eq!(config.github_base_url, "https://github.example.com");
        assert_eq!(config.log_level, "INFO");
        assert_eq!(config.request_timeout, 60);
        assert_eq!(config.concurrent_scans, 10);
        assert_eq!(config.publisher_type, "console");
    }

    #[test]
    fn test_slack_token_validation() {
        let mut config = Config::new_for_test(
            "test-token".to_string(),
            "test-org".to_string(),
            "https://github.example.com".to_string(),
        );

        // Test with invalid Slack token
        config.slack_bot_token = Some("invalid-token".to_string());
        config.publisher_type = "slack-canvas".to_string();

        let result = config.validate();
        assert!(result.is_err());
    }

    #[test]
    fn test_publisher_validation_slack_canvas_missing_token() {
        let mut config = Config::new_for_test(
            "test-token".to_string(),
            "test-org".to_string(),
            "https://github.example.com".to_string(),
        );

        config.publisher_type = "slack-canvas".to_string();
        let result = config.validate();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("SLACK_TOKEN is required")
        );
    }

    #[test]
    fn test_publisher_validation_slack_canvas_missing_channel() {
        let mut config = Config::new_for_test(
            "test-token".to_string(),
            "test-org".to_string(),
            "https://github.example.com".to_string(),
        );

        config.publisher_type = "slack-canvas".to_string();
        config.slack_bot_token = Some("xoxb-valid-token".to_string());
        let result = config.validate();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("SLACK_CHANNEL_ID is required")
        );
    }

    #[test]
    fn test_publisher_validation_console() {
        let config = Config::new_for_test(
            "test-token".to_string(),
            "test-org".to_string(),
            "https://github.example.com".to_string(),
        );

        let result = config.validate();
        assert!(result.is_ok());
    }

    #[test]
    fn test_publisher_validation_invalid_type() {
        let mut config = Config::new_for_test(
            "test-token".to_string(),
            "test-org".to_string(),
            "https://github.example.com".to_string(),
        );

        config.publisher_type = "invalid-type".to_string();
        let result = config.validate();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid publisher type")
        );
    }

    #[test]
    fn test_publisher_validation_slack_canvas_valid() {
        let mut config = Config::new_for_test(
            "test-token".to_string(),
            "test-org".to_string(),
            "https://github.example.com".to_string(),
        );
        config.publisher_type = "slack-canvas".to_string();
        config.slack_bot_token = Some("xoxb-valid-token".to_string());
        config.slack_channel_id = Some("C123456".to_string());
        config.slack_canvas_id = Some("F789012".to_string());
        let result = config.validate();
        assert!(result.is_ok());
    }

    #[test]
    fn test_publisher_validation_slack_canvas_missing_canvas_id() {
        let mut config = Config::new_for_test(
            "test-token".to_string(),
            "test-org".to_string(),
            "https://github.example.com".to_string(),
        );
        config.publisher_type = "slack-canvas".to_string();
        config.slack_bot_token = Some("xoxb-valid-token".to_string());
        config.slack_channel_id = Some("C123456".to_string());
        let result = config.validate();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("SLACK_CANVAS_ID is required")
        );
    }

    #[test]
    fn test_new_for_test_defaults() {
        let config = Config::new_for_test(
            "token".to_string(),
            "org".to_string(),
            "https://github.example.com".to_string(),
        );
        assert_eq!(config.connectivity_max_retries, 3);
        assert_eq!(config.connectivity_retry_interval, 5);
        assert_eq!(config.connectivity_timeout, 5);
        assert!(config.slack_bot_token.is_none());
        assert!(config.slack_channel_id.is_none());
        assert!(config.slack_canvas_id.is_none());
    }
}
