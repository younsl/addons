mod cli;
mod file;

use std::time::Duration;

pub use cli::{Cli, LogFormat};
pub use file::{FileConfig, Layer, default_path};

use crate::error::ConfigError;

const DEFAULT_PORT: u16 = 6379;
const DEFAULT_INTERVAL: u64 = 15;
const DEFAULT_PARALLEL: usize = 10;
const DEFAULT_EXEC_TIMEOUT: u64 = 10;
const DEFAULT_PROBE_IMAGE: &str = "valkey/valkey:9.1.3-alpine";
const DEFAULT_PROBE_TIMEOUT: u64 = 60;

/// Fully resolved runtime settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub target: Option<String>,
    pub context: Option<String>,
    /// Empty means the kubeconfig context namespace, unless `all_namespaces` is set.
    pub namespaces: Vec<String>,
    pub all_namespaces: bool,
    pub selector: Option<String>,
    pub port: u16,
    pub aws_profile: Option<String>,
    pub aws_region: Option<String>,
    pub interval: Duration,
    pub parallel: usize,
    pub exec_timeout: Duration,
    pub probe_image: String,
    pub probe_timeout: Duration,
}

/// Precedence, highest first: flag or its env var, the selected target, `[defaults]`, built-in.
pub fn resolve(cli: &Cli, file: Option<&FileConfig>) -> Result<Settings, ConfigError> {
    let empty = FileConfig::default();
    let file = file.unwrap_or(&empty);

    let target = file.target_name(cli.target.as_deref(), cli.aws_profile.as_deref());
    let target_layer = match &target {
        Some(name) => file.target(name)?,
        None => Layer::default(),
    };
    let p = file
        .defaults
        .clone()
        .overlay(target_layer)
        .overlay(Layer::from(cli));

    Ok(Settings {
        target,
        context: p.context,
        namespaces: p.namespaces.unwrap_or_default(),
        all_namespaces: p.all_namespaces.unwrap_or(false),
        selector: p.selector,
        port: nonzero(p.port.unwrap_or(DEFAULT_PORT), "port")?,
        aws_profile: p.aws_profile,
        aws_region: p.aws_region,
        interval: Duration::from_secs(nonzero(p.interval.unwrap_or(DEFAULT_INTERVAL), "interval")?),
        parallel: nonzero(p.parallel.unwrap_or(DEFAULT_PARALLEL), "parallel")?,
        exec_timeout: Duration::from_secs(nonzero(
            p.exec_timeout.unwrap_or(DEFAULT_EXEC_TIMEOUT),
            "exec-timeout",
        )?),
        probe_image: p.probe_image.unwrap_or_else(|| DEFAULT_PROBE_IMAGE.into()),
        probe_timeout: Duration::from_secs(nonzero(
            p.probe_timeout.unwrap_or(DEFAULT_PROBE_TIMEOUT),
            "probe-timeout",
        )?),
    })
}

fn nonzero<T: Default + PartialEq>(v: T, name: &'static str) -> Result<T, ConfigError> {
    if v == T::default() {
        Err(ConfigError::ZeroValue(name))
    } else {
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::file::tests::sample;
    use super::*;

    #[test]
    fn default_target_merges_defaults() {
        let s = resolve(&Cli::default(), Some(&sample())).unwrap();
        assert_eq!(s.target.as_deref(), Some("orders"));
        assert_eq!(s.context.as_deref(), Some("prod"));
        assert_eq!(s.namespaces, ["orders", "checkout"]);
        assert_eq!(s.aws_region.as_deref(), Some("us-east-1"));
        assert_eq!(s.interval, Duration::from_secs(5));
        assert_eq!((s.port, s.parallel), (DEFAULT_PORT, DEFAULT_PARALLEL));
        assert!(!s.all_namespaces);
    }

    #[test]
    fn flags_override_target() {
        let cli = Cli {
            target: Some("billing".into()),
            namespaces: Some(vec!["billing-canary".into()]),
            aws_profile: Some("staging".into()),
            ..Cli::default()
        };
        let s = resolve(&cli, Some(&sample())).unwrap();
        assert_eq!(s.namespaces, ["billing-canary"]);
        assert_eq!(s.interval, Duration::from_secs(30));
        assert_eq!(s.aws_profile.as_deref(), Some("staging"));
        let all = Cli {
            target: Some("everything".into()),
            ..Cli::default()
        };
        assert!(resolve(&all, Some(&sample())).unwrap().all_namespaces);
    }

    #[test]
    fn works_without_file() {
        let s = resolve(&Cli::default(), None).unwrap();
        assert!(s.namespaces.is_empty() && s.target.is_none());
        assert_eq!(s.probe_image, DEFAULT_PROBE_IMAGE);
        assert_eq!(s.probe_timeout, Duration::from_secs(DEFAULT_PROBE_TIMEOUT));
        assert_eq!(s.interval, Duration::from_secs(DEFAULT_INTERVAL));
    }

    #[test]
    fn rejects_bad_input() {
        let unknown = Cli {
            target: Some("nope".into()),
            ..Cli::default()
        };
        assert!(matches!(
            resolve(&unknown, Some(&sample())),
            Err(ConfigError::UnknownTarget { .. })
        ));
        let zero = Cli {
            parallel: Some(0),
            ..Cli::default()
        };
        assert!(matches!(
            resolve(&zero, None),
            Err(ConfigError::ZeroValue("parallel"))
        ));
    }
}
