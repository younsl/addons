//! Deletes artifacts that have gone unserved past their repository's retention
//! `idle_ttl`.

use std::sync::Arc;
use std::time::Duration;

use http::StatusCode;
use tokio_util::sync::CancellationToken;

use crate::audit;
use crate::meta;
use crate::repoconfig;

use super::Manager;

/// Bounds how many expired artifacts are deleted per repository per query,
/// keeping the single-writer SQLite responsive on large sweeps.
pub(crate) const REAP_BATCH: i64 = 256;

impl Manager {
    /// Periodically deletes artifacts that have gone unserved past their
    /// repository's retention `idle_ttl`. Like [`super::Engine::run_sweeper`] it
    /// is leader-gated by the caller so only one instance writes. Deletions
    /// decrement blob references; the sweeper reclaims the freed blobs.
    pub async fn run_idle_reaper(self: Arc<Self>, cancel: CancellationToken, interval: Duration) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => {
                    if let Err(err) = self.reap_once().await {
                        tracing::error!(err = %err, "idle reaper failed");
                    }
                }
            }
        }
    }

    /// Sweeps every repository once, deleting idle artifacts whose repository
    /// configures a positive retention `idle_ttl`, and returns how many it
    /// removed. Each deletion is recorded in the repository audit log with the
    /// artifact path so operators can see exactly what (which package@version)
    /// was auto-removed.
    pub(crate) async fn reap_once(&self) -> Result<i64, meta::Error> {
        let repos = self.store.list_repositories().await?;
        let mut total = 0;
        for repo in repos {
            // OCI artifacts are reference-linked (a manifest needs its layer
            // rows), so per-row idle deletion can break a still-tagged image;
            // the OCI reachability prune reclaims space for the format instead.
            if repo.format == meta::FORMAT_OCI {
                continue;
            }
            let cfg = match repoconfig::parse(&repo.config_json) {
                Ok(cfg) => cfg,
                Err(err) => {
                    tracing::error!(repo = %repo.name, err = %err, "idle reaper: bad repo config");
                    continue;
                }
            };
            let ttl = cfg.retention.idle_ttl.d();
            if ttl.is_zero() {
                continue;
            }
            let cutoff = self.engine.now()
                - chrono::TimeDelta::from_std(ttl).unwrap_or(chrono::TimeDelta::MAX);
            loop {
                let arts = self
                    .store
                    .list_expired_artifacts(repo.id, cutoff, REAP_BATCH)
                    .await?;
                if arts.is_empty() {
                    break;
                }
                let count = arts.len() as i64;
                for art in &arts {
                    match self.store.delete_artifact(repo.id, &art.path).await {
                        Ok(()) => {}
                        Err(e) if e.is_not_found() => continue,
                        Err(e) => return Err(e),
                    }
                    total += 1;
                    self.ttl_expired.with_label_values(&[&repo.name]).inc();
                    if let Some(rec) = &self.rec {
                        rec.record(audit::Event {
                            repo: repo.name.clone(),
                            action: meta::EVENT_TTL_EXPIRE.to_string(),
                            path: art.path.clone(),
                            username: "system".to_string(),
                            status: StatusCode::OK.as_u16() as i64,
                            ..Default::default()
                        });
                    }
                    tracing::debug!(
                        repo = %repo.name, path = %art.path, version = %art.version,
                        "idle artifact reaped"
                    );
                }
                if count < REAP_BATCH {
                    break;
                }
            }
        }
        Ok(total)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use chrono::{DateTime, TimeZone, Utc};

    use crate::meta::{self, Artifact, Store};
    use crate::repoconfig::{Config, Duration};

    use crate::testing::repo::{mk_repo, new_test_manager_with_recorder};

    async fn put(
        store: &Arc<Store>,
        repo_id: i64,
        path: &str,
        version: &str,
        last_accessed: DateTime<Utc>,
    ) {
        store
            .put_artifact(Artifact {
                repo_id,
                path: path.to_string(),
                version: version.to_string(),
                blob_sha256: path.to_string(),
                size: 4,
                cached_at: last_accessed,
                last_accessed_at: last_accessed,
                ..Default::default()
            })
            .await
            .expect("put artifact");
    }

    #[tokio::test]
    async fn reap_once() {
        let (tm, rec) = new_test_manager_with_recorder().await;
        let base: DateTime<Utc> = Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid instant");
        // Fixed clock so idleness is deterministic.
        tm.engine.set_now(Arc::new(move || base));

        // Repo with a 1h idle TTL.
        let mut ttl_cfg = Config::default();
        ttl_cfg.retention.idle_ttl = Duration(3600 * 1_000_000_000);
        let gated = mk_repo(&tm.store, "gated", meta::TYPE_HOSTED, "", ttl_cfg).await;
        // Repo with retention disabled (idle_ttl = 0): nothing is reaped.
        let off = mk_repo(&tm.store, "off", meta::TYPE_HOSTED, "", Config::default()).await;

        // In the gated repo: one idle (served 2h ago) and one fresh (served now).
        put(
            &tm.store,
            gated.id,
            "old/-/old-0.1.0.tgz",
            "0.1.0",
            base - chrono::TimeDelta::hours(2),
        )
        .await;
        put(
            &tm.store,
            gated.id,
            "fresh/-/fresh-2.0.0.tgz",
            "2.0.0",
            base,
        )
        .await;
        // In the disabled repo: an idle artifact that must survive.
        put(
            &tm.store,
            off.id,
            "keep/-/keep-1.0.0.tgz",
            "1.0.0",
            base - chrono::TimeDelta::hours(72),
        )
        .await;

        let n = tm.manager.reap_once().await.expect("reap");
        assert_eq!(n, 1, "only the idle artifact in the gated repo is reaped");

        assert_eq!(
            tm.store.count_artifacts(gated.id).await.expect("count"),
            1,
            "gated repo count (fresh survives)"
        );
        tm.store
            .get_artifact(gated.id, "fresh/-/fresh-2.0.0.tgz")
            .await
            .expect("fresh artifact removed");
        assert_eq!(
            tm.store.count_artifacts(off.id).await.expect("count"),
            1,
            "disabled repo count (retention off)"
        );

        rec.close().await; // flush buffered audit events
        let logs = tm
            .store
            .list_audit_logs("gated", meta::EVENT_TTL_EXPIRE, 10, 0)
            .await
            .expect("list audit logs");
        assert_eq!(logs.len(), 1, "ttl.expire audit entries");
        assert_eq!(logs[0].path, "old/-/old-0.1.0.tgz");
        assert_eq!(logs[0].username, "system");
    }
}
