//! Reconcile loop: the only place that touches the file system. HTTP handlers
//! read the last report and request a run through [`Shared`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use jiff::Timestamp;
use serde::Serialize;
use tokio::sync::{Notify, watch};
use tokio::time::{self, MissedTickBehavior};

use crate::config::Config;
use crate::linker::{self, Backups, Entry};
use crate::observability::metrics::Metrics;
use crate::spec::Spec;

#[derive(Debug, Clone)]
pub struct Settings {
    pub links_file: PathBuf,
    pub home: PathBuf,
    pub dry_run: bool,
}

impl From<&Config> for Settings {
    fn from(cfg: &Config) -> Self {
        Self {
            links_file: cfg.links_file.clone(),
            home: cfg.home.clone(),
            dry_run: cfg.dry_run,
        }
    }
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
}

impl Report {
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

/// State shared between the reconcile loop and the HTTP handlers.
#[derive(Debug, Default)]
pub struct Shared {
    report: RwLock<Option<Report>>,
    ready: AtomicBool,
    trigger: Notify,
}

impl Shared {
    pub fn report(&self) -> Option<Report> {
        self.report.read().ok().and_then(|guard| guard.clone())
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    /// Queue a reconcile. Requests made while one runs collapse into one more run.
    pub fn request_reconcile(&self) {
        self.trigger.notify_one();
    }

    pub fn publish(&self, report: Report) {
        self.ready.store(report.succeeded(), Ordering::SeqCst);
        if let Ok(mut guard) = self.report.write() {
            *guard = Some(report);
        }
    }
}

pub fn run_once(settings: &Settings, now: Timestamp) -> Report {
    let mut report = Report {
        started_at: now,
        dry_run: settings.dry_run,
        backup_dir: None,
        error: None,
        entries: Vec::new(),
    };

    let spec = match Spec::load(&settings.links_file, &settings.home) {
        Ok(spec) => spec,
        Err(err) => {
            report.error = Some(err.to_string());
            return report;
        }
    };

    let steps = linker::plan(&spec);
    if settings.dry_run {
        report.entries = linker::preview(steps);
    } else {
        let stamp = now.strftime("%Y%m%d-%H%M%S").to_string();
        let mut backups = Backups::new(&spec.backup_root, &stamp, &settings.home);
        report.entries = linker::apply(steps, &mut backups);
        report.backup_dir = backups.used_dir().map(Path::to_path_buf);
    }
    report
}

pub struct Reconciler {
    settings: Settings,
    shared: Arc<Shared>,
    metrics: Metrics,
}

impl Reconciler {
    pub const fn new(settings: Settings, shared: Arc<Shared>, metrics: Metrics) -> Self {
        Self {
            settings,
            shared,
            metrics,
        }
    }

    /// Reconcile on every tick or request until `shutdown` changes. A run in
    /// progress always finishes, so no link is left half replaced.
    pub async fn run(self, interval: Duration, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = time::interval(interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                _ = ticker.tick() => {},
                () = self.shared.trigger.notified() => tracing::info!("reconcile requested"),
            }
            self.reconcile().await;
        }
        tracing::info!("reconciler stopped");
    }

    async fn reconcile(&self) {
        let settings = self.settings.clone();
        let report = match tokio::task::spawn_blocking(move || {
            run_once(&settings, Timestamp::now())
        })
        .await
        {
            Ok(report) => report,
            Err(err) => {
                tracing::error!(error = %err, "reconcile task panicked");
                return;
            }
        };

        if let Some(err) = &report.error {
            tracing::error!(error = %err, "reconcile failed");
        } else {
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

        self.metrics.observe(&report);
        self.shared.publish(report);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use prometheus_client::registry::Registry;

    use super::*;
    use crate::linker::Action;

    struct Fixture {
        _dir: tempfile::TempDir,
        settings: Settings,
        target: PathBuf,
    }

    fn fixture(dry_run: bool) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        fs::create_dir_all(home.join("repo")).expect("repo");
        fs::write(home.join("repo/zshrc"), "zsh").expect("source");
        fs::write(home.join(".zshrc"), "local").expect("existing target");
        let links_file = dir.path().join("links.toml");
        fs::write(
            &links_file,
            "source_root = \"~/repo\"\nbackup_root = \"~/.backup\"\n\n[[links]]\nsource = \"zshrc\"\ntarget = \"~/.zshrc\"\n",
        )
        .expect("spec");
        Fixture {
            _dir: dir,
            target: home.join(".zshrc"),
            settings: Settings {
                links_file,
                home,
                dry_run,
            },
        }
    }

    fn now() -> Timestamp {
        "2026-01-02T03:04:05Z".parse().expect("timestamp")
    }

    #[test]
    fn run_once_applies_and_backs_up() {
        let fx = fixture(false);
        let report = run_once(&fx.settings, now());

        assert!(report.succeeded());
        assert_eq!(report.changed(), 1);
        assert_eq!(report.failed(), 0);
        assert_eq!(report.entries[0].step.action, Action::Backup);
        assert_eq!(
            report.backup_dir,
            Some(fx.settings.home.join(".backup/20260102-030405"))
        );
        assert!(
            fs::symlink_metadata(&fx.target)
                .expect("target")
                .is_symlink()
        );

        let again = run_once(&fx.settings, now());
        assert_eq!(again.changed(), 0);
        assert!(again.backup_dir.is_none());
    }

    #[test]
    fn run_once_dry_run_changes_nothing() {
        let fx = fixture(true);
        let report = run_once(&fx.settings, now());

        assert!(report.dry_run);
        assert_eq!(report.changed(), 0);
        assert_eq!(report.entries[0].step.action, Action::Backup);
        assert_eq!(fs::read_to_string(&fx.target).expect("untouched"), "local");
    }

    #[test]
    fn run_once_reports_spec_error() {
        let mut fx = fixture(false);
        fx.settings.links_file = fx.settings.home.join("missing.toml");
        let report = run_once(&fx.settings, now());
        assert!(!report.succeeded());
        assert_eq!(report.entries.len(), 0);
    }

    #[test]
    fn shared_publish_sets_readiness() {
        let shared = Shared::default();
        assert!(!shared.is_ready());
        assert!(shared.report().is_none());

        let fx = fixture(true);
        shared.publish(run_once(&fx.settings, now()));
        assert!(shared.is_ready());
        assert!(shared.report().is_some());

        let mut broken = fx.settings;
        broken.links_file = PathBuf::from("/nonexistent/links.toml");
        shared.publish(run_once(&broken, now()));
        assert!(!shared.is_ready());
    }

    #[tokio::test]
    async fn run_reconciles_on_start_request_and_stops() {
        let fx = fixture(false);
        let shared = Arc::new(Shared::default());
        let metrics = Metrics::register(&mut Registry::default());
        let reconciler = Reconciler::new(fx.settings.clone(), Arc::clone(&shared), metrics);
        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(reconciler.run(Duration::from_secs(3600), rx));

        let first = wait_for_report(&shared, None).await;
        assert_eq!(first.changed(), 1);

        shared.request_reconcile();
        let second = wait_for_report(&shared, Some(first.started_at)).await;
        assert_eq!(second.changed(), 0);

        tx.send(true).expect("shutdown");
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("reconciler stops")
            .expect("no panic");
    }

    async fn wait_for_report(shared: &Shared, after: Option<Timestamp>) -> Report {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(report) = shared.report()
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
