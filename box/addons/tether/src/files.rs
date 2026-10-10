//! In-memory snapshot of the source files the console can show.
//!
//! The controller builds it on each reconcile, so HTTP handlers only look up
//! a map and never read the file system. Only files tracked in git are
//! included: this repository is public, so whatever is tracked is already
//! published, while local-only files such as work git settings and signing
//! keys stay out of the console.

pub mod git_index;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_FILES_PER_ROOT: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileMeta {
    pub path: PathBuf,
    pub size: u64,
    /// Not shown: binary, not UTF-8, or larger than 1 MiB.
    pub unreadable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileView {
    #[serde(flatten)]
    pub meta: FileMeta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tree {
    pub root: PathBuf,
    pub files: Vec<FileMeta>,
}

#[derive(Debug, Default)]
pub struct Snapshot {
    files: BTreeMap<PathBuf, FileView>,
    roots: BTreeMap<PathBuf, Vec<PathBuf>>,
}

impl Snapshot {
    /// Read the tracked files under each of `roots`. `repo_hint` is any path
    /// inside the repository, used to find its git index.
    pub fn collect(repo_hint: &Path, roots: &[PathBuf]) -> Self {
        let tracked = tracked_files(repo_hint);
        let mut snapshot = Self::default();
        for root in roots {
            if snapshot.roots.contains_key(root) {
                continue;
            }
            let paths: Vec<PathBuf> = tracked
                .range(root.clone()..)
                .take_while(|p| p.starts_with(root))
                .take(MAX_FILES_PER_ROOT)
                .cloned()
                .collect();
            for path in &paths {
                if !snapshot.files.contains_key(path)
                    && let Some(view) = read(path)
                {
                    snapshot.files.insert(path.clone(), view);
                }
            }
            let present = paths
                .into_iter()
                .filter(|p| snapshot.files.contains_key(p))
                .collect();
            snapshot.roots.insert(root.clone(), present);
        }
        snapshot
    }

    /// Add tether's own config file. It usually sits outside the repository,
    /// so it is not tracked there, and it holds no secrets by design.
    #[must_use]
    pub fn with_config(mut self, path: &Path) -> Self {
        if let Some(view) = read(path) {
            self.files.insert(path.to_path_buf(), view);
            self.roots
                .insert(path.to_path_buf(), vec![path.to_path_buf()]);
        }
        self
    }

    pub fn tree(&self, root: &Path) -> Option<Tree> {
        let paths = self.roots.get(root)?;
        Some(Tree {
            root: root.to_path_buf(),
            files: paths
                .iter()
                .filter_map(|p| self.files.get(p).map(|v| v.meta.clone()))
                .collect(),
        })
    }

    pub fn file(&self, path: &Path) -> Option<&FileView> {
        self.files.get(path)
    }
}

fn read(path: &Path) -> Option<FileView> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let size = meta.len();
    let content = (size <= MAX_FILE_BYTES)
        .then(|| fs::read(path).ok())
        .flatten()
        .filter(|bytes| !bytes.iter().take(8000).any(|b| *b == 0))
        .and_then(|bytes| String::from_utf8(bytes).ok());
    Some(FileView {
        meta: FileMeta {
            path: path.to_path_buf(),
            size,
            unreadable: content.is_none(),
        },
        content,
    })
}

/// Repository root and index path for the repository holding `start`.
fn find_index(start: &Path) -> Option<(PathBuf, PathBuf)> {
    for dir in start.ancestors() {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return Some((dir.to_path_buf(), dot_git.join("index")));
        }
        if dot_git.is_file() {
            let text = fs::read_to_string(&dot_git).ok()?;
            let gitdir = text.trim().strip_prefix("gitdir:")?.trim();
            return Some((dir.to_path_buf(), dir.join(gitdir).join("index")));
        }
    }
    None
}

/// Absolute paths of every tracked file. Empty when no readable index is
/// found, so nothing is shown rather than guessing what is safe.
fn tracked_files(repo_hint: &Path) -> BTreeSet<PathBuf> {
    find_index(repo_hint)
        .and_then(|(root, index)| {
            let bytes = fs::read(index).ok()?;
            let paths = git_index::tracked(&bytes)?;
            Some(paths.into_iter().map(|p| root.join(p)).collect())
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Repo {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }

    fn repo(tracked: &[&str], files: &[(&str, &[u8])]) -> Repo {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        fs::create_dir_all(root.join(".git")).expect("git");
        fs::write(root.join(".git/index"), git_index::tests::index_v2(tracked)).expect("index");
        for (path, content) in files {
            let path = root.join(path);
            fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            fs::write(path, content).expect("write");
        }
        Repo { _dir: dir, root }
    }

    #[test]
    fn only_tracked_files_are_shown() {
        let r = repo(
            &[
                "configs/git/config",
                "configs/git/ignore",
                "configs/zsh/.zshrc",
                "configs/bin/tool",
            ],
            &[
                ("configs/git/config", b"[user]\n"),
                ("configs/git/ignore", b".DS_Store\n"),
                ("configs/git/config-work", b"[user]\n\temail = me@corp\n"),
                ("configs/git/signing.key", b"[user]\n"),
                ("configs/zsh/.zshrc", b"export A=1\n"),
                ("configs/bin/tool", b"\x7fELF\x00\x01"),
            ],
        );
        let git = r.root.join("configs/git");
        let zshrc = r.root.join("configs/zsh/.zshrc");
        let tool = r.root.join("configs/bin/tool");
        let snapshot = Snapshot::collect(
            &r.root.join("configs"),
            &[git.clone(), zshrc.clone(), tool.clone(), git.clone()],
        );

        let tree = snapshot.tree(&git).expect("tree");
        let names: Vec<_> = tree
            .files
            .iter()
            .map(|f| f.path.file_name().expect("name"))
            .collect();
        assert_eq!(names, ["config", "ignore"]);
        assert!(
            snapshot.file(&git.join("config-work")).is_none(),
            "untracked stays hidden"
        );
        assert!(snapshot.file(&git.join("signing.key")).is_none());

        let file = snapshot.file(&zshrc).expect("zshrc");
        assert_eq!(file.content.as_deref(), Some("export A=1\n"));
        assert_eq!(
            snapshot.tree(&zshrc).expect("single file root").files.len(),
            1
        );

        let binary = snapshot.file(&tool).expect("tool");
        assert!(binary.meta.unreadable);
        assert_eq!(binary.content, None);

        assert!(snapshot.tree(Path::new("/elsewhere")).is_none());
    }

    #[test]
    fn config_file_is_added_outside_the_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("config.toml");
        fs::write(&config, "source_root = \"~\"\n").expect("config");
        let snapshot = Snapshot::default().with_config(&config);
        assert_eq!(
            snapshot.file(&config).and_then(|f| f.content.as_deref()),
            Some("source_root = \"~\"\n")
        );
        assert_eq!(snapshot.tree(&config).expect("tree").files.len(), 1);
        let missing = Snapshot::default().with_config(&dir.path().join("none.toml"));
        assert!(missing.tree(&dir.path().join("none.toml")).is_none());
    }

    #[test]
    fn missing_tracked_file_and_no_repo() {
        let r = repo(&["gone.txt"], &[]);
        let snapshot = Snapshot::collect(&r.root, &[r.root.join("gone.txt")]);
        assert_eq!(
            snapshot
                .tree(&r.root.join("gone.txt"))
                .expect("tree")
                .files
                .len(),
            0
        );

        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a"), "a").expect("write");
        let snapshot = Snapshot::collect(dir.path(), &[dir.path().join("a")]);
        assert_eq!(
            snapshot
                .tree(&dir.path().join("a"))
                .expect("tree")
                .files
                .len(),
            0
        );
    }

    #[test]
    fn worktree_gitdir_file_is_followed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let main_git = dir.path().join("main.git");
        fs::create_dir_all(&main_git).expect("gitdir");
        fs::write(
            main_git.join("index"),
            git_index::tests::index_v2(&["a.txt"]),
        )
        .expect("index");
        let wt = dir.path().join("wt");
        fs::create_dir_all(&wt).expect("worktree");
        fs::write(wt.join(".git"), format!("gitdir: {}\n", main_git.display())).expect("dotgit");
        fs::write(wt.join("a.txt"), "hello").expect("file");

        let snapshot = Snapshot::collect(&wt, &[wt.join("a.txt")]);
        assert_eq!(
            snapshot
                .file(&wt.join("a.txt"))
                .and_then(|f| f.content.as_deref()),
            Some("hello")
        );
    }

    #[test]
    fn large_and_invalid_utf8_files_are_unreadable() {
        let big = vec![b'a'; usize::try_from(MAX_FILE_BYTES).expect("size") + 1];
        let r = repo(
            &["big.txt", "latin1.txt"],
            &[("big.txt", &big), ("latin1.txt", b"caf\xe9")],
        );
        let snapshot = Snapshot::collect(&r.root, std::slice::from_ref(&r.root));
        let big = snapshot.file(&r.root.join("big.txt")).expect("big");
        assert!(big.meta.unreadable);
        assert_eq!(big.meta.size, MAX_FILE_BYTES + 1);
        assert!(
            snapshot
                .file(&r.root.join("latin1.txt"))
                .expect("latin1")
                .meta
                .unreadable
        );
    }
}
