//! Autonomous response on the node: kill, quarantine, and exec blocking.

use std::path::{Path, PathBuf};

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const QUARANTINE_DIR: &str = "quarantine";

/// Exec blocking through the BPF LSM, when it is attached.
pub trait Blocker: Send + Sync {
    /// Adds a kernel `(dev_t, ino)` to the blocklist.
    fn block(&self, dev: u64, ino: u64) -> Result<(), String>;
    fn set_enforce(&self, enforce: bool) -> Result<(), String>;
    fn entries(&self) -> usize;
}

pub fn kill(pid: i32) -> Result<(), String> {
    if pid <= 1 {
        return Err(format!("refusing to kill pid {pid}"));
    }
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    )
    .map_err(|e| format!("kill {pid}: {e}"))
}

/// Moves the file at `rel` inside `root` (a container root such as
/// `/proc/<pid>/root`, or the host root) into the quarantine directory as
/// `<sha256>`, readable only by root.
///
/// The path is resolved once, inside `root`, refusing every symlink, and
/// the opened file must be the inode the scan saw (`inode` is `st_dev`,
/// `st_ino`). Copy and unlink then work on file descriptors, so a container
/// that swaps a directory for a symlink after the scan cannot steer the
/// agent at host files.
#[cfg(target_os = "linux")]
pub fn quarantine(
    root: &Path,
    rel: &str,
    inode: (u64, u64),
    state: &Path,
    sha256: &str,
) -> Result<PathBuf, String> {
    use std::io::ErrorKind;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    use nix::fcntl::{AtFlags, OFlag, OpenHow, ResolveFlag, open, openat2};
    use nix::sys::stat::{FileStat, Mode, SFlag, fstat, fstatat};
    use nix::unistd::{UnlinkatFlags, unlinkat};

    let rel = rel.trim_start_matches('/');
    let (parent, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    if name.is_empty() || name == "." || name == ".." {
        return Err(format!("not a file path: {rel}"));
    }
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("quarantine name must be a SHA-256".into());
    }
    let same = |st: &FileStat| {
        (st.st_mode & SFlag::S_IFMT.bits()) == SFlag::S_IFREG.bits()
            && (st.st_dev, st.st_ino) == inode
    };
    let no_links = ResolveFlag::RESOLVE_NO_SYMLINKS | ResolveFlag::RESOLVE_NO_MAGICLINKS;
    let root_fd = open(
        root,
        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| format!("open root {}: {e}", root.display()))?;
    let parent_fd = openat2(
        &root_fd,
        if parent.is_empty() { "." } else { parent },
        OpenHow::new()
            .flags(OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC)
            .resolve(ResolveFlag::RESOLVE_IN_ROOT | no_links),
    )
    .map_err(|e| format!("resolve /{parent}: {e}"))?;
    let src_fd = openat2(
        &parent_fd,
        name,
        OpenHow::new()
            .flags(OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
            .resolve(ResolveFlag::RESOLVE_BENEATH | no_links),
    )
    .map_err(|e| format!("open /{rel}: {e}"))?;
    let st = fstat(&src_fd).map_err(|e| format!("stat /{rel}: {e}"))?;
    if !same(&st) {
        return Err(format!("/{rel} changed since it was scanned"));
    }

    let dir = state.join(QUARANTINE_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("quarantine dir: {e}"))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("quarantine dir: {e}"))?;
    let dst = dir.join(sha256);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .custom_flags(OFlag::O_NOFOLLOW.bits())
        .open(&dst)
    {
        Ok(mut out) => {
            let mut src = std::fs::File::from(src_fd);
            std::io::copy(&mut src, &mut out).map_err(|e| format!("copy /{rel}: {e}"))?;
            out.sync_all().map_err(|e| format!("sync: {e}"))?;
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("create {}: {e}", dst.display())),
    }

    let again = fstatat(&parent_fd, name, AtFlags::AT_SYMLINK_NOFOLLOW)
        .map_err(|e| format!("stat /{rel}: {e}"))?;
    if !same(&again) {
        return Err(format!("/{rel} changed before removal"));
    }
    unlinkat(&parent_fd, name, UnlinkatFlags::NoRemoveDir)
        .map_err(|e| format!("remove /{rel}: {e}"))?;
    Ok(dst)
}

/// Opens `path` as a process inside `root` would see it, refusing every
/// symlink, so a container cannot point the agent at a file outside it.
#[cfg(target_os = "linux")]
pub fn open_in_root(root: &Path, path: &str) -> Result<std::fs::File, String> {
    use nix::fcntl::{OFlag, OpenHow, ResolveFlag, open, openat2};
    use nix::sys::stat::Mode;

    let root_fd = open(
        root,
        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| format!("open root {}: {e}", root.display()))?;
    let rel = path.trim_start_matches('/');
    let fd = openat2(
        &root_fd,
        if rel.is_empty() { "." } else { rel },
        OpenHow::new()
            .flags(OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
            .resolve(
                ResolveFlag::RESOLVE_IN_ROOT
                    | ResolveFlag::RESOLVE_NO_SYMLINKS
                    | ResolveFlag::RESOLVE_NO_MAGICLINKS,
            ),
    )
    .map_err(|e| format!("open {path}: {e}"))?;
    Ok(std::fs::File::from(fd))
}

#[cfg(not(target_os = "linux"))]
pub fn open_in_root(_root: &Path, path: &str) -> Result<std::fs::File, String> {
    Err(format!(
        "opening {path} inside a container needs Linux openat2"
    ))
}

#[cfg(not(target_os = "linux"))]
pub fn quarantine(
    _root: &Path,
    rel: &str,
    _inode: (u64, u64),
    _state: &Path,
    _sha256: &str,
) -> Result<PathBuf, String> {
    Err(format!(
        "quarantine of /{} needs Linux openat2",
        rel.trim_start_matches('/')
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    pub struct FakeBlocker {
        pub blocked: Mutex<Vec<(u64, u64)>>,
        pub enforce: Mutex<Option<bool>>,
    }

    impl Blocker for FakeBlocker {
        fn block(&self, dev: u64, ino: u64) -> Result<(), String> {
            self.blocked.lock().unwrap().push((dev, ino));
            Ok(())
        }
        fn set_enforce(&self, enforce: bool) -> Result<(), String> {
            *self.enforce.lock().unwrap() = Some(enforce);
            Ok(())
        }
        fn entries(&self) -> usize {
            self.blocked.lock().unwrap().len()
        }
    }

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[cfg(target_os = "linux")]
    fn inode(p: &Path) -> (u64, u64) {
        use std::os::unix::fs::MetadataExt as _;
        let m = std::fs::metadata(p).unwrap();
        (m.dev(), m.ino())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn quarantines_within_the_root() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("app")).unwrap();
        let src = root.path().join("app/evil");
        std::fs::write(&src, b"x").unwrap();
        let ino = inode(&src);
        assert!(
            quarantine(root.path(), "/app/evil", (0, 0), state.path(), SHA)
                .unwrap_err()
                .contains("changed since")
        );
        let dst = quarantine(root.path(), "/app/evil", ino, state.path(), SHA).unwrap();
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), b"x");
        assert_eq!(
            std::fs::metadata(&dst).unwrap().permissions().mode() & 0o777,
            0o400
        );
        assert!(quarantine(root.path(), "/app/evil", ino, state.path(), SHA).is_err());
        assert!(quarantine(root.path(), "/app/", ino, state.path(), SHA).is_err());
        assert!(quarantine(root.path(), "/app/x", ino, state.path(), "nothex").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn refuses_symlinked_directories() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let victim = outside.path().join("passwd");
        std::fs::write(&victim, b"host file").unwrap();
        let ino = inode(&victim);
        std::os::unix::fs::symlink(outside.path(), root.path().join("etc")).unwrap();
        let err = quarantine(root.path(), "/etc/passwd", ino, state.path(), SHA).unwrap_err();
        assert!(err.contains("resolve"), "{err}");
        assert!(victim.exists());
        std::os::unix::fs::symlink(&victim, root.path().join("link")).unwrap();
        assert!(quarantine(root.path(), "/link", ino, state.path(), SHA).is_err());
        assert!(victim.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn open_in_root_refuses_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"host").unwrap();
        std::fs::write(root.path().join("run.sh"), b"echo hi").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), root.path().join("link.sh"))
            .unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("dir")).unwrap();
        let mut f = open_in_root(root.path(), "/run.sh").unwrap();
        let mut s = String::new();
        std::io::Read::read_to_string(&mut f, &mut s).unwrap();
        assert_eq!(s, "echo hi");
        assert!(open_in_root(root.path(), "/link.sh").is_err());
        assert!(open_in_root(root.path(), "/dir/secret").is_err());
        assert!(open_in_root(root.path(), "/../../etc/passwd").is_err());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn quarantine_needs_linux() {
        let dir = tempfile::tempdir().unwrap();
        assert!(open_in_root(dir.path(), "/x").is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(quarantine(dir.path(), "/x", (0, 0), dir.path(), SHA).is_err());
    }

    #[test]
    fn kill_refuses_init_and_reports_missing() {
        assert!(kill(1).is_err());
        assert!(kill(0).is_err());
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        kill(i32::try_from(child.id()).unwrap()).unwrap();
        assert!(!child.wait().unwrap().success());
        let b = FakeBlocker::default();
        b.block(1, 2).unwrap();
        b.set_enforce(true).unwrap();
        assert_eq!(b.entries(), 1);
    }
}
