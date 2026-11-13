//! Prometheus metrics of the controller and the agent.

use std::sync::atomic::AtomicI64;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::info::Info;
use prometheus_client::registry::Registry;

use crate::agent::rules::Report;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ReconcileLabels {
    pub result: &'static str,
}

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

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct BuildLabels {
    version: &'static str,
    revision: &'static str,
    rust_version: &'static str,
}

#[derive(Default)]
pub struct Metrics {
    pub reconcile_total: Family<ReconcileLabels, Counter>,
    pub rule_outcomes_total: Family<RuleLabels, Counter>,
    pub last_run_timestamp_seconds: Family<ConfigurationLabels, Gauge<i64, AtomicI64>>,
    pub last_run_failed_rules: Family<ConfigurationLabels, Gauge<i64, AtomicI64>>,
    pub leader: Gauge<i64, AtomicI64>,
}

impl Metrics {
    pub fn new(registry: &mut Registry) -> Self {
        let metrics = Self::default();
        registry.register(
            "node_configuration_build",
            "Build information",
            Info::new(BuildLabels {
                version: crate::VERSION,
                revision: crate::COMMIT,
                rust_version: crate::RUSTC_VERSION,
            }),
        );
        registry.register(
            "node_configuration_reconcile",
            "Reconciles in the controller and runs in the agent by result",
            metrics.reconcile_total.clone(),
        );
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
        registry.register(
            "node_configuration_controller_leader",
            "1 while this controller replica acts, which is always without leader election",
            metrics.leader.clone(),
        );
        metrics
    }

    pub fn reconciled(&self, ok: bool) {
        let result = if ok { "success" } else { "error" };
        self.reconcile_total
            .get_or_create(&ReconcileLabels { result })
            .inc();
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
