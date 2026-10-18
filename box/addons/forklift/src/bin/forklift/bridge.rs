use std::collections::HashMap;
use std::sync::Arc;

use prometheus::core::{Collector, Desc};
use prometheus::proto::{Gauge, Metric, MetricFamily, MetricType};

/// Runs an async store query from a synchronous injection point (the metric
/// collector, the HA status provider, the notifier's callbacks).
///
/// The runtime is multi-threaded, so the poll is handed to `block_in_place`; a caller outside a
/// runtime (or on a current-thread one) falls back to a scratch thread so it can never deadlock
/// the caller's scheduler. It mirrors `metrics::block_on`, which is crate-private.
pub(crate) fn block_on<F>(fut: F) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(fut))
        }
        Ok(handle) => {
            std::thread::scope(|s| s.spawn(|| handle.block_on(fut)).join().expect("task"))
        }
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("scratch runtime")
            .block_on(fut),
    }
}

pub(crate) struct FnGauge {
    desc: Desc,
    value: Arc<dyn Fn() -> f64 + Send + Sync>,
}

impl FnGauge {
    pub(crate) fn new(name: &str, help: &str, value: Arc<dyn Fn() -> f64 + Send + Sync>) -> Self {
        Self {
            desc: Desc::new(name.to_string(), help.to_string(), vec![], HashMap::new())
                .unwrap_or_else(|e| panic!("metric descriptor {name}: {e}")),
            value,
        }
    }
}

impl Collector for FnGauge {
    fn desc(&self) -> Vec<&Desc> {
        vec![&self.desc]
    }

    fn collect(&self) -> Vec<MetricFamily> {
        let mut gauge = Gauge::default();
        gauge.set_value((self.value)());
        let mut metric = Metric::default();
        metric.set_gauge(gauge);
        let mut mf = MetricFamily::default();
        mf.set_name(self.desc.fq_name.clone());
        mf.set_help(self.desc.help.clone());
        mf.set_field_type(MetricType::GAUGE);
        mf.set_metric(vec![metric]);
        vec![mf]
    }
}
