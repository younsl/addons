//! Queues, runs and backfills the vulnerability scans the vuln gate consults.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

use crate::meta::{self, Error, Repository, VulnAdvisory};

use super::Manager;
use super::approvalgate::PENDING_MARK_TTL;
use super::cargo::{cargo_package, cargo_version};
use super::gomod::{go_package, go_version};
use super::maven::{maven_package, maven_version};
use super::npm::{npm_package, npm_version};
use super::path_base;
use super::pypi::{pypi_package_from_filename, pypi_version};
use super::reaper::REAP_BATCH;

/// Returns the OSV ecosystem and package name for an artifact path of the given
/// format, for joining stored scan results to listed artifacts. Returns empty
/// strings when the format has no scannable coordinate.
pub fn vuln_coordinate(format: &str, artifact_path: &str) -> (String, String) {
    let eco = osv_ecosystem(format);
    if eco.is_empty() {
        return (String::new(), String::new());
    }
    let pkg = match format {
        meta::FORMAT_MAVEN => maven_package(artifact_path),
        meta::FORMAT_NPM => npm_package(artifact_path),
        meta::FORMAT_CARGO => cargo_package(artifact_path),
        meta::FORMAT_GO => go_package(artifact_path),
        meta::FORMAT_PYPI => pypi_package_from_filename(path_base(artifact_path)),
        _ => return (String::new(), String::new()),
    };
    (eco.to_string(), pkg)
}

/// Extracts the version from a stored artifact path using the per-format rules,
/// mirroring how the format handlers derive it. Returns `""` when the path
/// carries no version (metadata/index paths).
pub(crate) fn version_for_path(format: &str, artifact_path: &str) -> String {
    match format {
        meta::FORMAT_MAVEN => maven_version(artifact_path),
        meta::FORMAT_NPM => npm_version(artifact_path),
        meta::FORMAT_CARGO => cargo_version(artifact_path),
        meta::FORMAT_GO => go_version(artifact_path),
        meta::FORMAT_PYPI => pypi_version(path_base(artifact_path)),
        _ => String::new(),
    }
}

/// Counts, for one repository, how many stored artifacts have a vulnerability
/// scan and how many of those are clean (no advisories).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanRatio {
    pub scanned: i64,
    pub clean: i64,
}

/// Bounds how stale the cached scan-coverage ratios may be.
///
/// The computation reads every versioned artifact and the whole scan table, then
/// derives a coordinate per row, so it is the most expensive thing the
/// repository list does — and it was doing it once per request, per viewer, per
/// tab. The ratio is a coverage indicator on a list page: a value up to half a
/// minute old is indistinguishable to a reader, while recomputing it per request
/// is what turns a few open consoles into a database queue.
pub(crate) const SCAN_RATIO_TTL: Duration = Duration::from_secs(30);

/// One queued vulnerability scan for a package coordinate.
#[derive(Debug, Clone, Default)]
pub(crate) struct ScanJob {
    pub(crate) ecosystem: String,
    pub(crate) package: String,
    pub(crate) version: String,
}

/// Maps a forklift repository format to its OSV ecosystem name. Returns `""`
/// for formats OSV does not cover (the gate then no-ops).
pub fn osv_ecosystem(format: &str) -> &'static str {
    match format {
        meta::FORMAT_MAVEN => "Maven",
        meta::FORMAT_NPM => "npm",
        meta::FORMAT_CARGO => "crates.io",
        meta::FORMAT_GO => "Go",
        meta::FORMAT_PYPI => "PyPI",
        _ => "",
    }
}

impl Manager {
    /// Enqueues an immediate vulnerability scan for a freshly stored artifact,
    /// so a hosted upload is scanned right away instead of waiting for the
    /// periodic backfill. The version is derived from the artifact path. It is a
    /// no-op without a scanner, for unscannable formats, or for paths that carry
    /// no version (e.g. an npm packument index), and deduplicates like any
    /// enqueue.
    pub(crate) fn scan_stored(&self, repo: &Repository, artifact_path: &str) {
        if self.scanner.read().is_none() {
            return;
        }
        let (eco, pkg) = vuln_coordinate(&repo.format, artifact_path);
        if pkg.is_empty() {
            return;
        }
        let version = version_for_path(&repo.format, artifact_path);
        if version.is_empty() {
            return;
        }
        self.enqueue_scan(&eco, &pkg, &version);
    }

    /// Returns per-repository scan aggregates for the list view: for each
    /// repository id, the number of scanned artifacts and how many are clean.
    /// An artifact counts as scanned when a stored scan exists for its
    /// coordinate, and clean when that scan's max severity is "none" (no
    /// advisories). Artifacts with an unscannable format or a versionless path
    /// (metadata/index) are ignored, mirroring how [`Manager::scan_stored`]
    /// decides what to enqueue. Two bulk queries, no N+1.
    ///
    /// Results are cached for [`SCAN_RATIO_TTL`]. The lock is held across the
    /// recomputation on purpose: concurrent callers wait for the one pass
    /// instead of each starting their own, so a burst of console requests costs
    /// a single scan.
    pub async fn clean_scan_ratios(&self) -> Result<HashMap<i64, ScanRatio>, Error> {
        let _guard = self.scan_ratio_refresh.lock().await;
        {
            let cached = self.scan_ratios.lock();
            if let Some(at) = cached.at
                && (self.engine.now() - at)
                    .to_std()
                    .is_ok_and(|elapsed| elapsed < SCAN_RATIO_TTL)
            {
                self.scan_ratio_served.with_label_values(&["hit"]).inc();
                return Ok(cached.ratios.clone());
            }
        }
        let started = self.engine.now();
        let ratios = self.compute_clean_scan_ratios().await?;
        {
            let mut cached = self.scan_ratios.lock();
            cached.ratios = ratios.clone();
            cached.at = Some(self.engine.now());
        }
        self.scan_ratio_served.with_label_values(&["miss"]).inc();
        self.scan_ratio_cost
            .observe(seconds_since(started, self.engine.now()));
        Ok(ratios)
    }

    /// Does the actual aggregation.
    async fn compute_clean_scan_ratios(&self) -> Result<HashMap<i64, ScanRatio>, Error> {
        let targets = self.store.all_scan_targets().await?;
        let severity = self.store.vuln_severity_by_coordinate().await?;
        let mut out: HashMap<i64, ScanRatio> = HashMap::new();
        for t in targets {
            let (eco, pkg) = vuln_coordinate(&t.format, &t.path);
            if pkg.is_empty() {
                continue;
            }
            let version = version_for_path(&t.format, &t.path);
            if version.is_empty() {
                continue;
            }
            let Some(sev) = severity.get(&format!("{eco}\u{0}{pkg}\u{0}{version}")) else {
                continue;
            };
            let entry = out.entry(t.repo_id).or_default();
            entry.scanned += 1;
            if sev == "none" {
                entry.clean += 1;
            }
        }
        Ok(out)
    }

    /// Schedules an async scan for a coordinate, deduplicated within the
    /// pending-mark TTL so hot paths do not flood the queue. Drops silently when
    /// the queue is full (the next request after the mark expires re-enqueues).
    /// An empty version enqueues a package-level scan (used by the approval gate
    /// when the requested version is unknown).
    pub(crate) fn enqueue_scan(&self, eco: &str, pkg: &str, version: &str) {
        if self.scanner.read().is_none() || eco.is_empty() || pkg.is_empty() {
            return;
        }
        let key = format!("scan\u{0}{eco}\u{0}{pkg}\u{0}{version}");
        if self.req_marks.has(&key) {
            return;
        }
        self.req_marks.set(&key, PENDING_MARK_TTL);
        let _ = self.scan_queue.tx.try_send(ScanJob {
            ecosystem: eco.to_string(),
            package: pkg.to_string(),
            version: version.to_string(),
        });
    }

    /// Drains the scan queue, querying the advisory source and storing results.
    /// It runs until `cancel` fires. A no-op when no scanner is set.
    pub async fn run_vuln_worker(self: Arc<Self>, cancel: CancellationToken) {
        if self.scanner.read().is_none() {
            return;
        }
        let Some(mut rx) = self.scan_queue.rx.lock().await.take() else {
            return;
        };
        loop {
            let stop = tokio::select! {
                _ = cancel.cancelled() => true,
                job = rx.recv() => match job {
                    Some(job) => {
                        self.run_scan(job).await;
                        false
                    }
                    None => true,
                },
            };
            if stop {
                *self.scan_queue.rx.lock().await = Some(rx);
                return;
            }
        }
    }

    pub(crate) async fn run_scan(&self, job: ScanJob) {
        let Some(scanner) = self.scanner.read().clone() else {
            return;
        };
        let start = self.engine.now();
        let finding = match scanner
            .query(&job.ecosystem, &job.package, &job.version)
            .await
        {
            Ok(f) => f,
            Err(err) => {
                self.vuln_scans.with_label_values(&["error"]).inc();
                tracing::warn!(
                    ecosystem = %job.ecosystem, package = %job.package, version = %job.version,
                    err = %err, "vuln scan failed"
                );
                return;
            }
        };
        let duration_ms = (self.engine.now() - start).num_milliseconds();
        if finding.ids.is_empty() {
            self.vuln_scans.with_label_values(&["clean"]).inc();
        } else {
            self.vuln_scans.with_label_values(&["vulnerable"]).inc();
        }
        let advisories: Vec<VulnAdvisory> = finding
            .advisories
            .iter()
            .map(|a| VulnAdvisory {
                id: a.id.clone(),
                severity: a.severity.clone(),
                score: a.score.clone(),
            })
            .collect();
        let counts: HashMap<String, i64> = finding.severity_counts().into_iter().collect();
        if let Err(err) = self
            .store
            .upsert_vuln_scan(
                &job.ecosystem,
                &job.package,
                &job.version,
                finding.max.as_str(),
                &finding.ids,
                &counts,
                duration_ms,
                &advisories,
                scanner.source(),
            )
            .await
        {
            tracing::error!(
                ecosystem = %job.ecosystem, package = %job.package, version = %job.version,
                err = %err, "store vuln scan failed"
            );
        }
    }

    /// Scans already-stored artifacts that have never been scanned, so
    /// vulnerability data exists for packages uploaded (hosted) or cached
    /// (proxy) before a scan ever covered them. It sweeps once immediately and
    /// then every `interval`. Leader-gated by the caller (only one instance
    /// should drive the queue). A no-op without a scanner.
    pub async fn run_vuln_backfill(self: Arc<Self>, cancel: CancellationToken, interval: Duration) {
        if self.scanner.read().is_none() {
            return;
        }
        self.backfill_once().await;
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => self.backfill_once().await,
            }
        }
    }

    /// Enqueues a scan for every stored artifact coordinate that has no scan
    /// yet. Already-scanned coordinates are skipped (the rescanner refreshes
    /// those); unscannable formats are ignored. Coordinates dropped because the
    /// queue was full are re-enqueued on the next sweep, since they remain
    /// unscanned.
    pub(crate) async fn backfill_once(&self) {
        let scanned = match self.store.scanned_keys().await {
            Ok(keys) => keys,
            Err(err) => {
                tracing::error!(err = %err, "vuln backfill: load scanned keys failed");
                return;
            }
        };
        let mut enqueued = 0i64;
        let mut offset = 0i64;
        loop {
            let targets = match self.store.list_scan_targets(REAP_BATCH, offset).await {
                Ok(t) => t,
                Err(err) => {
                    tracing::error!(err = %err, "vuln backfill: list targets failed");
                    return;
                }
            };
            for t in &targets {
                let (eco, pkg) = vuln_coordinate(&t.format, &t.path);
                if pkg.is_empty() {
                    continue;
                }
                if scanned.contains(&format!("{eco}\u{0}{pkg}\u{0}{}", t.version)) {
                    continue;
                }
                self.enqueue_scan(&eco, &pkg, &t.version);
                enqueued += 1;
            }
            if (targets.len() as i64) < REAP_BATCH {
                break;
            }
            offset += REAP_BATCH;
        }
        if enqueued > 0 {
            tracing::info!(
                count = enqueued,
                "vuln backfill enqueued scans for stored artifacts"
            );
        }
    }

    /// Periodically re-enqueues scans older than `ttl` so newly disclosed
    /// advisories on already-cached versions surface. Leader-gated by the caller
    /// (only one instance should drive the queue). A no-op without a scanner.
    pub async fn run_vuln_rescanner(
        self: Arc<Self>,
        cancel: CancellationToken,
        interval: Duration,
        ttl: Duration,
    ) {
        if self.scanner.read().is_none() {
            return;
        }
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => {
                    let cutoff = self.engine.now()
                        - chrono::TimeDelta::from_std(ttl).unwrap_or(chrono::TimeDelta::zero());
                    let stale = match self.store.list_stale_vuln_scans(cutoff, REAP_BATCH).await {
                        Ok(s) => s,
                        Err(err) => {
                            tracing::error!(err = %err, "vuln rescan list failed");
                            continue;
                        }
                    };
                    for s in stale {
                        self.enqueue_scan(&s.ecosystem, &s.package, &s.version);
                    }
                }
            }
        }
    }
}

/// The elapsed seconds between two instants, as `time.Sub(...).Seconds()`.
pub(crate) fn seconds_since(start: DateTime<Utc>, end: DateTime<Utc>) -> f64 {
    (end - start).num_nanoseconds().unwrap_or(0) as f64 / 1e9
}

#[cfg(test)]
pub(crate) mod tests {
    mod scanratio_cache {
        use std::collections::HashMap;
        use std::sync::Arc;

        use chrono::{DateTime, TimeZone, Utc};
        use parking_lot::Mutex;

        use crate::meta::{self, Artifact, Repository};

        use crate::repo::vulnscan::SCAN_RATIO_TTL;
        use crate::testing::repo::new_test_manager;

        /// The repository list called this per request, and the computation reads every
        /// versioned artifact plus the whole scan table, so a few open consoles were
        /// enough to keep the database busy. Caching it is the fix, which makes two
        /// things contractual: inside the window no second scan runs (the value is
        /// served as-is, even if artifacts changed), and past the window it refreshes.
        #[tokio::test]
        async fn clean_scan_ratios_cached() {
            let tm = new_test_manager().await;
            let clock = Arc::new(Mutex::new(
                Utc.with_ymd_and_hms(2026, 8, 20, 10, 0, 0)
                    .single()
                    .expect("valid instant"),
            ));
            let handle = Arc::clone(&clock);
            tm.engine.set_now(Arc::new(move || *handle.lock()));

            let repo = tm
                .store
                .create_repository(Repository {
                    name: "npm-hosted".to_string(),
                    format: meta::FORMAT_NPM.to_string(),
                    r#type: meta::TYPE_HOSTED.to_string(),
                    ..Default::default()
                })
                .await
                .expect("create repository");

            let seed = async |pkg: &str, version: &str| {
                let path = format!("{pkg}/-/{pkg}-{version}.tgz");
                tm.store
                    .put_artifact(Artifact {
                        repo_id: repo.id,
                        path,
                        version: version.to_string(),
                        blob_sha256: format!("sha-{version}"),
                        size: 10,
                        ..Default::default()
                    })
                    .await
                    .expect("put artifact");
                tm.store
                    .upsert_vuln_scan(
                        "npm",
                        pkg,
                        version,
                        "none",
                        &[],
                        &HashMap::new(),
                        0,
                        &[],
                        "OSV",
                    )
                    .await
                    .expect("put scan");
            };
            seed("lodash", "4.17.21").await;

            let ratios = tm.manager.clean_scan_ratios().await.expect("first call");
            let got = ratios.get(&repo.id).copied().unwrap_or_default();
            assert_eq!(
                (got.scanned, got.clean),
                (1, 1),
                "first call should see one scanned and clean artifact"
            );

            // A new artifact inside the window is deliberately not visible: the whole
            // point is that no second scan runs for it.
            seed("axios", "1.6.0").await;
            advance(&clock, SCAN_RATIO_TTL.as_secs() as i64 - 1);
            let ratios = tm.manager.clean_scan_ratios().await.expect("cached call");
            let got = ratios.get(&repo.id).copied().unwrap_or_default();
            assert_eq!(got.scanned, 1, "inside the window should serve the cache");
            assert_eq!(
                tm.manager
                    .scan_ratio_served
                    .with_label_values(&["hit"])
                    .get(),
                1.0,
                "cache hits"
            );

            // Past the window it recomputes, and the refresh is timed so the cost of the
            // scan stays visible.
            advance(&clock, 2);
            let ratios = tm
                .manager
                .clean_scan_ratios()
                .await
                .expect("refreshed call");
            let got = ratios.get(&repo.id).copied().unwrap_or_default();
            assert_eq!(
                (got.scanned, got.clean),
                (2, 2),
                "after the window both artifacts should be counted"
            );
            assert_eq!(
                tm.manager
                    .scan_ratio_served
                    .with_label_values(&["miss"])
                    .get(),
                2.0,
                "cache misses (the first call and the refresh)"
            );
        }

        fn advance(clock: &Arc<Mutex<DateTime<Utc>>>, seconds: i64) {
            let mut now = clock.lock();
            *now += chrono::TimeDelta::seconds(seconds);
        }
    }

    mod vulnscan_more {
        use std::sync::Arc;
        use std::time::Duration;

        use tokio_util::sync::CancellationToken;

        use crate::meta::{self, Artifact, Repository};

        use crate::repo::vulnscan::{ScanJob, osv_ecosystem, vuln_coordinate};
        use crate::testing::repo::new_test_manager;

        #[test]
        fn vuln_coordinate_and_ecosystem() {
            let (eco, pkg) =
                vuln_coordinate(meta::FORMAT_MAVEN, "com/acme/widget/1.0.0/widget-1.0.0.jar");
            assert_eq!((eco.as_str(), pkg.as_str()), ("Maven", "com.acme:widget"));
            assert_eq!(osv_ecosystem(meta::FORMAT_NPM), "npm");
            assert_eq!(
                osv_ecosystem("unknown-format"),
                "",
                "unknown ecosystem should be empty"
            );
        }

        #[tokio::test]
        async fn run_scan_persists_and_worker_guards() {
            let tm = new_test_manager().await;
            let repository = tm
                .store
                .create_repository(Repository {
                    name: "mvn-scan".to_string(),
                    format: meta::FORMAT_MAVEN.to_string(),
                    r#type: meta::TYPE_HOSTED.to_string(),
                    ..Default::default()
                })
                .await
                .expect("create repository");

            // Without a scanner `scan_stored` is a no-op and the worker returns
            // immediately.
            tm.manager
                .scan_stored(&repository, "com/acme/widget/1.0.0/widget-1.0.0.jar");
            Arc::clone(&tm.manager)
                .run_vuln_worker(CancellationToken::new())
                .await;

            tm.manager
                .set_vuln_scanner(Some(Arc::new(crate::repo::vulngate::tests::FakeScanner)));
            // `run_scan` performs the query and persists the (clean) verdict.
            tm.manager
                .run_scan(ScanJob {
                    ecosystem: "Maven".to_string(),
                    package: "com.acme:widget".to_string(),
                    version: "1.0.0".to_string(),
                })
                .await;
            let scan = tm
                .store
                .get_vuln_scan("Maven", "com.acme:widget", "1.0.0")
                .await
                .expect("get vuln scan");
            assert_eq!(scan.source, "fake");

            // A cancelled token makes the worker exit its select loop.
            let cancelled = CancellationToken::new();
            cancelled.cancel();
            tokio::time::timeout(
                Duration::from_secs(5),
                Arc::clone(&tm.manager).run_vuln_worker(cancelled),
            )
            .await
            .expect("worker did not stop on cancel");

            // `clean_scan_ratios` runs its aggregate query without error.
            tm.manager
                .clean_scan_ratios()
                .await
                .expect("clean_scan_ratios");
        }

        #[tokio::test]
        async fn backfill_once_enqueues_unscanned() {
            let tm = new_test_manager().await;
            let repository = tm
                .store
                .create_repository(Repository {
                    name: "mvn-bf".to_string(),
                    format: meta::FORMAT_MAVEN.to_string(),
                    r#type: meta::TYPE_HOSTED.to_string(),
                    ..Default::default()
                })
                .await
                .expect("create repository");
            tm.store
                .put_artifact(Artifact {
                    repo_id: repository.id,
                    path: "com/acme/lib/2.0.0/lib-2.0.0.jar".to_string(),
                    version: "2.0.0".to_string(),
                    blob_sha256: "digest-bf".to_string(),
                    size: 4,
                    ..Default::default()
                })
                .await
                .expect("put artifact");
            tm.manager
                .set_vuln_scanner(Some(Arc::new(crate::repo::vulngate::tests::FakeScanner)));
            tm.manager.backfill_once().await;

            // Drain the queued job and persist it.
            let job = {
                let mut rx = tm.manager.scan_queue.rx.lock().await;
                rx.as_mut()
                    .expect("queue receiver")
                    .try_recv()
                    .expect("backfill enqueued no scan for the unscanned artifact")
            };
            tm.manager.run_scan(job).await;
            tm.store
                .get_vuln_scan("Maven", "com.acme:lib", "2.0.0")
                .await
                .expect("backfilled scan missing");
        }
    }
}
