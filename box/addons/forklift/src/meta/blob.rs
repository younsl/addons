//! Blob reference records: lookup, aggregate stats, GC candidate listing, replication paging
//! and the guarded record delete.

use chrono::{DateTime, Utc};
use rusqlite::params;

use super::repository::query_all;
use super::{Blob, Error, FormatUsage, Result, Store, format_time, parse_time};

impl Store {
    /// Returns blob metadata by digest.
    pub async fn get_blob(&self, sha: &str) -> Result<Blob> {
        let sha = sha.to_string();
        self.read(move |conn| {
            conn.query_row(
                "SELECT sha256, size, ref_count, created_at FROM blobs WHERE sha256 = ?",
                params![sha],
                |r| {
                    let created: String = r.get(3)?;
                    Ok(Blob {
                        sha256: r.get(0)?,
                        size: r.get(1)?,
                        ref_count: r.get(2)?,
                        created_at: parse_time(&created),
                    })
                },
            )
            .map_err(|e| Error::sqlite("get blob", e))
        })
        .await
    }

    /// Returns the number of stored blobs and their total physical size in
    /// bytes, as `(count, bytes)`. Because blobs are content-addressed and
    /// deduplicated, this reflects actual disk usage rather than the logical
    /// artifact size.
    pub async fn blob_stats(&self) -> Result<(i64, i64)> {
        self.read(|conn| {
            conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM blobs",
                [],
                |r| {
                    let c: Option<i64> = r.get(0)?;
                    let b: Option<i64> = r.get(1)?;
                    Ok((c.unwrap_or(0), b.unwrap_or(0)))
                },
            )
            .map_err(|e| Error::sqlite("blob stats", e))
        })
        .await
    }

    /// Returns the stored blob footprint split by the format of the
    /// repositories referencing each blob, largest first. A blob shared across
    /// formats counts once, under the first format by name, so the parts never
    /// add up to more than [`Store::blob_stats`]. Blobs no artifact references
    /// (awaiting garbage collection) are not in any part.
    pub async fn blob_usage_by_format(&self) -> Result<Vec<FormatUsage>> {
        self.read(|conn| {
            query_all(
                conn,
                "blob usage by format",
                "SELECT format, COUNT(*), SUM(size) FROM (
                   SELECT b.size AS size, MIN(r.format) AS format
                     FROM blobs b
                     JOIN artifacts a ON a.blob_sha256 = b.sha256
                     JOIN repositories r ON r.id = a.repo_id
                    GROUP BY b.sha256
                 )
                 GROUP BY format
                 ORDER BY SUM(size) DESC, format ASC",
                [],
                |r| {
                    Ok(FormatUsage {
                        format: r.get(0)?,
                        blob_count: r.get(1)?,
                        bytes: r.get(2)?,
                    })
                },
            )
        })
        .await
    }

    /// Returns digests of blobs that have been unreferenced since before the
    /// cutoff, capped by limit. The cache sweeper uses this to delete bytes
    /// from the blob store.
    ///
    /// The cutoff enforces the GC grace period: bytes are reclaimed only once
    /// the digest has been unreferenced for long enough that an asynchronous
    /// metadata rollback (s3 backend, see `objstore`) can no longer resurrect a
    /// row pointing at them. `created_at` is checked too, so a blob written
    /// moments ago -- which a snapshot may have captured after `blobs.put` but
    /// before the artifact row took its reference -- is never a GC candidate.
    pub async fn list_unreferenced_blobs(
        &self,
        limit: i64,
        before: DateTime<Utc>,
    ) -> Result<Vec<String>> {
        let cutoff = format_time(before);
        self.read(move |conn| {
            query_all(
                conn,
                "list unreferenced blobs",
                "SELECT sha256 FROM blobs
           WHERE ref_count <= 0
             AND unreferenced_since IS NOT NULL AND julianday(unreferenced_since) <= julianday(?)
             AND julianday(created_at) <= julianday(?)
           LIMIT ?",
                params![cutoff, cutoff, limit],
                |r| r.get(0),
            )
        })
        .await
    }

    /// Returns blob digests ordered by sha256, starting strictly after the
    /// cursor, capped by limit. Replication standbys page through this to
    /// mirror the leader's blob set.
    pub async fn list_blob_digests(&self, after: &str, limit: i64) -> Result<Vec<String>> {
        let after = after.to_string();
        self.read(move |conn| {
            query_all(
                conn,
                "list blob digests",
                "SELECT sha256 FROM blobs WHERE sha256 > ? ORDER BY sha256 LIMIT ?",
                params![after, limit],
                |r| r.get(0),
            )
        })
        .await
    }

    /// Removes a blob row, but only while it is still unreferenced
    /// (`ref_count <= 0`) and past the grace cutoff, and reports whether a row
    /// was actually deleted. The sweeper deletes the record before the bytes
    /// and reclaims the bytes only when the result is `true`: if a concurrent
    /// upload or proxy fetch of identical (content-addressed) content
    /// re-referenced this digest since it was listed, the guarded delete
    /// removes no row, the result is `false`, and the now-live bytes are left
    /// in place. The cutoff conditions mirror [`Store::list_unreferenced_blobs`]
    /// so a digest re-referenced and re-freed between the two calls cannot skip
    /// its grace period.
    pub async fn delete_blob_record(&self, sha: &str, before: DateTime<Utc>) -> Result<bool> {
        let sha = sha.to_string();
        let cutoff = format_time(before);
        self.write(move |conn| {
            let n = conn
                .execute(
                    "DELETE FROM blobs
           WHERE sha256 = ?
             AND ref_count <= 0
             AND unreferenced_since IS NOT NULL AND julianday(unreferenced_since) <= julianday(?)
             AND julianday(created_at) <= julianday(?)",
                    params![sha, cutoff, cutoff],
                )
                .map_err(|e| Error::sqlite("delete blob record", e))?;
            Ok(n > 0)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{Artifact, FORMAT_NPM, FORMAT_RAW, Repository, TYPE_HOSTED};
    use crate::testing::meta::test_store;

    #[tokio::test]
    async fn blob_usage_splits_by_repository_format() {
        let (s, _dir) = test_store().await;
        let mut repo_ids = Vec::new();
        for (name, format) in [
            ("npm-a", FORMAT_NPM),
            ("npm-b", FORMAT_NPM),
            ("files", FORMAT_RAW),
        ] {
            let repo = s
                .create_repository(Repository {
                    name: name.to_string(),
                    format: format.to_string(),
                    r#type: TYPE_HOSTED.to_string(),
                    ..Default::default()
                })
                .await
                .unwrap();
            repo_ids.push(repo.id);
        }
        let now = Utc::now();
        // The npm blob is shared by two npm repositories and also stored in the
        // raw one, so it must count once, under npm.
        for (repo_id, path, sha, size) in [
            (repo_ids[0], "a/-/a-1.tgz", "shared", 100),
            (repo_ids[1], "a/-/a-1.tgz", "shared", 100),
            (repo_ids[2], "copy.tgz", "shared", 100),
            (repo_ids[2], "big.bin", "raw-only", 300),
        ] {
            s.put_artifact(Artifact {
                repo_id,
                path: path.to_string(),
                blob_sha256: sha.to_string(),
                size,
                cached_at: now,
                last_accessed_at: now,
                ..Default::default()
            })
            .await
            .unwrap();
        }

        let parts = s.blob_usage_by_format().await.unwrap();
        assert_eq!(
            parts,
            vec![
                FormatUsage {
                    format: FORMAT_RAW.to_string(),
                    blob_count: 1,
                    bytes: 300,
                },
                FormatUsage {
                    format: FORMAT_NPM.to_string(),
                    blob_count: 1,
                    bytes: 100,
                },
            ]
        );
        let (_, total) = s.blob_stats().await.unwrap();
        assert_eq!(parts.iter().map(|p| p.bytes).sum::<i64>(), total);
    }
}
