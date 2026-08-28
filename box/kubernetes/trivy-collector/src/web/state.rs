//! Application state for the server tier.
//!
//! Nothing here owns a database. Reports arrive through a `ReportStore` (a
//! `RemoteStore` in production), notes and API tokens come from watched
//! Kubernetes objects, and request logs go to stdout. That is what makes the
//! server pod disposable and lets it run more than one replica.

use std::sync::Arc;
use std::time::Instant;

use crate::alerts::AlertEvaluator;
use crate::auth::AuthState;
use crate::auth::rbac::RbacPolicy;
use crate::config::Config;
use crate::metrics::Metrics;
use crate::storage::{NotesStore, ReportStore, TokenStore};

/// Runtime configuration info (subset of Config for API exposure)
#[derive(Clone)]
pub struct ConfigInfo {
    pub mode: String,
    pub log_format: String,
    pub log_level: String,
    pub health_port: u16,
    pub cluster_name: String,
    pub namespaces: Vec<String>,
    pub collect_vulnerability_reports: bool,
    pub collect_sbom_reports: bool,
    pub server_port: u16,
    pub scraper_url: String,
    pub watch_local: bool,
    pub hub_secret_namespace: String,
    pub auth_mode: Option<String>,
    pub mcp_enabled: bool,
}

impl From<&Config> for ConfigInfo {
    fn from(config: &Config) -> Self {
        let auth_mode = if config.auth_mode == "none" {
            None
        } else {
            Some(config.auth_mode.clone())
        };

        Self {
            mode: config.mode.to_string(),
            log_format: config.log_format.clone(),
            log_level: config.log_level.clone(),
            health_port: config.health_port,
            cluster_name: config.cluster_name.clone(),
            namespaces: config.namespaces.clone(),
            collect_vulnerability_reports: config.collect_vulnerability_reports,
            collect_sbom_reports: config.collect_sbom_reports,
            server_port: config.server_port,
            scraper_url: config.scraper_url.clone(),
            watch_local: config.watch_local,
            hub_secret_namespace: config.hub_secret_namespace.clone(),
            auth_mode,
            mcp_enabled: config.mcp_enabled,
        }
    }
}

/// Runtime information collected at server startup
#[derive(Clone)]
pub struct RuntimeInfo {
    pub start_time: Instant,
    pub hostname: String,
}

impl RuntimeInfo {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            hostname: hostname::get()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_else(|_| "unknown".to_string()),
        }
    }

    /// Get uptime as human-readable string
    pub fn uptime_string(&self) -> String {
        format_uptime(self.start_time.elapsed().as_secs())
    }
}

/// Render a duration in seconds as `1d 2h 3m 4s`, dropping leading zero units.
fn format_uptime(total_secs: u64) -> String {
    let days = total_secs / 86400;
    let hours = (total_secs % 86400) / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;

    if days > 0 {
        format!("{}d {}h {}m {}s", days, hours, minutes, seconds)
    } else if hours > 0 {
        format!("{}h {}m {}s", hours, minutes, seconds)
    } else if minutes > 0 {
        format!("{}m {}s", minutes, seconds)
    } else {
        format!("{}s", seconds)
    }
}

impl Default for RuntimeInfo {
    fn default() -> Self {
        Self::new()
    }
}

/// Application state shared across handlers
#[derive(Clone)]
pub struct AppState {
    /// Report reads and writes. `RemoteStore` in production.
    pub store: Arc<dyn ReportStore>,
    pub config: Arc<ConfigInfo>,
    pub runtime: Arc<RuntimeInfo>,
    /// Authentication state (None when auth_mode == "none")
    pub auth: Option<Arc<AuthState>>,
    /// RBAC policy engine
    pub rbac: Arc<RbacPolicy>,
    /// Prometheus metrics
    pub metrics: Arc<Metrics>,
    /// Alert evaluator (None when Kubernetes API unavailable or alerts disabled)
    pub alerts: Option<Arc<AlertEvaluator>>,
    /// ConfigMap-backed report notes (None when the Kubernetes API is
    /// unavailable, in which case notes are read-only and empty).
    pub notes: Option<Arc<NotesStore>>,
    /// Secret-backed API tokens (None when the Kubernetes API is unavailable,
    /// in which case Bearer-token auth is unavailable).
    pub tokens: Option<Arc<TokenStore>>,
}

impl AppState {
    /// Join stored notes onto a page of report metadata. A missing notes store
    /// leaves the fields empty rather than failing the read.
    pub fn merge_notes(&self, metas: &mut [crate::storage::ReportMeta]) {
        if let Some(notes) = &self.notes {
            notes.cache().merge_all(metas);
        }
    }

    /// Join a stored note onto one report's metadata.
    pub fn merge_note(&self, meta: &mut crate::storage::ReportMeta) {
        if let Some(notes) = &self.notes {
            notes.cache().merge_meta(meta);
        }
    }

    /// Publish how many API tokens the Secret-backed store holds.
    pub fn record_token_count(&self) {
        if let (Some(tokens), Some(gauge)) = (&self.tokens, &self.metrics.api_tokens_total) {
            gauge.set(tokens.cache().len() as i64);
        }
    }

    /// Publish the notes ConfigMap's current size. The 1MiB object limit is a
    /// hard wall, so its headroom is worth a gauge rather than a surprise.
    pub fn record_notes_size(&self) {
        if let (Some(notes), Some(gauge)) = (&self.notes, &self.metrics.notes_configmap_bytes) {
            gauge.set(notes.cache().bytes() as i64);
        }
    }
}

/// Allow axum-extra PrivateCookieJar to extract the cookie Key from AppState
impl axum::extract::FromRef<AppState> for cookie::Key {
    fn from_ref(state: &AppState) -> Self {
        state
            .auth
            .as_ref()
            .map(|a| a.cookie_key.clone())
            .unwrap_or_else(cookie::Key::generate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(mode: crate::config::Mode) -> Config {
        Config::for_test(mode)
    }

    #[test]
    fn format_uptime_drops_leading_zero_units() {
        assert_eq!(format_uptime(0), "0s");
        assert_eq!(format_uptime(45), "45s");
        assert_eq!(format_uptime(90), "1m 30s");
        assert_eq!(format_uptime(3661), "1h 1m 1s");
        assert_eq!(format_uptime(90061), "1d 1h 1m 1s");
    }

    #[test]
    fn runtime_info_starts_at_zero_with_a_hostname() {
        let runtime = RuntimeInfo::new();
        assert_eq!(runtime.uptime_string(), "0s");
        assert!(!runtime.hostname.is_empty());
    }

    #[test]
    fn runtime_info_default_matches_new() {
        let runtime = RuntimeInfo::default();
        assert!(!runtime.hostname.is_empty());
        assert_eq!(runtime.uptime_string(), "0s");
    }

    #[test]
    fn config_info_carries_the_server_facing_subset() {
        let mut c = config(crate::config::Mode::Server);
        c.log_level = "debug".to_string();
        c.health_port = 9090;
        c.cluster_name = "my-cluster".to_string();
        c.namespaces = vec!["ns1".to_string()];
        c.collect_sbom_reports = false;
        c.server_port = 8080;
        c.scraper_url = "http://scraper:8081".to_string();
        c.auth_mode = "keycloak".to_string();

        let info = ConfigInfo::from(&c);
        assert_eq!(info.mode, "server");
        assert_eq!(info.log_level, "debug");
        assert_eq!(info.health_port, 9090);
        assert_eq!(info.cluster_name, "my-cluster");
        assert_eq!(info.namespaces, vec!["ns1"]);
        assert!(info.collect_vulnerability_reports);
        assert!(!info.collect_sbom_reports);
        assert_eq!(info.server_port, 8080);
        assert_eq!(info.scraper_url, "http://scraper:8081");
        assert_eq!(info.auth_mode, Some("keycloak".to_string()));
    }

    #[test]
    fn auth_mode_none_is_reported_as_absent() {
        let mut c = config(crate::config::Mode::Scraper);
        c.auth_mode = "none".to_string();
        let info = ConfigInfo::from(&c);
        assert_eq!(info.mode, "scraper");
        assert!(info.auth_mode.is_none());
    }
}
