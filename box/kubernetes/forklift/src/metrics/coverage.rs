//! How much of the organisation's source builds through this forklift.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use prometheus::core::{Collector, Desc};
use prometheus::proto::MetricFamily;

use super::{counter_family, desc, gauge_family};

/// One reading of the coverage picture, assembled by the caller from the
/// scanner. It is a plain struct so this module does not depend on the
/// scanner, and the scanner does not depend on Prometheus.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CoverageStats {
    /// Reports whether coverage scanning is switched on and pointed at a
    /// GitLab instance. Public so an absent measurement can be told apart from
    /// a broken one.
    pub enabled: bool,
    /// The coverage denominator: in-scope projects that have CI.
    pub target: i64,
    /// Counts per verdict, plus the two out-of-scope buckets.
    pub applied: i64,
    pub partial: i64,
    pub not_applied: i64,
    pub errored: i64,
    pub no_ci: i64,
    pub muted: i64,
    /// Applied over target, as the console shows it.
    pub percent: i64,
    pub last_scanned_at: Option<DateTime<Utc>>,
    pub last_scan_seconds: f64,
    /// True while a crawl is in flight.
    pub scanning: bool,
    /// Reports whether the most recent attempt ended in an error.
    pub last_scan_failed: bool,
    /// Scan attempt counters since process start.
    pub scans_succeeded: i64,
    pub scans_failed: i64,
    /// Concurrency the adaptive limiter settled on during the last crawl.
    pub concurrency: i64,
    pub peak_concurrency: i64,
}

/// Exports how much of the organisation's source builds through this forklift.
///
/// It exists because the failure this feature has is a silent one. A scan that
/// stops running, or one whose access token quietly expires, looks exactly
/// like a healthy instance from the outside: the console keeps showing the
/// last result and nobody notices the number stopped moving. The scan
/// timestamp and the failure counter are what make that alertable, and the
/// verdict counts are what make the rollout itself a graph rather than a page
/// somebody remembers to open.
pub struct CoverageCollector {
    stats: Arc<dyn Fn() -> CoverageStats + Send + Sync>,

    enabled: Desc,
    target: Desc,
    projects: Desc,
    percent: Desc,
    last_scan: Desc,
    scan_seconds: Desc,
    scanning: Desc,
    scans: Desc,
    concurrency: Desc,
    peak: Desc,
}

impl CoverageCollector {
    /// Builds a collector over a function that reads the scanner's current
    /// state. The read is in-memory, so it is safe on the scrape path.
    pub fn new(stats: Arc<dyn Fn() -> CoverageStats + Send + Sync>) -> CoverageCollector {
        CoverageCollector {
            stats,
            enabled: desc(
                "forklift_coverage_enabled",
                "1 when coverage scanning is switched on and has a GitLab connection, else 0.",
                &[],
            ),
            target: desc(
                "forklift_coverage_target",
                "Projects in scope that have CI; the coverage denominator. Zero usually means the access token sees nothing.",
                &[],
            ),
            projects: desc(
                "forklift_coverage_projects",
                "Scanned projects by verdict. applied and partial are wired, wholly or in half; no_ci and muted are out of scope.",
                &["state"],
            ),
            percent: desc(
                "forklift_coverage_percent",
                "Applied projects as a percentage of the target.",
                &[],
            ),
            last_scan: desc(
                "forklift_coverage_last_scan_timestamp_seconds",
                "When the last scan completed, as a Unix timestamp. Zero before the first one. Alert on its age: a scan that stopped running looks healthy from outside.",
                &[],
            ),
            scan_seconds: desc(
                "forklift_coverage_last_scan_duration_seconds",
                "How long the last completed scan took.",
                &[],
            ),
            scanning: desc(
                "forklift_coverage_scanning",
                "1 while a scan is in flight, else 0.",
                &[],
            ),
            scans: desc(
                "forklift_coverage_scans_total",
                "Scan attempts since start, by outcome. A rising failure count with a stale timestamp is the token or the instance, not the schedule.",
                &["result"],
            ),
            concurrency: desc(
                "forklift_coverage_gitlab_concurrency",
                "In-flight request limit the adaptive controller settled on during the last crawl. There is no rate setting, so this is what the instance turned out to tolerate.",
                &[],
            ),
            peak: desc(
                "forklift_coverage_gitlab_concurrency_peak",
                "Highest in-flight request limit reached during the last crawl.",
                &[],
            ),
        }
    }
}

impl Collector for CoverageCollector {
    fn desc(&self) -> Vec<&Desc> {
        vec![
            &self.enabled,
            &self.target,
            &self.projects,
            &self.percent,
            &self.last_scan,
            &self.scan_seconds,
            &self.scanning,
            &self.scans,
            &self.concurrency,
            &self.peak,
        ]
    }

    fn collect(&self) -> Vec<MetricFamily> {
        let s = (self.stats)();
        let gauge = |d: &Desc, v: f64| gauge_family(d, vec![(vec![], v)]);

        // Zero rather than a negative timestamp before the first scan, so an
        // alert on age can treat "never" and "stale" the same way.
        let last_scan = s
            .last_scanned_at
            .filter(|t| !crate::meta::time::is_zero(*t))
            .map_or(0.0, |t| t.timestamp() as f64);

        vec![
            gauge(&self.enabled, bool_value(s.enabled)),
            gauge(&self.target, s.target as f64),
            gauge(&self.percent, s.percent as f64),
            gauge(&self.scanning, bool_value(s.scanning)),
            gauge(&self.scan_seconds, s.last_scan_seconds),
            gauge(&self.concurrency, s.concurrency as f64),
            gauge(&self.peak, s.peak_concurrency as f64),
            gauge(&self.last_scan, last_scan),
            gauge_family(
                &self.projects,
                vec![
                    (vec!["applied".into()], s.applied as f64),
                    (vec!["partial".into()], s.partial as f64),
                    (vec!["not_applied".into()], s.not_applied as f64),
                    (vec!["error".into()], s.errored as f64),
                    (vec!["no_ci".into()], s.no_ci as f64),
                    (vec!["muted".into()], s.muted as f64),
                ],
            ),
            counter_family(
                &self.scans,
                vec![
                    (vec!["success".into()], s.scans_succeeded as f64),
                    (vec!["failure".into()], s.scans_failed as f64),
                ],
            ),
        ]
    }
}

fn bool_value(b: bool) -> f64 {
    if b { 1.0 } else { 0.0 }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use chrono::TimeZone;
    use prometheus::Registry;

    use crate::metrics::coverage::*;
    use crate::metrics::tests::encode_filtered;

    /// The coverage metrics exist for one question nobody thinks to ask: is the
    /// scan still running. A stopped scan looks exactly like a healthy one from
    /// outside, because the console keeps showing the last result. So the
    /// timestamp has to be exported as an absolute instant an age alert can be
    /// built on, the failure count has to be a counter, and the verdict counts
    /// have to be labelled apart rather than folded into the percentage.
    #[test]
    fn coverage_collector() {
        let scanned = Utc.timestamp_opt(1_770_000_000, 0).unwrap();
        let collector = CoverageCollector::new(Arc::new(move || CoverageStats {
            enabled: true,
            target: 4,
            applied: 1,
            partial: 1,
            not_applied: 1,
            errored: 1,
            no_ci: 2,
            muted: 3,
            percent: 25,
            last_scanned_at: Some(scanned),
            last_scan_seconds: 12.5,
            scanning: false,
            scans_succeeded: 7,
            scans_failed: 2,
            concurrency: 14,
            peak_concurrency: 22,
            ..Default::default()
        }));
        let reg = Registry::new();
        reg.register(Box::new(collector)).unwrap();

        let want = r#"# HELP forklift_coverage_enabled 1 when coverage scanning is switched on and has a GitLab connection, else 0.
# TYPE forklift_coverage_enabled gauge
forklift_coverage_enabled 1
# HELP forklift_coverage_gitlab_concurrency In-flight request limit the adaptive controller settled on during the last crawl. There is no rate setting, so this is what the instance turned out to tolerate.
# TYPE forklift_coverage_gitlab_concurrency gauge
forklift_coverage_gitlab_concurrency 14
# HELP forklift_coverage_last_scan_duration_seconds How long the last completed scan took.
# TYPE forklift_coverage_last_scan_duration_seconds gauge
forklift_coverage_last_scan_duration_seconds 12.5
# HELP forklift_coverage_last_scan_timestamp_seconds When the last scan completed, as a Unix timestamp. Zero before the first one. Alert on its age: a scan that stopped running looks healthy from outside.
# TYPE forklift_coverage_last_scan_timestamp_seconds gauge
forklift_coverage_last_scan_timestamp_seconds 1770000000
# HELP forklift_coverage_percent Applied projects as a percentage of the target.
# TYPE forklift_coverage_percent gauge
forklift_coverage_percent 25
# HELP forklift_coverage_projects Scanned projects by verdict. applied and partial are wired, wholly or in half; no_ci and muted are out of scope.
# TYPE forklift_coverage_projects gauge
forklift_coverage_projects{state="applied"} 1
forklift_coverage_projects{state="error"} 1
forklift_coverage_projects{state="muted"} 3
forklift_coverage_projects{state="no_ci"} 2
forklift_coverage_projects{state="not_applied"} 1
forklift_coverage_projects{state="partial"} 1
# HELP forklift_coverage_scanning 1 while a scan is in flight, else 0.
# TYPE forklift_coverage_scanning gauge
forklift_coverage_scanning 0
# HELP forklift_coverage_scans_total Scan attempts since start, by outcome. A rising failure count with a stale timestamp is the token or the instance, not the schedule.
# TYPE forklift_coverage_scans_total counter
forklift_coverage_scans_total{result="failure"} 2
forklift_coverage_scans_total{result="success"} 7
# HELP forklift_coverage_target Projects in scope that have CI; the coverage denominator. Zero usually means the access token sees nothing.
# TYPE forklift_coverage_target gauge
forklift_coverage_target 4
"#;
        assert_eq!(
            encode_filtered(
                &reg,
                &[
                    "forklift_coverage_enabled",
                    "forklift_coverage_gitlab_concurrency",
                    "forklift_coverage_last_scan_duration_seconds",
                    "forklift_coverage_last_scan_timestamp_seconds",
                    "forklift_coverage_percent",
                    "forklift_coverage_projects",
                    "forklift_coverage_scanning",
                    "forklift_coverage_scans_total",
                    "forklift_coverage_target",
                ]
            ),
            want
        );
    }

    /// Before the first scan the timestamp is zero rather than negative or absent,
    /// so one age alert covers "never ran" and "stopped running" without a special
    /// case.
    #[test]
    fn coverage_collector_before_first_scan() {
        let collector = CoverageCollector::new(Arc::new(|| CoverageStats {
            enabled: true,
            ..Default::default()
        }));
        let reg = Registry::new();
        reg.register(Box::new(collector)).unwrap();

        let want = r#"# HELP forklift_coverage_last_scan_timestamp_seconds When the last scan completed, as a Unix timestamp. Zero before the first one. Alert on its age: a scan that stopped running looks healthy from outside.
# TYPE forklift_coverage_last_scan_timestamp_seconds gauge
forklift_coverage_last_scan_timestamp_seconds 0
"#;
        assert_eq!(
            encode_filtered(&reg, &["forklift_coverage_last_scan_timestamp_seconds"]),
            want
        );
    }

    /// A forklift with coverage turned off still reports the gauge, so a dashboard
    /// can say "off" instead of showing a gap that reads as broken.
    #[test]
    fn coverage_collector_reports_disabled() {
        let collector = CoverageCollector::new(Arc::new(CoverageStats::default));
        let reg = Registry::new();
        reg.register(Box::new(collector)).unwrap();

        let want = r#"# HELP forklift_coverage_enabled 1 when coverage scanning is switched on and has a GitLab connection, else 0.
# TYPE forklift_coverage_enabled gauge
forklift_coverage_enabled 0
"#;
        assert_eq!(encode_filtered(&reg, &["forklift_coverage_enabled"]), want);
    }
}
