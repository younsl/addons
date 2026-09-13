//! Per-repository audit log.

use chrono::{DateTime, Utc};
use rusqlite::{Row, params, params_from_iter, types::Value};

use super::time::{format_time, is_zero, parse_time};
use super::{Error, Result, Store};

/// One recorded repository event: artifact traffic (download, upload, delete)
/// or a repository configuration change (repo.create, repo.update,
/// repo.delete).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuditLog {
    pub id: i64,
    pub repo_name: String,
    pub event: String,
    pub path: String,
    /// empty = anonymous
    pub username: String,
    pub method: String,
    pub status: i64,
    pub client_ip: String,
    pub user_agent: String,
    pub request_id: String,
    pub detail_json: String,
    /// When the event happened.
    pub created_at: DateTime<Utc>,
}

// Audit event constants.
pub const EVENT_DOWNLOAD: &str = "download";
pub const EVENT_UPLOAD: &str = "upload";
pub const EVENT_DELETE: &str = "delete";
/// console read/browse: repo detail or artifact listing
pub const EVENT_VIEW: &str = "view";
pub const EVENT_REPO_CREATE: &str = "repo.create";
pub const EVENT_REPO_UPDATE: &str = "repo.update";
pub const EVENT_REPO_DELETE: &str = "repo.delete";
/// artifact auto-deleted by the idle retention reaper
pub const EVENT_TTL_EXPIRE: &str = "ttl.expire";
/// request blocked by the vulnerability policy
pub const EVENT_VULN_BLOCK: &str = "vuln.block";
/// request blocked by the license policy
pub const EVENT_LICENSE_BLOCK: &str = "license.block";
pub const EVENT_UPLOAD_METADATA: &str = "upload.metadata";
pub const EVENT_UPLOAD_REJECT: &str = "upload.reject";
pub const EVENT_UPLOAD_CANCEL: &str = "upload.cancel";
/// Artifact labels: an operator tag added or removed on one stored path. A
/// refused attempt is recorded too (status 403), so the trail shows who tried
/// to label what, not only who succeeded.
pub const EVENT_ARTIFACT_LABEL_ADD: &str = "artifact.label.add";
pub const EVENT_ARTIFACT_LABEL_REMOVE: &str = "artifact.label.remove";

const INSERT_AUDIT_LOG: &str = "INSERT INTO audit_logs(repo_name, event, path, username, method, status, client_ip, user_agent, request_id, detail_json, created_at)
		 VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

/// The insert's `created_at` text: the entry's own time, or now when unset.
fn created_text(l: &AuditLog) -> String {
    if is_zero(l.created_at) {
        format_time(Utc::now())
    } else {
        format_time(l.created_at)
    }
}

impl Store {
    /// Appends one audit log entry. `created_at` defaults to now when unset.
    pub async fn insert_audit_log(&self, l: AuditLog) -> Result<()> {
        self.write(move |conn| {
            conn.execute(
                INSERT_AUDIT_LOG,
                params![
                    l.repo_name,
                    l.event,
                    l.path,
                    l.username,
                    l.method,
                    l.status,
                    l.client_ip,
                    l.user_agent,
                    l.request_id,
                    l.detail_json,
                    created_text(&l)
                ],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("insert audit log", e))
        })
        .await
    }

    /// Appends logs in one transaction. The audit recorder drains its buffer
    /// into batches so a request burst costs one write-connection acquisition
    /// and one fsync per batch instead of one per event; that is what keeps the
    /// buffer from overflowing (and dropping events) while the single write
    /// connection is busy with artifact writes. Rows are inserted in slice
    /// order, so ids follow arrival order as before. `created_at` defaults to
    /// now when unset.
    pub async fn insert_audit_logs(&self, logs: Vec<AuditLog>) -> Result<()> {
        if logs.is_empty() {
            return Ok(());
        }
        self.write(move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| Error::sqlite("begin audit batch", e))?;
            {
                let mut stmt = tx
                    .prepare(INSERT_AUDIT_LOG)
                    .map_err(|e| Error::sqlite("prepare audit batch", e))?;
                for l in &logs {
                    stmt.execute(params![
                        l.repo_name,
                        l.event,
                        l.path,
                        l.username,
                        l.method,
                        l.status,
                        l.client_ip,
                        l.user_agent,
                        l.request_id,
                        l.detail_json,
                        created_text(l)
                    ])
                    .map_err(|e| Error::sqlite("insert audit log", e))?;
                }
            }
            tx.commit()
                .map_err(|e| Error::sqlite("commit audit batch", e))
        })
        .await
    }

    /// Counts recorded successful GET responses in [since, until] for the
    /// requested repository-relative paths. Retained audit history is the source;
    /// requests through a group are recorded against that group, not its members.
    pub async fn artifact_download_counts(
        &self,
        repo_name: &str,
        paths: Vec<String>,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<std::collections::HashMap<String, i64>> {
        if paths.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let repo_name = repo_name.to_string();
        self.read(move |conn| {
            let placeholders = vec!["?"; paths.len()].join(",");
            let sql = format!(
                "SELECT path, COUNT(*) FROM audit_logs
                 WHERE repo_name = ? AND created_at >= ? AND created_at <= ?
                   AND event = 'download' AND method = 'GET' AND status IN (200, 206)
                   AND path IN ({placeholders}) GROUP BY path"
            );
            let mut args = vec![
                Value::Text(repo_name),
                Value::Text(format_time(since)),
                Value::Text(format_time(until)),
            ];
            args.extend(paths.into_iter().map(Value::Text));
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| Error::sqlite("prepare artifact downloads", e))?;
            stmt.query_map(params_from_iter(args.iter()), |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(|e| Error::sqlite("query artifact downloads", e))?
            .collect::<rusqlite::Result<std::collections::HashMap<String, i64>>>()
            .map_err(|e| Error::sqlite("read artifact downloads", e))
        })
        .await
    }

    /// Returns a repository's audit log entries, newest first. `event` filters
    /// to one event type when non-empty.
    pub async fn list_audit_logs(
        &self,
        repo_name: &str,
        event: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<AuditLog>> {
        let repo_name = repo_name.to_string();
        let event = event.to_string();
        self.read(move |conn| {
            let mut q = String::from(
                "SELECT id, repo_name, event, path, username, method, status, client_ip, user_agent, request_id, detail_json, created_at
          FROM audit_logs WHERE repo_name = ?",
            );
            let mut args: Vec<Value> = vec![Value::Text(repo_name)];
            if !event.is_empty() {
                q.push_str(" AND event = ?");
                args.push(Value::Text(event));
            }
            q.push_str(" ORDER BY id DESC LIMIT ? OFFSET ?");
            args.push(Value::Integer(limit));
            args.push(Value::Integer(offset));

            let mut stmt = conn
                .prepare(&q)
                .map_err(|e| Error::sqlite("list audit logs", e))?;
            let out = stmt
                .query_map(params_from_iter(args.iter()), scan_audit_log)
                .map_err(|e| Error::sqlite("list audit logs", e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::sqlite("scan audit log", e))?;
            Ok(out)
        })
        .await
    }

    /// Returns the number of audit log entries for a repository, optionally
    /// filtered to one event type.
    pub async fn count_audit_logs(&self, repo_name: &str, event: &str) -> Result<i64> {
        let repo_name = repo_name.to_string();
        let event = event.to_string();
        self.read(move |conn| {
            let mut q = String::from("SELECT COUNT(*) FROM audit_logs WHERE repo_name = ?");
            let mut args: Vec<Value> = vec![Value::Text(repo_name)];
            if !event.is_empty() {
                q.push_str(" AND event = ?");
                args.push(Value::Text(event));
            }
            conn.query_row(&q, params_from_iter(args.iter()), |r| r.get::<_, i64>(0))
                .map_err(|e| Error::sqlite("count audit logs", e))
        })
        .await
    }

    /// Deletes entries older than `before` and reports how many rows were
    /// removed. Used by the retention loop.
    pub async fn prune_audit_logs(&self, before: DateTime<Utc>) -> Result<i64> {
        self.write(move |conn| {
            let n = conn
                .execute(
                    "DELETE FROM audit_logs WHERE created_at < ?",
                    params![format_time(before)],
                )
                .map_err(|e| Error::sqlite("prune audit logs", e))?;
            Ok(n as i64)
        })
        .await
    }
}

fn scan_audit_log(r: &Row<'_>) -> rusqlite::Result<AuditLog> {
    let created: String = r.get(11)?;
    Ok(AuditLog {
        id: r.get(0)?,
        repo_name: r.get(1)?,
        event: r.get(2)?,
        path: r.get(3)?,
        username: r.get(4)?,
        method: r.get(5)?,
        status: r.get(6)?,
        client_ip: r.get(7)?,
        user_agent: r.get(8)?,
        request_id: r.get(9)?,
        detail_json: r.get(10)?,
        created_at: parse_time(&created),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use chrono::{Duration, Utc};

    use crate::meta::*;

    #[tokio::test]
    async fn artifact_download_counts_window_and_response_filters() {
        let (s, _dir) = test_store().await;
        let until = Utc::now();
        let since = until - Duration::days(30);
        let mut logs = Vec::new();
        for (repo, path, event, method, status, time) in [
            ("r", "a", EVENT_DOWNLOAD, "GET", 200, since),
            ("r", "a", EVENT_DOWNLOAD, "GET", 206, until),
            ("r", "b", EVENT_DOWNLOAD, "GET", 200, until),
            ("other", "a", EVENT_DOWNLOAD, "GET", 200, until),
            ("r", "a", EVENT_DOWNLOAD, "HEAD", 200, until),
            ("r", "a", EVENT_DOWNLOAD, "GET", 304, until),
            ("r", "a", EVENT_DOWNLOAD, "GET", 404, until),
            ("r", "a", EVENT_DOWNLOAD, "GET", 500, until),
            ("r", "a", EVENT_UPLOAD, "GET", 200, until),
            (
                "r",
                "a",
                EVENT_DOWNLOAD,
                "GET",
                200,
                since - Duration::seconds(1),
            ),
            (
                "r",
                "a",
                EVENT_DOWNLOAD,
                "GET",
                200,
                until + Duration::seconds(1),
            ),
        ] {
            logs.push(AuditLog {
                repo_name: repo.into(),
                path: path.into(),
                event: event.into(),
                method: method.into(),
                status,
                created_at: time,
                ..Default::default()
            });
        }
        s.insert_audit_logs(logs).await.unwrap();
        let counts = s
            .artifact_download_counts("r", vec!["a".into(), "missing".into()], since, until)
            .await
            .unwrap();
        assert_eq!(counts, std::collections::HashMap::from([("a".into(), 2)]));
        assert!(
            s.artifact_download_counts("r", vec![], since, until)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn audit_log_insert_list_count() {
        let (s, _dir) = test_store().await;

        let entries = vec![
            AuditLog {
                repo_name: "maven-central".into(),
                event: EVENT_DOWNLOAD.into(),
                path: "com/acme/app/1.0/app-1.0.jar".into(),
                username: "alice".into(),
                method: "GET".into(),
                status: 200,
                client_ip: "10.0.0.1".into(),
                user_agent: "maven".into(),
                ..AuditLog::default()
            },
            AuditLog {
                repo_name: "maven-central".into(),
                event: EVENT_DOWNLOAD.into(),
                path: "com/acme/app/2.0/app-2.0.jar".into(),
                status: 404,
                ..AuditLog::default()
            },
            AuditLog {
                repo_name: "maven-central".into(),
                event: EVENT_UPLOAD.into(),
                path: "com/acme/app/3.0/app-3.0.jar".into(),
                username: "bob".into(),
                method: "PUT".into(),
                status: 201,
                ..AuditLog::default()
            },
            AuditLog {
                repo_name: "npm-proxy".into(),
                event: EVENT_DOWNLOAD.into(),
                path: "react/-/react-18.0.0.tgz".into(),
                status: 200,
                ..AuditLog::default()
            },
        ];
        for e in entries {
            s.insert_audit_log(e).await.expect("insert");
        }

        let logs = s
            .list_audit_logs("maven-central", "", 100, 0)
            .await
            .expect("list");
        assert_eq!(logs.len(), 3, "len, want 3");
        // Newest first.
        assert_eq!(logs[0].event, EVENT_UPLOAD, "first log = {:?}", logs[0]);
        assert_eq!(logs[0].username, "bob", "first log = {:?}", logs[0]);
        assert_eq!(logs[2].client_ip, "10.0.0.1", "oldest log = {:?}", logs[2]);
        assert_eq!(logs[2].user_agent, "maven", "oldest log = {:?}", logs[2]);
        assert!(!time::is_zero(logs[0].created_at), "created_at not set");

        // Event filter.
        let logs = s
            .list_audit_logs("maven-central", EVENT_DOWNLOAD, 100, 0)
            .await
            .expect("list filtered");
        assert_eq!(logs.len(), 2, "filtered len, want 2");

        // Pagination.
        let logs = s
            .list_audit_logs("maven-central", "", 1, 1)
            .await
            .expect("list paginated");
        assert_eq!(logs.len(), 1, "paginated = {logs:?}");
        assert_eq!(logs[0].status, 404, "paginated = {logs:?}");

        let n = s.count_audit_logs("maven-central", "").await.unwrap();
        assert_eq!(n, 3, "count, want 3");
        let n = s
            .count_audit_logs("maven-central", EVENT_UPLOAD)
            .await
            .unwrap();
        assert_eq!(n, 1, "count uploads, want 1");
    }

    #[tokio::test]
    async fn audit_log_prune() {
        let (s, _dir) = test_store().await;

        let old = Utc::now() - Duration::hours(48);
        s.insert_audit_log(AuditLog {
            repo_name: "r".into(),
            event: EVENT_DOWNLOAD.into(),
            created_at: old,
            ..AuditLog::default()
        })
        .await
        .unwrap();
        s.insert_audit_log(AuditLog {
            repo_name: "r".into(),
            event: EVENT_DOWNLOAD.into(),
            ..AuditLog::default()
        })
        .await
        .unwrap();

        let n = s
            .prune_audit_logs(Utc::now() - Duration::hours(24))
            .await
            .expect("prune");
        assert_eq!(n, 1, "pruned, want 1");
        let left = s.count_audit_logs("r", "").await.unwrap();
        assert_eq!(left, 1, "remaining, want 1");
    }

    #[tokio::test]
    async fn audit_logs_batch_insert_keeps_arrival_order() {
        let (s, _dir) = test_store().await;
        s.insert_audit_logs(Vec::new()).await.unwrap();
        assert_eq!(s.count_audit_logs("r", "").await.unwrap(), 0);

        s.insert_audit_logs(vec![
            AuditLog {
                repo_name: "r".into(),
                event: EVENT_DOWNLOAD.into(),
                path: "first".into(),
                ..AuditLog::default()
            },
            AuditLog {
                repo_name: "r".into(),
                event: EVENT_UPLOAD.into(),
                path: "second".into(),
                ..AuditLog::default()
            },
        ])
        .await
        .unwrap();

        let logs = s.list_audit_logs("r", "", 10, 0).await.unwrap();
        assert_eq!(logs.len(), 2);
        // Newest first: the second entry got the higher id.
        assert_eq!(logs[0].path, "second");
        assert_eq!(logs[1].path, "first");
    }
}
