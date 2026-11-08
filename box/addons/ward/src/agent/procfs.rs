//! Reads processes from `/proc`. With `hostPID`, every container's process
//! is visible, and `/proc/<pid>/root` reaches its filesystem.

use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcStat {
    pub pid: i32,
    pub ppid: i32,
    pub comm: String,
    pub start_time: u64,
}

#[derive(Clone, Debug)]
pub struct ProcFs {
    root: PathBuf,
}

/// Parses `/proc/<pid>/stat`. `comm` may hold spaces and parentheses, so
/// the fields after it are split from the last `)`.
pub fn parse_stat(s: &str) -> Option<ProcStat> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let pid = s[..open].trim().parse().ok()?;
    let comm = s.get(open + 1..close)?.to_string();
    let rest: Vec<&str> = s.get(close + 2..)?.split_whitespace().collect();
    Some(ProcStat {
        pid,
        ppid: rest.get(1)?.parse().ok()?,
        comm,
        start_time: rest.get(19)?.parse().ok()?,
    })
}

fn is_container_id(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The container ID in a `/proc/<pid>/cgroup` document, for containerd,
/// CRI-O, and Docker under the systemd or cgroupfs driver.
pub fn container_id_from_cgroup(s: &str) -> Option<String> {
    for line in s.lines() {
        let path = line.rsplit(':').next().unwrap_or_default();
        for seg in path.rsplit('/') {
            let seg = seg.trim_end_matches(".scope");
            let id = seg.rsplit('-').next().unwrap_or(seg);
            if is_container_id(id) {
                return Some(id.to_string());
            }
        }
    }
    None
}

impl ProcFs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn pid_path(&self, pid: i32, name: &str) -> PathBuf {
        self.root.join(pid.to_string()).join(name)
    }

    /// The host filesystem, through PID 1.
    pub fn host_root(&self) -> PathBuf {
        self.pid_path(1, "root")
    }

    pub fn root_path(&self, pid: i32) -> PathBuf {
        self.pid_path(pid, "root")
    }

    /// Opening this path opens the executable even when it was deleted.
    pub fn exe_path(&self, pid: i32) -> PathBuf {
        self.pid_path(pid, "exe")
    }

    pub fn pids(&self) -> Vec<i32> {
        std::fs::read_dir(&self.root)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .filter_map(|e| e.file_name().to_str()?.parse().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn stat(&self, pid: i32) -> Option<ProcStat> {
        parse_stat(&std::fs::read_to_string(self.pid_path(pid, "stat")).ok()?)
    }

    pub fn exe(&self, pid: i32) -> Option<String> {
        std::fs::read_link(self.exe_path(pid))
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }

    pub fn cmdline(&self, pid: i32) -> String {
        std::fs::read(self.pid_path(pid, "cmdline"))
            .map(|b| args_to_string(&b))
            .unwrap_or_default()
    }

    pub fn uid(&self, pid: i32) -> Option<u32> {
        std::fs::read_to_string(self.pid_path(pid, "status"))
            .ok()?
            .lines()
            .find_map(|l| l.strip_prefix("Uid:"))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    }

    /// The thread group leader of `pid`, which differs for a thread.
    pub fn tgid(&self, pid: i32) -> Option<i32> {
        std::fs::read_to_string(self.pid_path(pid, "status"))
            .ok()?
            .lines()
            .find_map(|l| l.strip_prefix("Tgid:"))?
            .trim()
            .parse()
            .ok()
    }

    /// The highest PID the kernel hands out.
    pub fn pid_max(&self) -> i32 {
        std::fs::read_to_string(self.root.join("sys/kernel/pid_max"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(32_768)
    }

    pub fn cgroup(&self, pid: i32) -> Option<String> {
        std::fs::read_to_string(self.pid_path(pid, "cgroup")).ok()
    }

    pub fn container_id(&self, pid: i32) -> Option<String> {
        container_id_from_cgroup(&self.cgroup(pid)?)
    }

    /// Whether stdin or stdout is a socket, the mark of a reverse shell.
    pub fn stdio_socket(&self, pid: i32) -> bool {
        [0, 1].iter().any(|fd| {
            std::fs::read_link(self.pid_path(pid, &format!("fd/{fd}")))
                .is_ok_and(|t| t.to_string_lossy().starts_with("socket:"))
        })
    }

    /// The agent's own process tree, never acted on.
    pub fn descendants_of(&self, root: i32) -> HashSet<i32> {
        let stats: Vec<ProcStat> = self
            .pids()
            .into_iter()
            .filter_map(|p| self.stat(p))
            .collect();
        let mut out = HashSet::from([root]);
        loop {
            let before = out.len();
            for s in &stats {
                if out.contains(&s.ppid) {
                    out.insert(s.pid);
                }
            }
            if out.len() == before {
                return out;
            }
        }
    }
}

/// NUL-separated argv as one space-separated line, capped at 1024 bytes.
pub fn args_to_string(b: &[u8]) -> String {
    let s: String = String::from_utf8_lossy(b)
        .split('\0')
        .filter(|a| !a.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    s.chars().take(1024).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub struct FakeProc {
        pub dir: tempfile::TempDir,
    }

    impl FakeProc {
        pub fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
            }
        }

        pub fn fs(&self) -> ProcFs {
            ProcFs::new(self.dir.path())
        }

        pub fn add(&self, pid: i32, ppid: i32, comm: &str, exe: &str, cmd: &str, cgroup: &str) {
            let d = self.dir.path().join(pid.to_string());
            std::fs::create_dir_all(d.join("fd")).unwrap();
            std::fs::create_dir_all(d.join("root")).unwrap();
            std::fs::write(
                d.join("stat"),
                format!("{pid} ({comm}) S {ppid} 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 12345 0 0"),
            )
            .unwrap();
            std::fs::write(d.join("cmdline"), cmd.replace(' ', "\0")).unwrap();
            std::fs::write(d.join("status"), "Name:\tx\nUid:\t1000\t1000\t1000\t1000\n").unwrap();
            std::fs::write(d.join("cgroup"), cgroup).unwrap();
            let bin = self.dir.path().join(format!("bin-{pid}"));
            std::fs::create_dir_all(&bin).unwrap();
            let target = bin.join(comm);
            std::fs::write(&target, exe.as_bytes()).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, d.join("exe")).unwrap();
        }

        #[cfg(unix)]
        pub fn socket_stdio(&self, pid: i32) {
            let d = self.dir.path().join(pid.to_string()).join("fd");
            std::os::unix::fs::symlink("socket:[1234]", d.join("0")).unwrap();
        }
    }

    pub const CID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn stat_handles_odd_comm() {
        let s =
            parse_stat("42 (we ird) (x)) S 7 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 999 0").unwrap();
        assert_eq!(s.pid, 42);
        assert_eq!(s.ppid, 7);
        assert_eq!(s.comm, "we ird) (x)");
        assert_eq!(s.start_time, 999);
        assert!(parse_stat("garbage").is_none());
    }

    #[test]
    fn container_ids_from_every_runtime() {
        for doc in [
            format!("0::/kubepods.slice/kubepods-pod1.slice/cri-containerd-{CID}.scope"),
            format!("0::/kubepods/burstable/pod1/{CID}"),
            format!("12:pids:/kubepods/pod1/crio-{CID}.scope\n0::/"),
            format!("0::/system.slice/docker-{CID}.scope"),
        ] {
            assert_eq!(
                container_id_from_cgroup(&doc).as_deref(),
                Some(CID),
                "{doc}"
            );
        }
        assert!(container_id_from_cgroup("0::/user.slice/session-1.scope").is_none());
    }

    #[test]
    fn reads_a_fake_proc() {
        let p = FakeProc::new();
        p.add(
            10,
            1,
            "bash",
            "elf",
            "bash -i",
            &format!("0::/kubepods/pod1/{CID}"),
        );
        p.add(11, 10, "sleep", "elf", "sleep 100", "0::/");
        p.add(12, 99, "other", "elf", "other", "0::/");
        let fs = p.fs();
        let mut pids = fs.pids();
        pids.sort_unstable();
        assert_eq!(pids, vec![10, 11, 12]);
        assert_eq!(fs.stat(10).unwrap().comm, "bash");
        assert_eq!(fs.cmdline(10), "bash -i");
        assert_eq!(fs.uid(10), Some(1000));
        assert_eq!(fs.container_id(10).as_deref(), Some(CID));
        assert!(fs.container_id(11).is_none());
        assert!(fs.exe(10).unwrap().ends_with("bin-10/bash"));
        assert!(!fs.stdio_socket(10));
        p.socket_stdio(10);
        assert!(fs.stdio_socket(10));
        assert_eq!(fs.descendants_of(10), HashSet::from([10, 11]));
        assert!(fs.host_root().ends_with("1/root"));
        assert!(fs.root_path(10).ends_with("10/root"));
        assert!(fs.stat(999).is_none());
        assert!(fs.tgid(999).is_none());
        assert_eq!(fs.pid_max(), 32_768);
        assert_eq!(fs.cmdline(999), "");
        assert!(ProcFs::new("/nonexistent").pids().is_empty());
    }

    #[test]
    fn args_join() {
        assert_eq!(args_to_string(b"sh\0-c\0echo hi\0"), "sh -c echo hi");
        assert_eq!(args_to_string(&[b'a'; 2000]).len(), 1024);
    }
}
