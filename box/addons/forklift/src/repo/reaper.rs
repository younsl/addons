//! Deletes artifacts that have gone unserved past their repository's retention
//! `idle_ttl`.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use http::StatusCode;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::audit;
use crate::meta;
use crate::repoconfig;

use super::Manager;
use super::oci::oci_manifest_path;

/// Bounds how many expired artifacts are deleted per repository per query,
/// keeping the single-writer SQLite responsive on large sweeps.
pub(crate) const REAP_BATCH: i64 = 256;

/// Explains one retention deletion in its audit entry: the rule, the setting
/// in force, and the last activity that fell behind it.
pub(crate) fn retention_detail(
    ttl: Duration,
    last_active: DateTime<Utc>,
    digest: Option<&str>,
) -> String {
    let mut detail = serde_json::json!({
        "reason": "idle_retention",
        "idle_ttl": crate::repoconfig::Duration::from_std(ttl),
        "last_active_at": last_active,
    });
    if let Some(digest) = digest {
        detail["manifest_digest"] = serde_json::Value::from(digest);
    }
    detail.to_string()
}

/// Bounds how many rows a retention preview lists; the totals cover them all.
const PREVIEW_ITEMS: i64 = 20;

/// What the idle reaper would delete right now under a candidate `idle_ttl`.
/// For OCI the rows are tags and `size` is absent, because layers shared with
/// kept images stay and the bytes actually freed are unknown up front.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RetentionPreview {
    pub count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,
    pub items: Vec<RetentionPreviewItem>,
}

/// One row of a [`RetentionPreview`], least recently active first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RetentionPreviewItem {
    pub path: String,
    pub version: String,
    pub last_active_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,
    /// Who uploaded, pushed or first cached it ("" when anonymous or unknown).
    pub uploaded_by: String,
}

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
                () = cancel.cancelled() => return,
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
            // OCI artifacts are reference-linked (a manifest needs its layer
            // rows), so per-row idle deletion can break a still-tagged image.
            // Idle tags are removed instead and the reachability prune reclaims
            // what they held.
            if repo.format == meta::FORMAT_OCI {
                if repo.r#type != meta::TYPE_GROUP {
                    total += self.reap_oci_tags(&repo, cutoff, ttl).await?;
                }
                continue;
            }
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
                            status: i64::from(StatusCode::OK.as_u16()),
                            detail_json: retention_detail(ttl, art.last_accessed_at, None),
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

impl Manager {
    /// Reports what [`Self::reap_once`] would delete from `repo` if its
    /// retention were `ttl`, without deleting anything.
    pub async fn preview_retention(
        &self,
        repo: &meta::Repository,
        ttl: Duration,
    ) -> Result<RetentionPreview, meta::Error> {
        if ttl.is_zero() || repo.r#type == meta::TYPE_GROUP {
            return Ok(RetentionPreview::default());
        }
        let cutoff =
            self.engine.now() - chrono::TimeDelta::from_std(ttl).unwrap_or(chrono::TimeDelta::MAX);
        if repo.format == meta::FORMAT_OCI {
            let tags = self.idle_oci_tags(repo.id, cutoff).await?;
            let mut items = Vec::new();
            for t in tags.iter().take(PREVIEW_ITEMS as usize) {
                let uploaded_by = self
                    .store
                    .get_artifact(repo.id, &oci_manifest_path(&t.name, &t.digest))
                    .await
                    .map(|art| art.cached_by)
                    .unwrap_or_default();
                items.push(RetentionPreviewItem {
                    path: oci_manifest_path(&t.name, &t.tag),
                    version: t.tag.clone(),
                    last_active_at: t.last_active,
                    size: None,
                    uploaded_by,
                });
            }
            return Ok(RetentionPreview {
                count: tags.len() as i64,
                size: None,
                items,
            });
        }
        let (count, size) = self.store.count_expired_artifacts(repo.id, cutoff).await?;
        let arts = self
            .store
            .list_expired_artifacts(repo.id, cutoff, PREVIEW_ITEMS)
            .await?;
        Ok(RetentionPreview {
            count,
            size: Some(size),
            items: arts
                .into_iter()
                .map(|a| RetentionPreviewItem {
                    path: a.path,
                    version: a.version,
                    last_active_at: a.last_accessed_at,
                    size: Some(a.size),
                    uploaded_by: a.cached_by,
                })
                .collect(),
        })
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

        let preview = tm
            .manager
            .preview_retention(&gated, std::time::Duration::from_hours(1))
            .await
            .expect("preview");
        assert_eq!(preview.count, 1, "preview counts the idle artifact");
        assert_eq!(preview.size, Some(4), "preview sums sizes");
        assert_eq!(preview.items[0].path, "old/-/old-0.1.0.tgz");
        let wider = tm
            .manager
            .preview_retention(&gated, std::time::Duration::from_mins(1))
            .await
            .expect("preview");
        assert_eq!(wider.count, 1, "fresh artifact served now stays out");
        let off_preview = tm
            .manager
            .preview_retention(&off, std::time::Duration::ZERO)
            .await
            .expect("preview");
        assert_eq!(off_preview, super::RetentionPreview::default());

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
        let detail: serde_json::Value =
            serde_json::from_str(&logs[0].detail_json).expect("detail is JSON");
        assert_eq!(detail["reason"], "idle_retention");
        assert_eq!(detail["idle_ttl"], "1h0m0s");
        assert!(detail["last_active_at"].is_string(), "{detail}");
    }
}
