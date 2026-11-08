//! Who changed a watched file, after Wazuh whodata. The BPF LSM hooks
//! report each write to a watched inode and each create, delete, or rename
//! in a watched directory with the process behind it. The agent keeps the
//! latest actor per inode and per directory entry for a while, and an
//! integrity detection carries the actor of its change.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::policy::{ProcessInfo, Workload};

/// How long an actor stays attributable. A periodic check finds changes up
/// to an interval late, real-time ones within a second.
const TTL: Duration = Duration::from_mins(15);
const MAX_ENTRIES: usize = 50_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Write,
    Create,
    Delete,
    RenameFrom,
    RenameTo,
    /// A mode or owner change.
    Attrib,
}

impl Op {
    pub const fn from_raw(op: u32) -> Option<Self> {
        match op {
            ward_ebpf_common::FIM_WRITE => Some(Self::Write),
            ward_ebpf_common::FIM_CREATE => Some(Self::Create),
            ward_ebpf_common::FIM_DELETE => Some(Self::Delete),
            ward_ebpf_common::FIM_RENAME_FROM => Some(Self::RenameFrom),
            ward_ebpf_common::FIM_RENAME_TO => Some(Self::RenameTo),
            ward_ebpf_common::FIM_ATTRIB => Some(Self::Attrib),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Create => "create",
            Self::Delete => "delete",
            Self::RenameFrom => "rename-from",
            Self::RenameTo => "rename-to",
            Self::Attrib => "attrib",
        }
    }
}

/// The process behind one change.
#[derive(Clone, Debug)]
pub struct Actor {
    pub op: Op,
    pub process: ProcessInfo,
    pub workload: Option<Workload>,
    at: Instant,
}

/// The kernel side of whodata: the set of inodes the FIM hooks report on.
pub trait InodeWatch: Send + Sync {
    /// Replaces the watched set with `inodes` (kernel `dev_t`, inode) and
    /// returns how many are watched.
    fn watch(&self, inodes: &[(u64, u64)]) -> Result<usize, String>;
}

#[derive(Default)]
pub struct Whodata {
    /// Writers by the written file's `(dev, ino)`.
    writes: HashMap<(u64, u64), Actor>,
    /// Mode and owner changers by the file's `(dev, ino)`.
    attribs: HashMap<(u64, u64), Actor>,
    /// Entry changes by the directory's `(dev, ino)` and the entry name.
    entries: HashMap<(u64, u64, String), Actor>,
}

impl Whodata {
    pub fn record(
        &mut self,
        op: Op,
        dev: u64,
        ino: u64,
        name: &str,
        process: ProcessInfo,
        workload: Option<Workload>,
    ) {
        let now = Instant::now();
        if self.len() >= MAX_ENTRIES {
            self.writes.retain(|_, a| now.duration_since(a.at) < TTL);
            self.attribs.retain(|_, a| now.duration_since(a.at) < TTL);
            self.entries.retain(|_, a| now.duration_since(a.at) < TTL);
            if self.len() >= MAX_ENTRIES {
                self.writes.clear();
                self.attribs.clear();
                self.entries.clear();
            }
        }
        let actor = Actor {
            op,
            process,
            workload,
            at: now,
        };
        match op {
            Op::Write => {
                self.writes.insert((dev, ino), actor);
            }
            Op::Attrib => {
                self.attribs.insert((dev, ino), actor);
            }
            _ => {
                self.entries.insert((dev, ino, name.to_string()), actor);
            }
        }
    }

    fn len(&self) -> usize {
        self.writes.len() + self.attribs.len() + self.entries.len()
    }

    /// An actor recorded after `since`, so a change is never pinned on a
    /// process that acted before the previous check.
    fn fresh(a: Option<&Actor>, since: Instant) -> Option<&Actor> {
        a.filter(|a| a.at >= since && a.at.elapsed() < TTL)
    }

    /// The last process that wrote the file `(dev, ino)` after `since`.
    pub fn writer(&self, dev: u64, ino: u64, since: Instant) -> Option<&Actor> {
        Self::fresh(self.writes.get(&(dev, ino)), since)
    }

    /// The last process that changed the mode or owner of `(dev, ino)`.
    pub fn attrib(&self, dev: u64, ino: u64, since: Instant) -> Option<&Actor> {
        Self::fresh(self.attribs.get(&(dev, ino)), since)
    }

    /// The last process that created, deleted, or renamed `name` in the
    /// directory `(dev, ino)` after `since`.
    pub fn entry(&self, dev: u64, ino: u64, name: &str, since: Instant) -> Option<&Actor> {
        Self::fresh(self.entries.get(&(dev, ino, name.to_string())), since)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: i32, comm: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            comm: comm.into(),
            ..ProcessInfo::default()
        }
    }

    #[test]
    fn keeps_the_latest_actor_per_inode_and_entry() {
        let mut w = Whodata::default();
        let start = Instant::now();
        w.record(Op::Write, 1, 10, "", proc(5, "vi"), None);
        w.record(Op::Write, 1, 10, "", proc(6, "sed"), None);
        w.record(Op::Create, 1, 2, "evil", proc(7, "sh"), None);
        assert_eq!(w.writer(1, 10, start).unwrap().process.comm, "sed");
        assert!(w.writer(1, 11, start).is_none());
        assert!(
            w.writer(1, 10, Instant::now()).is_none(),
            "an actor from before the last check is stale"
        );
        w.record(Op::Attrib, 1, 10, "", proc(8, "chmod"), None);
        assert_eq!(w.attrib(1, 10, start).unwrap().process.comm, "chmod");
        assert_eq!(w.writer(1, 10, start).unwrap().process.comm, "sed");
        let a = w.entry(1, 2, "evil", start).unwrap();
        assert_eq!(a.op, Op::Create);
        assert_eq!(a.process.pid, 7);
        assert!(w.entry(1, 2, "other", start).is_none());
        for (raw, op) in [
            (ward_ebpf_common::FIM_WRITE, Op::Write),
            (ward_ebpf_common::FIM_CREATE, Op::Create),
            (ward_ebpf_common::FIM_DELETE, Op::Delete),
            (ward_ebpf_common::FIM_RENAME_FROM, Op::RenameFrom),
            (ward_ebpf_common::FIM_RENAME_TO, Op::RenameTo),
            (ward_ebpf_common::FIM_ATTRIB, Op::Attrib),
        ] {
            assert_eq!(Op::from_raw(raw), Some(op));
            assert!(!op.as_str().is_empty());
        }
        assert_eq!(Op::from_raw(99), None);
    }

    #[test]
    fn stays_bounded() {
        let mut w = Whodata::default();
        for i in 0..(MAX_ENTRIES as u64 + 10) {
            w.record(Op::Write, 1, i, "", proc(1, "x"), None);
        }
        assert!(w.writes.len() <= MAX_ENTRIES);
    }
}
