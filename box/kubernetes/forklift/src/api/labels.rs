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
use crate::meta::{self, Artifact};

use super::repositories::path_id;
use super::{Handler, map_error, principal_name, write_error, write_json};

/// One label on one artifact, with who put it there and when, so the console can
/// show provenance next to the tag itself.
#[derive(Debug, Clone, Serialize)]
pub(super) struct ArtifactLabelDTO {
    pub(super) label: String,
    pub(super) created_by: String,
    pub(super) created_at: DateTime<Utc>,
}

/// An artifact's labels plus whether this caller may change them. The permission
/// is per artifact, not per repository — an uploader may label what they
/// published and nothing else — so the server answers it rather than leaving the
/// console to guess.
#[derive(Debug, Clone, Serialize)]
struct ArtifactLabelListDTO {
    path: String,
    labels: Vec<ArtifactLabelDTO>,
    can_label: bool,
}

/// The add-label body. The path travels in the body rather than the URL because
/// an artifact path contains slashes (and, for scoped npm packages, characters a
/// path segment would have to escape).
#[derive(Debug, Clone, Default, Deserialize)]
struct ArtifactLabelReq {
    #[serde(default)]
    path: String,
    #[serde(default)]
    label: String,
}

pub(super) fn label_dtos(labels: Vec<meta::ArtifactLabel>) -> Vec<ArtifactLabelDTO> {
    labels
        .into_iter()
        .map(|l| ArtifactLabelDTO {
            label: l.label,
            created_by: l.created_by,
            created_at: l.created_at,
        })
        .collect()
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct LabelQuery {
    #[serde(default)]
    pub(super) path: String,
    #[serde(default)]
    pub(super) label: String,
}

/// Returns one artifact's labels. Read access to the repository is enough to see
/// them: a label is metadata about an artifact the caller can already read.
pub(super) async fn list(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<LabelQuery>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
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
    if q.path.is_empty() {
        return write_error(StatusCode::BAD_REQUEST, "path query parameter is required");
    }
    let artifact = match h.store.get_artifact(id, &q.path).await {
        Ok(artifact) => artifact,
        Err(err) => return map_error(err),
    };
    let labels = match h.store.list_artifact_labels(id, &q.path).await {
        Ok(labels) => labels,
        Err(err) => return map_error(err),
    };
    let can_label = h.can_label_artifact(&parts, &repo.name, &artifact).await;
    write_json(
        StatusCode::OK,
        ArtifactLabelListDTO {
            path: q.path,
            labels: label_dtos(labels),
            can_label,
        },
    )
}

/// Attaches a label to one artifact. Permitted for an administrator on the
/// repository and for the principal who put the artifact there; every attempt,
/// allowed or refused, lands in the repository's audit log.
pub(super) async fn add(
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
    let Ok(req) = serde_json::from_slice::<ArtifactLabelReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    if req.path.is_empty() {
        return write_error(StatusCode::BAD_REQUEST, "path is required");
    }
    let label = meta::normalize_artifact_label(&req.label);
    if !meta::valid_artifact_label(&label) {
        return write_error(StatusCode::BAD_REQUEST, &format!("label {LABEL_RULE_MSG}"));
    }
    let artifact = match h.store.get_artifact(id, &req.path).await {
        Ok(artifact) => artifact,
        Err(err) => return map_error(err),
    };
    if !h.can_label_artifact(&parts, &repo.name, &artifact).await {
        h.audit_label(
            &parts,
            &repo.name,
            &req.path,
            meta::EVENT_ARTIFACT_LABEL_ADD,
            &label,
            403,
        );
        return write_error(StatusCode::FORBIDDEN, LABEL_FORBIDDEN_MSG);
    }
    let stored = match h
        .store
        .add_artifact_label(meta::ArtifactLabel {
            repo_id: id,
            path: req.path.clone(),
            label: label.clone(),
            created_by: principal_name(&parts),
            ..Default::default()
        })
        .await
    {
        Ok(stored) => stored,
        Err(err) => return map_label_error(err),
    };
    h.audit_label(
        &parts,
        &repo.name,
        &stored.path,
        meta::EVENT_ARTIFACT_LABEL_ADD,
        &stored.label,
        201,
    );
    write_label_list(&h, &parts, id, &repo.name, &artifact, StatusCode::CREATED).await
}

/// Removes one label from one artifact, under the same permission and audit
/// rules as adding it.
pub(super) async fn delete(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<LabelQuery>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
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
    let label = meta::normalize_artifact_label(&q.label);
    if q.path.is_empty() || label.is_empty() {
        return write_error(
            StatusCode::BAD_REQUEST,
            "path and label query parameters are required",
        );
    }
    let artifact = match h.store.get_artifact(id, &q.path).await {
        Ok(artifact) => artifact,
        Err(err) => return map_error(err),
    };
    if !h.can_label_artifact(&parts, &repo.name, &artifact).await {
        h.audit_label(
            &parts,
            &repo.name,
            &q.path,
            meta::EVENT_ARTIFACT_LABEL_REMOVE,
            &label,
            403,
        );
        return write_error(StatusCode::FORBIDDEN, LABEL_FORBIDDEN_MSG);
    }
    if let Err(err) = h.store.delete_artifact_label(id, &q.path, &label).await {
        return map_label_error(err);
    }
    h.audit_label(
        &parts,
        &repo.name,
        &q.path,
        meta::EVENT_ARTIFACT_LABEL_REMOVE,
        &label,
        200,
    );
    write_label_list(&h, &parts, id, &repo.name, &artifact, StatusCode::OK).await
}

/// Answers a mutation with the artifact's labels as they now stand, so the
/// console renders from the server's state instead of predicting the outcome of
/// its own request.
async fn write_label_list(
    h: &Arc<Handler>,
    parts: &Parts,
    repo_id: i64,
    repo_name: &str,
    artifact: &Artifact,
    status: StatusCode,
) -> Response {
    let labels = match h.store.list_artifact_labels(repo_id, &artifact.path).await {
        Ok(labels) => labels,
        Err(err) => return map_error(err),
    };
    let can_label = h.can_label_artifact(parts, repo_name, artifact).await;
    write_json(
        status,
        ArtifactLabelListDTO {
            path: artifact.path.clone(),
            labels: label_dtos(labels),
            can_label,
        },
    )
}

pub(super) const LABEL_RULE_MSG: &str =
    "must be a key or key:value of letters, digits, '-' and '_' (max 64 chars)";

pub(super) const LABEL_FORBIDDEN_MSG: &str =
    "labelling this artifact requires admin on the repository or having uploaded it";

impl Handler {
    /// Reports whether the caller may change one artifact's labels: an
    /// administrator on the repository, or the principal who put the artifact
    /// there (the uploader recorded on the artifact, or the publisher of the
    /// publication it belongs to, which is the same person for a multi-file
    /// upload whose individual paths were derived by the server).
    ///
    /// Deliberately not the write action: write lets a principal publish new
    /// versions, while a label is an assertion about an artifact someone else may
    /// have published, so it stays with the owner and the administrator.
    pub(super) async fn can_label_artifact(
        &self,
        parts: &Parts,
        repo_name: &str,
        artifact: &Artifact,
    ) -> bool {
        if self.authz.is_none() {
            return true;
        }
        let Some(p) = auth::from_request_parts(parts) else {
            return false;
        };
        if p.can(repo_name, auth::ACTION_ADMIN) {
            return true;
        }
        // An anonymous upload records no principal, so it has no owner to match:
        // only an administrator can label it.
        if p.username.is_empty() {
            return false;
        }
        if artifact.cached_by == p.username {
            return true;
        }
        if !artifact.publication_id.is_empty()
            && let Ok(publication) = self
                .store
                .get_artifact_publication(&artifact.publication_id)
                .await
        {
            return publication.created_by == p.username;
        }
        false
    }

    /// Records one label action, allowed or refused, with the label itself in the
    /// detail so the trail says what was applied and not merely that something
    /// was. An impersonated session is attributed to the user it acts as, with
    /// the administrator behind it recorded alongside.
    pub(super) fn audit_label(
        &self,
        parts: &Parts,
        repo_name: &str,
        path: &str,
        event: &str,
        label: &str,
        status: i64,
    ) {
        let Some(rec) = &self.rec else {
            return;
        };
        let mut detail = std::collections::BTreeMap::new();
        detail.insert("label", label.to_string());
        if let Some(p) = auth::from_request_parts(parts)
            && !p.impersonator.is_empty()
        {
            detail.insert("impersonated_by", p.impersonator.clone());
        }
        if status == 403 {
            detail.insert(
                "denied",
                "not an administrator on this repository and not the uploader".to_string(),
            );
        }
        rec.record(audit::Event {
            repo: repo_name.to_string(),
            action: event.to_string(),
            path: path.to_string(),
            username: principal_name(parts),
            method: parts.method.to_string(),
            status,
            client_ip: audit::client_ip_parts(parts),
            user_agent: super::header_str(parts, http::header::USER_AGENT),
            detail_json: serde_json::to_string(&detail).unwrap_or_default(),
            ..Default::default()
        });
    }
}

/// Adds the label-specific outcomes to the shared store-error mapping: a
/// duplicate label is a conflict, and a full artifact is refused rather than
/// silently dropping the request.
pub(super) fn map_label_error(err: meta::Error) -> Response {
    match err {
        meta::Error::Conflict => {
            write_error(StatusCode::CONFLICT, "artifact already carries this label")
        }
        meta::Error::LabelLimit => write_error(
            StatusCode::CONFLICT,
            "artifact already carries the maximum number of labels",
        ),
        other => map_error(other),
    }
}

/// Resolves the per-artifact label permission for a whole listing in one pass:
/// the repository-wide part (administrator, or authorization off in tests) is
/// decided once, and each row is then matched against the caller's own name. It
/// exists so a page of artifacts costs no store lookups — the publication
/// creators come from the publications the listing already loaded.
#[derive(Debug, Clone, Default)]
pub(super) struct LabelPermission {
    all: bool,
    username: String,
    publications: HashMap<String, String>,
}

impl Handler {
    /// Builds the resolver for one repository listing. `publications` maps
    /// publication id to the principal who published it.
    pub(super) fn new_label_permission(
        &self,
        parts: &Parts,
        repo_name: &str,
        publications: HashMap<String, String>,
    ) -> LabelPermission {
        if self.authz.is_none() {
            return LabelPermission {
                all: true,
                ..Default::default()
            };
        }
        let Some(p) = auth::from_request_parts(parts) else {
            return LabelPermission::default();
        };
        if p.can(repo_name, auth::ACTION_ADMIN) {
            return LabelPermission {
                all: true,
                ..Default::default()
            };
        }
        LabelPermission {
            all: false,
            username: p.username.clone(),
            publications,
        }
    }
}

impl LabelPermission {
    /// Reports whether the caller may change this artifact's labels.
    pub(super) fn allows(&self, artifact: &Artifact) -> bool {
        if self.allows_owner(&artifact.cached_by) {
            return true;
        }
        if !artifact.publication_id.is_empty() {
            return !self.username.is_empty()
                && self
                    .publications
                    .get(&artifact.publication_id)
                    .is_some_and(|creator| *creator == self.username);
        }
        false
    }

    /// [`LabelPermission::allows`] for a view that carries the uploader's name
    /// but not the artifact row, which is how the OCI image listing presents a
    /// manifest.
    pub(super) fn allows_owner(&self, uploader: &str) -> bool {
        self.all || (!self.username.is_empty() && uploader == self.username)
    }
}
