use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

// ============================================
// Environment variable name constants
// These are shared between config parsing and API exposure
// ============================================
pub mod env {
    pub const MODE: &str = "MODE";
    pub const LOG_FORMAT: &str = "LOG_FORMAT";
    pub const LOG_LEVEL: &str = "LOG_LEVEL";
    pub const HEALTH_PORT: &str = "HEALTH_PORT";
    pub const CLUSTER_NAME: &str = "CLUSTER_NAME";
    pub const NAMESPACES: &str = "NAMESPACES";
    pub const COLLECT_VULN: &str = "COLLECT_VULN";
    pub const COLLECT_SBOM: &str = "COLLECT_SBOM";
    pub const SERVER_PORT: &str = "SERVER_PORT";
    pub const STORAGE_PATH: &str = "STORAGE_PATH";
    pub const WATCH_LOCAL: &str = "WATCH_LOCAL";

    // Internal API between the scraper (sole database owner) and the
    // stateless server pods.
    pub const INTERNAL_PORT: &str = "INTERNAL_PORT";
    pub const INTERNAL_TOKEN: &str = "INTERNAL_TOKEN";
    pub const SCRAPER_URL: &str = "SCRAPER_URL";

    // Kubernetes objects holding authored state (server-mode only).
    pub const NOTES_CONFIGMAP: &str = "NOTES_CONFIGMAP";
    pub const API_TOKENS_SECRET: &str = "API_TOKENS_SECRET";

    // Hub-pull mode (server-mode only). Hub is always on in server mode; no toggle.
    pub const HUB_SECRET_NAMESPACE: &str = "HUB_SECRET_NAMESPACE";

    // External base URL used by notification deep links (server-mode only).
    pub const EXTERNAL_URL: &str = "EXTERNAL_URL";

    // Embedded MCP server (server-mode only).
    pub const MCP_ENABLED: &str = "MCP_ENABLED";
    pub const MCP_ALLOWED_HOSTS: &str = "MCP_ALLOWED_HOSTS";
    pub const MCP_STATELESS: &str = "MCP_STATELESS";
    pub const MCP_MAX_CONCURRENCY: &str = "MCP_MAX_CONCURRENCY";

    // Authentication
    pub use crate::auth::config::env::*;
}

/// Deployment role. The binary ships as a single image but runs as one of
/// two pods on the central cluster, distinguished by `--mode` / `MODE=`:
///
/// - `server` — HTTP UI + API. Holds no database and no volume; reads reports
///   through the scraper's internal API. No watchers. Default for new installs.
/// - `scraper` — Hub-pull watchers (Secret watcher + per-cluster watchers +
///   optional local Trivy CRD watcher). Sole owner of the SQLite file on its
///   own `emptyDir`, and serves it back over the internal API.
///   No UI (only the internal API, /healthz, /readyz and /metrics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// UI / API only — proxies reads to the scraper, no database, no watchers.
    Server,
    /// Hub-pull scraper — runs all watchers and owns the database.
    Scraper,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mode::Server => write!(f, "server"),
            Mode::Scraper => write!(f, "scraper"),
        }
    }
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Show version information
    Version,
    /// One-shot migration off a PersistentVolume: read an existing SQLite
    /// database and write its authored state (API tokens, report notes) to a
    /// Secret and a ConfigMap through the API server.
    ///
    /// Reports need no export — the next scraper start relists them from the
    /// clusters that own the CRs.
    ExportState {
        /// Path to the legacy database file, e.g. `/data/trivy.db`.
        #[arg(long)]
        db_path: String,
        /// Namespace to write the objects into. Empty = auto-detect.
        #[arg(long, default_value = "")]
        namespace: String,
        /// Report only what would be written, without writing it.
        #[arg(long, default_value = "false")]
        dry_run: bool,
    },
}

#[derive(Parser, Debug, Clone)]
#[command(
    name = "trivy-collector",
    version,
    about = "Multi-cluster Trivy report collector and viewer",
    long_about = "A Kubernetes application that collects Trivy Operator reports from multiple clusters and provides a centralized UI for viewing and filtering security reports."
)]
pub struct Config {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Deployment role: `server` (UI/API) or `scraper` (watchers).
    #[arg(long, env = env::MODE, value_enum, default_value = "server")]
    pub mode: Mode,

    /// Log format: json or pretty
    #[arg(long, env = env::LOG_FORMAT, default_value = "json")]
    pub log_format: String,

    /// Log level: trace, debug, info, warn, error
    #[arg(long, env = env::LOG_LEVEL, default_value = "info")]
    pub log_level: String,

    /// Health check server port
    #[arg(long, env = env::HEALTH_PORT, default_value = "8080")]
    pub health_port: u16,

    // ============================================
    // Scraper mode settings
    // ============================================
    /// Cluster identifier
    #[arg(long, env = env::CLUSTER_NAME, default_value = "local")]
    pub cluster_name: String,

    /// Namespaces to watch, comma-separated (empty = all namespaces)
    #[arg(long, env = env::NAMESPACES, value_delimiter = ',')]
    pub namespaces: Vec<String>,

    /// Collect VulnerabilityReports
    #[arg(long, env = env::COLLECT_VULN, default_value = "true")]
    pub collect_vulnerability_reports: bool,

    /// Collect SbomReports
    #[arg(long, env = env::COLLECT_SBOM, default_value = "true")]
    pub collect_sbom_reports: bool,

    // ============================================
    // Server mode settings
    // ============================================
    /// API/UI server port (server mode only)
    #[arg(long, env = env::SERVER_PORT, default_value = "3000")]
    pub server_port: u16,

    /// Directory holding the SQLite database (scraper mode only). Backed by an
    /// `emptyDir`, so its contents are rebuilt from the watched clusters on
    /// every start.
    #[arg(long, env = env::STORAGE_PATH, default_value = "/data")]
    pub storage_path: String,

    /// Enable the local-cluster watcher (scraper mode only)
    #[arg(long, env = env::WATCH_LOCAL, default_value = "true")]
    pub watch_local: bool,

    // ============================================
    // Internal API (scraper <-> server)
    // ============================================
    /// Port the scraper serves its internal read API on. Never exposed
    /// through an HTTPRoute or ServiceMonitor.
    #[arg(long, env = env::INTERNAL_PORT, default_value = "8081")]
    pub internal_port: u16,

    /// Shared token guarding the internal API, mounted into both pods from the
    /// same Secret. An empty value makes the scraper reject every request.
    #[arg(long, env = env::INTERNAL_TOKEN, default_value = "")]
    pub internal_token: String,

    /// Base URL of the scraper's internal API (server mode only).
    #[arg(long, env = env::SCRAPER_URL, default_value = "http://localhost:8081")]
    pub scraper_url: String,

    /// ConfigMap holding report notes (server mode only).
    #[arg(long, env = env::NOTES_CONFIGMAP, default_value = "trivy-collector-notes")]
    pub notes_configmap: String,

    /// Secret holding API tokens (server mode only).
    #[arg(long, env = env::API_TOKENS_SECRET, default_value = "trivy-collector-api-tokens")]
    pub api_tokens_secret: String,

    /// Namespace where cluster-registration Secrets live. Empty = auto-detect from
    /// the in-cluster ServiceAccount mount. Hub-pull mode is always active in server mode.
    #[arg(long, env = env::HUB_SECRET_NAMESPACE, default_value = "")]
    pub hub_secret_namespace: String,

    /// External base URL (e.g. `https://trivy.example.com`) used to build
    /// deep links in outbound notifications. Empty = no link emitted.
    #[arg(long, env = env::EXTERNAL_URL, default_value = "")]
    pub external_url: String,

    // ============================================
    // MCP settings (server mode only)
    // ============================================
    /// Mount the embedded MCP server (Streamable HTTP) at `/mcp`
    #[arg(long, env = env::MCP_ENABLED, default_value = "false")]
    pub mcp_enabled: bool,

    /// Allowed `Host` header values for `/mcp`, comma-separated.
    /// Empty = disable Host validation (required for in-cluster Service DNS).
    #[arg(long, env = env::MCP_ALLOWED_HOSTS, value_delimiter = ',', default_value = "")]
    pub mcp_allowed_hosts: Vec<String>,

    /// Serve `/mcp` without sessions. Required when more than one server replica
    /// sits behind the same Service.
    #[arg(long, env = env::MCP_STATELESS, default_value = "false")]
    pub mcp_stateless: bool,

    /// Maximum MCP tool calls executing at once across all sessions. Protects
    /// the shared SQLite pool from agent fan-out. 0 = unlimited.
    #[arg(long, env = env::MCP_MAX_CONCURRENCY, default_value = "8")]
    pub mcp_max_concurrency: usize,

    // ============================================
    // Authentication settings (server mode only)
    // ============================================
    /// Authentication mode: "none" or "keycloak"
    #[arg(long, env = env::AUTH_MODE, default_value = "none")]
    pub auth_mode: String,

    /// OIDC issuer URL (required for keycloak mode)
    #[arg(long, env = env::OIDC_ISSUER_URL)]
    pub oidc_issuer_url: Option<String>,

    /// OIDC client ID (required for keycloak mode)
    #[arg(long, env = env::OIDC_CLIENT_ID)]
    pub oidc_client_id: Option<String>,

    /// OIDC client secret (required for keycloak mode)
    #[arg(long, env = env::OIDC_CLIENT_SECRET)]
    pub oidc_client_secret: Option<String>,

    /// OIDC redirect URL (required for keycloak mode)
    #[arg(long, env = env::OIDC_REDIRECT_URL)]
    pub oidc_redirect_url: Option<String>,

    /// OIDC scopes (space-separated)
    #[arg(long, env = env::OIDC_SCOPES, default_value = "openid profile email groups")]
    pub oidc_scopes: String,

    // ============================================
    // RBAC settings (server mode only)
    // ============================================
    /// RBAC policy CSV (inline or file path)
    #[arg(long, env = env::RBAC_POLICY_CSV, default_value = "")]
    pub rbac_policy_csv: String,

    /// Default RBAC policy (applied when no matching rules found)
    #[arg(long, env = env::RBAC_DEFAULT_POLICY, default_value = "role:readonly")]
    pub rbac_default_policy: String,
}

impl Config {
    pub fn from_args() -> Self {
        Config::parse()
    }

    /// Validate configuration based on mode
    pub fn validate(&self) -> Result<(), String> {
        match self.mode {
            Mode::Scraper => {
                // Scraper reads reports by watching Kubernetes directly; no
                // server URL needed. HUB_SECRET_NAMESPACE may be empty in dev
                // (warns and skips the Secret watcher).
            }
            Mode::Server => {
                if self.scraper_url.trim().is_empty() {
                    return Err(format!(
                        "{} is required in server mode — the server holds no database",
                        env::SCRAPER_URL
                    ));
                }
                if self.auth_mode == "keycloak" {
                    crate::auth::config::validate_keycloak_config(
                        &self.oidc_issuer_url,
                        &self.oidc_client_id,
                        &self.oidc_client_secret,
                        &self.oidc_redirect_url,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Get cluster name
    pub fn get_cluster_name(&self) -> &str {
        &self.cluster_name
    }

    /// Get SQLite database path
    pub fn get_db_path(&self) -> String {
        format!("{}/trivy.db", self.storage_path)
    }
}

/// Fully-defaulted configuration for tests, independent of the ambient
/// environment — clap's `env` fallbacks would otherwise leak whatever the
/// developer or CI has exported into every test in the crate.
#[cfg(test)]
impl Config {
    pub fn for_test(mode: Mode) -> Self {
        Self {
            command: None,
            mode,
            log_format: "json".to_string(),
            log_level: "info".to_string(),
            health_port: 8080,
            cluster_name: "local".to_string(),
            namespaces: vec![],
            collect_vulnerability_reports: true,
            collect_sbom_reports: true,
            server_port: 3000,
            storage_path: "/data".to_string(),
            watch_local: true,
            internal_port: 8081,
            internal_token: "test-token".to_string(),
            scraper_url: "http://localhost:8081".to_string(),
            notes_configmap: "trivy-collector-notes".to_string(),
            api_tokens_secret: "trivy-collector-api-tokens".to_string(),
            hub_secret_namespace: String::new(),
            external_url: String::new(),
            mcp_enabled: false,
            mcp_allowed_hosts: vec![],
            mcp_stateless: false,
            mcp_max_concurrency: 8,
            auth_mode: "none".to_string(),
            oidc_issuer_url: None,
            oidc_client_id: None,
            oidc_client_secret: None,
            oidc_redirect_url: None,
            oidc_scopes: "openid profile email groups".to_string(),
            rbac_policy_csv: String::new(),
            rbac_default_policy: "role:readonly".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_scraper_mode() {
        // Scraper needs no mandatory config; empty hub namespace just warns.
        let config = Config::for_test(Mode::Scraper);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_server_mode() {
        let config = Config::for_test(Mode::Server);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_get_db_path() {
        let config = Config::for_test(Mode::Server);
        assert_eq!(config.get_db_path(), "/data/trivy.db");
    }

    #[test]
    fn test_get_db_path_custom() {
        let mut config = Config::for_test(Mode::Server);
        config.storage_path = "/tmp/custom".to_string();
        assert_eq!(config.get_db_path(), "/tmp/custom/trivy.db");
    }

    #[test]
    fn test_mode_display() {
        assert_eq!(Mode::Scraper.to_string(), "scraper");
        assert_eq!(Mode::Server.to_string(), "server");
    }

    #[test]
    fn test_get_cluster_name() {
        let config = Config::for_test(Mode::Scraper);
        assert_eq!(config.get_cluster_name(), "local");
    }

    #[test]
    fn test_validate_server_keycloak_missing_oidc() {
        let mut config = Config::for_test(Mode::Server);
        config.auth_mode = "keycloak".to_string();
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("OIDC_ISSUER_URL"));
    }

    #[test]
    fn test_validate_server_keycloak_all_present() {
        let mut config = Config::for_test(Mode::Server);
        config.auth_mode = "keycloak".to_string();
        config.oidc_issuer_url = Some("https://keycloak.example.com/realms/test".to_string());
        config.oidc_client_id = Some("trivy-collector".to_string());
        config.oidc_client_secret = Some("secret".to_string());
        config.oidc_redirect_url = Some("http://localhost:3000/auth/callback".to_string());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_server_requires_a_scraper_url() {
        let mut config = Config::for_test(Mode::Server);
        config.scraper_url = "  ".to_string();
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("SCRAPER_URL"));
    }

    #[test]
    fn test_validate_scraper_needs_no_scraper_url() {
        let mut config = Config::for_test(Mode::Scraper);
        config.scraper_url = String::new();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_server_auth_none() {
        let config = Config::for_test(Mode::Server);
        assert!(config.validate().is_ok());
    }
}
