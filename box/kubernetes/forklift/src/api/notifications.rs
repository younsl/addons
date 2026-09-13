use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Request, State};
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::meta::{self, Receiver};
use crate::notify;
use crate::repoconfig;

use super::repositories::path_id;
use super::{
    Handler, NAME_RULE_MSG, StatusMessageDTO, map_error, principal_name, valid_name, write_error,
    write_json,
};

/// The wire shape of a notification receiver. The webhook URL is write-only —
/// never returned once stored (it may carry a secret token); a boolean reports
/// only whether one is configured.
#[derive(Debug, Clone, Serialize)]
struct ReceiverDTO {
    id: i64,
    name: String,
    description: String,
    webhook_configured: bool,
    enabled: bool,
    created_by: String,
    created_at: String,
    updated_at: String,
    /// The repositories whose notify config selects this receiver by name. A
    /// receiver with a non-empty list cannot be deleted.
    repositories: Vec<String>,
}

fn to_receiver_dto(r: Receiver) -> ReceiverDTO {
    ReceiverDTO {
        id: r.id,
        name: r.name,
        description: r.description,
        webhook_configured: !r.webhook_url.is_empty(),
        enabled: r.enabled,
        created_by: r.created_by,
        created_at: r
            .created_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        updated_at: r
            .updated_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        repositories: Vec::new(),
    }
}

impl Handler {
    /// Maps each receiver name to the repositories whose notify config selects
    /// it. Receivers are referenced by name inside repository config JSON, so
    /// usage is resolved by scanning the (small) repository list.
    async fn receiver_repo_usage(&self) -> Result<HashMap<String, Vec<String>>, meta::Error> {
        let repos = self.store.list_repositories().await?;
        let mut usage: HashMap<String, Vec<String>> = HashMap::new();
        for repo in repos {
            let Ok(cfg) = repoconfig::parse(&repo.config_json) else {
                continue;
            };
            for name in cfg.notify.receivers {
                usage.entry(name).or_default().push(repo.name.clone());
            }
        }
        Ok(usage)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ReceiverReq {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    webhook_url: String,
    #[serde(default)]
    enabled: Option<bool>,
}

/// Accepts only absolute http(s) URLs.
fn valid_webhook_url(s: &str) -> bool {
    match url::Url::parse(s.trim()) {
        Ok(u) => {
            (u.scheme() == "http" || u.scheme() == "https")
                && !u.host_str().unwrap_or("").is_empty()
        }
        Err(_) => false,
    }
}

/// Returns all notification receivers (oldest first).
pub(super) async fn list(State(h): State<Arc<Handler>>) -> Response {
    let receivers = match h.store.list_receivers().await {
        Ok(receivers) => receivers,
        Err(err) => return map_error(err),
    };
    let usage = match h.receiver_repo_usage().await {
        Ok(usage) => usage,
        Err(err) => return map_error(err),
    };
    let out: Vec<ReceiverDTO> = receivers
        .into_iter()
        .map(|rec| {
            let repositories = usage.get(&rec.name).cloned();
            let mut dto = to_receiver_dto(rec);
            if let Some(repositories) = repositories {
                dto.repositories = repositories;
            }
            dto
        })
        .collect();
    write_json(StatusCode::OK, out)
}

/// Adds a notification receiver.
pub(super) async fn create(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let req = match decode_receiver(body).await {
        Ok(req) => req,
        Err(response) => return *response,
    };
    if !valid_name(req.name.trim()) {
        return write_error(
            StatusCode::BAD_REQUEST,
            &format!("invalid receiver name: {NAME_RULE_MSG}"),
        );
    }
    if !valid_webhook_url(&req.webhook_url) {
        return write_error(
            StatusCode::BAD_REQUEST,
            "webhook_url must be an absolute http(s) URL",
        );
    }
    let rec = match h
        .store
        .create_receiver(Receiver {
            name: req.name.trim().to_string(),
            description: req.description.trim().to_string(),
            webhook_url: req.webhook_url.trim().to_string(),
            enabled: req.enabled.unwrap_or(true),
            created_by: principal_name(&parts),
            ..Default::default()
        })
        .await
    {
        Ok(rec) => rec,
        Err(err) => return map_error(err),
    };
    write_json(StatusCode::CREATED, to_receiver_dto(rec))
}

/// Overwrites a receiver's fields.
pub(super) async fn update(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (_, body) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let req = match decode_receiver(body).await {
        Ok(req) => req,
        Err(response) => return *response,
    };
    if !valid_name(req.name.trim()) {
        return write_error(
            StatusCode::BAD_REQUEST,
            &format!("invalid receiver name: {NAME_RULE_MSG}"),
        );
    }
    // The webhook URL is never shown back, so an edit submits it blank to keep
    // the stored one; a non-empty value replaces it (and must be valid).
    let existing = match h.store.get_receiver(id).await {
        Ok(existing) => existing,
        Err(err) => return map_error(err),
    };
    let mut webhook_url = existing.webhook_url.clone();
    let supplied = req.webhook_url.trim();
    if !supplied.is_empty() {
        if !valid_webhook_url(supplied) {
            return write_error(
                StatusCode::BAD_REQUEST,
                "webhook_url must be an absolute http(s) URL",
            );
        }
        webhook_url = supplied.to_string();
    }
    let new_name = req.name.trim().to_string();
    let rec = match h
        .store
        .update_receiver(Receiver {
            id,
            name: new_name.clone(),
            description: req.description.trim().to_string(),
            webhook_url,
            enabled: req.enabled.unwrap_or(true),
            ..Default::default()
        })
        .await
    {
        Ok(rec) => rec,
        Err(err) => return map_error(err),
    };
    // Repositories reference receivers by name in their notify config, so a
    // rename must follow into every config or those repositories silently stop
    // alarming. The receiver row is already renamed; a partial config failure is
    // reported so the operator knows which repositories still hold the old name.
    if existing.name != new_name {
        let failed = h.rename_receiver_refs(&existing.name, &new_name).await;
        if !failed.is_empty() {
            return write_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!(
                    "receiver renamed, but updating references failed for: {}",
                    failed.join(", ")
                ),
            );
        }
    }
    write_json(StatusCode::OK, to_receiver_dto(rec))
}

impl Handler {
    /// Rewrites `notify.receivers` entries equal to `old_name` in every
    /// repository config and returns the repositories it could not update.
    async fn rename_receiver_refs(&self, old_name: &str, new_name: &str) -> Vec<String> {
        let repos = match self.store.list_repositories().await {
            Ok(repos) => repos,
            Err(err) => return vec![format!("(listing repositories: {err})")],
        };
        let mut failed = Vec::new();
        for repo in repos {
            let Ok(mut cfg) = repoconfig::parse(&repo.config_json) else {
                continue;
            };
            let mut changed = false;
            for name in cfg.notify.receivers.iter_mut() {
                if name == old_name {
                    *name = new_name.to_string();
                    changed = true;
                }
            }
            if !changed {
                continue;
            }
            let Ok(config_json) = cfg.json() else {
                failed.push(repo.name.clone());
                continue;
            };
            if self
                .store
                .update_repository_config(repo.id, &repo.upstream_url, &config_json)
                .await
                .is_err()
            {
                failed.push(repo.name.clone());
            }
        }
        failed
    }
}

/// Sends a test alarm to a receiver's stored webhook URL and reports whether
/// delivery succeeded. The URL is write-only, so the test must run server-side
/// using the stored value.
pub(super) async fn test(State(h): State<Arc<Handler>>, UrlPath(id): UrlPath<String>) -> Response {
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let rec = match h.store.get_receiver(id).await {
        Ok(rec) => rec,
        Err(err) => return map_error(err),
    };
    if rec.webhook_url.is_empty() {
        return write_error(
            StatusCode::BAD_REQUEST,
            "this receiver has no webhook URL configured",
        );
    }
    let Some(notifier) = h.notifier() else {
        return notifications_unconfigured();
    };
    if let Err(err) = notifier.send_test(&rec.name, &rec.webhook_url).await {
        return write_error(
            StatusCode::BAD_GATEWAY,
            &format!("test delivery failed: {err}"),
        );
    }
    write_json(
        StatusCode::OK,
        StatusMessageDTO {
            status: "sent".to_string(),
        },
    )
}

/// The ad-hoc test target: a URL that has not been saved as a receiver yet, plus
/// the name to sign the test alarm with.
#[derive(Debug, Clone, Default, Deserialize)]
struct WebhookTestReq {
    #[serde(default)]
    webhook_url: String,
    #[serde(default)]
    name: String,
}

/// Sends a test alarm to a webhook URL supplied in the request body, for
/// verifying a URL before the receiver is saved (the create/edit form). The
/// stored-receiver test lives at `/receivers/{id}/test`.
pub(super) async fn test_adhoc(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (_, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid body");
    };
    let Ok(req) = serde_json::from_slice::<WebhookTestReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid body");
    };
    let url = req.webhook_url.trim();
    if !valid_webhook_url(url) {
        return write_error(
            StatusCode::BAD_REQUEST,
            "webhook_url must be an absolute http(s) URL",
        );
    }
    let Some(notifier) = h.notifier() else {
        return notifications_unconfigured();
    };
    let name = req.name.trim();
    let name = if name.is_empty() {
        "new receiver"
    } else {
        name
    };
    if let Err(err) = notifier.send_test(name, url).await {
        return write_error(
            StatusCode::BAD_GATEWAY,
            &format!("test delivery failed: {err}"),
        );
    }
    write_json(
        StatusCode::OK,
        StatusMessageDTO {
            status: "sent".to_string(),
        },
    )
}

impl Handler {
    /// Resolves a repository's selected receivers to delivery targets, returning
    /// per-receiver status (exists / enabled) for the preview and the
    /// deliverable targets for the send.
    async fn repo_sample_targets(
        &self,
        id: i64,
    ) -> Result<(String, Vec<SampleReceiverInfo>, Vec<notify::Target>), meta::Error> {
        let repo = self.store.get_repository(id).await?;
        let cfg = repoconfig::parse(&repo.config_json)
            .map_err(|err| meta::Error::Other(err.to_string()))?;
        let all = self.store.list_receivers().await?;
        let by_name: HashMap<String, Receiver> =
            all.into_iter().map(|rec| (rec.name.clone(), rec)).collect();
        let mut info = Vec::new();
        let mut targets = Vec::new();
        for name in cfg.notify.receivers {
            let rec = by_name.get(&name);
            info.push(SampleReceiverInfo {
                name: name.clone(),
                exists: rec.is_some(),
                enabled: rec.is_some_and(|rec| rec.enabled),
            });
            if let Some(rec) = rec
                && rec.enabled
                && !rec.webhook_url.is_empty()
            {
                targets.push(notify::Target {
                    name: rec.name.clone(),
                    url: rec.webhook_url.clone(),
                });
            }
        }
        Ok((repo.name, info, targets))
    }
}

/// The alarm a repository would send together with the receivers it would reach,
/// so an operator can check both before anything is delivered.
#[derive(Debug, Clone, Serialize)]
struct NotificationSamplePreviewDTO {
    payload: notify::ApprovalPayload,
    receivers: Vec<SampleReceiverInfo>,
}

/// One receiver's delivery outcome.
#[derive(Debug, Clone, Serialize)]
pub(super) struct NotificationSampleResultDTO {
    pub(super) name: String,
    pub(super) ok: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(super) error: String,
}

/// The per-receiver outcome of an actual send.
#[derive(Debug, Clone, Serialize)]
pub(super) struct NotificationSampleReportDTO {
    pub(super) results: Vec<NotificationSampleResultDTO>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct SampleReceiverInfo {
    pub(super) name: String,
    pub(super) exists: bool,
    pub(super) enabled: bool,
}

/// Returns the sample approval payload a repository would send and the receivers
/// it would target, without delivering anything.
pub(super) async fn preview_repo_sample(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let Some(notifier) = h.notifier() else {
        return notifications_unconfigured();
    };
    let (repo_name, info, _) = match h.repo_sample_targets(id).await {
        Ok(resolved) => resolved,
        Err(err) => return map_error(err),
    };
    // Preview the real alarm from the repository's current pending approvals so
    // the operator sees exactly what would be delivered. With no pending queue,
    // fall back to a representative sample so the preview is never empty.
    let pending = h
        .store
        .list_approvals(&repo_name, meta::APPROVAL_PENDING, 20, 0)
        .await
        .unwrap_or_default();
    let mut pkgs: Vec<notify::PreviewPackage> = pending
        .into_iter()
        .map(|a| notify::PreviewPackage {
            package: a.package,
            version: a.last_requested_version,
            requested_by: a.requested_by,
        })
        .collect();
    if pkgs.is_empty() {
        pkgs = vec![
            notify::PreviewPackage {
                package: "com.example:sample".to_string(),
                version: "1.0.0".to_string(),
                requested_by: principal_name(&parts),
            },
            notify::PreviewPackage {
                package: "left-pad".to_string(),
                version: "1.3.0".to_string(),
                requested_by: principal_name(&parts),
            },
        ];
    }
    let format = h
        .store
        .get_repository(id)
        .await
        .map(|repo| repo.format)
        .unwrap_or_default();
    write_json(
        StatusCode::OK,
        NotificationSamplePreviewDTO {
            payload: notifier.preview_approval(&repo_name, id, &format, &pkgs),
            receivers: info,
        },
    )
}

/// Delivers a sample approval alarm to the repository's selected enabled
/// receivers, reporting each receiver's result.
pub(super) async fn send_repo_sample(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let Some(notifier) = h.notifier() else {
        return notifications_unconfigured();
    };
    let (repo_name, _, targets) = match h.repo_sample_targets(id).await {
        Ok(resolved) => resolved,
        Err(err) => return map_error(err),
    };
    if targets.is_empty() {
        return write_error(
            StatusCode::BAD_REQUEST,
            "no enabled receivers are selected for this repository",
        );
    }
    let payload = notifier.build_approval_sample(&repo_name, id, &principal_name(&parts));
    let mut results = Vec::with_capacity(targets.len());
    for target in targets {
        match notifier.send_approval_payload(&target.url, &payload).await {
            Ok(()) => results.push(NotificationSampleResultDTO {
                name: target.name,
                ok: true,
                error: String::new(),
            }),
            Err(err) => results.push(NotificationSampleResultDTO {
                name: target.name,
                ok: false,
                error: err.to_string(),
            }),
        }
    }
    write_json(StatusCode::OK, NotificationSampleReportDTO { results })
}

/// Removes a receiver. A receiver still selected by any repository's notify
/// config is refused: deleting it would silently drop that repository's alarms,
/// so the references must be detached first.
pub(super) async fn delete(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let rec = match h.store.get_receiver(id).await {
        Ok(rec) => rec,
        Err(err) => return map_error(err),
    };
    let usage = match h.receiver_repo_usage().await {
        Ok(usage) => usage,
        Err(err) => return map_error(err),
    };
    if let Some(repos) = usage.get(&rec.name)
        && !repos.is_empty()
    {
        return write_error(
            StatusCode::CONFLICT,
            &format!(
                "receiver is selected by {} repository(ies): {}",
                repos.len(),
                repos.join(", ")
            ),
        );
    }
    if let Err(err) = h.store.delete_receiver(id).await {
        return map_error(err);
    }
    StatusCode::NO_CONTENT.into_response()
}

/// The error is boxed because a `Response` dwarfs the request it competes with
/// in the `Result`.
async fn decode_receiver(body: axum::body::Body) -> Result<ReceiverReq, Box<Response>> {
    let invalid = || Box::new(write_error(StatusCode::BAD_REQUEST, "invalid body"));
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return Err(invalid());
    };
    serde_json::from_slice::<ReceiverReq>(&bytes).map_err(|_| invalid())
}

fn notifications_unconfigured() -> Response {
    write_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "notifications are not configured",
    )
}
