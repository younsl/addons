//! Reconcile loop over the desired state in the config file.
//!
//! Shaped like a kube-rs controller: [`reconcile`] does the work,
//! [`error_policy`] turns a failure into a report, [`State`] is what the web
//! server reads, and [`run`] drives it on an interval or on request.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use jiff::Timestamp;
use serde::Serialize;
use tokio::sync::{Notify, watch};
use tokio::time::{self, MissedTickBehavior};

use crate::config::Config;
use crate::files::Snapshot;
use crate::linker::{self, Backups, Entry};
use crate::metrics::Metrics;
use crate::packages::{self, FileReport};
use crate::spec::Spec;
use crate::{Error, Result};

/// What one reconcile needs, built once from the settings.
pub struct Context {
    pub config_file: PathBuf,
    pub home: PathBuf,
    pub dry_run: bool,
    pub metrics: Arc<Metrics>,
    /// Set while a missing mount has been warned about, so the warning
    /// repeats only after the mount comes back and goes missing again.
    pub(crate) mount_warned: AtomicBool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub started_at: Timestamp,
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_dir: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub entries: Vec<Entry>,
    pub packages: Vec<FileReport>,
    /// Tracked source files for the console viewer, served by /api/tree and /api/file.
    #[serde(skip)]
    pub files: Arc<Snapshot>,
}

impl Report {
    fn new(started_at: Timestamp, dry_run: bool) -> Self {
        Self {
            started_at,
            dry_run,
            backup_dir: None,
            error: None,
            entries: Vec::new(),
            packages: Vec::new(),
            files: Arc::default(),
        }
    }

    pub fn changed(&self) -> usize {
        self.entries.iter().filter(|e| e.applied).count()
    }

    pub fn failed(&self) -> usize {
        self.entries.iter().filter(|e| e.error.is_some()).count()
    }

    pub const fn succeeded(&self) -> bool {
        self.error.is_none()
    }
}

/// Last reconcile as served by the web server.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Diagnostics {
    pub last_report: Option<Report>,
}

/// State shared between the reconcile loop and the web server.
#[derive(Clone, Default)]
pub struct State {
    diagnostics: Arc<RwLock<Diagnostics>>,
    metrics: Arc<Metrics>,
    trigger: Arc<Notify>,
}

impl State {
    pub fn new(metrics: Metrics) -> Self {
        Self {
            metrics: Arc::new(metrics),
            ..Self::default()
        }
    }

    pub fn metrics(&self) -> Result<String, std::fmt::Error> {
        self.metrics.render()
    }

    pub fn diagnostics(&self) -> Diagnostics {
        self.diagnostics
            .read()
            .map(|d| d.clone())
            .unwrap_or_default()
    }

    pub fn report(&self) -> Option<Report> {
        self.diagnostics().last_report
    }

    pub fn is_ready(&self) -> bool {
        self.report().is_some_and(|r| r.succeeded())
    }

    /// Queue a reconcile. Requests made while one runs collapse into one more run.
    pub fn request_reconcile(&self) {
        self.trigger.notify_one();
    }

    pub fn publish(&self, report: Report) {
        if let Ok(mut diagnostics) = self.diagnostics.write() {
            diagnostics.last_report = Some(report);
        }
    }

    pub fn to_context(&self, cfg: &Config) -> Arc<Context> {
        Arc::new(Context {
            config_file: cfg.file.clone(),
            home: cfg.home.clone(),
            dry_run: cfg.dry_run,
            metrics: Arc::clone(&self.metrics),
            mount_warned: AtomicBool::new(false),
        })
    }
}

/// Bring links and package files in line with the config file. Per-entry
/// failures stay in the report; only an unreadable config is an error.
pub fn reconcile(ctx: &Context, now: Timestamp) -> Result<Report> {
    let spec = Spec::load(&ctx.config_file, &ctx.home)?;
    let mut report = Report::new(now, ctx.dry_run);

    let steps = linker::plan(&spec);
    if ctx.dry_run {
        report.entries = linker::preview(steps);
    } else {
        let stamp = now.strftime("%Y%m%d-%H%M%S").to_string();
        let mut backups = Backups::new(&spec.backup_root, &stamp, &ctx.home);
        report.entries = linker::apply(steps, &mut backups);
        report.backup_dir = backups.used_dir().map(Path::to_path_buf);
    }

    if let Some(spec) = &spec.packages {
        let date = now.strftime("%Y-%m-%d").to_string();
        report.packages = packages::sync(spec, &ctx.home, &date, ctx.dry_run);
    }

    let roots: Vec<PathBuf> = report
        .entries
        .iter()
        .map(|e| e.step.source.clone())
        .chain(report.packages.iter().map(|p| p.path.clone()))
        .collect();
    report.files = Arc::new(Snapshot::collect(&spec.source_root, &roots));
    Ok(report)
}

/// Record a failed reconcile and turn it into a report the console can show.
pub fn error_policy(error: &Error, ctx: &Context, now: Timestamp) -> Report {
    ctx.metrics.reconcile_failure(error);
    let mut report = Report::new(now, ctx.dry_run);
    report.error = Some(error.to_string());
    report
}

/// Reconcile on every tick or request until `shutdown` changes. A run in
/// progress always finishes, so no link is left half replaced.
pub async fn run(
    state: State,
    ctx: Arc<Context>,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            _ = ticker.tick() => {},
            () = state.trigger.notified() => tracing::info!("reconcile requested"),
        }
        reconcile_once(&state, &ctx).await;
    }
    tracing::info!("controller stopped");
}

async fn reconcile_once(state: &State, ctx: &Arc<Context>) {
    let _measure = ctx.metrics.count_and_measure();
    let worker = Arc::clone(ctx);
    let joined = tokio::task::spawn_blocking(move || {
        let now = Timestamp::now();
        reconcile(&worker, now).map_err(|err| (now, err))
    })
    .await;
    let report = match joined {
        Ok(Ok(report)) => report,
        Ok(Err((now, err))) => {
            tracing::error!(error = %err, "reconcile failed");
            error_policy(&err, ctx, now)
        }
        Err(err) => {
            tracing::error!(error = %err, "reconcile task panicked");
            return;
        }
    };

    if report.succeeded() {
        tracing::info!(
            dry_run = report.dry_run,
            entries = report.entries.len(),
            changed = report.changed(),
            failed = report.failed(),
            backup_dir = report.backup_dir.as_ref().map(|p| p.display().to_string()),
            "reconcile finished"
        );
    }
    for entry in report
        .entries
        .iter()
        .filter(|e| e.applied || e.error.is_some())
    {
        tracing::info!(
            action = entry.step.action.as_str(),
            target = %entry.step.target.display(),
            source = %entry.step.source.display(),
            error = entry.error.as_deref(),
            "link"
        );
    }
    for file in report
        .packages
        .iter()
        .filter(|p| p.updated || p.error.is_some())
    {
        tracing::info!(
            path = %file.path.display(),
            entries = file.entries,
            updated = file.updated,
            error = file.error.as_deref(),
            "package file"
        );
    }

    warn_missing_mount(ctx, &report);
    ctx.metrics.observe(&report);
    state.publish(report);
}

fn warn_missing_mount(ctx: &Context, report: &Report) {
    let missing = report
        .packages
        .iter()
        .find_map(|p| p.missing_mount.as_deref());
    match missing {
        Some(prefix) if !ctx.mount_warned.swap(true, Ordering::SeqCst) => {
            tracing::warn!(
                prefix = %prefix.display(),
                "Homebrew prefix is not visible in the container, so the Brewfile is not refreshed. \
                 Share it with the Podman VM (podman machine init --volume {0}:{0}) \
                 and run tether with -v {0}:{0}:ro",
                prefix.display()
            );
        }
        Some(_) => {}
        None => ctx.mount_warned.store(false, Ordering::SeqCst),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::fixtures::TempHome;
    use crate::linker::Action;

    const SPEC: &str = "source_root = \"~/repo\"\nbackup_root = \"~/.backup\"\n\n[[links]]\nsource = \"zshrc\"\ntarget = \"~/.zshrc\"\n";

    fn setup(dry_run: bool) -> (TempHome, Arc<Context>) {
        let home = TempHome::new();
        fs::write(home.source_file("zshrc"), "zsh").expect("source");
        fs::write(home.home.join(".zshrc"), "local").expect("existing target");
        let ctx = home.context(SPEC, dry_run);
        (home, ctx)
    }

    fn now() -> Timestamp {
        "2026-01-02T03:04:05Z".parse().expect("timestamp")
    }

    #[test]
    fn reconcile_applies_and_backs_up() {
        let (home, ctx) = setup(false);
        let report = reconcile(&ctx, now()).expect("reconcile");

        assert!(report.succeeded());
        assert_eq!(report.changed(), 1);
        assert_eq!(report.failed(), 0);
        assert_eq!(report.entries[0].step.action, Action::Backup);
        assert_eq!(
            report.backup_dir,
            Some(home.home.join(".backup/20260102-030405"))
        );
        assert!(
            fs::symlink_metadata(home.home.join(".zshrc"))
                .expect("target")
                .is_symlink()
        );
        assert_eq!(report.packages.len(), 0);

        let again = reconcile(&ctx, now()).expect("again");
        assert_eq!(again.changed(), 0);
        assert!(again.backup_dir.is_none());
    }

    #[test]
    fn dry_run_changes_nothing() {
        let (home, ctx) = setup(true);
        let report = reconcile(&ctx, now()).expect("reconcile");
        assert!(report.dry_run);
        assert_eq!(report.changed(), 0);
        assert_eq!(report.entries[0].step.action, Action::Backup);
        assert_eq!(
            fs::read_to_string(home.home.join(".zshrc")).expect("untouched"),
            "local"
        );
    }

    #[test]
    fn packages_are_synced_when_configured() {
        let home = TempHome::new();
        let receipts = home.home.join(".krew/receipts");
        fs::create_dir_all(&receipts).expect("receipts");
        fs::write(
            receipts.join("stern.yaml"),
            "status:\n  source:\n    name: default\n",
        )
        .expect("receipt");
        let ctx = home.context(
            "source_root = \"~/repo\"\nbackup_root = \"~/b\"\n\n[packages]\nkrewfile = \"krewfile\"\n",
            false,
        );

        let report = reconcile(&ctx, now()).expect("reconcile");
        assert_eq!(report.packages.len(), 1);
        assert!(report.packages[0].updated);
        assert_eq!(report.packages[0].entries, 1);
        let krewfile = fs::read_to_string(home.repo.join("krewfile")).expect("krewfile");
        assert!(krewfile.contains("# Backup completed on 2026-01-02"));
        assert!(krewfile.ends_with("stern\n"));
    }

    #[tokio::test]
    async fn missing_homebrew_prefix_is_reported_and_warned_once() {
        let home = TempHome::new();
        let ctx = home.context(
            "source_root = \"~/repo\"\nbackup_root = \"~/b\"\n\n[packages]\nbrewfile = \"Brewfile\"\nhomebrew_prefix = \"/nonexistent/homebrew\"\n",
            false,
        );
        let report = reconcile(&ctx, now()).expect("reconcile");
        assert_eq!(
            report.packages[0].missing_mount.as_deref(),
            Some(Path::new("/nonexistent/homebrew"))
        );
        assert!(report.packages[0].error.is_some());
        assert!(!home.repo.join("Brewfile").exists(), "nothing written");

        let state = State::default();
        reconcile_once(&state, &ctx).await;
        assert!(ctx.mount_warned.load(Ordering::SeqCst));
        reconcile_once(&state, &ctx).await;
        assert!(ctx.mount_warned.load(Ordering::SeqCst));

        warn_missing_mount(&ctx, &Report::new(now(), false));
        assert!(
            !ctx.mount_warned.load(Ordering::SeqCst),
            "reset once the mount is back"
        );
    }

    #[test]
    fn spec_error_becomes_failed_report() {
        let (home, ctx) = setup(false);
        fs::remove_file(home.root.join("config.toml")).expect("remove");
        let err = reconcile(&ctx, now()).expect_err("missing config");
        let report = error_policy(&err, &ctx, now());
        assert!(!report.succeeded());
        assert_eq!(report.entries.len(), 0);
        assert!(
            ctx.metrics
                .render()
                .expect("render")
                .contains("tether_reconcile_failures_total{error=\"config_read\"} 1")
        );
    }

    #[test]
    fn state_tracks_readiness_and_reports() {
        let state = State::default();
        assert!(!state.is_ready());
        assert!(state.report().is_none());

        let (_home, ctx) = setup(true);
        state.publish(reconcile(&ctx, now()).expect("reconcile"));
        assert!(state.is_ready());
        assert!(state.diagnostics().last_report.is_some());

        state.publish(error_policy(
            &Error::DuplicateTarget("/x".into()),
            &ctx,
            now(),
        ));
        assert!(!state.is_ready());
        assert!(
            state
                .metrics()
                .expect("metrics")
                .contains("tether_build_info")
        );
    }

    #[tokio::test]
    async fn run_reconciles_on_start_request_and_stops() {
        let (_home, ctx) = setup(false);
        let state = State::default();
        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(run(
            state.clone(),
            Arc::clone(&ctx),
            Duration::from_secs(3600),
            rx,
        ));

        let first = wait_for_report(&state, None).await;
        assert_eq!(first.changed(), 1);

        state.request_reconcile();
        let second = wait_for_report(&state, Some(first.started_at)).await;
        assert_eq!(second.changed(), 0);

        tx.send(true).expect("shutdown");
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("controller stops")
            .expect("no panic");
    }

    #[tokio::test]
    async fn run_publishes_error_report() {
        let home = TempHome::new();
        let ctx = home.context("not toml", false);
        let state = State::default();
        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(run(state.clone(), ctx, Duration::from_secs(3600), rx));
        let report = wait_for_report(&state, None).await;
        assert!(!report.succeeded());
        tx.send(true).expect("shutdown");
        handle.await.expect("no panic");
    }

    async fn wait_for_report(state: &State, after: Option<Timestamp>) -> Report {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(report) = state.report()
                    && after.is_none_or(|t| report.started_at > t)
                {
                    return report;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("report published")
    }
}
