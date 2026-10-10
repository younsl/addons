//! Link spec file: which dotfiles source is linked to which home target.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::SpecError;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSpec {
    source_root: String,
    backup_root: String,
    #[serde(default)]
    links: Vec<RawLink>,
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
    pub backup_root: PathBuf,
    pub links: Vec<Link>,
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
    pub fn load(path: &Path, home: &Path) -> Result<Self, SpecError> {
        let text = fs::read_to_string(path).map_err(|error| SpecError::Read {
            path: path.to_path_buf(),
            error,
        })?;
        Self::parse(&text, path, home)
    }

    pub fn parse(text: &str, path: &Path, home: &Path) -> Result<Self, SpecError> {
        let raw: RawSpec = toml::from_str(text).map_err(|error| SpecError::Parse {
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
                    return Err(SpecError::DuplicateTarget(target));
                }
                Ok(Link {
                    source: resolve_source(&link.source, &source_root, home)?,
                    target,
                    per_entry: link.per_entry,
                })
            })
            .collect::<Result<_, _>>()?;

        Ok(Self { backup_root, links })
    }
}

fn resolve_source(value: &str, source_root: &Path, home: &Path) -> Result<PathBuf, SpecError> {
    if value.starts_with('~') || Path::new(value).is_absolute() {
        expand("source", value, home)
    } else {
        Ok(source_root.join(value))
    }
}

fn expand(field: &'static str, value: &str, home: &Path) -> Result<PathBuf, SpecError> {
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
        Err(SpecError::NotAbsolute {
            field,
            value: value.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/dev";

    fn parse(text: &str) -> Result<Spec, SpecError> {
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
            SpecError::NotAbsolute {
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
        assert!(matches!(err, SpecError::DuplicateTarget(_)));
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
