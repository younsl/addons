//! SQLite connection-pool saturation.

use std::sync::Arc;

use prometheus::core::{Collector, Desc};
use prometheus::proto::MetricFamily;

use super::{counter_family, desc, gauge_family};
use crate::meta::PoolStats;

/// Reports the metadata store's connection-pool statistics. The store
/// implements it; the trait keeps this collector testable.
pub trait PoolStatser: Send + Sync {
    /// Write-pool and read-pool statistics, in that order.
    fn pool_stats(&self) -> (PoolStats, PoolStats);
}

impl PoolStatser for crate::meta::Store {
    fn pool_stats(&self) -> (PoolStats, PoolStats) {
        crate::meta::Store::pool_stats(self)
    }
}

/// Exports SQLite connection-pool saturation.
///
/// It is here because of how this store fails. Those two counters are what separate "the
/// database is busy" from "the process is wedged" during an incident, and what a saturation
/// alert should be built on.
pub struct DbPoolCollector {
    s: Arc<dyn PoolStatser>,

    open: Desc,
    in_use: Desc,
    idle: Desc,
    max: Desc,
    waits: Desc,
    wait_time: Desc,
}

impl DbPoolCollector {
    /// Builds a collector over the store's pools.
    pub fn new(s: Arc<dyn PoolStatser>) -> DbPoolCollector {
        let label = ["pool"];
        DbPoolCollector {
            s,
            open: desc(
                "forklift_db_connections_open",
                "Open connections in the pool (in use plus idle).",
                &label,
            ),
            in_use: desc(
                "forklift_db_connections_in_use",
                "Connections currently executing a statement.",
                &label,
            ),
            idle: desc(
                "forklift_db_connections_idle",
                "Connections open and idle.",
                &label,
            ),
            max: desc(
                "forklift_db_connections_max",
                "Connection limit for the pool; the write pool is deliberately 1 (single-writer SQLite).",
                &label,
            ),
            waits: desc(
                "forklift_db_connection_waits_total",
                "Times a caller had to wait for a free connection. Sustained growth on the write pool is saturation.",
                &label,
            ),
            wait_time: desc(
                "forklift_db_connection_wait_seconds_total",
                "Total time callers spent waiting for a free connection.",
                &label,
            ),
        }
    }
}

impl Collector for DbPoolCollector {
    fn desc(&self) -> Vec<&Desc> {
        vec![
            &self.open,
            &self.in_use,
            &self.idle,
            &self.max,
            &self.waits,
            &self.wait_time,
        ]
    }

    fn collect(&self) -> Vec<MetricFamily> {
        let (write, read) = self.s.pool_stats();
        let pools = [("write", write), ("read", read)];
        let sample = |f: fn(&PoolStats) -> f64| {
            pools
                .iter()
                .map(|(pool, st)| (vec![(*pool).to_string()], f(st)))
                .collect::<Vec<_>>()
        };
        vec![
            gauge_family(&self.open, sample(|st| (st.in_use + st.idle) as f64)),
            gauge_family(&self.in_use, sample(|st| st.in_use as f64)),
            gauge_family(&self.idle, sample(|st| st.idle as f64)),
            gauge_family(&self.max, sample(|st| st.max_open as f64)),
            // Counters: monotonic for the process lifetime, which is what a
            // rate() over them needs.
            counter_family(&self.waits, sample(|st| st.wait_count as f64)),
            counter_family(&self.wait_time, sample(|st| st.wait_duration.as_secs_f64())),
        ]
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use prometheus::Registry;

    use crate::metrics::db::*;
    use crate::metrics::tests::encode_filtered;

    struct FakePools {
        write: PoolStats,
        read: PoolStats,
    }

    impl PoolStatser for FakePools {
        fn pool_stats(&self) -> (PoolStats, PoolStats) {
            (self.write, self.read)
        }
    }

    /// The pool metrics exist for one question during an incident: is the single
    /// write connection saturated. So the wait counters have to be exported as
    /// counters (a rate() over them is the saturation signal), both pools have to
    /// be labelled apart, and the write pool's limit of 1 has to be visible rather
    /// than implied.
    #[test]
    fn db_pool_collector() {
        let collector = DbPoolCollector::new(Arc::new(FakePools {
            write: PoolStats {
                max_open: 1,
                in_use: 1,
                idle: 0,
                wait_count: 42,
                wait_duration: Duration::from_secs(3),
            },
            read: PoolStats {
                max_open: 8,
                in_use: 1,
                idle: 2,
                wait_count: 0,
                wait_duration: Duration::ZERO,
            },
        }));
        let reg = Registry::new();
        reg.register(Box::new(collector)).unwrap();

        let want = r#"# HELP forklift_db_connection_wait_seconds_total Total time callers spent waiting for a free connection.
# TYPE forklift_db_connection_wait_seconds_total counter
forklift_db_connection_wait_seconds_total{pool="read"} 0
forklift_db_connection_wait_seconds_total{pool="write"} 3
# HELP forklift_db_connection_waits_total Times a caller had to wait for a free connection. Sustained growth on the write pool is saturation.
# TYPE forklift_db_connection_waits_total counter
forklift_db_connection_waits_total{pool="read"} 0
forklift_db_connection_waits_total{pool="write"} 42
# HELP forklift_db_connections_in_use Connections currently executing a statement.
# TYPE forklift_db_connections_in_use gauge
forklift_db_connections_in_use{pool="read"} 1
forklift_db_connections_in_use{pool="write"} 1
# HELP forklift_db_connections_max Connection limit for the pool; the write pool is deliberately 1 (single-writer SQLite).
# TYPE forklift_db_connections_max gauge
forklift_db_connections_max{pool="read"} 8
forklift_db_connections_max{pool="write"} 1
"#;
        assert_eq!(
            encode_filtered(
                &reg,
                &[
                    "forklift_db_connection_wait_seconds_total",
                    "forklift_db_connection_waits_total",
                    "forklift_db_connections_in_use",
                    "forklift_db_connections_max",
                ]
            ),
            want
        );
    }

    #[test]
    fn db_pool_collector_reports_open_connections() {
        let collector = DbPoolCollector::new(Arc::new(FakePools {
            write: PoolStats {
                max_open: 1,
                in_use: 1,
                idle: 0,
                ..Default::default()
            },
            read: PoolStats {
                max_open: 8,
                in_use: 1,
                idle: 2,
                ..Default::default()
            },
        }));
        let reg = Registry::new();
        reg.register(Box::new(collector)).unwrap();

        let want = r#"# HELP forklift_db_connections_open Open connections in the pool (in use plus idle).
# TYPE forklift_db_connections_open gauge
forklift_db_connections_open{pool="read"} 3
forklift_db_connections_open{pool="write"} 1
"#;
        assert_eq!(
            encode_filtered(&reg, &["forklift_db_connections_open"]),
            want
        );
    }
}
