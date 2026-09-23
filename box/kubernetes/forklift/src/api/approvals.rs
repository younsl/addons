use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::response::Response;
use chrono::{DateTime, Utc};
use http::StatusCode;
use http::request::Parts;
use serde::{Deserialize, Serialize};

use crate::audit;
use crate::auth;
use crate::meta::{self, PackageApproval, VulnAdvisory};
use crate::repo;

use super::repositories::{int_param, path_id};
use super::{Handler, map_error, principal_name, write_error, write_json};

/// The JSON shape for one package approval row.
#[derive(Debug, Clone, Default, Serialize)]
struct ApprovalDTO {
    id: i64,
    repo_name: String,
    package: String,
    status: String,
    requested_by: String,
    decided_by: String,
    note: String,
    request_count: i64,
    last_requested_version: String,
    first_requested_at: DateTime<Utc>,
    last_requested_at: DateTime<Utc>,
    decided_at: Option<DateTime<Utc>>,
    /// The vulnerability scan surfaced for the approval decision. `vuln_scope`
    /// is `version` when the scan is for the exact requested version, or
    /// `package` when the version was unknown and the scan covers the package
    /// across all versions. Empty when the coordinate has not been scanned yet.
    #[serde(skip_serializing_if = "String::is_empty")]
    vuln_severity: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    vuln_ids: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    vuln_scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    vuln_counts: Option<std::collections::BTreeMap<String, i64>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    vuln_advisories: Vec<VulnAdvisory>,
    #[serde(skip_serializing_if = "String::is_empty")]
    vuln_source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    vuln_scanned_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "is_zero")]
    vuln_scan_ms: i64,
    /// Usernames permitted to approve this repository. Populated only on the
    /// single-approval detail endpoint.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reviewers: Vec<String>,
    /// Upstream provenance: the proxy repository's upstream base URL and the
    /// resolved full URL of this package on that upstream. Populated only on the
    /// single-approval detail endpoint.
    #[serde(skip_serializing_if = "String::is_empty")]
    upstream_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    upstream_package_url: String,
    /// The receivers an approval-request alarm was dispatched to for this
    /// package (empty when none).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notified_receivers: Vec<String>,
    /// The notification delivery outcome, recorded when the alarm was actually
    /// sent: `notified_at` is the send time, `notify_result` is `delivered` or
    /// `failed`, and `notify_duration_ms` is the send elapsed time. Zero/empty
    /// until delivered.
    #[serde(skip_serializing_if = "Option::is_none")]
    notified_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "String::is_empty")]
    notify_result: String,
    #[serde(skip_serializing_if = "is_zero")]
    notify_duration_ms: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    notify_detail: String,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

fn approval_to_dto(a: PackageApproval) -> ApprovalDTO {
    ApprovalDTO {
        id: a.id,
        repo_name: a.repo_name,
        package: a.package,
        status: a.status,
        requested_by: a.requested_by,
        decided_by: a.decided_by,
        note: a.note,
        request_count: a.request_count,
        last_requested_version: a.last_requested_version,
        first_requested_at: a.first_requested_at,
        last_requested_at: a.last_requested_at,
        decided_at: a.decided_at,
        notified_receivers: a.notified_receivers,
        notified_at: a.notified_at,
        notify_result: a.notify_result,
        notify_duration_ms: a.notify_duration_ms,
        notify_detail: a.notify_detail,
        ..Default::default()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct ListQuery {
    #[serde(default)]
    pub(super) repo: String,
    #[serde(default)]
    pub(super) status: String,
    #[serde(default)]
    pub(super) limit: String,
    #[serde(default)]
    pub(super) offset: String,
    #[serde(default)]
    pub(super) q: String,
    #[serde(default)]
    pub(super) regex: String,
}

/// Returns approval rows, newest first, with optional repo/status filters and
/// limit/offset pagination.
pub(super) async fn list(State(h): State<Arc<Handler>>, Query(q): Query<ListQuery>) -> Response {
    if !q.status.is_empty() && !valid_approval_status(&q.status) {
        return write_error(
            StatusCode::BAD_REQUEST,
            "invalid status (pending|approved|rejected)",
        );
    }
    let mut limit = int_param(&q.limit, 100);
    if !(1..=500).contains(&limit) {
        limit = 100;
    }
    let offset = int_param(&q.offset, 0).max(0);

    let page = if q.regex == "true" && !q.q.is_empty() {
        let re = match super::search_regex(&q.q) {
            Ok(re) => re,
            Err(err) => {
                return write_error(StatusCode::BAD_REQUEST, &format!("invalid regex: {err}"));
            }
        };
        h.store
            .search_approvals_page_regex(&q.repo, &q.status, re, limit, offset)
            .await
    } else {
        h.store
            .search_approvals_page(&q.repo, &q.status, &q.q, limit, offset)
            .await
    };
    let (rows, count) = match page {
        Ok(page) => page,
        Err(err) => return map_error(err),
    };
    // Map repo name -> OSV ecosystem so each approval's last requested version
    // can be annotated with its stored vulnerability scan, if any.
    let mut eco_by_repo: HashMap<String, String> = HashMap::new();
    if let Ok(repos) = h.store.list_repositories().await {
        for repo in repos {
            eco_by_repo.insert(repo.name, repo::osv_ecosystem(&repo.format).to_string());
        }
    }
    let mut out = Vec::with_capacity(rows.len());
    for a in rows {
        let eco = eco_by_repo.get(&a.repo_name).cloned().unwrap_or_default();
        let mut dto = approval_to_dto(a.clone());
        h.annotate_approval_vuln(&mut dto, &a, &eco).await;
        out.push(dto);
    }
    write_json(
        StatusCode::OK,
        ApprovalListDTO {
            count,
            approvals: out,
        },
    )
}

impl Handler {
    /// Fills `dto`'s vulnerability fields from the stored scan for the approval's
    /// coordinate: the exact requested version when known (scope `version`),
    /// otherwise a package-level scan (scope `package`), so the reviewer always
    /// has a signal even when the requested version is unknown. A no-op when
    /// `eco` is empty (a format OSV does not cover) or the coordinate is not
    /// scanned yet.
    async fn annotate_approval_vuln(&self, dto: &mut ApprovalDTO, a: &PackageApproval, eco: &str) {
        if eco.is_empty() {
            return;
        }
        let scope = if a.last_requested_version.is_empty() {
            "package"
        } else {
            "version"
        };
        if let Ok(scan) = self
            .store
            .get_vuln_scan(eco, &a.package, &a.last_requested_version)
            .await
        {
            dto.vuln_severity = scan.max_severity;
            dto.vuln_ids = scan.vuln_ids;
            dto.vuln_scope = scope.to_string();
            dto.vuln_counts = Some(scan.severity_counts.into_iter().collect());
            dto.vuln_advisories = scan.advisories;
            dto.vuln_source = scan.source;
            dto.vuln_scan_ms = scan.duration_ms;
            dto.vuln_scanned_at = Some(scan.scanned_at);
        }
    }
}

/// Returns one approval row with its joined vulnerability scan, for the approval
/// detail page.
pub(super) async fn get(State(h): State<Arc<Handler>>, UrlPath(id): UrlPath<String>) -> Response {
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let a = match h.store.get_approval(id).await {
        Ok(a) => a,
        Err(err) => return map_error(err),
    };
    let mut dto = approval_to_dto(a.clone());
    if let Ok(repo_row) = h.store.get_repository_by_name(&a.repo_name).await {
        h.annotate_approval_vuln(&mut dto, &a, repo::osv_ecosystem(&repo_row.format))
            .await;
        dto.upstream_url = repo_row.upstream_url.clone();
        dto.upstream_package_url =
            repo::upstream_package_url(&repo_row.format, &repo_row.upstream_url, &a.package);
    }
    if let Some(authz) = &h.authz
        && let Ok(reviewers) = authz.approvers_for(&a.repo_name).await
    {
        dto.reviewers = reviewers;
    }
    write_json(StatusCode::OK, dto)
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct CountQuery {
    #[serde(default)]
    pub(super) repo: String,
    #[serde(default)]
    pub(super) status: String,
}

/// Returns just the matching row count (sidebar badge).
pub(super) async fn count(State(h): State<Arc<Handler>>, Query(q): Query<CountQuery>) -> Response {
    if !q.status.is_empty() && !valid_approval_status(&q.status) {
        return write_error(
            StatusCode::BAD_REQUEST,
            "invalid status (pending|approved|rejected)",
        );
    }
    match h.store.count_approvals(&q.repo, &q.status).await {
        Ok(count) => write_json(StatusCode::OK, ApprovalCountDTO { count }),
        Err(err) => map_error(err),
    }
}

/// One page of the approval queue. `count` is every row matching the filter
/// rather than just this page, so a client can page without asking again.
#[derive(Debug, Clone, Serialize)]
struct ApprovalListDTO {
    count: i64,
    approvals: Vec<ApprovalDTO>,
}

/// Answers the queue-size question on its own, for the pending badge that does
/// not need the rows.
#[derive(Debug, Clone, Serialize)]
struct ApprovalCountDTO {
    count: i64,
}

/// Wraps the bulk-approve overview rows.
#[derive(Debug, Clone, Serialize)]
struct PendingRepoListDTO {
    repos: Vec<PendingRepoDTO>,
}

/// Summarises one repository's approval queue: how many packages are pending and
/// how many of those are Clean (would be approved by a Clean-only bulk approve).
/// Drives the bulk-approve overview.
#[derive(Debug, Clone, Serialize)]
struct PendingRepoDTO {
    id: i64,
    repo_name: String,
    format: String,
    r#type: String,
    pending: i64,
    clean: i64,
}

/// Returns the per-repository approval-queue overview, limited to repositories
/// with at least one pending request. The bulk-approve modal uses this both to
/// offer only repositories that actually have a queue and to preview how many
/// packages (total, and Clean) each approval would cover.
pub(super) async fn pending_repos(State(h): State<Arc<Handler>>) -> Response {
    let counts = match h.store.pending_approval_count_by_repo().await {
        Ok(counts) => counts,
        Err(err) => return map_error(err),
    };
    // Repository metadata per name: the OSV ecosystem (so the Clean count can
    // join stored scans; missing/uncovered formats yield 0) plus format and type
    // for display. Both proxy and hosted repositories can have approval queues.
    let mut meta_by_repo: HashMap<String, (String, String, String, i64)> = HashMap::new();
    if let Ok(repos) = h.store.list_repositories().await {
        for repo_row in repos {
            meta_by_repo.insert(
                repo_row.name.clone(),
                (
                    repo::osv_ecosystem(&repo_row.format).to_string(),
                    repo_row.format,
                    repo_row.r#type,
                    repo_row.id,
                ),
            );
        }
    }
    let mut out = Vec::with_capacity(counts.len());
    for (name, pending) in counts {
        let (eco, format, r#type, id) = meta_by_repo.get(&name).cloned().unwrap_or_default();
        let clean = match h.store.count_clean_pending(&name, &eco).await {
            Ok(clean) => clean,
            Err(err) => return map_error(err),
        };
        out.push(PendingRepoDTO {
            id,
            repo_name: name,
            format,
            r#type,
            pending,
            clean,
        });
    }
    out.sort_by(|a, b| a.repo_name.cmp(&b.repo_name));
    write_json(StatusCode::OK, PendingRepoListDTO { repos: out })
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CreateApprovalReq {
    #[serde(default)]
    repo: String,
    #[serde(default)]
    package: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    note: String,
}

/// Records a manual decision for a package that may not have been requested yet
/// (pre-approval, or pre-emptive rejection).
pub(super) async fn create(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let Ok(req) = serde_json::from_slice::<CreateApprovalReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let package = req.package.trim();
    if package.is_empty() {
        return write_error(StatusCode::BAD_REQUEST, "package is required");
    }
    if req.status != meta::APPROVAL_APPROVED && req.status != meta::APPROVAL_REJECTED {
        return write_error(
            StatusCode::BAD_REQUEST,
            "status must be approved or rejected",
        );
    }
    let repo_row = match h.store.get_repository_by_name(req.repo.trim()).await {
        Ok(repo_row) => repo_row,
        Err(err) => return map_error(err),
    };
    if repo_row.r#type != meta::TYPE_PROXY && repo_row.r#type != meta::TYPE_HOSTED {
        return write_error(
            StatusCode::BAD_REQUEST,
            "approval is only valid for proxy or hosted repositories",
        );
    }
    if let Some(response) = h.can_approve(&parts, &repo_row.name) {
        return response;
    }
    let a = match h
        .store
        .upsert_approval_decision(
            &repo_row.name,
            package,
            &req.status,
            &principal_name(&parts),
            &req.note,
        )
        .await
    {
        Ok(a) => a,
        Err(err) => return map_error(err),
    };
    h.audit_approval(&parts, &a, 201);
    write_json(StatusCode::CREATED, approval_to_dto(a))
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ApproveAllReq {
    #[serde(default)]
    repo: String,
    #[serde(default)]
    note: String,
    /// Approves only packages whose stored scan is Clean (no known advisories),
    /// leaving vulnerable and unscanned packages pending. When false every
    /// pending package is approved.
    #[serde(default)]
    clean_only: bool,
}

/// Reports how many packages a bulk approve admitted. With `clean_only` set that
/// is fewer than the queue held, so the count is what tells the caller how much
/// is left.
#[derive(Debug, Clone, Serialize)]
struct BulkApproveResultDTO {
    approved: usize,
}

/// Approves pending packages in one proxy or hosted repository. Scoped to a
/// single repository so the per-repository approve permission check is
/// unambiguous; the response reports how many rows were approved. When
/// `clean_only` is set only packages with a Clean scan are approved.
pub(super) async fn approve_all_pending(
    State(h): State<Arc<Handler>>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let Ok(req) = serde_json::from_slice::<ApproveAllReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    // A bulk approval records who vouched for the whole queue, so the comment is
    // mandatory (unlike a single-package decision, where it is optional).
    let note = req.note.trim().to_string();
    if note.is_empty() {
        return write_error(StatusCode::BAD_REQUEST, "approval comment is required");
    }
    let repo_row = match h.store.get_repository_by_name(req.repo.trim()).await {
        Ok(repo_row) => repo_row,
        Err(err) => return map_error(err),
    };
    if repo_row.r#type != meta::TYPE_PROXY && repo_row.r#type != meta::TYPE_HOSTED {
        return write_error(
            StatusCode::BAD_REQUEST,
            "approval is only valid for proxy or hosted repositories",
        );
    }
    if let Some(response) = h.can_approve(&parts, &repo_row.name) {
        return response;
    }
    let approved = if req.clean_only {
        h.store
            .approve_all_pending_clean(
                &repo_row.name,
                repo::osv_ecosystem(&repo_row.format),
                &principal_name(&parts),
                &note,
            )
            .await
    } else {
        h.store
            .approve_all_pending(&repo_row.name, &principal_name(&parts), &note)
            .await
    };
    let approved = match approved {
        Ok(approved) => approved,
        Err(err) => return map_error(err),
    };
    for a in &approved {
        h.audit_approval(&parts, a, 200);
    }
    write_json(
        StatusCode::OK,
        BulkApproveResultDTO {
            approved: approved.len(),
        },
    )
}

#[derive(Debug, Clone, Default, Deserialize)]
struct DecideApprovalReq {
    #[serde(default)]
    note: String,
}

pub(super) async fn approve(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    decide(h, id, request, meta::APPROVAL_APPROVED).await
}

pub(super) async fn reject(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    decide(h, id, request, meta::APPROVAL_REJECTED).await
}

/// Flips one approval row to the given status. Re-deciding is allowed: approving
/// a rejected package (and vice versa) takes effect on the next package request
/// because the gate runs before any cache lookup.
async fn decide(h: Arc<Handler>, id: String, request: Request, status: &str) -> Response {
    let (parts, body) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    // The body is optional.
    let req = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => serde_json::from_slice::<DecideApprovalReq>(&bytes).unwrap_or_default(),
        Err(_) => DecideApprovalReq::default(),
    };
    // Resolve the row first: the per-repository permission check needs its repo.
    let existing = match h.store.get_approval(id).await {
        Ok(existing) => existing,
        Err(err) => return map_error(err),
    };
    if let Some(response) = h.can_approve(&parts, &existing.repo_name) {
        return response;
    }
    if let Err(err) = h
        .store
        .decide_approval(id, status, &principal_name(&parts), &req.note)
        .await
    {
        return map_error(err);
    }
    let a = match h.store.get_approval(id).await {
        Ok(a) => a,
        Err(err) => return map_error(err),
    };
    h.audit_approval(&parts, &a, 200);
    write_json(StatusCode::OK, approval_to_dto(a))
}

impl Handler {
    /// Enforces the per-repository approve permission (admin qualifies via
    /// admin-implies-all). Only the approve action grants it: an auditor may read
    /// the approvals surface but not decide, so the audit action is deliberately
    /// not accepted here.
    ///
    pub(super) fn can_approve(&self, parts: &Parts, repo_name: &str) -> Option<Response> {
        // No authorization service (tests) allows everything.
        let allowed = self.authz.is_none()
            || auth::from_request_parts(parts)
                .is_some_and(|p| p.can(repo_name, auth::ACTION_APPROVE));
        if allowed {
            return None;
        }
        Some(write_error(
            StatusCode::FORBIDDEN,
            &format!("approve permission required for repository {repo_name}"),
        ))
    }

    /// Records an approval decision in the repository's audit log, with the
    /// package name in the path column.
    fn audit_approval(&self, parts: &Parts, a: &PackageApproval, status: i64) {
        let Some(rec) = &self.rec else {
            return;
        };
        let event = if a.status == meta::APPROVAL_REJECTED {
            meta::EVENT_APPROVAL_REJECT
        } else {
            meta::EVENT_APPROVAL_APPROVE
        };
        rec.record(audit::Event {
            repo: a.repo_name.clone(),
            action: event.to_string(),
            path: a.package.clone(),
            username: principal_name(parts),
            method: parts.method.to_string(),
            status,
            client_ip: audit::client_ip_parts(parts),
            user_agent: super::header_str(parts, http::header::USER_AGENT),
            ..Default::default()
        });
    }
}

pub(super) fn valid_approval_status(s: &str) -> bool {
    s == meta::APPROVAL_PENDING || s == meta::APPROVAL_APPROVED || s == meta::APPROVAL_REJECTED
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;

    use axum::body::Body;
    use http::{Method, Request, StatusCode};

    use crate::repoconfig;

    use crate::testing::api::{ADMIN_USER, mk_proxy_repo, new_test_server};

    #[tokio::test]
    async fn approval_lifecycle() {
        let srv = new_test_server().await;
        mk_proxy_repo(&srv, "npmjs").await;

        // Manual pre-approval.
        let resp = srv
            .admin_do(
                Method::POST,
                "/approvals",
                r#"{"repo":"npmjs","package":"lodash","status":"approved","note":"trusted"}"#,
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::CREATED,
            "create approval: status={}",
            resp.status
        );
        let created = resp.json();
        assert_eq!(created["status"], "approved", "created = {created}");
        let created_id = created["id"].as_i64().expect("approval id");

        // Re-decide: reject the approved package.
        let resp = srv
            .admin_do(
                Method::POST,
                &format!("/approvals/{created_id}/reject"),
                r#"{"note":"incident"}"#,
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::OK,
            "reject: status={}",
            resp.status
        );
        let decided = resp.json();
        assert!(
            decided["status"] == "rejected"
                && decided["decided_by"] == ADMIN_USER
                && decided["note"] == "incident",
            "decided = {decided}"
        );

        // Approve again (body optional).
        let resp = srv
            .admin_do(
                Method::POST,
                &format!("/approvals/{created_id}/approve"),
                "",
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::OK,
            "approve: status={}",
            resp.status
        );

        // List with filters.
        let list = srv
            .admin_do(Method::GET, "/approvals?repo=npmjs&status=approved", "")
            .await
            .json();
        assert!(
            list["count"] == 1
                && list["approvals"].as_array().map(Vec::len) == Some(1)
                && list["approvals"][0]["package"] == "lodash",
            "list = {list}"
        );

        // Count endpoint (badge).
        let cnt = srv
            .admin_do(Method::GET, "/approvals/count?status=pending", "")
            .await
            .json();
        assert_eq!(cnt["count"], 0, "pending count = {}", cnt["count"]);

        // Decide on unknown id.
        let resp = srv
            .admin_do(Method::POST, "/approvals/9999/approve", "")
            .await;
        assert_eq!(
            resp.status,
            StatusCode::NOT_FOUND,
            "unknown id: status={}",
            resp.status
        );
    }

    #[tokio::test]
    async fn get_approval_with_vuln() {
        let srv = new_test_server().await;
        mk_proxy_repo(&srv, "npmjs").await;

        // A pending request without a resolved version, plus a package-level scan.
        srv.store
            .upsert_pending_approval("npmjs", "express", "alice", "")
            .await
            .expect("upsert pending approval");
        srv.store
            .upsert_vuln_scan(
                "npm",
                "express",
                "",
                "medium",
                &["CVE-1".to_string(), "CVE-2".to_string()],
                &HashMap::from([("medium".to_string(), 2)]),
                0,
                &[],
                "OSV",
            )
            .await
            .expect("upsert vuln scan");
        let rows = srv
            .store
            .list_approvals("npmjs", "pending", 10, 0)
            .await
            .expect("list pending");
        assert_eq!(rows.len(), 1, "list pending: rows={}", rows.len());

        let resp = srv
            .admin_do(Method::GET, &format!("/approvals/{}", rows[0].id), "")
            .await;
        assert_eq!(
            resp.status,
            StatusCode::OK,
            "get approval: status={}",
            resp.status
        );
        let dto = resp.json();
        // Version unknown -> package-level scan surfaced with scope "package".
        assert!(
            dto["package"] == "express"
                && dto["vuln_severity"] == "medium"
                && dto["vuln_scope"] == "package"
                && dto["vuln_ids"].as_array().map(Vec::len) == Some(2),
            "dto = {dto}"
        );
        assert!(
            !dto["vuln_scanned_at"].is_null(),
            "vuln_scanned_at should be set for a scanned coordinate"
        );
        // The seeded admin can approve any repository and must be listed as a
        // reviewer.
        assert!(
            dto["reviewers"]
                .as_array()
                .map(|r| r.iter().any(|v| v == ADMIN_USER))
                .unwrap_or(false),
            "reviewers = {}, want to contain {ADMIN_USER:?}",
            dto["reviewers"]
        );

        // Unknown id -> 404.
        let resp = srv.admin_do(Method::GET, "/approvals/9999", "").await;
        assert_eq!(
            resp.status,
            StatusCode::NOT_FOUND,
            "unknown id: status={}",
            resp.status
        );
    }

    #[tokio::test]
    async fn approval_validation() {
        let srv = new_test_server().await;
        mk_proxy_repo(&srv, "npmjs").await;

        for (name, body) in [
            ("missing package", r#"{"repo":"npmjs","status":"approved"}"#),
            (
                "bad status",
                r#"{"repo":"npmjs","package":"x","status":"pending"}"#,
            ),
            (
                "unknown repo",
                r#"{"repo":"nope","package":"x","status":"approved"}"#,
            ),
        ] {
            let resp = srv.admin_do(Method::POST, "/approvals", body).await;
            assert!(
                resp.status == StatusCode::BAD_REQUEST || resp.status == StatusCode::NOT_FOUND,
                "{name}: status={}",
                resp.status
            );
        }

        // Hosted repos share the same package approval model as proxies so browser
        // uploads can be quarantined before they are served.
        srv.admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"npm-hosted","format":"npm","type":"hosted"}"#,
        )
        .await;
        let resp = srv
            .admin_do(
                Method::POST,
                "/approvals",
                r#"{"repo":"npm-hosted","package":"x","status":"approved"}"#,
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::CREATED,
            "approval on hosted: status={}",
            resp.status
        );
        let mut cfg = repoconfig::default();
        cfg.approval.enabled = true;
        let hosted_body = serde_json::json!({
            "name": "npm-h2", "format": "npm", "type": "hosted", "config": cfg,
        })
        .to_string();
        let resp = srv
            .admin_do(Method::POST, "/repositories", &hosted_body)
            .await;
        assert_eq!(
            resp.status,
            StatusCode::CREATED,
            "approval config on hosted: status={}",
            resp.status
        );

        // Unauthenticated access is denied.
        let plain = srv.anon_do(Method::GET, "/approvals", "").await;
        assert!(
            plain.status == StatusCode::UNAUTHORIZED || plain.status == StatusCode::FORBIDDEN,
            "unauthenticated: status={}",
            plain.status
        );
    }

    /// Covers the read-only auditor: the audit action grants read access to the
    /// admin surfaces (users, roles, audit logs) and to the package approval surface
    /// (queue, version denies), but no mutations anywhere -- deciding and
    /// bulk-approving require the approve action.
    #[tokio::test]
    async fn auditor_role() {
        let srv = new_test_server().await;
        let repo_id = mk_proxy_repo(&srv, "npmjs").await;

        let resp = srv
            .admin_do(
                Method::POST,
                "/users",
                r#"{"username":"aud1","password":"pw123456"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
        let user_id = resp.json()["id"].as_i64().expect("user id");
        let resp = srv
            .admin_do(
                Method::POST,
                "/roles",
                r#"{"name":"auditor","description":"read-only auditor"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
        let role_id = resp.json()["id"].as_i64().expect("role id");
        let resp = srv
            .admin_do(
                Method::POST,
                &format!("/roles/{role_id}/permissions"),
                r#"{"repo_pattern":"*","actions":["read","audit"]}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
        let resp = srv
            .admin_do(
                Method::POST,
                &format!("/users/{user_id}/roles"),
                &format!(r#"{{"role_id":{role_id}}}"#),
            )
            .await;
        assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());

        // /me reports the auditor capability but not admin.
        let me = srv
            .do_as("aud1", "pw123456", Method::GET, "/me", "")
            .await
            .json();
        assert!(me["admin"] == false && me["auditor"] == true, "me = {me}");

        // Read-only admin surfaces are allowed.
        for path in [
            "/users".to_string(),
            "/roles".to_string(),
            format!("/repositories/{repo_id}/audit-logs"),
        ] {
            let resp = srv.do_as("aud1", "pw123456", Method::GET, &path, "").await;
            assert_eq!(resp.status, StatusCode::OK, "{path}: {}", resp.text());
        }

        // The approval surface is readable: the queue and version-deny list are
        // visible to an auditor.
        for path in ["/approvals", "/approvals/count", "/version-denies"] {
            let resp = srv.do_as("aud1", "pw123456", Method::GET, path, "").await;
            assert_eq!(resp.status, StatusCode::OK, "{path}: {}", resp.text());
        }

        // But deciding, bulk-approving and managing denies are all forbidden: they
        // require the approve action, which the auditor does not hold.
        let resp = srv
            .admin_do(
                Method::POST,
                "/approvals",
                r#"{"repo":"npmjs","package":"left-pad","status":"rejected"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
        let row_id = resp.json()["id"].as_i64().expect("approval id");
        let resp = srv
            .do_as(
                "aud1",
                "pw123456",
                Method::POST,
                &format!("/approvals/{row_id}/approve"),
                r#"{"note":"nope"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
        let resp = srv
            .do_as(
                "aud1",
                "pw123456",
                Method::POST,
                "/approvals",
                r#"{"repo":"npmjs","package":"axios","status":"approved"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
        let resp = srv
            .do_as(
                "aud1",
                "pw123456",
                Method::POST,
                "/approvals/approve-all",
                r#"{"repo":"npmjs","note":"nope"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
        let resp = srv
            .do_as(
                "aud1",
                "pw123456",
                Method::POST,
                "/version-denies",
                r#"{"repo":"npmjs","package":"evil","version":"1.0.0","reason":"nope"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());

        // Mutations outside the approval surface remain forbidden.
        let resp = srv
            .do_as(
                "aud1",
                "pw123456",
                Method::POST,
                "/users",
                r#"{"username":"x","password":"pw123456"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
        let resp = srv
            .do_as(
                "aud1",
                "pw123456",
                Method::POST,
                "/repositories",
                r#"{"name":"nope","format":"npm","type":"hosted"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
        let resp = srv
            .do_as(
                "aud1",
                "pw123456",
                Method::DELETE,
                &format!("/roles/{role_id}"),
                "",
            )
            .await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
    }

    /// Covers the non-admin approver flow: a role with the approve action on a repo
    /// pattern can run the approvals API for matching repositories only, and gets no
    /// other admin surface.
    #[tokio::test]
    async fn approve_action_for_security_engineers() {
        let srv = new_test_server().await;
        mk_proxy_repo(&srv, "npm-gated").await;

        // pypi proxy outside the security role's npm-* pattern.
        let resp = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"pypi-gated","format":"pypi","type":"proxy","upstream_url":"https://pypi.org/simple"}"#,
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());

        // Security engineer: approve on npm-* plus read everywhere.
        let resp = srv
            .admin_do(
                Method::POST,
                "/users",
                r#"{"username":"sec1","password":"pw123456"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
        let user_id = resp.json()["id"].as_i64().expect("user id");
        let resp = srv
            .admin_do(
                Method::POST,
                "/roles",
                r#"{"name":"security","description":"package approvers"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
        let role_id = resp.json()["id"].as_i64().expect("role id");
        let resp = srv
            .admin_do(
                Method::POST,
                &format!("/roles/{role_id}/permissions"),
                r#"{"repo_pattern":"npm-*","actions":["read","approve"]}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
        let resp = srv
            .admin_do(
                Method::POST,
                &format!("/users/{user_id}/roles"),
                &format!(r#"{{"role_id":{role_id}}}"#),
            )
            .await;
        assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());

        // Pending rows in both repos (seeded by admin pre-decisions, then flipped to
        // pending is not possible via API, so use create + the queue endpoint).
        let resp = srv
            .admin_do(
                Method::POST,
                "/approvals",
                r#"{"repo":"npm-gated","package":"left-pad","status":"rejected"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
        let npm_row_id = resp.json()["id"].as_i64().expect("approval id");
        let resp = srv
            .admin_do(
                Method::POST,
                "/approvals",
                r#"{"repo":"pypi-gated","package":"requests","status":"rejected"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
        let pypi_row_id = resp.json()["id"].as_i64().expect("approval id");

        // /me reports the approver capability.
        let me = srv
            .do_as("sec1", "pw123456", Method::GET, "/me", "")
            .await
            .json();
        assert!(me["admin"] == false && me["approver"] == true, "me = {me}");

        // The shared queue is visible.
        let resp = srv
            .do_as("sec1", "pw123456", Method::GET, "/approvals", "")
            .await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
        let resp = srv
            .do_as("sec1", "pw123456", Method::GET, "/approvals/count", "")
            .await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());

        // Deciding inside the pattern works; outside it is forbidden.
        let resp = srv
            .do_as(
                "sec1",
                "pw123456",
                Method::POST,
                &format!("/approvals/{npm_row_id}/approve"),
                r#"{"note":"sec review"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
        let decided = resp.json();
        assert!(
            decided["status"] == "approved" && decided["decided_by"] == "sec1",
            "decided = {decided}"
        );
        let resp = srv
            .do_as(
                "sec1",
                "pw123456",
                Method::POST,
                &format!("/approvals/{pypi_row_id}/approve"),
                "",
            )
            .await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());

        // Manual pre-decisions follow the same pattern scoping.
        let resp = srv
            .do_as(
                "sec1",
                "pw123456",
                Method::POST,
                "/approvals",
                r#"{"repo":"npm-gated","package":"axios","status":"approved"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
        let resp = srv
            .do_as(
                "sec1",
                "pw123456",
                Method::POST,
                "/approvals",
                r#"{"repo":"pypi-gated","package":"boto3","status":"approved"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());

        // No other admin surface leaks through (repository listing is now readable
        // by any authenticated user, so probe a still-admin-only endpoint instead).
        let resp = srv
            .do_as("sec1", "pw123456", Method::GET, "/users", "")
            .await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());

        // A PAT minted by the approver cannot approve: token scopes never carry the
        // approve action, so `require_approver` rejects token-authenticated
        // principals.
        let resp = srv
        .do_as(
            "sec1",
            "pw123456",
            Method::POST,
            "/tokens",
            r#"{"name":"ci","description":"ci token","scopes":[{"repo_pattern":"*","actions":["read"]}],"expires_in":"720h"}"#,
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
        let token = resp.json()["token"].as_str().expect("token").to_string();
        let request = Request::builder()
            .method(Method::GET)
            .uri("/approvals")
            .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .expect("build request");
        let tresp = srv.send(request).await;
        assert_eq!(
            tresp.status,
            StatusCode::FORBIDDEN,
            "PAT approvals access: status={}, want 403",
            tresp.status
        );

        // Tokens still cannot be minted with an approve scope at all.
        let resp = srv
        .do_as(
            "sec1",
            "pw123456",
            Method::POST,
            "/tokens",
            r#"{"name":"bad","description":"x","scopes":[{"repo_pattern":"*","actions":["approve"]}],"expires_in":"720h"}"#,
        )
        .await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
    }

    #[tokio::test]
    async fn approve_all_pending_endpoint() {
        let srv = new_test_server().await;
        mk_proxy_repo(&srv, "npmjs").await;
        mk_proxy_repo(&srv, "pypi").await;

        // Seed pending demand directly: the public API can't create pending rows.
        for p in ["left-pad", "is-odd", "lodash"] {
            srv.store
                .upsert_pending_approval("npmjs", p, "alice", "")
                .await
                .expect("upsert pending approval");
        }
        srv.store
            .upsert_pending_approval("pypi", "requests", "bob", "")
            .await
            .expect("upsert pending approval");

        // Approve every pending package in npmjs only.
        let resp = srv
            .admin_do(
                Method::POST,
                "/approvals/approve-all",
                r#"{"repo":"npmjs","note":"batch reviewed"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
        let got = resp.json();
        assert_eq!(got["approved"], 3, "approved = {}, want 3", got["approved"]);

        // npmjs now has no pending rows; pypi is untouched.
        let n = srv
            .store
            .count_approvals("npmjs", "pending")
            .await
            .unwrap_or_default();
        assert_eq!(n, 0, "npmjs pending = {n}, want 0");
        let n = srv
            .store
            .count_approvals("pypi", "pending")
            .await
            .unwrap_or_default();
        assert_eq!(n, 1, "pypi pending = {n}, want 1");

        // Re-running approves nothing.
        let resp = srv
            .admin_do(
                Method::POST,
                "/approvals/approve-all",
                r#"{"repo":"npmjs","note":"again"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
        assert_eq!(resp.json()["approved"], 0, "second run should approve 0");

        // The approval comment is mandatory for a bulk approve.
        let resp = srv
            .admin_do(
                Method::POST,
                "/approvals/approve-all",
                r#"{"repo":"npmjs"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
        let resp = srv
            .admin_do(
                Method::POST,
                "/approvals/approve-all",
                r#"{"repo":"npmjs","note":"   "}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());

        // Unknown repo is a 404 (comment supplied so it passes the note gate).
        let resp = srv
            .admin_do(
                Method::POST,
                "/approvals/approve-all",
                r#"{"repo":"nope","note":"x"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::NOT_FOUND, "{}", resp.text());
    }

    #[tokio::test]
    async fn approval_deleted_with_repo() {
        let srv = new_test_server().await;
        // A non-seed name: predefined repositories are protected from deletion.
        let id = mk_proxy_repo(&srv, "npm-proxy").await;

        srv.admin_do(
            Method::POST,
            "/approvals",
            r#"{"repo":"npm-proxy","package":"lodash","status":"approved"}"#,
        )
        .await;

        let resp = srv
            .admin_do(Method::DELETE, &format!("/repositories/{id}"), "")
            .await;
        assert_eq!(
            resp.status,
            StatusCode::NO_CONTENT,
            "delete repo: status={}",
            resp.status
        );

        let list = srv
            .admin_do(Method::GET, "/approvals?repo=npm-proxy", "")
            .await
            .json();
        assert_eq!(
            list["count"], 0,
            "approvals survived repo deletion: count={}",
            list["count"]
        );
    }
}
