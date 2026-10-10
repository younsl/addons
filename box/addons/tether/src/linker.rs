//! Plans and applies the symlinks a [`Spec`] declares.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::Error;
use crate::spec::{Link, Spec};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Target already links to the source.
    InSync,
    /// Target does not exist.
    Create,
    /// Target is a symlink to somewhere else.
    Relink,
    /// Target is a real file or directory, moved to the backup directory.
    Backup,
    /// Target of a per-entry link is not a real directory yet.
    MakeDir,
    /// Source does not exist, nothing to link.
    MissingSource,
}

impl Action {
    pub const ALL: [Self; 6] = [
        Self::InSync,
        Self::Create,
        Self::Relink,
        Self::Backup,
        Self::MakeDir,
        Self::MissingSource,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InSync => "in_sync",
            Self::Create => "create",
            Self::Relink => "relink",
            Self::Backup => "backup",
            Self::MakeDir => "make_dir",
            Self::MissingSource => "missing_source",
        }
    }

    pub const fn changes(self) -> bool {
        !matches!(self, Self::InSync | Self::MissingSource)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Step {
    pub source: PathBuf,
    pub target: PathBuf,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    #[serde(flatten)]
    pub step: Step,
    pub applied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Backup directory for one reconcile, created on first use.
#[derive(Debug)]
pub struct Backups {
    dir: PathBuf,
    home: PathBuf,
    used: bool,
}

impl Backups {
    pub fn new(root: &Path, stamp: &str, home: &Path) -> Self {
        Self {
            dir: root.join(stamp),
            home: home.to_path_buf(),
            used: false,
        }
    }

    pub fn used_dir(&self) -> Option<&Path> {
        self.used.then_some(self.dir.as_path())
    }

    fn stash(&mut self, target: &Path) -> Result<PathBuf, Error> {
        let relative = target
            .strip_prefix(&self.home)
            .or_else(|_| target.strip_prefix("/"))
            .unwrap_or(target);
        let dest = self.dir.join(relative);
        create_parent(&dest)?;
        fs::rename(target, &dest).map_err(Error::io("move", target))?;
        self.used = true;
        Ok(dest)
    }
}

pub fn plan(spec: &Spec) -> Vec<Step> {
    let mut steps = Vec::new();
    for link in &spec.links {
        if link.per_entry {
            plan_per_entry(link, &mut steps);
        } else {
            steps.push(plan_link(&link.source, &link.target));
        }
    }
    steps
}

fn plan_link(source: &Path, target: &Path) -> Step {
    let action = if source.exists() {
        match fs::symlink_metadata(target) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if fs::read_link(target).is_ok_and(|current| current == source) {
                    Action::InSync
                } else {
                    Action::Relink
                }
            }
            Ok(_) => Action::Backup,
            Err(_) => Action::Create,
        }
    } else {
        Action::MissingSource
    };
    step(source, target, action)
}

fn plan_per_entry(link: &Link, steps: &mut Vec<Step>) {
    let Ok(names) = child_dirs(&link.source) else {
        steps.push(step(&link.source, &link.target, Action::MissingSource));
        return;
    };

    let target_is_dir = fs::symlink_metadata(&link.target).is_ok_and(|meta| meta.is_dir());
    if !target_is_dir {
        steps.push(step(&link.source, &link.target, Action::MakeDir));
    }

    for name in names {
        let source = link.source.join(&name);
        let target = link.target.join(&name);
        steps.push(if target_is_dir {
            plan_link(&source, &target)
        } else {
            step(&source, &target, Action::Create)
        });
    }
}

fn child_dirs(dir: &Path) -> io::Result<Vec<OsString>> {
    let mut names: Vec<_> = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name())
        .collect();
    names.sort();
    Ok(names)
}

fn step(source: &Path, target: &Path, action: Action) -> Step {
    Step {
        source: source.to_path_buf(),
        target: target.to_path_buf(),
        action,
    }
}

/// Report the steps without touching the file system.
pub fn preview(steps: Vec<Step>) -> Vec<Entry> {
    steps
        .into_iter()
        .map(|step| Entry {
            step,
            applied: false,
            backup: None,
            error: None,
        })
        .collect()
}

pub fn apply(steps: Vec<Step>, backups: &mut Backups) -> Vec<Entry> {
    steps
        .into_iter()
        .map(|step| match apply_step(&step, backups) {
            Ok(backup) => Entry {
                applied: step.action.changes(),
                step,
                backup,
                error: None,
            },
            Err(err) => Entry {
                step,
                applied: false,
                backup: None,
                error: Some(err.to_string()),
            },
        })
        .collect()
}

fn apply_step(step: &Step, backups: &mut Backups) -> Result<Option<PathBuf>, Error> {
    let Step {
        source,
        target,
        action,
    } = step;
    match action {
        Action::InSync | Action::MissingSource => Ok(None),
        Action::MakeDir => {
            if fs::symlink_metadata(target).is_ok_and(|meta| meta.file_type().is_symlink()) {
                remove(target)?;
            }
            fs::create_dir_all(target).map_err(Error::io("create dir", target))?;
            Ok(None)
        }
        Action::Create => {
            create_parent(target)?;
            link(source, target)?;
            Ok(None)
        }
        Action::Relink => {
            remove(target)?;
            link(source, target)?;
            Ok(None)
        }
        Action::Backup => {
            let backup = backups.stash(target)?;
            link(source, target)?;
            Ok(Some(backup))
        }
    }
}

fn create_parent(path: &Path) -> Result<(), Error> {
    path.parent().map_or(Ok(()), |parent| {
        fs::create_dir_all(parent).map_err(Error::io("create dir", parent))
    })
}

fn remove(path: &Path) -> Result<(), Error> {
    fs::remove_file(path).map_err(Error::io("remove", path))
}

fn link(source: &Path, target: &Path) -> Result<(), Error> {
    symlink(source, target).map_err(Error::io("link", target))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::TempHome;

    struct Fixture(TempHome);

    impl std::ops::Deref for Fixture {
        type Target = TempHome;

        fn deref(&self) -> &TempHome {
            &self.0
        }
    }

    impl Fixture {
        fn new() -> Self {
            Self(TempHome::new())
        }

        fn spec(&self, links: Vec<Link>) -> Spec {
            Spec {
                source_root: self.repo.clone(),
                backup_root: self.home.join(".dotfiles-backup"),
                links,
                packages: None,
            }
        }

        fn apply(&self, spec: &Spec) -> (Vec<Entry>, Option<PathBuf>) {
            let mut backups = Backups::new(&spec.backup_root, "20260101-000000", &self.home);
            let entries = apply(plan(spec), &mut backups);
            (entries, backups.used_dir().map(Path::to_path_buf))
        }
    }

    fn link_of(source: &Path, target: &Path) -> Link {
        Link {
            source: source.to_path_buf(),
            target: target.to_path_buf(),
            per_entry: false,
        }
    }

    fn actions(spec: &Spec) -> Vec<Action> {
        plan(spec).into_iter().map(|s| s.action).collect()
    }

    #[test]
    fn creates_then_stays_in_sync() {
        let fx = Fixture::new();
        let source = fx.source_file("zsh/.zshrc");
        let target = fx.home.join(".config/zsh/.zshrc");
        let spec = fx.spec(vec![link_of(&source, &target)]);

        assert_eq!(actions(&spec), vec![Action::Create]);
        let (entries, backup_dir) = fx.apply(&spec);
        assert!(entries[0].applied);
        assert!(entries[0].error.is_none());
        assert!(backup_dir.is_none());
        assert_eq!(fs::read_link(&target).expect("link"), source);

        assert_eq!(actions(&spec), vec![Action::InSync]);
        let (entries, _) = fx.apply(&spec);
        assert!(!entries[0].applied);
    }

    #[test]
    fn relinks_foreign_symlink() {
        let fx = Fixture::new();
        let source = fx.source_dir("git");
        let other = fx.source_dir("other");
        let target = fx.home.join("git");
        symlink(&other, &target).expect("symlink");
        let spec = fx.spec(vec![link_of(&source, &target)]);

        assert_eq!(actions(&spec), vec![Action::Relink]);
        fx.apply(&spec);
        assert_eq!(fs::read_link(&target).expect("link"), source);
        assert!(other.exists(), "relink never touches the old link target");
    }

    #[test]
    fn backs_up_real_file() {
        let fx = Fixture::new();
        let source = fx.source_file("npmrc");
        let target = fx.home.join(".npmrc");
        fs::write(&target, "local").expect("write");
        let spec = fx.spec(vec![link_of(&source, &target)]);

        assert_eq!(actions(&spec), vec![Action::Backup]);
        let (entries, backup_dir) = fx.apply(&spec);
        let backup = entries[0].backup.clone().expect("backup path");
        let backup_dir = backup_dir.expect("backup dir used");

        assert_eq!(backup, backup_dir.join(".npmrc"));
        assert!(backup_dir.ends_with(".dotfiles-backup/20260101-000000"));
        assert_eq!(fs::read_to_string(&backup).expect("backup"), "local");
        assert_eq!(fs::read_link(&target).expect("link"), source);
    }

    #[test]
    fn backup_outside_home_keeps_absolute_layout() {
        let fx = Fixture::new();
        let mut backups = Backups::new(&fx.home.join("b"), "s", Path::new("/nonexistent"));
        let target = fx.home.join("x");
        fs::write(&target, "x").expect("write");
        let dest = backups.stash(&target).expect("stash");
        assert!(dest.starts_with(fx.home.join("b/s")));
        assert!(dest.ends_with("home/x"));
    }

    #[test]
    fn missing_source_is_left_alone() {
        let fx = Fixture::new();
        let target = fx.home.join(".yarnrc");
        let spec = fx.spec(vec![link_of(&fx.repo.join("yarnrc"), &target)]);

        assert_eq!(actions(&spec), vec![Action::MissingSource]);
        let (entries, _) = fx.apply(&spec);
        assert!(!entries[0].applied);
        assert!(fs::symlink_metadata(&target).is_err());
    }

    #[test]
    fn per_entry_links_each_subdirectory() {
        let fx = Fixture::new();
        let skills = fx.source_dir("skills");
        fx.source_dir("skills/b");
        fx.source_dir("skills/a");
        fx.source_file("skills/README.md");
        let target = fx.home.join(".claude/skills");
        let spec = fx.spec(vec![Link {
            source: skills.clone(),
            target: target.clone(),
            per_entry: true,
        }]);

        assert_eq!(
            actions(&spec),
            vec![Action::MakeDir, Action::Create, Action::Create]
        );
        fx.apply(&spec);
        assert!(fs::symlink_metadata(&target).expect("dir").is_dir());
        assert_eq!(
            fs::read_link(target.join("a")).expect("a"),
            skills.join("a")
        );
        assert_eq!(
            fs::read_link(target.join("b")).expect("b"),
            skills.join("b")
        );

        fs::create_dir_all(target.join("managed-elsewhere")).expect("foreign");
        assert_eq!(actions(&spec), vec![Action::InSync, Action::InSync]);
        assert!(target.join("managed-elsewhere").exists());
    }

    #[test]
    fn per_entry_replaces_whole_directory_symlink() {
        let fx = Fixture::new();
        let skills = fx.source_dir("skills");
        fx.source_dir("skills/a");
        let target = fx.home.join("skills");
        symlink(&skills, &target).expect("symlink");
        let spec = fx.spec(vec![Link {
            source: skills.clone(),
            target: target.clone(),
            per_entry: true,
        }]);

        assert_eq!(actions(&spec), vec![Action::MakeDir, Action::Create]);
        let (entries, _) = fx.apply(&spec);
        assert!(entries.iter().all(|e| e.error.is_none()), "{entries:?}");
        assert!(
            !fs::symlink_metadata(&target)
                .expect("dir")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_link(target.join("a")).expect("a"),
            skills.join("a")
        );
    }

    #[test]
    fn per_entry_missing_source() {
        let fx = Fixture::new();
        let spec = fx.spec(vec![Link {
            source: fx.repo.join("nope"),
            target: fx.home.join("skills"),
            per_entry: true,
        }]);
        assert_eq!(actions(&spec), vec![Action::MissingSource]);
    }

    #[test]
    fn failure_is_reported_per_entry() {
        let fx = Fixture::new();
        fx.source_dir("skills/a");
        let target = fx.home.join("skills");
        fs::write(&target, "file in the way").expect("write");
        let spec = fx.spec(vec![Link {
            source: fx.repo.join("skills"),
            target,
            per_entry: true,
        }]);

        let (entries, _) = fx.apply(&spec);
        let err = entries[0].error.as_deref().expect("make_dir fails");
        assert!(err.starts_with("create dir "), "{err}");
        assert!(entries.iter().all(|e| !e.applied));
    }

    #[test]
    fn preview_changes_nothing() {
        let fx = Fixture::new();
        let source = fx.source_file("gpg.conf");
        let target = fx.home.join(".gnupg/gpg.conf");
        let spec = fx.spec(vec![link_of(&source, &target)]);

        let entries = preview(plan(&spec));
        assert_eq!(entries[0].step.action, Action::Create);
        assert!(!entries[0].applied);
        assert!(!target.parent().expect("parent").exists());
    }

    #[test]
    fn action_names_are_unique() {
        let names: std::collections::HashSet<_> = Action::ALL.iter().map(|a| a.as_str()).collect();
        assert_eq!(names.len(), Action::ALL.len());
        assert!(!Action::InSync.changes());
        assert!(Action::Backup.changes());
    }
}
