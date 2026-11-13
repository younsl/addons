//! Path resolution and file writes confined to the mounted host root.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};

const MAX_SYMLINK_HOPS: usize = 40;

/// The host filesystem as mounted into the agent container.
#[derive(Clone, Debug)]
pub struct HostRoot {
    root: PathBuf,
}

enum Step {
    Root,
    Up,
    Name(OsString),
}

fn steps(path: &Path) -> impl Iterator<Item = Step> + '_ {
    path.components().filter_map(|c| match c {
        Component::RootDir => Some(Step::Root),
        Component::ParentDir => Some(Step::Up),
        Component::Normal(name) => Some(Step::Name(name.to_os_string())),
        Component::CurDir | Component::Prefix(_) => None,
    })
}

/// Matches `*` (any run) and `?` (one character) against a whole file name.
pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    let (mut pi, mut ni) = (0, 0);
    let mut backtrack: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            backtrack = Some((pi, ni));
            pi += 1;
        } else if let Some((star, matched)) = backtrack {
            pi = star + 1;
            ni = matched + 1;
            backtrack = Some((star, matched + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

pub fn has_wildcard(path: &str) -> bool {
    path.contains(['*', '?'])
}

impl HostRoot {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn real(&self, current: &[OsString]) -> PathBuf {
        let mut out = self.root.clone();
        out.extend(current);
        out
    }

    /// Resolves a host path the way a process chrooted into the host would,
    /// so an absolute symbolic link never leads out of the mount.
    pub fn resolve(&self, path: &str) -> io::Result<PathBuf> {
        let mut pending: VecDeque<Step> = steps(Path::new(path)).collect();
        let mut current: Vec<OsString> = Vec::new();
        let mut hops = 0;
        while let Some(step) = pending.pop_front() {
            match step {
                Step::Root => current.clear(),
                Step::Up => {
                    current.pop();
                }
                Step::Name(name) => {
                    current.push(name);
                    let candidate = self.real(&current);
                    if fs::symlink_metadata(&candidate)?.file_type().is_symlink() {
                        hops += 1;
                        if hops > MAX_SYMLINK_HOPS {
                            return Err(io::Error::other("too many levels of symbolic links"));
                        }
                        current.pop();
                        let target = fs::read_link(&candidate)?;
                        for (i, step) in steps(&target).enumerate() {
                            pending.insert(i, step);
                        }
                    }
                }
            }
        }
        Ok(self.real(&current))
    }

    /// Resolves the directory of `path` and keeps the last segment as is, for
    /// operations that replace the entry itself.
    pub fn resolve_entry(&self, path: &str) -> io::Result<PathBuf> {
        let (dir, name) = path
            .rsplit_once('/')
            .filter(|(_, name)| !name.is_empty())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a file path"))?;
        Ok(self
            .resolve(if dir.is_empty() { "/" } else { dir })?
            .join(name))
    }

    /// Host paths matching a pattern whose last segment holds wildcards,
    /// sorted. A missing directory matches nothing.
    pub fn glob(&self, pattern: &str) -> io::Result<Vec<String>> {
        let Some((dir, name_pattern)) = pattern.rsplit_once('/') else {
            return Ok(Vec::new());
        };
        let real_dir = match self.resolve(if dir.is_empty() { "/" } else { dir }) {
            Ok(real_dir) => real_dir,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let entries = match fs::read_dir(real_dir) {
            Ok(entries) => entries,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        for entry in entries {
            let name = entry?.file_name();
            if let Some(name) = name.to_str()
                && wildcard_match(name_pattern, name)
            {
                out.push(format!("{dir}/{name}"));
            }
        }
        out.sort();
        Ok(out)
    }
}

/// Mode and owner a rewritten file keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attributes {
    pub mode: u32,
    pub owner: Option<(u32, u32)>,
}

impl Attributes {
    pub fn of(meta: &fs::Metadata) -> Self {
        Self {
            mode: meta.mode() & 0o7777,
            owner: Some((meta.uid(), meta.gid())),
        }
    }
}

/// Replaces `path` through a rename in its own directory, so a reader never
/// sees a half-written file.
pub fn write_atomic(path: &Path, content: &[u8], attributes: Attributes) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a file path"))?;
    let mut temp_name = OsString::from(".");
    temp_name.push(name);
    temp_name.push(OsStr::new(".node-configuration-operator.tmp"));
    let temp = path.with_file_name(temp_name);

    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)?;
        file.write_all(content)?;
        file.set_permissions(fs::Permissions::from_mode(attributes.mode))?;
        if let Some((uid, gid)) = attributes.owner {
            let meta = file.metadata()?;
            if (meta.uid(), meta.gid()) != (uid, gid) {
                std::os::unix::fs::fchown(&file, Some(uid), Some(gid))?;
            }
        }
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn wildcards_match_whole_names() {
        assert!(wildcard_match(".bash*", ".bashrc"));
        assert!(wildcard_match(".bash*", ".bash"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("a?c", "abc"));
        assert!(wildcard_match("*.conf", "sshd.conf"));
        assert!(wildcard_match("a*b*c", "aXbYbZc"));
        assert!(!wildcard_match(".bash*", ".zshrc"));
        assert!(!wildcard_match("a?c", "ac"));
        assert!(!wildcard_match("abc", "abcd"));
        assert!(has_wildcard("/root/.bash*"));
        assert!(!has_wildcard("/usr/bin/su"));
    }

    #[test]
    fn absolute_symlinks_stay_inside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("usr/bin")).unwrap();
        fs::create_dir_all(root.join("run")).unwrap();
        fs::write(root.join("usr/bin/su"), "").unwrap();
        fs::write(root.join("run/motd"), "").unwrap();
        symlink("usr/bin", root.join("bin")).unwrap();
        symlink("/run/motd", root.join("motd")).unwrap();
        symlink("/bin/../bin/su", root.join("su")).unwrap();

        let host = HostRoot::new(root);
        assert_eq!(host.resolve("/bin/su").unwrap(), root.join("usr/bin/su"));
        assert_eq!(host.resolve("/motd").unwrap(), root.join("run/motd"));
        assert_eq!(host.resolve("/su").unwrap(), root.join("usr/bin/su"));
        assert_eq!(
            host.resolve("/../../run/./motd").unwrap(),
            root.join("run/motd")
        );
        assert_eq!(host.resolve("/").unwrap(), root);
        assert_eq!(
            host.resolve("/missing").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(host.resolve_entry("/motd").unwrap(), root.join("motd"));
        assert_eq!(
            host.resolve_entry("/bin/new").unwrap(),
            root.join("usr/bin/new")
        );
        assert!(host.resolve_entry("/bin/").is_err());
    }

    #[test]
    fn symlink_loops_end_in_an_error() {
        let dir = tempfile::tempdir().unwrap();
        symlink("/b", dir.path().join("a")).unwrap();
        symlink("/a", dir.path().join("b")).unwrap();
        let err = HostRoot::new(dir.path()).resolve("/a").unwrap_err();
        assert!(err.to_string().contains("symbolic links"), "{err}");
    }

    #[test]
    fn glob_lists_one_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("root")).unwrap();
        for name in [".bashrc", ".bash_profile", ".cshrc"] {
            fs::write(root.join("root").join(name), "").unwrap();
        }
        fs::write(root.join("file"), "").unwrap();
        let host = HostRoot::new(root);
        assert_eq!(
            host.glob("/root/.bash*").unwrap(),
            vec!["/root/.bash_profile", "/root/.bashrc"]
        );
        assert_eq!(
            host.glob("/home/nobody/.bash*").unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(host.glob("/file/*").unwrap(), Vec::<String>::new());
        assert_eq!(host.glob("no-slash*").unwrap(), Vec::<String>::new());
        assert_eq!(host.glob("/fil?").unwrap(), vec!["/file"]);
    }

    #[test]
    fn atomic_write_sets_content_and_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("motd");
        symlink("/elsewhere", &path).unwrap();
        let attributes = Attributes {
            mode: 0o640,
            owner: None,
        };
        write_atomic(&path, b"hello\n", attributes).unwrap();
        let meta = fs::symlink_metadata(&path).unwrap();
        assert!(meta.file_type().is_file());
        assert_eq!(meta.mode() & 0o7777, 0o640);
        assert_eq!(fs::read(&path).unwrap(), b"hello\n");
        assert_eq!(Attributes::of(&meta).mode, 0o640);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);

        let missing = dir.path().join("missing/file");
        assert!(write_atomic(&missing, b"", attributes).is_err());
        assert!(write_atomic(Path::new("/"), b"", attributes).is_err());
    }
}
