//! Polls EC2 on a schedule and serves the last successful snapshot as
//! Prometheus metrics.
//!
//! Instance metrics are encoded straight from the snapshot at scrape time, so
//! a scrape during a refresh never observes a half-populated (or empty)
//! result, and terminated instances disappear as soon as a new snapshot lands.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use prometheus_client::collector::Collector as PromCollector;
use prometheus_client::encoding::{DescriptorEncoder, EncodeMetric};
use prometheus_client::metrics::MetricType;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;
use tokio::sync::watch;

use crate::aws::ec2::InstanceSource;
use crate::observability::health::Health;
use crate::types::{INFO_LABELS, Instance};

type Snapshot = Arc<RwLock<Vec<Instance>>>;

/// Polls EC2 and owns the exporter's metric registry.
pub struct Collector<S> {
    source: S,
    health: Arc<Health>,
    snapshot: Snapshot,
    scrape_errors: Counter,
    scrape_duration: Histogram,
    last_success: Gauge<f64, AtomicU64>,
}

impl<S: InstanceSource> Collector<S> {
    /// Build a collector and register its metrics on `registry`. The health
    /// endpoint flips to ready after the first successful scrape.
    pub fn new(source: S, health: Arc<Health>, registry: &mut Registry) -> Self {
        let snapshot: Snapshot = Arc::default();
        let scrape_errors = Counter::default();
        // 50ms .. ~25.6s
        let scrape_duration = Histogram::new(exponential_buckets(0.05, 2.0, 10));
        let last_success = Gauge::<f64, AtomicU64>::default();

        registry.register(
            "ec2_metadata_scrape_errors",
            "Total EC2 API scrape failures",
            scrape_errors.clone(),
        );
        registry.register(
            "ec2_metadata_scrape_duration_seconds",
            "Duration of EC2 API scrapes",
            scrape_duration.clone(),
        );
        registry.register(
            "ec2_metadata_last_scrape_success_timestamp_seconds",
            "Unix timestamp of the last successful EC2 API scrape",
            last_success.clone(),
        );
        registry.register_collector(Box::new(SnapshotCollector {
            snapshot: Arc::clone(&snapshot),
        }));

        Self {
            source,
            health,
            snapshot,
            scrape_errors,
            scrape_duration,
            last_success,
        }
    }

    /// Refresh once immediately, then on every tick until `shutdown` fires.
    pub async fn run(&self, interval: Duration, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = tokio::time::interval(interval);
        // The first tick fires immediately, which doubles as the startup refresh.
        loop {
            tokio::select! {
                _ = ticker.tick() => self.refresh().await,
                _ = shutdown.changed() => break,
            }
        }
        self.health.set_ready(false);
    }

    /// Poll EC2 and swap in a new snapshot. On failure the previous snapshot
    /// keeps serving so a transient API error never blanks the metrics.
    pub async fn refresh(&self) {
        let start = Instant::now();
        let result = self.source.describe_all().await;
        self.scrape_duration.observe(start.elapsed().as_secs_f64());

        let instances = match result {
            Ok(instances) => instances,
            Err(err) => {
                self.scrape_errors.inc();
                tracing::error!(
                    error = format!("{err:#}"),
                    "failed to describe EC2 instances"
                );
                return;
            }
        };

        let count = instances.len();
        *self
            .snapshot
            .write()
            .unwrap_or_else(PoisonError::into_inner) = instances;
        self.last_success.set(unix_now());
        self.health.set_ready(true);
        tracing::info!(
            instances = count,
            duration_ms = start.elapsed().as_millis(),
            "refreshed EC2 instance metrics"
        );
    }
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Encodes the instance snapshot on every scrape.
#[derive(Debug)]
struct SnapshotCollector {
    snapshot: Snapshot,
}

impl PromCollector for SnapshotCollector {
    fn encode(&self, mut encoder: DescriptorEncoder) -> Result<(), std::fmt::Error> {
        let snapshot = self
            .snapshot
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();

        let mut info = encoder.encode_descriptor(
            "ec2_metadata_instance_info",
            "EC2 instance metadata. Value is always 1; labels carry the private IP, private DNS name (the Kubernetes node name on EKS), Name tag, instance type, availability zone, state, lifecycle (on-demand or spot), and CPU architecture",
            None,
            MetricType::Gauge,
        )?;
        for inst in &snapshot {
            let values = inst.info_label_values();
            let labels: Vec<(&str, &str)> = INFO_LABELS.into_iter().zip(values).collect();
            ConstGauge(1.0).encode(info.encode_family(&labels)?)?;
        }

        let mut launch = encoder.encode_descriptor(
            "ec2_metadata_instance_launch_time_seconds",
            "Unix timestamp of the instance's most recent launch. Resets on stop/start; uptime is time() minus this value",
            None,
            MetricType::Gauge,
        )?;
        for inst in &snapshot {
            if let Some(ts) = inst.launch_time {
                let labels = [
                    ("instance_id", inst.id.as_str()),
                    ("name", inst.name.as_str()),
                ];
                #[allow(clippy::cast_precision_loss)]
                ConstGauge(ts as f64).encode(launch.encode_family(&labels)?)?;
            }
        }

        let mut options = encoder.encode_descriptor(
            "ec2_metadata_instance_metadata_options",
            "Instance Metadata Service configuration. Value is always 1; http_tokens is required for IMDSv2-only instances and optional when IMDSv1 still answers, and hop_limit below 2 blocks containers from reaching IMDS",
            None,
            MetricType::Gauge,
        )?;
        for inst in &snapshot {
            if let Some(opts) = &inst.metadata_options {
                let hop_limit = opts.hop_limit.map(|h| h.to_string()).unwrap_or_default();
                let labels = [
                    ("instance_id", inst.id.as_str()),
                    ("name", inst.name.as_str()),
                    ("http_tokens", opts.http_tokens.as_str()),
                    ("http_endpoint", opts.http_endpoint.as_str()),
                    ("hop_limit", hop_limit.as_str()),
                ];
                ConstGauge(1.0).encode(options.encode_family(&labels)?)?;
            }
        }

        let mut by_state: BTreeMap<&str, u64> = BTreeMap::new();
        for inst in &snapshot {
            *by_state.entry(inst.state.as_str()).or_default() += 1;
        }
        let mut count = encoder.encode_descriptor(
            "ec2_metadata_instances",
            "Number of EC2 instances observed in the last successful scrape, by instance state",
            None,
            MetricType::Gauge,
        )?;
        for (state, n) in by_state {
            #[allow(clippy::cast_precision_loss)]
            ConstGauge(n as f64).encode(count.encode_family(&[("state", state)])?)?;
        }
        Ok(())
    }
}

/// A gauge value with no backing storage, encoded straight from the snapshot.
struct ConstGauge(f64);

impl EncodeMetric for ConstGauge {
    fn encode(
        &self,
        mut encoder: prometheus_client::encoding::MetricEncoder,
    ) -> Result<(), std::fmt::Error> {
        encoder.encode_gauge(&self.0)
    }

    fn metric_type(&self) -> MetricType {
        MetricType::Gauge
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use prometheus_client::encoding::text::encode;

    use super::*;
    use crate::types::MetadataOptions;

    #[derive(Default)]
    struct FakeSource {
        responses: Mutex<Vec<anyhow::Result<Vec<Instance>>>>,
        calls: AtomicUsize,
    }

    impl FakeSource {
        fn with(responses: Vec<anyhow::Result<Vec<Instance>>>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses),
                calls: AtomicUsize::new(0),
            })
        }
    }

    impl InstanceSource for Arc<FakeSource> {
        fn describe_all(&self) -> impl Future<Output = anyhow::Result<Vec<Instance>>> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut responses = self.responses.lock().expect("lock");
            let next = if responses.is_empty() {
                Ok(Vec::new())
            } else {
                responses.remove(0)
            };
            std::future::ready(next)
        }
    }

    fn instance(id: &str, ip: &str, state: &str) -> Instance {
        Instance {
            id: id.into(),
            name: format!("name-{id}"),
            private_ip: ip.into(),
            private_dns_name: format!("ip-{}.internal", ip.replace('.', "-")),
            instance_type: "m5.large".into(),
            availability_zone: "ap-northeast-2a".into(),
            state: state.into(),
            lifecycle: "on-demand".into(),
            architecture: "x86_64".into(),
            launch_time: Some(1_752_994_800),
            metadata_options: Some(MetadataOptions {
                http_tokens: "required".into(),
                http_endpoint: "enabled".into(),
                hop_limit: Some(2),
            }),
        }
    }

    fn setup(
        responses: Vec<anyhow::Result<Vec<Instance>>>,
    ) -> (
        Collector<Arc<FakeSource>>,
        Arc<FakeSource>,
        Arc<Health>,
        Registry,
    ) {
        let source = FakeSource::with(responses);
        let health = Arc::new(Health::default());
        let mut registry = Registry::default();
        let collector = Collector::new(Arc::clone(&source), Arc::clone(&health), &mut registry);
        (collector, source, health, registry)
    }

    fn render(registry: &Registry) -> String {
        let mut out = String::new();
        encode(&mut out, registry).expect("encode");
        out
    }

    #[tokio::test]
    async fn refresh_publishes_instance_info() {
        let (collector, _, health, registry) = setup(vec![Ok(vec![
            instance("i-1", "10.0.0.1", "running"),
            instance("i-2", "10.0.0.2", "stopped"),
        ])]);
        assert!(!health.is_ready());
        collector.refresh().await;
        assert!(health.is_ready());

        let out = render(&registry);
        assert!(out.contains(
            r#"ec2_metadata_instance_info{instance_id="i-1",name="name-i-1",private_ip="10.0.0.1",private_dns_name="ip-10-0-0-1.internal",instance_type="m5.large",availability_zone="ap-northeast-2a",state="running",lifecycle="on-demand",architecture="x86_64"} 1"#
        ), "{out}");
        assert!(out.contains(
            r#"ec2_metadata_instance_metadata_options{instance_id="i-1",name="name-i-1",http_tokens="required",http_endpoint="enabled",hop_limit="2"} 1"#
        ), "{out}");
        assert!(out.contains(r#"ec2_metadata_instance_launch_time_seconds{instance_id="i-1",name="name-i-1"} 1752994800"#), "{out}");
        assert!(
            out.contains(r#"ec2_metadata_instances{state="running"} 1"#),
            "{out}"
        );
        assert!(
            out.contains(r#"ec2_metadata_instances{state="stopped"} 1"#),
            "{out}"
        );
        assert!(out.contains("ec2_metadata_scrape_errors_total 0"), "{out}");
        assert!(
            out.contains("ec2_metadata_scrape_duration_seconds_count 1"),
            "{out}"
        );
        assert!(
            !out.contains("ec2_metadata_last_scrape_success_timestamp_seconds 0.0\n"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn refresh_resets_removed_instances() {
        let (collector, _, _, registry) = setup(vec![
            Ok(vec![instance("i-1", "10.0.0.1", "running")]),
            Ok(vec![instance("i-2", "10.0.0.2", "running")]),
        ]);
        collector.refresh().await;
        collector.refresh().await;
        let out = render(&registry);
        assert!(!out.contains(r#"instance_id="i-1""#), "{out}");
        assert!(out.contains(r#"instance_id="i-2""#), "{out}");
    }

    #[tokio::test]
    async fn metadata_options_omitted_when_absent() {
        let mut inst = instance("i-1", "10.0.0.1", "running");
        inst.metadata_options = None;
        let (collector, _, _, registry) = setup(vec![Ok(vec![inst])]);
        collector.refresh().await;
        let out = render(&registry);
        assert!(
            !out.contains("ec2_metadata_instance_metadata_options{"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn metadata_options_hop_limit_empty_when_missing() {
        let mut inst = instance("i-1", "10.0.0.1", "running");
        inst.metadata_options = Some(MetadataOptions {
            http_tokens: "optional".into(),
            http_endpoint: "enabled".into(),
            hop_limit: None,
        });
        let (collector, _, _, registry) = setup(vec![Ok(vec![inst])]);
        collector.refresh().await;
        let out = render(&registry);
        assert!(
            out.contains(r#"http_tokens="optional",http_endpoint="enabled",hop_limit=""#),
            "{out}"
        );
    }

    #[tokio::test]
    async fn launch_time_omitted_when_missing() {
        let mut inst = instance("i-1", "10.0.0.1", "running");
        inst.launch_time = None;
        let (collector, _, _, registry) = setup(vec![Ok(vec![inst])]);
        collector.refresh().await;
        let out = render(&registry);
        assert!(
            !out.contains("ec2_metadata_instance_launch_time_seconds{"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn refresh_keeps_snapshot_and_counts_errors_on_failure() {
        let (collector, _, health, registry) = setup(vec![
            Ok(vec![instance("i-1", "10.0.0.1", "running")]),
            Err(anyhow::anyhow!("boom")),
        ]);
        collector.refresh().await;
        collector.refresh().await;
        let out = render(&registry);
        assert!(out.contains(r#"instance_id="i-1""#), "{out}");
        assert!(out.contains("ec2_metadata_scrape_errors_total 1"), "{out}");
        assert!(
            out.contains("ec2_metadata_scrape_duration_seconds_count 2"),
            "{out}"
        );
        assert!(health.is_ready(), "failure after success keeps readiness");
    }

    #[tokio::test]
    async fn failure_before_success_stays_not_ready() {
        let (collector, _, health, _) = setup(vec![Err(anyhow::anyhow!("boom"))]);
        collector.refresh().await;
        assert!(!health.is_ready());
    }

    #[tokio::test]
    async fn empty_snapshot_encodes_only_descriptors() {
        let (_, _, _, registry) = setup(vec![]);
        let out = render(&registry);
        assert!(
            out.contains("# TYPE ec2_metadata_instance_info gauge"),
            "{out}"
        );
        assert!(!out.contains("ec2_metadata_instance_info{"), "{out}");
    }

    #[tokio::test]
    async fn run_refreshes_on_tick_and_stops_on_shutdown() {
        let (collector, source, health, _) = setup(vec![]);
        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            collector.run(Duration::from_millis(20), rx).await;
        });
        tokio::time::sleep(Duration::from_millis(120)).await;
        tx.send(true).expect("send shutdown");
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("run stops")
            .expect("no panic");
        assert!(
            source.calls.load(Ordering::SeqCst) >= 2,
            "expected startup refresh plus ticks"
        );
        assert!(!health.is_ready(), "shutdown flips readiness off");
    }
}
