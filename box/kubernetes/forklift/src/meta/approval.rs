//! Package-level approval (quarantine) decisions for proxy repositories.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use regex::Regex;
use rusqlite::{Row, params, params_from_iter, types::Value};

use super::search::{like_contains, search_time, search_time_expr};
use super::time::{is_zero, now_rfc3339, parse_time, parse_time_opt};
use super::{Error, Result, Store};

/// One package-level approval decision (or pending request) for a proxy
/// repository. The package string is the canonical per-format name (npm
/// package, normalized PyPI project, maven group:artifact, crate name, go
/// module path).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PackageApproval {
    pub id: i64,
    pub repo_name: String,
    pub package: String,
    pub status: String,
    /// first requester, empty = anonymous
    pub requested_by: String,
    pub decided_by: String,
    pub note: String,
    pub request_count: i64,
    /// last version seen in a blocked request, "" if none carried one
    pub last_requested_version: String,
    pub first_requested_at: DateTime<Utc>,
    pub last_requested_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    /// The receiver names an approval-request alarm was dispatched to for this
    /// package (empty when none configured).
    pub notified_receivers: Vec<String>,
    /// Notification delivery outcome, recorded when the alarm is actually sent.
    /// `notified_at` is the send time (`None` until delivered), `notify_result`
    /// is "delivered" or "failed", and `notify_duration_ms` is the send elapsed
    /// time.
    pub notified_at: Option<DateTime<Utc>>,
    pub notify_result: String,
    pub notify_duration_ms: i64,
    /// A concrete outcome detail: the HTTP status on a reachable webhook
    /// ("HTTP 200") or a short reason on failure ("no response from webhook").
    /// Empty until delivered.
    pub notify_detail: String,
}

// Approval status constants.
pub const APPROVAL_PENDING: &str = "pending";
pub const APPROVAL_APPROVED: &str = "approved";
pub const APPROVAL_REJECTED: &str = "rejected";

// Approval audit event constants.
pub const EVENT_APPROVAL_REQUEST: &str = "approval.request";
pub const EVENT_APPROVAL_APPROVE: &str = "approval.approve";
pub const EVENT_APPROVAL_REJECT: &str = "approval.reject";

const APPROVAL_COLS: &str = "id, repo_name, package, status, requested_by, decided_by, note,
       request_count, last_requested_version, first_requested_at, last_requested_at, decided_at, notified_receivers,
       notified_at, notify_result, notify_duration_ms, notify_detail";

impl Store {
    /// Returns a package's approval status for a repository. Hot path for the
    /// approval gate: a single indexed point read. Returns [`Error::NotFound`]
    /// when the package has never been requested or decided.
    pub async fn get_approval_status(&self, repo_name: &str, pkg: &str) -> Result<String> {
        let repo_name = repo_name.to_string();
        let pkg = pkg.to_string();
        self.read(move |conn| {
            conn.query_row(
                "SELECT status FROM package_approvals WHERE repo_name = ? AND package = ?",
                params![repo_name, pkg],
                |r| r.get::<_, String>(0),
            )
            .map_err(|e| Error::sqlite("get approval status", e))
        })
        .await
    }

    /// Records demand for an unapproved package: it creates a pending row on
    /// first request and bumps request_count/last_requested_at on subsequent
    /// ones. Approved rows are left untouched. Returns `true` only when a new
    /// pending row was inserted (drives the approval.request audit event).
    ///
    /// `version` is the version observed in the blocked request ("" for
    /// metadata requests that carry none). It is display-only context for the
    /// queue; a non-empty value overwrites the stored one, but an empty value
    /// never clobbers a version already recorded from an earlier versioned
    /// request.
    pub async fn upsert_pending_approval(
        &self,
        repo_name: &str,
        pkg: &str,
        username: &str,
        version: &str,
    ) -> Result<bool> {
        let repo_name = repo_name.to_string();
        let pkg = pkg.to_string();
        let username = username.to_string();
        let version = version.to_string();
        self.write(move |conn| {
            let now = now_rfc3339();
            match conn.query_row(
                "INSERT INTO package_approvals(repo_name, package, status, requested_by, last_requested_version, first_requested_at, last_requested_at)
         VALUES(?, ?, 'pending', ?, ?, ?, ?)
         ON CONFLICT(repo_name, package) DO UPDATE SET
             request_count = request_count + 1,
             last_requested_at = excluded.last_requested_at,
             last_requested_version = CASE
                 WHEN excluded.last_requested_version != '' THEN excluded.last_requested_version
                 ELSE package_approvals.last_requested_version END
             WHERE package_approvals.status != 'approved'
         RETURNING request_count",
                params![repo_name, pkg, username, version, now, now],
                |r| r.get::<_, i64>(0),
            ) {
                Ok(count) => Ok(count == 1),
                // The DO UPDATE WHERE clause skipped an approved row.
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
                Err(e) => Err(Error::sqlite("upsert pending approval", e)),
            }
        })
        .await
    }

    /// Returns one approval row by id.
    pub async fn get_approval(&self, id: i64) -> Result<PackageApproval> {
        self.read(move |conn| {
            conn.query_row(
                &format!("SELECT {APPROVAL_COLS} FROM package_approvals WHERE id = ?"),
                params![id],
                scan_approval,
            )
            .map_err(|e| Error::sqlite("get approval", e))
        })
        .await
    }

    /// Returns approval rows, newest first. `repo_name` and `status` are
    /// optional filters.
    pub async fn list_approvals(
        &self,
        repo_name: &str,
        status: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<PackageApproval>> {
        let repo_name = repo_name.to_string();
        let status = status.to_string();
        self.read(move |conn| {
            let mut q = format!("SELECT {APPROVAL_COLS} FROM package_approvals WHERE 1=1");
            let mut args: Vec<Value> = Vec::new();
            if !repo_name.is_empty() {
                q.push_str(" AND repo_name = ?");
                args.push(Value::Text(repo_name));
            }
            if !status.is_empty() {
                q.push_str(" AND status = ?");
                args.push(Value::Text(status));
            }
            q.push_str(" ORDER BY id DESC LIMIT ? OFFSET ?");
            args.push(Value::Integer(limit));
            args.push(Value::Integer(offset));

            let mut stmt = conn
                .prepare(&q)
                .map_err(|e| Error::sqlite("list approvals", e))?;
            let out = stmt
                .query_map(params_from_iter(args.iter()), scan_approval)
                .map_err(|e| Error::sqlite("list approvals", e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::sqlite("scan approval", e))?;
            Ok(out)
        })
        .await
    }

    /// Returns the number of approval rows matching the optional `repo_name`
    /// and `status` filters.
    pub async fn count_approvals(&self, repo_name: &str, status: &str) -> Result<i64> {
        let repo_name = repo_name.to_string();
        let status = status.to_string();
        self.read(move |conn| {
            let mut q = String::from("SELECT COUNT(*) FROM package_approvals WHERE 1=1");
            let mut args: Vec<Value> = Vec::new();
            if !repo_name.is_empty() {
                q.push_str(" AND repo_name = ?");
                args.push(Value::Text(repo_name));
            }
            if !status.is_empty() {
                q.push_str(" AND status = ?");
                args.push(Value::Text(status));
            }
            conn.query_row(&q, params_from_iter(args.iter()), |r| r.get::<_, i64>(0))
                .map_err(|e| Error::sqlite("count approvals", e))
        })
        .await
    }

    /// Returns the number of pending approval requests per repository name, for
    /// repositories that have at least one. Repositories with no pending
    /// requests are absent from the map. Used by the repository list to flag
    /// repositories with packages awaiting approval.
    pub async fn pending_approval_count_by_repo(&self) -> Result<HashMap<String, i64>> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare("SELECT repo_name, COUNT(*) FROM package_approvals WHERE status = ? GROUP BY repo_name")
                .map_err(|e| Error::sqlite("pending approvals by repo", e))?;
            let rows = stmt
                .query_map(params![APPROVAL_PENDING], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })
                .map_err(|e| Error::sqlite("pending approvals by repo", e))?;
            let mut out = HashMap::new();
            for row in rows {
                let (name, n) = row.map_err(|e| Error::sqlite("pending approvals by repo", e))?;
                out.insert(name, n);
            }
            Ok(out)
        })
        .await
    }

    /// Sets a row's status (approved or rejected), recording who decided and an
    /// optional note. Re-deciding is allowed (approve after reject and vice
    /// versa).
    pub async fn decide_approval(
        &self,
        id: i64,
        status: &str,
        decided_by: &str,
        note: &str,
    ) -> Result<()> {
        let status = status.to_string();
        let decided_by = decided_by.to_string();
        let note = note.to_string();
        self.write(move |conn| {
            let n = conn
                .execute(
                    "UPDATE package_approvals SET status = ?, decided_by = ?, note = ?, decided_at = ? WHERE id = ?",
                    params![status, decided_by, note, now_rfc3339(), id],
                )
                .map_err(|e| Error::sqlite("decide approval", e))?;
            if n == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
        .await
    }

    /// Approves every pending package in one repository in a single statement
    /// and returns the rows it changed (for the audit log). Already approved or
    /// rejected rows are left untouched. Scoped to one repository so the
    /// per-repository approve permission check stays meaningful.
    pub async fn approve_all_pending(
        &self,
        repo_name: &str,
        decided_by: &str,
        note: &str,
    ) -> Result<Vec<PackageApproval>> {
        let repo_name = repo_name.to_string();
        let decided_by = decided_by.to_string();
        let note = note.to_string();
        self.write(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "UPDATE package_approvals SET status = 'approved', decided_by = ?, note = ?, decided_at = ?
         WHERE repo_name = ? AND status = 'pending'
         RETURNING {APPROVAL_COLS}"
                ))
                .map_err(|e| Error::sqlite("approve all pending", e))?;
            let out = stmt
                .query_map(
                    params![decided_by, note, now_rfc3339(), repo_name],
                    scan_approval,
                )
                .map_err(|e| Error::sqlite("approve all pending", e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::sqlite("scan approval", e))?;
            Ok(out)
        })
        .await
    }

    /// Returns how many pending approvals in one repository have a stored
    /// vulnerability scan whose max severity is "none" (Clean). `eco` is the
    /// repository's OSV ecosystem; an empty `eco` or an unscanned coordinate
    /// never counts as Clean, so the result is 0 for formats OSV does not
    /// cover. The scan join matches on last_requested_version (the empty string
    /// matches a package-level scan recorded with version "").
    pub async fn count_clean_pending(&self, repo_name: &str, eco: &str) -> Result<i64> {
        if eco.is_empty() {
            return Ok(0);
        }
        let repo_name = repo_name.to_string();
        let eco = eco.to_string();
        self.read(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM package_approvals p
         WHERE p.repo_name = ? AND p.status = 'pending'
           AND EXISTS (
               SELECT 1 FROM vuln_scans v
               WHERE v.ecosystem = ? AND v.package = p.package
                 AND v.version = p.last_requested_version
                 AND v.max_severity = 'none')",
                params![repo_name, eco],
                |r| r.get::<_, i64>(0),
            )
            .map_err(|e| Error::sqlite("count clean pending", e))
        })
        .await
    }

    /// Approves only the pending packages in one repository whose stored scan
    /// is Clean (max severity "none"), leaving vulnerable and unscanned packages
    /// pending for individual review. `eco` is the repository's OSV ecosystem;
    /// an empty `eco` approves nothing. Mirrors [`Store::approve_all_pending`]
    /// otherwise.
    pub async fn approve_all_pending_clean(
        &self,
        repo_name: &str,
        eco: &str,
        decided_by: &str,
        note: &str,
    ) -> Result<Vec<PackageApproval>> {
        if eco.is_empty() {
            return Ok(Vec::new());
        }
        let repo_name = repo_name.to_string();
        let eco = eco.to_string();
        let decided_by = decided_by.to_string();
        let note = note.to_string();
        self.write(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "UPDATE package_approvals SET status = 'approved', decided_by = ?, note = ?, decided_at = ?
         WHERE repo_name = ? AND status = 'pending'
           AND EXISTS (
               SELECT 1 FROM vuln_scans v
               WHERE v.ecosystem = ? AND v.package = package_approvals.package
                 AND v.version = package_approvals.last_requested_version
                 AND v.max_severity = 'none')
         RETURNING {APPROVAL_COLS}"
                ))
                .map_err(|e| Error::sqlite("approve all pending clean", e))?;
            let out = stmt
                .query_map(
                    params![decided_by, note, now_rfc3339(), repo_name, eco],
                    scan_approval,
                )
                .map_err(|e| Error::sqlite("approve all pending clean", e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::sqlite("scan approval", e))?;
            Ok(out)
        })
        .await
    }

    /// Creates or overwrites a decision for a package that may not have been
    /// requested yet (manual pre-approval via the admin API).
    pub async fn upsert_approval_decision(
        &self,
        repo_name: &str,
        pkg: &str,
        status: &str,
        decided_by: &str,
        note: &str,
    ) -> Result<PackageApproval> {
        let repo_name = repo_name.to_string();
        let pkg = pkg.to_string();
        let status = status.to_string();
        let decided_by = decided_by.to_string();
        let note = note.to_string();
        self.write(move |conn| {
            let now = now_rfc3339();
            conn.query_row(
                &format!(
                    "INSERT INTO package_approvals(repo_name, package, status, decided_by, note, request_count, first_requested_at, last_requested_at, decided_at)
         VALUES(?, ?, ?, ?, ?, 0, ?, ?, ?)
         ON CONFLICT(repo_name, package) DO UPDATE SET
             status = excluded.status,
             decided_by = excluded.decided_by,
             note = excluded.note,
             decided_at = excluded.decided_at
         RETURNING {APPROVAL_COLS}"
                ),
                params![repo_name, pkg, status, decided_by, note, now, now, now],
                scan_approval,
            )
            .map_err(|e| Error::sqlite("upsert approval decision", e))
        })
        .await
    }

    /// Removes all approval rows for a repository. Called on repository
    /// deletion so a recreated same-name repo does not inherit old trust
    /// decisions.
    pub async fn delete_approvals_for_repo(&self, repo_name: &str) -> Result<()> {
        let repo_name = repo_name.to_string();
        self.write(move |conn| {
            conn.execute(
                "DELETE FROM package_approvals WHERE repo_name = ?",
                params![repo_name],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("delete approvals for repo", e))
        })
        .await
    }

    /// Records the receivers an approval-request alarm was dispatched to for a
    /// package, for display in the approval queue. `receivers` are stored
    /// comma-separated (receiver names never contain commas).
    pub async fn mark_approval_notified(
        &self,
        repo_name: &str,
        pkg: &str,
        receivers: &[String],
    ) -> Result<()> {
        let repo_name = repo_name.to_string();
        let pkg = pkg.to_string();
        let joined = receivers.join(",");
        self.write(move |conn| {
            conn.execute(
                "UPDATE package_approvals SET notified_receivers = ? WHERE repo_name = ? AND package = ?",
                params![joined, repo_name, pkg],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("mark approval notified", e))
        })
        .await
    }

    /// Records the outcome of an approval-request alarm's actual delivery: the
    /// send time, the result ("delivered" or "failed") and how long it took.
    /// Called from the delivery recorder after the (batched, async) send
    /// completes, so the review detail page can show when and how the alarm
    /// went out. Last delivery wins when several receivers are targeted.
    pub async fn record_approval_delivery(
        &self,
        repo_name: &str,
        pkg: &str,
        result: &str,
        detail: &str,
        duration_ms: i64,
    ) -> Result<()> {
        let repo_name = repo_name.to_string();
        let pkg = pkg.to_string();
        let result = result.to_string();
        let detail = detail.to_string();
        self.write(move |conn| {
            conn.execute(
                "UPDATE package_approvals SET notified_at = ?, notify_result = ?, notify_detail = ?, notify_duration_ms = ?
         WHERE repo_name = ? AND package = ?",
                params![now_rfc3339(), result, detail, duration_ms, repo_name, pkg],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("record approval delivery", e))
        })
        .await
    }
}

//

/// Concatenates the textual columns the approval queue renders, for regex
/// matching in Rust.
fn approval_search_text(a: &PackageApproval) -> String {
    [
        a.repo_name.as_str(),
        a.package.as_str(),
        a.last_requested_version.as_str(),
        a.requested_by.as_str(),
        a.status.as_str(),
        &search_time(a.last_requested_at),
    ]
    .join("\n")
}

/// Builds the WHERE clause for the approval queue: the optional
/// `repo_name`/`status` filters plus a substring search over the rendered
/// columns when `q` is non-empty.
fn approval_search_where(repo_name: &str, status: &str, q: &str) -> (String, Vec<Value>) {
    let mut where_clause = String::from("1=1");
    let mut args: Vec<Value> = Vec::new();
    if !repo_name.is_empty() {
        where_clause.push_str(" AND repo_name = ?");
        args.push(Value::Text(repo_name.to_string()));
    }
    if !status.is_empty() {
        where_clause.push_str(" AND status = ?");
        args.push(Value::Text(status.to_string()));
    }
    if !q.is_empty() {
        let pat = like_contains(q);
        where_clause.push_str(&format!(
            " AND (repo_name LIKE ? ESCAPE '\\' OR package LIKE ? ESCAPE '\\' OR last_requested_version LIKE ? ESCAPE '\\'
		 OR requested_by LIKE ? ESCAPE '\\' OR status LIKE ? ESCAPE '\\' OR CAST(request_count AS TEXT) LIKE ? ESCAPE '\\'
		 OR {} LIKE ? ESCAPE '\\')",
            search_time_expr("last_requested_at")
        ));
        for _ in 0..7 {
            args.push(Value::Text(pat.clone()));
        }
    }
    (where_clause, args)
}

impl Store {
    /// Returns one page of approval rows matching the optional
    /// `repo_name`/`status` filters and the substring `q`, newest first, plus
    /// the total match count.
    pub async fn search_approvals_page(
        &self,
        repo_name: &str,
        status: &str,
        q: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<PackageApproval>, i64)> {
        let (where_clause, args) = approval_search_where(repo_name, status, q);
        self.read(move |conn| {
            let total: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM package_approvals WHERE {where_clause}"),
                    params_from_iter(args.iter()),
                    |r| r.get(0),
                )
                .map_err(|e| Error::sqlite("count approval matches", e))?;
            let mut page_args = args;
            page_args.push(Value::Integer(limit));
            page_args.push(Value::Integer(offset));
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {APPROVAL_COLS} FROM package_approvals WHERE {where_clause} ORDER BY id DESC LIMIT ? OFFSET ?"
                ))
                .map_err(|e| Error::sqlite("search approvals page", e))?;
            let out = stmt
                .query_map(params_from_iter(page_args.iter()), scan_approval)
                .map_err(|e| Error::sqlite("search approvals page", e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::sqlite("scan approval match", e))?;
            Ok((out, total))
        })
        .await
    }

    /// [`Store::search_approvals_page`] with `re` matched against the same
    /// rendered columns, in Rust, streaming newest first. SQLite carries no
    /// regexp function, so the filter runs here and only the requested page is
    /// kept, which bounds memory on large queues.
    pub async fn search_approvals_page_regex(
        &self,
        repo_name: &str,
        status: &str,
        re: Regex,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<PackageApproval>, i64)> {
        let (where_clause, args) = approval_search_where(repo_name, status, "");
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {APPROVAL_COLS} FROM package_approvals WHERE {where_clause} ORDER BY id DESC"
                ))
                .map_err(|e| Error::sqlite("search approvals page", e))?;
            let rows = stmt
                .query_map(params_from_iter(args.iter()), scan_approval)
                .map_err(|e| Error::sqlite("search approvals page", e))?;
            let mut total: i64 = 0;
            let mut out: Vec<PackageApproval> = Vec::new();
            for row in rows {
                let a = row.map_err(|e| Error::sqlite("scan approval match", e))?;
                if !re.is_match(&approval_search_text(&a)) {
                    continue;
                }
                if total >= offset && (out.len() as i64) < limit {
                    out.push(a);
                }
                total += 1;
            }
            Ok((out, total))
        })
        .await
    }

    /// Returns how many approval rows match `q` (package or repository name).
    pub async fn search_approvals_count(&self, q: &str) -> Result<i64> {
        let pat = like_contains(q);
        self.read(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM package_approvals
		  WHERE package LIKE ? ESCAPE '\\' OR repo_name LIKE ? ESCAPE '\\'",
                params![pat, pat],
                |r| r.get::<_, i64>(0),
            )
            .map_err(|e| Error::sqlite("count approval hits", e))
        })
        .await
    }

    /// Returns approval rows whose package or repository name contains `q`,
    /// newest first.
    pub async fn search_approvals(&self, q: &str, limit: i64) -> Result<Vec<PackageApproval>> {
        let pat = like_contains(q);
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {APPROVAL_COLS} FROM package_approvals
		  WHERE package LIKE ? ESCAPE '\\' OR repo_name LIKE ? ESCAPE '\\'
		  ORDER BY id DESC LIMIT ?"
                ))
                .map_err(|e| Error::sqlite("search approvals", e))?;
            let out = stmt
                .query_map(params![pat, pat, limit], scan_approval)
                .map_err(|e| Error::sqlite("search approvals", e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::sqlite("scan approval", e))?;
            Ok(out)
        })
        .await
    }
}

/// Reads one approval row (the `APPROVAL_COLS` projection).
fn scan_approval(r: &Row<'_>) -> rusqlite::Result<PackageApproval> {
    let first: String = r.get(9)?;
    let last: String = r.get(10)?;
    let decided: Option<String> = r.get(11)?;
    let notified: String = r.get(12)?;
    let notified_at: String = r.get(13)?;
    let notified_receivers = if notified.is_empty() {
        Vec::new()
    } else {
        notified.split(',').map(str::to_string).collect()
    };
    let notified_at = if notified_at.is_empty() {
        None
    } else {
        Some(parse_time(&notified_at)).filter(|tm| !is_zero(*tm))
    };
    Ok(PackageApproval {
        id: r.get(0)?,
        repo_name: r.get(1)?,
        package: r.get(2)?,
        status: r.get(3)?,
        requested_by: r.get(4)?,
        decided_by: r.get(5)?,
        note: r.get(6)?,
        request_count: r.get(7)?,
        last_requested_version: r.get(8)?,
        first_requested_at: parse_time(&first),
        last_requested_at: parse_time(&last),
        decided_at: parse_time_opt(decided.as_deref()),
        notified_receivers,
        notified_at,
        notify_result: r.get(14)?,
        notify_duration_ms: r.get(15)?,
        notify_detail: r.get(16)?,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;

    use chrono::{Duration, Utc};
    use regex::Regex;

    use crate::meta::*;

    #[tokio::test]
    async fn upsert_pending_approval() {
        let (s, _dir) = test_store().await;

        // First demand came from a metadata request with no version.
        let created = s
            .upsert_pending_approval("npmjs", "left-pad", "alice", "")
            .await
            .unwrap();
        assert!(created, "first upsert should create");

        // Repeat requests dedup into the same row and bump the counter. A later
        // versioned request records the version; a subsequent empty one must not
        // clobber it.
        for (i, ver) in ["1.3.0", "", ""].iter().enumerate() {
            let created = s
                .upsert_pending_approval("npmjs", "left-pad", "bob", ver)
                .await
                .unwrap();
            assert!(!created, "repeat upsert {i} must not report created");
        }
        let rows = s
            .list_approvals("npmjs", APPROVAL_PENDING, 10, 0)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "rows, want 1");
        let a = rows[0].clone();
        assert_eq!(a.request_count, 4, "row = {a:?}");
        assert_eq!(a.requested_by, "alice", "row = {a:?}");
        assert_eq!(a.status, APPROVAL_PENDING, "row = {a:?}");
        assert_eq!(
            a.last_requested_version, "1.3.0",
            "empty requests must not clobber last_requested_version"
        );
        assert!(
            a.last_requested_at >= a.first_requested_at,
            "last_requested_at {} before first {}",
            a.last_requested_at,
            a.first_requested_at
        );

        // Approved rows are left untouched by further demand.
        s.decide_approval(a.id, APPROVAL_APPROVED, "admin", "ok")
            .await
            .unwrap();
        let created = s
            .upsert_pending_approval("npmjs", "left-pad", "carol", "")
            .await
            .unwrap();
        assert!(!created, "upsert on approved row must not create");
        let got = s.get_approval(a.id).await.unwrap();
        assert_eq!(
            got.status, APPROVAL_APPROVED,
            "approved row mutated: {got:?}"
        );
        assert_eq!(got.request_count, 4, "approved row mutated: {got:?}");

        // Rejected rows keep accruing demand.
        s.decide_approval(a.id, APPROVAL_REJECTED, "admin", "nope")
            .await
            .unwrap();
        s.upsert_pending_approval("npmjs", "left-pad", "", "")
            .await
            .unwrap();
        let got = s.get_approval(a.id).await.unwrap();
        assert_eq!(got.request_count, 5, "rejected row = {got:?}");
        assert_eq!(got.status, APPROVAL_REJECTED, "rejected row = {got:?}");
    }

    #[tokio::test]
    async fn approval_status_and_decide() {
        let (s, _dir) = test_store().await;

        let err = s.get_approval_status("npmjs", "lodash").await.unwrap_err();
        assert!(err.is_not_found(), "status of unknown package err = {err}");
        s.upsert_pending_approval("npmjs", "lodash", "alice", "")
            .await
            .unwrap();
        let st = s.get_approval_status("npmjs", "lodash").await.unwrap();
        assert_eq!(st, APPROVAL_PENDING, "status");

        let rows = s.list_approvals("", "", 10, 0).await.unwrap();
        s.decide_approval(rows[0].id, APPROVAL_APPROVED, "admin", "reviewed")
            .await
            .unwrap();
        let st = s.get_approval_status("npmjs", "lodash").await.unwrap();
        assert_eq!(st, APPROVAL_APPROVED, "status after approve");
        let got = s.get_approval(rows[0].id).await.unwrap();
        assert_eq!(got.decided_by, "admin", "decided row = {got:?}");
        assert_eq!(got.note, "reviewed", "decided row = {got:?}");
        assert!(got.decided_at.is_some(), "decided row = {got:?}");

        // Re-deciding flips the status.
        s.decide_approval(rows[0].id, APPROVAL_REJECTED, "admin", "incident")
            .await
            .unwrap();
        let st = s.get_approval_status("npmjs", "lodash").await.unwrap();
        assert_eq!(st, APPROVAL_REJECTED, "status after reject");

        let err = s
            .decide_approval(9999, APPROVAL_APPROVED, "admin", "")
            .await
            .unwrap_err();
        assert!(err.is_not_found(), "decide unknown id err = {err}");
    }

    #[tokio::test]
    async fn upsert_approval_decision() {
        let (s, _dir) = test_store().await;

        // Manual pre-approval of a never-requested package.
        let a = s
            .upsert_approval_decision(
                "npmjs",
                "@company/lib",
                APPROVAL_APPROVED,
                "admin",
                "internal",
            )
            .await
            .unwrap();
        assert_eq!(a.status, APPROVAL_APPROVED, "pre-approval = {a:?}");
        assert_eq!(a.request_count, 0, "pre-approval = {a:?}");
        assert!(a.decided_at.is_some(), "pre-approval = {a:?}");

        // Overwriting an existing pending row preserves its demand counters.
        s.upsert_pending_approval("npmjs", "axios", "alice", "")
            .await
            .unwrap();
        let a = s
            .upsert_approval_decision("npmjs", "axios", APPROVAL_REJECTED, "admin", "no")
            .await
            .unwrap();
        assert_eq!(a.status, APPROVAL_REJECTED, "overwrite = {a:?}");
        assert_eq!(a.request_count, 1, "overwrite = {a:?}");
        assert_eq!(a.requested_by, "alice", "overwrite = {a:?}");
    }

    #[tokio::test]
    async fn approve_all_pending() {
        let (s, _dir) = test_store().await;

        for p in ["a", "b", "c"] {
            s.upsert_pending_approval("npmjs", p, "alice", "")
                .await
                .unwrap();
        }
        // A different repo must not be touched by a scoped bulk approve.
        s.upsert_pending_approval("pypi", "requests", "", "")
            .await
            .unwrap();
        // An already-rejected row in the target repo must stay rejected.
        let rej = s
            .upsert_approval_decision("npmjs", "evil", APPROVAL_REJECTED, "admin", "ioc")
            .await
            .unwrap();

        let approved = s
            .approve_all_pending("npmjs", "admin", "batch ok")
            .await
            .unwrap();
        assert_eq!(approved.len(), 3, "approved rows, want 3");
        for a in &approved {
            assert_eq!(a.status, APPROVAL_APPROVED, "approved row: {a:?}");
            assert_eq!(a.decided_by, "admin", "approved row: {a:?}");
            assert_eq!(a.note, "batch ok", "approved row: {a:?}");
            assert!(a.decided_at.is_some(), "approved row: {a:?}");
        }
        // The rejected row is untouched.
        let got = s.get_approval(rej.id).await.unwrap();
        assert_eq!(got.status, APPROVAL_REJECTED, "rejected row flipped");
        // The other repo's pending row is untouched.
        let st = s.get_approval_status("pypi", "requests").await.unwrap();
        assert_eq!(st, APPROVAL_PENDING, "pypi row status");
        // No pending rows left in npmjs: a second run approves nothing.
        let again = s.approve_all_pending("npmjs", "admin", "").await.unwrap();
        assert!(again.is_empty(), "second run approved {}", again.len());
    }

    #[tokio::test]
    async fn approve_all_pending_clean() {
        let (s, _dir) = test_store().await;

        // Three pending npm packages: one Clean-scanned, one vulnerable, one never
        // scanned. Only the Clean one may be approved by a Clean-only bulk approve.
        for (pkg, version) in [
            ("clean-pkg", "1.0.0"),
            ("vuln-pkg", "2.0.0"),
            ("unscanned-pkg", "3.0.0"),
        ] {
            s.upsert_pending_approval("npmjs", pkg, "alice", version)
                .await
                .unwrap();
        }

        let no_counts: HashMap<String, i64> = HashMap::new();
        s.upsert_vuln_scan(
            "npm",
            "clean-pkg",
            "1.0.0",
            "none",
            &[],
            &no_counts,
            0,
            &[],
            "OSV",
        )
        .await
        .unwrap();
        s.upsert_vuln_scan(
            "npm",
            "vuln-pkg",
            "2.0.0",
            "high",
            &["CVE-1".to_string()],
            &no_counts,
            0,
            &[],
            "OSV",
        )
        .await
        .unwrap();

        let n = s.count_clean_pending("npmjs", "npm").await.unwrap();
        assert_eq!(n, 1, "count_clean_pending, want 1");
        // An empty ecosystem (format OSV does not cover) counts nothing.
        let n = s.count_clean_pending("npmjs", "").await.unwrap();
        assert_eq!(n, 0, "count_clean_pending(empty eco), want 0");

        let approved = s
            .approve_all_pending_clean("npmjs", "npm", "admin", "clean batch")
            .await
            .unwrap();
        assert_eq!(approved.len(), 1, "clean approve = {approved:?}");
        assert_eq!(approved[0].package, "clean-pkg", "clean approve");
        // The vulnerable and unscanned rows stay pending for individual review.
        let st = s.get_approval_status("npmjs", "vuln-pkg").await.unwrap();
        assert_eq!(st, APPROVAL_PENDING, "vuln-pkg status");
        let st = s
            .get_approval_status("npmjs", "unscanned-pkg")
            .await
            .unwrap();
        assert_eq!(st, APPROVAL_PENDING, "unscanned-pkg status");
        // An empty ecosystem approves nothing.
        let got = s
            .approve_all_pending_clean("npmjs", "", "admin", "")
            .await
            .unwrap();
        assert!(got.is_empty(), "clean approve(empty eco) = {}", got.len());
    }

    #[tokio::test]
    async fn approval_notify_delivery() {
        let (s, _dir) = test_store().await;

        s.upsert_pending_approval("npmjs", "left-pad", "alice", "1.3.0")
            .await
            .unwrap();
        // Dispatch targets recorded first (the queue's Noted signal).
        s.mark_approval_notified(
            "npmjs",
            "left-pad",
            &["sec-slack".to_string(), "ops-slack".to_string()],
        )
        .await
        .unwrap();
        let got = s.get_approval_status("npmjs", "left-pad").await.unwrap();
        assert_eq!(got, APPROVAL_PENDING, "status");
        let rows = s.list_approvals("npmjs", "", 10, 0).await.unwrap();
        let a = rows[0].clone();
        assert_eq!(
            a.notified_receivers,
            vec!["sec-slack".to_string(), "ops-slack".to_string()],
            "notified receivers"
        );
        assert!(
            a.notified_at.is_none() && a.notify_result.is_empty(),
            "delivery fields set before delivery: {a:?}"
        );

        // Actual delivery outcome recorded when the alarm is sent.
        s.record_approval_delivery("npmjs", "left-pad", "delivered", "HTTP 200", 142)
            .await
            .unwrap();
        let a2 = s.get_approval(a.id).await.unwrap();
        assert_eq!(a2.notify_result, "delivered", "delivery: {a2:?}");
        assert_eq!(a2.notify_duration_ms, 142, "delivery: {a2:?}");
        assert!(a2.notified_at.is_some(), "delivery: {a2:?}");
        assert_eq!(a2.notify_detail, "HTTP 200", "delivery: {a2:?}");
        // Receivers are preserved across the delivery update.
        assert_eq!(a2.notified_receivers.len(), 2, "receivers lost");
    }

    #[tokio::test]
    async fn approval_list_filters_and_delete() {
        let (s, _dir) = test_store().await;

        for p in ["a", "b", "c"] {
            s.upsert_pending_approval("npmjs", p, "", "").await.unwrap();
        }
        s.upsert_pending_approval("pypi", "requests", "", "")
            .await
            .unwrap();
        let rows = s.list_approvals("npmjs", "", 10, 0).await.unwrap();
        s.decide_approval(rows[0].id, APPROVAL_APPROVED, "admin", "")
            .await
            .unwrap();

        assert_eq!(s.count_approvals("", "").await.unwrap(), 4, "total count");
        assert_eq!(
            s.count_approvals("npmjs", APPROVAL_PENDING).await.unwrap(),
            2,
            "npmjs pending count"
        );
        let got = s.list_approvals("", APPROVAL_PENDING, 2, 0).await.unwrap();
        assert_eq!(got.len(), 2, "paginated len");
        let got = s.list_approvals("", APPROVAL_PENDING, 2, 2).await.unwrap();
        assert_eq!(got.len(), 1, "second page len");

        s.delete_approvals_for_repo("npmjs").await.unwrap();
        assert_eq!(
            s.count_approvals("", "").await.unwrap(),
            1,
            "count after delete"
        );
    }

    #[tokio::test]
    async fn pending_approval_count_by_repo_store() {
        let (s, _dir) = test_store().await;
        s.upsert_approval_decision("npmjs", "left-pad", APPROVAL_PENDING, "", "")
            .await
            .unwrap();
        s.upsert_approval_decision("npmjs", "is-odd", APPROVAL_PENDING, "", "")
            .await
            .unwrap();
        // A decided one must not be counted as pending.
        s.upsert_approval_decision("npmjs", "axios", APPROVAL_APPROVED, "admin", "ok")
            .await
            .unwrap();
        let counts = s.pending_approval_count_by_repo().await.unwrap();
        assert_eq!(counts.get("npmjs"), Some(&2), "pending count, want 2");
        // A repository with nothing pending is absent rather than present with a
        // zero, which is what lets the repository list treat a missing key as none.
        assert_eq!(counts.get("pypi"), None);
    }

    #[tokio::test]
    async fn search_approvals_page_store() {
        let (s, _dir) = test_store().await;
        for i in 0..30 {
            let repo = if i % 2 == 0 { "npmjs" } else { "pypi" };
            s.upsert_pending_approval(repo, &format!("seed-pkg-{i:03}"), "alice", "2.0.1")
                .await
                .unwrap();
        }

        // Keyword search across columns with repo scope and paging.
        let (rows, total) = s
            .search_approvals_page("npmjs", "pending", "seed-pkg", 10, 10)
            .await
            .unwrap();
        assert_eq!(total, 15, "keyword total, want 15");
        assert_eq!(rows.len(), 5, "keyword rows, want 5");
        // Matching a non-package column (requester).
        let (_, total) = s
            .search_approvals_page("", "", "alice", 50, 0)
            .await
            .unwrap();
        assert_eq!(total, 30, "requester total, want 30");
        // Regex variant respects the same filters.
        let (rows, total) = s
            .search_approvals_page_regex(
                "pypi",
                "",
                Regex::new(r"(?i)seed-pkg-0[01]\d").unwrap(),
                50,
                0,
            )
            .await
            .unwrap();
        assert_eq!(total, 10, "regex total, want 10");
        assert_eq!(rows.len(), 10, "regex rows, want 10");
    }

    #[tokio::test]
    async fn search_and_aggregate_store_methods_part_b() {
        let (s, _dir) = test_store().await;
        let repo_name = "search-repo";

        // Approvals search.
        s.upsert_pending_approval(repo_name, "com.acme:widget", "alice", "1.0.0")
            .await
            .unwrap();
        let n = s
            .search_approvals_count("widget")
            .await
            .expect("search_approvals_count");
        assert_ne!(n, 0, "search_approvals_count = {n}");
        let approvals = s
            .search_approvals("widget", 10)
            .await
            .expect("search_approvals");
        assert!(!approvals.is_empty(), "search_approvals = {approvals:?}");

        // Vulnerability severity map.
        let no_counts: HashMap<String, i64> = HashMap::new();
        s.upsert_vuln_scan(
            "Maven",
            "com.acme:widget",
            "1.0.0",
            "high",
            &["CVE-1".to_string()],
            &no_counts,
            0,
            &[],
            "OSV",
        )
        .await
        .unwrap();
        let sev = s
            .vuln_severity_by_coordinate()
            .await
            .expect("vuln_severity_by_coordinate");
        assert!(
            !sev.is_empty(),
            "vuln_severity_by_coordinate returned an empty map"
        );

        // Token count + scope update.
        let u = s
            .create_user(User {
                username: "tokuser".into(),
                source: SOURCE_LOCAL.into(),
                ..User::default()
            })
            .await
            .unwrap();
        let exp = Utc::now() + Duration::hours(1);
        let tok = s
            .create_token(Token {
                user_id: u.id,
                name: "ci".into(),
                hash: "hash-x".into(),
                scopes_json: "[]".into(),
                expires_at: Some(exp),
                ..Token::default()
            })
            .await
            .unwrap();
        let c = s.count_tokens(u.id).await.expect("count_tokens");
        assert_eq!(c, 1, "count_tokens = {c}");
        s.update_token_scopes(u.id, tok.id, r#"[{"repo_pattern":"*","actions":["read"]}]"#)
            .await
            .expect("update_token_scopes");
    }
}
