//! Loads and validates the gate configuration.
//!
//! Everything that varies per deployment lives in a YAML file mounted from a
//! `ConfigMap`. The process flags cover only wiring (listen addresses, TLS
//! material, log settings).

use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Decides whether an in-progress canary denies the sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Denies the sync while a Rollout update is in flight.
    Enforce,
    /// Allows the sync, attaches a warning, and counts the hit. Use it to
    /// observe the blast radius before enforcing.
    Warn,
}

impl Mode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enforce => "enforce",
            Self::Warn => "warn",
        }
    }
}

/// Decides the verdict when the Rollout list itself fails.
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
            annotation: "canary-gate.younsl.github.io/skip".to_string(),
        }
    }
}

/// Points at the Argo CD installation the gate protects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct ArgoCd {
    /// Holds the Application resources. Denial Events are written here.
    pub namespace: String,
}

impl Default for ArgoCd {
    fn default() -> Self {
        Self {
            namespace: "argocd".to_string(),
        }
    }
}

/// How Rollouts are located and which strategies the gate watches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct Rollouts {
    /// The label tying a Rollout to its Application. Argo CD stamps every
    /// managed resource with its tracking label, whose value is the
    /// Application name.
    pub tracking_label: String,
    /// The strategies the gate watches. A Rollout using any other strategy is
    /// ignored.
    pub strategies: Vec<String>,
}

impl Default for Rollouts {
    fn default() -> Self {
        Self {
            tracking_label: "argocd.argoproj.io/instance".to_string(),
            strategies: vec!["canary".to_string()],
        }
    }
}

/// The full gate configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct Config {
    pub mode: Mode,
    /// The verdict when the Rollout list fails. `deny` keeps a broken lookup
    /// from silently opening the gate.
    pub on_error: OnError,
    pub exempt: Exempt,
    pub argocd: ArgoCd,
    pub rollouts: Rollouts,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Enforce,
            on_error: OnError::Deny,
            exempt: Exempt::default(),
            argocd: ArgoCd::default(),
            rollouts: Rollouts::default(),
        }
    }
}

/// The strategies the gate knows how to judge.
const KNOWN_STRATEGIES: [&str; 2] = ["canary", "blueGreen"];

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
        if self.rollouts.tracking_label.trim().is_empty() {
            return Err("rollouts.trackingLabel must not be empty".to_string());
        }
        if self.rollouts.strategies.is_empty() {
            return Err("rollouts.strategies must not be empty".to_string());
        }
        for strategy in &self.rollouts.strategies {
            if !KNOWN_STRATEGIES.contains(&strategy.as_str()) {
                return Err(format!(
                    "rollouts.strategies entry {strategy:?} is not one of {KNOWN_STRATEGIES:?}"
                ));
            }
        }
        if self.exempt.annotation.trim().is_empty() {
            return Err("exempt.annotation must not be empty".to_string());
        }
        if self.argocd.namespace.trim().is_empty() {
            return Err("argocd.namespace must not be empty".to_string());
        }
        Ok(())
    }

    /// Reports whether the named strategy is one the gate watches.
    #[must_use]
    pub fn watches_strategy(&self, strategy: &str) -> bool {
        self.rollouts.strategies.iter().any(|s| s == strategy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_applies_defaults_under_partial_input() {
        let cfg = Config::parse("mode: warn\n").unwrap();
        assert_eq!(cfg.mode, Mode::Warn);
        assert_eq!(cfg.on_error, OnError::Deny);
        assert_eq!(cfg.rollouts.tracking_label, "argocd.argoproj.io/instance");
        assert_eq!(cfg.rollouts.strategies, vec!["canary"]);
        assert_eq!(cfg.argocd.namespace, "argocd");
        assert_eq!(cfg.exempt.usernames.len(), 1);
        assert!(cfg.exempt.automated);
        assert_eq!(cfg.exempt.annotation, "canary-gate.younsl.github.io/skip");
        assert_eq!(Mode::Enforce.as_str(), "enforce");
        assert_eq!(Mode::Warn.as_str(), "warn");
        assert_eq!(OnError::Allow.as_str(), "allow");
        assert_eq!(OnError::Deny.as_str(), "deny");
    }

    #[test]
    fn empty_input_is_the_default_policy() {
        let cfg = Config::parse("{}\n").unwrap();
        assert_eq!(cfg, Config::default());
        assert!(cfg.watches_strategy("canary"));
        assert!(!cfg.watches_strategy("blueGreen"));
    }

    #[test]
    fn parse_rejects_unknown_fields_and_bad_enums() {
        let err = Config::parse("mode: maybe\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        let err = Config::parse("onError: shrug\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        let err = Config::parse("bogus: 1\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        let err = Config::parse("rollouts:\n  trackingLabels: x\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
    }

    #[test]
    fn validate_rejects_unenforceable_configs() {
        let cases: &[(&str, &str)] = &[
            (
                "rollouts:\n  trackingLabel: ' '",
                "trackingLabel must not be empty",
            ),
            (
                "rollouts:\n  strategies: []",
                "strategies must not be empty",
            ),
            ("rollouts:\n  strategies: [recreate]", "is not one of"),
            ("exempt:\n  annotation: ' '", "annotation must not be empty"),
            ("argocd:\n  namespace: ''", "namespace must not be empty"),
        ];
        for (raw, want) in cases {
            let err = Config::parse(raw).unwrap_err().to_string();
            assert!(err.contains(want), "{raw}: got {err}");
        }
    }

    #[test]
    fn blue_green_is_accepted_when_asked_for() {
        let cfg = Config::parse("rollouts:\n  strategies: [canary, blueGreen]\n").unwrap();
        assert!(cfg.watches_strategy("blueGreen"));
    }

    #[test]
    fn load_reports_missing_and_invalid_files() {
        let err = Config::load("/nonexistent/gate.yaml").unwrap_err();
        assert!(matches!(err, ConfigError::Read { .. }), "{err}");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gate.yaml");
        std::fs::write(&path, "rollouts:\n  strategies: []\n").unwrap();
        let err = Config::load(&path).unwrap_err().to_string();
        assert!(
            err.contains("gate.yaml") && err.contains("must not be empty"),
            "{err}"
        );

        std::fs::write(&path, "bogus: 1\n").unwrap();
        let err = Config::load(&path).unwrap_err().to_string();
        assert!(err.contains("parse"), "{err}");

        std::fs::write(&path, "mode: enforce\n").unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.mode, Mode::Enforce);
    }
}
