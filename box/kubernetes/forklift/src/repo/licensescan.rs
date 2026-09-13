//! Queues, runs and backfills the license resolutions the license gate
//! consults.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::meta::{self, Repository};

use super::Manager;
use super::approvalgate::PENDING_MARK_TTL;
use super::cargo::cargo_package;
use super::gomod::go_package;
use super::maven::maven_package;
use super::npm::npm_package;
use super::path_base;
use super::pypi::pypi_package_from_filename;
use super::reaper::REAP_BATCH;
use super::vulnscan::version_for_path;

/// Returns the deps.dev system and package name for an artifact path of the
/// given format, for joining stored license results to listed artifacts. The
/// package name matches the OSV coordinate (Maven uses `group:artifact`); only
/// the system label differs. Returns empty strings when the format has no
/// resolvable coordinate.
pub fn license_coordinate(format: &str, artifact_path: &str) -> (String, String) {
    let system = deps_dev_system(format);
    if system.is_empty() {
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
    (system.to_string(), pkg)
}

/// Maps a forklift repository format to its deps.dev system name. Returns `""`
/// for unsupported formats (the gate then no-ops).
pub fn deps_dev_system(format: &str) -> &'static str {
    match format {
        meta::FORMAT_MAVEN => "maven",
        meta::FORMAT_NPM => "npm",
        meta::FORMAT_CARGO => "cargo",
        meta::FORMAT_GO => "go",
        meta::FORMAT_PYPI => "pypi",
        _ => "",
    }
}

/// One queued license resolution for a package coordinate.
#[derive(Debug, Clone, Default)]
pub(crate) struct ResolveJob {
    pub(crate) system: String,
    pub(crate) package: String,
    pub(crate) version: String,
}

impl Manager {
    /// Enqueues an immediate license resolution for a freshly stored artifact,
    /// so a hosted upload is resolved right away instead of waiting for the
    /// periodic backfill. It is a no-op without a resolver, for unsupported
    /// formats, or for paths that carry no version, and deduplicates like any
    /// enqueue.
    pub(crate) fn resolve_stored(&self, repo: &Repository, artifact_path: &str) {
        if self.resolver.read().is_none() {
            return;
        }
        let (system, pkg) = license_coordinate(&repo.format, artifact_path);
        if pkg.is_empty() {
            return;
        }
        let version = version_for_path(&repo.format, artifact_path);
        if version.is_empty() {
            return;
        }
        self.enqueue_resolve(&system, &pkg, &version);
    }

    /// Schedules an async license resolution for a coordinate, deduplicated
    /// within the pending-mark TTL so hot paths do not flood the queue. Drops
    /// silently when the queue is full (the next request after the mark expires
    /// re-enqueues).
    pub(crate) fn enqueue_resolve(&self, system: &str, pkg: &str, version: &str) {
        if self.resolver.read().is_none()
            || system.is_empty()
            || pkg.is_empty()
            || version.is_empty()
        {
            return;
        }
        let key = format!("license\u{0}{system}\u{0}{pkg}\u{0}{version}");
        if self.req_marks.has(&key) {
            return;
        }
        self.req_marks.set(&key, PENDING_MARK_TTL);
        let _ = self.resolve_queue.tx.try_send(ResolveJob {
            system: system.to_string(),
            package: pkg.to_string(),
            version: version.to_string(),
        });
    }

    /// Drains the resolve queue, querying the license source and storing
    /// results. It runs until `cancel` fires. A no-op without a resolver.
    pub async fn run_license_worker(self: Arc<Self>, cancel: CancellationToken) {
        if self.resolver.read().is_none() {
            return;
        }
        let Some(mut rx) = self.resolve_queue.rx.lock().await.take() else {
            return;
        };
        loop {
            let stop = tokio::select! {
                _ = cancel.cancelled() => true,
                job = rx.recv() => match job {
                    Some(job) => {
                        self.run_resolve(job).await;
                        false
                    }
                    None => true,
                },
            };
            if stop {
                *self.resolve_queue.rx.lock().await = Some(rx);
                return;
            }
        }
    }

    pub(crate) async fn run_resolve(&self, job: ResolveJob) {
        let Some(resolver) = self.resolver.read().clone() else {
            return;
        };
        let res = match resolver
            .resolve(&job.system, &job.package, &job.version)
            .await
        {
            Ok(res) => res,
            Err(err) => {
                self.license_resolves.with_label_values(&["error"]).inc();
                tracing::warn!(
                    system = %job.system, package = %job.package, version = %job.version,
                    err = %err, "license resolve failed"
                );
                return;
            }
        };
        if res.licenses.is_empty() {
            self.license_resolves.with_label_values(&["unknown"]).inc();
        } else {
            self.license_resolves.with_label_values(&["resolved"]).inc();
        }
        if let Err(err) = self
            .store
            .upsert_license_scan(
                &job.system,
                &job.package,
                &job.version,
                &res.licenses,
                resolver.source(),
            )
            .await
        {
            tracing::error!(
                system = %job.system, package = %job.package, version = %job.version,
                err = %err, "store license result failed"
            );
        }
    }

    /// Resolves already-stored artifacts that have never been resolved, so
    /// license data exists for packages uploaded (hosted) or cached (proxy)
    /// before resolution ever covered them. It sweeps once immediately and then
    /// every `interval`. Leader-gated by the caller. A no-op without a resolver.
    pub async fn run_license_backfill(
        self: Arc<Self>,
        cancel: CancellationToken,
        interval: Duration,
    ) {
        if self.resolver.read().is_none() {
            return;
        }
        self.license_backfill_once().await;
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => self.license_backfill_once().await,
            }
        }
    }

    /// Enqueues a resolution for every stored artifact coordinate that has no
    /// result yet. Already-resolved coordinates are skipped (the re-resolver
    /// refreshes those); unsupported formats are ignored.
    pub(crate) async fn license_backfill_once(&self) {
        let resolved = match self.store.resolved_license_keys().await {
            Ok(keys) => keys,
            Err(err) => {
                tracing::error!(err = %err, "license backfill: load resolved keys failed");
                return;
            }
        };
        let mut enqueued = 0i64;
        let mut offset = 0i64;
        loop {
            let targets = match self.store.list_scan_targets(REAP_BATCH, offset).await {
                Ok(t) => t,
                Err(err) => {
                    tracing::error!(err = %err, "license backfill: list targets failed");
                    return;
                }
            };
            for t in &targets {
                let (system, pkg) = license_coordinate(&t.format, &t.path);
                if pkg.is_empty() {
                    continue;
                }
                if resolved.contains(&format!("{system}\u{0}{pkg}\u{0}{}", t.version)) {
                    continue;
                }
                self.enqueue_resolve(&system, &pkg, &t.version);
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
                "license backfill enqueued resolutions for stored artifacts"
            );
        }
    }

    /// Periodically re-enqueues resolutions older than `ttl` so license metadata
    /// changes on already-cached versions surface. Leader-gated by the caller. A
    /// no-op without a resolver.
    pub async fn run_license_rescanner(
        self: Arc<Self>,
        cancel: CancellationToken,
        interval: Duration,
        ttl: Duration,
    ) {
        if self.resolver.read().is_none() {
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
                    let stale = match self.store.list_stale_license_scans(cutoff, REAP_BATCH).await {
                        Ok(s) => s,
                        Err(err) => {
                            tracing::error!(err = %err, "license rescan list failed");
                            continue;
                        }
                    };
                    for s in stale {
                        self.enqueue_resolve(&s.system, &s.package, &s.version);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    mod licensescan_more {
        use std::sync::Arc;
        use std::time::Duration;

        use tokio_util::sync::CancellationToken;

        use crate::meta::{self, Artifact, Repository};

        use crate::repo::licensescan::{ResolveJob, deps_dev_system};
        use crate::testing::repo::new_test_manager;

        #[test]
        fn deps_dev_system_mapping() {
            assert_eq!(deps_dev_system(meta::FORMAT_MAVEN), "maven");
            assert_eq!(
                deps_dev_system("unknown-format"),
                "",
                "unknown system should be empty"
            );
        }

        #[tokio::test]
        async fn run_resolve_persists_and_worker_guards() {
            let tm = new_test_manager().await;
            let repository = tm
                .store
                .create_repository(Repository {
                    name: "mvn-lic".to_string(),
                    format: meta::FORMAT_MAVEN.to_string(),
                    r#type: meta::TYPE_HOSTED.to_string(),
                    ..Default::default()
                })
                .await
                .expect("create repository");

            // No resolver: `resolve_stored` is a no-op and the worker returns.
            tm.manager
                .resolve_stored(&repository, "com/acme/widget/1.0.0/widget-1.0.0.jar");
            Arc::clone(&tm.manager)
                .run_license_worker(CancellationToken::new())
                .await;

            tm.manager.set_license_resolver(Some(Arc::new(
                crate::repo::licensegate::tests::FakeResolver,
            )));
            tm.manager
                .run_resolve(ResolveJob {
                    system: "maven".to_string(),
                    package: "com.acme:widget".to_string(),
                    version: "1.0.0".to_string(),
                })
                .await;
            tm.store
                .get_license_scan("maven", "com.acme:widget", "1.0.0")
                .await
                .expect("get license scan");

            // A cancelled token makes the worker exit its select loop.
            let cancelled = CancellationToken::new();
            cancelled.cancel();
            tokio::time::timeout(
                Duration::from_secs(5),
                Arc::clone(&tm.manager).run_license_worker(cancelled),
            )
            .await
            .expect("worker did not stop on cancel");
        }

        #[tokio::test]
        async fn license_backfill_once_enqueues() {
            let tm = new_test_manager().await;
            let repository = tm
                .store
                .create_repository(Repository {
                    name: "mvn-lic-bf".to_string(),
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
                    blob_sha256: "digest-lic".to_string(),
                    size: 4,
                    ..Default::default()
                })
                .await
                .expect("put artifact");
            tm.manager.set_license_resolver(Some(Arc::new(
                crate::repo::licensegate::tests::FakeResolver,
            )));
            tm.manager.license_backfill_once().await;

            let job = {
                let mut rx = tm.manager.resolve_queue.rx.lock().await;
                rx.as_mut()
                    .expect("queue receiver")
                    .try_recv()
                    .expect("license backfill enqueued nothing")
            };
            tm.manager.run_resolve(job).await;
            tm.store
                .get_license_scan("maven", "com.acme:lib", "2.0.0")
                .await
                .expect("backfilled license scan missing");
        }
    }
}
