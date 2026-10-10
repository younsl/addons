//! OCI tags (mutable pointers onto immutable manifest artifacts) and blob push sessions.

use chrono::{DateTime, Utc};
use rusqlite::{Row, params};

use super::repository::query_all;
use super::{Error, Result, Store, format_time, now_rfc3339, parse_time};

/// A mutable pointer from (repository, OCI name, tag) to a manifest digest.
/// See migration 0030: manifests are immutable artifact rows, tags are the
/// moving identity on top of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OCITag {
    pub repo_id: i64,
    pub name: String,
    pub tag: String,
    pub manifest_digest: String,
    pub updated_at: DateTime<Utc>,
}

/// A blob push in progress: bytes accumulate in a file named by `id` under the
/// OCI upload directory, and `offset` is how many bytes have been appended so
/// far. Persisted so the requests of one push may hit different replicas (see
/// migration 0031).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OCIUploadSession {
    pub id: String,
    pub repo_id: i64,
    pub name: String,
    pub offset: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Store {
    /// Points (`repo_id`, `name`, `tag`) at `digest`, atomically replacing any
    /// previous target. Re-tagging is a single row write, so a tag never
    /// observably points at nothing.
    pub async fn upsert_oci_tag(
        &self,
        repo_id: i64,
        name: &str,
        tag: &str,
        digest: &str,
    ) -> Result<()> {
        let (name, tag, digest) = (name.to_string(), tag.to_string(), digest.to_string());
        self.write(move |conn| {
            let now = now_rfc3339();
            conn.execute(
                "INSERT INTO oci_tags(repo_id, name, tag, manifest_digest, updated_at) VALUES(?, ?, ?, ?, ?)
         ON CONFLICT(repo_id, name, tag) DO UPDATE SET
             manifest_digest = excluded.manifest_digest,
             updated_at = excluded.updated_at",
                params![repo_id, name, tag, digest, now],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("upsert oci tag", e))
        })
        .await
    }

    /// Resolves one tag. [`Error::NotFound`] when the tag does not exist.
    pub async fn get_oci_tag(&self, repo_id: i64, name: &str, tag: &str) -> Result<OCITag> {
        let (name, tag) = (name.to_string(), tag.to_string());
        self.read(move |conn| {
            conn.query_row(
                "SELECT repo_id, name, tag, manifest_digest, updated_at FROM oci_tags
         WHERE repo_id = ? AND name = ? AND tag = ?",
                params![repo_id, name, tag],
                scan_oci_tag,
            )
            .map_err(|e| Error::sqlite("get oci tag", e))
        })
        .await
    }

    /// Returns the tag names for one OCI name, lexically sorted (the order the
    /// tags/list endpoint must present).
    pub async fn list_oci_tags(&self, repo_id: i64, name: &str) -> Result<Vec<String>> {
        let name = name.to_string();
        self.read(move |conn| {
            query_all(
                conn,
                "list oci tags",
                "SELECT tag FROM oci_tags WHERE repo_id = ? AND name = ? ORDER BY tag",
                params![repo_id, name],
                |r| r.get(0),
            )
        })
        .await
    }

    /// Returns every tag row in a repository, for the prune pass to compute the
    /// tagged manifest set per name.
    pub async fn list_oci_tag_rows(&self, repo_id: i64) -> Result<Vec<OCITag>> {
        self.read(move |conn| {
            query_all(
                conn,
                "list oci tag rows",
                "SELECT repo_id, name, tag, manifest_digest, updated_at FROM oci_tags WHERE repo_id = ?",
                params![repo_id],
                scan_oci_tag,
            )
        })
        .await
    }

    /// Removes one tag. Deleting a tag never touches the manifest it pointed at
    /// (per the distribution spec); the prune pass collects manifests no tag
    /// reaches. Reports [`Error::NotFound`] when the tag does not exist.
    pub async fn delete_oci_tag(&self, repo_id: i64, name: &str, tag: &str) -> Result<()> {
        let (name, tag) = (name.to_string(), tag.to_string());
        self.write(move |conn| {
            let n = conn
                .execute(
                    "DELETE FROM oci_tags WHERE repo_id = ? AND name = ? AND tag = ?",
                    params![repo_id, name, tag],
                )
                .map_err(|e| Error::sqlite("delete oci tag", e))?;
            if n == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
        .await
    }

    /// Removes every tag pointing at a manifest digest, the spec behavior of
    /// `DELETE …/manifests/{digest}`.
    pub async fn delete_oci_tags_by_digest(
        &self,
        repo_id: i64,
        name: &str,
        digest: &str,
    ) -> Result<()> {
        let (name, digest) = (name.to_string(), digest.to_string());
        self.write(move |conn| {
            conn.execute(
                "DELETE FROM oci_tags WHERE repo_id = ? AND name = ? AND manifest_digest = ?",
                params![repo_id, name, digest],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("delete oci tags by digest", e))
        })
        .await
    }

    /// Opens a push session.
    pub async fn create_oci_upload_session(
        &self,
        id: &str,
        repo_id: i64,
        name: &str,
    ) -> Result<()> {
        let (id, name) = (id.to_string(), name.to_string());
        self.write(move |conn| {
            let now = now_rfc3339();
            conn.execute(
                "INSERT INTO oci_upload_sessions(id, repo_id, name, offset, created_at, updated_at)
         VALUES(?, ?, ?, 0, ?, ?)",
                params![id, repo_id, name, now, now],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("create oci upload session", e))
        })
        .await
    }

    /// Loads one session. [`Error::NotFound`] when absent (completed,
    /// cancelled, or expired).
    pub async fn get_oci_upload_session(&self, id: &str) -> Result<OCIUploadSession> {
        let id = id.to_string();
        self.read(move |conn| {
            conn.query_row(
                "SELECT id, repo_id, name, offset, created_at, updated_at FROM oci_upload_sessions WHERE id = ?",
                params![id],
                scan_oci_upload_session,
            )
            .map_err(|e| Error::sqlite("get oci upload session", e))
        })
        .await
    }

    /// Records how many bytes the session file now holds, guarded by the
    /// previous offset so two replicas appending concurrently cannot both win:
    /// the loser sees [`Error::NotFound`] and reports the conflict to its
    /// client.
    pub async fn set_oci_upload_session_offset(&self, id: &str, from: i64, to: i64) -> Result<()> {
        let id = id.to_string();
        self.write(move |conn| {
            let n = conn
                .execute(
                    "UPDATE oci_upload_sessions SET offset = ?, updated_at = ? WHERE id = ? AND offset = ?",
                    params![to, now_rfc3339(), id, from],
                )
                .map_err(|e| Error::sqlite("update oci upload session", e))?;
            if n == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
        .await
    }

    /// Reports how many pushes are currently in flight, for the sessions
    /// gauge.
    pub async fn count_oci_upload_sessions(&self) -> Result<i64> {
        self.read(|conn| {
            conn.query_row("SELECT COUNT(*) FROM oci_upload_sessions", [], |r| r.get(0))
                .map_err(|e| Error::sqlite("count oci upload sessions", e))
        })
        .await
    }

    /// Removes a session row (finalized, cancelled, or expired). The caller
    /// removes the session file.
    pub async fn delete_oci_upload_session(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        self.write(move |conn| {
            conn.execute("DELETE FROM oci_upload_sessions WHERE id = ?", params![id])
                .map(|_| ())
                .map_err(|e| Error::sqlite("delete oci upload session", e))
        })
        .await
    }

    /// Returns sessions untouched since `cutoff`, for the prune pass to expire
    /// with their temp files.
    pub async fn list_expired_oci_upload_sessions(
        &self,
        cutoff: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<OCIUploadSession>> {
        let cutoff = format_time(cutoff);
        self.read(move |conn| {
            query_all(
                conn,
                "list expired oci upload sessions",
                "SELECT id, repo_id, name, offset, created_at, updated_at FROM oci_upload_sessions
         WHERE updated_at < ? ORDER BY updated_at LIMIT ?",
                params![cutoff, limit],
                scan_oci_upload_session,
            )
        })
        .await
    }
}

fn scan_oci_tag(row: &Row<'_>) -> rusqlite::Result<OCITag> {
    let updated: String = row.get(4)?;
    Ok(OCITag {
        repo_id: row.get(0)?,
        name: row.get(1)?,
        tag: row.get(2)?,
        manifest_digest: row.get(3)?,
        updated_at: parse_time(&updated),
    })
}

fn scan_oci_upload_session(row: &Row<'_>) -> rusqlite::Result<OCIUploadSession> {
    let created: String = row.get(4)?;
    let updated: String = row.get(5)?;
    Ok(OCIUploadSession {
        id: row.get(0)?,
        repo_id: row.get(1)?,
        name: row.get(2)?,
        offset: row.get(3)?,
        created_at: parse_time(&created),
        updated_at: parse_time(&updated),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use chrono::{Duration, Utc};

    use crate::meta::*;

    /// A store with one OCI repository, the fixture both cases start from.
    async fn oci_test_store() -> (Store, tempfile::TempDir, Repository) {
        let (s, dir) = test_store().await;
        let repo = s
            .create_repository(Repository {
                name: "oci".into(),
                format: FORMAT_OCI.into(),
                r#type: TYPE_HOSTED.into(),
                ..Repository::default()
            })
            .await
            .unwrap();
        (s, dir, repo)
    }

    #[tokio::test]
    async fn oci_tag_crud() {
        let (s, _dir, repo) = oci_test_store().await;
        let d1 = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
        let d2 = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

        s.upsert_oci_tag(repo.id, "app", "v1", d1).await.unwrap();
        // Retag is a single-row replace.
        s.upsert_oci_tag(repo.id, "app", "v1", d2).await.unwrap();
        let tag = s
            .get_oci_tag(repo.id, "app", "v1")
            .await
            .expect("get after retag");
        assert_eq!(tag.manifest_digest, d2);
        s.upsert_oci_tag(repo.id, "app", "v2", d2).await.unwrap();
        s.upsert_oci_tag(repo.id, "other", "v1", d1).await.unwrap();

        let tags = s.list_oci_tags(repo.id, "app").await.unwrap();
        assert_eq!(tags, vec!["v1".to_string(), "v2".to_string()]);
        let rows = s.list_oci_tag_rows(repo.id).await.unwrap();
        assert_eq!(rows.len(), 3);

        // Deleting by digest removes every tag pointing at it, within the name.
        s.delete_oci_tags_by_digest(repo.id, "app", d2)
            .await
            .unwrap();
        let err = s.get_oci_tag(repo.id, "app", "v1").await.unwrap_err();
        assert!(err.is_not_found(), "v1 after digest delete: {err}");
        s.get_oci_tag(repo.id, "other", "v1")
            .await
            .expect("other name affected");
        s.delete_oci_tag(repo.id, "other", "v1").await.unwrap();
        let err = s.delete_oci_tag(repo.id, "other", "v1").await.unwrap_err();
        assert!(err.is_not_found(), "double delete = {err}");
    }

    #[tokio::test]
    async fn oci_upload_session_lifecycle() {
        let (s, _dir, repo) = oci_test_store().await;

        s.create_oci_upload_session("aaaa", repo.id, "app")
            .await
            .unwrap();
        let sess = s.get_oci_upload_session("aaaa").await.expect("get");
        assert_eq!((sess.offset, sess.name.as_str()), (0, "app"));
        assert_eq!(s.count_oci_upload_sessions().await.unwrap(), 1, "count");

        // Offset updates are guarded by the previous value.
        s.set_oci_upload_session_offset("aaaa", 0, 5).await.unwrap();
        let err = s
            .set_oci_upload_session_offset("aaaa", 0, 9)
            .await
            .unwrap_err();
        assert!(err.is_not_found(), "stale offset update = {err}");
        let sess = s.get_oci_upload_session("aaaa").await.unwrap();
        assert_eq!(sess.offset, 5);

        // Expiry listing keys on updated_at.
        let expired = s
            .list_expired_oci_upload_sessions(Utc::now() + Duration::hours(1), 10)
            .await
            .unwrap();
        assert_eq!(expired.len(), 1);
        let fresh = s
            .list_expired_oci_upload_sessions(Utc::now() - Duration::hours(1), 10)
            .await
            .unwrap();
        assert!(fresh.is_empty(), "fresh treated as expired");

        s.delete_oci_upload_session("aaaa").await.unwrap();
        let err = s.get_oci_upload_session("aaaa").await.unwrap_err();
        assert!(err.is_not_found(), "get after delete = {err}");
        assert_eq!(
            s.count_oci_upload_sessions().await.unwrap(),
            0,
            "count after delete"
        );
    }
}
