//! Server settings loaded from environment variables.

use std::time::Duration;

use anyhow::{Context, Result, bail};

/// How MCP clients reach the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Streamable HTTP on `LISTEN_PORT`, the mode kagent and other remote
    /// clients use.
    Http,
    /// JSON-RPC over stdin and stdout, for a local MCP client that spawns the
    /// binary directly.
    Stdio,
}

impl Transport {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Stdio => "stdio",
        }
    }
}

/// All runtime settings.
#[derive(Debug, Clone)]
pub struct Config {
    /// Backstage backend base URL without a trailing slash, for example
    /// `http://backstage.backstage.svc:7007`.
    pub backstage_url: String,
    /// Static external-access token presented to Backstage as a bearer token.
    /// Empty sends unauthenticated requests.
    pub backstage_token: String,
    /// Bound on a single request to Backstage.
    pub request_timeout: Duration,
    pub transport: Transport,
    pub listen_port: u16,
    /// Path the MCP endpoint is mounted on.
    pub mcp_path: String,
    /// Bearer token MCP clients must present. Empty disables inbound auth.
    pub mcp_bearer_token: String,
    /// Upper bound on the characters of one tool result.
    pub max_result_chars: usize,
    pub log_level: String,
    pub log_format: String,
}

impl Config {
    /// Loads the configuration from the process environment.
    ///
    /// # Errors
    ///
    /// Returns an error when a required variable is missing or a value cannot
    /// be parsed.
    pub fn load() -> Result<Self> {
        Self::from_env(|key| std::env::var(key).ok())
    }

    /// Loads the configuration from `env`, a lookup that mirrors
    /// `std::env::var` so tests can inject values.
    ///
    /// # Errors
    ///
    /// Returns an error when a required variable is missing or a value cannot
    /// be parsed.
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let lookup = |key: &str| {
            env(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };

        let backstage_url = lookup("BACKSTAGE_URL")
            .context("BACKSTAGE_URL is required (Backstage backend base URL)")?;
        if !backstage_url.starts_with("http://") && !backstage_url.starts_with("https://") {
            bail!("BACKSTAGE_URL must start with http:// or https://, got {backstage_url:?}");
        }

        let transport = match lookup("MCP_TRANSPORT").as_deref() {
            None | Some("http") => Transport::Http,
            Some("stdio") => Transport::Stdio,
            Some(other) => bail!("MCP_TRANSPORT must be http or stdio, got {other:?}"),
        };

        let mcp_path = lookup("MCP_PATH").unwrap_or_else(|| "/mcp".to_string());
        if !mcp_path.starts_with('/') || mcp_path.len() < 2 {
            bail!("MCP_PATH must be an absolute path such as /mcp, got {mcp_path:?}");
        }

        let log_level = lookup("LOG_LEVEL").unwrap_or_else(|| "info".to_string());
        let log_format = lookup("LOG_FORMAT").unwrap_or_else(|| "json".to_string());
        if log_format != "json" && log_format != "text" {
            bail!("LOG_FORMAT must be json or text, got {log_format:?}");
        }

        Ok(Self {
            backstage_url: backstage_url.trim_end_matches('/').to_string(),
            backstage_token: lookup("BACKSTAGE_TOKEN").unwrap_or_default(),
            request_timeout: Duration::from_secs(parse_positive(
                &lookup,
                "REQUEST_TIMEOUT_SECONDS",
                30,
            )?),
            transport,
            listen_port: u16::try_from(parse_positive(&lookup, "LISTEN_PORT", 8080)?)
                .context("LISTEN_PORT must fit in 16 bits")?,
            mcp_path: mcp_path.trim_end_matches('/').to_string(),
            mcp_bearer_token: lookup("MCP_BEARER_TOKEN").unwrap_or_default(),
            max_result_chars: usize::try_from(parse_positive(
                &lookup,
                "MAX_RESULT_CHARS",
                100_000,
            )?)
            .context("MAX_RESULT_CHARS is too large")?,
            log_level,
            log_format,
        })
    }
}

fn parse_positive(
    lookup: &impl Fn(&str) -> Option<String>,
    key: &str,
    fallback: u64,
) -> Result<u64> {
    let Some(raw) = lookup(key) else {
        return Ok(fallback);
    };
    let value: u64 = raw
        .parse()
        .with_context(|| format!("{key} must be a positive integer, got {raw:?}"))?;
    if value == 0 {
        bail!("{key} must be greater than zero");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key| owned.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    #[test]
    fn defaults_apply_when_only_url_is_set() {
        let cfg =
            Config::from_env(env_from(&[("BACKSTAGE_URL", "http://backstage:7007/")])).unwrap();
        assert_eq!(cfg.backstage_url, "http://backstage:7007");
        assert_eq!(cfg.transport, Transport::Http);
        assert_eq!(cfg.listen_port, 8080);
        assert_eq!(cfg.mcp_path, "/mcp");
        assert_eq!(cfg.request_timeout, Duration::from_secs(30));
        assert_eq!(cfg.max_result_chars, 100_000);
        assert!(cfg.backstage_token.is_empty());
        assert!(cfg.mcp_bearer_token.is_empty());
        assert_eq!(cfg.log_format, "json");
    }

    #[test]
    fn url_is_required_and_validated() {
        assert!(Config::from_env(env_from(&[])).is_err());
        assert!(Config::from_env(env_from(&[("BACKSTAGE_URL", "backstage:7007")])).is_err());
        assert!(Config::from_env(env_from(&[("BACKSTAGE_URL", "   ")])).is_err());
    }

    #[test]
    fn overrides_are_parsed() {
        let cfg = Config::from_env(env_from(&[
            ("BACKSTAGE_URL", "https://backstage.example.com"),
            ("BACKSTAGE_TOKEN", "secret"),
            ("MCP_TRANSPORT", "stdio"),
            ("LISTEN_PORT", "9090"),
            ("MCP_PATH", "/api/mcp/"),
            ("MCP_BEARER_TOKEN", "inbound"),
            ("REQUEST_TIMEOUT_SECONDS", "5"),
            ("MAX_RESULT_CHARS", "2000"),
            ("LOG_LEVEL", "debug"),
            ("LOG_FORMAT", "text"),
        ]))
        .unwrap();
        assert_eq!(cfg.backstage_token, "secret");
        assert_eq!(cfg.transport, Transport::Stdio);
        assert_eq!(cfg.transport.as_str(), "stdio");
        assert_eq!(cfg.listen_port, 9090);
        assert_eq!(cfg.mcp_path, "/api/mcp");
        assert_eq!(cfg.mcp_bearer_token, "inbound");
        assert_eq!(cfg.request_timeout, Duration::from_secs(5));
        assert_eq!(cfg.max_result_chars, 2000);
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(cfg.log_format, "text");
    }

    #[test]
    fn invalid_values_are_rejected() {
        let base = ("BACKSTAGE_URL", "http://backstage:7007");
        assert!(Config::from_env(env_from(&[base, ("MCP_TRANSPORT", "sse")])).is_err());
        assert!(Config::from_env(env_from(&[base, ("MCP_PATH", "mcp")])).is_err());
        assert!(Config::from_env(env_from(&[base, ("LISTEN_PORT", "0")])).is_err());
        assert!(Config::from_env(env_from(&[base, ("LISTEN_PORT", "70000")])).is_err());
        assert!(Config::from_env(env_from(&[base, ("REQUEST_TIMEOUT_SECONDS", "abc")])).is_err());
        assert!(Config::from_env(env_from(&[base, ("LOG_FORMAT", "yaml")])).is_err());
    }
}
