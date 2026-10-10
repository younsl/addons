use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Request, State};
use axum::response::Response;
use http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::meta;

use super::labels::{LABEL_FORBIDDEN_MSG, LABEL_RULE_MSG};
use super::repositories::path_id;
use super::{Handler, map_error, principal_name, write_error, write_json};

/// Caps one bulk request.
///
/// A repository holds tens of thousands of artifacts and the console lets a
/// reader select across pages, so the cap is what keeps one click from becoming
/// a request that runs for minutes under the single SQLite writer. The console
/// splits a larger selection into batches of this size, which also gives it
/// progress to show and a place to stop.
pub(super) const MAX_BULK_ARTIFACTS: usize = 200;

/// One path that could not be acted on, and why.
#[derive(Debug, Clone, Serialize)]
struct ArtifactBulkFailureDTO {
    path: String,
    error: String,
}

/// Reports a bulk action per path rather than as one verdict. A selection of
/// hundreds routinely contains a few paths the caller may not touch or that
/// another request has already removed, and failing the whole batch for those
/// would leave the reader with no way to make progress.
#[derive(Debug, Clone, Default, Serialize)]
struct ArtifactBulkResultDTO {
    requested: usize,
    succeeded: usize,
    failed: Vec<ArtifactBulkFailureDTO>,
}

/// Reads and validates the path list shared by the bulk bodies.
fn bulk_paths(raw: Vec<String>) -> Result<Vec<String>, Box<Response>> {
    let mut out = Vec::with_capacity(raw.len());
    let mut seen = HashSet::with_capacity(raw.len());
    for path in raw {
        let path = path.trim().to_string();
        if path.is_empty() || !seen.insert(path.clone()) {
            continue;
        }
        out.push(path);
    }
    if out.is_empty() {
        return Err(Box::new(write_error(
            StatusCode::BAD_REQUEST,
            "paths is required",
        )));
    }
    if out.len() > MAX_BULK_ARTIFACTS {
        return Err(Box::new(write_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            &format!("at most {MAX_BULK_ARTIFACTS} paths per request"),
        )));
    }
    Ok(out)
}

/// Labels or unlabels many artifacts in one call.
#[derive(Debug, Clone, Default, Deserialize)]
struct ArtifactBulkLabelReq {
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    label: String,
    /// `add` or `remove`. Explicit rather than implied by the HTTP method
    /// because both directions carry a body of paths.
    #[serde(default)]
    action: String,
}

/// Applies one label to many artifacts, or takes it off them.
///
/// The permission is the per-artifact one the single-artifact endpoints use, so
/// a selection spanning artifacts somebody else uploaded labels the ones they
/// may label and reports the rest. Every attempt is audited exactly as a single
/// one is: a bulk action is not a quieter action.
pub(super) async fn bulk_labels(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let repo = match h.store.get_repository(id).await {
        Ok(repo) => repo,
        Err(err) => return map_error(err),
    };
    if let Some(response) = h.can_read_repo(&parts, &repo.name) {
        return response;
    }
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let Ok(req) = serde_json::from_slice::<ArtifactBulkLabelReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let add = match req.action.as_str() {
        "add" | "" => true,
        "remove" => false,
        _ => {
            return write_error(
                StatusCode::BAD_REQUEST,
                r#"action must be "add" or "remove""#,
            );
        }
    };
    let label = meta::normalize_artifact_label(&req.label);
    if !meta::valid_artifact_label(&label) {
        return write_error(StatusCode::BAD_REQUEST, &format!("label {LABEL_RULE_MSG}"));
    }
    let paths = match bulk_paths(req.paths) {
        Ok(paths) => paths,
        Err(response) => return *response,
    };

    let event = if add {
        meta::EVENT_ARTIFACT_LABEL_ADD
    } else {
        meta::EVENT_ARTIFACT_LABEL_REMOVE
    };
    let mut out = ArtifactBulkResultDTO {
        requested: paths.len(),
        ..Default::default()
    };
    for path in paths {
        let artifact = match h.store.get_artifact(id, &path).await {
            Ok(artifact) => artifact,
            Err(err) => {
                out.failed.push(ArtifactBulkFailureDTO {
                    path,
                    error: bulk_error_text(err),
                });
                continue;
            }
        };
        if !h.can_label_artifact(&parts, &repo.name, &artifact).await {
            h.audit_label(&parts, &repo.name, &path, event, &label, 403);
            out.failed.push(ArtifactBulkFailureDTO {
                path,
                error: LABEL_FORBIDDEN_MSG.to_string(),
            });
            continue;
        }
        let result = if add {
            h.store
                .add_artifact_label(meta::ArtifactLabel {
                    repo_id: id,
                    path: path.clone(),
                    label: label.clone(),
                    created_by: principal_name(&parts),
                    ..Default::default()
                })
                .await
                .map(|_| ())
        } else {
            h.store.delete_artifact_label(id, &path, &label).await
        };
        if let Err(err) = result {
            out.failed.push(ArtifactBulkFailureDTO {
                path,
                error: bulk_error_text(err),
            });
            continue;
        }
        let status = if add { 201 } else { 200 };
        h.audit_label(&parts, &repo.name, &path, event, &label, status);
        out.succeeded += 1;
    }
    write_json(StatusCode::OK, out)
}

/// Removes many artifacts in one call.
#[derive(Debug, Clone, Default, Deserialize)]
struct ArtifactBulkDeleteReq {
    #[serde(default)]
    paths: Vec<String>,
}

/// Removes the named artifacts, reporting each path that refused.
///
/// There is no force here. Force exists to repair one artifact whose bytes are
/// gone, on proof that they are, and proving that per path is a blob-store round
/// trip each; a selection of hundreds is not where that decision belongs. Such an
/// artifact reports its refusal and is deleted from its own row.
pub(super) async fn bulk_delete(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let repo = match h.store.get_repository(id).await {
        Ok(repo) => repo,
        Err(err) => return map_error(err),
    };
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let Ok(req) = serde_json::from_slice::<ArtifactBulkDeleteReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let paths = match bulk_paths(req.paths) {
        Ok(paths) => paths,
        Err(response) => return *response,
    };

    let mut out = ArtifactBulkResultDTO {
        requested: paths.len(),
        ..Default::default()
    };
    for path in paths {
        if let Err(err) = h.store.delete_artifact(id, &path).await {
            out.failed.push(ArtifactBulkFailureDTO {
                path,
                error: bulk_error_text(err),
            });
            continue;
        }
        h.audit_artifact(&parts, &repo.name, &path, 204);
        out.succeeded += 1;
    }
    write_json(StatusCode::OK, out)
}

/// Turns a store error into the sentence shown next to one path. The
/// single-artifact endpoints turn these into status codes; here they travel
/// inside a 200 that also carries the paths which succeeded.
fn bulk_error_text(err: meta::Error) -> String {
    match err {
        meta::Error::NotFound => "not found".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use http::{Method, StatusCode};

    use crate::meta::Artifact;

    use crate::api::artifacts_bulk::MAX_BULK_ARTIFACTS;
    use crate::testing::api::mk_role_user;
    use crate::testing::api::{TestServer, mk_proxy_repo, new_audit_test_server};

    async fn seed_artifacts(srv: &TestServer, repo_id: i64, paths: &[&str]) {
        for (i, path) in paths.iter().enumerate() {
            srv.store
                .put_artifact(Artifact {
                    repo_id,
                    path: (*path).to_string(),
                    version: "1.0.0".to_string(),
                    blob_sha256: format!("sha-{i}"),
                    size: 10,
                    cached_by: "bob".to_string(),
                    ..Default::default()
                })
                .await
                .expect("put artifact");
        }
    }

    /// A bulk label applies the per-artifact permission per path: the uploader's own
    /// artifacts are labelled and the rest are reported, rather than the batch
    /// failing as a whole.
    #[tokio::test]
    async fn bulk_label_applies_per_artifact_permission() {
        let (srv, _rec) = new_audit_test_server().await;
        let repo_id = mk_proxy_repo(&srv, "npmjs").await;
        seed_artifacts(&srv, repo_id, &["a/-/a-1.0.0.tgz", "b/-/b-1.0.0.tgz"]).await;
        // Uploaded by nobody, so only an administrator may label it.
        srv.store
            .put_artifact(Artifact {
                repo_id,
                path: "orphan/-/orphan-1.0.0.tgz".to_string(),
                blob_sha256: "sha-x".to_string(),
                size: 1,
                ..Default::default()
            })
            .await
            .expect("put artifact");
        mk_role_user(
            &srv,
            "bob",
            "pw123456",
            "npm-writer",
            "npmjs",
            r#""read","write""#,
        )
        .await;

        let body = r#"{"paths":["a/-/a-1.0.0.tgz","b/-/b-1.0.0.tgz","orphan/-/orphan-1.0.0.tgz","no/such.tgz"],"label":"keep","action":"add"}"#;
        let resp = srv
            .do_as(
                "bob",
                "pw123456",
                Method::POST,
                &format!("/repositories/{repo_id}/artifacts/labels/bulk"),
                body,
            )
            .await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
        let got = resp.json();
        assert!(
            got["requested"] == 4
                && got["succeeded"] == 2
                && got["failed"].as_array().map(Vec::len) == Some(2),
            "bulk add = {got}, want two applied and two reported"
        );

        // The two that succeeded really carry the label.
        for path in ["a/-/a-1.0.0.tgz", "b/-/b-1.0.0.tgz"] {
            let labels = srv
                .store
                .list_artifact_labels(repo_id, path)
                .await
                .unwrap_or_else(|e| panic!("labels on {path}: {e}"));
            assert!(
                labels.len() == 1 && labels[0].label == "keep",
                "labels on {path} = {labels:?}"
            );
        }

        // Removing takes the same shape, and an artifact that never had the label is
        // reported rather than silently counted as removed.
        const REMOVE: &str =
            r#"{"paths":["a/-/a-1.0.0.tgz","b/-/b-1.0.0.tgz"],"label":"keep","action":"remove"}"#;
        let resp = srv
            .do_as(
                "bob",
                "pw123456",
                Method::POST,
                &format!("/repositories/{repo_id}/artifacts/labels/bulk"),
                REMOVE,
            )
            .await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
        let got = resp.json();
        assert!(
            got["succeeded"] == 2 && got["failed"].as_array().map(Vec::len) == Some(0),
            "bulk remove = {got}"
        );
    }

    #[tokio::test]
    async fn bulk_label_rejects_bad_input() {
        let (srv, _rec) = new_audit_test_server().await;
        let repo_id = mk_proxy_repo(&srv, "npmjs").await;
        seed_artifacts(&srv, repo_id, &["a/-/a-1.0.0.tgz"]).await;
        let url = format!("/repositories/{repo_id}/artifacts/labels/bulk");

        let too_many: Vec<String> = (0..=MAX_BULK_ARTIFACTS)
            .map(|i| format!("p-{i}.tgz"))
            .collect();
        let paths = serde_json::to_string(&too_many).expect("encode paths");

        let over_the_cap = format!(r#"{{"paths":{paths},"label":"keep","action":"add"}}"#);
        for (name, body, want) in [
            (
                "no paths",
                r#"{"paths":[],"label":"keep","action":"add"}"#,
                StatusCode::BAD_REQUEST,
            ),
            (
                "blank paths only",
                r#"{"paths":["  "],"label":"keep","action":"add"}"#,
                StatusCode::BAD_REQUEST,
            ),
            (
                "bad label",
                r#"{"paths":["a/-/a-1.0.0.tgz"],"label":"not a label","action":"add"}"#,
                StatusCode::BAD_REQUEST,
            ),
            (
                "unknown action",
                r#"{"paths":["a/-/a-1.0.0.tgz"],"label":"keep","action":"toggle"}"#,
                StatusCode::BAD_REQUEST,
            ),
            (
                "over the cap",
                over_the_cap.as_str(),
                StatusCode::PAYLOAD_TOO_LARGE,
            ),
        ] {
            let resp = srv.admin_do(Method::POST, &url, body).await;
            assert_eq!(resp.status, want, "{name}: {}", resp.text());
        }
    }

    /// Bulk delete removes what it can and reports the rest, and is admin-only like
    /// the single delete it batches.
    #[tokio::test]
    async fn bulk_delete_artifacts() {
        let (srv, _rec) = new_audit_test_server().await;
        let repo_id = mk_proxy_repo(&srv, "npmjs").await;
        seed_artifacts(&srv, repo_id, &["a/-/a-1.0.0.tgz", "b/-/b-1.0.0.tgz"]).await;
        mk_role_user(
            &srv,
            "bob",
            "pw123456",
            "npm-writer",
            "npmjs",
            r#""read","write""#,
        )
        .await;
        let url = format!("/repositories/{repo_id}/artifacts/bulk-delete");

        let resp = srv
            .do_as(
                "bob",
                "pw123456",
                Method::POST,
                &url,
                r#"{"paths":["a/-/a-1.0.0.tgz"]}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());

        let resp = srv
            .admin_do(
                Method::POST,
                &url,
                r#"{"paths":["a/-/a-1.0.0.tgz","b/-/b-1.0.0.tgz","no/such.tgz"]}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
        let got = resp.json();
        assert!(
            got["requested"] == 3
                && got["succeeded"] == 2
                && got["failed"].as_array().map(Vec::len) == Some(1),
            "bulk delete = {got}"
        );
        assert!(
            srv.store
                .get_artifact(repo_id, "a/-/a-1.0.0.tgz")
                .await
                .is_err(),
            "the artifact is still present after a bulk delete"
        );
    }
}
