//! Scrape-time gauges for repository inventory and physical storage usage.
//! Values are computed on each Prometheus scrape (like the approval_pending
//! gauge) so they need no leader gating and stay accurate on standbys after a
//! replication snapshot swap.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use prometheus::core::{Collector, Desc};
use prometheus::proto::{Gauge, LabelPair, Metric, MetricFamily, MetricType};

pub mod coverage;
pub mod db;
pub mod upstream;

pub use coverage::{CoverageCollector, CoverageStats};
pub use db::{DbPoolCollector, PoolStatser};
pub use upstream::{RepoLister, UpstreamProber};

/// The subset of the metadata store the collector queries on scrape.
#[async_trait]
pub trait Reader: Send + Sync {
    /// All repositories, ordered by name.
    async fn list_repositories(&self) -> crate::meta::Result<Vec<crate::meta::Repository>>;
    /// Artifact count and total size per repository id.
    async fn all_repo_stats(&self) -> crate::meta::Result<HashMap<i64, crate::meta::RepoStats>>;
    /// Deduplicated blob count and their total physical size in bytes.
    async fn blob_stats(&self) -> crate::meta::Result<(i64, i64)>;
}

#[async_trait]
impl Reader for crate::meta::Store {
    async fn list_repositories(&self) -> crate::meta::Result<Vec<crate::meta::Repository>> {
        crate::meta::Store::list_repositories(self).await
    }

    async fn all_repo_stats(&self) -> crate::meta::Result<HashMap<i64, crate::meta::RepoStats>> {
        crate::meta::Store::all_repo_stats(self).await
    }

    async fn blob_stats(&self) -> crate::meta::Result<(i64, i64)> {
        crate::meta::Store::blob_stats(self).await
    }
}

/// Reports repository inventory and blob storage usage.
pub struct StorageCollector {
    r: Arc<dyn Reader>,
    timeout: Duration,

    repositories: Desc,
    artifacts: Desc,
    blobs: Desc,
    storage_bytes: Desc,

    repo_artifacts: Desc,
    repo_size_bytes: Desc,
}

impl StorageCollector {
    /// Builds a collector backed by the metadata store.
    pub fn new(r: Arc<dyn Reader>) -> StorageCollector {
        StorageCollector {
            r,
            timeout: Duration::from_secs(5),
            repositories: desc(
                "forklift_repositories",
                "Configured repositories by format and type.",
                &["format", "type"],
            ),
            artifacts: desc(
                "forklift_artifacts",
                "Logical artifacts stored across all repositories.",
                &[],
            ),
            blobs: desc(
                "forklift_blobs",
                "Deduplicated content-addressed blobs in the blob store.",
                &[],
            ),
            storage_bytes: desc(
                "forklift_storage_bytes",
                "Physical bytes used by deduplicated blobs.",
                &[],
            ),
            repo_artifacts: desc(
                "forklift_repository_artifacts",
                "Logical artifacts stored per repository.",
                &["repository", "format", "type"],
            ),
            repo_size_bytes: desc(
                "forklift_repository_size_bytes",
                "Logical (pre-dedup) bytes of artifacts stored per repository.",
                &["repository", "format", "type"],
            ),
        }
    }
}

impl Collector for StorageCollector {
    fn desc(&self) -> Vec<&Desc> {
        vec![
            &self.repositories,
            &self.artifacts,
            &self.blobs,
            &self.storage_bytes,
            &self.repo_artifacts,
            &self.repo_size_bytes,
        ]
    }

    /// Individual query failures skip only their own metrics so a single error
    /// never empties the whole scrape.
    fn collect(&self) -> Vec<MetricFamily> {
        let timeout = self.timeout;
        let r = Arc::clone(&self.r);
        let (repos, stats, blobs) = block_on(async move {
            let repos = tokio::time::timeout(timeout, r.list_repositories())
                .await
                .unwrap_or_else(|_| Err(crate::meta::Error::Other("timeout".into())));
            let stats = tokio::time::timeout(timeout, r.all_repo_stats())
                .await
                .unwrap_or_else(|_| Err(crate::meta::Error::Other("timeout".into())));
            let blobs = tokio::time::timeout(timeout, r.blob_stats())
                .await
                .unwrap_or_else(|_| Err(crate::meta::Error::Other("timeout".into())));
            (repos, stats, blobs)
        });

        let mut out = Vec::new();
        if let Ok(repos) = &repos {
            let mut counts: HashMap<(String, String), i64> = HashMap::new();
            for r in repos {
                *counts
                    .entry((r.format.clone(), r.r#type.clone()))
                    .or_default() += 1;
            }
            out.push(gauge_family(
                &self.repositories,
                counts
                    .into_iter()
                    .map(|((format, typ), n)| (vec![format, typ], n as f64))
                    .collect(),
            ));
        }

        if let Ok(stats) = &stats {
            let total: i64 = stats.values().map(|s| s.artifact_count).sum();
            out.push(gauge_family(&self.artifacts, vec![(vec![], total as f64)]));
        }

        // Per-repository inventory needs both the repo list (for
        // name/format/type) and the stats keyed by id; repos with no artifacts
        // are absent from stats and report zero. Sizes here are logical (sum of
        // artifact sizes) and so differ from forklift_storage_bytes, which is
        // deduplicated physical usage.
        if let (Ok(repos), Ok(stats)) = (&repos, &stats) {
            let mut artifacts = Vec::with_capacity(repos.len());
            let mut sizes = Vec::with_capacity(repos.len());
            for r in repos {
                let st = stats.get(&r.id).copied().unwrap_or_default();
                let labels = vec![r.name.clone(), r.format.clone(), r.r#type.clone()];
                artifacts.push((labels.clone(), st.artifact_count as f64));
                sizes.push((labels, st.total_size as f64));
            }
            out.push(gauge_family(&self.repo_artifacts, artifacts));
            out.push(gauge_family(&self.repo_size_bytes, sizes));
        }

        if let Ok((count, bytes)) = blobs {
            out.push(gauge_family(&self.blobs, vec![(vec![], count as f64)]));
            out.push(gauge_family(
                &self.storage_bytes,
                vec![(vec![], bytes as f64)],
            ));
        }
        out
    }
}

pub(crate) fn desc(name: &str, help: &str, labels: &[&str]) -> Desc {
    Desc::new(
        name.to_string(),
        help.to_string(),
        labels.iter().map(|l| (*l).to_string()).collect(),
        HashMap::new(),
    )
    .unwrap_or_else(|e| panic!("metric descriptor {name}: {e}"))
}

/// Assembles a gauge family from label values in descriptor order.
pub(crate) fn gauge_family(desc: &Desc, samples: Vec<(Vec<String>, f64)>) -> MetricFamily {
    family(desc, MetricType::GAUGE, samples)
}

/// Assembles a counter family; counters are monotonic for the process
/// lifetime, which is what a `rate()` over them needs.
pub(crate) fn counter_family(desc: &Desc, samples: Vec<(Vec<String>, f64)>) -> MetricFamily {
    family(desc, MetricType::COUNTER, samples)
}

fn family(desc: &Desc, kind: MetricType, samples: Vec<(Vec<String>, f64)>) -> MetricFamily {
    let mut mf = MetricFamily::default();
    mf.set_name(desc.fq_name.clone());
    mf.set_help(desc.help.clone());
    mf.set_field_type(kind);
    let metrics = samples
        .into_iter()
        .map(|(values, value)| {
            let mut labels: Vec<LabelPair> = desc
                .variable_labels
                .iter()
                .zip(values)
                .map(|(name, value)| {
                    let mut lp = LabelPair::default();
                    lp.set_name(name.clone());
                    lp.set_value(value);
                    lp
                })
                .collect();
            labels.sort_by(|a, b| a.name().cmp(b.name()));
            let mut m = Metric::default();
            m.set_label(labels);
            match kind {
                MetricType::COUNTER => {
                    let mut c = prometheus::proto::Counter::default();
                    c.set_value(value);
                    m.set_counter(c);
                }
                _ => {
                    let mut g = Gauge::default();
                    g.set_value(value);
                    m.set_gauge(g);
                }
            }
            m
        })
        .collect();
    mf.set_metric(metrics);
    mf
}

/// Runs an async store query from the synchronous `Collector::collect`.
///
/// Scrapes arrive on the multi-threaded runtime, so the poll is handed to `block_in_place`; a
/// collector driven from outside a runtime (or from a current-thread one) falls back to a
/// scratch thread so it can never deadlock the caller's scheduler.
pub(crate) fn block_on<F>(fut: F) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(fut))
        }
        Ok(handle) => std::thread::scope(|s| {
            s.spawn(|| handle.block_on(fut))
                .join()
                .expect("collector task")
        }),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("scrape runtime")
            .block_on(fut),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use prometheus::Registry;

    use crate::meta::{RepoStats, Repository};
    use crate::metrics::*;

    #[derive(Default)]
    pub(crate) struct FakeReader {
        pub(crate) repos: Vec<Repository>,
        pub(crate) stats: HashMap<i64, RepoStats>,
        pub(crate) blob_count: i64,
        pub(crate) blob_bytes: i64,
        pub(crate) err: bool,
    }

    #[async_trait]
    impl Reader for FakeReader {
        async fn list_repositories(&self) -> crate::meta::Result<Vec<Repository>> {
            if self.err {
                return Err(crate::meta::Error::Other("boom".into()));
            }
            Ok(self.repos.clone())
        }

        async fn all_repo_stats(&self) -> crate::meta::Result<HashMap<i64, RepoStats>> {
            if self.err {
                return Err(crate::meta::Error::Other("boom".into()));
            }
            Ok(self.stats.clone())
        }

        async fn blob_stats(&self) -> crate::meta::Result<(i64, i64)> {
            if self.err {
                return Err(crate::meta::Error::Other("boom".into()));
            }
            Ok((self.blob_count, self.blob_bytes))
        }
    }

    /// A repository row with only the fields these metrics read.
    pub(crate) fn repo(id: i64, name: &str, format: &str, typ: &str) -> Repository {
        Repository {
            id,
            name: name.into(),
            format: format.into(),
            r#type: typ.into(),
            ..Default::default()
        }
    }

    pub(crate) fn collect_and_encode(c: Box<dyn Collector>) -> String {
        let reg = Registry::new();
        reg.register(c).expect("register collector");
        encode(&reg)
    }

    /// Renders a registry's text exposition.
    pub(crate) fn encode(reg: &Registry) -> String {
        prometheus::TextEncoder::new()
            .encode_to_string(&reg.gather())
            .expect("encode")
    }

    pub(crate) fn encode_filtered(reg: &Registry, names: &[&str]) -> String {
        let families: Vec<_> = reg
            .gather()
            .into_iter()
            .filter(|f| names.contains(&f.name()))
            .collect();
        prometheus::TextEncoder::new()
            .encode_to_string(&families)
            .expect("encode")
    }

    pub(crate) fn collect_and_count(c: &dyn Collector) -> usize {
        c.collect().iter().map(|f| f.get_metric().len()).sum()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn storage_collector() {
        let c = StorageCollector::new(Arc::new(FakeReader {
            repos: vec![
                repo(1, "npm-hosted", "npm", "hosted"),
                repo(2, "npm-proxy", "npm", "proxy"),
                repo(3, "maven-hosted", "maven", "hosted"),
            ],
            stats: HashMap::from([
                (
                    1,
                    RepoStats {
                        artifact_count: 4,
                        total_size: 100,
                    },
                ),
                (
                    2,
                    RepoStats {
                        artifact_count: 6,
                        total_size: 200,
                    },
                ),
            ]),
            blob_count: 7,
            blob_bytes: 4096,
            ..Default::default()
        }));

        let want = r#"# HELP forklift_artifacts Logical artifacts stored across all repositories.
# TYPE forklift_artifacts gauge
forklift_artifacts 10
# HELP forklift_blobs Deduplicated content-addressed blobs in the blob store.
# TYPE forklift_blobs gauge
forklift_blobs 7
# HELP forklift_repositories Configured repositories by format and type.
# TYPE forklift_repositories gauge
forklift_repositories{format="maven",type="hosted"} 1
forklift_repositories{format="npm",type="hosted"} 1
forklift_repositories{format="npm",type="proxy"} 1
# HELP forklift_repository_artifacts Logical artifacts stored per repository.
# TYPE forklift_repository_artifacts gauge
forklift_repository_artifacts{format="maven",repository="maven-hosted",type="hosted"} 0
forklift_repository_artifacts{format="npm",repository="npm-hosted",type="hosted"} 4
forklift_repository_artifacts{format="npm",repository="npm-proxy",type="proxy"} 6
# HELP forklift_repository_size_bytes Logical (pre-dedup) bytes of artifacts stored per repository.
# TYPE forklift_repository_size_bytes gauge
forklift_repository_size_bytes{format="maven",repository="maven-hosted",type="hosted"} 0
forklift_repository_size_bytes{format="npm",repository="npm-hosted",type="hosted"} 100
forklift_repository_size_bytes{format="npm",repository="npm-proxy",type="proxy"} 200
# HELP forklift_storage_bytes Physical bytes used by deduplicated blobs.
# TYPE forklift_storage_bytes gauge
forklift_storage_bytes 4096
"#;
        assert_eq!(collect_and_encode(Box::new(c)), want);
    }

    /// A failing reader must not emit any metric (a single error never poisons the
    /// rest of the scrape; here all three queries fail so the output is empty).
    #[tokio::test(flavor = "multi_thread")]
    async fn storage_collector_errors_are_skipped() {
        let c = StorageCollector::new(Arc::new(FakeReader {
            err: true,
            ..Default::default()
        }));
        assert_eq!(collect_and_count(&c), 0, "expected no metrics on error");
    }

    /// The real store satisfies the reader trait, so the collector scrapes an
    /// empty database without erroring (the wiring the binary depends on).
    #[tokio::test(flavor = "multi_thread")]
    async fn storage_collector_reads_the_real_store() {
        let (store, _dir) = crate::testing::meta::test_store().await;
        let c = StorageCollector::new(Arc::new(store));
        let text = collect_and_encode(Box::new(c));
        assert!(text.contains("forklift_artifacts 0"), "{text}");
        assert!(text.contains("forklift_blobs 0"), "{text}");
        assert!(text.contains("forklift_storage_bytes 0"), "{text}");
    }

    /// The pool collector reads the real store's pools: the write pool is one
    /// connection by design and that has to be visible.
    #[tokio::test(flavor = "multi_thread")]
    async fn db_pool_collector_reads_the_real_store() {
        let (store, _dir) = crate::testing::meta::test_store().await;
        let c = DbPoolCollector::new(Arc::new(store));
        let text = collect_and_encode(Box::new(c));
        assert!(
            text.contains(r#"forklift_db_connections_max{pool="write"} 1"#),
            "{text}"
        );
    }
}
