//! The process table the behavior engine and storylines read: every live
//! process the agent has seen, keyed by PID.

use std::collections::HashMap;

use crate::policy::ProcessInfo;

const MAX_PROCESSES: usize = 65_536;
const STORYLINE_DEPTH: usize = 8;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub comm: String,
    pub exe: Option<String>,
    pub cmdline: String,
    pub uid: Option<u32>,
    pub container_id: Option<String>,
}

impl Proc {
    /// The executable's basename, else `comm`.
    pub fn name(&self) -> &str {
        self.exe
            .as_deref()
            .and_then(|e| e.rsplit('/').next())
            .filter(|n| !n.is_empty())
            .unwrap_or(&self.comm)
    }
}

#[derive(Default)]
pub struct ProcessTable {
    procs: HashMap<i32, Proc>,
}

impl ProcessTable {
    pub fn insert(&mut self, p: Proc) {
        if self.procs.len() >= MAX_PROCESSES && !self.procs.contains_key(&p.pid) {
            self.procs.clear();
        }
        self.procs.insert(p.pid, p);
    }

    pub fn remove(&mut self, pid: i32) {
        self.procs.remove(&pid);
    }

    pub fn get(&self, pid: i32) -> Option<&Proc> {
        self.procs.get(&pid)
    }

    /// Ancestors from the parent up, as `pid:comm`.
    pub fn storyline(&self, pid: i32) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = self.procs.get(&pid).map(|p| p.ppid);
        while let Some(ppid) = cur {
            if out.len() >= STORYLINE_DEPTH || ppid <= 0 {
                break;
            }
            match self.procs.get(&ppid) {
                Some(p) => {
                    out.push(format!("{}:{}", p.pid, p.comm));
                    cur = (p.ppid != p.pid).then_some(p.ppid);
                }
                None => {
                    out.push(format!("{ppid}:?"));
                    break;
                }
            }
        }
        out
    }

    /// Every live descendant of `pid`.
    pub fn descendants(&self, pid: i32) -> Vec<i32> {
        let mut out = Vec::new();
        let mut frontier = vec![pid];
        while let Some(p) = frontier.pop() {
            for c in self.procs.values().filter(|c| c.ppid == p && c.pid != p) {
                if !out.contains(&c.pid) && out.len() < 1024 {
                    out.push(c.pid);
                    frontier.push(c.pid);
                }
            }
        }
        out
    }

    pub fn info(&self, pid: i32) -> Option<ProcessInfo> {
        let p = self.procs.get(&pid)?;
        Some(ProcessInfo {
            pid: p.pid,
            ppid: p.ppid,
            comm: p.comm.clone(),
            exe: p.exe.clone(),
            cmdline: p.cmdline.clone(),
            uid: p.uid,
            storyline: self.storyline(pid),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: i32, ppid: i32, comm: &str) -> Proc {
        Proc {
            pid,
            ppid,
            comm: comm.into(),
            ..Proc::default()
        }
    }

    #[test]
    fn storyline_and_descendants() {
        let mut t = ProcessTable::default();
        t.insert(p(1, 0, "init"));
        t.insert(p(10, 1, "nginx"));
        t.insert(p(11, 10, "sh"));
        t.insert(p(12, 11, "curl"));
        t.insert(p(13, 11, "sh"));
        t.insert(p(20, 99, "orphan"));
        assert_eq!(t.storyline(12), vec!["11:sh", "10:nginx", "1:init"]);
        assert_eq!(t.storyline(20), vec!["99:?"]);
        let mut d = t.descendants(10);
        d.sort_unstable();
        assert_eq!(d, vec![11, 12, 13]);
        let info = t.info(12).unwrap();
        assert_eq!(info.comm, "curl");
        assert_eq!(info.storyline.len(), 3);
        assert!(t.info(999).is_none());
        t.remove(12);
        assert!(t.get(12).is_none());
    }

    #[test]
    fn name_prefers_exe_basename() {
        let mut x = p(1, 0, "comm");
        assert_eq!(x.name(), "comm");
        x.exe = Some("/usr/bin/bash".into());
        assert_eq!(x.name(), "bash");
        x.exe = Some("/".into());
        assert_eq!(x.name(), "comm");
    }
}
