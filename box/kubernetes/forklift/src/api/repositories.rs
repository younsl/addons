use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use http::StatusCode;
use http::request::Parts;
use serde::{Deserialize, Serialize};

use crate::audit;
use crate::auth;
use crate::meta::{self, Artifact, ForceDeleteResult, Repository, VulnAdvisory};
use crate::repo;
use crate::repoconfig;

use super::labels::{ArtifactLabelDTO, label_dtos};
use super::storage::{StatusCountDTO, dangling_refs, status_counts};
use super::{
    Handler, NAME_RULE_MSG, map_error, principal_name, valid_name, write_error, write_json,
};

pub(super) fn int_param(value: &str, default: i64) -> i64 {
    if value.is_empty() {
        return default;
    }
    value.parse::<i64>().unwrap_or(default)
}

/// Parses the `{id}` path parameter, answering 400 when it is not an integer.
/// The error is boxed because a `Response` is far larger than the `i64` it
/// competes with in the `Result`.
pub(super) fn path_id(id: &str) -> Result<i64, Box<Response>> {
    id.parse::<i64>()
        .map_err(|_| Box::new(write_error(StatusCode::BAD_REQUEST, "invalid id")))
}

/// The JSON shape for a repository.
#[derive(Debug, Clone, Default, Serialize)]
struct RepositoryDTO {
    id: i64,
    name: String,
    format: String,
    r#type: String,
    upstream_url: String,
    config: repoconfig::Config,
    /// Optional operator-facing free text.
    description: String,
    disabled: bool,
    /// Marks a predefined repository that cannot be deleted (see
    /// [`repo::is_default_repo`]); clients use it to hide the delete action.
    seeded: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    capabilities: RepositoryCapabilitiesDTO,
    publish_methods: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct RepositoryCapabilitiesDTO {
    read: bool,
    write: bool,
    delete: bool,
    upload: bool,
}

fn to_dto(r: &Repository) -> Result<RepositoryDTO, repoconfig::Error> {
    let mut cfg = repoconfig::parse(&r.config_json)?;
    // Upstream credential secrets never leave the server in the clear; a masked
    // value round-tripped on update keeps the stored secret.
    cfg.upstream_auth = cfg.upstream_auth.masked();
    Ok(RepositoryDTO {
        id: r.id,
        name: r.name.clone(),
        format: r.format.clone(),
        r#type: r.r#type.clone(),
        upstream_url: r.upstream_url.clone(),
        config: cfg,
        description: r.description.clone(),
        disabled: r.disabled,
        seeded: repo::is_default_repo(&r.name),
        created_at: r.created_at,
        updated_at: r.updated_at,
        capabilities: RepositoryCapabilitiesDTO::default(),
        publish_methods: Vec::new(),
    })
}

impl Handler {
    fn repository_dto(
        &self,
        parts: &Parts,
        repository: &Repository,
    ) -> Result<RepositoryDTO, repoconfig::Error> {
        let mut dto = to_dto(repository)?;
        let p = auth::from_request_parts(parts);
        let can = |action: &str| {
            self.authz.is_none() || p.as_ref().is_some_and(|p| p.can(&repository.name, action))
        };
        dto.capabilities = RepositoryCapabilitiesDTO {
            read: can(auth::ACTION_READ),
            write: can(auth::ACTION_WRITE),
            delete: can(auth::ACTION_DELETE),
            upload: false,
        };
        dto.capabilities.upload = self.upload_enabled()
            && self
                .uploader()
                .is_some_and(|u| u.supports(&repository.format))
            && !repository.disabled
            && repository.r#type == meta::TYPE_HOSTED
            && dto.capabilities.write;
        dto.publish_methods = match repository.format.as_str() {
            meta::FORMAT_MAVEN => vec!["mvn".to_string()],
            meta::FORMAT_NPM => vec!["npm".to_string()],
            meta::FORMAT_PYPI => vec!["twine".to_string()],
            meta::FORMAT_CARGO => vec!["cargo".to_string()],
            meta::FORMAT_OCI => vec!["docker".to_string(), "helm".to_string(), "oras".to_string()],
            _ => Vec::new(),
        };
        Ok(dto)
    }
}

/// A repository with artifact aggregates. Both the list and the detail endpoint
/// return this shape; the detail endpoint fills `can_write` and
/// `pending_approval_count` and leaves the aggregates at zero, which still
/// serialise because none of these fields is omitted.
#[derive(Debug, Clone, Default, Serialize)]
struct RepositoryListItemDTO {
    #[serde(flatten)]
    repository: RepositoryDTO,
    artifact_count: i64,
    total_size: i64,
    /// The number of packages awaiting approval in this repository (0 when none
    /// or when approval is not configured).
    pending_approval_count: i64,
    can_write: bool,
    /// `scanned_count` is the number of stored artifacts that have a
    /// vulnerability scan; `clean_count` is how many of those are clean (no
    /// advisories). The UI renders them as a percentage. Both 0 when nothing is
    /// scanned or the repository format is not scannable.
    scanned_count: i64,
    clean_count: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CreateRepositoryReq {
    #[serde(default)]
    name: String,
    #[serde(default)]
    format: String,
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    upstream_url: String,
    #[serde(default)]
    config: Option<repoconfig::Config>,
    /// Optional free text (capped like other short fields).
    #[serde(default)]
    description: String,
}

/// Caps the free-text repository description (bytes).
const MAX_DESCRIPTION_LEN: usize = 1000;

fn valid_format(format: &str) -> bool {
    [
        meta::FORMAT_MAVEN,
        meta::FORMAT_NPM,
        meta::FORMAT_CARGO,
        meta::FORMAT_GO,
        meta::FORMAT_PYPI,
        meta::FORMAT_RAW,
        meta::FORMAT_OCI,
    ]
    .contains(&format)
}

pub(super) async fn list(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let repos = match h.store.list_repositories().await {
        Ok(repos) => repos,
        Err(err) => return map_error(err),
    };
    let stats = match h.store.all_repo_stats().await {
        Ok(stats) => stats,
        Err(err) => return map_error(err),
    };
    let pending = match h.store.pending_approval_count_by_repo().await {
        Ok(pending) => pending,
        Err(err) => return map_error(err),
    };
    // The per-repo clean-scan ratio is best-effort: it needs the repo manager
    // (for per-format coordinate parsing) and never blocks the listing on
    // failure.
    let mut ratios: HashMap<i64, repo::ScanRatio> = HashMap::new();
    if let Some(manager) = h.repo_manager()
        && let Ok(rs) = manager.clean_scan_ratios().await
    {
        ratios = rs;
    }
    // Non-admins see only repositories they can read. Admins (`can` returns true
    // for every repo) and the authz-disabled case (tests) see all.
    let p = auth::from_request_parts(&parts);
    let mut out = Vec::with_capacity(repos.len());
    for repository in repos {
        if h.authz.is_some()
            && !p
                .as_ref()
                .is_some_and(|p| p.can(&repository.name, auth::ACTION_READ))
        {
            continue;
        }
        let dto = match h.repository_dto(&parts, &repository) {
            Ok(dto) => dto,
            Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
        };
        let st = stats.get(&repository.id).cloned().unwrap_or_default();
        let ratio = ratios.get(&repository.id).cloned().unwrap_or_default();
        out.push(RepositoryListItemDTO {
            repository: dto,
            artifact_count: st.artifact_count,
            total_size: st.total_size,
            pending_approval_count: pending.get(&repository.name).copied().unwrap_or_default(),
            scanned_count: ratio.scanned,
            clean_count: ratio.clean,
            can_write: false,
        });
    }
    write_json(StatusCode::OK, out)
}

/// The slim shape returned to any authenticated user for token-scope
/// autocomplete: names only, no config, upstream URLs or stats.
#[derive(Debug, Clone, Serialize)]
struct RepositoryNameDTO {
    name: String,
    format: String,
    r#type: String,
}

/// Returns repository names so the token-creation UI can autocomplete scope
/// patterns. Unlike [`list`] it is available to every authenticated user
/// (scoping a token requires knowing the repository names), and deliberately
/// exposes no configuration or upstream details.
pub(super) async fn list_names(State(h): State<Arc<Handler>>) -> Response {
    match h.store.list_repositories().await {
        Ok(repos) => write_json(
            StatusCode::OK,
            repos
                .into_iter()
                .map(|repository| RepositoryNameDTO {
                    name: repository.name,
                    format: repository.format,
                    r#type: repository.r#type,
                })
                .collect::<Vec<_>>(),
        ),
        Err(err) => map_error(err),
    }
}

pub(super) async fn create(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let Ok(mut req) = serde_json::from_slice::<CreateRepositoryReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    req.name = req.name.trim().to_string();
    if !valid_name(&req.name) {
        return write_error(
            StatusCode::BAD_REQUEST,
            &format!("invalid repository name: {NAME_RULE_MSG}"),
        );
    }
    if !valid_format(&req.format) {
        return write_error(StatusCode::BAD_REQUEST, "invalid format");
    }
    if req.r#type != meta::TYPE_HOSTED
        && req.r#type != meta::TYPE_PROXY
        && req.r#type != meta::TYPE_GROUP
    {
        return write_error(StatusCode::BAD_REQUEST, "invalid type (hosted|proxy|group)");
    }
    if req.r#type == meta::TYPE_PROXY && req.upstream_url.trim().is_empty() {
        return write_error(
            StatusCode::BAD_REQUEST,
            "proxy repository requires upstream_url",
        );
    }

    let cfg = req.config.clone().unwrap_or_else(repoconfig::default);
    if let Err(err) = cfg.validate() {
        return write_error(StatusCode::BAD_REQUEST, &err.to_string());
    }
    if req.description.len() > MAX_DESCRIPTION_LEN {
        return write_error(StatusCode::BAD_REQUEST, "description is too long");
    }
    if req.r#type == meta::TYPE_GROUP {
        req.upstream_url = String::new();
        if let Err(err) =
            repo::validate_group_members(&h.store, &req.format, &cfg.group.members).await
        {
            return write_error(StatusCode::BAD_REQUEST, &err.to_string());
        }
    } else if !cfg.group.members.is_empty() {
        return write_error(
            StatusCode::BAD_REQUEST,
            "group members are only valid for group repositories",
        );
    }
    if cfg.approval.enabled && req.r#type == meta::TYPE_GROUP {
        return write_error(
            StatusCode::BAD_REQUEST,
            "approval is only valid for proxy or hosted repositories",
        );
    }
    if cfg.upload.pypi_allow_legacy_zip
        && (req.r#type != meta::TYPE_HOSTED || req.format != meta::FORMAT_PYPI)
    {
        return write_error(
            StatusCode::BAD_REQUEST,
            "pypi legacy zip compatibility is only valid for hosted PyPI repositories",
        );
    }
    if cfg.upstream_auth.enabled() && req.r#type != meta::TYPE_PROXY {
        return write_error(
            StatusCode::BAD_REQUEST,
            "upstream_auth is only valid for proxy repositories",
        );
    }
    let config_json = match cfg.json() {
        Ok(config_json) => config_json,
        Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };

    let repository = match h
        .store
        .create_repository(Repository {
            name: req.name.clone(),
            format: req.format.clone(),
            r#type: req.r#type.clone(),
            upstream_url: req.upstream_url.trim().to_string(),
            config_json,
            description: req.description.trim().to_string(),
            ..Default::default()
        })
        .await
    {
        Ok(repository) => repository,
        Err(err) => {
            if err.to_string().contains("UNIQUE") || matches!(err, meta::Error::Conflict) {
                return write_error(StatusCode::CONFLICT, "repository name already exists");
            }
            return map_error(err);
        }
    };
    let dto = match h.repository_dto(&parts, &repository) {
        Ok(dto) => dto,
        Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    h.audit(&parts, &repository.name, meta::EVENT_REPO_CREATE, 201);
    write_json(StatusCode::CREATED, dto)
}

/// One row of the OCI image view with its labels attached. An OCI manifest is an
/// ordinary artifact row, so it carries labels like any other format; the path is
/// included because that is the identity the label endpoints take, and the
/// console would otherwise have to rebuild it from name and digest.
#[derive(Debug, Clone, Serialize)]
struct OciTagDTO {
    #[serde(flatten)]
    info: repo::OCITagInfo,
    path: String,
    labels: Vec<ArtifactLabelDTO>,
    can_label: bool,
}

/// The OCI drill-down with the same label fields as the listing.
///
/// `OCIArtifactDetail` holds raw JSON documents and is not `Clone`, so the DTO
/// takes it by value rather than deriving `Clone` alongside it.
#[derive(Debug, Serialize)]
struct OciDetailDTO {
    #[serde(flatten)]
    detail: repo::OCIArtifactDetail,
    path: String,
    labels: Vec<ArtifactLabelDTO>,
    can_label: bool,
}

/// Serves the Harbor-style image view for an OCI repository: every tag with its
/// manifest digest, kind, platforms, content size and push time. Empty (not an
/// error) for other formats, which simply have no tag rows.
pub(super) async fn list_oci_tags(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let repository = match h.store.get_repository(id).await {
        Ok(repository) => repository,
        Err(err) => return map_error(err),
    };
    if let Some(response) = h.can_read_repo(&parts, &repository.name) {
        return response;
    }
    let Some(manager) = h.repo_manager() else {
        return write_json(
            StatusCode::OK,
            serde_json::json!({"tags": Vec::<OciTagDTO>::new()}),
        );
    };
    let tags = match manager.list_oci_tags(repository.id).await {
        Ok(tags) => tags,
        Err(err) => return map_error(err),
    };
    let labels_by_path = match h.store.artifact_labels_by_path(repository.id).await {
        Ok(labels) => labels,
        Err(err) => return map_error(err),
    };
    let label_perm = h.new_label_permission(&parts, &repository.name, HashMap::new());
    let out: Vec<OciTagDTO> = tags
        .into_iter()
        .map(|tag| {
            let path = repo::oci_manifest_path(&tag.name, &tag.digest);
            let can_label = label_perm.allows_owner(&tag.pushed_by);
            OciTagDTO {
                labels: label_dtos(labels_by_path.get(&path).cloned().unwrap_or_default()),
                path,
                can_label,
                info: tag,
            }
        })
        .collect();
    write_json(StatusCode::OK, serde_json::json!({"tags": out}))
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct OciDetailQuery {
    #[serde(default)]
    pub(super) name: String,
    #[serde(default)]
    pub(super) r#ref: String,
}

/// Serves the Harbor-style artifact drill-down for one tagged (or
/// digest-addressed) OCI artifact: overview fields, the raw manifest and config
/// documents, and per-kind additions (chart values/README, image config summary,
/// index children). `name` and `ref` arrive as query parameters because an OCI
/// name itself contains slashes.
pub(super) async fn get_oci_detail(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<OciDetailQuery>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let repository = match h.store.get_repository(id).await {
        Ok(repository) => repository,
        Err(err) => return map_error(err),
    };
    if let Some(response) = h.can_read_repo(&parts, &repository.name) {
        return response;
    }
    let manager = h.repo_manager();
    if q.name.is_empty() || q.r#ref.is_empty() || manager.is_none() {
        return write_error(
            StatusCode::BAD_REQUEST,
            "name and ref query parameters are required",
        );
    }
    let manager = manager.expect("checked above");
    let detail = match manager
        .oci_artifact_detail(repository.id, &q.name, &q.r#ref)
        .await
    {
        Ok(detail) => detail,
        Err(err) => return map_error(err),
    };
    let path = repo::oci_manifest_path(&detail.info.name, &detail.info.digest);
    let labels = match h.store.list_artifact_labels(repository.id, &path).await {
        Ok(labels) => labels,
        Err(err) => return map_error(err),
    };
    let can_label = h
        .new_label_permission(&parts, &repository.name, HashMap::new())
        .allows_owner(&detail.info.pushed_by);
    write_json(
        StatusCode::OK,
        OciDetailDTO {
            detail,
            path,
            labels: label_dtos(labels),
            can_label,
        },
    )
}

impl Handler {
    /// Enforces read access to a single repository for non-admins. Admins (`can`
    /// returns true for every repo) and the authz-disabled case (tests) pass.
    ///
    pub(super) fn can_read_repo(&self, parts: &Parts, repo_name: &str) -> Option<Response> {
        // No authorization service (tests) allows everything.
        let allowed = self.authz.is_none()
            || auth::from_request_parts(parts).is_some_and(|p| p.can(repo_name, auth::ACTION_READ));
        if allowed {
            return None;
        }
        Some(write_error(StatusCode::FORBIDDEN, "forbidden"))
    }

    /// Enforces the per-repository write permission.
    fn can_write_repo(&self, parts: &Parts, repo_name: &str) -> Option<Response> {
        let allowed = self.authz.is_none()
            || auth::from_request_parts(parts)
                .is_some_and(|p| p.can(repo_name, auth::ACTION_WRITE));
        if allowed {
            return None;
        }
        Some(write_error(
            StatusCode::FORBIDDEN,
            &format!("write permission required for repository {repo_name}"),
        ))
    }

    /// Enforces the per-repository security permission (admin qualifies via
    /// admin-implies-all). Neither approve nor audit is accepted here: deciding
    /// one package and reading the admin surfaces are separate capabilities from
    /// rewriting the policy those decisions are measured against.
    fn can_security(&self, parts: &Parts, repo_name: &str) -> Option<Response> {
        let allowed = self.authz.is_none()
            || auth::from_request_parts(parts)
                .is_some_and(|p| p.can(repo_name, auth::ACTION_SECURITY));
        if allowed {
            return None;
        }
        Some(write_error(
            StatusCode::FORBIDDEN,
            &format!("security permission required for repository {repo_name}"),
        ))
    }
}

pub(super) async fn get(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let repository = match h.store.get_repository(id).await {
        Ok(repository) => repository,
        Err(err) => return map_error(err),
    };
    if let Some(response) = h.can_read_repo(&parts, &repository.name) {
        return response;
    }
    let dto = match h.repository_dto(&parts, &repository) {
        Ok(dto) => dto,
        Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    let mut item = RepositoryListItemDTO {
        repository: dto,
        ..Default::default()
    };
    if let Some(p) = auth::from_request_parts(&parts) {
        item.can_write = p.can(&repository.name, auth::ACTION_WRITE);
    }
    // The pending-approval count so the detail page (policy flow) can badge the
    // approval gate and link to the queue. Proxy/hosted only; best-effort.
    if (repository.r#type == meta::TYPE_PROXY || repository.r#type == meta::TYPE_HOSTED)
        && let Ok(n) = h
            .store
            .count_approvals(&repository.name, meta::APPROVAL_PENDING)
            .await
    {
        item.pending_approval_count = n;
    }
    h.audit_view(&parts, &repository.name, "(repository)");
    write_json(StatusCode::OK, item)
}

#[derive(Debug, Clone, Default, Deserialize)]
struct UpdateRepositoryReq {
    #[serde(default)]
    upstream_url: String,
    #[serde(default)]
    config: repoconfig::Config,
    #[serde(default)]
    description: String,
}

pub(super) async fn update(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let Ok(mut req) = serde_json::from_slice::<UpdateRepositoryReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    if let Err(err) = req.config.validate() {
        return write_error(StatusCode::BAD_REQUEST, &err.to_string());
    }
    if req.description.len() > MAX_DESCRIPTION_LEN {
        return write_error(StatusCode::BAD_REQUEST, "description is too long");
    }
    let existing = match h.store.get_repository(id).await {
        Ok(existing) => existing,
        Err(err) => return map_error(err),
    };
    // Clients only ever see masked upstream secrets; restore the stored values
    // for any field that came back as the mask.
    if let Ok(existing_cfg) = repoconfig::parse(&existing.config_json) {
        req.config.upstream_auth = req
            .config
            .upstream_auth
            .unmask_from(&existing_cfg.upstream_auth);
    }
    if existing.r#type == meta::TYPE_GROUP {
        if let Err(err) =
            repo::validate_group_members(&h.store, &existing.format, &req.config.group.members)
                .await
        {
            return write_error(StatusCode::BAD_REQUEST, &err.to_string());
        }
    } else if !req.config.group.members.is_empty() {
        return write_error(
            StatusCode::BAD_REQUEST,
            "group members are only valid for group repositories",
        );
    }
    if req.config.approval.enabled && existing.r#type == meta::TYPE_GROUP {
        return write_error(
            StatusCode::BAD_REQUEST,
            "approval is only valid for proxy or hosted repositories",
        );
    }
    if req.config.upload.pypi_allow_legacy_zip
        && (existing.r#type != meta::TYPE_HOSTED || existing.format != meta::FORMAT_PYPI)
    {
        return write_error(
            StatusCode::BAD_REQUEST,
            "pypi legacy zip compatibility is only valid for hosted PyPI repositories",
        );
    }
    if req.config.upstream_auth.enabled() && existing.r#type != meta::TYPE_PROXY {
        return write_error(
            StatusCode::BAD_REQUEST,
            "upstream_auth is only valid for proxy repositories",
        );
    }
    // PUT replaces the whole resource, so an omitted upstream_url would otherwise
    // zero out a proxy's upstream and silently break it. Mirrors the create
    // guard.
    if existing.r#type == meta::TYPE_PROXY && req.upstream_url.trim().is_empty() {
        return write_error(
            StatusCode::BAD_REQUEST,
            "proxy repository requires upstream_url",
        );
    }
    let config_json = match req.config.json() {
        Ok(config_json) => config_json,
        Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    if let Err(err) = h
        .store
        .update_repository_config(id, req.upstream_url.trim(), &config_json)
        .await
    {
        return map_error(err);
    }
    let description = req.description.trim();
    if description != existing.description
        && let Err(err) = h.store.update_repository_description(id, description).await
    {
        return map_error(err);
    }
    let repository = match h.store.get_repository(id).await {
        Ok(repository) => repository,
        Err(err) => return map_error(err),
    };
    let dto = match h.repository_dto(&parts, &repository) {
        Ok(dto) => dto,
        Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    h.audit(&parts, &repository.name, meta::EVENT_REPO_UPDATE, 200);
    write_json(StatusCode::OK, dto)
}

/// The security-policy subset of a repository's config: the fields the Security
/// tab owns. Everything else a repository carries (cache, retention, group
/// members, upload compatibility, upstream URL and above all upstream
/// credentials) is deliberately absent from this shape, so decoding a request
/// into it cannot touch those fields no matter what the client sends. That
/// whitelist, not the separate route, is what keeps the security action from
/// escalating into repository management.
#[derive(Debug, Clone, Default, Deserialize)]
struct SecurityConfig {
    #[serde(default)]
    age_policy: repoconfig::AgePolicyConfig,
    #[serde(default)]
    approval: repoconfig::ApprovalConfig,
    #[serde(default)]
    vuln: repoconfig::VulnPolicyConfig,
    #[serde(default)]
    license: repoconfig::LicensePolicyConfig,
    #[serde(default)]
    ip_acl: repoconfig::IPACLConfig,
    #[serde(default)]
    notify: repoconfig::NotifyConfig,
    /// Exposes the repository to anonymous reads (see `repoconfig::Config`).
    #[serde(default)]
    public: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct UpdateRepositorySecurityReq {
    #[serde(default)]
    config: SecurityConfig,
}

/// Replaces a repository's security policy. It is the non-admin write path onto
/// repository config: administrators plus principals holding the security action
/// on the repository (e.g. a security engineer) may call it, while the upstream
/// URL and credentials stay behind the admin-only `PUT /repositories/{id}`. Like
/// that route this is a replace, not a merge, of the sections it owns; the
/// sections it does not own are carried over from the stored config untouched.
pub(super) async fn update_security(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let Ok(req) = serde_json::from_slice::<UpdateRepositorySecurityReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let existing = match h.store.get_repository(id).await {
        Ok(existing) => existing,
        Err(err) => return map_error(err),
    };
    if let Some(response) = h.can_security(&parts, &existing.name) {
        return response;
    }
    let mut cfg = match repoconfig::parse(&existing.config_json) {
        Ok(cfg) => cfg,
        Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    cfg.age_policy = req.config.age_policy;
    cfg.approval = req.config.approval;
    cfg.vuln = req.config.vuln;
    cfg.license = req.config.license;
    cfg.ip_acl = req.config.ip_acl;
    cfg.notify = req.config.notify;
    cfg.public = req.config.public;
    if let Err(err) = cfg.validate() {
        return write_error(StatusCode::BAD_REQUEST, &err.to_string());
    }
    if cfg.approval.enabled && existing.r#type == meta::TYPE_GROUP {
        return write_error(
            StatusCode::BAD_REQUEST,
            "approval is only valid for proxy or hosted repositories",
        );
    }
    let config_json = match cfg.json() {
        Ok(config_json) => config_json,
        Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    // The upstream URL is passed through unchanged: this route never edits it.
    if let Err(err) = h
        .store
        .update_repository_config(id, &existing.upstream_url, &config_json)
        .await
    {
        return map_error(err);
    }
    let repository = match h.store.get_repository(id).await {
        Ok(repository) => repository,
        Err(err) => return map_error(err),
    };
    let dto = match h.repository_dto(&parts, &repository) {
        Ok(dto) => dto,
        Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    h.audit(&parts, &repository.name, meta::EVENT_REPO_UPDATE, 200);
    write_json(StatusCode::OK, dto)
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SetDisabledReq {
    #[serde(default)]
    disabled: bool,
}

/// Toggles a repository online/offline. A disabled repository keeps its config
/// and artifacts but stops serving the package protocols (503), so it can be
/// re-enabled later.
pub(super) async fn set_disabled(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let Ok(req) = serde_json::from_slice::<SetDisabledReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let repository = match h.store.get_repository(id).await {
        Ok(repository) => repository,
        Err(err) => return map_error(err),
    };
    if let Err(err) = h.store.set_repository_disabled(id, req.disabled).await {
        return map_error(err);
    }
    h.audit(&parts, &repository.name, meta::EVENT_REPO_UPDATE, 200);
    let repository = match h.store.get_repository(id).await {
        Ok(repository) => repository,
        Err(err) => return map_error(err),
    };
    match h.repository_dto(&parts, &repository) {
        Ok(dto) => write_json(StatusCode::OK, dto),
        Err(err) => write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

#[derive(Debug, Clone, Serialize)]
struct RepoPermissionDTO {
    role_id: i64,
    role: String,
    repo_pattern: String,
    actions: Vec<String>,
    user_count: i64,
}

/// Lists the role permissions that grant access to this repository: every
/// permission whose repo pattern matches the repository name, with the granting
/// role, the matched pattern, the actions, and how many users hold that role.
pub(super) async fn permissions(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let repository = match h.store.get_repository(id).await {
        Ok(repository) => repository,
        Err(err) => return map_error(err),
    };
    let roles = match h.store.list_roles().await {
        Ok(roles) => roles,
        Err(err) => return map_error(err),
    };
    let perms = match h.store.list_permissions().await {
        Ok(perms) => perms,
        Err(err) => return map_error(err),
    };
    let roles_by = match h.store.roles_by_user().await {
        Ok(roles_by) => roles_by,
        Err(err) => return map_error(err),
    };
    let role_name: HashMap<i64, String> =
        roles.into_iter().map(|role| (role.id, role.name)).collect();
    let mut user_count: HashMap<i64, i64> = HashMap::new();
    for user_roles in roles_by.values() {
        for role in user_roles {
            *user_count.entry(role.id).or_default() += 1;
        }
    }
    let out: Vec<RepoPermissionDTO> = perms
        .into_iter()
        .filter(|p| auth::match_repo_pattern(&p.repo_pattern, &repository.name))
        .map(|p| RepoPermissionDTO {
            role_id: p.role_id,
            role: role_name.get(&p.role_id).cloned().unwrap_or_default(),
            repo_pattern: p.repo_pattern,
            actions: p.actions.split(',').map(str::to_string).collect(),
            user_count: user_count.get(&p.role_id).copied().unwrap_or_default(),
        })
        .collect();
    write_json(StatusCode::OK, out)
}

#[derive(Debug, Clone, Serialize)]
struct RepoTokenDTO {
    token_id: i64,
    name: String,
    owner: String,
    repo_pattern: String,
    actions: Vec<String>,
    unscoped: bool,
    expires_at: Option<DateTime<Utc>>,
    last_used_at: Option<DateTime<Utc>>,
}

/// Lists personal access tokens that can reach this repository: tokens with a
/// scope whose pattern matches the repo (scoped grant), plus unscoped tokens
/// (which inherit the owner's role access to any repo). The effective access of a
/// scoped token is still bounded by its owner's roles.
pub(super) async fn tokens(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let repository = match h.store.get_repository(id).await {
        Ok(repository) => repository,
        Err(err) => return map_error(err),
    };
    let tokens = match h.store.list_all_tokens().await {
        Ok(tokens) => tokens,
        Err(err) => return map_error(err),
    };
    let users = match h.store.list_users().await {
        Ok(users) => users,
        Err(err) => return map_error(err),
    };
    let owner: HashMap<i64, String> = users.into_iter().map(|u| (u.id, u.username)).collect();
    #[derive(Deserialize)]
    struct TokenScope {
        #[serde(default)]
        repo_pattern: String,
        #[serde(default)]
        actions: Vec<String>,
    }
    let mut out = Vec::new();
    for t in tokens {
        let scopes: Vec<TokenScope> = serde_json::from_str(&t.scopes_json).unwrap_or_default();
        if scopes.is_empty() {
            // Unscoped: inherits the owner's role access to any repository.
            out.push(RepoTokenDTO {
                token_id: t.id,
                name: t.name,
                owner: owner.get(&t.user_id).cloned().unwrap_or_default(),
                repo_pattern: "*".to_string(),
                actions: Vec::new(),
                unscoped: true,
                expires_at: t.expires_at,
                last_used_at: t.last_used_at,
            });
            continue;
        }
        let mut matched = String::new();
        let mut seen = std::collections::HashSet::new();
        let mut actions = Vec::new();
        for sc in scopes {
            if !auth::match_repo_pattern(&sc.repo_pattern, &repository.name) {
                continue;
            }
            matched = sc.repo_pattern;
            for a in sc.actions {
                if seen.insert(a.clone()) {
                    actions.push(a);
                }
            }
        }
        if matched.is_empty() {
            continue;
        }
        out.push(RepoTokenDTO {
            token_id: t.id,
            name: t.name,
            owner: owner.get(&t.user_id).cloned().unwrap_or_default(),
            repo_pattern: matched,
            actions,
            unscoped: false,
            expires_at: t.expires_at,
            last_used_at: t.last_used_at,
        });
    }
    write_json(StatusCode::OK, out)
}

#[derive(Debug, Clone, Default, Serialize)]
struct ArtifactDTO {
    path: String,
    version: String,
    size: i64,
    content_type: String,
    published_at: Option<DateTime<Utc>>,
    cached_at: DateTime<Utc>,
    last_accessed_at: DateTime<Utc>,
    /// The principal who first cached/uploaded the artifact ("" = anonymous).
    cached_by: String,
    /// Who last downloaded it ("" = anonymous or never served since the column
    /// was introduced). Minute-grained like `last_accessed_at`.
    last_accessed_by: String,
    /// Successful GET responses recorded in retained audit history over 30 days.
    downloads_30d: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    publication_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    artifact_role: String,
    /// The operator tags on this artifact, and whether this caller may change
    /// them (administrator on the repository, or the principal who put the
    /// artifact there). Both are always serialised: an empty list is "no
    /// labels", and the console needs the permission per row to decide whether to
    /// offer the editor at all.
    labels: Vec<ArtifactLabelDTO>,
    can_label: bool,
    /// The vulnerability scan result for this version, when scanned.
    /// `max_severity` is empty/"none" when clean or not yet scanned.
    /// `vuln_counts` is the per-severity advisory breakdown that powers the
    /// segmented severity bar (the same shape the approvals view uses).
    #[serde(skip_serializing_if = "String::is_empty")]
    max_severity: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    vuln_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vuln_counts: Option<std::collections::BTreeMap<String, i64>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    vuln_advisories: Vec<VulnAdvisory>,
    /// Provenance of the scan: the advisory source (e.g. "OSV") and when it ran.
    #[serde(skip_serializing_if = "String::is_empty")]
    vuln_source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    vuln_scanned_at: Option<DateTime<Utc>>,
    /// The resolved SPDX license(s) for this version, when resolved. Empty when
    /// not yet resolved or when the source reports no license. `license_source`
    /// names the data source (e.g. "deps.dev") and `license_resolved_at` when it
    /// ran.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    licenses: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    license_source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    license_resolved_at: Option<DateTime<Utc>>,
    /// Marks an artifact whose bytes are absent from the blob store, so serving
    /// it fails. The flag comes from references observed missing while serving or
    /// publishing, and `blob_missing_since` is when that was first seen. The view
    /// warns on these instead of leaving a user to hit the failure mid-build.
    #[serde(skip_serializing_if = "is_false")]
    blob_missing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    blob_missing_since: Option<DateTime<Utc>>,
    /// The response codes the failures produced, so the warning can name the
    /// error a user saw rather than describing it in the abstract.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    blob_missing_statuses: Vec<StatusCountDTO>,
    /// The most recent failure, so the warning can point at the error a user just
    /// saw rather than only at the aggregate.
    #[serde(skip_serializing_if = "Option::is_none")]
    blob_missing_last_seen: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "is_zero")]
    blob_missing_last_status: i64,
}

fn is_false(v: &bool) -> bool {
    !*v
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Serialize)]
struct PublicationDTO {
    id: String,
    format: String,
    coordinate: String,
    package: String,
    version: String,
    asset_count: i64,
    total_size: i64,
    actions: Vec<String>,
    yanked: bool,
    /// How many of this publication's assets have lost their blob bytes and can
    /// no longer be served. Counted server-side from every tracked reference in
    /// the repository, not from the current artifact page, so paging cannot hide a
    /// broken asset.
    #[serde(skip_serializing_if = "is_zero")]
    broken_assets: i64,
    created_by: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct ArtifactQuery {
    #[serde(default)]
    pub(super) prefix: String,
    /// Narrows the listing to the artifacts one Statistics panel counts. See
    /// [`ArtifactFilter`].
    #[serde(default)]
    pub(super) filter: String,
    #[serde(default)]
    pub(super) q: String,
    #[serde(default)]
    pub(super) limit: String,
    #[serde(default)]
    pub(super) offset: String,
    #[serde(default)]
    pub(super) regex: String,
    #[serde(default)]
    pub(super) path: String,
    #[serde(default)]
    pub(super) force: String,
    #[serde(default)]
    pub(super) event: String,
}

/// The artifacts a Statistics panel counts, so its drill-down lists exactly
/// what the number summarises. Labeling coverage is counted over the whole
/// repository, so `Labeled` filters in SQL. The scan, license and broken panels
/// are counted over the most recently accessed [`STATS_WINDOW`] artifacts
/// (the console's sample), so those filters apply over that same window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtifactFilter {
    Labeled,
    Scanned,
    Clean,
    Vulnerable,
    Licensed,
    Broken,
}

/// The sample the Statistics tab aggregates its scan panels over: one page of
/// the listing at its maximum size.
const STATS_WINDOW: i64 = 500;

impl ArtifactFilter {
    fn parse(raw: &str) -> Result<Option<Self>, String> {
        Ok(Some(match raw {
            "" => return Ok(None),
            "labeled" => Self::Labeled,
            "scanned" => Self::Scanned,
            "clean" => Self::Clean,
            "vulnerable" => Self::Vulnerable,
            "licensed" => Self::Licensed,
            "broken" => Self::Broken,
            other => return Err(format!("unknown filter {other:?}")),
        }))
    }

    /// Reports whether an enriched row is one this filter keeps. `Labeled` is
    /// applied in SQL and keeps every row it is handed.
    fn keeps(self, a: &ArtifactDTO) -> bool {
        match self {
            Self::Labeled => true,
            Self::Scanned => !a.max_severity.is_empty(),
            Self::Clean => a.max_severity == "none",
            Self::Vulnerable => !a.max_severity.is_empty() && a.max_severity != "none",
            Self::Licensed => !a.licenses.is_empty(),
            Self::Broken => a.blob_missing,
        }
    }
}

/// Returns the artifacts stored (hosted or cached) in a repository, powering the
/// Nexus-style artifact browser in the UI.
pub(super) async fn list_artifacts(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<ArtifactQuery>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let repository = match h.store.get_repository(id).await {
        Ok(repository) => repository,
        Err(err) => return map_error(err),
    };
    if let Some(response) = h.can_read_repo(&parts, &repository.name) {
        return response;
    }
    let mut limit = int_param(&query.limit, 50);
    if !(1..=500).contains(&limit) {
        limit = 50;
    }
    let offset = int_param(&query.offset, 0).max(0);
    let filter = match ArtifactFilter::parse(&query.filter) {
        Ok(filter) => filter,
        Err(msg) => return write_error(StatusCode::BAD_REQUEST, &msg),
    };
    if filter.is_some() && (!query.prefix.is_empty() || query.regex == "true") {
        return write_error(
            StatusCode::BAD_REQUEST,
            "filter combines only with a plain q search",
        );
    }
    let page = if let Some(filter) = filter {
        if filter == ArtifactFilter::Labeled {
            h.store
                .search_repo_labeled_artifacts(id, &query.q, limit, offset)
                .await
        } else {
            // The whole window is enriched and filtered below, then paged.
            h.store
                .search_repo_artifacts(id, &query.q, STATS_WINDOW, 0)
                .await
        }
    } else if !query.prefix.is_empty() {
        // The legacy path-prefix filter (kept for API compatibility).
        h.store
            .list_repo_artifacts(id, &query.prefix, 500)
            .await
            .map(|arts| {
                let filtered = arts.len() as i64;
                (arts, filtered)
            })
    } else if query.regex == "true" && !query.q.is_empty() {
        match super::search_regex(&query.q) {
            Ok(re) => {
                h.store
                    .search_repo_artifacts_regex(id, &re, limit, offset)
                    .await
            }
            Err(err) => {
                return write_error(StatusCode::BAD_REQUEST, &format!("invalid regex: {err}"));
            }
        }
    } else {
        h.store
            .search_repo_artifacts(id, &query.q, limit, offset)
            .await
    };
    let (arts, mut filtered) = match page {
        Ok(page) => page,
        Err(err) => return map_error(err),
    };
    let now = Utc::now();
    let downloads = match h
        .store
        .artifact_download_counts(
            &repository.name,
            arts.iter().map(|a| a.path.clone()).collect(),
            now - chrono::Duration::days(30),
            now,
        )
        .await
    {
        Ok(counts) => counts,
        Err(err) => return map_error(err),
    };
    let count = h.store.count_artifacts(id).await.unwrap_or_default();
    let size = h.store.repo_size(id).await.unwrap_or_default();
    let publications = match h.store.list_artifact_publications(id).await {
        Ok(publications) => publications,
        Err(err) => return map_error(err),
    };
    // References already observed to be missing their bytes, so broken artifacts
    // can be flagged without a blob store round trip per row.
    let manager = h.repo_manager();
    let dangling = match &manager {
        Some(manager) => manager.dangling_refs_for_repo(id),
        None => HashMap::new(),
    };
    // Fold the tracked references into per-publication counts. A publication row
    // otherwise looks healthy while its tarball is unservable, which is exactly
    // the state that makes an install fail with the version sitting right there
    // in the list.
    let mut broken_by_publication: HashMap<String, i64> = HashMap::new();
    for (path, reference) in &dangling {
        let Ok(artifact) = h.store.get_artifact(id, path).await else {
            continue;
        };
        if artifact.blob_sha256 != reference.sha256 || artifact.publication_id.is_empty() {
            continue;
        }
        *broken_by_publication
            .entry(artifact.publication_id.clone())
            .or_default() += 1;
    }
    // Labels for the whole repository in one query, and the publishers of its
    // publications, so each row's labels and label permission cost nothing extra.
    let labels_by_path = match h.store.artifact_labels_by_path(id).await {
        Ok(labels) => labels,
        Err(err) => return map_error(err),
    };
    // Every label row names a stored path (the foreign key cascades on removal),
    // so the repository-wide map's size is the number of labeled artifacts.
    let labeled_count = labels_by_path.len() as i64;
    let publishers: HashMap<String, String> = publications
        .iter()
        .map(|publication| (publication.id.clone(), publication.created_by.clone()))
        .collect();
    let label_perm = h.new_label_permission(&parts, &repository.name, publishers);
    let mut out = Vec::with_capacity(arts.len());
    for a in &arts {
        let mut dto = ArtifactDTO {
            path: a.path.clone(),
            version: a.version.clone(),
            size: a.size,
            content_type: a.content_type.clone(),
            published_at: a.published_at,
            cached_at: a.cached_at,
            last_accessed_at: a.last_accessed_at,
            cached_by: a.cached_by.clone(),
            last_accessed_by: a.last_accessed_by.clone(),
            downloads_30d: downloads.get(&a.path).copied().unwrap_or_default(),
            publication_id: a.publication_id.clone(),
            artifact_role: a.artifact_role.clone(),
            labels: label_dtos(labels_by_path.get(&a.path).cloned().unwrap_or_default()),
            can_label: label_perm.allows(a),
            ..Default::default()
        };
        if let Some(reference) = dangling.get(&a.path)
            && let Some(manager) = &manager
        {
            // Confirm against the blob store before warning: bytes may have been
            // restored at the same digest since the failure was recorded.
            if reference.sha256 == a.blob_sha256
                && manager.still_missing(id, &a.path, &a.blob_sha256).await
            {
                dto.blob_missing = true;
                dto.blob_missing_since = Some(reference.first_seen);
                dto.blob_missing_statuses = status_counts(&reference.statuses);
                dto.blob_missing_last_seen = Some(reference.last_seen);
                dto.blob_missing_last_status = reference.last_status;
            } else if reference.sha256 != a.blob_sha256 {
                // The artifact was rewritten (a republish, or a proxy re-fetch)
                // and now points at different bytes, so the recorded failure no
                // longer applies.
                manager.forget_dangling_ref(id, &a.path);
            }
        }
        // Attach the stored vulnerability scan for this coordinate, if any.
        if !a.version.is_empty() {
            let (eco, pkg) = repo::vuln_coordinate(&repository.format, &a.path);
            if !pkg.is_empty()
                && let Ok(scan) = h.store.get_vuln_scan(&eco, &pkg, &a.version).await
            {
                dto.max_severity = scan.max_severity;
                dto.vuln_ids = scan.vuln_ids;
                dto.vuln_counts = Some(scan.severity_counts.into_iter().collect());
                dto.vuln_advisories = scan.advisories;
                dto.vuln_source = scan.source;
                if !meta::time::is_zero(scan.scanned_at) {
                    dto.vuln_scanned_at = Some(scan.scanned_at);
                }
            }
            // Attach the stored license resolution for this coordinate, if any.
            let (system, pkg) = repo::license_coordinate(&repository.format, &a.path);
            if !pkg.is_empty()
                && let Ok(ls) = h.store.get_license_scan(&system, &pkg, &a.version).await
            {
                dto.licenses = ls.licenses;
                dto.license_source = ls.source;
                if !meta::time::is_zero(ls.resolved_at) {
                    dto.license_resolved_at = Some(ls.resolved_at);
                }
            }
        }
        out.push(dto);
    }
    if let Some(filter) = filter
        && filter != ArtifactFilter::Labeled
    {
        out.retain(|a| filter.keeps(a));
        filtered = out.len() as i64;
        out = out
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();
    }
    let p = auth::from_request_parts(&parts);
    let can_write = h.authz.is_none()
        || p.as_ref()
            .is_some_and(|p| p.can(&repository.name, auth::ACTION_WRITE));
    let can_delete = h.authz.is_none()
        || p.as_ref()
            .is_some_and(|p| p.can(&repository.name, auth::ACTION_DELETE));
    let publication_out: Vec<PublicationDTO> = publications
        .into_iter()
        .map(|publication| {
            let mut actions: Vec<String> = Vec::new();
            match publication.format.as_str() {
                meta::FORMAT_MAVEN => {
                    if can_write && can_delete {
                        actions.push("replace".to_string());
                    }
                    if can_delete {
                        actions.push("delete".to_string());
                    }
                }
                meta::FORMAT_PYPI => {
                    if can_write {
                        actions.push("extend".to_string());
                    }
                    if can_delete {
                        actions.push("delete".to_string());
                    }
                }
                meta::FORMAT_NPM if can_delete => actions.push("delete".to_string()),
                meta::FORMAT_CARGO if can_write => {
                    actions.push(if publication.yanked { "unyank" } else { "yank" }.to_string());
                }
                _ => {}
            }
            PublicationDTO {
                broken_assets: broken_by_publication
                    .get(&publication.id)
                    .copied()
                    .unwrap_or_default(),
                id: publication.id,
                format: publication.format,
                coordinate: publication.coordinate,
                package: publication.package_name,
                version: publication.version,
                asset_count: publication.asset_count,
                total_size: publication.total_size,
                actions,
                yanked: publication.yanked,
                created_by: publication.created_by,
                created_at: publication.created_at,
                updated_at: publication.updated_at,
            }
        })
        .collect();
    let view_path = if !query.prefix.is_empty() {
        query.prefix.clone()
    } else if !query.q.is_empty() {
        query.q.clone()
    } else {
        "(artifacts)".to_string()
    };
    h.audit_view(&parts, &repository.name, &view_path);
    write_json(
        StatusCode::OK,
        ArtifactListDTO {
            count,
            total_size: size,
            filtered,
            labeled_count,
            artifacts: out,
            publications: publication_out,
        },
    )
}

/// One page of a repository's artifacts together with the publications that
/// group them. Every field is always serialised, including the empty cases, so a
/// client never has to tell "no artifacts" apart from "the server did not say".
#[derive(Debug, Clone, Serialize)]
struct ArtifactListDTO {
    /// `count` is every artifact in the repository, ignoring the active search;
    /// `filtered` is how many match it across all pages.
    count: i64,
    total_size: i64,
    filtered: i64,
    /// Artifacts in the repository carrying at least one label, ignoring the
    /// active search, so `labeled_count / count` is the labeling coverage.
    labeled_count: i64,
    artifacts: Vec<ArtifactDTO>,
    publications: Vec<PublicationDTO>,
}

/// One page of a repository's audit log. `count` is every entry matching the
/// filter, not just this page, so a client can page without a second request.
#[derive(Debug, Clone, Serialize)]
struct AuditLogListDTO {
    count: i64,
    logs: Vec<AuditLogDTO>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ValidateUploadReq {
    #[serde(default)]
    path: String,
    #[serde(default)]
    size: i64,
    #[serde(default)]
    content_type: String,
}

/// The browser preflight. It performs all path, format, size, repository-state
/// and collision checks without reading or storing bytes. The PUT endpoint
/// repeats these checks for security.
pub(super) async fn validate_upload(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let repository = match h.store.get_repository(id).await {
        Ok(repository) => repository,
        Err(err) => return map_error(err),
    };
    if let Some(response) = h.can_write_repo(&parts, &repository.name) {
        return response;
    }
    let Ok(bytes) = axum::body::to_bytes(body, 64 << 10).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let Ok(req) = serde_json::from_slice::<ValidateUploadReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let Some(manager) = h.repo_manager() else {
        return write_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "upload service is unavailable",
        );
    };
    match manager
        .validate_hosted_upload(id, &req.path, &req.content_type, req.size)
        .await
    {
        Ok(plan) => write_json(StatusCode::OK, plan),
        Err(err) => map_upload_error(err),
    }
}

/// Streams a preflighted file straight into the blob store. A raw request body
/// avoids multipart buffering and keeps memory use constant.
pub(super) async fn upload_artifact(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<ArtifactQuery>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let repository = match h.store.get_repository(id).await {
        Ok(repository) => repository,
        Err(err) => return map_error(err),
    };
    if let Some(response) = h.can_write_repo(&parts, &repository.name) {
        return response;
    }
    let Some(manager) = h.repo_manager() else {
        return write_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "upload service is unavailable",
        );
    };
    let Some(content_length) = super::header_str(&parts, http::header::CONTENT_LENGTH)
        .parse::<i64>()
        .ok()
        .filter(|n| *n >= 0)
    else {
        return write_error(StatusCode::LENGTH_REQUIRED, "content length is required");
    };
    let reader = tokio_util::io::StreamReader::new(futures_util::TryStreamExt::map_err(
        body.into_data_stream(),
        std::io::Error::other,
    ));
    let content_type = super::header_str(&parts, http::header::CONTENT_TYPE);
    match manager
        .upload_hosted(
            &parts,
            id,
            &query.path,
            &content_type,
            content_length,
            reader,
        )
        .await
    {
        Ok(result) => {
            h.audit_artifact_upload(&parts, &repository.name, &result.plan.path, 201);
            write_json(StatusCode::CREATED, result)
        }
        Err(err) => map_upload_error(err),
    }
}

/// Adds the upload-specific outcome to the shared store-error mapping: a
/// validation failure is safe to hand back to the client as its own 4xx.
fn map_upload_error(err: repo::UploadError) -> Response {
    match err {
        repo::UploadError::Validation(validation) => {
            write_error(validation.status, &validation.message)
        }
        repo::UploadError::Meta(err) => map_error(err),
        other => write_error(StatusCode::INTERNAL_SERVER_ERROR, &other.to_string()),
    }
}

impl Handler {
    fn audit_artifact_upload(
        &self,
        parts: &Parts,
        repo_name: &str,
        artifact_path: &str,
        status: i64,
    ) {
        let Some(rec) = &self.rec else {
            return;
        };
        rec.record(audit::Event {
            repo: repo_name.to_string(),
            action: meta::EVENT_UPLOAD.to_string(),
            path: artifact_path.to_string(),
            username: principal_name(parts),
            method: parts.method.to_string(),
            status,
            client_ip: audit::client_ip_parts(parts),
            user_agent: super::header_str(parts, http::header::USER_AGENT),
            ..Default::default()
        });
    }

    /// Records an artifact deletion in the repository's audit log, with the
    /// artifact path (or "(all artifacts)") in the path column.
    pub(super) fn audit_artifact(&self, parts: &Parts, repo_name: &str, path: &str, status: i64) {
        let Some(rec) = &self.rec else {
            return;
        };
        rec.record(audit::Event {
            repo: repo_name.to_string(),
            action: meta::EVENT_DELETE.to_string(),
            path: path.to_string(),
            username: principal_name(parts),
            method: parts.method.to_string(),
            status,
            client_ip: audit::client_ip_parts(parts),
            user_agent: super::header_str(parts, http::header::USER_AGENT),
            ..Default::default()
        });
    }

    /// Records a console read (repository detail view or artifact browse) so the
    /// audit trail covers plain lookups and not only mutations. `path` names what
    /// was read, e.g. "(repository)" or an artifact prefix.
    fn audit_view(&self, parts: &Parts, repo_name: &str, path: &str) {
        let Some(rec) = &self.rec else {
            return;
        };
        rec.record(audit::Event {
            repo: repo_name.to_string(),
            action: meta::EVENT_VIEW.to_string(),
            path: path.to_string(),
            username: principal_name(parts),
            method: parts.method.to_string(),
            status: 200,
            client_ip: audit::client_ip_parts(parts),
            user_agent: super::header_str(parts, http::header::USER_AGENT),
            ..Default::default()
        });
    }

    /// Records a forced delete distinctly from an ordinary one: it bypassed the
    /// managed-artifact guard, so the reason and the removed publication belong
    /// in the trail.
    fn audit_force_delete(
        &self,
        parts: &Parts,
        repo_name: &str,
        path: &str,
        sha: &str,
        result: &ForceDeleteResult,
    ) {
        let Some(rec) = &self.rec else {
            return;
        };
        let detail = serde_json::json!({
            "force": true,
            "publication_coordinate": result.coordinate,
            "publication_id": result.publication_id,
            "reason": "blob bytes missing from the blob store",
            "sha256": sha,
        });
        rec.record(audit::Event {
            repo: repo_name.to_string(),
            action: meta::EVENT_DELETE.to_string(),
            path: path.to_string(),
            username: principal_name(parts),
            method: parts.method.to_string(),
            status: 200,
            client_ip: audit::client_ip_parts(parts),
            user_agent: super::header_str(parts, http::header::USER_AGENT),
            detail_json: detail.to_string(),
            ..Default::default()
        });
    }
}

/// Reports this repository's artifacts whose blob bytes are missing. Views that
/// show artifact paths (the audit log, for one) use it to mark the affected rows,
/// so a reader is not left correlating a 5xx by hand.
pub(super) async fn list_dangling(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let repository = match h.store.get_repository(id).await {
        Ok(repository) => repository,
        Err(err) => return map_error(err),
    };
    if let Some(response) = h.can_read_repo(&parts, &repository.name) {
        return response;
    }
    write_json(StatusCode::OK, dangling_refs(&h, id).await)
}

/// Reports how many artifacts a purge removed. A purge takes out an unknown
/// number of rows, so the count is the only way the caller learns what happened.
#[derive(Debug, Clone, Serialize)]
struct ArtifactPurgeResultDTO {
    deleted: i64,
}

/// The other shape this route answers with: a forced delete names the one path it
/// removed, the digest that was missing, and whether the publication went with
/// it. Note that "deleted" is a path here and a count above — the same key, two
/// types, which is why the document declares the response as one or the other
/// rather than merging them.
#[derive(Debug, Clone, Serialize)]
struct ArtifactForceDeleteResultDTO {
    deleted: String,
    sha256: String,
    publication_deleted: bool,
    publication_coordinate: String,
}

/// Removes artifacts from a repository (admin only, via the admin route group).
/// With a `path` query parameter it deletes that one artifact; without it, it
/// purges every artifact in the repository (the Danger Zone "purge all" action).
/// Blob bytes are reclaimed asynchronously by the sweeper once their reference
/// count reaches zero.
pub(super) async fn delete_artifact(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<ArtifactQuery>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let repository = match h.store.get_repository(id).await {
        Ok(repository) => repository,
        Err(err) => return map_error(err),
    };

    let path = query.path.trim().to_string();
    let force = query.force == "true";
    if path.is_empty() {
        if force {
            // Force exists to repair one broken artifact, never to bypass the
            // managed guard wholesale. A repository-wide purge has no place doing
            // that.
            return write_error(StatusCode::BAD_REQUEST, "force requires a path");
        }
        // No path: purge the whole repository's artifacts.
        let deleted = match h.store.purge_artifacts(id).await {
            Ok(deleted) => deleted,
            Err(err) => return map_error(err),
        };
        h.audit_artifact(&parts, &repository.name, "(all artifacts)", 200);
        return write_json(StatusCode::OK, ArtifactPurgeResultDTO { deleted });
    }

    if force {
        return force_delete_artifact(&h, &parts, &repository, &path).await;
    }
    if let Err(err) = h.store.delete_artifact(id, &path).await {
        return map_error(err);
    }
    h.audit_artifact(&parts, &repository.name, &path, 204);
    StatusCode::NO_CONTENT.into_response()
}

/// Removes an artifact whose bytes are gone, which the ordinary delete refuses
/// when the artifact is owned by a managed publication.
///
/// The refusal is normally right: managed artifacts must be removed through the
/// publication lifecycle so the derived metadata stays consistent. But when the
/// bytes are missing, that lifecycle cannot run — it has to read the derived
/// metadata first, and that is precisely what is unreadable — leaving an artifact
/// that can be neither served nor deleted. This is the escape hatch, and it opens
/// only on proof that the bytes are absent: anything else, including a blob store
/// that cannot be reached, keeps it shut.
async fn force_delete_artifact(
    h: &Arc<Handler>,
    parts: &Parts,
    repository: &Repository,
    path: &str,
) -> Response {
    let Some(manager) = h.repo_manager() else {
        return write_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "force delete is unavailable",
        );
    };
    let artifact: Artifact = match h.store.get_artifact(repository.id, path).await {
        Ok(artifact) => artifact,
        Err(err) => return map_error(err),
    };
    let missing = match manager.confirmed_missing(&artifact.blob_sha256).await {
        Ok(missing) => missing,
        // Undecidable: the blob store did not answer. Refusing keeps a storage
        // outage from turning into a licence to delete healthy artifacts.
        Err(err) => {
            return write_error(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("cannot confirm the artifact bytes are missing: {err}"),
            );
        }
    };
    if !missing {
        return write_error(
            StatusCode::CONFLICT,
            "artifact bytes are present; use the publication lifecycle to delete it",
        );
    }
    let result = match h.store.force_delete_artifact(repository.id, path).await {
        Ok(result) => result,
        Err(err) => return map_error(err),
    };
    manager.forget_dangling_ref(repository.id, path);
    h.audit_force_delete(
        parts,
        &repository.name,
        path,
        &artifact.blob_sha256,
        &result,
    );
    write_json(
        StatusCode::OK,
        ArtifactForceDeleteResultDTO {
            deleted: path.to_string(),
            sha256: artifact.blob_sha256,
            publication_deleted: !result.publication_id.is_empty(),
            publication_coordinate: result.coordinate,
        },
    )
}

/// Bounds the whole probe handler — the metadata read plus the outbound probe —
/// with a deadline the server owns. The metadata DB uses a single SQLite
/// connection, so a contended connection would otherwise block the repository
/// read indefinitely, leaving the client's "checking…" badge spinning. Set
/// slightly above the 5s probe client timeout so a genuinely slow upstream still
/// resolves via the probe rather than tripping this guard.
const UPSTREAM_HEALTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// Probes a proxy repository's upstream with a short timeout. Any HTTP response
/// (even 4xx) means the upstream is reachable; only transport errors are treated
/// as unreachable.
pub(super) async fn upstream_health(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let repository =
        match tokio::time::timeout(UPSTREAM_HEALTH_TIMEOUT, h.store.get_repository(id)).await {
            Ok(Ok(repository)) => repository,
            Ok(Err(err)) => return map_error(err),
            Err(_) => return map_error(meta::Error::Other("timeout".to_string())),
        };
    if let Some(response) = h.can_read_repo(&parts, &repository.name) {
        return response;
    }
    if repository.r#type != meta::TYPE_PROXY || repository.upstream_url.is_empty() {
        return write_json(StatusCode::OK, serde_json::json!({"applicable": false}));
    }
    // Probe with the repository's stored credentials so an auth-required upstream
    // is not misreported: any HTTP response counts as reachable, but the surfaced
    // status should reflect what real fetches will see.
    let upstream_auth = repoconfig::parse(&repository.config_json)
        .map(|cfg| cfg.upstream_auth)
        .unwrap_or_default();
    write_json(
        StatusCode::OK,
        probe_upstream(&h, &repository.upstream_url, &upstream_auth).await,
    )
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CheckUpstreamReq {
    #[serde(default)]
    url: String,
    /// Reuses an existing repository's stored upstream credentials (clients only
    /// ever see masked secrets, so they cannot resend them).
    #[serde(default)]
    repository_id: i64,
    /// Inline credentials for pre-create connectivity checks.
    #[serde(default)]
    auth: Option<repoconfig::UpstreamAuthConfig>,
}

/// Probes an arbitrary upstream URL so the New repository form can validate
/// connectivity before the repository is created. Admin-only (the route group),
/// mirroring [`upstream_health`]'s reachability semantics.
pub(super) async fn check_upstream(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (_, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let Ok(req) = serde_json::from_slice::<CheckUpstreamReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let raw = req.url.trim().to_string();
    if raw.is_empty() {
        return write_error(StatusCode::BAD_REQUEST, "url is required");
    }
    let probe_url = url::Url::parse(&raw);
    let valid = probe_url.as_ref().is_ok_and(|u| {
        (u.scheme() == "http" || u.scheme() == "https") && !u.host_str().unwrap_or("").is_empty()
    });
    if !valid {
        return write_json(
            StatusCode::OK,
            serde_json::json!({
                "applicable": true,
                "error": "url must be http(s) with a host",
                "reachable": false,
            }),
        );
    }
    let probe_url = probe_url.expect("checked above");
    let mut upstream_auth = repoconfig::UpstreamAuthConfig::default();
    if req.auth.as_ref().is_some_and(|a| !a.type_.is_empty()) {
        let supplied = req.auth.expect("checked above");
        if let Err(err) = supplied.validate() {
            return write_error(StatusCode::BAD_REQUEST, &err.to_string());
        }
        upstream_auth = supplied;
    } else if req.repository_id != 0 {
        let repository = match h.store.get_repository(req.repository_id).await {
            Ok(repository) => repository,
            Err(err) => return map_error(err),
        };
        // Stored credentials are only ever presented to the host they were
        // configured for. Without this pin, a probe URL pointing anywhere would
        // exfiltrate the (otherwise always masked) secret to that host.
        let stored = url::Url::parse(&repository.upstream_url);
        let same_target = stored.as_ref().is_ok_and(|stored| {
            probe_url.scheme() == stored.scheme()
                && probe_url.host_str() == stored.host_str()
                && probe_url.port_or_known_default() == stored.port_or_known_default()
        });
        if !same_target {
            return write_error(
                StatusCode::BAD_REQUEST,
                "url must match the repository's upstream scheme and host",
            );
        }
        if let Ok(cfg) = repoconfig::parse(&repository.config_json) {
            upstream_auth = cfg.upstream_auth;
        }
    }
    write_json(
        StatusCode::OK,
        probe_upstream(&h, &raw, &upstream_auth).await,
    )
}

/// Issues a short GET to `raw_url` with the given upstream credentials and
/// reports reachability, the HTTP status, and latency. Any HTTP response (even
/// 4xx) counts as reachable; only a transport error is unreachable.
async fn probe_upstream(
    h: &Arc<Handler>,
    raw_url: &str,
    upstream_auth: &repoconfig::UpstreamAuthConfig,
) -> serde_json::Value {
    let start = std::time::Instant::now();
    let request = upstream_auth.apply(h.client.get(raw_url));
    let result = tokio::time::timeout(UPSTREAM_HEALTH_TIMEOUT, request.send()).await;
    let latency = start.elapsed().as_millis() as i64;
    match result {
        Ok(Ok(resp)) => serde_json::json!({
            "applicable": true,
            "latency_ms": latency,
            "reachable": true,
            "status": resp.status().as_u16(),
        }),
        Ok(Err(err)) => serde_json::json!({
            "applicable": true,
            "error": err.to_string(),
            "latency_ms": latency,
            "reachable": false,
        }),
        Err(_) => serde_json::json!({
            "applicable": true,
            "error": "context deadline exceeded",
            "latency_ms": latency,
            "reachable": false,
        }),
    }
}

pub(super) async fn delete(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    // Resolve the name before deletion so the audit entry can reference it.
    let repository = match h.store.get_repository(id).await {
        Ok(repository) => repository,
        Err(err) => return map_error(err),
    };
    // Predefined seed repositories are protected: deleting one would break the
    // groups that reference it and it would reappear on the next startup, so it
    // is refused regardless of the caller's admin rights.
    if repo::is_default_repo(&repository.name) {
        h.audit(&parts, &repository.name, meta::EVENT_REPO_DELETE, 403);
        return write_error(
            StatusCode::FORBIDDEN,
            "cannot delete a predefined repository",
        );
    }
    if let Err(err) = h.store.delete_repository(id).await {
        return map_error(err);
    }
    // Approvals and version denies must not outlive the repo: a recreated
    // same-name repo would silently inherit its trust decisions.
    if let Err(err) = h.store.delete_approvals_for_repo(&repository.name).await {
        tracing::error!(repo = %repository.name, err = %err, "delete approvals for repo failed");
    }
    if let Err(err) = h
        .store
        .delete_version_denies_for_repo(&repository.name)
        .await
    {
        tracing::error!(repo = %repository.name, err = %err, "delete version denies for repo failed");
    }
    h.audit(&parts, &repository.name, meta::EVENT_REPO_DELETE, 204);
    StatusCode::NO_CONTENT.into_response()
}

/// The JSON shape for one audit log entry.
#[derive(Debug, Clone, Serialize)]
struct AuditLogDTO {
    id: i64,
    event: String,
    path: String,
    username: String,
    method: String,
    status: i64,
    client_ip: String,
    user_agent: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    request_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    detail_json: String,
    created_at: DateTime<Utc>,
}

/// Returns a repository's audit log, newest first, with optional event filtering
/// and limit/offset pagination.
pub(super) async fn list_audit_logs(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<ArtifactQuery>,
) -> Response {
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let repository = match h.store.get_repository(id).await {
        Ok(repository) => repository,
        Err(err) => return map_error(err),
    };
    let mut limit = int_param(&query.limit, 100);
    if !(1..=500).contains(&limit) {
        limit = 100;
    }
    let offset = int_param(&query.offset, 0).max(0);
    let logs = match h
        .store
        .list_audit_logs(&repository.name, &query.event, limit, offset)
        .await
    {
        Ok(logs) => logs,
        Err(err) => return map_error(err),
    };
    let count = match h
        .store
        .count_audit_logs(&repository.name, &query.event)
        .await
    {
        Ok(count) => count,
        Err(err) => return map_error(err),
    };
    write_json(
        StatusCode::OK,
        AuditLogListDTO {
            count,
            logs: logs
                .into_iter()
                .map(|l| AuditLogDTO {
                    id: l.id,
                    event: l.event,
                    path: l.path,
                    username: l.username,
                    method: l.method,
                    status: l.status,
                    client_ip: l.client_ip,
                    user_agent: l.user_agent,
                    request_id: l.request_id,
                    detail_json: l.detail_json,
                    created_at: l.created_at,
                })
                .collect(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_filter_parses_and_keeps() {
        assert_eq!(ArtifactFilter::parse(""), Ok(None));
        assert_eq!(
            ArtifactFilter::parse("broken"),
            Ok(Some(ArtifactFilter::Broken))
        );
        assert!(ArtifactFilter::parse("Broken").is_err());

        let unscanned = ArtifactDTO::default();
        let clean = ArtifactDTO {
            max_severity: "none".to_string(),
            ..Default::default()
        };
        let vulnerable = ArtifactDTO {
            max_severity: "critical".to_string(),
            licenses: vec!["MIT".to_string()],
            ..Default::default()
        };
        let broken = ArtifactDTO {
            blob_missing: true,
            ..Default::default()
        };
        let kept = |filter: ArtifactFilter| -> Vec<bool> {
            [&unscanned, &clean, &vulnerable, &broken]
                .into_iter()
                .map(|a| filter.keeps(a))
                .collect()
        };
        assert_eq!(kept(ArtifactFilter::Scanned), [false, true, true, false]);
        assert_eq!(kept(ArtifactFilter::Clean), [false, true, false, false]);
        assert_eq!(
            kept(ArtifactFilter::Vulnerable),
            [false, false, true, false]
        );
        assert_eq!(kept(ArtifactFilter::Licensed), [false, false, true, false]);
        assert_eq!(kept(ArtifactFilter::Broken), [false, false, false, true]);
        assert_eq!(kept(ArtifactFilter::Labeled), [true, true, true, true]);
    }
}
