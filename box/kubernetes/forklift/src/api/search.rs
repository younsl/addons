use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::response::Response;
use http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::auth;

use super::repositories::int_param;
use super::{Handler, map_error, write_error, write_json};

// Global search result shapes. Each section is `None` (JSON `null`) when the
// principal is not allowed to see that surface at all, and an empty array when
// permitted but nothing matched, so the UI can tell "no access" from "no hit".
#[derive(Debug, Clone, Serialize)]
struct SearchRepoDTO {
    id: i64,
    name: String,
    format: String,
    r#type: String,
}

#[derive(Debug, Clone, Serialize)]
struct SearchArtifactDTO {
    repo_id: i64,
    repo_name: String,
    path: String,
    size: i64,
}

/// One artifact label match. The label is its own section rather than folded
/// into the artifacts list because that is how it is looked for: an operator
/// searches "keep-forever" to see what carries it, and a path list would hide
/// which label matched.
#[derive(Debug, Clone, Serialize)]
struct SearchLabelDTO {
    repo_id: i64,
    repo_name: String,
    path: String,
    label: String,
}

#[derive(Debug, Clone, Serialize)]
struct SearchApprovalDTO {
    id: i64,
    repo_name: String,
    package: String,
    status: String,
}

#[derive(Debug, Clone, Serialize)]
struct SearchUserDTO {
    id: i64,
    username: String,
    robot: bool,
}

#[derive(Debug, Clone, Serialize)]
struct SearchRoleDTO {
    id: i64,
    name: String,
    description: String,
}

/// Sections the principal may not see stay `None` and serialize as `null`;
/// permitted sections are always present, so an empty array means "no match".
/// `skip_serializing_if` is deliberately not used: it cannot distinguish the
/// two. `counts` carries the exact total of matches per permitted section (keyed
/// like the section fields), which can exceed the truncated item lists capped at
/// the `limit` parameter.
#[derive(Debug, Clone, Default, Serialize)]
struct SearchResultDTO {
    query: String,
    repositories: Option<Vec<SearchRepoDTO>>,
    artifacts: Option<Vec<SearchArtifactDTO>>,
    labels: Option<Vec<SearchLabelDTO>>,
    approvals: Option<Vec<SearchApprovalDTO>>,
    users: Option<Vec<SearchUserDTO>>,
    roles: Option<Vec<SearchRoleDTO>>,
    counts: BTreeMap<String, i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct SearchQuery {
    #[serde(default)]
    pub(super) q: String,
    #[serde(default)]
    pub(super) limit: String,
}

/// The global search behind the sidebar search box. It fans out over
/// repositories, artifacts, artifact labels, package approvals, users and roles,
/// returning only what the caller may see: repositories, artifacts and labels
/// are filtered per-repo by the read action, approvals require approve or audit
/// rights (the queue is shared, mirroring the `/approvals` surface), and
/// users/roles require admin or audit rights (mirroring `require_auditor`).
pub(super) async fn search(
    State(h): State<Arc<Handler>>,
    Query(query): Query<SearchQuery>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let q = query.q.trim().to_string();
    if q.is_empty() {
        return write_error(StatusCode::BAD_REQUEST, "missing query parameter q");
    }
    let limit = int_param(&query.limit, 5).clamp(1, 20) as usize;

    let principal = auth::from_request_parts(&parts);
    let authz_on = h.authz.is_some();
    let can_read_repo = |name: &str| {
        !authz_on
            || principal
                .as_ref()
                .is_some_and(|p| p.can(name, auth::ACTION_READ))
    };
    let can_see_approvals = !authz_on
        || principal
            .as_ref()
            .is_some_and(|p| p.is_admin() || p.can_approve_any() || p.can_audit_any());
    let can_see_admin_reads = !authz_on
        || principal
            .as_ref()
            .is_some_and(|p| p.is_admin() || p.can_audit_any());

    let mut out = SearchResultDTO {
        query: q.clone(),
        ..Default::default()
    };
    let needle = q.to_lowercase();

    // Repositories: every authenticated user, narrowed to readable repos.
    let repos = match h.store.list_repositories().await {
        Ok(repos) => repos,
        Err(err) => return map_error(err),
    };
    let mut repositories = Vec::new();
    out.counts.insert("repositories".to_string(), 0);
    for repo in repos {
        if !repo.name.to_lowercase().contains(&needle) || !can_read_repo(&repo.name) {
            continue;
        }
        *out.counts.entry("repositories".to_string()).or_default() += 1;
        if repositories.len() < limit {
            repositories.push(SearchRepoDTO {
                id: repo.id,
                name: repo.name,
                format: repo.format,
                r#type: repo.r#type,
            });
        }
    }
    out.repositories = Some(repositories);

    // Artifacts: matched by path, then narrowed to readable repositories. The
    // store query is over-fetched so per-repo filtering can still fill the page;
    // the exact total comes from a per-repo count summed over readable repos.
    let hits = match h.store.search_artifacts(&q, 200).await {
        Ok(hits) => hits,
        Err(err) => return map_error(err),
    };
    let mut artifacts = Vec::new();
    for hit in hits {
        if artifacts.len() >= limit {
            break;
        }
        if can_read_repo(&hit.repo_name) {
            artifacts.push(SearchArtifactDTO {
                repo_id: hit.repo_id,
                repo_name: hit.repo_name,
                path: hit.path,
                size: hit.size,
            });
        }
    }
    out.artifacts = Some(artifacts);
    let artifact_counts = match h.store.search_artifact_counts_by_repo(&q).await {
        Ok(counts) => counts,
        Err(err) => return map_error(err),
    };
    let mut total = 0;
    for (repo_name, n) in artifact_counts {
        if can_read_repo(&repo_name) {
            total += n;
        }
    }
    out.counts.insert("artifacts".to_string(), total);

    // Artifact labels, narrowed to readable repositories the same way. Same
    // over-fetch as the artifact hits above, for the same reason.
    let label_hits = match h.store.search_artifact_labels(&q, 200).await {
        Ok(hits) => hits,
        Err(err) => return map_error(err),
    };
    let mut labels = Vec::new();
    for hit in label_hits {
        if labels.len() >= limit {
            break;
        }
        if can_read_repo(&hit.repo_name) {
            labels.push(SearchLabelDTO {
                repo_id: hit.repo_id,
                repo_name: hit.repo_name,
                path: hit.path,
                label: hit.label,
            });
        }
    }
    out.labels = Some(labels);
    let label_counts = match h.store.search_artifact_label_counts_by_repo(&q).await {
        Ok(counts) => counts,
        Err(err) => return map_error(err),
    };
    let mut total = 0;
    for (repo_name, n) in label_counts {
        if can_read_repo(&repo_name) {
            total += n;
        }
    }
    out.counts.insert("labels".to_string(), total);

    if can_see_approvals {
        let approvals = match h.store.search_approvals(&q, limit as i64).await {
            Ok(approvals) => approvals,
            Err(err) => return map_error(err),
        };
        out.approvals = Some(
            approvals
                .into_iter()
                .map(|a| SearchApprovalDTO {
                    id: a.id,
                    repo_name: a.repo_name,
                    package: a.package,
                    status: a.status,
                })
                .collect(),
        );
        match h.store.search_approvals_count(&q).await {
            Ok(count) => {
                out.counts.insert("approvals".to_string(), count);
            }
            Err(err) => return map_error(err),
        }
    }

    if can_see_admin_reads {
        let users = match h.store.list_users().await {
            Ok(users) => users,
            Err(err) => return map_error(err),
        };
        let mut matched = Vec::new();
        let mut count = 0;
        for user in users {
            if !user.username.to_lowercase().contains(&needle) {
                continue;
            }
            count += 1;
            if matched.len() < limit {
                matched.push(SearchUserDTO {
                    id: user.id,
                    username: user.username,
                    robot: user.robot,
                });
            }
        }
        out.users = Some(matched);
        out.counts.insert("users".to_string(), count);

        let roles = match h.store.list_roles().await {
            Ok(roles) => roles,
            Err(err) => return map_error(err),
        };
        let mut matched = Vec::new();
        let mut count = 0;
        for role in roles {
            if !role.name.to_lowercase().contains(&needle) {
                continue;
            }
            count += 1;
            if matched.len() < limit {
                matched.push(SearchRoleDTO {
                    id: role.id,
                    name: role.name,
                    description: role.description,
                });
            }
        }
        out.roles = Some(matched);
        out.counts.insert("roles".to_string(), count);
    }

    write_json(StatusCode::OK, out)
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::testing::api::mk_role_user;
    use http::{Method, StatusCode};
    use serde_json::Value;

    use crate::meta::Artifact;

    use crate::testing::api::{ADMIN_PASS, ADMIN_USER, mk_proxy_repo, new_test_server};

    /// Covers the permission-aware sidebar search: repositories and artifacts narrow
    /// to readable repos, approvals require approve/audit rights, users and roles
    /// require admin/audit rights. Sections without access come back null,
    /// permitted-but-empty sections come back `[]`.
    #[tokio::test]
    async fn global_search() {
        let srv = new_test_server().await;

        let npm_id = mk_proxy_repo(&srv, "npmjs").await;
        let priv_id = mk_proxy_repo(&srv, "npm-internal").await;
        for a in [
            Artifact {
                repo_id: npm_id,
                path: "lodash/-/lodash-4.17.21.tgz".to_string(),
                version: "4.17.21".to_string(),
                blob_sha256: "b1".to_string(),
                size: 10,
                ..Default::default()
            },
            Artifact {
                repo_id: priv_id,
                path: "lodash-fork/-/lodash-fork-1.0.0.tgz".to_string(),
                version: "1.0.0".to_string(),
                blob_sha256: "b2".to_string(),
                size: 5,
                ..Default::default()
            },
        ] {
            srv.store.put_artifact(a).await.expect("put artifact");
        }
        srv.store
            .upsert_pending_approval("npmjs", "lodash", "someone", "4.17.21")
            .await
            .expect("upsert approval");

        // reader can read only npmjs; auditor reads everything, read-only.
        mk_role_user(
            &srv,
            "reader1",
            "pw123456",
            "npm-reader",
            "npmjs",
            r#""read""#,
        )
        .await;
        mk_role_user(
            &srv,
            "aud2",
            "pw123456",
            "auditor2",
            "*",
            r#""read","audit""#,
        )
        .await;

        let search = async |user: &str, pass: &str, q: &str| -> Value {
            let resp = srv
                .do_as(user, pass, Method::GET, &format!("/search?q={q}"), "")
                .await;
            assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
            resp.json()
        };

        // Admin sees every section, with exact per-section totals.
        let got = search(ADMIN_USER, ADMIN_PASS, "lodash").await;
        assert_eq!(
            got["artifacts"].as_array().map(Vec::len),
            Some(2),
            "admin artifacts = {}, want 2",
            got["artifacts"]
        );
        assert!(
            got["counts"]["artifacts"] == 2
                && got["counts"]["approvals"] == 1
                && got["counts"]["repositories"] == 0,
            "admin counts = {}",
            got["counts"]
        );
        assert!(
            got["approvals"].as_array().map(Vec::len) == Some(1)
                && got["approvals"][0]["package"] == "lodash",
            "admin approvals = {}",
            got["approvals"]
        );
        assert!(
            !got["users"].is_null() && !got["roles"].is_null(),
            "admin users/roles sections missing: {got}"
        );
        let got = search(ADMIN_USER, ADMIN_PASS, "npm").await;
        assert_eq!(
            got["repositories"].as_array().map(Vec::len),
            Some(2),
            "admin repos = {}, want 2",
            got["repositories"]
        );

        // Reader: only the readable repo's artifact; admin-ish sections are null.
        let got = search("reader1", "pw123456", "lodash").await;
        assert!(
            got["artifacts"].as_array().map(Vec::len) == Some(1)
                && got["artifacts"][0]["repo_name"] == "npmjs",
            "reader artifacts = {}, want only npmjs",
            got["artifacts"]
        );
        assert_eq!(
            got["counts"]["artifacts"], 1,
            "reader artifact count = {}, want 1",
            got["counts"]
        );
        assert!(
            got["counts"].get("users").is_none(),
            "reader counts leak restricted sections: {}",
            got["counts"]
        );
        assert!(
            got["approvals"].is_null() && got["users"].is_null() && got["roles"].is_null(),
            "reader sees restricted sections: {got}"
        );
        let got = search("reader1", "pw123456", "npm").await;
        assert!(
            got["repositories"].as_array().map(Vec::len) == Some(1)
                && got["repositories"][0]["name"] == "npmjs",
            "reader repos = {}, want only npmjs",
            got["repositories"]
        );

        // Auditor: approvals and users/roles are visible (read-only surfaces).
        let got = search("aud2", "pw123456", "lodash").await;
        assert_eq!(
            got["approvals"].as_array().map(Vec::len),
            Some(1),
            "auditor approvals = {}, want 1",
            got["approvals"]
        );
        assert!(
            !got["users"].is_null() && !got["roles"].is_null(),
            "auditor users/roles sections missing: {got}"
        );
        let got = search("aud2", "pw123456", "reader1").await;
        assert!(
            got["users"].as_array().map(Vec::len) == Some(1)
                && got["users"][0]["username"] == "reader1",
            "auditor user search = {}",
            got["users"]
        );

        // Missing q -> 400; unauthenticated -> 401.
        let resp = srv.admin_do(Method::GET, "/search", "").await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
        let resp = srv.anon_do(Method::GET, "/search?q=x", "").await;
        assert_eq!(
            resp.status,
            StatusCode::UNAUTHORIZED,
            "anonymous search = {}, want 401",
            resp.status
        );
    }
}
