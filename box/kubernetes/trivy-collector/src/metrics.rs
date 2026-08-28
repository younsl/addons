//! Prometheus metrics for the trivy-collector.
//!
//! The two pods measure different things and only register what they own. The
//! scraper owns the database, so the database gauges live there; the server
//! owns request handling and the authored-state caches, so those live there.
//! Registering a metric a pod can never move would publish a permanent zero.

use std::sync::Arc;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

use crate::config::Mode;
use crate::storage::Database;

// ============================================
// Label types
// ============================================

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct InfoLabels {
    pub version: String,
    pub mode: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct HttpLabels {
    pub method: String,
    pub status: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct HttpDurationLabels {
    pub method: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ReportReceivedLabels {
    pub cluster: String,
    pub report_type: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ReportTypeLabels {
    pub report_type: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct McpToolLabels {
    pub tool: String,
    pub result: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct McpToolDurationLabels {
    pub tool: String,
}

// ============================================
// Histogram buckets
// ============================================

const HTTP_DURATION_BUCKETS: &[f64] = &[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0];

/// The two report types, used to pre-initialize per-type families so a fresh
/// scrape shows zeros rather than "no data".
pub const REPORT_TYPES: [&str; 2] = ["vulnerabilityreport", "sbomreport"];

// ============================================
// Metrics struct
// ============================================

/// All Prometheus metrics for trivy-collector.
///
/// Fields are wrapped in `Option` so that only the metrics belonging to the
/// running mode are registered.
pub struct Metrics {
    // -- Metadata --
    registered_count: usize,

    // -- Common --
    pub info: Family<InfoLabels, Gauge>,

    // -- Server mode --
    pub http_requests_total: Option<Family<HttpLabels, Counter>>,
    pub http_request_duration_seconds: Option<Family<HttpDurationLabels, Histogram>>,
    pub reports_received_total: Option<Family<ReportReceivedLabels, Counter>>,
    /// Serialized size of the notes ConfigMap. A ConfigMap caps at roughly
    /// 1MiB, so its headroom is worth watching rather than discovering.
    pub notes_configmap_bytes: Option<Gauge>,
    /// API tokens currently held in the Secret-backed store.
    pub api_tokens_total: Option<Gauge>,
    pub mcp_tool_calls_total: Option<Family<McpToolLabels, Counter>>,
    pub mcp_tool_duration_seconds: Option<Family<McpToolDurationLabels, Histogram>>,
    pub mcp_tool_calls_in_flight: Option<Gauge>,

    // -- Scraper mode --
    pub db_size_bytes: Option<Gauge>,
    pub db_reports_total: Option<Family<ReportTypeLabels, Gauge>>,
    /// Clusters registered with the scraper.
    pub clusters_total: Option<Gauge>,
    /// 1 once every registered cluster has replayed its initial list. Reads on
    /// an unhydrated fleet are legitimately partial.
    pub fleet_hydrated: Option<Gauge>,
}

impl Metrics {
    /// Create and register metrics based on mode.
    pub fn new(registry: &mut Registry, mode: Mode) -> Arc<Self> {
        let info = Family::<InfoLabels, Gauge>::default();
        registry.register("trivy_collector_info", "Build information", info.clone());

        info.get_or_create(&InfoLabels {
            version: env!("CARGO_PKG_VERSION").to_string(),
            mode: mode.to_string(),
        })
        .set(1);

        let mut metrics = Metrics {
            registered_count: 1, // info (always registered)
            info,
            http_requests_total: None,
            http_request_duration_seconds: None,
            reports_received_total: None,
            notes_configmap_bytes: None,
            api_tokens_total: None,
            mcp_tool_calls_total: None,
            mcp_tool_duration_seconds: None,
            mcp_tool_calls_in_flight: None,
            db_size_bytes: None,
            db_reports_total: None,
            clusters_total: None,
            fleet_hydrated: None,
        };

        metrics.registered_count += match mode {
            Mode::Server => metrics.register_server(registry),
            Mode::Scraper => metrics.register_scraper(registry),
        };

        let metrics = Arc::new(metrics);
        match mode {
            Mode::Server => metrics.init_for_server(),
            Mode::Scraper => metrics.init_for_scraper(),
        }
        metrics
    }

    /// Returns the total number of registered metric families.
    pub fn count(&self) -> usize {
        self.registered_count
    }

    fn register_server(&mut self, registry: &mut Registry) -> usize {
        let http_requests_total = Family::<HttpLabels, Counter>::default();
        registry.register(
            "trivy_collector_http_requests",
            "Total HTTP requests",
            http_requests_total.clone(),
        );
        self.http_requests_total = Some(http_requests_total);

        let http_request_duration_seconds =
            Family::<HttpDurationLabels, Histogram>::new_with_constructor(|| {
                Histogram::new(HTTP_DURATION_BUCKETS.iter().copied())
            });
        registry.register(
            "trivy_collector_http_request_duration_seconds",
            "HTTP request duration in seconds",
            http_request_duration_seconds.clone(),
        );
        self.http_request_duration_seconds = Some(http_request_duration_seconds);

        let reports_received_total = Family::<ReportReceivedLabels, Counter>::default();
        registry.register(
            "trivy_collector_reports_received",
            "Total reports received on the push ingest route",
            reports_received_total.clone(),
        );
        self.reports_received_total = Some(reports_received_total);

        let notes_configmap_bytes = Gauge::default();
        registry.register(
            "trivy_collector_notes_configmap_bytes",
            "Serialized size of the notes ConfigMap in bytes",
            notes_configmap_bytes.clone(),
        );
        self.notes_configmap_bytes = Some(notes_configmap_bytes);

        let api_tokens_total = Gauge::default();
        registry.register(
            "trivy_collector_api_tokens",
            "API tokens held in the tokens Secret",
            api_tokens_total.clone(),
        );
        self.api_tokens_total = Some(api_tokens_total);

        let mcp_tool_calls_total = Family::<McpToolLabels, Counter>::default();
        registry.register(
            "trivy_collector_mcp_tool_calls",
            "Total MCP tool invocations by tool and result",
            mcp_tool_calls_total.clone(),
        );
        self.mcp_tool_calls_total = Some(mcp_tool_calls_total);

        let mcp_tool_duration_seconds =
            Family::<McpToolDurationLabels, Histogram>::new_with_constructor(|| {
                Histogram::new(HTTP_DURATION_BUCKETS.iter().copied())
            });
        registry.register(
            "trivy_collector_mcp_tool_duration_seconds",
            "MCP tool execution time in seconds, including queueing for a concurrency slot",
            mcp_tool_duration_seconds.clone(),
        );
        self.mcp_tool_duration_seconds = Some(mcp_tool_duration_seconds);

        let mcp_tool_calls_in_flight = Gauge::default();
        registry.register(
            "trivy_collector_mcp_tool_calls_in_flight",
            "MCP tool invocations currently executing",
            mcp_tool_calls_in_flight.clone(),
        );
        self.mcp_tool_calls_in_flight = Some(mcp_tool_calls_in_flight);

        8
    }

    fn register_scraper(&mut self, registry: &mut Registry) -> usize {
        let db_size_bytes = Gauge::default();
        registry.register(
            "trivy_collector_db_size_bytes",
            "SQLite database file size in bytes",
            db_size_bytes.clone(),
        );
        self.db_size_bytes = Some(db_size_bytes);

        let db_reports_total = Family::<ReportTypeLabels, Gauge>::default();
        registry.register(
            "trivy_collector_db_reports",
            "Reports currently mirrored into the database",
            db_reports_total.clone(),
        );
        self.db_reports_total = Some(db_reports_total);

        let clusters_total = Gauge::default();
        registry.register(
            "trivy_collector_clusters",
            "Clusters registered with the scraper",
            clusters_total.clone(),
        );
        self.clusters_total = Some(clusters_total);

        let fleet_hydrated = Gauge::default();
        registry.register(
            "trivy_collector_fleet_hydrated",
            "1 when every registered cluster has finished its initial sync",
            fleet_hydrated.clone(),
        );
        self.fleet_hydrated = Some(fleet_hydrated);

        4
    }

    /// Pre-initialize server counters so a first scrape shows zeros.
    fn init_for_server(&self) {
        if let Some(ref http) = self.http_requests_total {
            for method in &["GET", "POST", "PUT", "DELETE"] {
                for status in &["200", "400", "404", "500"] {
                    let _ = http.get_or_create(&HttpLabels {
                        method: method.to_string(),
                        status: status.to_string(),
                    });
                }
            }
        }

        if let Some(ref received) = self.reports_received_total {
            for rt in REPORT_TYPES {
                let _ = received.get_or_create(&ReportReceivedLabels {
                    cluster: String::new(),
                    report_type: rt.to_string(),
                });
            }
        }
    }

    /// Pre-initialize scraper gauges so a first scrape shows zeros.
    fn init_for_scraper(&self) {
        if let Some(ref reports) = self.db_reports_total {
            for rt in REPORT_TYPES {
                let _ = reports.get_or_create(&ReportTypeLabels {
                    report_type: rt.to_string(),
                });
            }
        }
    }

    /// Refresh the gauges that describe the scraper's database.
    pub async fn refresh_db_gauges(&self, db: &Database, db_path: &str) {
        if let Some(ref gauge) = self.db_size_bytes
            && let Ok(metadata) = std::fs::metadata(db_path)
        {
            gauge.set(metadata.len() as i64);
        }

        if let Some(ref family) = self.db_reports_total {
            for rt in REPORT_TYPES {
                if let Ok(count) = db.count_reports(rt).await {
                    family
                        .get_or_create(&ReportTypeLabels {
                            report_type: rt.to_string(),
                        })
                        .set(count);
                }
            }
        }
    }

    /// Publish the fleet's hydration state.
    pub fn record_hydration(&self, clusters: usize, hydrated: bool) {
        if let Some(ref gauge) = self.clusters_total {
            gauge.set(clusters as i64);
        }
        if let Some(ref gauge) = self.fleet_hydrated {
            gauge.set(i64::from(hydrated));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus_client::encoding::text::encode;

    fn scrape(registry: &Registry) -> String {
        let mut buf = String::new();
        encode(&mut buf, registry).unwrap();
        buf
    }

    #[test]
    fn server_mode_registers_only_server_metrics() {
        let mut registry = Registry::default();
        let metrics = Metrics::new(&mut registry, Mode::Server);

        assert!(metrics.http_requests_total.is_some());
        assert!(metrics.reports_received_total.is_some());
        assert!(metrics.notes_configmap_bytes.is_some());
        assert!(metrics.api_tokens_total.is_some());
        assert!(metrics.mcp_tool_calls_total.is_some());

        // The server owns no database, so it publishes no database gauges.
        assert!(metrics.db_size_bytes.is_none());
        assert!(metrics.db_reports_total.is_none());
        assert!(metrics.fleet_hydrated.is_none());
    }

    #[test]
    fn scraper_mode_registers_only_scraper_metrics() {
        let mut registry = Registry::default();
        let metrics = Metrics::new(&mut registry, Mode::Scraper);

        assert!(metrics.db_size_bytes.is_some());
        assert!(metrics.db_reports_total.is_some());
        assert!(metrics.clusters_total.is_some());
        assert!(metrics.fleet_hydrated.is_some());

        assert!(metrics.http_requests_total.is_none());
        assert!(metrics.notes_configmap_bytes.is_none());
        assert!(metrics.mcp_tool_calls_total.is_none());
    }

    #[test]
    fn count_matches_what_was_registered() {
        let mut registry = Registry::default();
        assert_eq!(Metrics::new(&mut registry, Mode::Server).count(), 9);

        let mut registry = Registry::default();
        assert_eq!(Metrics::new(&mut registry, Mode::Scraper).count(), 5);
    }

    #[test]
    fn info_carries_the_running_mode() {
        let mut registry = Registry::default();
        let _ = Metrics::new(&mut registry, Mode::Scraper);
        let out = scrape(&registry);
        assert!(out.contains(r#"mode="scraper""#));
        assert!(out.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn server_counters_start_at_zero_rather_than_absent() {
        let mut registry = Registry::default();
        let _ = Metrics::new(&mut registry, Mode::Server);
        let out = scrape(&registry);

        assert!(
            out.contains(r#"trivy_collector_http_requests_total{method="GET",status="200"} 0"#)
        );
        assert!(out.contains(
            r#"trivy_collector_reports_received_total{cluster="",report_type="sbomreport"} 0"#
        ));
        assert!(out.contains("trivy_collector_notes_configmap_bytes 0"));
    }

    #[test]
    fn scraper_gauges_start_at_zero_rather_than_absent() {
        let mut registry = Registry::default();
        let _ = Metrics::new(&mut registry, Mode::Scraper);
        let out = scrape(&registry);

        assert!(out.contains(r#"trivy_collector_db_reports{report_type="sbomreport"} 0"#));
        assert!(out.contains("trivy_collector_fleet_hydrated 0"));
    }

    #[test]
    fn record_hydration_publishes_cluster_count_and_state() {
        let mut registry = Registry::default();
        let metrics = Metrics::new(&mut registry, Mode::Scraper);
        metrics.record_hydration(3, true);

        let out = scrape(&registry);
        assert!(out.contains("trivy_collector_clusters 3"));
        assert!(out.contains("trivy_collector_fleet_hydrated 1"));
    }

    #[test]
    fn record_hydration_is_a_noop_in_server_mode() {
        let mut registry = Registry::default();
        let metrics = Metrics::new(&mut registry, Mode::Server);
        // Must not panic on the mode that never registered these gauges.
        metrics.record_hydration(3, true);
        assert!(!scrape(&registry).contains("trivy_collector_fleet_hydrated"));
    }

    #[tokio::test]
    async fn refresh_db_gauges_reads_report_counts() {
        let mut registry = Registry::default();
        let metrics = Metrics::new(&mut registry, Mode::Scraper);
        let db = Database::new(":memory:").await.unwrap();

        metrics
            .refresh_db_gauges(&db, "/nonexistent/trivy.db")
            .await;

        let out = scrape(&registry);
        assert!(out.contains(r#"trivy_collector_db_reports{report_type="vulnerabilityreport"} 0"#));
        // A missing file leaves the size gauge untouched rather than lying.
        assert!(out.contains("trivy_collector_db_size_bytes 0"));
    }
}
