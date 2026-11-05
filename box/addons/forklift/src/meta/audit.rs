//! Per-repository audit log.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{OptionalExtension, Row, Transaction, params, params_from_iter, types::Value};

use super::time::{format_time, is_zero, parse_time};
use super::{Error, Result, Store};

/// One recorded repository event: artifact traffic (download, upload, delete)
/// or a repository configuration change (repo.create, repo.update,
/// repo.delete).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

const UPSERT_DOWNLOAD_DAY: &str = "INSERT INTO artifact_download_daily(repo_name, path, day, count)
		 VALUES(?, ?, ?, ?)
		 ON CONFLICT(repo_name, path, day) DO UPDATE SET count = count + excluded.count";

/// The span the artifact page's rolling download counts cover. Daily counts
/// older than this are never read and are pruned.
pub const DOWNLOAD_COUNT_WINDOW: TimeDelta = TimeDelta::days(30);

/// Bounds on the rows one prune batch deletes. The size adapts so each batch
/// holds the single write connection for about [`PRUNE_BATCH_TARGET`]: on a
/// large database with a cold page cache a fixed 5,000-row batch took about
/// 11 seconds, and every console write queued behind it.
const PRUNE_BATCH_START: i64 = 1_000;
const PRUNE_BATCH_MIN: i64 = 100;
const PRUNE_BATCH_MAX: i64 = 5_000;
const PRUNE_BATCH_TARGET: Duration = Duration::from_millis(200);

/// Pause between prune batches, leaving the write connection free for other
/// writers.
const PRUNE_PAUSE: Duration = Duration::from_millis(100);

/// Rows of expired downloads selected per batch: refused downloads and other
/// events are kept, so the scan walks ids after the previous batch instead of
/// re-reading every kept row from the oldest one. The `+` operators keep the
/// planner on the rowid range rather than `idx_audit_created`, which would
/// restart at the oldest kept entry.
const SELECT_EXPIRED_DOWNLOADS: &str = "SELECT id FROM audit_logs
 WHERE id > ?1 AND id <= ?2
   AND +event = 'download' AND +status NOT IN (401, 403) AND +created_at < ?3
 ORDER BY id LIMIT ?4";

/// The audit id range one backfill step folds into the daily counts.
const BACKFILL_CHUNK: i64 = 20_000;

/// (repo, path, UTC day) to download count.
type DayCounts = HashMap<(String, String, String), i64>;

/// Whether an entry counts toward an artifact's downloads: a successful GET.
fn is_counted_download(event: &str, method: &str, status: i64) -> bool {
    event == EVENT_DOWNLOAD && method == "GET" && matches!(status, 200 | 206)
}

/// The UTC calendar day of an RFC 3339 timestamp as written by `format_time`.
fn day_of(created: &str) -> &str {
    created.get(..10).unwrap_or(created)
}

fn upsert_download_days(tx: &Transaction<'_>, days: DayCounts) -> Result<()> {
    let mut stmt = tx
        .prepare(UPSERT_DOWNLOAD_DAY)
        .map_err(|e| Error::sqlite("prepare download days", e))?;
    for ((repo, path, day), n) in days {
        stmt.execute(params![repo, path, day, n])
            .map_err(|e| Error::sqlite("upsert download day", e))?;
    }
    Ok(())
}

/// The next prune batch size: halved when the last batch held the write
/// connection past [`PRUNE_BATCH_TARGET`], doubled when it took under half of
/// it, within [`PRUNE_BATCH_MIN`] and [`PRUNE_BATCH_MAX`].
fn next_prune_batch(limit: i64, held: Duration) -> i64 {
    if held > PRUNE_BATCH_TARGET {
        (limit / 2).max(PRUNE_BATCH_MIN)
    } else if held < PRUNE_BATCH_TARGET / 2 {
        (limit * 2).min(PRUNE_BATCH_MAX)
    } else {
        limit
    }
}

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
        self.insert_audit_logs(vec![l]).await
    }

    /// Appends logs in one transaction. The audit recorder drains its buffer
    /// into batches so a request burst costs one write-connection acquisition
    /// and one fsync per batch instead of one per event; that is what keeps the
    /// buffer from overflowing (and dropping events) while the single write
    /// connection is busy with artifact writes. Rows are inserted in slice
    /// order, so ids follow arrival order as before. `created_at` defaults to
    /// now when unset. Successful GET downloads are also added to the daily
    /// counts in the same transaction.
    pub async fn insert_audit_logs(&self, logs: Vec<AuditLog>) -> Result<()> {
        if logs.is_empty() {
            return Ok(());
        }
        self.write(move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| Error::sqlite("begin audit batch", e))?;
            let mut days = DayCounts::new();
            {
                let mut stmt = tx
                    .prepare(INSERT_AUDIT_LOG)
                    .map_err(|e| Error::sqlite("prepare audit batch", e))?;
                for l in &logs {
                    let created = created_text(l);
                    if is_counted_download(&l.event, &l.method, l.status) {
                        *days
                            .entry((
                                l.repo_name.clone(),
                                l.path.clone(),
                                day_of(&created).to_owned(),
                            ))
                            .or_default() += 1;
                    }
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
                        created
                    ])
                    .map_err(|e| Error::sqlite("insert audit log", e))?;
                }
            }
            upsert_download_days(&tx, days)?;
            tx.commit()
                .map_err(|e| Error::sqlite("commit audit batch", e))
        })
        .await
    }

    /// Counts recorded successful GET responses on the UTC days from `since`
    /// through `until` for the requested repository-relative paths. The daily
    /// counts are the source, so the window is whole days and does not depend
    /// on how long download rows stay in the audit log. Requests through a
    /// group are recorded against that group, not its members.
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
                "SELECT path, SUM(count) FROM artifact_download_daily
                 WHERE repo_name = ? AND day >= ? AND day <= ?
                   AND path IN ({placeholders}) GROUP BY path"
            );
            let mut args = vec![
                Value::Text(repo_name),
                Value::Text(day_of(&format_time(since)).to_owned()),
                Value::Text(day_of(&format_time(until)).to_owned()),
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
        self.delete_in_batches(
            "prune audit logs",
            "DELETE FROM audit_logs WHERE id IN (
                SELECT id FROM audit_logs WHERE created_at < ?1 LIMIT ?2)",
            format_time(before),
        )
        .await
    }

    /// Deletes download entries older than `before`, except refused ones (401,
    /// 403) that the security insights read. Used when downloads have a shorter
    /// retention than the rest of the log.
    ///
    /// Ids follow arrival order, so the run covers ids up to the newest entry
    /// older than `before`. An entry recorded out of order past that id is left
    /// for the next run.
    pub async fn prune_download_audit_logs(&self, before: DateTime<Utc>) -> Result<i64> {
        let cutoff = format_time(before);
        let bound = cutoff.clone();
        let last: Option<i64> = self
            .read(move |conn| {
                conn.query_row(
                    "SELECT id FROM audit_logs WHERE created_at < ?1
                     ORDER BY created_at DESC, id DESC LIMIT 1",
                    [bound],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| Error::sqlite("find download prune bound", e))
            })
            .await?;
        let Some(last) = last else {
            return Ok(0);
        };
        let mut after = 0i64;
        let mut limit = PRUNE_BATCH_START;
        let mut total = 0i64;
        loop {
            self.yield_to_writers().await;
            let cutoff = cutoff.clone();
            let (ids, held) = self
                .write(move |conn| {
                    let started = Instant::now();
                    let tx = conn
                        .transaction()
                        .map_err(|e| Error::sqlite("begin download prune", e))?;
                    let ids = {
                        let mut stmt = tx
                            .prepare(SELECT_EXPIRED_DOWNLOADS)
                            .map_err(|e| Error::sqlite("prepare download prune", e))?;
                        stmt.query_map(params![after, last, cutoff, limit], |r| r.get::<_, i64>(0))
                            .map_err(|e| Error::sqlite("select expired downloads", e))?
                            .collect::<rusqlite::Result<Vec<_>>>()
                            .map_err(|e| Error::sqlite("select expired downloads", e))?
                    };
                    {
                        let mut del = tx
                            .prepare("DELETE FROM audit_logs WHERE id = ?1")
                            .map_err(|e| Error::sqlite("prepare download prune", e))?;
                        for id in &ids {
                            del.execute([id])
                                .map_err(|e| Error::sqlite("prune download audit logs", e))?;
                        }
                    }
                    tx.commit()
                        .map_err(|e| Error::sqlite("commit download prune", e))?;
                    Ok((ids, started.elapsed()))
                })
                .await?;
            let n = ids.len() as i64;
            total += n;
            match ids.last() {
                Some(&id) if n == limit => after = id,
                _ => return Ok(total),
            }
            limit = next_prune_batch(limit, held);
            tokio::time::sleep(PRUNE_PAUSE).await;
        }
    }

    /// Deletes daily download counts for days before `before`'s UTC day.
    pub async fn prune_download_days(&self, before: DateTime<Utc>) -> Result<i64> {
        self.delete_in_batches(
            "prune download days",
            "DELETE FROM artifact_download_daily WHERE (repo_name, path, day) IN (
                SELECT repo_name, path, day FROM artifact_download_daily WHERE day < ?1 LIMIT ?2)",
            day_of(&format_time(before)).to_owned(),
        )
        .await
    }

    /// Runs `sql` (bound to `bound` and the batch size) until it deletes less
    /// than a full batch, one write transaction per batch, pausing and
    /// yielding to queued writes between batches.
    async fn delete_in_batches(
        &self,
        op: &'static str,
        sql: &'static str,
        bound: String,
    ) -> Result<i64> {
        let mut limit = PRUNE_BATCH_START;
        let mut total = 0i64;
        loop {
            self.yield_to_writers().await;
            let bound = bound.clone();
            let (n, held) = self
                .write(move |conn| {
                    let started = Instant::now();
                    conn.execute(sql, params![bound, limit])
                        .map(|n| (n as i64, started.elapsed()))
                        .map_err(|e| Error::sqlite(op, e))
                })
                .await?;
            total += n;
            if n < limit {
                return Ok(total);
            }
            limit = next_prune_batch(limit, held);
            tokio::time::sleep(PRUNE_PAUSE).await;
        }
    }

    /// Folds the next chunk of pre-cutover audit rows into the daily download
    /// counts and reports whether the backfill is complete. Each chunk commits
    /// with its progress marker, so an interrupted backfill resumes without
    /// counting a row twice. The final step drops the partial index the old
    /// count query needed.
    pub async fn backfill_download_days(&self) -> Result<bool> {
        self.write(|conn| {
            let tx = conn
                .transaction()
                .map_err(|e| Error::sqlite("begin download backfill", e))?;
            let state: Option<(i64, i64, bool)> = tx
                .query_row(
                    "SELECT cutover_id, last_id, done FROM artifact_download_backfill WHERE id = 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(|e| Error::sqlite("read download backfill", e))?;
            let Some((cutover, last, false)) = state else {
                return Ok(true);
            };
            if last >= cutover {
                tx.execute_batch(
                    "DROP INDEX IF EXISTS idx_audit_artifact_downloads;
                     UPDATE artifact_download_backfill SET done = 1 WHERE id = 1;",
                )
                .map_err(|e| Error::sqlite("finish download backfill", e))?;
                tx.commit()
                    .map_err(|e| Error::sqlite("commit download backfill", e))?;
                return Ok(true);
            }
            let upto = cutover.min(last.saturating_add(BACKFILL_CHUNK));
            let mut days = DayCounts::new();
            {
                let mut stmt = tx
                    .prepare(
                        "SELECT repo_name, path, created_at FROM audit_logs
                         WHERE id > ?1 AND id <= ?2
                           AND event = 'download' AND method = 'GET' AND status IN (200, 206)",
                    )
                    .map_err(|e| Error::sqlite("prepare download backfill", e))?;
                let rows = stmt
                    .query_map(params![last, upto], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    })
                    .map_err(|e| Error::sqlite("query download backfill", e))?;
                for row in rows {
                    let (repo, path, created) =
                        row.map_err(|e| Error::sqlite("read download backfill", e))?;
                    let day = day_of(&created).to_owned();
                    *days.entry((repo, path, day)).or_default() += 1;
                }
            }
            upsert_download_days(&tx, days)?;
            tx.execute(
                "UPDATE artifact_download_backfill SET last_id = ?1 WHERE id = 1",
                [upto],
            )
            .map_err(|e| Error::sqlite("advance download backfill", e))?;
            tx.commit()
                .map_err(|e| Error::sqlite("commit download backfill", e))?;
            Ok(false)
        })
        .await
    }

    /// Whether every pre-cutover download row has been counted. Download rows
    /// must not be pruned before then, or their counts are lost.
    pub async fn download_backfill_done(&self) -> Result<bool> {
        self.read(|conn| {
            conn.query_row(
                "SELECT done FROM artifact_download_backfill WHERE id = 1",
                [],
                |r| r.get::<_, bool>(0),
            )
            .optional()
            .map(|d| d.unwrap_or(true))
            .map_err(|e| Error::sqlite("read download backfill", e))
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
                since - Duration::days(1),
            ),
            (
                "r",
                "a",
                EVENT_DOWNLOAD,
                "GET",
                200,
                until + Duration::days(1),
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

    #[test]
    fn prune_batch_adapts_to_hold_time() {
        use std::time::Duration as Std;

        use super::{PRUNE_BATCH_MAX, PRUNE_BATCH_MIN, PRUNE_BATCH_TARGET, next_prune_batch};

        assert_eq!(next_prune_batch(1_000, Std::from_secs(11)), 500);
        assert_eq!(next_prune_batch(1_000, Std::from_millis(10)), 2_000);
        assert_eq!(next_prune_batch(1_000, PRUNE_BATCH_TARGET), 1_000);
        assert_eq!(next_prune_batch(150, Std::from_secs(1)), PRUNE_BATCH_MIN);
        assert_eq!(next_prune_batch(4_000, Std::ZERO), PRUNE_BATCH_MAX);
    }

    /// Inserts `n` entries of one kind, `age` old, in one batch.
    async fn insert_entries(s: &Store, n: usize, event: &str, status: i64, age: Duration) {
        let at = Utc::now() - age;
        let logs = (0..n)
            .map(|i| AuditLog {
                repo_name: "r".into(),
                event: event.into(),
                path: format!("p{i}"),
                method: "GET".into(),
                status,
                created_at: at,
                ..AuditLog::default()
            })
            .collect();
        s.insert_audit_logs(logs).await.unwrap();
    }

    #[tokio::test]
    async fn prune_audit_logs_spans_batches() {
        let (s, _dir) = test_store().await;
        insert_entries(&s, 2_500, EVENT_UPLOAD, 201, Duration::days(10)).await;
        insert_entries(&s, 3, EVENT_UPLOAD, 201, Duration::zero()).await;
        let n = s
            .prune_audit_logs(Utc::now() - Duration::days(1))
            .await
            .unwrap();
        assert_eq!(n, 2_500);
        assert_eq!(s.count_audit_logs("r", "").await.unwrap(), 3);
    }

    #[tokio::test]
    async fn prune_download_audit_logs_keeps_refused_recent_and_other_events() {
        let (s, _dir) = test_store().await;
        let old = Duration::days(10);
        // Kept rows interleave with expired downloads, so the scan has to
        // step past them across several batches.
        for _ in 0..3 {
            insert_entries(&s, 900, EVENT_DOWNLOAD, 200, old).await;
            insert_entries(&s, 2, EVENT_DOWNLOAD, 401, old).await;
            insert_entries(&s, 1, EVENT_DOWNLOAD, 403, old).await;
            insert_entries(&s, 4, EVENT_UPLOAD, 201, old).await;
        }
        insert_entries(&s, 5, EVENT_DOWNLOAD, 200, Duration::zero()).await;

        let n = s
            .prune_download_audit_logs(Utc::now() - Duration::days(1))
            .await
            .unwrap();
        assert_eq!(n, 2_700);
        assert_eq!(s.count_audit_logs("r", EVENT_UPLOAD).await.unwrap(), 12);
        assert_eq!(
            s.count_audit_logs("r", EVENT_DOWNLOAD).await.unwrap(),
            9 + 5
        );

        let again = s
            .prune_download_audit_logs(Utc::now() - Duration::days(1))
            .await
            .unwrap();
        assert_eq!(again, 0);
    }

    #[tokio::test]
    async fn prune_download_audit_logs_without_expired_entries() {
        let (s, _dir) = test_store().await;
        assert_eq!(
            s.prune_download_audit_logs(Utc::now()).await.unwrap(),
            0,
            "empty log"
        );
        insert_entries(&s, 3, EVENT_DOWNLOAD, 200, Duration::zero()).await;
        let n = s
            .prune_download_audit_logs(Utc::now() - Duration::days(1))
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(s.count_audit_logs("r", "").await.unwrap(), 3);
    }

    /// The expired-download scan must walk the rowid range: the created_at
    /// index restarts at the oldest kept entry on every batch.
    #[tokio::test]
    async fn expired_download_scan_uses_the_rowid_range() {
        let (s, _dir) = test_store().await;
        let plan = s
            .read(|conn| {
                let mut stmt = conn
                    .prepare(&format!(
                        "EXPLAIN QUERY PLAN {}",
                        super::SELECT_EXPIRED_DOWNLOADS
                    ))
                    .map_err(|e| Error::sqlite("explain", e))?;
                let rows = stmt
                    .query_map(rusqlite::params![0, 10, "z", 10], |r| r.get::<_, String>(3))
                    .map_err(|e| Error::sqlite("explain", e))?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|e| Error::sqlite("explain", e))?;
                Ok(rows.join("\n"))
            })
            .await
            .unwrap();
        assert!(plan.contains("INTEGER PRIMARY KEY"), "plan: {plan}");
        assert!(!plan.contains("idx_audit"), "plan: {plan}");
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
