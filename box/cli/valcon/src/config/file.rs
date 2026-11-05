use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::cli::Cli;
use crate::error::ConfigError;

/// One layer of settings. Every field is optional so layers can be stacked.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    pub context: Option<String>,
    pub namespaces: Option<Vec<String>>,
    pub all_namespaces: Option<bool>,
    pub selector: Option<String>,
    pub port: Option<u16>,
    pub aws_profile: Option<String>,
    pub aws_region: Option<String>,
    pub interval: Option<u64>,
    pub parallel: Option<usize>,
    pub exec_timeout: Option<u64>,
    pub probe_image: Option<String>,
    pub probe_timeout: Option<u64>,
}

impl Layer {
    /// Fields set in `top` win over `self`.
    #[must_use]
    pub fn overlay(self, top: Self) -> Self {
        Self {
            context: top.context.or(self.context),
            namespaces: top.namespaces.or(self.namespaces),
            all_namespaces: top.all_namespaces.or(self.all_namespaces),
            selector: top.selector.or(self.selector),
            port: top.port.or(self.port),
            aws_profile: top.aws_profile.or(self.aws_profile),
            aws_region: top.aws_region.or(self.aws_region),
            interval: top.interval.or(self.interval),
            parallel: top.parallel.or(self.parallel),
            exec_timeout: top.exec_timeout.or(self.exec_timeout),
            probe_image: top.probe_image.or(self.probe_image),
            probe_timeout: top.probe_timeout.or(self.probe_timeout),
        }
    }
}

impl From<&Cli> for Layer {
    fn from(cli: &Cli) -> Self {
        Self {
            context: cli.context.clone(),
            namespaces: cli.namespaces.clone(),
            all_namespaces: cli.all_namespaces.then_some(true),
            selector: cli.selector.clone(),
            port: cli.port,
            aws_profile: cli.aws_profile.clone(),
            aws_region: cli.aws_region.clone(),
            interval: cli.interval,
            parallel: cli.parallel,
            exec_timeout: cli.exec_timeout,
            probe_image: cli.probe_image.clone(),
            probe_timeout: cli.probe_timeout,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub default_target: Option<String>,
    #[serde(default)]
    pub defaults: Layer,
    #[serde(default)]
    pub targets: BTreeMap<String, Layer>,
}

impl FileConfig {
    pub fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source: Box::new(source),
        })
    }

    /// Reads `explicit`, or the default path when it exists.
    pub fn load(explicit: Option<&Path>) -> Result<Option<(PathBuf, Self)>, ConfigError> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => match default_path() {
                Some(p) if p.exists() => p,
                _ => return Ok(None),
            },
        };
        let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        let parsed = Self::parse(&text, &path)?;
        Ok(Some((path, parsed)))
    }

    /// The requested target, else `default_target`, else the only target.
    /// The requested target, else the target named after the AWS profile in use, else
    /// `default_target`, else the only target.
    pub fn target_name(
        &self,
        requested: Option<&str>,
        aws_profile: Option<&str>,
    ) -> Option<String> {
        requested
            .map(str::to_owned)
            .or_else(|| {
                aws_profile
                    .filter(|p| self.targets.contains_key(*p))
                    .map(str::to_owned)
            })
            .or_else(|| self.default_target.clone())
            .or_else(|| {
                if self.targets.len() == 1 {
                    self.targets.keys().next().cloned()
                } else {
                    None
                }
            })
    }

    pub fn target(&self, name: &str) -> Result<Layer, ConfigError> {
        self.targets
            .get(name)
            .cloned()
            .ok_or_else(|| ConfigError::UnknownTarget {
                name: name.to_owned(),
                available: self.targets.keys().cloned().collect::<Vec<_>>().join(", "),
            })
    }

    pub fn describe_targets(&self) -> Vec<String> {
        self.targets
            .iter()
            .map(|(name, t)| {
                let merged = self.defaults.clone().overlay(t.clone());
                let mark = if self.default_target.as_deref() == Some(name) {
                    " (default)"
                } else {
                    ""
                };
                let scope = if merged.all_namespaces == Some(true) {
                    "*".to_owned()
                } else {
                    merged
                        .namespaces
                        .map_or_else(|| "-".to_owned(), |n| n.join(","))
                };
                format!(
                    "{name}{mark}: {}/{scope} aws={}",
                    merged.context.as_deref().unwrap_or("current-context"),
                    merged.aws_profile.as_deref().unwrap_or("-"),
                )
            })
            .collect()
    }
}

pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join(env!("CARGO_PKG_NAME")).join("config.toml"))
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub const SAMPLE: &str = r#"
default_target = "orders"

[defaults]
aws_region = "us-east-1"
interval = 5

[targets.orders]
context = "prod"
namespaces = ["orders", "checkout"]
aws_profile = "prod"

[targets.billing]
namespaces = ["billing"]
interval = 30

[targets.everything]
all_namespaces = true
"#;

    pub fn sample() -> FileConfig {
        FileConfig::parse(SAMPLE, Path::new("config.toml")).unwrap()
    }

    #[test]
    fn overlay_prefers_top() {
        let base = Layer {
            namespaces: Some(vec!["a".into()]),
            port: Some(1),
            ..Layer::default()
        };
        let top = Layer {
            namespaces: Some(vec!["b".into()]),
            ..Layer::default()
        };
        let merged = base.overlay(top);
        assert_eq!(merged.namespaces, Some(vec!["b".into()]));
        assert_eq!(merged.port, Some(1));
    }

    #[test]
    fn target_name_fallbacks() {
        let f = sample();
        assert_eq!(
            f.target_name(Some("billing"), Some("everything"))
                .as_deref(),
            Some("billing")
        );
        assert_eq!(
            f.target_name(None, Some("everything")).as_deref(),
            Some("everything"),
            "profile picks the target"
        );
        assert_eq!(
            f.target_name(None, Some("unrelated")).as_deref(),
            Some("orders")
        );
        assert_eq!(f.target_name(None, None).as_deref(), Some("orders"));
        let single = FileConfig::parse("[targets.only]\n", Path::new("c")).unwrap();
        assert_eq!(single.target_name(None, None).as_deref(), Some("only"));
        assert_eq!(FileConfig::default().target_name(None, None), None);
    }

    #[test]
    fn unknown_target_lists_available() {
        let err = sample().target("nope").unwrap_err().to_string();
        assert!(err.contains("billing, everything, orders"), "{err}");
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(matches!(
            FileConfig::parse("bogus = 1", Path::new("c.toml")),
            Err(ConfigError::Parse { .. })
        ));
        assert!(FileConfig::parse("[targets.x]\nnamespace = \"typo\"\n", Path::new("c")).is_err());
    }

    #[test]
    fn lists_targets_with_default_mark() {
        let lines = sample().describe_targets();
        assert_eq!(lines.len(), 3);
        assert!(
            lines.contains(&"orders (default): prod/orders,checkout aws=prod".to_owned()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"billing: current-context/billing aws=-".to_owned()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"everything: current-context/* aws=-".to_owned()),
            "{lines:?}"
        );
    }

    #[test]
    fn loads_explicit_file_and_reports_missing() {
        let dir = std::env::temp_dir().join(format!("vct-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, SAMPLE).unwrap();
        let (p, f) = FileConfig::load(Some(&path)).unwrap().unwrap();
        assert_eq!(p, path);
        assert_eq!(f.targets.len(), 3);
        assert!(matches!(
            FileConfig::load(Some(&dir.join("missing.toml"))),
            Err(ConfigError::Read { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn default_path_ends_with_crate_name() {
        let p = default_path().unwrap();
        assert!(p.ends_with("valcon/config.toml"));
    }
}
