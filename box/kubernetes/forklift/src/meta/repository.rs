//! Repository rows: create, lookup, list, config/description/rename updates, the online/offline
//! toggle and cascading delete.

use rusqlite::{Connection, Row, params};

use super::publication::{clear_group_metadata_cache, invalidate_group_caches_for_member};
use super::{Error, Repository, Result, Store, now_rfc3339, parse_time};

impl Store {
    /// Inserts a repository and returns it with its assigned ID.
    pub async fn create_repository(&self, mut r: Repository) -> Result<Repository> {
        let id = self
            .write(move |conn| {
                let now = now_rfc3339();
                if r.config_json.is_empty() {
                    r.config_json = "{}".to_string();
                }
                conn.execute(
                    "INSERT INTO repositories(name, format, type, upstream_url, config_json, description, created_at, updated_at)
         VALUES(?, ?, ?, ?, ?, ?, ?, ?)",
                    params![
                        r.name,
                        r.format,
                        r.r#type,
                        r.upstream_url,
                        r.config_json,
                        r.description,
                        now,
                        now
                    ],
                )
                .map_err(|e| Error::sqlite("create repository", e))?;
                Ok(conn.last_insert_rowid())
            })
            .await?;
        self.get_repository(id).await
    }

    /// Returns a repository by ID.
    pub async fn get_repository(&self, id: i64) -> Result<Repository> {
        self.read(move |conn| {
            conn.query_row(
                "SELECT id, name, format, type, upstream_url, config_json, description, created_at, updated_at, disabled
         FROM repositories WHERE id = ?",
                params![id],
                scan_repository,
            )
            .map_err(|e| Error::sqlite("get repository", e))
        })
        .await
    }

    /// Returns a repository by name.
    pub async fn get_repository_by_name(&self, name: &str) -> Result<Repository> {
        let name = name.to_string();
        self.read(move |conn| {
            conn.query_row(
                "SELECT id, name, format, type, upstream_url, config_json, description, created_at, updated_at, disabled
         FROM repositories WHERE name = ?",
                params![name],
                scan_repository,
            )
            .map_err(|e| Error::sqlite("get repository by name", e))
        })
        .await
    }

    /// Returns all repositories ordered by name.
    pub async fn list_repositories(&self) -> Result<Vec<Repository>> {
        self.read(|conn| {
            query_all(
                conn,
                "list repositories",
                "SELECT id, name, format, type, upstream_url, config_json, description, created_at, updated_at, disabled
         FROM repositories ORDER BY name",
                [],
                scan_repository,
            )
        })
        .await
    }

    /// Updates the upstream URL and config JSON of a repository.
    pub async fn update_repository_config(
        &self,
        id: i64,
        upstream_url: &str,
        config_json: &str,
    ) -> Result<()> {
        let upstream_url = upstream_url.to_string();
        let config_json = config_json.to_string();
        self.write(move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| Error::sqlite("begin update repository config", e))?;
            clear_group_metadata_cache(&tx, id)?;
            invalidate_group_caches_for_member(&tx, id, "")?;
            let n = tx
                .execute(
                    "UPDATE repositories SET upstream_url = ?, config_json = ?, updated_at = ? WHERE id = ?",
                    params![upstream_url, config_json, now_rfc3339(), id],
                )
                .map_err(|e| Error::sqlite("update repository config", e))?;
            ensure_affected(n)?;
            tx.commit()
                .map_err(|e| Error::sqlite("commit update repository config", e))
        })
        .await
    }

    /// Returns the number of repositories, for the public landing statistics.
    pub async fn count_repositories(&self) -> Result<i64> {
        self.read(|conn| {
            conn.query_row("SELECT COUNT(*) FROM repositories", [], |r| r.get(0))
                .map_err(|e| Error::sqlite("count repositories", e))
        })
        .await
    }

    /// Changes a repository's name. Reserved for seed-definition renames: the
    /// name is otherwise a stable identity (permissions, approvals and audit
    /// records key on it), so no API route exposes this.
    pub async fn rename_repository(&self, id: i64, name: &str) -> Result<()> {
        let name = name.to_string();
        self.write(move |conn| {
            let n = conn
                .execute(
                    "UPDATE repositories SET name = ?, updated_at = ? WHERE id = ?",
                    params![name, now_rfc3339(), id],
                )
                .map_err(|e| Error::sqlite("rename repository", e))?;
            ensure_affected(n)
        })
        .await
    }

    /// Replaces the free-text description.
    pub async fn update_repository_description(&self, id: i64, description: &str) -> Result<()> {
        let description = description.to_string();
        self.write(move |conn| {
            let n = conn
                .execute(
                    "UPDATE repositories SET description = ?, updated_at = ? WHERE id = ?",
                    params![description, now_rfc3339(), id],
                )
                .map_err(|e| Error::sqlite("update repository description", e))?;
            ensure_affected(n)
        })
        .await
    }

    /// Removes a repository. Artifacts cascade; blob ref counts are decremented
    /// first so unreferenced blobs can be garbage-collected.
    pub async fn delete_repository(&self, id: i64) -> Result<()> {
        self.write(move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| Error::sqlite("begin delete repository", e))?;
            clear_group_metadata_cache(&tx, id)?;
            invalidate_group_caches_for_member(&tx, id, "")?;
            tx.execute(
                "UPDATE blobs SET ref_count = ref_count - 1,
             unreferenced_since = CASE WHEN ref_count - 1 <= 0
                 THEN COALESCE(unreferenced_since, ?)
                 ELSE unreferenced_since END
         WHERE sha256 IN (SELECT blob_sha256 FROM artifacts WHERE repo_id = ?)",
                params![now_rfc3339(), id],
            )
            .map_err(|e| Error::sqlite("release repository blobs", e))?;
            let n = tx
                .execute("DELETE FROM repositories WHERE id = ?", params![id])
                .map_err(|e| Error::sqlite("delete repository", e))?;
            ensure_affected(n)?;
            tx.commit()
                .map_err(|e| Error::sqlite("commit delete repository", e))
        })
        .await
    }

    /// Toggles a repository's online/offline state.
    pub async fn set_repository_disabled(&self, id: i64, disabled: bool) -> Result<()> {
        self.write(move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| Error::sqlite("begin set repository disabled", e))?;
            invalidate_group_caches_for_member(&tx, id, "")?;
            let n = tx
                .execute(
                    "UPDATE repositories SET disabled = ?, updated_at = ? WHERE id = ?",
                    params![i64::from(disabled), now_rfc3339(), id],
                )
                .map_err(|e| Error::sqlite("set repository disabled", e))?;
            ensure_affected(n)?;
            tx.commit()
                .map_err(|e| Error::sqlite("commit set repository disabled", e))
        })
        .await
    }
}

/// Decodes one row in the column order every repository SELECT uses.
fn scan_repository(row: &Row<'_>) -> rusqlite::Result<Repository> {
    let created: String = row.get(7)?;
    let updated: String = row.get(8)?;
    let disabled: i64 = row.get(9)?;
    Ok(Repository {
        id: row.get(0)?,
        name: row.get(1)?,
        format: row.get(2)?,
        r#type: row.get(3)?,
        upstream_url: row.get(4)?,
        config_json: row.get(5)?,
        description: row.get(6)?,
        created_at: parse_time(&created),
        updated_at: parse_time(&updated),
        disabled: disabled != 0,
    })
}

/// Turns a zero rows-affected count into [`Error::NotFound`], the way every
/// targeted UPDATE/DELETE reports a missing row.
pub(crate) fn ensure_affected(n: usize) -> Result<()> {
    if n == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
}

/// Runs a SELECT and decodes every row with `f`, wrapping any failure as `Error::sqlite(op,
/// ..)`.
pub(crate) fn query_all<T, P, F>(
    conn: &Connection,
    op: &'static str,
    sql: &str,
    params: P,
    f: F,
) -> Result<Vec<T>>
where
    P: rusqlite::Params,
    F: FnMut(&Row<'_>) -> rusqlite::Result<T>,
{
    let mut stmt = conn.prepare(sql).map_err(|e| Error::sqlite(op, e))?;
    let rows = stmt
        .query_map(params, f)
        .map_err(|e| Error::sqlite(op, e))?;
    rows.collect::<rusqlite::Result<Vec<T>>>()
        .map_err(|e| Error::sqlite(op, e))
}
