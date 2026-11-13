//! Path resolution and file writes confined to the mounted host root.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// A symbolic link is followed only when root or the owner of the host root
/// created it. A link planted by another account in a directory it can write
/// to would otherwise redirect a rule to any file on the node.
const fn trusted_link(link_uid: u32, root_uid: u32) -> bool {
    link_uid == 0 || link_uid == root_uid
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
        let root_uid = fs::metadata(&self.root)?.uid();
        while let Some(step) = pending.pop_front() {
            match step {
                Step::Root => current.clear(),
                Step::Up => {
                    current.pop();
                }
                Step::Name(name) => {
                    current.push(name);
                    let candidate = self.real(&current);
                    let meta = fs::symlink_metadata(&candidate)?;
                    if meta.file_type().is_symlink() {
                        if !trusted_link(meta.uid(), root_uid) {
                            return Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                format!(
                                    "symbolic link owned by uid {} is not followed",
                                    meta.uid()
                                ),
                            ));
                        }
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

const TEMP_ATTEMPTS: u32 = 8;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_path(path: &Path, name: &OsStr) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let unique = format!(
        ".{:x}-{nanos:x}-{:x}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let mut temp_name = OsString::from(".");
    temp_name.push(name);
    temp_name.push(unique);
    path.with_file_name(temp_name)
}

/// Creates the temporary file exclusively, so a name planted in a directory
/// another account can write to is never opened or followed.
fn create_temp(path: &Path, name: &OsStr) -> io::Result<(PathBuf, fs::File)> {
    let mut attempt = 0;
    loop {
        let temp = temp_path(path, name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
        {
            Ok(file) => return Ok((temp, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < TEMP_ATTEMPTS => {
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Replaces `path` through a rename in its own directory, so a reader never
/// sees a half-written file.
pub fn write_atomic(path: &Path, content: &[u8], attributes: Attributes) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a file path"))?;
    let (temp, mut file) = create_temp(path, name)?;

    let result = (|| {
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

/// Opens an existing file or directory for a change of its own attributes.
/// The entry must not be a symbolic link, and the opened file is checked to
/// be the entry that was inspected, so a link swapped in meanwhile is refused.
pub fn open_entry(path: &Path) -> io::Result<fs::File> {
    let before = fs::symlink_metadata(path)?;
    if !(before.is_file() || before.is_dir()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file or directory",
        ));
    }
    let file = fs::File::open(path)?;
    let opened = file.metadata()?;
    if (opened.dev(), opened.ino()) != (before.dev(), before.ino()) {
        return Err(io::Error::other("the entry changed while it was checked"));
    }
    Ok(file)
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
    fn only_links_of_root_or_the_host_root_owner_are_followed() {
        assert!(trusted_link(0, 0));
        assert!(trusted_link(0, 501));
        assert!(trusted_link(501, 501));
        assert!(!trusted_link(1000, 0));
        assert!(!trusted_link(1000, 501));
    }

    #[test]
    fn a_planted_temporary_name_is_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, "secret").unwrap();
        let target = dir.path().join("motd");
        // The name the first attempt of this process would pick cannot be
        // guessed, so every leftover in the directory must be left alone.
        std::os::unix::fs::symlink(&victim, dir.path().join(".motd.planted.tmp")).unwrap();

        write_atomic(
            &target,
            b"hi\n",
            Attributes {
                mode: 0o644,
                owner: None,
            },
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "hi\n");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "secret");
        let left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with(".motd.") && name != ".motd.planted.tmp")
            .collect();
        assert_eq!(left, Vec::<String>::new());

        let (first, _) = create_temp(&target, OsStr::new("motd")).unwrap();
        let (second, _) = create_temp(&target, OsStr::new("motd")).unwrap();
        assert_ne!(first, second);
        let mode = fs::metadata(&first).unwrap().mode() & 0o7777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn an_entry_is_opened_only_when_it_is_a_file_or_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, "x").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();

        assert!(open_entry(&file).is_ok());
        assert!(open_entry(dir.path()).is_ok());
        assert_eq!(
            open_entry(&link).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            open_entry(&dir.path().join("missing")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
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
