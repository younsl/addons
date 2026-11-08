//! Loads the embedded eBPF object, resolves kernel offsets into its
//! globals, attaches the probes, and streams ring buffer records.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::mpsc;
use ward_ebpf_common::{
    CONFIG_ENFORCE, CONFIG_SELF_TGID, FIM_INODES_MAX, InodeKey, UNKNOWN_OFFSET, globals, inode_key,
    names,
};

use super::btf::Btf;
use super::events::{self, ProcessEvent};
use super::metrics::AgentMetrics;
use super::response::Blocker;
use super::tracefs::read_format;
use super::whodata::InodeWatch;

/// The object for this architecture, empty when the build had none.
pub const OBJECT: &[u8] = aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/ward.bpf.o"));

#[derive(Debug, thiserror::Error)]
pub enum EbpfError {
    #[error("this build has no eBPF object")]
    NoObject,
    #[error("load: {0}")]
    Load(String),
    #[error("attach {program}: {reason}")]
    Attach { program: String, reason: String },
}

/// Offsets for every global the object reads.
pub fn offsets(btf: Option<&Btf>, tracefs: &Path) -> Vec<(&'static str, u32)> {
    let exec = read_format(tracefs, "sched", "sched_process_exec");
    let sock = read_format(tracefs, "sock", "inet_sock_set_state");
    let tp = |m: &std::collections::HashMap<String, u32>, f: &str, default: u32| {
        m.get(f).copied().unwrap_or(if m.is_empty() {
            default
        } else {
            UNKNOWN_OFFSET
        })
    };
    let k = |s: &str, f: &str| btf.and_then(|b| b.offset(s, f)).unwrap_or(UNKNOWN_OFFSET);
    vec![
        (globals::TP_EXEC_FILENAME, tp(&exec, "filename", 8)),
        (globals::TP_SOCK_NEWSTATE, tp(&sock, "newstate", 20)),
        (globals::TP_SOCK_DPORT, tp(&sock, "dport", 26)),
        (globals::TP_SOCK_FAMILY, tp(&sock, "family", 28)),
        (globals::TP_SOCK_PROTOCOL, tp(&sock, "protocol", 30)),
        (globals::TP_SOCK_DADDR, tp(&sock, "daddr", 36)),
        (globals::TP_SOCK_DADDR_V6, tp(&sock, "daddr_v6", 56)),
        (globals::TASK_REAL_PARENT, k("task_struct", "real_parent")),
        (globals::TASK_TGID, k("task_struct", "tgid")),
        (globals::TASK_MM, k("task_struct", "mm")),
        (globals::MM_ARG_START, k("mm_struct", "arg_start")),
        (globals::MM_ARG_END, k("mm_struct", "arg_end")),
        (globals::BPRM_FILE, k("linux_binprm", "file")),
        (globals::FILE_INODE, k("file", "f_inode")),
        (globals::INODE_INO, k("inode", "i_ino")),
        (globals::INODE_SB, k("inode", "i_sb")),
        (globals::SB_DEV, k("super_block", "s_dev")),
        (globals::FILE_F_MODE, k("file", "f_mode")),
        (globals::DENTRY_D_NAME, k("dentry", "d_name")),
        (globals::QSTR_NAME, k("qstr", "name")),
        (globals::PATH_DENTRY, k("path", "dentry")),
        (globals::DENTRY_D_INODE, k("dentry", "d_inode")),
    ]
}

/// Mode and owner hooks. They need `CONFIG_SECURITY_PATH`, so a kernel
/// without it still gets whodata for content and entry changes.
const FIM_ATTRIB_PROGRAMS: [(&str, &str); 2] = [
    (names::PROG_PATH_CHMOD, "path_chmod"),
    (names::PROG_PATH_CHOWN, "path_chown"),
];

/// The FIM hooks and the kernel functions they attach to.
const FIM_PROGRAMS: [(&str, &str); 4] = [
    (names::PROG_FILE_OPEN, "file_open"),
    (names::PROG_INODE_CREATE, "inode_create"),
    (names::PROG_INODE_UNLINK, "inode_unlink"),
    (names::PROG_INODE_RENAME, "inode_rename"),
];

type BlockMap = aya::maps::HashMap<aya::maps::MapData, [u8; 16], u32>;
type ConfigMap = aya::maps::Array<aya::maps::MapData, u64>;

type FimMap = aya::maps::HashMap<aya::maps::MapData, InodeKey, u32>;
/// The FIM inode map with the keys the agent last wrote into it.
type FimInodes = Mutex<(FimMap, HashSet<InodeKey>)>;

pub struct Kernel {
    _ebpf: aya::Ebpf,
    blocklist: Mutex<BlockMap>,
    config: Mutex<ConfigMap>,
    entries: AtomicUsize,
    pub lsm: bool,
    /// The FIM hooks' inode set, present when they are attached.
    fim: Option<FimInodes>,
}

impl Kernel {
    pub const fn whodata(&self) -> bool {
        self.fim.is_some()
    }
}

impl InodeWatch for Kernel {
    fn watch(&self, inodes: &[(u64, u64)]) -> Result<usize, String> {
        let fim = self.fim.as_ref().ok_or("whodata hooks are not attached")?;
        let mut guard = fim.lock().map_err(|e| e.to_string())?;
        let (map, current) = &mut *guard;
        let want: HashSet<InodeKey> = inodes
            .iter()
            .take(FIM_INODES_MAX as usize)
            .map(|(d, i)| inode_key(*d, *i))
            .collect();
        current.retain(|k| {
            let keep = want.contains(k);
            if !keep {
                let _ = map.remove(k);
            }
            keep
        });
        for key in want {
            if current.insert(key)
                && let Err(e) = map.insert(&key, &1, 0)
            {
                current.remove(&key);
                return Err(e.to_string());
            }
        }
        Ok(current.len())
    }
}

impl Kernel {
    pub const fn mode(&self) -> &'static str {
        if self.lsm { "ebpf-lsm" } else { "ebpf" }
    }
}

impl Blocker for Kernel {
    fn block(&self, dev: u64, ino: u64) -> Result<(), String> {
        if !self.lsm {
            return Err("BPF LSM is not attached".into());
        }
        self.blocklist
            .lock()
            .map_err(|e| e.to_string())?
            .insert(&inode_key(dev, ino), &1, 0)
            .map_err(|e| e.to_string())?;
        self.entries.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn set_enforce(&self, enforce: bool) -> Result<(), String> {
        self.config
            .lock()
            .map_err(|e| e.to_string())?
            .set(CONFIG_ENFORCE, &u64::from(enforce), 0)
            .map_err(|e| e.to_string())
    }

    fn entries(&self) -> usize {
        self.entries.load(Ordering::Relaxed)
    }
}

/// An error with every source, so a refused map or program names its errno.
fn chain(e: &dyn std::error::Error) -> String {
    let mut msg = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        src = s.source();
    }
    msg
}

/// Loads and attaches every program. The LSM program is optional: without
/// the BPF LSM the agent still observes, it only cannot deny exec.
pub fn load(
    btf_path: &Path,
    tracefs: &Path,
    tx: mpsc::Sender<ProcessEvent>,
    metrics: AgentMetrics,
) -> Result<std::sync::Arc<Kernel>, EbpfError> {
    if OBJECT.is_empty() {
        return Err(EbpfError::NoObject);
    }
    let btf = std::fs::read(btf_path)
        .ok()
        .and_then(|b| Btf::parse(&b).ok());
    let offsets = offsets(btf.as_ref(), tracefs);
    let mut loader = aya::EbpfLoader::new();
    for (name, value) in &offsets {
        loader.override_global(name, value, true);
    }
    let mut ebpf = loader
        .load(OBJECT)
        .map_err(|e| EbpfError::Load(chain(&e)))?;

    attach_tracepoints(&mut ebpf)?;

    let lsm_ready = offsets
        .iter()
        .filter(|(n, _)| {
            [
                globals::BPRM_FILE,
                globals::FILE_INODE,
                globals::INODE_INO,
                globals::INODE_SB,
                globals::SB_DEV,
            ]
            .contains(n)
        })
        .all(|(_, v)| *v != UNKNOWN_OFFSET);
    let lsm = lsm_ready
        && match attach_lsm(&mut ebpf) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "BPF LSM unavailable, exec blocking is off");
                false
            }
        };
    let fim = if lsm {
        whodata(&mut ebpf, &offsets)?
    } else {
        None
    };

    let mut config: ConfigMap = ebpf
        .take_map(names::CONFIG)
        .ok_or_else(|| EbpfError::Load("CONFIG map missing".into()))?
        .try_into()
        .map_err(|e: aya::maps::MapError| EbpfError::Load(e.to_string()))?;
    config
        .set(CONFIG_SELF_TGID, &u64::from(std::process::id()), 0)
        .map_err(|e| EbpfError::Load(e.to_string()))?;
    let blocklist: BlockMap = ebpf
        .take_map(names::BLOCKLIST)
        .ok_or_else(|| EbpfError::Load("BLOCKLIST map missing".into()))?
        .try_into()
        .map_err(|e: aya::maps::MapError| EbpfError::Load(e.to_string()))?;
    let ring: aya::maps::RingBuf<aya::maps::MapData> = ebpf
        .take_map(names::EVENTS)
        .ok_or_else(|| EbpfError::Load("EVENTS map missing".into()))?
        .try_into()
        .map_err(|e: aya::maps::MapError| EbpfError::Load(e.to_string()))?;
    // The AsyncFd owns the ring buffer and with it the fd, so the I/O safety
    // concern behind the deprecation does not apply.
    #[allow(deprecated)]
    let mut fd = tokio::io::unix::AsyncFd::new(ring).map_err(|e| EbpfError::Load(e.to_string()))?;
    tokio::spawn(async move {
        loop {
            let Ok(mut guard) = fd.readable_mut().await else {
                return;
            };
            let ring = guard.get_inner_mut();
            while let Some(item) = ring.next() {
                if let Some(ev) = events::parse(&item)
                    && tx.try_send(ev).is_err()
                {
                    metrics.events_dropped.inc();
                }
            }
            guard.clear_ready();
        }
    });

    Ok(std::sync::Arc::new(Kernel {
        _ebpf: ebpf,
        blocklist: Mutex::new(blocklist),
        config: Mutex::new(config),
        entries: AtomicUsize::new(0),
        lsm,
        fim,
    }))
}

/// Attaches the FIM hooks and takes their inode map, `None` when the
/// kernel lacks a field they read or refuses a hook.
fn whodata(
    ebpf: &mut aya::Ebpf,
    offsets: &[(&'static str, u32)],
) -> Result<Option<FimInodes>, EbpfError> {
    let ready = offsets
        .iter()
        .any(|(n, v)| *n == globals::FILE_F_MODE && *v != UNKNOWN_OFFSET);
    if !ready {
        return Ok(None);
    }
    if let Err(e) = attach_fim(ebpf) {
        tracing::warn!(error = %e, "FIM hooks unavailable, integrity changes carry no whodata");
        return Ok(None);
    }
    let map: FimMap = ebpf
        .take_map(names::FIM_INODES)
        .ok_or_else(|| EbpfError::Load("FIM_INODES map missing".into()))?
        .try_into()
        .map_err(|e: aya::maps::MapError| EbpfError::Load(e.to_string()))?;
    Ok(Some(Mutex::new((map, HashSet::new()))))
}

fn attach_fim(ebpf: &mut aya::Ebpf) -> Result<(), String> {
    let btf = aya::Btf::from_sys_fs().map_err(|e| e.to_string())?;
    for (name, hook) in FIM_PROGRAMS {
        let p: &mut aya::programs::Lsm = ebpf
            .program_mut(name)
            .ok_or_else(|| format!("{name} missing"))?
            .try_into()
            .map_err(|e: aya::programs::ProgramError| e.to_string())?;
        p.load(hook, &btf).map_err(|e| format!("{hook}: {e}"))?;
        p.attach().map_err(|e| format!("{hook}: {e}"))?;
    }
    for (name, hook) in FIM_ATTRIB_PROGRAMS {
        let attached = ebpf
            .program_mut(name)
            .ok_or_else(|| format!("{name} missing"))
            .and_then(|p| {
                let p: &mut aya::programs::Lsm = p
                    .try_into()
                    .map_err(|e: aya::programs::ProgramError| e.to_string())?;
                p.load(hook, &btf).map_err(|e| e.to_string())?;
                p.attach().map(|_| ()).map_err(|e| e.to_string())
            });
        if let Err(e) = attached {
            tracing::warn!(hook, error = %e, "mode and owner changes carry no whodata");
        }
    }
    Ok(())
}

fn attach_tracepoints(ebpf: &mut aya::Ebpf) -> Result<(), EbpfError> {
    for (prog, category, event) in [
        (names::PROG_EXEC, "sched", "sched_process_exec"),
        (names::PROG_EXIT, "sched", "sched_process_exit"),
        (names::PROG_CONNECT, "sock", "inet_sock_set_state"),
    ] {
        let attach = |e: String| EbpfError::Attach {
            program: prog.into(),
            reason: e,
        };
        let p: &mut aya::programs::TracePoint = ebpf
            .program_mut(prog)
            .ok_or_else(|| attach("missing".into()))?
            .try_into()
            .map_err(|e: aya::programs::ProgramError| attach(e.to_string()))?;
        p.load().map_err(|e| attach(e.to_string()))?;
        p.attach(category, event)
            .map_err(|e| attach(e.to_string()))?;
    }
    Ok(())
}

fn attach_lsm(ebpf: &mut aya::Ebpf) -> Result<(), String> {
    let btf = aya::Btf::from_sys_fs().map_err(|e| e.to_string())?;
    let p: &mut aya::programs::Lsm = ebpf
        .program_mut(names::PROG_BPRM_CHECK)
        .ok_or("missing")?
        .try_into()
        .map_err(|e: aya::programs::ProgramError| e.to_string())?;
    p.load("bprm_check_security", &btf)
        .map_err(|e| e.to_string())?;
    p.attach().map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_fall_back_without_tracefs_or_btf() {
        let dir = tempfile::tempdir().unwrap();
        let o: std::collections::HashMap<_, _> = offsets(None, dir.path()).into_iter().collect();
        assert_eq!(o[globals::TP_EXEC_FILENAME], 8);
        assert_eq!(o[globals::TASK_TGID], UNKNOWN_OFFSET);
        let btf = Btf::parse(&super::super::btf::tests::kernel()).unwrap();
        let o: std::collections::HashMap<_, _> =
            offsets(Some(&btf), dir.path()).into_iter().collect();
        assert_eq!(o[globals::TASK_TGID], 1240);
        assert_eq!(o[globals::MM_ARG_END], 48);
    }
}
