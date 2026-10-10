//! Link spec file: which dotfiles source is linked to which home target.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{Error, Result};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSpec {
    source_root: String,
    backup_root: String,
    #[serde(default)]
    links: Vec<RawLink>,
    packages: Option<RawPackages>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPackages {
    brewfile: Option<String>,
    krewfile: Option<String>,
    homebrew_prefix: Option<String>,
    homebrew_cache: Option<String>,
    trust_file: Option<String>,
    launch_agents: Option<String>,
    krew_root: Option<String>,
    cargo_home: Option<String>,
    go_bin: Option<String>,
    npm_prefix: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLink {
    source: String,
    target: String,
    #[serde(default)]
    per_entry: bool,
}

/// Resolved spec with every path absolute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub source_root: PathBuf,
    pub backup_root: PathBuf,
    pub links: Vec<Link>,
    pub packages: Option<Packages>,
}

/// Package manager files regenerated from what is installed on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packages {
    pub brewfile: Option<PathBuf>,
    pub krewfile: Option<PathBuf>,
    pub homebrew_prefix: PathBuf,
    pub homebrew_cache: PathBuf,
    pub trust_file: PathBuf,
    pub launch_agents: PathBuf,
    pub krew_root: PathBuf,
    pub cargo_home: PathBuf,
    pub go_bin: PathBuf,
    pub npm_prefix: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub source: PathBuf,
    pub target: PathBuf,
    /// Link each subdirectory of `source` into `target` instead of `source`
    /// itself, so entries other tools create in `target` stay in place.
    pub per_entry: bool,
}

impl Spec {
    pub fn load(path: &Path, home: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).map_err(|error| Error::ConfigRead {
            path: path.to_path_buf(),
            error,
        })?;
        Self::parse(&text, path, home)
    }

    pub fn parse(text: &str, path: &Path, home: &Path) -> Result<Self> {
        let raw: RawSpec = toml::from_str(text).map_err(|error| Error::ConfigParse {
            path: path.to_path_buf(),
            error: Box::new(error),
        })?;

        let source_root = expand("source_root", &raw.source_root, home)?;
        let backup_root = expand("backup_root", &raw.backup_root, home)?;

        let mut seen = HashSet::new();
        let links = raw
            .links
            .into_iter()
            .map(|link| {
                let target = expand("target", &link.target, home)?;
                if !seen.insert(target.clone()) {
                    return Err(Error::DuplicateTarget(target));
                }
                Ok(Link {
                    source: resolve_source("source", &link.source, &source_root, home)?,
                    target,
                    per_entry: link.per_entry,
                })
            })
            .collect::<Result<_, _>>()?;

        let packages = raw
            .packages
            .map(|raw| resolve_packages(raw, &source_root, home))
            .transpose()?;

        Ok(Self {
            source_root,
            backup_root,
            links,
            packages,
        })
    }
}

fn resolve_packages(raw: RawPackages, source_root: &Path, home: &Path) -> Result<Packages> {
    let path_or = |field: &'static str, value: Option<String>, default: &str| {
        expand(field, value.as_deref().unwrap_or(default), home)
    };
    let file = |field: &'static str, value: Option<String>| {
        value
            .map(|v| resolve_source(field, &v, source_root, home))
            .transpose()
    };
    Ok(Packages {
        brewfile: file("brewfile", raw.brewfile)?,
        krewfile: file("krewfile", raw.krewfile)?,
        homebrew_prefix: path_or("homebrew_prefix", raw.homebrew_prefix, "/opt/homebrew")?,
        homebrew_cache: path_or(
            "homebrew_cache",
            raw.homebrew_cache,
            "~/Library/Caches/Homebrew",
        )?,
        trust_file: path_or(
            "trust_file",
            raw.trust_file,
            "~/.config/homebrew/trust.json",
        )?,
        launch_agents: path_or("launch_agents", raw.launch_agents, "~/Library/LaunchAgents")?,
        krew_root: path_or("krew_root", raw.krew_root, "~/.krew")?,
        cargo_home: path_or("cargo_home", raw.cargo_home, "~/.cargo")?,
        go_bin: path_or("go_bin", raw.go_bin, "~/go/bin")?,
        npm_prefix: raw
            .npm_prefix
            .map(|v| expand("npm_prefix", &v, home))
            .transpose()?,
    })
}

fn resolve_source(
    field: &'static str,
    value: &str,
    source_root: &Path,
    home: &Path,
) -> Result<PathBuf> {
    if value.starts_with('~') || Path::new(value).is_absolute() {
        expand(field, value, home)
    } else {
        Ok(source_root.join(value))
    }
}

fn expand(field: &'static str, value: &str, home: &Path) -> Result<PathBuf> {
    if value == "~" {
        return Ok(home.to_path_buf());
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return Ok(home.join(rest));
    }
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(Error::NotAbsolute {
            field,
            value: value.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/dev";

    fn parse(text: &str) -> Result<Spec> {
        Spec::parse(text, Path::new("config.toml"), Path::new(HOME))
    }

    #[test]
    fn resolves_paths() {
        let spec = parse(
            r#"
            source_root = "~/dotfiles/configs"
            backup_root = "/var/backup"

            [[links]]
            source = "zsh/.zshrc"
            target = "~/.zshrc"

            [[links]]
            source = "/opt/skills"
            target = "~/.claude/skills"
            per_entry = true

            [[links]]
            source = "~/elsewhere/git"
            target = "/etc/git"
            "#,
        )
        .expect("valid spec");

        assert_eq!(spec.backup_root, PathBuf::from("/var/backup"));
        assert_eq!(
            spec.links,
            vec![
                Link {
                    source: "/home/dev/dotfiles/configs/zsh/.zshrc".into(),
                    target: "/home/dev/.zshrc".into(),
                    per_entry: false,
                },
                Link {
                    source: "/opt/skills".into(),
                    target: "/home/dev/.claude/skills".into(),
                    per_entry: true,
                },
                Link {
                    source: "/home/dev/elsewhere/git".into(),
                    target: "/etc/git".into(),
                    per_entry: false,
                },
            ]
        );
    }

    #[test]
    fn bare_tilde_is_home() {
        let spec = parse("source_root = \"~\"\nbackup_root = \"~\"\n").expect("valid spec");
        assert_eq!(spec.backup_root, PathBuf::from(HOME));
        assert_eq!(spec.links.len(), 0);
    }

    #[test]
    fn rejects_relative_target() {
        let err = parse(
            r#"
            source_root = "~/d"
            backup_root = "~/b"
            [[links]]
            source = "git"
            target = ".config/git"
            "#,
        )
        .expect_err("relative target");
        assert!(matches!(
            err,
            Error::NotAbsolute {
                field: "target",
                ..
            }
        ));
    }

    #[test]
    fn rejects_relative_roots() {
        let err = parse("source_root = \"d\"\nbackup_root = \"~/b\"\n").expect_err("relative root");
        assert!(err.to_string().contains("source_root"), "{err}");
    }

    #[test]
    fn rejects_duplicate_target() {
        let err = parse(
            r#"
            source_root = "~/d"
            backup_root = "~/b"
            [[links]]
            source = "a"
            target = "~/.x"
            [[links]]
            source = "b"
            target = "/home/dev/.x"
            "#,
        )
        .expect_err("duplicate target");
        assert!(matches!(err, Error::DuplicateTarget(_)));
    }

    #[test]
    fn rejects_unknown_field() {
        let err = parse("source_root = \"~\"\nbackup_root = \"~\"\nextra = 1\n")
            .expect_err("unknown field");
        assert!(err.to_string().starts_with("parse config.toml"), "{err}");
    }

    #[test]
    fn load_reports_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("missing.toml");
        let err = Spec::load(&path, Path::new(HOME)).expect_err("missing file");
        assert!(err.to_string().starts_with("read "), "{err}");
    }

    #[test]
    fn load_reads_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(&path, "source_root = \"~\"\nbackup_root = \"~\"\n").expect("write");
        assert!(Spec::load(&path, Path::new(HOME)).is_ok());
    }
}
