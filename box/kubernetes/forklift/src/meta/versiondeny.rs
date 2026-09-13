//! Per-version deny list (quarantine v2) for proxy repositories.

use chrono::{DateTime, Utc};
use rusqlite::{Row, params, params_from_iter, types::Value};

use super::time::{now_rfc3339, parse_time};
use super::{Error, Result, Store};

/// One per-version deny entry for a proxy repository: the exact (package,
/// version) is blocked regardless of the package's approval status. The
/// package string follows the same canonical per-format convention as
/// [`super::PackageApproval`]; the version is the exact string seen in request
/// paths (go modules keep the "v" prefix).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VersionDeny {
    pub id: i64,
    pub repo_name: String,
    pub package: String,
    pub version: String,
    pub reason: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

// Version-deny audit event constants.
pub const EVENT_DENY_CREATE: &str = "deny.create";
pub const EVENT_DENY_DELETE: &str = "deny.delete";
pub const EVENT_DENY_BLOCK: &str = "deny.block";

const DENY_COLS: &str = "id, repo_name, package, version, reason, created_by, created_at";

impl Store {
    /// Reports whether an exact (package, version) is denied in a repository.
    /// Hot path for the approval gate: a single point read on the
    /// UNIQUE(repo_name, package, version) index.
    pub async fn is_version_denied(
        &self,
        repo_name: &str,
        pkg: &str,
        version: &str,
    ) -> Result<bool> {
        let repo_name = repo_name.to_string();
        let pkg = pkg.to_string();
        let version = version.to_string();
        self.read(move |conn| {
            match conn.query_row(
                "SELECT 1 FROM version_denies WHERE repo_name = ? AND package = ? AND version = ?",
                params![repo_name, pkg, version],
                |r| r.get::<_, i64>(0),
            ) {
                Ok(_) => Ok(true),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
                Err(e) => Err(Error::sqlite("is version denied", e)),
            }
        })
        .await
    }

    /// Creates a deny entry, or refreshes reason/created_by when the same (repo,
    /// package, version) is denied again (idempotent re-deny).
    pub async fn upsert_version_deny(
        &self,
        repo_name: &str,
        pkg: &str,
        version: &str,
        reason: &str,
        created_by: &str,
    ) -> Result<VersionDeny> {
        let repo_name = repo_name.to_string();
        let pkg = pkg.to_string();
        let version = version.to_string();
        let reason = reason.to_string();
        let created_by = created_by.to_string();
        self.write(move |conn| {
            conn.query_row(
                &format!(
                    "INSERT INTO version_denies(repo_name, package, version, reason, created_by, created_at)
         VALUES(?, ?, ?, ?, ?, ?)
         ON CONFLICT(repo_name, package, version) DO UPDATE SET
             reason = excluded.reason,
             created_by = excluded.created_by
         RETURNING {DENY_COLS}"
                ),
                params![repo_name, pkg, version, reason, created_by, now_rfc3339()],
                scan_version_deny,
            )
            .map_err(|e| Error::sqlite("upsert version deny", e))
        })
        .await
    }

    /// Returns one deny entry by id.
    pub async fn get_version_deny(&self, id: i64) -> Result<VersionDeny> {
        self.read(move |conn| {
            conn.query_row(
                &format!("SELECT {DENY_COLS} FROM version_denies WHERE id = ?"),
                params![id],
                scan_version_deny,
            )
            .map_err(|e| Error::sqlite("get version deny", e))
        })
        .await
    }

    /// Returns deny entries, newest first. `repo_name` is an optional filter.
    pub async fn list_version_denies(
        &self,
        repo_name: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<VersionDeny>> {
        let repo_name = repo_name.to_string();
        self.read(move |conn| {
            let mut q = format!("SELECT {DENY_COLS} FROM version_denies WHERE 1=1");
            let mut args: Vec<Value> = Vec::new();
            if !repo_name.is_empty() {
                q.push_str(" AND repo_name = ?");
                args.push(Value::Text(repo_name));
            }
            q.push_str(" ORDER BY id DESC LIMIT ? OFFSET ?");
            args.push(Value::Integer(limit));
            args.push(Value::Integer(offset));

            let mut stmt = conn
                .prepare(&q)
                .map_err(|e| Error::sqlite("list version denies", e))?;
            let out = stmt
                .query_map(params_from_iter(args.iter()), scan_version_deny)
                .map_err(|e| Error::sqlite("list version denies", e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::sqlite("scan version deny", e))?;
            Ok(out)
        })
        .await
    }

    /// Returns the number of deny entries matching the optional `repo_name`
    /// filter.
    pub async fn count_version_denies(&self, repo_name: &str) -> Result<i64> {
        let repo_name = repo_name.to_string();
        self.read(move |conn| {
            let mut q = String::from("SELECT COUNT(*) FROM version_denies WHERE 1=1");
            let mut args: Vec<Value> = Vec::new();
            if !repo_name.is_empty() {
                q.push_str(" AND repo_name = ?");
                args.push(Value::Text(repo_name));
            }
            conn.query_row(&q, params_from_iter(args.iter()), |r| r.get::<_, i64>(0))
                .map_err(|e| Error::sqlite("count version denies", e))
        })
        .await
    }

    /// Removes one deny entry (un-deny). The next request for the version goes
    /// back through the regular approval/age gates.
    pub async fn delete_version_deny(&self, id: i64) -> Result<()> {
        self.write(move |conn| {
            let n = conn
                .execute("DELETE FROM version_denies WHERE id = ?", params![id])
                .map_err(|e| Error::sqlite("delete version deny", e))?;
            if n == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
        .await
    }

    /// Removes all deny entries for a repository. Called on repository deletion
    /// so a recreated same-name repo does not inherit old deny decisions.
    pub async fn delete_version_denies_for_repo(&self, repo_name: &str) -> Result<()> {
        let repo_name = repo_name.to_string();
        self.write(move |conn| {
            conn.execute(
                "DELETE FROM version_denies WHERE repo_name = ?",
                params![repo_name],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("delete version denies for repo", e))
        })
        .await
    }
}

/// Reads one deny row (the `DENY_COLS` projection).
fn scan_version_deny(r: &Row<'_>) -> rusqlite::Result<VersionDeny> {
    let created: String = r.get(6)?;
    Ok(VersionDeny {
        id: r.get(0)?,
        repo_name: r.get(1)?,
        package: r.get(2)?,
        version: r.get(3)?,
        reason: r.get(4)?,
        created_by: r.get(5)?,
        created_at: parse_time(&created),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::meta::*;

    #[tokio::test]
    async fn version_deny_crud() {
        let (s, _dir) = test_store().await;

        let denied = s
            .is_version_denied("npmjs", "lodash", "4.17.99")
            .await
            .unwrap();
        assert!(!denied, "empty table");

        let d = s
            .upsert_version_deny("npmjs", "lodash", "4.17.99", "IOC", "alice")
            .await
            .unwrap();
        assert_ne!(d.id, 0, "created deny = {d:?}");
        assert_eq!(d.reason, "IOC", "created deny = {d:?}");
        assert_eq!(d.created_by, "alice", "created deny = {d:?}");
        assert!(!time::is_zero(d.created_at), "created deny = {d:?}");

        let denied = s
            .is_version_denied("npmjs", "lodash", "4.17.99")
            .await
            .unwrap();
        assert!(denied, "want true");
        // Exact-version semantics: other versions, packages and repos stay open.
        for c in [
            ["npmjs", "lodash", "4.17.21"],
            ["npmjs", "left-pad", "4.17.99"],
            ["npm-internal", "lodash", "4.17.99"],
        ] {
            let denied = s.is_version_denied(c[0], c[1], c[2]).await.unwrap();
            assert!(!denied, "{c:?} unexpectedly denied");
        }

        // Re-deny is idempotent and refreshes reason/author, keeping the same row.
        let d2 = s
            .upsert_version_deny("npmjs", "lodash", "4.17.99", "CVE-2026-0001", "bob")
            .await
            .unwrap();
        assert_eq!(d2.id, d.id, "re-deny = {d2:?}, want same id");
        assert_eq!(d2.reason, "CVE-2026-0001", "re-deny = {d2:?}");
        assert_eq!(d2.created_by, "bob", "re-deny = {d2:?}");

        let got = s.get_version_deny(d.id).await.unwrap();
        assert_eq!(got.package, "lodash", "get = {got:?}");
        assert_eq!(got.version, "4.17.99", "get = {got:?}");

        s.delete_version_deny(d.id).await.unwrap();
        let denied = s
            .is_version_denied("npmjs", "lodash", "4.17.99")
            .await
            .unwrap();
        assert!(!denied, "deleted deny still blocks");
        let err = s.delete_version_deny(d.id).await.unwrap_err();
        assert!(err.is_not_found(), "double delete err = {err}");
        let err = s.get_version_deny(d.id).await.unwrap_err();
        assert!(err.is_not_found(), "get deleted err = {err}");
    }

    #[tokio::test]
    async fn version_deny_list_and_repo_cleanup() {
        let (s, _dir) = test_store().await;

        let seed = [
            ["npmjs", "lodash", "4.17.99"],
            ["npmjs", "left-pad", "1.3.0"],
            ["pypi-proxy", "requests", "2.99.0"],
        ];
        for c in seed {
            s.upsert_version_deny(c[0], c[1], c[2], "", "sec")
                .await
                .unwrap();
        }

        let all = s.list_version_denies("", 10, 0).await.unwrap();
        assert_eq!(all.len(), 3, "list all, want 3");
        // Newest first.
        assert_eq!(all[0].package, "requests", "order: first");
        let scoped = s.list_version_denies("npmjs", 10, 0).await.unwrap();
        assert_eq!(scoped.len(), 2, "list npmjs, want 2");
        assert_eq!(
            s.count_version_denies("").await.unwrap(),
            3,
            "count all, want 3"
        );
        assert_eq!(
            s.count_version_denies("pypi-proxy").await.unwrap(),
            1,
            "count pypi-proxy, want 1"
        );
        // Pagination.
        let page = s.list_version_denies("", 2, 2).await.unwrap();
        assert_eq!(page.len(), 1, "page, want 1");

        s.delete_version_denies_for_repo("npmjs").await.unwrap();
        assert_eq!(
            s.count_version_denies("").await.unwrap(),
            1,
            "after repo cleanup count, want 1"
        );
    }
}
