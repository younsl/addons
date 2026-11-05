use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Local;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::procnet;
use crate::source::{AddressSource, CatalogSource, MetricsSource, PodSource};
use crate::types::{CacheGroup, NodeMetrics, PodRef, PodScan, Snapshot};

pub enum Msg {
    RoundStarted,
    Snapshot(Box<Snapshot>),
}

#[derive(Debug, Clone, Copy)]
pub struct CollectorConfig {
    pub port: u16,
    pub interval: Duration,
    pub parallel: usize,
}

pub struct Sources<C, A, M, P> {
    pub catalog: C,
    pub addresses: A,
    pub metrics: M,
    pub pods: Arc<P>,
}

/// Last good value of each source, reused when a later call fails.
#[derive(Default)]
struct Last {
    groups: Vec<CacheGroup>,
    ips: HashMap<String, IpAddr>,
    metrics: HashMap<String, NodeMetrics>,
}

pub struct Collector<C, A, M, P> {
    sources: Sources<C, A, M, P>,
    config: CollectorConfig,
    last: Last,
}

impl<C, A, M, P> Collector<C, A, M, P>
where
    C: CatalogSource,
    A: AddressSource,
    M: MetricsSource,
    P: PodSource,
{
    pub fn new(sources: Sources<C, A, M, P>, config: CollectorConfig) -> Self {
        Self {
            sources,
            config,
            last: Last::default(),
        }
    }

    /// Rounds run back to back with `interval` in between, or sooner on a refresh request.
    pub async fn run(mut self, tx: mpsc::Sender<Msg>, mut refresh: mpsc::Receiver<()>) {
        loop {
            if tx.send(Msg::RoundStarted).await.is_err() {
                return;
            }
            let snapshot = self.round().await;
            if tx.send(Msg::Snapshot(Box::new(snapshot))).await.is_err() {
                return;
            }
            tokio::select! {
                () = tokio::time::sleep(self.config.interval) => {}
                r = refresh.recv() => if r.is_none() { return },
            }
        }
    }

    pub async fn round(&mut self) -> Snapshot {
        let started = Instant::now();
        let mut errors = Vec::new();

        let ((groups, metrics), ips, (pods, pods_listed)) = tokio::join!(
            self.groups_and_metrics(),
            self.sources.addresses.node_ips(),
            self.scan_pods(&mut errors)
        );

        match groups {
            Ok(g) => self.last.groups = g,
            Err(e) => errors.push(format!("replication groups: {e}")),
        }
        match ips {
            Ok(ips) => self.last.ips = ips,
            Err(e) => errors.push(format!("node addresses: {e}")),
        }
        match metrics {
            Ok(m) => self.last.metrics = m,
            Err(e) => errors.push(format!("metrics: {e}")),
        }

        let snapshot = Snapshot {
            at: Local::now(),
            took: started.elapsed(),
            groups: self.last.groups.clone(),
            ips: self.last.ips.clone(),
            metrics: self.last.metrics.clone(),
            pods,
            pods_listed,
            errors,
        };
        tracing::info!(
            groups = snapshot.groups.len(),
            pods = snapshot.pods.len(),
            errors = snapshot.errors.len(),
            took_ms = snapshot.took.as_millis(),
            "round done"
        );
        snapshot
    }

    /// Metrics need node ids, so they follow the catalog. Failed catalog calls fall back to the
    /// last known groups.
    async fn groups_and_metrics(
        &self,
    ) -> (
        Result<Vec<CacheGroup>, String>,
        Result<HashMap<String, NodeMetrics>, String>,
    ) {
        let groups = self
            .sources
            .catalog
            .groups()
            .await
            .map_err(|e| e.to_string());
        tracing::debug!("catalog done");
        let nodes: Vec<String> = groups
            .as_ref()
            .unwrap_or(&self.last.groups)
            .iter()
            .flat_map(|g| g.nodes.iter().map(|n| n.id.clone()))
            .collect();
        let metrics = if nodes.is_empty() {
            Ok(HashMap::new())
        } else {
            self.sources
                .metrics
                .latest(nodes)
                .await
                .map_err(|e| e.to_string())
        };
        tracing::debug!("metrics done");
        (groups, metrics)
    }

    async fn scan_pods(&self, errors: &mut Vec<String>) -> (Vec<PodScan>, bool) {
        let pods = match self.sources.pods.list().await {
            Ok(p) => p,
            Err(e) => {
                errors.push(format!("pod list: {e}"));
                return (Vec::new(), false);
            }
        };
        tracing::debug!(pods = pods.len(), "pod list done");
        let scans = self.sample_all(pods).await;
        tracing::debug!("pod scan done");
        (scans, true)
    }

    async fn sample_all(&self, pods: Vec<PodRef>) -> Vec<PodScan> {
        let permits = Arc::new(Semaphore::new(self.config.parallel));
        let port = self.config.port;
        let mut set = JoinSet::new();
        for pod in pods {
            let source = Arc::clone(&self.sources.pods);
            let permits = Arc::clone(&permits);
            set.spawn(async move {
                let _permit = permits.acquire_owned().await.ok();
                let result = source
                    .read_tcp_tables(&pod)
                    .await
                    .map(|text| procnet::count_by_ip(&text, port))
                    .map_err(|e| e.to_string());
                PodScan { pod, result }
            });
        }
        let mut scans = Vec::new();
        while let Some(res) = set.join_next().await {
            match res {
                Ok(s) => scans.push(s),
                Err(e) => tracing::error!(error = %e, "scan task panicked"),
            }
        }
        scans.sort_by_key(|s| s.pod.key());
        scans
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;
    use std::future::{Future, ready};
    use std::sync::Mutex;

    use super::*;
    use crate::types::tests::{group, pod};

    struct Queue<T>(Mutex<Vec<Result<T, String>>>);

    impl<T: Clone> Queue<T> {
        fn new(items: Vec<Result<T, String>>) -> Self {
            Self(Mutex::new(items))
        }

        fn next(&self) -> Result<T, String> {
            let mut q = self.0.lock().unwrap();
            if q.len() > 1 {
                q.remove(0)
            } else {
                q[0].clone()
            }
        }
    }

    impl CatalogSource for Queue<Vec<CacheGroup>> {
        type Error = String;
        fn groups(&self) -> impl Future<Output = Result<Vec<CacheGroup>, String>> + Send {
            ready(self.next())
        }
    }

    impl AddressSource for Queue<HashMap<String, IpAddr>> {
        type Error = String;
        fn node_ips(&self) -> impl Future<Output = Result<HashMap<String, IpAddr>, String>> + Send {
            ready(self.next())
        }
    }

    struct FakeMetrics(Mutex<Vec<Vec<String>>>);

    impl MetricsSource for FakeMetrics {
        type Error = String;
        fn latest(
            &self,
            nodes: Vec<String>,
        ) -> impl Future<Output = Result<HashMap<String, NodeMetrics>, String>> + Send {
            self.0.lock().unwrap().push(nodes.clone());
            let m = nodes
                .into_iter()
                .map(|n| {
                    (
                        n,
                        NodeMetrics {
                            items: Some(5.0),
                            ..NodeMetrics::default()
                        },
                    )
                })
                .collect();
            ready(Ok(m))
        }
    }

    struct FakePods {
        list: Result<Vec<PodRef>, String>,
        tables: HashMap<String, Result<String, String>>,
    }

    impl PodSource for FakePods {
        type Error = String;
        fn list(&self) -> impl Future<Output = Result<Vec<PodRef>, String>> + Send {
            ready(self.list.clone())
        }
        fn read_tcp_tables(
            &self,
            p: &PodRef,
        ) -> impl Future<Output = Result<String, String>> + Send {
            ready(self.tables[&p.name].clone())
        }
    }

    fn table(remotes: &[&str]) -> String {
        let mut s = String::from("  sl  local_address rem_address   st\n");
        for (i, r) in remotes.iter().enumerate() {
            writeln!(s, "   {i}: 0A00000A:1000 {r}:18EB 01 0").unwrap();
        }
        s
    }

    fn ips() -> HashMap<String, IpAddr> {
        HashMap::from([("orders-001".into(), "10.0.0.1".parse().unwrap())])
    }

    type Fake =
        Collector<Queue<Vec<CacheGroup>>, Queue<HashMap<String, IpAddr>>, FakeMetrics, FakePods>;

    fn collector(
        groups: Vec<Result<Vec<CacheGroup>, String>>,
        addrs: Vec<Result<HashMap<String, IpAddr>, String>>,
        pods: FakePods,
    ) -> Fake {
        Collector::new(
            Sources {
                catalog: Queue::new(groups),
                addresses: Queue::new(addrs),
                metrics: FakeMetrics(Mutex::new(vec![])),
                pods: Arc::new(pods),
            },
            CollectorConfig {
                port: 6379,
                interval: Duration::from_secs(60),
                parallel: 2,
            },
        )
    }

    fn pods() -> FakePods {
        FakePods {
            list: Ok(vec![
                pod("shop", "api-1", "api"),
                pod("shop", "job-1", "job"),
            ]),
            tables: HashMap::from([
                (
                    "api-1".into(),
                    Ok(table(&["0100000A", "0100000A", "0200000A"])),
                ),
                ("job-1".into(), Err("cat: not found".into())),
            ]),
        }
    }

    #[tokio::test]
    async fn round_collects_every_source() {
        let mut c = collector(vec![Ok(vec![group("orders")])], vec![Ok(ips())], pods());
        let snap = c.round().await;
        assert!(snap.errors.is_empty() && snap.pods_listed);
        assert_eq!(snap.groups.len(), 1);
        assert_eq!(snap.metrics["orders-001"].items, Some(5.0));
        assert_eq!(snap.pods.len(), 2);
        let by_ip = snap.pods[0].result.as_ref().unwrap();
        assert_eq!(by_ip[&"10.0.0.1".parse::<IpAddr>().unwrap()], 2);
        assert!(snap.pods[1].result.is_err());
    }

    #[tokio::test]
    async fn failures_keep_last_good_values() {
        let mut c = collector(
            vec![Ok(vec![group("orders")]), Err("throttled".into())],
            vec![Ok(ips()), Err("denied".into())],
            FakePods {
                list: Err("forbidden".into()),
                tables: HashMap::new(),
            },
        );
        c.round().await;
        let snap = c.round().await;
        assert_eq!(snap.groups.len(), 1);
        assert_eq!(snap.ips.len(), 1);
        assert!(!snap.pods_listed);
        assert_eq!(snap.errors.len(), 3);
        let calls = c.sources.metrics.0.lock().unwrap();
        assert_eq!(
            calls[1],
            ["orders-001", "orders-002"],
            "metrics fall back to last groups"
        );
    }

    #[tokio::test]
    async fn empty_catalog_skips_metrics() {
        let mut c = collector(vec![Ok(vec![])], vec![Ok(HashMap::new())], pods());
        let snap = c.round().await;
        assert!(snap.metrics.is_empty());
        assert!(c.sources.metrics.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn run_streams_rounds_and_honours_refresh() {
        let c = collector(vec![Ok(vec![])], vec![Ok(HashMap::new())], pods());
        let (tx, mut rx) = mpsc::channel(8);
        let (refresh_tx, refresh_rx) = mpsc::channel(1);
        let task = tokio::spawn(c.run(tx, refresh_rx));
        assert!(matches!(rx.recv().await, Some(Msg::RoundStarted)));
        assert!(matches!(rx.recv().await, Some(Msg::Snapshot(_))));
        refresh_tx.send(()).await.unwrap();
        assert!(matches!(rx.recv().await, Some(Msg::RoundStarted)));
        assert!(matches!(rx.recv().await, Some(Msg::Snapshot(_))));
        drop(refresh_tx);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }
}
