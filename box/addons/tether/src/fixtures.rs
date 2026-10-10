//! Test fixtures: a throwaway home directory with a dotfiles repo inside it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::controller::Context;
use crate::metrics::Metrics;

pub struct TempHome {
    _dir: tempfile::TempDir,
    pub root: PathBuf,
    pub home: PathBuf,
    pub repo: PathBuf,
}

impl Default for TempHome {
    fn default() -> Self {
        Self::new()
    }
}

impl TempHome {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let home = root.join("home");
        let repo = home.join("repo");
        fs::create_dir_all(&repo).expect("repo");
        Self {
            _dir: dir,
            root,
            home,
            repo,
        }
    }

    /// A file under the repo whose content is its own relative path.
    pub fn source_file(&self, rel: &str) -> PathBuf {
        let path = self.repo.join(rel);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, rel).expect("write");
        path
    }

    pub fn source_dir(&self, rel: &str) -> PathBuf {
        let path = self.repo.join(rel);
        fs::create_dir_all(&path).expect("mkdir");
        path
    }

    pub fn config(&self, text: &str) -> PathBuf {
        let path = self.root.join("config.toml");
        fs::write(&path, text).expect("config");
        path
    }

    pub fn context(&self, config: &str, dry_run: bool) -> Arc<Context> {
        Arc::new(Context {
            config_file: self.config(config),
            home: self.home.clone(),
            dry_run,
            metrics: Arc::new(Metrics::default()),
            mount_warned: AtomicBool::new(false),
        })
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.home.join(rel)
    }

    pub fn exists(path: &Path) -> bool {
        fs::symlink_metadata(path).is_ok()
    }
}
