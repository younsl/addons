//! Prometheus metrics of the agent.

use std::sync::atomic::AtomicI64;

use node_configuration_common::telemetry::metrics::Reconciles;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::rules::Report;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ConfigurationLabels {
    pub configuration: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct RuleLabels {
    pub configuration: String,
    pub kind: &'static str,
    pub outcome: &'static str,
}

#[derive(Default)]
pub struct Metrics {
    reconciles: Reconciles,
    pub rule_outcomes_total: Family<RuleLabels, Counter>,
    pub last_run_timestamp_seconds: Family<ConfigurationLabels, Gauge<i64, AtomicI64>>,
    pub last_run_failed_rules: Family<ConfigurationLabels, Gauge<i64, AtomicI64>>,
}

impl Metrics {
    pub fn new(registry: &mut Registry) -> Self {
        let metrics = Self {
            reconciles: Reconciles::new(registry, &crate::BUILD),
            ..Self::default()
        };
        registry.register(
            "node_configuration_agent_rule_outcomes",
            "Rule outcomes on this node by configuration, rule kind, and outcome",
            metrics.rule_outcomes_total.clone(),
        );
        registry.register(
            "node_configuration_agent_last_run_timestamp_seconds",
            "Unix time of the last run of a configuration on this node",
            metrics.last_run_timestamp_seconds.clone(),
        );
        registry.register(
            "node_configuration_agent_last_run_failed_rules",
            "Rules that failed in the last run of a configuration on this node",
            metrics.last_run_failed_rules.clone(),
        );
        metrics
    }

    pub fn reconciled(&self, ok: bool) {
        self.reconciles.record(ok);
    }

    pub fn record_run(&self, configuration: &str, report: &Report, failed: usize, now: i64) {
        for entry in &report.entries {
            self.rule_outcomes_total
                .get_or_create(&RuleLabels {
                    configuration: configuration.to_string(),
                    kind: entry.kind,
                    outcome: entry.outcome.as_str(),
                })
                .inc();
        }
        let labels = ConfigurationLabels {
            configuration: configuration.to_string(),
        };
        self.last_run_timestamp_seconds
            .get_or_create(&labels)
            .set(now);
        self.last_run_failed_rules
            .get_or_create(&labels)
            .set(i64::try_from(failed).unwrap_or(i64::MAX));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{Entry, Outcome};
    use prometheus_client::encoding::text::encode;

    #[test]
    fn the_agent_exposes_its_runs() {
        let mut registry = Registry::default();
        let metrics = Metrics::new(&mut registry);
        metrics.reconciled(true);
        let report = Report {
            entries: vec![Entry {
                rule: "motd".into(),
                kind: "fileMode",
                target: "/usr/bin/su".to_string(),
                outcome: Outcome::Changed,
                detail: String::new(),
                ignore_failure: false,
            }],
        };
        metrics.record_run("mng", &report, 0, 1_700_000_000);
        let mut body = String::new();
        encode(&mut body, &registry).unwrap();
        for needle in [
            "node_configuration_build_info{version=",
            "node_configuration_reconcile_total{result=\"success\"} 1",
            "node_configuration_agent_rule_outcomes_total{configuration=\"mng\",kind=\"fileMode\",outcome=\"changed\"} 1",
            "node_configuration_agent_last_run_timestamp_seconds{configuration=\"mng\"} 1700000000",
            "node_configuration_agent_last_run_failed_rules{configuration=\"mng\"} 0",
        ] {
            assert!(body.contains(needle), "missing {needle:?} in:\n{body}");
        }
        assert!(!body.contains("node_configuration_controller"), "{body}");
    }
}
