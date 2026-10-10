//! Artifact rows and their blob reference bookkeeping: upsert, lookup, listing, LRU eviction,
//! purge, forced delete and the scan/stat aggregates.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::publication::invalidate_group_caches_for_member;
use super::repository::query_all;
use super::time::is_zero;
use super::{
    Artifact, Error, Result, Store, format_time, format_time_opt, now_rfc3339, parse_time,
    parse_time_opt,
};

/// Reports what a forced delete removed, so the caller can audit it and tell
/// the operator whether a coordinate is now free to be published again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForceDeleteResult {
    pub blob_sha256: String,
    /// Set when the artifact was the last asset of a managed publication and
    /// that publication row was removed with it.
    pub publication_id: String,
    /// The removed publication's coordinate, for the audit trail.
    pub coordinate: String,
}

/// Per-repository artifact aggregates for list views.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RepoStats {
    pub artifact_count: i64,
    pub total_size: i64,
}

/// A stored artifact's repository format with its path and version, enough to
/// derive an OSV scan coordinate for backfill scanning.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanTarget {
    pub format: String,
    pub path: String,
    pub version: String,
}

/// A stored artifact's repository id and format with its path and version,
/// enough to derive an OSV coordinate and attribute the scan to a specific
/// repository for per-repo aggregates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoScanTarget {
    pub repo_id: i64,
    pub format: String,
    pub path: String,
    pub version: String,
}

impl Store {
    /// Upserts an artifact at (`repo_id`, `path`), maintaining blob reference
    /// counts. The blob bytes must already be in the blob store. `cached_at` /
    /// `last_accessed_at` / `updated_at` are set to now when zero.
    pub async fn put_artifact(&self, mut a: Artifact) -> Result<Artifact> {
        let repo_id = a.repo_id;
        let path = a.path.clone();
        self.write(move |conn| {
            let now = now_rfc3339();
            if a.content_type.is_empty() {
                a.content_type = "application/octet-stream".to_string();
            }
            if a.metadata_json.is_empty() {
                a.metadata_json = "{}".to_string();
            }
            // Callers may stamp cache times explicitly (engine uses its own clock
            // for testable freshness); otherwise default to now.
            let cached = if is_zero(a.cached_at) { now.clone() } else { format_time(a.cached_at) };
            let accessed = if is_zero(a.last_accessed_at) {
                now.clone()
            } else {
                format_time(a.last_accessed_at)
            };

            let tx = conn
                .transaction()
                .map_err(|e| Error::sqlite("begin put artifact", e))?;

            // Ensure the blob record exists (ref_count starts at 0, adjusted
            // below). A row born at ref_count 0 is stamped as unreferenced right
            // away: if no reference ever arrives (an abandoned upload) the sweeper
            // still needs a timestamp to measure the grace period from.
            tx.execute(
                "INSERT INTO blobs(sha256, size, ref_count, created_at, unreferenced_since) VALUES(?, ?, 0, ?, ?)
         ON CONFLICT(sha256) DO NOTHING",
                params![a.blob_sha256, a.size, now, now],
            )
            .map_err(|e| Error::sqlite("ensure blob", e))?;

            // Find the blob currently referenced at this path, if any.
            let old: Option<(String, Option<String>, String)> = tx
                .query_row(
                    "SELECT blob_sha256, publication_id, metadata_json FROM artifacts WHERE repo_id = ? AND path = ?",
                    params![a.repo_id, a.path],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(|e| Error::sqlite("lookup artifact", e))?;
            if let Some((_, old_publication, old_metadata)) = &old
                && a.publication_id.is_empty()
                && (old_publication.as_deref().is_some_and(|p| !p.is_empty()) || is_ui_managed_metadata(old_metadata))
            {
                return Err(Error::ManagedArtifact);
            }
            if a.artifact_role.is_empty() {
                a.artifact_role = "primary".to_string();
            }

            tx.execute(
                "INSERT INTO artifacts(repo_id, path, version, blob_sha256, size, content_type, metadata_json, published_at, cached_at, last_accessed_at, updated_at, cached_by, publication_id, artifact_role)
		 VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULLIF(?, ''), ?)
		 ON CONFLICT(repo_id, path) DO UPDATE SET
             version = excluded.version,
             blob_sha256 = excluded.blob_sha256,
             size = excluded.size,
             content_type = excluded.content_type,
             metadata_json = excluded.metadata_json,
             published_at = excluded.published_at,
             cached_at = excluded.cached_at,
             last_accessed_at = excluded.last_accessed_at,
             updated_at = excluded.updated_at,
			 cached_by = excluded.cached_by,
			 publication_id = excluded.publication_id,
			 artifact_role = excluded.artifact_role",
                params![
                    a.repo_id,
                    a.path,
                    a.version,
                    a.blob_sha256,
                    a.size,
                    a.content_type,
                    a.metadata_json,
                    format_time_opt(a.published_at),
                    cached,
                    accessed,
                    now,
                    a.cached_by,
                    a.publication_id,
                    a.artifact_role
                ],
            )
            .map_err(|e| Error::sqlite("upsert artifact", e))?;

            match &old {
                None => adjust_ref(&tx, &a.blob_sha256, 1)?,
                Some((old_blob, _, _)) if *old_blob != a.blob_sha256 => {
                    adjust_ref(&tx, &a.blob_sha256, 1)?;
                    adjust_ref(&tx, old_blob, -1)?;
                }
                Some(_) => {}
            }
            invalidate_group_caches_for_member(&tx, a.repo_id, &a.path)?;

            tx.commit().map_err(|e| Error::sqlite("commit put artifact", e))
        })
        .await?;
        self.get_artifact(repo_id, &path).await
    }

    /// Records a blob that exists in the blob store but is not (yet) referenced
    /// by any artifact, so the sweeper can reclaim it if no reference ever
    /// appears. Streaming upload handlers use this to hand an abandoned upload
    /// (blob stored, but the request later failed validation) to the sweeper. A
    /// blob already recorded keeps its current reference count.
    pub async fn ensure_blob(&self, sha256: &str, size: i64) -> Result<()> {
        let sha256 = sha256.to_string();
        self.write(move |conn| {
            let now = now_rfc3339();
            conn.execute(
                "INSERT INTO blobs(sha256, size, ref_count, created_at, unreferenced_since) VALUES(?, ?, 0, ?, ?)
         ON CONFLICT(sha256) DO NOTHING",
                params![sha256, size, now, now],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("ensure blob", e))
        })
        .await
    }

    /// Returns the artifact at (`repo_id`, `path`).
    pub async fn get_artifact(&self, repo_id: i64, path: &str) -> Result<Artifact> {
        let path = path.to_string();
        self.read(move |conn| {
            conn.query_row(
                "SELECT id, repo_id, path, version, blob_sha256, size, content_type, metadata_json, published_at, cached_at, last_accessed_at, updated_at, cached_by, last_accessed_by, COALESCE(publication_id, ''), artifact_role
         FROM artifacts WHERE repo_id = ? AND path = ?",
                params![repo_id, path],
                scan_artifact,
            )
            .map_err(|e| Error::sqlite("get artifact", e))
        })
        .await
    }

    /// Returns artifacts in a repository whose path begins with `prefix`.
    pub async fn list_artifacts(&self, repo_id: i64, prefix: &str) -> Result<Vec<Artifact>> {
        let like = format!("{prefix}%");
        self.read(move |conn| {
            query_all(
                conn,
                "list artifacts",
                "SELECT id, repo_id, path, version, blob_sha256, size, content_type, metadata_json, published_at, cached_at, last_accessed_at, updated_at, cached_by, last_accessed_by, COALESCE(publication_id, ''), artifact_role
         FROM artifacts WHERE repo_id = ? AND path LIKE ? ORDER BY path",
                params![repo_id, like],
                scan_artifact,
            )
        })
        .await
    }

    /// Returns a repository's artifacts whose path begins with `prefix`,
    /// most-recently-accessed first, capped by `limit` (500 when not positive).
    /// It powers the artifact browser in the UI.
    pub async fn list_repo_artifacts(
        &self,
        repo_id: i64,
        prefix: &str,
        limit: i64,
    ) -> Result<Vec<Artifact>> {
        let limit = if limit <= 0 { 500 } else { limit };
        let like = format!("{prefix}%");
        self.read(move |conn| {
            query_all(
                conn,
                "list repo artifacts",
                "SELECT id, repo_id, path, version, blob_sha256, size, content_type, metadata_json, published_at, cached_at, last_accessed_at, updated_at, cached_by, last_accessed_by, COALESCE(publication_id, ''), artifact_role
         FROM artifacts WHERE repo_id = ? AND path LIKE ? ORDER BY last_accessed_at DESC LIMIT ?",
                params![repo_id, like, limit],
                scan_artifact,
            )
        })
        .await
    }

    /// Returns the total artifact rows across every repository, for the public
    /// landing statistics.
    pub async fn count_all_artifacts(&self) -> Result<i64> {
        self.read(|conn| {
            conn.query_row("SELECT COUNT(*) FROM artifacts", [], |r| r.get(0))
                .map_err(|e| Error::sqlite("count all artifacts", e))
        })
        .await
    }

    /// Returns the number of artifacts in a repository.
    pub async fn count_artifacts(&self, repo_id: i64) -> Result<i64> {
        self.read(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM artifacts WHERE repo_id = ?",
                params![repo_id],
                |r| r.get(0),
            )
            .map_err(|e| Error::sqlite("count artifacts", e))
        })
        .await
    }

    /// Updates an artifact's `last_accessed_at` to now (for LRU eviction),
    /// recording `username` as the last reader unless it is empty.
    pub async fn touch(&self, repo_id: i64, path: &str, username: &str) -> Result<()> {
        let path = path.to_string();
        let username = username.to_string();
        self.write(move |conn| {
            conn.execute(
                "UPDATE artifacts SET last_accessed_at = ?,
             last_accessed_by = CASE WHEN ? = '' THEN last_accessed_by ELSE ? END
         WHERE repo_id = ? AND path = ?",
                params![now_rfc3339(), username, username, repo_id, path],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("touch artifact", e))
        })
        .await
    }

    /// Removes an artifact and decrements its blob reference count.
    pub async fn delete_artifact(&self, repo_id: i64, path: &str) -> Result<()> {
        let path = path.to_string();
        self.write(move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| Error::sqlite("begin delete artifact", e))?;

            let (blob, publication_id, metadata_json): (String, Option<String>, String) = tx
                .query_row(
                    "SELECT blob_sha256, publication_id, metadata_json FROM artifacts WHERE repo_id = ? AND path = ?",
                    params![repo_id, path],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .map_err(|e| Error::sqlite("lookup artifact", e))?;
            if publication_id.as_deref().is_some_and(|p| !p.is_empty()) || is_ui_managed_metadata(&metadata_json) {
                return Err(Error::ManagedArtifact);
            }
            tx.execute(
                "DELETE FROM artifacts WHERE repo_id = ? AND path = ?",
                params![repo_id, path],
            )
            .map_err(|e| Error::sqlite("delete artifact", e))?;
            adjust_ref(&tx, &blob, -1)?;
            invalidate_group_caches_for_member(&tx, repo_id, &path)?;
            tx.commit()
                .map_err(|e| Error::sqlite("commit delete artifact", e))
        })
        .await
    }

    /// Removes an artifact even when it is owned by a managed publication,
    /// which [`Store::delete_artifact`] refuses.
    ///
    /// It exists for one situation: the artifact's bytes are gone from the
    /// blob store, so it cannot be served and the ordinary lifecycle operations
    /// cannot run either (deleting a publication has to read the derived
    /// metadata first, which is exactly what is unreadable). Without this the
    /// repository is stuck with an artifact that can be neither served nor
    /// removed. The caller is responsible for establishing that the bytes are
    /// really absent; this method only performs the removal.
    ///
    /// When the artifact was the last asset of its publication, the publication
    /// row is removed too, and deliberately WITHOUT a tombstone: the point of
    /// the operation is to let the same version be published again.
    pub async fn force_delete_artifact(
        &self,
        repo_id: i64,
        path: &str,
    ) -> Result<ForceDeleteResult> {
        let path = path.to_string();
        self.write(move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| Error::sqlite("begin force delete artifact", e))?;

            let (blob, publication_id): (String, Option<String>) = tx
                .query_row(
                    "SELECT blob_sha256, publication_id FROM artifacts WHERE repo_id = ? AND path = ?",
                    params![repo_id, path],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(|e| Error::sqlite("lookup artifact", e))?;
            tx.execute(
                "DELETE FROM artifacts WHERE repo_id = ? AND path = ?",
                params![repo_id, path],
            )
            .map_err(|e| Error::sqlite("force delete artifact", e))?;
            adjust_ref(&tx, &blob, -1)?;
            let mut out = ForceDeleteResult {
                blob_sha256: blob,
                ..ForceDeleteResult::default()
            };
            if let Some(publication_id) = publication_id.filter(|p| !p.is_empty()) {
                let remaining: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM artifacts WHERE publication_id = ?",
                        params![publication_id],
                        |r| r.get(0),
                    )
                    .map_err(|e| Error::sqlite("count publication artifacts", e))?;
                if remaining == 0 {
                    let coordinate: Option<String> = tx
                        .query_row(
                            "SELECT coordinate FROM artifact_publications WHERE id = ?",
                            params![publication_id],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(|e| Error::sqlite("lookup publication coordinate", e))?;
                    tx.execute(
                        "DELETE FROM artifact_publications WHERE id = ?",
                        params![publication_id],
                    )
                    .map_err(|e| Error::sqlite("force delete publication", e))?;
                    out.publication_id = publication_id;
                    out.coordinate = coordinate.unwrap_or_default();
                }
            }
            invalidate_group_caches_for_member(&tx, repo_id, &path)?;
            tx.commit()
                .map_err(|e| Error::sqlite("commit force delete artifact", e))?;
            Ok(out)
        })
        .await
    }

    /// Removes every artifact in a repository in one transaction, decrementing
    /// each referenced blob's count, and returns the number of rows deleted.
    /// Unreferenced blobs are reclaimed separately by the sweeper. Used by the
    /// repository's "purge all artifacts" admin action.
    pub async fn purge_artifacts(&self, repo_id: i64) -> Result<i64> {
        self.write(move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| Error::sqlite("begin purge artifacts", e))?;
            let managed: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM artifacts WHERE repo_id = ? AND (publication_id IS NOT NULL OR metadata_json LIKE '%\"managed_by\":\"ui_upload%')",
                    params![repo_id],
                    |r| r.get(0),
                )
                .map_err(|e| Error::sqlite("purge inspect managed artifacts", e))?;
            if managed > 0 {
                return Err(Error::ManagedArtifact);
            }

            // Drop one reference for every artifact about to be deleted. Two
            // artifacts in this repo can point at the same blob, so subtract the
            // per-blob row count rather than a flat 1 (which would leak references
            // and pin the blob).
            tx.execute(
                "UPDATE blobs SET ref_count = ref_count - (
             SELECT COUNT(*) FROM artifacts
             WHERE artifacts.repo_id = ? AND artifacts.blob_sha256 = blobs.sha256),
             unreferenced_since = CASE WHEN ref_count - (
                 SELECT COUNT(*) FROM artifacts
                 WHERE artifacts.repo_id = ? AND artifacts.blob_sha256 = blobs.sha256) <= 0
                 THEN COALESCE(unreferenced_since, ?)
                 ELSE unreferenced_since END
         WHERE sha256 IN (SELECT blob_sha256 FROM artifacts WHERE repo_id = ?)",
                params![repo_id, repo_id, now_rfc3339(), repo_id],
            )
            .map_err(|e| Error::sqlite("purge adjust refs", e))?;
            let n = tx
                .execute("DELETE FROM artifacts WHERE repo_id = ?", params![repo_id])
                .map_err(|e| Error::sqlite("purge artifacts", e))?;
            tx.commit()
                .map_err(|e| Error::sqlite("commit purge artifacts", e))?;
            Ok(n as i64)
        })
        .await
    }

    /// Returns artifacts in a repository last accessed (served) before
    /// `cutoff`, oldest first, capped by `limit` (256 when not positive). Path
    /// and version are populated so the idle-retention reaper can audit exactly
    /// what it removes. The cutoff is compared against the RFC3339Nano UTC text
    /// in `last_accessed_at`, whose lexicographic order matches chronological
    /// order.
    pub async fn list_expired_artifacts(
        &self,
        repo_id: i64,
        cutoff: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<Artifact>> {
        let limit = if limit <= 0 { 256 } else { limit };
        let cutoff = format_time(cutoff);
        self.read(move |conn| {
            query_all(
                conn,
                "list expired artifacts",
                "SELECT id, repo_id, path, version, blob_sha256, size, content_type, metadata_json, published_at, cached_at, last_accessed_at, updated_at, cached_by, last_accessed_by, COALESCE(publication_id, ''), artifact_role
         FROM artifacts WHERE repo_id = ? AND last_accessed_at < ? ORDER BY last_accessed_at ASC LIMIT ?",
                params![repo_id, cutoff, limit],
                scan_artifact,
            )
        })
        .await
    }

    /// Removes up to `limit` least-recently-accessed artifacts in a repository
    /// and returns how many paths were freed. Blob ref counts are decremented;
    /// unreferenced blobs are reclaimed separately via
    /// [`Store::list_unreferenced_blobs`].
    pub async fn evict_lru(&self, repo_id: i64, limit: i64) -> Result<i64> {
        let paths: Vec<String> = self
            .read(move |conn| {
                query_all(
                    conn,
                    "list lru artifacts",
                    "SELECT path FROM artifacts WHERE repo_id = ? ORDER BY last_accessed_at ASC LIMIT ?",
                    params![repo_id, limit],
                    |r| r.get(0),
                )
            })
            .await?;
        for p in &paths {
            match self.delete_artifact(repo_id, p).await {
                Ok(()) | Err(Error::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(paths.len() as i64)
    }

    /// Returns stored artifacts that carry a version, joined with their
    /// repository format, paged by artifact id (ascending). Used by the
    /// backfill worker to scan already-uploaded packages.
    pub async fn list_scan_targets(&self, limit: i64, offset: i64) -> Result<Vec<ScanTarget>> {
        self.read(move |conn| {
            query_all(
                conn,
                "list scan targets",
                "SELECT r.format, a.path, a.version
		   FROM artifacts a JOIN repositories r ON a.repo_id = r.id
		  WHERE a.version != ''
		  ORDER BY a.id LIMIT ? OFFSET ?",
                params![limit, offset],
                |r| {
                    Ok(ScanTarget {
                        format: r.get(0)?,
                        path: r.get(1)?,
                        version: r.get(2)?,
                    })
                },
            )
        })
        .await
    }

    /// Returns every stored artifact that carries a version, joined with its
    /// repository id and format, in one unpaged query. Used to compute
    /// per-repository scan aggregates for the list view.
    pub async fn all_scan_targets(&self) -> Result<Vec<RepoScanTarget>> {
        self.read(|conn| {
            query_all(
                conn,
                "all scan targets",
                "SELECT a.repo_id, r.format, a.path, a.version
		   FROM artifacts a JOIN repositories r ON a.repo_id = r.id
		  WHERE a.version != ''",
                [],
                |r| {
                    Ok(RepoScanTarget {
                        repo_id: r.get(0)?,
                        format: r.get(1)?,
                        path: r.get(2)?,
                        version: r.get(3)?,
                    })
                },
            )
        })
        .await
    }

    /// Returns artifact count and total size for every repository that has
    /// artifacts, in one query (repositories without artifacts are absent).
    pub async fn all_repo_stats(&self) -> Result<HashMap<i64, RepoStats>> {
        self.read(|conn| {
            let rows = query_all(
                conn,
                "all repo stats",
                "SELECT repo_id, COUNT(*), COALESCE(SUM(size), 0) FROM artifacts GROUP BY repo_id",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        RepoStats {
                            artifact_count: r.get(1)?,
                            total_size: r.get(2)?,
                        },
                    ))
                },
            )?;
            Ok(rows.into_iter().collect())
        })
        .await
    }

    /// Returns the total size of artifacts stored in a repository.
    pub async fn repo_size(&self, repo_id: i64) -> Result<i64> {
        self.read(move |conn| {
            conn.query_row(
                "SELECT SUM(size) FROM artifacts WHERE repo_id = ?",
                params![repo_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .map(|s| s.unwrap_or(0))
            .map_err(|e| Error::sqlite("repo size", e))
        })
        .await
    }
}

/// Moves a blob's reference count and maintains `unreferenced_since` alongside
/// it: the timestamp is stamped when the count first reaches zero (kept as-is
/// on repeated zero writes, so the grace period is measured from when the blob
/// actually became unreferenced) and cleared the moment it is referenced again.
/// The sweeper reads it to honour the GC grace period.
pub(crate) const ADJUST_REF_SQL: &str = "UPDATE blobs
   SET ref_count = ref_count + ?,
       unreferenced_since = CASE WHEN ref_count + ? <= 0
                                 THEN COALESCE(unreferenced_since, ?)
                                 ELSE NULL END
 WHERE sha256 = ?";

/// Applies [`ADJUST_REF_SQL`] for `sha` without checking that a row matched;
/// callers here have just ensured the blob row exists.
pub(crate) fn adjust_ref(conn: &Connection, sha: &str, delta: i64) -> Result<()> {
    conn.execute(ADJUST_REF_SQL, params![delta, delta, now_rfc3339(), sha])
        .map(|_| ())
        .map_err(|e| Error::sqlite("adjust blob ref", e))
}

/// Reports whether stored metadata marks a path as owned by the UI upload
/// flow, either as a publication asset or as a shared aggregate index.
pub(crate) fn is_ui_managed_metadata(value: &str) -> bool {
    let managed_by = managed_by_metadata(value);
    managed_by == "ui_upload" || managed_by == "ui_upload_aggregate"
}

/// Identifies shared indexes that may only be mutated by the component-aware
/// publication planner.
pub fn is_ui_managed_aggregate_metadata(value: &str) -> bool {
    managed_by_metadata(value) == "ui_upload_aggregate"
}

fn managed_by_metadata(value: &str) -> String {
    serde_json::from_str::<serde_json::Value>(value)
        .ok()
        .and_then(|v| v.get("managed_by")?.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Decodes one row in the column order every artifact SELECT uses (see
/// `search::ARTIFACT_COLS`).
pub(crate) fn scan_artifact(row: &Row<'_>) -> rusqlite::Result<Artifact> {
    let published: Option<String> = row.get(8)?;
    let cached: String = row.get(9)?;
    let accessed: String = row.get(10)?;
    let updated: String = row.get(11)?;
    Ok(Artifact {
        id: row.get(0)?,
        repo_id: row.get(1)?,
        path: row.get(2)?,
        version: row.get(3)?,
        blob_sha256: row.get(4)?,
        size: row.get(5)?,
        content_type: row.get(6)?,
        metadata_json: row.get(7)?,
        published_at: parse_time_opt(published.as_deref()),
        cached_at: parse_time(&cached),
        last_accessed_at: parse_time(&accessed),
        updated_at: parse_time(&updated),
        cached_by: row.get(12)?,
        last_accessed_by: row.get(13)?,
        publication_id: row.get(14)?,
        artifact_role: row.get(15)?,
    })
}
