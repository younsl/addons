//! Loads and validates the gate configuration.
//!
//! Everything that varies per deployment lives in a YAML file mounted from a
//! `ConfigMap`. The process flags cover only wiring (listen addresses, TLS
//! material, log settings).

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Decides the verdict when a check cannot be evaluated because a dependency
/// failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnError {
    Allow,
    Deny,
}

impl OnError {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

/// Decides whether a failing image comparison denies the sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageTagMode {
    /// Denies the sync on a tag mismatch.
    Enforce,
    /// Allows the sync, attaches a warning, and counts the mismatch. Use it to
    /// observe the blast radius before enforcing.
    Warn,
}

impl ImageTagMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enforce => "enforce",
            Self::Warn => "warn",
        }
    }
}

/// The upstream status conditions that must hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Require {
    /// Requires the upstream to report `status.sync.status: Synced`.
    pub sync: bool,
    /// Requires the upstream to report `status.health.status: Healthy`.
    pub health: bool,
}

impl Default for Require {
    fn default() -> Self {
        Self {
            sync: true,
            health: true,
        }
    }
}

/// Configures the image tag comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct ImageTag {
    /// Compares image tags on top of the upstream sync and health checks.
    pub enabled: bool,
    pub mode: ImageTagMode,
    /// The workload kinds queried for desired images.
    pub kinds: Vec<String>,
    /// Repository basenames excluded from comparison. A trailing `*` globs, so
    /// `autoinstrumentation-*` covers a sidecar family.
    pub ignore_repos: Vec<String>,
    /// The verdict when the desired image lookup fails.
    pub on_error: OnError,
}

impl Default for ImageTag {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: ImageTagMode::Warn,
            kinds: [
                "Deployment",
                "StatefulSet",
                "DaemonSet",
                "CronJob",
                "Rollout",
            ]
            .iter()
            .map(ToString::to_string)
            .collect(),
            ignore_repos: Vec::new(),
            on_error: OnError::Deny,
        }
    }
}

/// Configures the rollback allowance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct Rollback {
    /// Lets a sync through when its target revision is one this Application
    /// has already deployed, which is what a rollback is.
    ///
    /// This cannot introduce anything new into the environment: the revision
    /// was running here before, so every image it carries has already been
    /// through whatever checks applied at the time. Refusing it would leave an
    /// incident with no way back except an annotation, which is the wrong
    /// thing to ask of somebody at 3am.
    pub allow_previously_deployed_revision: bool,
}

impl Default for Rollback {
    fn default() -> Self {
        Self {
            allow_previously_deployed_revision: true,
        }
    }
}

/// The ways a sync bypasses the gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Exempt {
    /// Principals whose sync requests bypass the gate. The Argo CD application
    /// controller belongs here so an auto-sync is never blocked mid-reconcile,
    /// where the denial would only produce a retry loop.
    pub usernames: Vec<String>,
    /// Bypasses the gate when Argo CD marks the operation automated.
    pub automated: bool,
    /// Opts one Application out of the gate when set to `"true"`.
    pub annotation: String,
}

impl Default for Exempt {
    fn default() -> Self {
        Self {
            usernames: vec![
                "system:serviceaccount:argocd:argocd-application-controller".to_string(),
            ],
            automated: true,
            annotation: "promotion-gate.younsl.github.io/skip".to_string(),
        }
    }
}

/// Points at the Argo CD installation the gate reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct ArgoCd {
    /// Holds the Application resources.
    pub namespace: String,
    /// The base URL of argocd-server, used for the desired image lookup that
    /// the Kubernetes API cannot answer.
    ///
    /// Argo CD's self-signed serving certificate carries SANs for `localhost`
    /// and `argocd-server` only, so the in-namespace short name verifies while
    /// the fully qualified service name does not.
    pub server_address: String,
    /// The PEM bundle that signs the argocd-server certificate. Mounting
    /// argocd-secret's `tls.crt` is enough, since that certificate is its own
    /// issuer.
    pub ca_file: String,
    /// Disables TLS verification against argocd-server. It exists only for
    /// clusters that terminate TLS elsewhere. Prefer `ca_file`, because the
    /// token this client sends is a full Argo CD API credential.
    pub insecure_skip_verify: bool,
    pub token_path: String,
    /// Bounds each argocd-server call. It must stay well under the webhook's
    /// own timeout.
    pub timeout_seconds: i64,
    /// How long a desired image lookup is reused.
    #[serde(rename = "cacheTtlSeconds")]
    pub cache_ttl_seconds: i64,
}

impl Default for ArgoCd {
    fn default() -> Self {
        Self {
            namespace: "argocd".to_string(),
            server_address: "https://argocd-server".to_string(),
            ca_file: "/etc/argocd-promotion-gate/argocd-ca/tls.crt".to_string(),
            insecure_skip_verify: false,
            token_path: "/etc/argocd-promotion-gate/token/token".to_string(),
            timeout_seconds: 3,
            cache_ttl_seconds: 30,
        }
    }
}

/// The full gate configuration.
///
/// `imageTag.mode` defaults to warn rather than enforce: turning tag equality
/// on in an estate that has never enforced it can block every pending
/// production sync at once, so the safe default reports first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct Config {
    /// The promotion order, lowest environment first. The upstream of an
    /// environment is its predecessor in this list.
    pub chain: Vec<String>,
    /// The environments the gate enforces. Empty means every environment in
    /// the chain except the head.
    pub gated_envs: Vec<String>,
    pub require: Require,
    pub image_tag: ImageTag,
    pub rollback: Rollback,
    pub exempt: Exempt,
    pub argocd: ArgoCd,
}

/// Why a configuration was rejected.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("read gate config {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("gate config {path}: parse: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("gate config {path}: {reason}")]
    Invalid { path: String, reason: String },
}

impl Config {
    /// Reads and validates the configuration file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let display = path.display().to_string();
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: display.clone(),
            source,
        })?;
        Self::parse(&raw).map_err(|err| match err {
            ConfigError::Parse { source, .. } => ConfigError::Parse {
                path: display.clone(),
                source,
            },
            ConfigError::Invalid { reason, .. } => ConfigError::Invalid {
                path: display.clone(),
                reason,
            },
            other @ ConfigError::Read { .. } => other,
        })
    }

    /// Decodes the configuration on top of the defaults and validates it.
    ///
    /// Unknown fields are rejected so a misspelled key surfaces at startup
    /// instead of silently leaving a check disabled.
    pub fn parse(raw: &str) -> Result<Self, ConfigError> {
        let cfg: Self = serde_yaml::from_str(raw).map_err(|source| ConfigError::Parse {
            path: "<inline>".to_string(),
            source,
        })?;
        cfg.validate().map_err(|reason| ConfigError::Invalid {
            path: "<inline>".to_string(),
            reason,
        })?;
        Ok(cfg)
    }

    /// Rejects configurations that cannot enforce anything, so a typo fails
    /// startup instead of quietly allowing every sync.
    pub fn validate(&self) -> Result<(), String> {
        if self.chain.len() < 2 {
            return Err(format!(
                "chain needs at least two environments, got {}: a chain without a predecessor has no upstream to gate on",
                self.chain.len()
            ));
        }

        let mut seen = HashSet::with_capacity(self.chain.len());
        for env in &self.chain {
            if env.trim().is_empty() {
                return Err("chain contains an empty entry".to_string());
            }
            if !seen.insert(env.as_str()) {
                return Err(format!("chain contains duplicate env {env:?}"));
            }
        }

        for env in &self.gated_envs {
            if !seen.contains(env.as_str()) {
                return Err(format!(
                    "gatedEnvs entry {env:?} is not present in chain {:?}",
                    self.chain
                ));
            }
            if env == &self.chain[0] {
                return Err(format!(
                    "gatedEnvs entry {env:?} is the chain head and has no upstream to gate on"
                ));
            }
        }

        if self.image_tag.enabled {
            if self.image_tag.kinds.is_empty() {
                return Err(
                    "imageTag.kinds must not be empty when imageTag.enabled is true".to_string(),
                );
            }
            if self.argocd.server_address.trim().is_empty() {
                return Err(
                    "argocd.serverAddress is required when imageTag.enabled is true".to_string(),
                );
            }
        }

        if self.exempt.annotation.trim().is_empty() {
            return Err("exempt.annotation must not be empty".to_string());
        }
        if self.argocd.namespace.trim().is_empty() {
            return Err("argocd.namespace must not be empty".to_string());
        }
        if self.argocd.timeout_seconds <= 0 {
            return Err(format!(
                "argocd.timeoutSeconds must be greater than 0, got {}",
                self.argocd.timeout_seconds
            ));
        }
        if self.argocd.cache_ttl_seconds < 0 {
            return Err(format!(
                "argocd.cacheTtlSeconds must not be negative, got {}",
                self.argocd.cache_ttl_seconds
            ));
        }
        Ok(())
    }

    /// The environment that must be promoted before `env` may sync, or `None`
    /// when `env` is outside the chain or is the chain head.
    #[must_use]
    pub fn upstream_env(&self, env: &str) -> Option<&str> {
        let idx = self.chain.iter().position(|candidate| candidate == env)?;
        if idx == 0 {
            return None;
        }
        Some(self.chain[idx - 1].as_str())
    }

    /// Reports whether the gate enforces anything for `env`.
    ///
    /// An empty `gated_envs` means "every environment in the chain except the
    /// head", which is the useful reading for a fully ordered promotion chain.
    #[must_use]
    pub fn is_gated(&self, env: &str) -> bool {
        if self.upstream_env(env).is_none() {
            return false;
        }
        self.gated_envs.is_empty() || self.gated_envs.iter().any(|g| g == env)
    }

    /// The environments the gate actually enforces, which with an empty list
    /// is the whole chain except its head.
    #[must_use]
    pub fn gated_envs(&self) -> Vec<String> {
        if !self.gated_envs.is_empty() {
            return self.gated_envs.clone();
        }
        self.chain.iter().skip(1).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_applies_defaults_under_partial_input() {
        let cfg = Config::parse("chain: [dev, stg, prd]\nimageTag:\n  mode: enforce\n").unwrap();
        assert_eq!(cfg.chain, vec!["dev", "stg", "prd"]);
        assert!(cfg.require.sync);
        assert!(cfg.image_tag.enabled);
        assert_eq!(cfg.image_tag.mode, ImageTagMode::Enforce);
        assert_eq!(cfg.image_tag.on_error, OnError::Deny);
        assert_eq!(cfg.image_tag.kinds.len(), 5);
        assert!(cfg.rollback.allow_previously_deployed_revision);
        assert_eq!(cfg.argocd.namespace, "argocd");
        assert_eq!(cfg.argocd.cache_ttl_seconds, 30);
        assert_eq!(cfg.exempt.usernames.len(), 1);
        assert_eq!(OnError::Allow.as_str(), "allow");
        assert_eq!(ImageTagMode::Warn.as_str(), "warn");
    }

    #[test]
    fn parse_rejects_unknown_fields_and_bad_enums() {
        let err = Config::parse("chain: [a, b]\nrequire:\n  synced: true\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        let err = Config::parse("chain: [a, b]\nimageTag:\n  mode: maybe\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        let err = Config::parse("chain: [a, b]\nimageTag:\n  onError: shrug\n").unwrap_err();
        assert!(err.to_string().contains("parse"), "{err}");
    }

    #[test]
    fn validate_rejects_broken_chains() {
        let cases: &[(&str, &str)] = &[
            ("chain: [prd]", "at least two"),
            ("chain: [a, '']", "empty entry"),
            ("chain: [a, a]", "duplicate"),
            ("chain: [a, b]\ngatedEnvs: [c]", "not present"),
            ("chain: [a, b]\ngatedEnvs: [a]", "chain head"),
            (
                "chain: [a, b]\nimageTag:\n  kinds: []",
                "kinds must not be empty",
            ),
            (
                "chain: [a, b]\nargocd:\n  serverAddress: ''",
                "serverAddress is required",
            ),
            (
                "chain: [a, b]\nexempt:\n  annotation: ' '",
                "annotation must not be empty",
            ),
            (
                "chain: [a, b]\nargocd:\n  namespace: ''",
                "namespace must not be empty",
            ),
            (
                "chain: [a, b]\nargocd:\n  timeoutSeconds: 0",
                "timeoutSeconds",
            ),
            (
                "chain: [a, b]\nargocd:\n  cacheTtlSeconds: -1",
                "cacheTtlSeconds",
            ),
        ];
        for (raw, want) in cases {
            let err = Config::parse(raw).unwrap_err().to_string();
            assert!(err.contains(want), "{raw}: got {err}");
        }
    }

    #[test]
    fn image_tag_disabled_skips_argocd_checks() {
        let cfg = Config::parse("chain: [a, b]\nimageTag:\n  enabled: false\n  kinds: []\nargocd:\n  serverAddress: ''\n").unwrap();
        assert!(!cfg.image_tag.enabled);
    }

    #[test]
    fn upstream_and_gating() {
        let cfg = Config::parse("chain: [dev, stg, prd]\n").unwrap();
        assert_eq!(cfg.upstream_env("dev"), None);
        assert_eq!(cfg.upstream_env("stg"), Some("dev"));
        assert_eq!(cfg.upstream_env("prd"), Some("stg"));
        assert_eq!(cfg.upstream_env("qa"), None);
        assert!(!cfg.is_gated("dev"));
        assert!(cfg.is_gated("stg"));
        assert!(cfg.is_gated("prd"));
        assert!(!cfg.is_gated("qa"));
        assert_eq!(cfg.gated_envs(), vec!["stg", "prd"]);

        let cfg = Config::parse("chain: [dev, stg, prd]\ngatedEnvs: [prd]\n").unwrap();
        assert!(!cfg.is_gated("stg"));
        assert!(cfg.is_gated("prd"));
        assert_eq!(cfg.gated_envs(), vec!["prd"]);
    }

    #[test]
    fn load_reports_missing_and_invalid_files() {
        let err = Config::load("/nonexistent/gate.yaml").unwrap_err();
        assert!(matches!(err, ConfigError::Read { .. }), "{err}");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gate.yaml");
        std::fs::write(&path, "chain: [prd]\n").unwrap();
        let err = Config::load(&path).unwrap_err().to_string();
        assert!(
            err.contains("gate.yaml") && err.contains("at least two"),
            "{err}"
        );

        std::fs::write(&path, "chain: [a, b]\nbogus: 1\n").unwrap();
        let err = Config::load(&path).unwrap_err().to_string();
        assert!(err.contains("parse"), "{err}");

        std::fs::write(&path, "chain: [stg, prd]\n").unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.chain, vec!["stg", "prd"]);
    }
}
