//! Object-store reachability from the background [`HealthMonitor`].

use std::sync::Arc;

use prometheus::core::{Collector, Desc};
use prometheus::proto::MetricFamily;

use super::{desc, gauge_family};
use crate::storage::health::HealthMonitor;

/// Reports the monitor's most recent check, so both series stay absent until
/// the first check finishes rather than reading as down.
pub struct StorageHealthCollector {
    monitor: Arc<HealthMonitor>,
    up: Desc,
    duration: Desc,
}

impl StorageHealthCollector {
    pub fn new(monitor: Arc<HealthMonitor>) -> StorageHealthCollector {
        StorageHealthCollector {
            monitor,
            up: desc(
                "forklift_storage_up",
                "1 if the object store answered the last HeadBucket check on the configured bucket, else 0.",
                &[],
            ),
            duration: desc(
                "forklift_storage_check_duration_seconds",
                "Duration of the last HeadBucket check, failed checks included.",
                &[],
            ),
        }
    }
}

impl Collector for StorageHealthCollector {
    fn desc(&self) -> Vec<&Desc> {
        vec![&self.up, &self.duration]
    }

    fn collect(&self) -> Vec<MetricFamily> {
        let Some(last) = self.monitor.last() else {
            return Vec::new();
        };
        vec![
            gauge_family(
                &self.up,
                vec![(Vec::new(), if last.ok { 1.0 } else { 0.0 })],
            ),
            gauge_family(
                &self.duration,
                vec![(Vec::new(), last.latency_ms as f64 / 1000.0)],
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use prometheus::Registry;

    use crate::metrics::storage_health::*;
    use crate::metrics::tests::encode;
    use crate::storage::health::Probe;

    struct Switch(AtomicBool);

    #[async_trait]
    impl Probe for Switch {
        async fn check(&self) -> Result<(), String> {
            if self.0.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err("down".into())
            }
        }
    }

    #[tokio::test]
    async fn reports_the_last_check_and_nothing_before_it() {
        let probe = Arc::new(Switch(AtomicBool::new(true)));
        let monitor = HealthMonitor::new(Arc::clone(&probe) as Arc<dyn Probe>);
        let reg = Registry::new();
        reg.register(Box::new(StorageHealthCollector::new(Arc::clone(&monitor))))
            .unwrap();
        assert_eq!(encode(&reg), "");

        monitor.check_once().await;
        let text = encode(&reg);
        assert!(
            text.contains("# TYPE forklift_storage_up gauge\nforklift_storage_up 1\n"),
            "{text}"
        );
        assert!(
            text.contains("# TYPE forklift_storage_check_duration_seconds gauge\n"),
            "{text}"
        );

        probe.0.store(false, Ordering::SeqCst);
        monitor.check_once().await;
        assert!(encode(&reg).contains("forklift_storage_up 0\n"));
    }
}
