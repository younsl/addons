//! Prometheus metrics of the controller.

use std::sync::atomic::AtomicI64;

use node_configuration_common::telemetry::metrics::Reconciles;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

#[derive(Default)]
pub struct Metrics {
    reconciles: Reconciles,
    pub leader: Gauge<i64, AtomicI64>,
}

impl Metrics {
    pub fn new(registry: &mut Registry) -> Self {
        let metrics = Self {
            reconciles: Reconciles::new(registry, &crate::BUILD),
            leader: Gauge::default(),
        };
        registry.register(
            "node_configuration_controller_leader",
            "1 while this controller replica acts, which is always without leader election",
            metrics.leader.clone(),
        );
        metrics
    }

    pub fn reconciled(&self, ok: bool) {
        self.reconciles.record(ok);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus_client::encoding::text::encode;

    #[test]
    fn the_controller_exposes_its_own_metrics_only() {
        let mut registry = Registry::default();
        let metrics = Metrics::new(&mut registry);
        metrics.reconciled(true);
        metrics.leader.set(1);
        let mut body = String::new();
        encode(&mut body, &registry).unwrap();
        for needle in [
            "node_configuration_build_info{version=",
            "node_configuration_reconcile_total{result=\"success\"} 1",
            "node_configuration_controller_leader 1",
        ] {
            assert!(body.contains(needle), "missing {needle:?} in:\n{body}");
        }
        assert!(!body.contains("node_configuration_agent"), "{body}");
    }
}
