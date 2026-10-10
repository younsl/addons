//! Prometheus metrics, registered once and shared through [`crate::State`].

use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;

use crate::Error;
use crate::config::BuildInfo;
use crate::controller::Report;
use crate::linker::Action;

#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ErrorLabels {
    error: String,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ActionLabels {
    action: String,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
struct BuildLabels {
    version: String,
    commit: String,
    rustc: String,
}

#[derive(Clone)]
pub struct Metrics {
    pub registry: Arc<Registry>,
    runs: Counter,
    failures: Family<ErrorLabels, Counter>,
    duration: Histogram,
    entries: Family<ActionLabels, Gauge>,
    failed_entries: Gauge,
    backups: Counter,
    package_updates: Counter,
    last_success: Gauge,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new(BuildInfo::CURRENT)
    }
}

impl Metrics {
    pub fn new(build: BuildInfo) -> Self {
        let mut registry = Registry::with_prefix("tether");
        let runs = Counter::default();
        let failures = Family::<ErrorLabels, Counter>::default();
        let duration = Histogram::new(exponential_buckets(0.001, 2.0, 14));
        let entries = Family::<ActionLabels, Gauge>::default();
        let failed_entries = Gauge::default();
        let backups = Counter::default();
        let package_updates = Counter::default();
        let last_success = Gauge::default();
        let build_info = Family::<BuildLabels, Gauge>::default();
        build_info
            .get_or_create(&BuildLabels {
                version: build.version.to_string(),
                commit: build.commit.to_string(),
                rustc: build.rustc.to_string(),
            })
            .set(1);

        registry.register("reconcile_runs", "Reconcile runs", runs.clone());
        registry.register(
            "reconcile_failures",
            "Reconcile runs that failed, by error",
            failures.clone(),
        );
        registry.register(
            "reconcile_duration_seconds",
            "Time one reconcile took",
            duration.clone(),
        );
        registry.register(
            "link_entries",
            "Link entries in the last reconcile by planned action",
            entries.clone(),
        );
        registry.register(
            "link_failed_entries",
            "Link entries that failed to apply in the last reconcile",
            failed_entries.clone(),
        );
        registry.register(
            "backups",
            "Files and directories moved to the backup directory",
            backups.clone(),
        );
        registry.register(
            "package_file_updates",
            "Package manager files rewritten because their package list changed",
            package_updates.clone(),
        );
        registry.register(
            "last_success_timestamp_seconds",
            "Unix time the last successful reconcile started",
            last_success.clone(),
        );
        registry.register("build_info", "Build metadata", build_info);

        Self {
            registry: Arc::new(registry),
            runs,
            failures,
            duration,
            entries,
            failed_entries,
            backups,
            package_updates,
            last_success,
        }
    }

    pub fn render(&self) -> Result<String, fmt::Error> {
        let mut body = String::new();
        encode(&mut body, &self.registry)?;
        Ok(body)
    }

    /// Count a run and record its duration when the guard drops.
    pub fn count_and_measure(&self) -> ReconcileMeasurer {
        self.runs.inc();
        ReconcileMeasurer {
            start: Instant::now(),
            metric: self.duration.clone(),
        }
    }

    pub fn reconcile_failure(&self, error: &Error) {
        self.failure(error.metric_label());
    }

    fn failure(&self, label: &str) {
        self.failures
            .get_or_create(&ErrorLabels {
                error: label.to_string(),
            })
            .inc();
    }

    pub fn observe(&self, report: &Report) {
        for action in Action::ALL {
            let count = report
                .entries
                .iter()
                .filter(|e| e.step.action == action)
                .count();
            self.entries
                .get_or_create(&ActionLabels {
                    action: action.as_str().to_string(),
                })
                .set(to_i64(count));
        }
        let failed = report.failed();
        self.failed_entries.set(to_i64(failed));
        self.backups
            .inc_by(report.entries.iter().filter(|e| e.backup.is_some()).count() as u64);
        self.package_updates.inc_by(
            report
                .packages
                .iter()
                .filter(|p| p.updated && !report.dry_run)
                .count() as u64,
        );
        if failed > 0 {
            self.failure("link");
        }
        if report.packages.iter().any(|p| p.error.is_some()) {
            self.failure("package_file");
        }
        if report.succeeded() && failed == 0 {
            self.last_success.set(report.started_at.as_second());
        }
    }
}

fn to_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

pub struct ReconcileMeasurer {
    start: Instant,
    metric: Histogram,
}

impl Drop for ReconcileMeasurer {
    fn drop(&mut self) {
        self.metric.observe(self.start.elapsed().as_secs_f64());
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::linker::{Entry, Step};
    use crate::packages::FileReport;

    fn entry(action: Action, backup: bool, error: bool) -> Entry {
        Entry {
            step: Step {
                source: PathBuf::from("/s"),
                target: PathBuf::from("/t"),
                action,
            },
            applied: !error && action.changes(),
            backup: backup.then(|| PathBuf::from("/b")),
            error: error.then(|| "boom".to_string()),
        }
    }

    fn report(entries: Vec<Entry>, packages: Vec<FileReport>) -> Report {
        Report {
            started_at: "2026-01-02T03:04:05Z".parse().expect("timestamp"),
            dry_run: false,
            backup_dir: None,
            error: None,
            entries,
            packages,
            files: Arc::default(),
        }
    }

    #[test]
    fn observe_measure_and_render() {
        let metrics = Metrics::default();
        drop(metrics.count_and_measure());
        metrics.observe(&report(
            vec![
                entry(Action::InSync, false, false),
                entry(Action::Backup, true, false),
            ],
            vec![FileReport {
                path: "/Brewfile".into(),
                entries: 3,
                updated: true,
                error: None,
                missing_mount: None,
            }],
        ));
        metrics.observe(&report(
            vec![entry(Action::Create, false, true)],
            vec![FileReport {
                path: "/krewfile".into(),
                entries: 0,
                updated: false,
                error: Some("no receipts".into()),
                missing_mount: None,
            }],
        ));
        metrics.reconcile_failure(&Error::DuplicateTarget("/x".into()));

        let body = metrics.render().expect("encode");
        for expected in [
            "tether_reconcile_runs_total 1",
            "tether_reconcile_duration_seconds_count 1",
            "tether_reconcile_failures_total{error=\"link\"} 1",
            "tether_reconcile_failures_total{error=\"package_file\"} 1",
            "tether_reconcile_failures_total{error=\"config_invalid\"} 1",
            "tether_link_entries{action=\"create\"} 1",
            "tether_link_failed_entries 1",
            "tether_backups_total 1",
            "tether_package_file_updates_total 1",
            "tether_last_success_timestamp_seconds 1767323045",
            "tether_build_info{",
        ] {
            assert!(body.contains(expected), "missing {expected}:\n{body}");
        }
    }
}
