//! Prometheus metrics for reconcile results.

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::config::BuildInfo;
use crate::linker::Action;
use crate::reconciler::Report;

#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ActionLabels {
    action: String,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ResultLabels {
    result: String,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
struct BuildLabels {
    version: String,
    commit: String,
    rustc: String,
}

#[derive(Debug, Clone)]
pub struct Metrics {
    entries: Family<ActionLabels, Gauge>,
    failed_entries: Gauge,
    reconciles: Family<ResultLabels, Counter>,
    backups: Counter,
    last_success: Gauge,
}

impl Metrics {
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = Self {
            entries: Family::default(),
            failed_entries: Gauge::default(),
            reconciles: Family::default(),
            backups: Counter::default(),
            last_success: Gauge::default(),
        };
        registry.register(
            "tether_entries",
            "Link entries in the last reconcile by planned action",
            metrics.entries.clone(),
        );
        registry.register(
            "tether_failed_entries",
            "Link entries that failed to apply in the last reconcile",
            metrics.failed_entries.clone(),
        );
        registry.register(
            "tether_reconciles",
            "Reconcile runs by result",
            metrics.reconciles.clone(),
        );
        registry.register(
            "tether_backups",
            "Files and directories moved to the backup directory",
            metrics.backups.clone(),
        );
        registry.register(
            "tether_last_success_timestamp_seconds",
            "Unix time the last successful reconcile started",
            metrics.last_success.clone(),
        );
        metrics
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
        let backups = report.entries.iter().filter(|e| e.backup.is_some()).count();
        self.backups.inc_by(backups as u64);

        let result = if !report.succeeded() || failed > 0 {
            "failure"
        } else {
            "success"
        };
        self.reconciles
            .get_or_create(&ResultLabels {
                result: result.to_string(),
            })
            .inc();
        if result == "success" {
            self.last_success.set(report.started_at.as_second());
        }
    }
}

fn to_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

pub fn register_build_info(registry: &mut Registry, build: BuildInfo) {
    let info = Family::<BuildLabels, Gauge>::default();
    info.get_or_create(&BuildLabels {
        version: build.version.to_string(),
        commit: build.commit.to_string(),
        rustc: build.rustc.to_string(),
    })
    .set(1);
    registry.register("tether_build_info", "Build metadata", info);
}

pub fn render(registry: &Registry) -> Result<String, std::fmt::Error> {
    let mut body = String::new();
    encode(&mut body, registry)?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::linker::{Entry, Step};

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

    fn report(entries: Vec<Entry>, error: Option<&str>) -> Report {
        Report {
            started_at: "2026-01-02T03:04:05Z".parse().expect("timestamp"),
            dry_run: false,
            backup_dir: None,
            error: error.map(str::to_string),
            entries,
        }
    }

    #[test]
    fn observe_and_render() {
        let mut registry = Registry::default();
        register_build_info(&mut registry, BuildInfo::CURRENT);
        let metrics = Metrics::register(&mut registry);

        metrics.observe(&report(
            vec![
                entry(Action::InSync, false, false),
                entry(Action::Backup, true, false),
            ],
            None,
        ));
        metrics.observe(&report(vec![entry(Action::Create, false, true)], None));
        metrics.observe(&report(Vec::new(), Some("read failed")));

        let body = render(&registry).expect("encode");
        assert!(body.contains("tether_build_info{"), "{body}");
        assert!(
            body.contains("tether_entries{action=\"in_sync\"} 0"),
            "{body}"
        );
        assert!(body.contains("tether_backups_total 1"), "{body}");
        assert!(body.contains("tether_failed_entries 0"), "{body}");
        assert!(
            body.contains("tether_reconciles_total{result=\"success\"} 1"),
            "{body}"
        );
        assert!(
            body.contains("tether_reconciles_total{result=\"failure\"} 2"),
            "{body}"
        );
        assert!(
            body.contains("tether_last_success_timestamp_seconds 1767323045"),
            "{body}"
        );
    }
}
