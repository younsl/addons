//! Blob reference records: lookup, aggregate stats, GC candidate listing, replication paging
//! and the guarded record delete.

use chrono::{DateTime, Utc};
use rusqlite::params;

use super::repository::query_all;
use super::{Blob, Error, Result, Store, format_time, parse_time};

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
