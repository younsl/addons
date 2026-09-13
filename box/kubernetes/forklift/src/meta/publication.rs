//! Component-aware publications: durable upload requests, staged-blob leases, the atomic
//! artifact batch commit and the delete/yank lifecycle, plus the group-cache invalidation
//! helpers every other table shares.

use std::collections::HashSet;

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use super::artifact::{ADJUST_REF_SQL, scan_artifact};
use super::receiver::is_unique_violation;
use super::repository::query_all;
use super::time::is_zero;
use super::{
    Artifact, ArtifactPublication, ArtifactPublicationTombstone, ArtifactUploadRequest, Error,
    Result, Store, UPLOAD_COMMITTED, UPLOAD_CONFLICT, UPLOAD_FAILED, UPLOAD_RECEIVING, format_time,
    format_time_opt, now_rfc3339, parse_time,
};

/// The complete idempotency scope of one upload request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct UploadRequestKey {
    pub repo_id: i64,
    pub principal_source: String,
    pub principal_name: String,
    pub idempotency_key: String,
}

/// Replaces or creates a shared mutable path only when the digest observed by
/// the planner is still current. An empty `expected_sha256` means absent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtifactCAS {
    pub artifact: Artifact,
    pub expected_sha256: String,
}

/// The complete metadata mutation for one publication.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtifactBatch {
    pub expected_upload_state: String,
    pub publication: ArtifactPublication,
    pub create: Vec<Artifact>,
    pub replace: Vec<Artifact>,
    pub remove_paths: Vec<String>,
    pub tombstones: Vec<ArtifactPublicationTombstone>,
    pub mutable_cas: Vec<ArtifactCAS>,
    pub invalidate_group_cache: Vec<GroupMetadataCacheKey>,
    pub upload_result_json: String,
}

/// Applies deletion/yank mutations without creating a synthetic upload
/// request. Every owned and aggregate precondition is checked in the same
/// immediate transaction as blob reference updates.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PublicationLifecycleBatch {
    pub publication: ArtifactPublication,
    pub remove_paths: Vec<String>,
    pub tombstones: Vec<ArtifactPublicationTombstone>,
    pub mutable_cas: Vec<ArtifactCAS>,
    pub mutable_remove_cas: Vec<ArtifactCAS>,
    pub invalidate_group_cache: Vec<GroupMetadataCacheKey>,
    pub delete_publication: bool,
    pub set_yanked: Option<bool>,
}

/// Identifies one cached representation to invalidate. An empty
/// `representation` addresses every representation of the path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupMetadataCacheKey {
    pub group_repo_id: i64,
    pub path: String,
    pub representation: String,
}

const UPLOAD_REQUEST_COLS: &str =
    "idempotency_key, repo_id, principal_name, principal_source, upload_id,
		 state, plan_json, result_json, created_at, updated_at, expires_at";

const PUBLICATION_COLS: &str =
    "p.id, p.repo_id, p.format, p.package_name, p.version, p.coordinate, p.upload_id,
		 p.created_by, p.created_by_source, p.yanked, p.created_at, p.updated_at,
		 COUNT(a.id), COALESCE(SUM(a.size), 0)";

impl Store {
    /// Reserves an idempotency key before request bytes are read. Returns
    /// [`Error::Conflict`] when the scope is already taken.
    pub async fn create_upload_request(&self, mut r: ArtifactUploadRequest) -> Result<()> {
        self.write(move |conn| {
            let now = Utc::now();
            if is_zero(r.created_at) {
                r.created_at = now;
            }
            if is_zero(r.updated_at) {
                r.updated_at = r.created_at;
            }
            if r.state.is_empty() {
                r.state = UPLOAD_RECEIVING.to_string();
            }
            conn.execute(
                "INSERT INTO artifact_upload_requests(
		 idempotency_key, repo_id, principal_name, principal_source, upload_id, state,
		 plan_json, result_json, created_at, updated_at, expires_at)
		 VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    r.idempotency_key,
                    r.repo_id,
                    r.principal_name,
                    r.principal_source,
                    r.upload_id,
                    r.state,
                    r.plan_json,
                    r.result_json,
                    format_time(r.created_at),
                    format_time(r.updated_at),
                    format_time(r.expires_at)
                ],
            )
            .map(|_| ())
            .map_err(|e| {
                if is_unique_violation(&e) {
                    Error::Conflict
                } else {
                    Error::sqlite("create upload request", e)
                }
            })
        })
        .await
    }

    /// Returns a request by its full idempotency scope.
    pub async fn get_upload_request(&self, key: UploadRequestKey) -> Result<ArtifactUploadRequest> {
        self.read(move |conn| {
            conn.query_row(
                &format!(
                    "SELECT {UPLOAD_REQUEST_COLS}
		 FROM artifact_upload_requests
		 WHERE repo_id = ? AND principal_source = ? AND principal_name = ? AND idempotency_key = ?"
                ),
                params![
                    key.repo_id,
                    key.principal_source,
                    key.principal_name,
                    key.idempotency_key
                ],
                scan_upload_request,
            )
            .map_err(|e| Error::sqlite("get upload request", e))
        })
        .await
    }

    /// Resolves confirmation/status routes without weakening the
    /// principal/repository checks performed by their callers.
    pub async fn get_upload_request_by_id(&self, upload_id: &str) -> Result<ArtifactUploadRequest> {
        let upload_id = upload_id.to_string();
        self.read(move |conn| {
            conn.query_row(
                &format!(
                    "SELECT {UPLOAD_REQUEST_COLS}
		 FROM artifact_upload_requests WHERE upload_id = ?"
                ),
                params![upload_id],
                scan_upload_request,
            )
            .map_err(|e| Error::sqlite("get upload request by id", e))
        })
        .await
    }

    /// Returns one component-aware package version.
    pub async fn get_artifact_publication(&self, id: &str) -> Result<ArtifactPublication> {
        let id = id.to_string();
        self.read(move |conn| {
            conn.query_row(
                &format!(
                    "SELECT {PUBLICATION_COLS}
		 FROM artifact_publications p LEFT JOIN artifacts a ON a.publication_id = p.id
		 WHERE p.id = ? GROUP BY p.id"
                ),
                params![id],
                scan_publication,
            )
            .map_err(|e| Error::sqlite("get artifact publication", e))
        })
        .await
    }

    /// Returns the managed component for one ecosystem package/version
    /// identity.
    pub async fn get_artifact_publication_by_identity(
        &self,
        repo_id: i64,
        format: &str,
        package_name: &str,
        version: &str,
    ) -> Result<ArtifactPublication> {
        let (format, package_name, version) = (
            format.to_string(),
            package_name.to_string(),
            version.to_string(),
        );
        self.read(move |conn| {
            conn.query_row(
                &format!(
                    "SELECT {PUBLICATION_COLS}
		 FROM artifact_publications p LEFT JOIN artifacts a ON a.publication_id = p.id
		 WHERE p.repo_id = ? AND p.format = ? AND p.package_name = ? AND p.version = ?
		 GROUP BY p.id"
                ),
                params![repo_id, format, package_name, version],
                scan_publication,
            )
            .map_err(|e| Error::sqlite("get artifact publication by identity", e))
        })
        .await
    }

    /// Reports whether an immutable coordinate or asset key was previously
    /// removed and therefore may not be rebound to new bytes.
    pub async fn has_publication_tombstone(
        &self,
        repo_id: i64,
        format: &str,
        package_name: &str,
        version: &str,
        asset_key: &str,
    ) -> Result<bool> {
        let (format, package_name, version, asset_key) = (
            format.to_string(),
            package_name.to_string(),
            version.to_string(),
            asset_key.to_string(),
        );
        self.read(move |conn| {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM artifact_publication_tombstones
		 WHERE repo_id = ? AND format = ? AND package_name = ? AND version = ?
		 AND (asset_key = ? OR asset_key = '*')",
                    params![repo_id, format, package_name, version, asset_key],
                    |r| r.get(0),
                )
                .map_err(|e| Error::sqlite("has publication tombstone", e))?;
            Ok(count > 0)
        })
        .await
    }

    /// Returns managed package versions in display order.
    pub async fn list_artifact_publications(
        &self,
        repo_id: i64,
    ) -> Result<Vec<ArtifactPublication>> {
        self.read(move |conn| {
            query_all(
                conn,
                "list artifact publications",
                &format!(
                    "SELECT {PUBLICATION_COLS}
		 FROM artifact_publications p LEFT JOIN artifacts a ON a.publication_id = p.id
		 WHERE p.repo_id = ? GROUP BY p.id ORDER BY p.updated_at DESC, p.id DESC"
                ),
                params![repo_id],
                scan_publication,
            )
        })
        .await
    }

    /// Returns every immutable path owned by a managed publication. Shared
    /// aggregate indexes are intentionally excluded.
    pub async fn list_publication_artifacts(&self, publication_id: &str) -> Result<Vec<Artifact>> {
        let publication_id = publication_id.to_string();
        self.read(move |conn| {
            query_all(
                conn,
                "list publication artifacts",
                "SELECT id, repo_id, path, version, blob_sha256, size, content_type, metadata_json,
		 published_at, cached_at, last_accessed_at, updated_at, cached_by, last_accessed_by,
		 COALESCE(publication_id, ''), artifact_role
		 FROM artifacts WHERE publication_id = ? ORDER BY path",
                params![publication_id],
                scan_artifact,
            )
        })
        .await
    }

    /// Records an unreferenced staged blob so sweepers preserve it while an
    /// actionable conflict plan is live.
    pub async fn lease_upload_blob(&self, upload_id: &str, sha256: &str, size: i64) -> Result<()> {
        let (upload_id, sha256) = (upload_id.to_string(), sha256.to_string());
        self.write(move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| Error::sqlite("begin lease upload blob", e))?;
            // A staged blob starts unreferenced, so it is stamped for the GC grace
            // period immediately; the reference the publication takes later clears
            // the stamp.
            let staged_at = now_rfc3339();
            tx.execute(
                "INSERT INTO blobs(sha256, size, ref_count, created_at, unreferenced_since) VALUES(?, ?, 0, ?, ?)
			 ON CONFLICT(sha256) DO NOTHING",
                params![sha256, size, staged_at, staged_at],
            )
            .map_err(|e| Error::sqlite("ensure staged blob", e))?;
            tx.execute(
                "INSERT INTO artifact_upload_staged_blobs(upload_id, sha256, size) VALUES(?, ?, ?)
			 ON CONFLICT(upload_id, sha256) DO UPDATE SET size = excluded.size",
                params![upload_id, sha256, size],
            )
            .map_err(|e| Error::sqlite("lease staged blob", e))?;
            tx.commit()
                .map_err(|e| Error::sqlite("commit lease upload blob", e))
        })
        .await
    }

    /// Persists a non-commit terminal/conflict state with an expected state
    /// guard. Committed transitions are only legal through
    /// [`Store::apply_artifact_batch`].
    pub async fn set_upload_state(
        &self,
        upload_id: &str,
        expected: &str,
        next: &str,
        plan_json: &str,
        expires_at: chrono::DateTime<Utc>,
    ) -> Result<()> {
        if next == UPLOAD_COMMITTED {
            return Err(Error::Other(
                "committed state requires ApplyArtifactBatch".to_string(),
            ));
        }
        let (upload_id, expected, next, plan_json) = (
            upload_id.to_string(),
            expected.to_string(),
            next.to_string(),
            plan_json.to_string(),
        );
        self.write(move |conn| {
            let n = conn
                .execute(
                    "UPDATE artifact_upload_requests
		 SET state = ?, plan_json = ?, updated_at = ?, expires_at = ?
		 WHERE upload_id = ? AND state = ?",
                    params![
                        next,
                        plan_json,
                        now_rfc3339(),
                        format_time(expires_at),
                        upload_id,
                        expected
                    ],
                )
                .map_err(|e| Error::sqlite("set upload state", e))?;
            if n != 1 {
                return Err(Error::UploadState);
            }
            Ok(())
        })
        .await
    }

    /// Releases staged-blob leases and makes a retained plan terminal without
    /// deleting its idempotency/audit record.
    pub async fn cancel_upload_conflict(&self, upload_id: &str) -> Result<()> {
        let upload_id = upload_id.to_string();
        self.write(move |conn| {
            with_immediate_tx(conn, |q| {
                q.execute(
                    "DELETE FROM artifact_upload_staged_blobs WHERE upload_id = ?",
                    params![upload_id],
                )
                .map_err(|e| Error::sqlite("release staged blobs", e))?;
                let count = q
                    .execute(
                        "UPDATE artifact_upload_requests SET state = ?, plan_json = '', updated_at = ? WHERE upload_id = ? AND state = ?",
                        params![UPLOAD_FAILED, now_rfc3339(), upload_id, UPLOAD_CONFLICT],
                    )
                    .map_err(|e| Error::sqlite("cancel upload conflict", e))?;
                if count != 1 {
                    return Err(Error::UploadState);
                }
                Ok(())
            })
        })
        .await
    }

    /// Atomically commits publication ownership, artifact paths, blob
    /// references, tombstones, aggregate cache invalidation and idempotency.
    pub async fn apply_artifact_batch(
        &self,
        key: UploadRequestKey,
        mut batch: ArtifactBatch,
    ) -> Result<()> {
        if batch.expected_upload_state.is_empty() {
            batch.expected_upload_state = UPLOAD_RECEIVING.to_string();
        }
        if batch.upload_result_json.is_empty() {
            batch.upload_result_json = "{}".to_string();
        }
        validate_artifact_batch(&key, &batch)?;
        self.write(move |conn| {
            with_immediate_tx(conn, |q| {
                let (upload_id, state): (String, String) = q
                    .query_row(
                        "SELECT upload_id, state FROM artifact_upload_requests
				 WHERE repo_id = ? AND principal_source = ? AND principal_name = ? AND idempotency_key = ?",
                        params![
                            key.repo_id,
                            key.principal_source,
                            key.principal_name,
                            key.idempotency_key
                        ],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .map_err(|e| Error::sqlite("load upload request", e))?;
                if state != batch.expected_upload_state || upload_id != batch.publication.upload_id
                {
                    return Err(Error::UploadState);
                }
                if batch.publication.repo_id != key.repo_id || batch.publication.id.is_empty() {
                    return Err(Error::Other("invalid publication identity".to_string()));
                }

                check_immutable_preconditions(q, &batch)?;
                check_mutable_preconditions(q, &batch.mutable_cas)?;
                ensure_batch_blobs(
                    q,
                    batch
                        .create
                        .iter()
                        .chain(batch.replace.iter())
                        .chain(batch.mutable_cas.iter().map(|cas| &cas.artifact)),
                )?;
                upsert_publication(q, &batch.publication)?;
                for path in &batch.remove_paths {
                    delete_owned_artifact(q, key.repo_id, path, &batch.publication.id)?;
                }
                for artifact in &batch.create {
                    put_batch_artifact(q, artifact)?;
                }
                for artifact in &batch.replace {
                    put_batch_artifact(q, artifact)?;
                }
                for cas in &batch.mutable_cas {
                    put_batch_artifact(q, &cas.artifact)?;
                }
                for tombstone in &batch.tombstones {
                    insert_tombstone(q, tombstone)
                        .map_err(|e| Error::sqlite("insert publication tombstone", e))?;
                }
                for cache_key in &batch.invalidate_group_cache {
                    delete_group_cache(q, cache_key)?;
                }
                invalidate_group_caches_for_member(q, key.repo_id, "")?;
                let negative: i64 = q
                    .query_row("SELECT COUNT(*) FROM blobs WHERE ref_count < 0", [], |r| {
                        r.get(0)
                    })
                    .map_err(|e| Error::sqlite("count negative blob refs", e))?;
                if negative != 0 {
                    return Err(Error::Other("negative blob reference count".to_string()));
                }
                q.execute(
                    "DELETE FROM artifact_upload_staged_blobs WHERE upload_id = ?",
                    params![upload_id],
                )
                .map_err(|e| Error::sqlite("release staged blobs", e))?;
                let n = q
                    .execute(
                        "UPDATE artifact_upload_requests
				 SET state = ?, result_json = ?, plan_json = '', updated_at = ?
				 WHERE upload_id = ? AND state = ?",
                        params![
                            UPLOAD_COMMITTED,
                            batch.upload_result_json,
                            now_rfc3339(),
                            upload_id,
                            batch.expected_upload_state
                        ],
                    )
                    .map_err(|e| Error::sqlite("commit upload request", e))?;
                if n != 1 {
                    return Err(Error::UploadState);
                }
                Ok(())
            })
        })
        .await
    }

    /// Applies a delete/yank lifecycle batch: owned paths are removed only
    /// while still owned, aggregate indexes are compare-and-swapped, tombstones
    /// recorded, group caches invalidated, and the publication row deleted or
    /// its yanked flag updated, all in one immediate transaction.
    pub async fn apply_publication_lifecycle(
        &self,
        batch: PublicationLifecycleBatch,
    ) -> Result<()> {
        if batch.publication.id.is_empty() || batch.publication.repo_id == 0 {
            return Err(Error::Other(
                "invalid publication lifecycle batch".to_string(),
            ));
        }
        self.write(move |conn| {
            with_immediate_tx(conn, |q| {
                let repo_id: i64 = q
                    .query_row(
                        "SELECT repo_id FROM artifact_publications WHERE id = ?",
                        params![batch.publication.id],
                        |r| r.get(0),
                    )
                    .map_err(|e| Error::sqlite("load publication", e))?;
                if repo_id != batch.publication.repo_id {
                    return Err(Error::ArtifactConflict);
                }
                check_mutable_preconditions(q, &batch.mutable_cas)?;
                check_mutable_preconditions(q, &batch.mutable_remove_cas)?;
                ensure_batch_blobs(q, batch.mutable_cas.iter().map(|cas| &cas.artifact))?;
                for artifact_path in &batch.remove_paths {
                    delete_owned_artifact(q, repo_id, artifact_path, &batch.publication.id)?;
                }
                for cas in &batch.mutable_cas {
                    put_batch_artifact(q, &cas.artifact)?;
                }
                for cas in &batch.mutable_remove_cas {
                    delete_aggregate_artifact(q, repo_id, &cas.artifact.path)?;
                }
                for tombstone in &batch.tombstones {
                    insert_tombstone(q, tombstone)
                        .map_err(|e| Error::sqlite("insert publication tombstone", e))?;
                }
                for cache_key in &batch.invalidate_group_cache {
                    delete_group_cache(q, cache_key)?;
                }
                invalidate_group_caches_for_member(q, batch.publication.repo_id, "")?;
                if batch.delete_publication {
                    q.execute(
                        "DELETE FROM artifact_publications WHERE id = ?",
                        params![batch.publication.id],
                    )
                    .map_err(|e| Error::sqlite("delete publication", e))?;
                } else {
                    let yanked = batch.set_yanked.unwrap_or(batch.publication.yanked);
                    q.execute(
                        "UPDATE artifact_publications SET yanked = ?, updated_at = ? WHERE id = ?",
                        params![yanked, now_rfc3339(), batch.publication.id],
                    )
                    .map_err(|e| Error::sqlite("update publication", e))?;
                }
                Ok(())
            })
        })
        .await
    }
}

/// Rejects batches whose identity, ownership or path sets are inconsistent
/// before any statement runs.
fn validate_artifact_batch(key: &UploadRequestKey, batch: &ArtifactBatch) -> Result<()> {
    let publication = &batch.publication;
    if publication.id.is_empty()
        || publication.upload_id.is_empty()
        || publication.repo_id != key.repo_id
        || publication.format.is_empty()
        || publication.package_name.is_empty()
        || publication.version.is_empty()
        || publication.coordinate.is_empty()
        || serde_json::from_str::<serde::de::IgnoredAny>(&batch.upload_result_json).is_err()
    {
        return Err(Error::Other("invalid artifact batch".to_string()));
    }
    let mut seen: HashSet<&str> = HashSet::new();
    fn check_owned<'a>(
        seen: &mut HashSet<&'a str>,
        key: &UploadRequestKey,
        publication: &ArtifactPublication,
        artifact: &'a Artifact,
    ) -> Result<()> {
        if artifact.repo_id != key.repo_id
            || artifact.path.is_empty()
            || artifact.publication_id != publication.id
            || !seen.insert(artifact.path.as_str())
        {
            return Err(Error::Other(
                "invalid or duplicate publication artifact".to_string(),
            ));
        }
        Ok(())
    }
    for artifact in &batch.create {
        check_owned(&mut seen, key, publication, artifact)?;
    }
    for artifact in &batch.replace {
        check_owned(&mut seen, key, publication, artifact)?;
    }
    for path in &batch.remove_paths {
        if path.is_empty() || !seen.insert(path.as_str()) {
            return Err(Error::Other(
                "invalid or duplicate removed artifact".to_string(),
            ));
        }
    }
    for cas in &batch.mutable_cas {
        if cas.artifact.repo_id != key.repo_id
            || cas.artifact.path.is_empty()
            || !cas.artifact.publication_id.is_empty()
            || !seen.insert(cas.artifact.path.as_str())
        {
            return Err(Error::Other(
                "invalid or duplicate mutable artifact".to_string(),
            ));
        }
    }
    for tombstone in &batch.tombstones {
        if tombstone.repo_id != key.repo_id
            || tombstone.format.is_empty()
            || tombstone.package_name.is_empty()
            || tombstone.version.is_empty()
            || tombstone.asset_key.is_empty()
        {
            return Err(Error::Other("invalid tombstone".to_string()));
        }
    }
    Ok(())
}

/// Runs `f` inside a `BEGIN IMMEDIATE` transaction on the write connection,
/// committing on success and rolling back on any error. Taking the reserved
/// lock up front means the precondition reads inside `f` can never be
/// invalidated by another writer before the batch commits.
pub(crate) fn with_immediate_tx<T>(
    conn: &mut Connection,
    f: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| Error::sqlite("begin immediate", e))?;
    let out = f(&tx)?;
    tx.commit().map_err(|e| Error::sqlite("commit", e))?;
    Ok(out)
}

/// Created paths must be absent; replaced paths must already belong to this
/// publication.
fn check_immutable_preconditions(q: &Connection, batch: &ArtifactBatch) -> Result<()> {
    for artifact in &batch.create {
        let exists: Option<i64> = q
            .query_row(
                "SELECT 1 FROM artifacts WHERE repo_id = ? AND path = ?",
                params![artifact.repo_id, artifact.path],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| Error::sqlite("check created artifact", e))?;
        if exists.is_some() {
            return Err(Error::ArtifactConflict);
        }
    }
    for artifact in &batch.replace {
        let owner: Option<Option<String>> = q
            .query_row(
                "SELECT publication_id FROM artifacts WHERE repo_id = ? AND path = ?",
                params![artifact.repo_id, artifact.path],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| Error::sqlite("check replaced artifact", e))?;
        match owner {
            Some(Some(id)) if id == batch.publication.id => {}
            _ => return Err(Error::ArtifactConflict),
        }
    }
    Ok(())
}

/// Every compare-and-swap path must still hold the digest the planner saw (or
/// still be absent when it expected none).
fn check_mutable_preconditions(q: &Connection, values: &[ArtifactCAS]) -> Result<()> {
    for cas in values {
        let digest: Option<String> = q
            .query_row(
                "SELECT blob_sha256 FROM artifacts WHERE repo_id = ? AND path = ?",
                params![cas.artifact.repo_id, cas.artifact.path],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| Error::sqlite("check mutable artifact", e))?;
        match digest {
            None if cas.expected_sha256.is_empty() => {}
            Some(d) if d == cas.expected_sha256 => {}
            _ => return Err(Error::DerivedMetadataChanged),
        }
    }
    Ok(())
}

/// Makes sure every blob the batch will reference has a row whose recorded
/// size matches what the caller staged.
fn ensure_batch_blobs<'a>(
    q: &Connection,
    artifacts: impl IntoIterator<Item = &'a Artifact>,
) -> Result<()> {
    let blob_at = now_rfc3339();
    for artifact in artifacts {
        if artifact.blob_sha256.is_empty() || artifact.size < 0 {
            return Err(Error::Other("invalid staged blob".to_string()));
        }
        q.execute(
            "INSERT INTO blobs(sha256, size, ref_count, created_at, unreferenced_since) VALUES(?, ?, 0, ?, ?)
			 ON CONFLICT(sha256) DO NOTHING",
            params![artifact.blob_sha256, artifact.size, blob_at, blob_at],
        )
        .map_err(|e| Error::sqlite("ensure batch blob", e))?;
        let recorded_size: i64 = q
            .query_row(
                "SELECT size FROM blobs WHERE sha256 = ?",
                params![artifact.blob_sha256],
                |r| r.get(0),
            )
            .map_err(|e| Error::sqlite("read batch blob size", e))?;
        if recorded_size != artifact.size {
            return Err(Error::Other(format!(
                "blob size mismatch for {}",
                artifact.blob_sha256
            )));
        }
    }
    Ok(())
}

/// Inserts or refreshes the publication row; a uniqueness failure on the
/// package identity is [`Error::ArtifactConflict`].
fn upsert_publication(q: &Connection, publication: &ArtifactPublication) -> Result<()> {
    let now = Utc::now();
    let created_at = if is_zero(publication.created_at) {
        now
    } else {
        publication.created_at
    };
    let updated_at = if is_zero(publication.updated_at) {
        now
    } else {
        publication.updated_at
    };
    q.execute(
        "INSERT INTO artifact_publications(
		 id, repo_id, format, package_name, version, coordinate, upload_id,
		 created_by, created_by_source, yanked, created_at, updated_at)
		 VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
		 ON CONFLICT(id) DO UPDATE SET
		 coordinate = excluded.coordinate, upload_id = excluded.upload_id, yanked = excluded.yanked,
		 updated_at = excluded.updated_at",
        params![
            publication.id,
            publication.repo_id,
            publication.format,
            publication.package_name,
            publication.version,
            publication.coordinate,
            publication.upload_id,
            publication.created_by,
            publication.created_by_source,
            publication.yanked,
            format_time(created_at),
            format_time(updated_at)
        ],
    )
    .map(|_| ())
    .map_err(|e| {
        if is_unique_violation(&e) {
            Error::ArtifactConflict
        } else {
            Error::sqlite("upsert publication", e)
        }
    })
}

/// Upserts one batch artifact row, stamping all cache times to now, and moves
/// blob references the same way [`Store::put_artifact`] does but with the
/// strict row-count check of [`adjust_ref_runner`].
fn put_batch_artifact(q: &Connection, artifact: &Artifact) -> Result<()> {
    let now = now_rfc3339();
    let content_type = if artifact.content_type.is_empty() {
        "application/octet-stream"
    } else {
        artifact.content_type.as_str()
    };
    let metadata_json = if artifact.metadata_json.is_empty() {
        "{}"
    } else {
        artifact.metadata_json.as_str()
    };
    let artifact_role = if artifact.artifact_role.is_empty() {
        "primary"
    } else {
        artifact.artifact_role.as_str()
    };
    let old_digest: Option<String> = q
        .query_row(
            "SELECT blob_sha256 FROM artifacts WHERE repo_id = ? AND path = ?",
            params![artifact.repo_id, artifact.path],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| Error::sqlite("lookup batch artifact", e))?;
    q.execute(
        "INSERT INTO artifacts(
		 repo_id, path, version, blob_sha256, size, content_type, metadata_json,
		 published_at, cached_at, last_accessed_at, updated_at, cached_by,
		 publication_id, artifact_role)
		 VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULLIF(?, ''), ?)
		 ON CONFLICT(repo_id, path) DO UPDATE SET
		 version = excluded.version, blob_sha256 = excluded.blob_sha256,
		 size = excluded.size, content_type = excluded.content_type,
		 metadata_json = excluded.metadata_json, published_at = excluded.published_at,
		 cached_at = excluded.cached_at, last_accessed_at = excluded.last_accessed_at,
		 updated_at = excluded.updated_at, cached_by = excluded.cached_by,
		 publication_id = excluded.publication_id, artifact_role = excluded.artifact_role",
        params![
            artifact.repo_id,
            artifact.path,
            artifact.version,
            artifact.blob_sha256,
            artifact.size,
            content_type,
            metadata_json,
            format_time_opt(artifact.published_at),
            now,
            now,
            now,
            artifact.cached_by,
            artifact.publication_id,
            artifact_role
        ],
    )
    .map_err(|e| Error::sqlite("upsert batch artifact", e))?;
    match old_digest {
        None => adjust_ref_runner(q, &artifact.blob_sha256, 1),
        Some(old) if old != artifact.blob_sha256 => {
            adjust_ref_runner(q, &artifact.blob_sha256, 1)?;
            adjust_ref_runner(q, &old, -1)
        }
        Some(_) => Ok(()),
    }
}

/// Removes a path only while it is still owned by `publication_id`; anything
/// else is [`Error::ArtifactConflict`].
fn delete_owned_artifact(
    q: &Connection,
    repo_id: i64,
    path: &str,
    publication_id: &str,
) -> Result<()> {
    let row: Option<(String, Option<String>)> = q
        .query_row(
            "SELECT blob_sha256, publication_id FROM artifacts WHERE repo_id = ? AND path = ?",
            params![repo_id, path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| Error::sqlite("lookup owned artifact", e))?;
    let digest = match row {
        Some((digest, Some(owner))) if owner == publication_id => digest,
        _ => return Err(Error::ArtifactConflict),
    };
    q.execute(
        "DELETE FROM artifacts WHERE repo_id = ? AND path = ?",
        params![repo_id, path],
    )
    .map_err(|e| Error::sqlite("delete owned artifact", e))?;
    adjust_ref_runner(q, &digest, -1)
}

/// Removes a shared aggregate index; a missing row or one owned by a
/// publication means the planner's view is stale
/// ([`Error::DerivedMetadataChanged`]).
fn delete_aggregate_artifact(q: &Connection, repo_id: i64, path: &str) -> Result<()> {
    let row: Option<(String, Option<String>)> = q
        .query_row(
            "SELECT blob_sha256, publication_id FROM artifacts WHERE repo_id = ? AND path = ?",
            params![repo_id, path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| Error::sqlite("lookup aggregate artifact", e))?;
    let digest = match row {
        Some((digest, owner)) if !owner.as_deref().is_some_and(|o| !o.is_empty()) => digest,
        _ => return Err(Error::DerivedMetadataChanged),
    };
    q.execute(
        "DELETE FROM artifacts WHERE repo_id = ? AND path = ?",
        params![repo_id, path],
    )
    .map_err(|e| Error::sqlite("delete aggregate artifact", e))?;
    adjust_ref_runner(q, &digest, -1)
}

/// Records a tombstone, defaulting `deleted_at` to now. Idempotent on the
/// coordinate/asset key.
fn insert_tombstone(
    q: &Connection,
    tombstone: &ArtifactPublicationTombstone,
) -> rusqlite::Result<()> {
    let deleted_at = if is_zero(tombstone.deleted_at) {
        Utc::now()
    } else {
        tombstone.deleted_at
    };
    q.execute(
        "INSERT INTO artifact_publication_tombstones(
				 repo_id, format, package_name, version, asset_key, deleted_at, deleted_by)
				 VALUES(?, ?, ?, ?, ?, ?, ?)
				 ON CONFLICT(repo_id, format, package_name, version, asset_key) DO NOTHING",
        params![
            tombstone.repo_id,
            tombstone.format,
            tombstone.package_name,
            tombstone.version,
            tombstone.asset_key,
            format_time(deleted_at),
            tombstone.deleted_by
        ],
    )
    .map(|_| ())
}

/// Drops one cached representation (or every representation of the path when
/// `key.representation` is empty) and releases the blob references it held.
pub(crate) fn delete_group_cache(q: &Connection, key: &GroupMetadataCacheKey) -> Result<()> {
    if key.representation.is_empty() {
        let digests: Vec<String> = query_all(
            q,
            "list group cache digests",
            "SELECT blob_sha256 FROM group_metadata_cache WHERE group_repo_id = ? AND path = ?",
            params![key.group_repo_id, key.path],
            |r| r.get(0),
        )?;
        q.execute(
            "DELETE FROM group_metadata_cache WHERE group_repo_id = ? AND path = ?",
            params![key.group_repo_id, key.path],
        )
        .map_err(|e| Error::sqlite("delete group cache", e))?;
        for digest in &digests {
            adjust_ref_runner(q, digest, -1)?;
        }
        return Ok(());
    }
    let digest: Option<String> = q
        .query_row(
            "SELECT blob_sha256 FROM group_metadata_cache
			 WHERE group_repo_id = ? AND path = ? AND representation = ?",
            params![key.group_repo_id, key.path, key.representation],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| Error::sqlite("lookup group cache", e))?;
    let Some(digest) = digest else {
        return Ok(());
    };
    q.execute(
        "DELETE FROM group_metadata_cache WHERE group_repo_id = ? AND path = ? AND representation = ?",
        params![key.group_repo_id, key.path, key.representation],
    )
    .map_err(|e| Error::sqlite("delete group cache", e))?;
    adjust_ref_runner(q, &digest, -1)
}

/// Resolves reverse group membership inside the caller's transaction. An
/// empty `path` deliberately clears every cached representation for the
/// affected groups; managed publications use this broad form so PyPI
/// root/project indexes are invalidated alongside direct paths.
pub(crate) fn invalidate_group_caches_for_member(
    q: &Connection,
    member_repo_id: i64,
    path: &str,
) -> Result<()> {
    let group_ids: Vec<i64> = query_all(
        q,
        "resolve member groups",
        "SELECT DISTINCT g.id FROM repositories g
		JOIN repositories m ON m.id = ?
		JOIN json_each(g.config_json, '$.group.members') member ON member.value = m.name
		WHERE g.type = 'group'",
        params![member_repo_id],
        |r| r.get(0),
    )?;
    for group_id in group_ids {
        let keys: Vec<GroupMetadataCacheKey> = if path.is_empty() {
            query_all(
                q,
                "list group cache keys",
                "SELECT path, representation FROM group_metadata_cache WHERE group_repo_id = ?",
                params![group_id],
                |r| {
                    Ok(GroupMetadataCacheKey {
                        group_repo_id: group_id,
                        path: r.get(0)?,
                        representation: r.get(1)?,
                    })
                },
            )?
        } else {
            query_all(
                q,
                "list group cache keys",
                "SELECT path, representation FROM group_metadata_cache WHERE group_repo_id = ? AND path = ?",
                params![group_id, path],
                |r| {
                    Ok(GroupMetadataCacheKey {
                        group_repo_id: group_id,
                        path: r.get(0)?,
                        representation: r.get(1)?,
                    })
                },
            )?
        };
        for key in &keys {
            delete_group_cache(q, key)?;
        }
    }
    Ok(())
}

/// Drops every cached representation a group repository holds, releasing the
/// blob references.
pub(crate) fn clear_group_metadata_cache(q: &Connection, group_repo_id: i64) -> Result<()> {
    let keys: Vec<GroupMetadataCacheKey> = query_all(
        q,
        "list group cache keys",
        "SELECT path, representation FROM group_metadata_cache WHERE group_repo_id = ?",
        params![group_repo_id],
        |r| {
            Ok(GroupMetadataCacheKey {
                group_repo_id,
                path: r.get(0)?,
                representation: r.get(1)?,
            })
        },
    )?;
    for key in &keys {
        delete_group_cache(q, key)?;
    }
    Ok(())
}

/// Applies `ADJUST_REF_SQL` and insists exactly one blob row moved; a missing
/// row means the batch references bytes nobody staged.
pub(crate) fn adjust_ref_runner(q: &Connection, sha: &str, delta: i64) -> Result<()> {
    let n = q
        .execute(ADJUST_REF_SQL, params![delta, delta, now_rfc3339(), sha])
        .map_err(|e| Error::sqlite("adjust blob ref", e))?;
    if n != 1 {
        return Err(Error::Other(format!("blob {sha} missing")));
    }
    Ok(())
}

fn scan_upload_request(row: &Row<'_>) -> rusqlite::Result<ArtifactUploadRequest> {
    let created: String = row.get(8)?;
    let updated: String = row.get(9)?;
    let expires: String = row.get(10)?;
    Ok(ArtifactUploadRequest {
        idempotency_key: row.get(0)?,
        repo_id: row.get(1)?,
        principal_name: row.get(2)?,
        principal_source: row.get(3)?,
        upload_id: row.get(4)?,
        state: row.get(5)?,
        plan_json: row.get(6)?,
        result_json: row.get(7)?,
        created_at: parse_time(&created),
        updated_at: parse_time(&updated),
        expires_at: parse_time(&expires),
    })
}

fn scan_publication(row: &Row<'_>) -> rusqlite::Result<ArtifactPublication> {
    let created: String = row.get(10)?;
    let updated: String = row.get(11)?;
    Ok(ArtifactPublication {
        id: row.get(0)?,
        repo_id: row.get(1)?,
        format: row.get(2)?,
        package_name: row.get(3)?,
        version: row.get(4)?,
        coordinate: row.get(5)?,
        upload_id: row.get(6)?,
        created_by: row.get(7)?,
        created_by_source: row.get(8)?,
        yanked: row.get(9)?,
        created_at: parse_time(&created),
        updated_at: parse_time(&updated),
        asset_count: row.get(12)?,
        total_size: row.get(13)?,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use chrono::{Duration, Utc};

    use crate::meta::*;

    /// Creates a hosted Maven repository plus a receiving upload request, the
    /// fixture every publication case starts from. Shared with `extra_test.rs`.
    pub(crate) async fn upload_fixture(
        s: &Store,
    ) -> (Repository, UploadRequestKey, ArtifactUploadRequest) {
        let repository = s
            .create_repository(Repository {
                name: "maven-upload".into(),
                format: FORMAT_MAVEN.into(),
                r#type: TYPE_HOSTED.into(),
                ..Repository::default()
            })
            .await
            .unwrap();
        let key = UploadRequestKey {
            repo_id: repository.id,
            principal_source: SOURCE_LOCAL.into(),
            principal_name: "alice".into(),
            idempotency_key: "12345678-1234-4234-8234-123456789abc".into(),
        };
        let request = ArtifactUploadRequest {
            idempotency_key: key.idempotency_key.clone(),
            repo_id: key.repo_id,
            principal_name: key.principal_name.clone(),
            principal_source: key.principal_source.clone(),
            upload_id: "01UPLOAD000000000000000001".into(),
            state: UPLOAD_RECEIVING.into(),
            expires_at: Utc::now() + Duration::hours(24),
            ..ArtifactUploadRequest::default()
        };
        s.create_upload_request(request.clone()).await.unwrap();
        (repository, key, request)
    }

    #[tokio::test]
    async fn upload_request_idempotency_scope() {
        let (s, _dir) = test_store().await;
        let (_, key, request) = upload_fixture(&s).await;
        let err = s.create_upload_request(request.clone()).await.unwrap_err();
        assert!(
            err.is_conflict(),
            "duplicate request error = {err}, want Conflict"
        );
        let got = s
            .get_upload_request(key.clone())
            .await
            .expect("get request");
        assert_eq!(got.upload_id, request.upload_id);
        assert_eq!(got.state, UPLOAD_RECEIVING);
        let by_id = s
            .get_upload_request_by_id(&request.upload_id)
            .await
            .expect("get request by ID");
        assert_eq!(by_id.idempotency_key, key.idempotency_key);
    }

    #[tokio::test]
    async fn apply_artifact_batch_commits_atomically() {
        let (s, _dir) = test_store().await;
        let (repository, key, request) = upload_fixture(&s).await;
        s.lease_upload_blob(&request.upload_id, "jar-digest", 10)
            .await
            .unwrap();
        let publication = ArtifactPublication {
            id: "01PUBLICATION0000000000001".into(),
            repo_id: repository.id,
            format: FORMAT_MAVEN.into(),
            package_name: "com.acme:widget".into(),
            version: "1.0.0".into(),
            coordinate: "com.acme:widget:1.0.0".into(),
            upload_id: request.upload_id.clone(),
            created_by: "alice".into(),
            created_by_source: SOURCE_LOCAL.into(),
            ..ArtifactPublication::default()
        };
        let batch = ArtifactBatch {
            expected_upload_state: UPLOAD_RECEIVING.into(),
            publication: publication.clone(),
            create: vec![
                Artifact {
                    repo_id: repository.id,
                    path: "com/acme/widget/1.0.0/widget-1.0.0.jar".into(),
                    version: "1.0.0".into(),
                    blob_sha256: "jar-digest".into(),
                    size: 10,
                    publication_id: publication.id.clone(),
                    artifact_role: "primary".into(),
                    ..Artifact::default()
                },
                Artifact {
                    repo_id: repository.id,
                    path: "com/acme/widget/1.0.0/widget-1.0.0.pom".into(),
                    version: "1.0.0".into(),
                    blob_sha256: "pom-digest".into(),
                    size: 5,
                    publication_id: publication.id.clone(),
                    artifact_role: "metadata".into(),
                    ..Artifact::default()
                },
            ],
            mutable_cas: vec![ArtifactCAS {
                artifact: Artifact {
                    repo_id: repository.id,
                    path: "com/acme/widget/maven-metadata.xml".into(),
                    blob_sha256: "metadata-digest".into(),
                    size: 7,
                    metadata_json: r#"{"schema_version":1,"managed_by":"ui_upload_aggregate"}"#
                        .into(),
                    artifact_role: "index".into(),
                    ..Artifact::default()
                },
                expected_sha256: String::new(),
            }],
            upload_result_json: r#"{"upload_id":"01UPLOAD000000000000000001"}"#.into(),
            ..ArtifactBatch::default()
        };
        s.apply_artifact_batch(key.clone(), batch.clone())
            .await
            .unwrap();
        let got = s
            .get_upload_request(key.clone())
            .await
            .expect("committed request");
        assert_eq!(got.state, UPLOAD_COMMITTED);
        assert_eq!(got.result_json, batch.upload_result_json);
        for (path, want_role) in [
            ("com/acme/widget/1.0.0/widget-1.0.0.jar", "primary"),
            ("com/acme/widget/1.0.0/widget-1.0.0.pom", "metadata"),
            ("com/acme/widget/maven-metadata.xml", "index"),
        ] {
            let artifact = s.get_artifact(repository.id, path).await.expect(path);
            assert_eq!(artifact.artifact_role, want_role, "artifact {path}");
            if want_role == "index" {
                assert_eq!(
                    artifact.publication_id, "",
                    "shared index owns a publication"
                );
            }
        }
        for digest in ["jar-digest", "pom-digest", "metadata-digest"] {
            let blob = s.get_blob(digest).await.expect(digest);
            assert_eq!(blob.ref_count, 1, "blob {digest}");
        }
        let upload_id = request.upload_id.clone();
        let leases: i64 = s
            .read(move |c| {
                c.query_row(
                    "SELECT COUNT(*) FROM artifact_upload_staged_blobs WHERE upload_id = ?",
                    rusqlite::params![upload_id],
                    |r| r.get(0),
                )
                .map_err(|e| Error::sqlite("count leases", e))
            })
            .await
            .unwrap();
        assert_eq!(leases, 0, "leases");
        let err = s
            .apply_artifact_batch(key, batch.clone())
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::UploadState),
            "replay commit error = {err}, want UploadState"
        );
        let err = s
            .put_artifact(Artifact {
                repo_id: repository.id,
                path: batch.create[0].path.clone(),
                blob_sha256: "raw-overwrite".into(),
                size: 1,
                ..Artifact::default()
            })
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::ManagedArtifact),
            "raw overwrite error = {err}, want ManagedArtifact"
        );
        let aggregate_path = batch.mutable_cas[0].artifact.path.clone();
        let err = s
            .put_artifact(Artifact {
                repo_id: repository.id,
                path: aggregate_path.clone(),
                blob_sha256: "raw-overwrite".into(),
                size: 1,
                ..Artifact::default()
            })
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::ManagedArtifact),
            "raw aggregate overwrite error = {err}, want ManagedArtifact"
        );
        let err = s
            .delete_artifact(repository.id, &aggregate_path)
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::ManagedArtifact),
            "raw aggregate delete error = {err}, want ManagedArtifact"
        );
    }

    #[tokio::test]
    async fn apply_artifact_batch_rolls_back_on_conflict() {
        let (s, _dir) = test_store().await;
        let (repository, key, request) = upload_fixture(&s).await;
        let conflict_path = "com/acme/widget/1.0.0/widget-1.0.0.jar";
        s.put_artifact(Artifact {
            repo_id: repository.id,
            path: conflict_path.into(),
            blob_sha256: "legacy".into(),
            size: 3,
            ..Artifact::default()
        })
        .await
        .unwrap();
        let publication = ArtifactPublication {
            id: "01PUBLICATION0000000000002".into(),
            repo_id: repository.id,
            format: FORMAT_MAVEN.into(),
            package_name: "com.acme:widget".into(),
            version: "1.0.0".into(),
            coordinate: "com.acme:widget:1.0.0".into(),
            upload_id: request.upload_id.clone(),
            created_by: "alice".into(),
            created_by_source: SOURCE_LOCAL.into(),
            ..ArtifactPublication::default()
        };
        let err = s
            .apply_artifact_batch(
                key.clone(),
                ArtifactBatch {
                    publication: publication.clone(),
                    create: vec![Artifact {
                        repo_id: repository.id,
                        path: conflict_path.into(),
                        blob_sha256: "new".into(),
                        size: 4,
                        publication_id: publication.id.clone(),
                        ..Artifact::default()
                    }],
                    upload_result_json: "{}".into(),
                    ..ArtifactBatch::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::ArtifactConflict),
            "commit error = {err}, want ArtifactConflict"
        );
        let got = s.get_upload_request(key).await.unwrap();
        assert_eq!(got.state, UPLOAD_RECEIVING, "request state");
        let publications: i64 = s
            .read(|c| {
                c.query_row("SELECT COUNT(*) FROM artifact_publications", [], |r| {
                    r.get(0)
                })
                .map_err(|e| Error::sqlite("count publications", e))
            })
            .await
            .unwrap();
        assert_eq!(publications, 0, "publication count");
        let artifact = s.get_artifact(repository.id, conflict_path).await.unwrap();
        assert_eq!(
            artifact.blob_sha256, "legacy",
            "conflicting artifact changed"
        );
        let err = s.get_blob("new").await.unwrap_err();
        assert!(
            err.is_not_found(),
            "new blob metadata survived rollback: {err}"
        );
    }

    #[tokio::test]
    async fn apply_artifact_batch_rejects_stale_mutable_metadata() {
        let (s, _dir) = test_store().await;
        let (repository, key, request) = upload_fixture(&s).await;
        let path = "pkg/index.json";
        s.put_artifact(Artifact {
            repo_id: repository.id,
            path: path.into(),
            blob_sha256: "current".into(),
            size: 7,
            artifact_role: "index".into(),
            ..Artifact::default()
        })
        .await
        .unwrap();
        let publication = ArtifactPublication {
            id: "01PUBLICATION0000000000003".into(),
            repo_id: repository.id,
            format: FORMAT_MAVEN.into(),
            package_name: "com.acme:widget".into(),
            version: "1.0.0".into(),
            coordinate: "com.acme:widget:1.0.0".into(),
            upload_id: request.upload_id.clone(),
            created_by: "alice".into(),
            created_by_source: SOURCE_LOCAL.into(),
            ..ArtifactPublication::default()
        };
        let err = s
            .apply_artifact_batch(
                key,
                ArtifactBatch {
                    publication,
                    mutable_cas: vec![ArtifactCAS {
                        artifact: Artifact {
                            repo_id: repository.id,
                            path: path.into(),
                            blob_sha256: "next".into(),
                            size: 8,
                            artifact_role: "index".into(),
                            ..Artifact::default()
                        },
                        expected_sha256: "stale".into(),
                    }],
                    ..ArtifactBatch::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::DerivedMetadataChanged),
            "commit error = {err}, want DerivedMetadataChanged"
        );
        let artifact = s.get_artifact(repository.id, path).await.unwrap();
        assert_eq!(artifact.blob_sha256, "current", "mutable metadata changed");
    }

    /// Commits a Maven publication with two owned artifacts and returns the
    /// publication so query/lifecycle helpers can be exercised.
    async fn commit_fixture_batch(s: &Store) -> (Repository, ArtifactPublication) {
        let (repository, key, request) = upload_fixture(s).await;
        s.lease_upload_blob(&request.upload_id, "jar-digest", 10)
            .await
            .unwrap();
        let publication = ArtifactPublication {
            id: "01PUBLICATION0000000000001".into(),
            repo_id: repository.id,
            format: FORMAT_MAVEN.into(),
            package_name: "com.acme:widget".into(),
            version: "1.0.0".into(),
            coordinate: "com.acme:widget:1.0.0".into(),
            upload_id: request.upload_id.clone(),
            created_by: "alice".into(),
            created_by_source: SOURCE_LOCAL.into(),
            ..ArtifactPublication::default()
        };
        let batch = ArtifactBatch {
            expected_upload_state: UPLOAD_RECEIVING.into(),
            publication: publication.clone(),
            create: vec![
                Artifact {
                    repo_id: repository.id,
                    path: "com/acme/widget/1.0.0/widget-1.0.0.jar".into(),
                    version: "1.0.0".into(),
                    blob_sha256: "jar-digest".into(),
                    size: 10,
                    publication_id: publication.id.clone(),
                    artifact_role: "primary".into(),
                    ..Artifact::default()
                },
                Artifact {
                    repo_id: repository.id,
                    path: "com/acme/widget/1.0.0/widget-1.0.0.pom".into(),
                    version: "1.0.0".into(),
                    blob_sha256: "pom-digest".into(),
                    size: 5,
                    publication_id: publication.id.clone(),
                    artifact_role: "metadata".into(),
                    ..Artifact::default()
                },
            ],
            upload_result_json: r#"{"upload_id":"01UPLOAD000000000000000001"}"#.into(),
            ..ArtifactBatch::default()
        };
        s.apply_artifact_batch(key, batch).await.unwrap();
        (repository, publication)
    }

    #[tokio::test]
    async fn publication_queries() {
        let (s, _dir) = test_store().await;
        let (repository, publication) = commit_fixture_batch(&s).await;

        let by_id = s
            .get_artifact_publication(&publication.id)
            .await
            .expect("get_artifact_publication");
        assert_eq!(by_id.package_name, "com.acme:widget");
        assert_eq!(by_id.asset_count, 2);
        let by_identity = s
            .get_artifact_publication_by_identity(
                repository.id,
                FORMAT_MAVEN,
                "com.acme:widget",
                "1.0.0",
            )
            .await
            .expect("get_artifact_publication_by_identity");
        assert_eq!(by_identity.id, publication.id);
        let list = s
            .list_artifact_publications(repository.id)
            .await
            .expect("list_artifact_publications");
        assert_eq!(list.len(), 1);
        let owned = s
            .list_publication_artifacts(&publication.id)
            .await
            .expect("list_publication_artifacts");
        assert_eq!(owned.len(), 2);
        let has = s
            .has_publication_tombstone(
                repository.id,
                FORMAT_MAVEN,
                "com.acme:widget",
                "1.0.0",
                "widget-1.0.0.jar",
            )
            .await
            .expect("has_publication_tombstone");
        assert!(!has);

        // Unknown ids surface NotFound.
        let err = s
            .get_artifact_publication("does-not-exist")
            .await
            .unwrap_err();
        assert!(
            err.is_not_found(),
            "missing publication error = {err}, want NotFound"
        );
    }

    #[tokio::test]
    async fn apply_publication_lifecycle_deletes() {
        let (s, _dir) = test_store().await;
        let (repository, publication) = commit_fixture_batch(&s).await;

        let owned = s.list_publication_artifacts(&publication.id).await.unwrap();
        let mut batch = PublicationLifecycleBatch {
            publication: publication.clone(),
            delete_publication: true,
            ..PublicationLifecycleBatch::default()
        };
        for artifact in &owned {
            batch.remove_paths.push(artifact.path.clone());
        }
        let remove_paths = batch.remove_paths.clone();
        s.apply_publication_lifecycle(batch)
            .await
            .expect("apply_publication_lifecycle");
        let list = s.list_artifact_publications(repository.id).await.unwrap();
        assert!(list.is_empty(), "publications after delete = {list:?}");
        for path in &remove_paths {
            let err = s.get_artifact(repository.id, path).await.unwrap_err();
            assert!(err.is_not_found(), "artifact {path} still present: {err}");
        }
    }

    #[tokio::test]
    async fn set_upload_state_and_cancel_conflict() {
        let (s, _dir) = test_store().await;
        let (_, _, request) = upload_fixture(&s).await;

        let expires = Utc::now() + Duration::hours(1);
        s.set_upload_state(
            &request.upload_id,
            UPLOAD_RECEIVING,
            UPLOAD_CONFLICT,
            r#"{"problem":{"code":"conflict"}}"#,
            expires,
        )
        .await
        .expect("set_upload_state");
        let got = s
            .get_upload_request_by_id(&request.upload_id)
            .await
            .unwrap();
        assert_eq!(got.state, UPLOAD_CONFLICT, "state after set");
        // A wrong expected state is rejected (actual is now conflict, not receiving).
        let err = s
            .set_upload_state(
                &request.upload_id,
                UPLOAD_RECEIVING,
                UPLOAD_CONFLICT,
                "",
                expires,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::UploadState),
            "stale set_upload_state = {err}, want UploadState"
        );
        s.cancel_upload_conflict(&request.upload_id)
            .await
            .expect("cancel_upload_conflict");
    }

    mod extra {
        use crate::meta::publication::tests::upload_fixture;
        use crate::meta::*;

        /// The announcement is a single row that may not exist yet, so "never set" has
        /// to be distinguishable from "set to empty": the first is NotFound, the
        /// second is a stored empty body that still records who cleared it.
        #[tokio::test]
        async fn announcement_lifecycle() {
            let (s, _dir) = test_store().await;

            let err = s.get_announcement().await.unwrap_err();
            assert!(
                err.is_not_found(),
                "unset announcement = {err}, want NotFound"
            );

            let set = s
                .set_announcement("# maintenance window", "admin")
                .await
                .expect("set");
            assert_eq!(set.body, "# maintenance window");
            assert_eq!(set.updated_by, "admin");
            assert!(
                !time::is_zero(set.updated_at),
                "set returned a zero timestamp"
            );
            let got = s.get_announcement().await.expect("get");
            assert_eq!(got.body, set.body, "stored announcement");
            assert_eq!(got.updated_by, "admin");

            // Clearing keeps the row: the console still shows who removed the notice.
            s.set_announcement("", "operator").await.expect("clear");
            let got = s.get_announcement().await.expect("get after clear");
            assert_eq!(
                (got.body.as_str(), got.updated_by.as_str()),
                ("", "operator")
            );
        }

        /// The landing page shows two instance-wide totals to anonymous callers, so
        /// they must count every repository and artifact rather than a permitted
        /// subset.
        #[tokio::test]
        async fn landing_counts() {
            let (s, _dir) = test_store().await;

            assert_eq!(
                s.count_repositories().await.unwrap(),
                0,
                "empty repository count"
            );
            assert_eq!(
                s.count_all_artifacts().await.unwrap(),
                0,
                "empty artifact count"
            );

            let first = s
                .create_repository(Repository {
                    name: "maven-hosted".into(),
                    format: FORMAT_MAVEN.into(),
                    r#type: TYPE_HOSTED.into(),
                    ..Repository::default()
                })
                .await
                .unwrap();
            let second = s
                .create_repository(Repository {
                    name: "npm-hosted".into(),
                    format: FORMAT_NPM.into(),
                    r#type: TYPE_HOSTED.into(),
                    ..Repository::default()
                })
                .await
                .unwrap();
            for (repo_id, path, sha, size) in [
                (first.id, "a.jar", "sha-a", 1),
                (second.id, "b.tgz", "sha-b", 2),
                (second.id, "c.tgz", "sha-c", 3),
            ] {
                s.put_artifact(Artifact {
                    repo_id,
                    path: path.into(),
                    blob_sha256: sha.into(),
                    size,
                    ..Artifact::default()
                })
                .await
                .unwrap();
            }

            assert_eq!(s.count_repositories().await.unwrap(), 2, "repository count");
            assert_eq!(s.count_all_artifacts().await.unwrap(), 3, "artifact count");
        }

        /// Renaming and re-describing a repository are the two seed-reconciliation
        /// writes, and both must report NotFound for an id that is not there rather
        /// than silently doing nothing: the reconciler treats a missing row as a
        /// repository to create.
        #[tokio::test]
        async fn rename_and_describe_repository() {
            let (s, _dir) = test_store().await;
            let repo = s
                .create_repository(Repository {
                    name: "docker-hub".into(),
                    format: FORMAT_OCI.into(),
                    r#type: TYPE_PROXY.into(),
                    ..Repository::default()
                })
                .await
                .unwrap();

            s.rename_repository(repo.id, "docker.io-proxy")
                .await
                .expect("rename");
            s.update_repository_description(repo.id, "Docker Hub mirror")
                .await
                .expect("describe");
            let got = s.get_repository(repo.id).await.expect("get");
            assert_eq!(got.name, "docker.io-proxy", "repository = {got:?}");
            assert_eq!(got.description, "Docker Hub mirror", "repository = {got:?}");
            // The old name is gone, so a lookup by it must fail rather than resolve.
            let err = s.get_repository_by_name("docker-hub").await.unwrap_err();
            assert!(
                err.is_not_found(),
                "lookup by old name = {err}, want NotFound"
            );

            let err = s.rename_repository(4242, "ghost").await.unwrap_err();
            assert!(err.is_not_found(), "rename unknown = {err}, want NotFound");
            let err = s
                .update_repository_description(4242, "ghost")
                .await
                .unwrap_err();
            assert!(
                err.is_not_found(),
                "describe unknown = {err}, want NotFound"
            );
        }

        /// `force_delete_artifact` is the repair path for an artifact whose bytes are
        /// gone: it removes what the ordinary delete refuses, and when the artifact was
        /// the last asset of a publication it takes the publication with it and
        /// deliberately leaves no tombstone, so the same version can be published
        /// again.
        #[tokio::test]
        async fn force_delete_artifact_removes_managed_publication() {
            let (s, _dir) = test_store().await;
            // The fixture leaves a receiving upload request, which is what a publication
            // has to be committed against.
            let (repo, key, request) = upload_fixture(&s).await;
            const PATH: &str = "com/acme/widget/1.0.0/widget-1.0.0.jar";
            let publication = ArtifactPublication {
                id: "01PUBLICATION0000000000009".into(),
                repo_id: repo.id,
                format: FORMAT_MAVEN.into(),
                package_name: "com.acme:widget".into(),
                version: "1.0.0".into(),
                coordinate: "com.acme:widget:1.0.0".into(),
                upload_id: request.upload_id.clone(),
                created_by: "alice".into(),
                created_by_source: SOURCE_LOCAL.into(),
                ..ArtifactPublication::default()
            };
            s.apply_artifact_batch(
                key,
                ArtifactBatch {
                    publication: publication.clone(),
                    create: vec![Artifact {
                        repo_id: repo.id,
                        path: PATH.into(),
                        version: "1.0.0".into(),
                        blob_sha256: "sha-app".into(),
                        size: 12,
                        publication_id: publication.id.clone(),
                        artifact_role: "primary".into(),
                        cached_by: "alice".into(),
                        ..Artifact::default()
                    }],
                    upload_result_json: "{}".into(),
                    ..ArtifactBatch::default()
                },
            )
            .await
            .expect("apply batch");

            // The ordinary delete refuses a managed path: that is what force exists for.
            let err = s.delete_artifact(repo.id, PATH).await.unwrap_err();
            assert!(
                matches!(err, Error::ManagedArtifact),
                "plain delete of a managed artifact = {err}, want ManagedArtifact"
            );

            let result = s
                .force_delete_artifact(repo.id, PATH)
                .await
                .expect("force delete");
            assert_eq!(result.blob_sha256, "sha-app", "result = {result:?}");
            assert_eq!(result.publication_id, publication.id, "result = {result:?}");
            assert_eq!(
                result.coordinate, publication.coordinate,
                "result = {result:?}"
            );
            let err = s.get_artifact(repo.id, PATH).await.unwrap_err();
            assert!(
                err.is_not_found(),
                "artifact after force delete = {err}, want NotFound"
            );
            let err = s
                .get_artifact_publication(&publication.id)
                .await
                .unwrap_err();
            assert!(
                err.is_not_found(),
                "publication after force delete = {err}, want NotFound"
            );
            // No tombstone: republishing the same version has to be possible.
            let tombstoned = s
                .has_publication_tombstone(repo.id, FORMAT_MAVEN, "com.acme:widget", "1.0.0", PATH)
                .await
                .expect("tombstone lookup");
            assert!(
                !tombstoned,
                "force delete left a tombstone, which would block republishing the version"
            );

            let err = s.force_delete_artifact(repo.id, PATH).await.unwrap_err();
            assert!(
                err.is_not_found(),
                "force delete of a missing artifact = {err}, want NotFound"
            );
        }
    }
}
