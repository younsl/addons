use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::response::{IntoResponse as _, Response};
use chrono::{DateTime, Utc};
use http::StatusCode;
use http::request::Parts;
use serde::{Deserialize, Serialize};

use crate::audit;
use crate::meta;

use super::repositories::{int_param, path_id};
use super::{Handler, map_error, principal_name, write_error, write_json};

/// The JSON shape for one version deny entry.
#[derive(Debug, Clone, Serialize)]
struct VersionDenyDTO {
    id: i64,
    repo_name: String,
    package: String,
    version: String,
    reason: String,
    created_by: String,
    created_at: DateTime<Utc>,
}

fn version_deny_to_dto(d: meta::VersionDeny) -> VersionDenyDTO {
    VersionDenyDTO {
        id: d.id,
        repo_name: d.repo_name,
        package: d.package,
        version: d.version,
        reason: d.reason,
        created_by: d.created_by,
        created_at: d.created_at,
    }
}

/// One page of the per-version deny list. `count` is every rule matching the
/// filter, not just this page.
#[derive(Debug, Clone, Serialize)]
struct VersionDenyListDTO {
    count: i64,
    denies: Vec<VersionDenyDTO>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct ListQuery {
    #[serde(default)]
    pub(super) repo: String,
    #[serde(default)]
    pub(super) limit: String,
    #[serde(default)]
    pub(super) offset: String,
}

/// Returns deny entries, newest first, with an optional repo filter and
/// limit/offset pagination.
pub(super) async fn list(State(h): State<Arc<Handler>>, Query(q): Query<ListQuery>) -> Response {
    let mut limit = int_param(&q.limit, 100);
    if !(1..=500).contains(&limit) {
        limit = 100;
    }
    let offset = int_param(&q.offset, 0).max(0);

    let rows = match h.store.list_version_denies(&q.repo, limit, offset).await {
        Ok(rows) => rows,
        Err(err) => return map_error(err),
    };
    let count = match h.store.count_version_denies(&q.repo).await {
        Ok(count) => count,
        Err(err) => return map_error(err),
    };
    write_json(
        StatusCode::OK,
        VersionDenyListDTO {
            count,
            denies: rows.into_iter().map(version_deny_to_dto).collect(),
        },
    )
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CreateVersionDenyReq {
    #[serde(default)]
    repo: String,
    #[serde(default)]
    package: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    reason: String,
}

/// Blocks one exact (package, version) in a proxy repository. The deny takes
/// effect on the next request: the gate runs before any cache lookup, so
/// already-cached copies stop being served immediately.
pub(super) async fn create(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let Ok(req) = serde_json::from_slice::<CreateVersionDenyReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let package = req.package.trim();
    let version = req.version.trim();
    if package.is_empty() || version.is_empty() {
        return write_error(StatusCode::BAD_REQUEST, "package and version are required");
    }
    let repo = match h.store.get_repository_by_name(req.repo.trim()).await {
        Ok(repo) => repo,
        Err(err) => return map_error(err),
    };
    if repo.r#type != meta::TYPE_PROXY && repo.r#type != meta::TYPE_HOSTED {
        return write_error(
            StatusCode::BAD_REQUEST,
            "version denies are only valid for proxy or hosted repositories",
        );
    }
    if let Some(response) = h.can_approve(&parts, &repo.name) {
        return response;
    }
    let deny = match h
        .store
        .upsert_version_deny(
            &repo.name,
            package,
            version,
            &req.reason,
            &principal_name(&parts),
        )
        .await
    {
        Ok(deny) => deny,
        Err(err) => return map_error(err),
    };
    audit_version_deny(&h, &parts, &deny, meta::EVENT_DENY_CREATE, 201);
    write_json(StatusCode::CREATED, version_deny_to_dto(deny))
}

/// Removes one deny entry (un-deny). The version goes back through the regular
/// approval and age gates on its next request.
pub(super) async fn delete(
    State(h): State<Arc<Handler>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    // Resolve the row first: the per-repository permission check needs its repo.
    let deny = match h.store.get_version_deny(id).await {
        Ok(deny) => deny,
        Err(err) => return map_error(err),
    };
    if let Some(response) = h.can_approve(&parts, &deny.repo_name) {
        return response;
    }
    if let Err(err) = h.store.delete_version_deny(id).await {
        return map_error(err);
    }
    audit_version_deny(&h, &parts, &deny, meta::EVENT_DENY_DELETE, 204);
    StatusCode::NO_CONTENT.into_response()
}

/// Records a deny list change in the repository's audit log, with the
/// `package@version` coordinate in the path column.
fn audit_version_deny(
    h: &Arc<Handler>,
    parts: &Parts,
    d: &meta::VersionDeny,
    event: &str,
    status: i64,
) {
    let Some(rec) = &h.rec else {
        return;
    };
    rec.record(audit::Event {
        repo: d.repo_name.clone(),
        action: event.to_string(),
        path: format!("{}@{}", d.package, d.version),
        username: principal_name(parts),
        method: parts.method.to_string(),
        status,
        client_ip: audit::client_ip_parts(parts),
        user_agent: parts
            .headers
            .get(http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string(),
        ..Default::default()
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use http::{Method, StatusCode};

    use crate::api::valid_name;
    use crate::testing::api::{ADMIN_USER, mk_proxy_repo, new_test_server};

    #[tokio::test]
    async fn version_deny_lifecycle() {
        let srv = new_test_server().await;
        mk_proxy_repo(&srv, "npmjs").await;

        // Deny one exact version.
        let resp = srv
            .admin_do(
                Method::POST,
                "/version-denies",
                r#"{"repo":"npmjs","package":"lodash","version":"4.17.99","reason":"IOC"}"#,
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::CREATED,
            "create deny: status={}",
            resp.status
        );
        let created = resp.json();
        assert!(
            created["id"].as_i64() != Some(0)
                && created["version"] == "4.17.99"
                && created["created_by"] == ADMIN_USER,
            "created = {created}"
        );
        let created_id = created["id"].as_i64().expect("deny id");

        // Re-deny is idempotent (same row, refreshed reason).
        let resp = srv
        .admin_do(
            Method::POST,
            "/version-denies",
            r#"{"repo":"npmjs","package":"lodash","version":"4.17.99","reason":"CVE-2026-0001"}"#,
        )
        .await;
        let again = resp.json();
        assert!(
            again["id"].as_i64() == Some(created_id) && again["reason"] == "CVE-2026-0001",
            "re-deny = {again}, want same id"
        );

        // List with repo filter.
        let resp = srv
            .admin_do(Method::GET, "/version-denies?repo=npmjs", "")
            .await;
        let list = resp.json();
        assert!(
            list["count"] == 1
                && list["denies"].as_array().map(Vec::len) == Some(1)
                && list["denies"][0]["package"] == "lodash",
            "list = {list}"
        );

        // Remove, then verify the list is empty and double-delete 404s.
        let resp = srv
            .admin_do(Method::DELETE, &format!("/version-denies/{created_id}"), "")
            .await;
        assert_eq!(
            resp.status,
            StatusCode::NO_CONTENT,
            "delete: status={}",
            resp.status
        );
        let resp = srv
            .admin_do(Method::DELETE, &format!("/version-denies/{created_id}"), "")
            .await;
        assert_eq!(
            resp.status,
            StatusCode::NOT_FOUND,
            "double delete: status={}",
            resp.status
        );
    }

    #[tokio::test]
    async fn version_deny_validation() {
        let srv = new_test_server().await;
        mk_proxy_repo(&srv, "npmjs").await;

        // Hosted repos can revoke an uploaded version using the same immediate deny
        // control as cached proxy artifacts.
        let resp = srv
            .admin_do(
                Method::POST,
                "/repositories",
                r#"{"name":"npm-hosted","format":"npm","type":"hosted"}"#,
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::CREATED,
            "create hosted: status={}",
            resp.status
        );

        for (body, want) in [
            (
                r#"{"repo":"npmjs","package":"","version":"1.0.0"}"#,
                StatusCode::BAD_REQUEST,
            ),
            (
                r#"{"repo":"npmjs","package":"lodash","version":""}"#,
                StatusCode::BAD_REQUEST,
            ),
            (
                r#"{"repo":"missing","package":"lodash","version":"1.0.0"}"#,
                StatusCode::NOT_FOUND,
            ),
            (
                r#"{"repo":"npm-hosted","package":"lodash","version":"1.0"}"#,
                StatusCode::CREATED,
            ),
        ] {
            let resp = srv.admin_do(Method::POST, "/version-denies", body).await;
            assert_eq!(
                resp.status, want,
                "{body}: status={}, want {want}",
                resp.status
            );
        }
    }

    #[tokio::test]
    async fn version_deny_repo_delete_cleanup() {
        let srv = new_test_server().await;
        // A non-seed name: predefined repositories are protected from deletion.
        let id = mk_proxy_repo(&srv, "npm-proxy").await;

        let resp = srv
            .admin_do(
                Method::POST,
                "/version-denies",
                r#"{"repo":"npm-proxy","package":"lodash","version":"4.17.99"}"#,
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::CREATED,
            "create deny: status={}",
            resp.status
        );

        // Deleting the repository must drop its denies: a recreated same-name repo
        // would otherwise inherit them.
        let resp = srv
            .admin_do(Method::DELETE, &format!("/repositories/{id}"), "")
            .await;
        assert_eq!(
            resp.status,
            StatusCode::NO_CONTENT,
            "delete repo: status={}",
            resp.status
        );
        let resp = srv.admin_do(Method::GET, "/version-denies", "").await;
        let list = resp.json();
        assert_eq!(
            list["count"], 0,
            "denies after repo delete = {}, want 0",
            list["count"]
        );
    }

    #[test]
    fn valid_name_cases() {
        for (name, want) in [
            ("npm-proxy", true),
            ("Npm_Proxy-2", true),
            ("a", true),
            ("", false),
            ("npm proxy", false),
            ("npm.proxy", false),
            ("npm/проxy", false),
            ("한글", false),
        ] {
            assert_eq!(
                valid_name(name),
                want,
                "valid_name({name:?}) = {}, want {want}",
                valid_name(name)
            );
        }
        assert!(
            !valid_name(&"\0".repeat(65)),
            "65-char name must be invalid"
        );
    }

    #[tokio::test]
    async fn name_validation_on_create_endpoints() {
        let srv = new_test_server().await;

        for (url, body) in [
            (
                "/repositories",
                r#"{"name":"bad.name","format":"npm","type":"hosted"}"#,
            ),
            ("/roles", r#"{"name":"bad role"}"#),
            ("/users", r#"{"username":"bad user","password":"pw"}"#),
            (
                "/tokens",
                r#"{"name":"bad name","description":"d","scopes":[{"repo_pattern":"*","actions":["read"]}],"expires_in":"1h"}"#,
            ),
        ] {
            let resp = srv.admin_do(Method::POST, url, body).await;
            assert_eq!(
                resp.status,
                StatusCode::BAD_REQUEST,
                "{url} {body}: status={}, want 400",
                resp.status
            );
        }
    }
}
