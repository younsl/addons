//! Reads claim utilization from `kubelet_volume_stats_*` on the shared
//! metrics backend, a cluster-local Prometheus or a central Mimir. The
//! configured label selector (for example `cluster="prd"`) scopes every query
//! to this cluster when the backend holds several.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use async_trait::async_trait;

use crate::autoscaler::persistent_volume_claim::policy::Usage;
use crate::promql::{self, Sample};

/// The label kubelet puts the claim name in.
const CLAIM_LABEL: &str = "persistentvolumeclaim";

/// Instant queries against the metrics backend.
#[async_trait]
pub trait UsageSource: Send + Sync {
    async fn query(&self, expr: &str) -> Result<Vec<Sample>, String>;
}

#[async_trait]
impl UsageSource for promql::Client {
    async fn query(&self, expr: &str) -> Result<Vec<Sample>, String> {
        Self::query(self, expr).await.map_err(|e| e.to_string())
    }
}

/// Used percent of `used` against `total` for `claims` in `namespace`.
/// `max by` collapses the duplicates two scrapers of the same kubelet
/// produce. Naming the claims keeps the response to the targets in a
/// namespace holding many other claims.
#[must_use]
pub fn ratio_query(
    used: &str,
    total: &str,
    namespace: &str,
    claims: &[&str],
    selector: &BTreeMap<String, String>,
) -> String {
    let m = scope(namespace, claims, selector);
    format!(
        "100 * max by ({CLAIM_LABEL}) ({used}{{{m}}}) / max by ({CLAIM_LABEL}) ({total}{{{m}}})"
    )
}

/// The label matchers every query shares: the namespace, the backend
/// selector, and the target claims.
fn scope(namespace: &str, claims: &[&str], selector: &BTreeMap<String, String>) -> String {
    let mut labels = BTreeMap::from([("namespace".to_string(), namespace.to_string())]);
    labels.extend(selector.iter().map(|(k, v)| (k.clone(), v.clone())));
    let mut m = promql::matchers(&labels);
    if !claims.is_empty() {
        let names: Vec<String> = claims.iter().map(|c| c.replace('.', "\\.")).collect();
        let _ = write!(m, ",{CLAIM_LABEL}=~{}", promql::quote(&names.join("|")));
    }
    m
}

/// The filesystem size kubelet reports per claim.
#[must_use]
pub fn capacity_query(
    namespace: &str,
    claims: &[&str],
    selector: &BTreeMap<String, String>,
) -> String {
    let m = scope(namespace, claims, selector);
    format!("max by ({CLAIM_LABEL}) (kubelet_volume_stats_capacity_bytes{{{m}}})")
}

/// The storage utilization query.
#[must_use]
pub fn storage_query(
    namespace: &str,
    claims: &[&str],
    selector: &BTreeMap<String, String>,
) -> String {
    ratio_query(
        "kubelet_volume_stats_used_bytes",
        "kubelet_volume_stats_capacity_bytes",
        namespace,
        claims,
        selector,
    )
}

/// The inode utilization query.
#[must_use]
pub fn inodes_query(
    namespace: &str,
    claims: &[&str],
    selector: &BTreeMap<String, String>,
) -> String {
    ratio_query(
        "kubelet_volume_stats_inodes_used",
        "kubelet_volume_stats_inodes",
        namespace,
        claims,
        selector,
    )
}

/// Keys samples by claim name, dropping the non-finite values a zero total
/// produces.
fn by_claim(samples: Vec<Sample>) -> HashMap<String, f64> {
    samples
        .into_iter()
        .filter(|s| s.value.is_finite())
        .filter_map(|s| Some((s.labels.get(CLAIM_LABEL)?.clone(), s.value)))
        .collect()
}

/// Which series a reconcile needs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Wanted {
    pub storage: bool,
    pub inodes: bool,
    /// The filesystem size, recorded at each resize to tell a grown
    /// filesystem from a stale measurement.
    pub capacity: bool,
}

/// Utilization of `claims` in `namespace` for the resources asked for.
pub async fn usage(
    source: &dyn UsageSource,
    namespace: &str,
    claims: &[&str],
    selector: &BTreeMap<String, String>,
    wanted: Wanted,
) -> Result<HashMap<String, Usage>, String> {
    let Wanted {
        storage,
        inodes,
        capacity,
    } = wanted;
    let mut out: HashMap<String, Usage> = HashMap::new();
    if storage {
        for (claim, v) in by_claim(
            source
                .query(&storage_query(namespace, claims, selector))
                .await?,
        ) {
            out.entry(claim).or_default().storage = Some(v);
        }
    }
    if inodes {
        for (claim, v) in by_claim(
            source
                .query(&inodes_query(namespace, claims, selector))
                .await?,
        ) {
            out.entry(claim).or_default().inodes = Some(v);
        }
    }
    if capacity {
        for (claim, v) in by_claim(
            source
                .query(&capacity_query(namespace, claims, selector))
                .await?,
        ) {
            out.entry(claim).or_default().capacity_bytes = Some(v);
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Answers each query from a fixed table and records what was asked.
    #[derive(Default)]
    pub(crate) struct Fake {
        pub(crate) answers: Vec<(&'static str, Result<Vec<Sample>, String>)>,
        pub(crate) asked: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl UsageSource for Fake {
        async fn query(&self, expr: &str) -> Result<Vec<Sample>, String> {
            self.asked.lock().unwrap().push(expr.to_string());
            self.answers
                .iter()
                .find(|(needle, _)| expr.contains(needle))
                .map_or_else(|| Ok(Vec::new()), |(_, a)| a.clone())
        }
    }

    pub(crate) fn sample(claim: &str, value: f64) -> Sample {
        Sample {
            labels: BTreeMap::from([(CLAIM_LABEL.to_string(), claim.to_string())]),
            value,
        }
    }

    #[test]
    fn queries_scope_to_namespace_and_selector() {
        let sel = BTreeMap::from([("cluster".to_string(), "prd".to_string())]);
        let q = storage_query("db", &[], &sel);
        assert_eq!(
            q,
            r#"100 * max by (persistentvolumeclaim) (kubelet_volume_stats_used_bytes{cluster="prd",namespace="db"}) / max by (persistentvolumeclaim) (kubelet_volume_stats_capacity_bytes{cluster="prd",namespace="db"})"#
        );
        let q = inodes_query("db", &[], &BTreeMap::new());
        assert!(
            q.contains(r#"kubelet_volume_stats_inodes_used{namespace="db"}"#),
            "{q}"
        );
        assert!(
            q.contains(r#"kubelet_volume_stats_inodes{namespace="db"}"#),
            "{q}"
        );
        let q = storage_query("db", &["data-web-0", "a.b"], &BTreeMap::new());
        assert!(
            q.contains(r#"{namespace="db",persistentvolumeclaim=~"data-web-0|a\\.b"}"#),
            "{q}"
        );
    }

    #[tokio::test]
    async fn usage_merges_resources_and_drops_nan() {
        let fake = Fake {
            answers: vec![
                (
                    "used_bytes",
                    Ok(vec![sample("a", 85.0), sample("b", f64::NAN)]),
                ),
                (
                    "inodes_used",
                    Ok(vec![sample("a", 10.0), sample("c", 95.0)]),
                ),
            ],
            ..Fake::default()
        };
        let u = usage(
            &fake,
            "db",
            &[],
            &BTreeMap::new(),
            Wanted {
                storage: true,
                inodes: true,
                capacity: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            u["a"],
            Usage {
                storage: Some(85.0),
                inodes: Some(10.0),
                capacity_bytes: None,
            }
        );
        assert!(!u.contains_key("b"));
        assert_eq!(u["c"].inodes, Some(95.0));

        let u = usage(
            &fake,
            "db",
            &[],
            &BTreeMap::new(),
            Wanted {
                storage: true,
                ..Wanted::default()
            },
        )
        .await
        .unwrap();
        assert!(!u.contains_key("c"), "inodes not asked");
        assert_eq!(fake.asked.lock().unwrap().len(), 3);

        let failing = Fake {
            answers: vec![("used_bytes", Err("503".into()))],
            ..Fake::default()
        };
        assert_eq!(
            usage(
                &failing,
                "db",
                &[],
                &BTreeMap::new(),
                Wanted {
                    storage: true,
                    ..Wanted::default()
                }
            )
            .await
            .unwrap_err(),
            "503"
        );

        let fake = Fake {
            answers: vec![
                ("used_bytes", Ok(vec![sample("a", 90.0)])),
                (
                    "capacity_bytes",
                    Ok(vec![sample("a", 50.0 * 1_073_741_824.0)]),
                ),
            ],
            ..Fake::default()
        };
        let all = Wanted {
            storage: true,
            inodes: false,
            capacity: true,
        };
        let u = usage(&fake, "db", &["a"], &BTreeMap::new(), all)
            .await
            .unwrap();
        assert_eq!(u["a"].storage, Some(90.0));
        assert_eq!(u["a"].capacity_bytes, Some(50.0 * 1_073_741_824.0));
        let asked = fake.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 2);
        assert_eq!(
            asked[1],
            r#"max by (persistentvolumeclaim) (kubelet_volume_stats_capacity_bytes{namespace="db",persistentvolumeclaim=~"a"})"#
        );
    }
}
