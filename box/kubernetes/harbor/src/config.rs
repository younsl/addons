use std::fmt;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::error::{Error, Result};

/// The Harbor release this build targets. Changing the VERSION file is what
/// triggers a release in CI.
const DEFAULT_HARBOR_VERSION: &str = include_str!("../VERSION");

pub const DEFAULT_HARBOR_REPO: &str = "https://github.com/goharbor/harbor.git";
pub const DEFAULT_REGISTRY: &str = "ghcr.io/younsl/harbor";

#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Cli {
    /// Enable debug logging
    #[arg(short, long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Build, verify and optionally push every chart image for linux/arm64
    Build(BuildArgs),
    /// Print the chart image names, one per line
    Images,
    /// Print harbor-helm values that point every image at the arm64 builds
    Values {
        /// Image prefix the arm64 images were pushed to
        #[arg(long, env = "REGISTRY_PREFIX", default_value = DEFAULT_REGISTRY)]
        registry: String,
    },
    /// Print the Harbor version this binary targets
    HarborVersion,
}

#[derive(Debug, Args)]
pub struct BuildArgs {
    /// Harbor git tag to build (default: the VERSION file)
    #[arg(long)]
    pub harbor_version: Option<String>,

    /// Target image prefix
    #[arg(long, env = "REGISTRY_PREFIX", default_value = DEFAULT_REGISTRY)]
    pub registry: String,

    /// Harbor source checkout directory (default: $TMPDIR/harbor-arm64-<tag>)
    #[arg(long)]
    pub workdir: Option<PathBuf>,

    /// Harbor git repository to clone
    #[arg(long, default_value = DEFAULT_HARBOR_REPO)]
    pub repo: String,

    /// Push the retagged images to the target prefix
    #[arg(long)]
    pub push: bool,

    /// Log commands instead of running them
    #[arg(long)]
    pub dry_run: bool,
}

/// A validated `vMAJOR.MINOR.PATCH` Harbor tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarborVersion(String);

impl HarborVersion {
    pub fn parse(raw: &str) -> Result<Self> {
        let tag = raw.trim();
        let valid = tag.strip_prefix('v').is_some_and(|rest| {
            let parts: Vec<_> = rest.split('.').collect();
            parts.len() == 3
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        });
        if valid {
            Ok(Self(tag.to_string()))
        } else {
            Err(Error::InvalidVersion(tag.to_string()))
        }
    }

    pub fn default_version() -> Result<Self> {
        Self::parse(DEFAULT_HARBOR_VERSION)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HarborVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildConfig {
    pub version: HarborVersion,
    pub registry: String,
    pub workdir: PathBuf,
    pub repo: String,
    pub push: bool,
    pub dry_run: bool,
}

impl BuildConfig {
    pub fn resolve(args: BuildArgs) -> Result<Self> {
        let version = match args.harbor_version {
            Some(v) => HarborVersion::parse(&v)?,
            None => HarborVersion::default_version()?,
        };
        let workdir = args
            .workdir
            .unwrap_or_else(|| std::env::temp_dir().join(format!("harbor-arm64-{version}")));
        Ok(Self {
            registry: args.registry.trim_end_matches('/').to_string(),
            version,
            workdir,
            repo: args.repo,
            push: args.push,
            dry_run: args.dry_run,
        })
    }

    /// Local tag produced by the upstream Makefile.
    pub fn upstream_ref(&self, image: &str) -> String {
        format!("goharbor/{image}:{}", self.version)
    }

    pub fn target_ref(&self, image: &str) -> String {
        format!("{}/{image}:{}", self.registry, self.version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> BuildArgs {
        BuildArgs {
            harbor_version: None,
            registry: DEFAULT_REGISTRY.to_string(),
            workdir: None,
            repo: DEFAULT_HARBOR_REPO.to_string(),
            push: false,
            dry_run: false,
        }
    }

    #[test]
    fn parses_valid_tags() {
        assert_eq!(HarborVersion::parse("v2.15.2").unwrap().as_str(), "v2.15.2");
        assert_eq!(
            HarborVersion::parse(" v2.15.2\n").unwrap().as_str(),
            "v2.15.2"
        );
    }

    #[test]
    fn rejects_invalid_tags() {
        for raw in ["2.15.2", "v2.15", "v2.15.2-rc1", "v2..2", "", "vx.y.z"] {
            assert!(
                matches!(HarborVersion::parse(raw), Err(Error::InvalidVersion(_))),
                "{raw} accepted"
            );
        }
    }

    #[test]
    fn version_file_is_valid() {
        HarborVersion::default_version().unwrap();
    }

    #[test]
    fn resolve_defaults() {
        let cfg = BuildConfig::resolve(args()).unwrap();
        assert_eq!(cfg.version, HarborVersion::default_version().unwrap());
        assert!(
            cfg.workdir
                .ends_with(format!("harbor-arm64-{}", cfg.version))
        );
        assert!(!cfg.push);
    }

    #[test]
    fn resolve_overrides() {
        let cfg = BuildConfig::resolve(BuildArgs {
            harbor_version: Some("v2.15.3".to_string()),
            registry: "registry.example.com/harbor/".to_string(),
            workdir: Some(PathBuf::from("/work")),
            push: true,
            ..args()
        })
        .unwrap();
        assert_eq!(cfg.version.as_str(), "v2.15.3");
        assert_eq!(cfg.workdir, PathBuf::from("/work"));
        assert_eq!(
            cfg.target_ref("harbor-core"),
            "registry.example.com/harbor/harbor-core:v2.15.3"
        );
        assert_eq!(
            cfg.upstream_ref("harbor-core"),
            "goharbor/harbor-core:v2.15.3"
        );
    }

    #[test]
    fn resolve_rejects_bad_version() {
        let err = BuildConfig::resolve(BuildArgs {
            harbor_version: Some("latest".to_string()),
            ..args()
        })
        .unwrap_err();
        assert!(matches!(err, Error::InvalidVersion(_)));
    }

    #[test]
    fn cli_parses_build() {
        let cli =
            Cli::try_parse_from(["harbor-arm64", "-v", "build", "--push", "--dry-run"]).unwrap();
        assert!(cli.verbose);
        let Command::Build(b) = cli.command else {
            panic!("expected build");
        };
        assert!(b.push && b.dry_run);
    }
}
