//! Anomaly checks for rootkits, after Wazuh rootcheck: processes the kernel
//! runs but `/proc` does not list, regular or hidden files in `/dev`, and
//! setuid files in world-writable temporary directories. They need no
//! signatures, so they also catch rootkits no feed knows.

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use walkdir::WalkDir;

/// `/dev` entries a stock system ships, never reported.
const DEV_KNOWN: &[&str] = &[
    "MAKEDEV",
    "README.MAKEDEV",
    "MAKEDEV.README",
    "MAKEDEV.local",
    ".udev",
    ".udevdb",
    ".udev.tdb",
    ".initramfs",
    ".initramfs-tools",
    ".mount",
    ".lxc",
    ".lxd-mounts",
    ".blkid.tab",
    ".blkid.tab.old",
    "oprofile",
];
const TEMP_DIRS: &[&str] = &["tmp", "var/tmp", "dev/shm"];
const MAX_DEPTH: usize = 6;
const MAX_ENTRIES: usize = 200_000;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Finding {
    /// A process that answers signals but is missing from `/proc`.
    HiddenProcess(i32),
    /// A regular file in `/dev`, where only device nodes belong.
    DevFile(String),
    /// A dot-named entry in `/dev`.
    DevHidden(String),
    /// A setuid or setgid file in a world-writable directory.
    TempSetuid(String),
}

impl Finding {
    pub const fn rule(&self) -> &'static str {
        match self {
            Self::HiddenProcess(_) => "hidden-process",
            Self::DevFile(_) => "dev-regular-file",
            Self::DevHidden(_) => "dev-hidden-entry",
            Self::TempSetuid(_) => "setuid-in-temp",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::HiddenProcess(pid) => {
                format!("process {pid} answers signals but /proc does not list it")
            }
            Self::DevFile(p) => format!("regular file {p} in /dev"),
            Self::DevHidden(p) => format!("hidden entry {p} in /dev"),
            Self::TempSetuid(p) => format!("setuid or setgid file {p} in a temporary directory"),
        }
    }

    /// The host path the finding is about, when it is a file.
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::HiddenProcess(_) => None,
            Self::DevFile(p) | Self::DevHidden(p) | Self::TempSetuid(p) => Some(p),
        }
    }
}

/// PIDs up to `max_pid` that `alive` confirms but `listed` misses, skipping
/// threads (whose thread group leader differs). `relisted` runs after the
/// probe so processes that started meanwhile are not reported.
pub fn hidden_processes(
    listed: &HashSet<i32>,
    max_pid: i32,
    alive: impl Fn(i32) -> bool,
    tgid: impl Fn(i32) -> Option<i32>,
    relisted: impl FnOnce() -> HashSet<i32>,
) -> Vec<i32> {
    let is_process = |pid: i32| tgid(pid).is_none_or(|t| t == pid);
    let suspects: Vec<i32> = (1..=max_pid)
        .filter(|p| !listed.contains(p) && alive(*p) && is_process(*p))
        .collect();
    if suspects.is_empty() {
        return suspects;
    }
    let listed = relisted();
    suspects
        .into_iter()
        .filter(|p| !listed.contains(p) && alive(*p) && is_process(*p))
        .collect()
}

/// Whether `pid` exists, by sending it no signal.
pub fn signal_alive(pid: i32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    matches!(kill(Pid::from_raw(pid), None), Ok(()) | Err(Errno::EPERM))
}

fn walk_same_fs(
    root: &Path,
) -> walkdir::FilterEntry<walkdir::IntoIter, impl FnMut(&walkdir::DirEntry) -> bool + use<>> {
    let dev = std::fs::symlink_metadata(root).map(|m| m.dev()).ok();
    WalkDir::new(root)
        .follow_links(false)
        .max_depth(MAX_DEPTH)
        .min_depth(1)
        .into_iter()
        .filter_entry(move |e| e.metadata().is_ok_and(|m| Some(m.dev()) == dev))
}

fn host_path(host_root: &Path, p: &Path) -> String {
    format!("/{}", p.strip_prefix(host_root).unwrap_or(p).display())
}

/// Regular files and dot-named entries in the host `/dev`, staying on its
/// file system so `/dev/shm`, `/dev/pts`, and other mounts are left out. A
/// hidden directory is reported once, not entry by entry.
pub fn dev_anomalies(host_root: &Path) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut it = walk_same_fs(&host_root.join("dev"));
    let mut seen = 0usize;
    while let Some(entry) = it.next() {
        let Ok(e) = entry else { continue };
        seen += 1;
        if seen > MAX_ENTRIES {
            break;
        }
        let name = e.file_name().to_string_lossy();
        let dir = e.file_type().is_dir();
        if DEV_KNOWN.contains(&name.as_ref()) {
            if dir {
                it.skip_current_dir();
            }
            continue;
        }
        let path = host_path(host_root, e.path());
        if name.starts_with('.') {
            out.push(Finding::DevHidden(path));
            if dir {
                it.skip_current_dir();
            }
        } else if e.file_type().is_file() {
            out.push(Finding::DevFile(path));
        }
    }
    out
}

/// Setuid and setgid regular files under the host temporary directories.
pub fn temp_setuid(host_root: &Path) -> Vec<Finding> {
    let mut out = Vec::new();
    for dir in TEMP_DIRS {
        for e in walk_same_fs(&host_root.join(dir))
            .filter_map(Result::ok)
            .take(MAX_ENTRIES)
        {
            let Ok(m) = e.metadata() else { continue };
            if m.is_file() && m.mode() & 0o6000 != 0 {
                out.push(Finding::TempSetuid(host_path(host_root, e.path())));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn reports_processes_missing_from_the_listing() {
        let listed: HashSet<i32> = [1, 2, 5].into();
        let alive = |p: i32| [1, 2, 3, 4, 5, 7].contains(&p);
        // 4 is a thread of 2, 7 started during the probe.
        let tgid = |p: i32| Some(if p == 4 { 2 } else { p });
        let found = hidden_processes(&listed, 10, alive, tgid, || [1, 2, 5, 7].into());
        assert_eq!(found, vec![3]);
        let none = hidden_processes(
            &listed,
            10,
            |p| listed.contains(&p),
            |_| None,
            || unreachable!("nothing to recheck"),
        );
        assert!(none.is_empty());
        assert!(signal_alive(i32::try_from(std::process::id()).unwrap()));
        assert!(!signal_alive(i32::MAX - 1));
    }

    #[test]
    fn finds_dev_and_temp_anomalies() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        std::fs::create_dir_all(r.join("dev/.hid")).unwrap();
        std::fs::create_dir_all(r.join("dev/.udev")).unwrap();
        std::fs::write(r.join("dev/.udev/db"), b"x").unwrap();
        std::fs::write(r.join("dev/.hid/payload"), b"x").unwrap();
        std::fs::write(r.join("dev/stash"), b"x").unwrap();
        std::fs::write(r.join("dev/MAKEDEV"), b"x").unwrap();
        std::fs::create_dir_all(r.join("tmp/a")).unwrap();
        std::fs::create_dir_all(r.join("var/tmp")).unwrap();
        let suid = r.join("tmp/a/sh");
        std::fs::write(&suid, b"x").unwrap();
        std::fs::set_permissions(&suid, std::fs::Permissions::from_mode(0o4755)).unwrap();
        std::fs::write(r.join("var/tmp/plain"), b"x").unwrap();

        let mut dev = dev_anomalies(r);
        dev.sort();
        assert_eq!(
            dev,
            vec![
                Finding::DevFile("/dev/stash".into()),
                Finding::DevHidden("/dev/.hid".into()),
            ]
        );
        let temp = temp_setuid(r);
        assert_eq!(temp, vec![Finding::TempSetuid("/tmp/a/sh".into())]);
        assert_eq!(temp[0].rule(), "setuid-in-temp");
        assert_eq!(temp[0].path(), Some("/tmp/a/sh"));
        assert!(Finding::HiddenProcess(9).message().contains('9'));
        assert!(Finding::HiddenProcess(9).path().is_none());
        assert!(dev_anomalies(&r.join("missing")).is_empty());
    }
}
