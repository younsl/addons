//! The JSON management REST API: repositories, users, roles, group mappings and
//! personal access tokens.
//!

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use http::request::Parts;
use serde::Serialize;

use crate::audit;
use crate::auth as auth_service;
use crate::coverage as coverage_scanner;
use crate::meta::{self, Store};
use crate::notify;
use crate::repo;
use crate::storage as storage_backend;
use crate::version;

mod announcement;
mod approvals;
mod artifacts_bulk;
mod auth;
mod coverage;
mod denies;
mod ha;
mod labels;
mod notifications;
mod repositories;
mod search;
mod storage;
mod uploads;

pub use ha::HAStatus;

/// Assembles live HA/leadership status.
pub type HAStatusFn = Arc<dyn Fn() -> HAStatus + Send + Sync>;
/// Asks this instance to release leadership, reporting whether it was the
/// leader (and so stepped down).
pub type HAStepDownFn = Arc<dyn Fn() -> bool + Send + Sync>;
/// Starts a manual coverage scan detached from the request, reporting whether it
/// was accepted (false when one is already running).
pub type CoverageScanFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;
/// Queries the MinIO Admin API when the backend is a MinIO endpoint.
pub type MinIOInfoFn = Arc<
    dyn Fn() -> std::pin::Pin<
            Box<dyn Future<Output = Result<storage_backend::MinIOInfo, String>> + Send>,
        > + Send
        + Sync,
>;

/// The storage-backend descriptor the Storage admin page renders.
#[derive(Debug, Clone, Default)]
pub struct StorageBackend {
    pub backend: String,
    pub endpoint: String,
    pub bucket: String,
    pub prefix: String,
}

/// Everything `main` injects after construction.
#[derive(Default)]
struct Injected {
    ha_status: Option<HAStatusFn>,
    ha_step_down: Option<HAStepDownFn>,
    notifier: Option<Arc<notify::Notifier>>,
    storage: StorageBackend,
    minio: Option<MinIOInfoFn>,
    upload_enabled: bool,
    uploader: Option<Arc<repo::Uploader>>,
    external_url: String,
    repo_manager: Option<Arc<repo::Manager>>,
    coverage: Option<Arc<coverage_scanner::Scanner>>,
    coverage_scan: Option<CoverageScanFn>,
}

/// Serves the management API.
pub struct Handler {
    pub(crate) store: Arc<Store>,
    pub(crate) authz: Option<Arc<auth_service::Service>>,
    /// Upstream reachability probes only. Redirects are never followed: a probe
    /// may carry stored credentials, and a redirecting upstream must not be able
    /// to bounce them elsewhere.
    pub(crate) client: reqwest::Client,
    pub(crate) rec: Option<Arc<audit::Recorder>>,
    injected: parking_lot::RwLock<Injected>,
}

impl Handler {
    /// Creates an API handler. `authz` may be `None` in tests that exercise only
    /// public endpoints and `rec` may be `None` to disable audit logging, but
    /// production wiring always provides both.
    pub fn new(
        store: Arc<Store>,
        authz: Option<Arc<auth_service::Service>>,
        rec: Option<Arc<audit::Recorder>>,
    ) -> Arc<Handler> {
        Arc::new(Handler {
            store,
            authz,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default(),
            rec,
            injected: parking_lot::RwLock::new(Injected::default()),
        })
    }

    /// Injects the manual coverage-scan trigger. The scan itself outlives the
    /// request that asked for it, so `main` owns its lifetime rather than the
    /// handler.
    pub fn set_coverage_scan(&self, f: CoverageScanFn) {
        self.injected.write().coverage_scan = Some(f);
    }

    /// Injects the outbound-alarm notifier used by the receiver test and
    /// repository sample/preview endpoints.
    pub fn set_notifier(&self, n: Arc<notify::Notifier>) {
        self.injected.write().notifier = Some(n);
    }

    /// Configures the global UI/API upload feature gate.
    pub fn set_upload_enabled(&self, enabled: bool) {
        self.injected.write().upload_enabled = enabled;
    }

    /// Injects the publication service after the repository engine and blob
    /// backend have been constructed.
    pub fn set_uploader(&self, uploader: Arc<repo::Uploader>, external_url: &str) {
        let mut injected = self.injected.write();
        injected.uploader = Some(uploader);
        injected.external_url = external_url.to_string();
    }

    /// Wires browser uploads to the package engine so UI traffic follows the
    /// same storage, scanning and approval path as registry clients.
    pub fn set_repo_manager(&self, m: Arc<repo::Manager>) {
        self.injected.write().repo_manager = Some(m);
    }

    /// Injects the coverage scanner. Absent, every coverage endpoint answers
    /// 503.
    pub fn set_coverage(&self, scanner: Arc<coverage_scanner::Scanner>) {
        self.injected.write().coverage = Some(scanner);
    }

    /// Describes the storage backend the Storage admin page renders.
    pub fn set_storage_backend(&self, backend: StorageBackend, minio: Option<MinIOInfoFn>) {
        let mut injected = self.injected.write();
        injected.storage = backend;
        injected.minio = minio;
    }

    pub(crate) fn uploader(&self) -> Option<Arc<repo::Uploader>> {
        self.injected.read().uploader.clone()
    }

    pub(crate) fn upload_enabled(&self) -> bool {
        self.injected.read().upload_enabled
    }

    pub(crate) fn external_url(&self) -> String {
        self.injected.read().external_url.clone()
    }

    pub(crate) fn repo_manager(&self) -> Option<Arc<repo::Manager>> {
        self.injected.read().repo_manager.clone()
    }

    pub(crate) fn notifier(&self) -> Option<Arc<notify::Notifier>> {
        self.injected.read().notifier.clone()
    }

    pub(crate) fn coverage(&self) -> Option<Arc<coverage_scanner::Scanner>> {
        self.injected.read().coverage.clone()
    }

    pub(crate) fn coverage_scan(&self) -> Option<CoverageScanFn> {
        self.injected.read().coverage_scan.clone()
    }

    /// Records a repository lifecycle event performed through the management
    /// API, attributed to the authenticated principal.
    pub(crate) fn audit(&self, parts: &Parts, repo_name: &str, event: &str, status: i64) {
        let Some(rec) = &self.rec else {
            return;
        };
        let mut username = String::new();
        let mut detail = String::new();
        if let Some(p) = auth_service::from_request_parts(parts) {
            username = p.username.clone();
            // An impersonated session acts as the target user, so the event
            // stays attributed to them; the administrator behind it is recorded
            // alongside so the trail never loses the real operator.
            if !p.impersonator.is_empty() {
                detail = serde_json::json!({"impersonated_by": p.impersonator}).to_string();
            }
        }
        rec.record(audit::Event {
            repo: repo_name.to_string(),
            action: event.to_string(),
            username,
            method: parts.method.to_string(),
            status,
            client_ip: audit::client_ip_parts(parts),
            user_agent: header_str(parts, http::header::USER_AGENT),
            detail_json: detail,
            ..Default::default()
        });
    }
}

fn header_str(parts: &Parts, name: http::HeaderName) -> String {
    parts
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// The router mounted under `/api/v1`. The auth service's middleware is applied
/// by the caller, so a principal (if any) is already in the request extensions.
pub fn routes(h: Arc<Handler>) -> Router {
    let guarded = h.authz.is_some();

    // Public.
    let public = Router::new()
        .route("/login", axum::routing::post(auth::login))
        .route("/logout", axum::routing::post(auth::logout))
        .route("/me", axum::routing::get(auth::me))
        .route("/version", axum::routing::get(version_info))
        .route("/stats/landing", axum::routing::get(landing_stats));

    // Authenticated self-service and reads.
    let mut authenticated = Router::new()
        // Authenticated self-service: personal access tokens.
        .route(
            "/tokens",
            axum::routing::get(auth::list_tokens).post(auth::create_token),
        )
        .route(
            "/tokens/{id}",
            axum::routing::patch(auth::update_token).delete(auth::delete_token),
        )
        // Ending an impersonation is deliberately not admin-gated: the session
        // being ended carries the impersonated user's permissions, which are
        // usually not administrative, and it must still be able to hand control
        // back to the administrator who started it.
        .route(
            "/impersonate/stop",
            axum::routing::post(auth::stop_impersonation),
        )
        // Names only, for token-scope autocomplete.
        .route(
            "/repository-names",
            axum::routing::get(repositories::list_names),
        )
        // Global search (sidebar). The handler narrows every section to what the
        // principal may see, so authenticated is the only gate here.
        .route("/search", axum::routing::get(search::search))
        // Repository listing and read-only detail (config + artifact browse) are
        // available to any authenticated user; the handlers filter to
        // repositories the principal can read. Mutations and audit logs stay
        // admin-only. Mirrors Nexus browse/read vs admin privileges.
        .route("/repositories", axum::routing::get(repositories::list))
        .route("/repositories/{id}", axum::routing::get(repositories::get))
        .route(
            "/repositories/{id}/upstream-health",
            axum::routing::get(repositories::upstream_health),
        )
        .route(
            "/repositories/{id}/artifacts",
            axum::routing::get(repositories::list_artifacts),
        )
        .route(
            "/repositories/{id}/oci-tags",
            axum::routing::get(repositories::list_oci_tags),
        )
        .route(
            "/repositories/{id}/oci-detail",
            axum::routing::get(repositories::get_oci_detail),
        )
        .route(
            "/repositories/{id}/dangling",
            axum::routing::get(repositories::list_dangling),
        )
        .route(
            "/repositories/{id}/artifacts/validate-upload",
            axum::routing::post(repositories::validate_upload),
        )
        .route(
            "/repositories/{id}/artifacts/upload",
            axum::routing::put(repositories::upload_artifact),
        )
        // Security policy is the one part of a repository's config that a
        // non-admin may change: the handler enforces the security action per
        // repository and accepts only the policy fields, so the upstream URL and
        // its credentials remain reachable solely through the admin-only
        // PUT /repositories/{id} below.
        .route(
            "/repositories/{id}/security",
            axum::routing::put(repositories::update_security),
        )
        // Site-wide announcement (Jenkins system-message style), readable by any
        // signed-in user; edits are admin-only.
        .route("/announcement", axum::routing::get(announcement::get))
        // Artifact labels. Reading follows repository read access; the two
        // mutations are gated per artifact inside the handlers (administrator on
        // the repository, or the principal who uploaded that artifact), which is
        // why they sit in the authenticated group rather than the admin one.
        .route(
            "/repositories/{id}/artifacts/labels",
            axum::routing::get(labels::list)
                .post(labels::add)
                .delete(labels::delete),
        )
        // The same two mutations over a selection, gated the same way per
        // artifact, so labelling a thousand artifacts is not a thousand round
        // trips from the console.
        .route(
            "/repositories/{id}/artifacts/labels/bulk",
            axum::routing::post(artifacts_bulk::bulk_labels),
        )
        .route(
            "/repositories/{id}/uploads",
            axum::routing::post(uploads::receive),
        )
        .route(
            "/repositories/{id}/uploads/{uploadID}",
            axum::routing::get(uploads::get).delete(uploads::cancel),
        )
        .route(
            "/repositories/{id}/uploads/{uploadID}/commit",
            axum::routing::post(uploads::commit),
        )
        .route(
            "/repositories/{id}/publications/{publicationID}",
            axum::routing::delete(uploads::delete_publication),
        )
        .route(
            "/repositories/{id}/publications/{publicationID}/yank",
            axum::routing::post(uploads::yank_publication),
        )
        // Forklift coverage. The measurement is what the whole organisation is
        // migrating towards, so every signed-in user can read it; running a scan
        // and changing what is measured stay admin-only.
        .route("/coverage", axum::routing::get(coverage::get))
        .route(
            "/coverage/groups",
            axum::routing::get(coverage::list_groups),
        )
        .route(
            "/coverage/history",
            axum::routing::get(coverage::list_history),
        )
        .route(
            "/coverage/project",
            axum::routing::get(coverage::get_project),
        )
        .route(
            "/coverage/project/last-commit",
            axum::routing::get(coverage::get_project_last_commit),
        );
    if guarded {
        authenticated =
            authenticated.route_layer(axum::middleware::from_fn(auth_service::require_auth));
    }

    // Administrative mutations: administrators only.
    // Administrative reads: administrators plus principals holding the audit
    // action (e.g. a security auditor). Read-only views of the admin surfaces.
    let mut auditor = Router::new()
        .route(
            "/repositories/{id}/audit-logs",
            axum::routing::get(repositories::list_audit_logs),
        )
        .route(
            "/repositories/{id}/permissions",
            axum::routing::get(repositories::permissions),
        )
        .route(
            "/repositories/{id}/tokens",
            axum::routing::get(repositories::tokens),
        )
        .route("/users", axum::routing::get(auth::list_users))
        .route(
            "/users/{id}/tokens",
            axum::routing::get(auth::list_user_tokens),
        )
        .route("/roles", axum::routing::get(auth::list_roles))
        .route(
            "/group-mappings",
            axum::routing::get(auth::list_group_mappings),
        )
        .route(
            // Notification receivers are listed for admins/auditors so the
            // repository settings UI can offer them for selection.
            "/notification/receivers",
            axum::routing::get(notifications::list),
        );
    if guarded {
        auditor = auditor.route_layer(axum::middleware::from_fn(auth_service::require_auditor));
    }

    let mut admin = Router::new()
        .route("/ha", axum::routing::get(ha::get_status))
        .route("/ha/step-down", axum::routing::post(ha::step_down))
        .route("/storage", axum::routing::get(storage::get_stats))
        .route(
            "/repositories/{id}/artifacts/bulk-delete",
            axum::routing::post(artifacts_bulk::bulk_delete),
        )
        .route("/announcement", axum::routing::put(announcement::put))
        .route("/repositories", axum::routing::post(repositories::create))
        .route(
            "/repositories/check-upstream",
            axum::routing::post(repositories::check_upstream),
        )
        .route(
            "/repositories/{id}",
            axum::routing::put(repositories::update).delete(repositories::delete),
        )
        .route(
            "/repositories/{id}/disabled",
            axum::routing::post(repositories::set_disabled),
        )
        .route(
            "/repositories/{id}/artifacts",
            axum::routing::delete(repositories::delete_artifact),
        )
        .route("/users", axum::routing::post(auth::create_user))
        .route(
            "/users/{id}",
            axum::routing::put(auth::update_user).delete(auth::delete_user),
        )
        .route(
            "/users/{id}/impersonate",
            axum::routing::post(auth::impersonate_user),
        )
        .route("/users/{id}/roles", axum::routing::post(auth::assign_role))
        .route(
            "/users/{id}/roles/{roleID}",
            axum::routing::delete(auth::remove_role),
        )
        .route(
            "/users/{id}/tokens",
            axum::routing::post(auth::create_user_token),
        )
        .route(
            "/users/{id}/tokens/{tokenID}",
            axum::routing::patch(auth::update_user_token).delete(auth::delete_user_token),
        )
        .route("/roles", axum::routing::post(auth::create_role))
        .route("/roles/{id}", axum::routing::delete(auth::delete_role))
        .route(
            "/roles/{id}/permissions",
            axum::routing::post(auth::add_permission),
        )
        .route(
            "/roles/{id}/permissions/{permID}",
            axum::routing::delete(auth::delete_permission),
        )
        .route(
            "/group-mappings",
            axum::routing::post(auth::create_group_mapping),
        )
        .route(
            "/group-mappings/{id}",
            axum::routing::delete(auth::delete_group_mapping),
        )
        .route(
            "/notification/receivers",
            axum::routing::post(notifications::create),
        )
        .route(
            "/notification/receivers/{id}",
            axum::routing::put(notifications::update).delete(notifications::delete),
        )
        .route(
            "/notification/receivers/{id}/test",
            axum::routing::post(notifications::test),
        )
        .route(
            "/notification/test",
            axum::routing::post(notifications::test_adhoc),
        )
        .route(
            "/repositories/{id}/notification/sample",
            axum::routing::get(notifications::preview_repo_sample)
                .post(notifications::send_repo_sample),
        )
        // Coverage administration: the scan itself, what counts towards it, and
        // where the report is sent. The pipeline viewer is here too — it reads
        // GitLab CI definitions, which is a narrower surface than repository
        // source but still more than the dashboard needs.
        .route(
            "/coverage/settings",
            axum::routing::get(coverage::get_settings).put(coverage::update_settings),
        )
        .route(
            "/coverage/settings/check-host",
            axum::routing::post(coverage::check_host),
        )
        .route(
            "/coverage/gitlab-check",
            axum::routing::get(coverage::gitlab_check),
        )
        .route("/coverage/scan", axum::routing::post(coverage::start_scan))
        .route(
            "/coverage/project/mute",
            axum::routing::put(coverage::update_project_mute),
        )
        .route(
            "/coverage/project/pipeline",
            axum::routing::get(coverage::get_project_pipeline),
        )
        .route(
            "/coverage/notification/preview",
            axum::routing::get(coverage::preview_alarm),
        )
        .route(
            "/coverage/notification/send",
            axum::routing::post(coverage::send_alarm),
        );
    if guarded {
        admin = admin.route_layer(axum::middleware::from_fn(auth_service::require_admin));
    }

    // Package approvals: administrators plus principals holding the approve
    // action (e.g. a security-engineer role) or the audit action (a security
    // auditor, read-only). Auditors may view this surface but not decide;
    // mutations are enforced per repository inside the handlers via the approve
    // action.
    let mut approver_or_auditor = Router::new()
        .route(
            "/approvals",
            axum::routing::get(approvals::list).post(approvals::create),
        )
        .route("/approvals/count", axum::routing::get(approvals::count))
        .route(
            "/approvals/pending-repos",
            axum::routing::get(approvals::pending_repos),
        )
        .route("/approvals/{id}", axum::routing::get(approvals::get))
        .route(
            "/approvals/approve-all",
            axum::routing::post(approvals::approve_all_pending),
        )
        .route(
            "/approvals/{id}/approve",
            axum::routing::post(approvals::approve),
        )
        .route(
            "/approvals/{id}/reject",
            axum::routing::post(approvals::reject),
        )
        .route(
            "/version-denies",
            axum::routing::get(denies::list).post(denies::create),
        )
        .route(
            "/version-denies/{id}",
            axum::routing::delete(denies::delete),
        );
    if guarded {
        approver_or_auditor = approver_or_auditor.route_layer(axum::middleware::from_fn(
            auth_service::require_approver_or_auditor,
        ));
    }

    public
        .merge(authenticated)
        .merge(approver_or_auditor)
        .merge(auditor)
        .merge(admin)
        .with_state(Arc::clone(&h))
}

/// Reports the build-time version metadata so the web UI can show it in the
/// sidebar. Public: it leaks nothing sensitive and aids support triage.
async fn version_info(State(h): State<Arc<Handler>>) -> Response {
    // `oidc_enabled` drives the login page: the "Sign in with Keycloak" button
    // is hidden when OIDC is not configured (its `/auth/login` route is
    // unregistered).
    let oidc_enabled = h.authz.as_ref().is_some_and(|a| a.oidc_enabled());
    write_json(
        StatusCode::OK,
        serde_json::json!({
            "version": version::VERSION,
            "commit": version::COMMIT,
            "oidc_enabled": oidc_enabled,
        }),
    )
}

/// Reports the coarse instance-wide counts the login page's welcome line shows.
/// Deliberately public and deliberately coarse: two totals, no names or
/// per-repository detail.
async fn landing_stats(State(h): State<Arc<Handler>>) -> Response {
    let repositories = match h.store.count_repositories().await {
        Ok(v) => v,
        Err(err) => return map_error(err),
    };
    let artifacts = match h.store.count_all_artifacts().await {
        Ok(v) => v,
        Err(err) => return map_error(err),
    };
    write_json(
        StatusCode::OK,
        serde_json::json!({"repositories": repositories, "artifacts": artifacts}),
    )
}

/// Accepts the identifier charset shared by every "name" input (repository,
/// token, role, user names): ASCII letters, digits, `-` and `_`, at most 64
/// characters. Descriptions and notes stay free-form.
pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub(crate) const NAME_RULE_MSG: &str =
    "may only contain letters, digits, '-' and '_' (max 64 chars)";

/// The acknowledgement returned by operations whose outcome is the fact that
/// they ran: a step-down was accepted, a test alarm went out. The value is prose
/// for the operator, not a code to branch on.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusMessageDTO {
    pub(crate) status: String,
}

pub(crate) fn write_json<T: Serialize>(status: StatusCode, value: T) -> Response {
    let mut body = serde_json::to_vec(&value).unwrap_or_default();
    // `json.Encoder.Encode` terminates every document with a newline.
    body.push(b'\n');
    (
        status,
        [(
            http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

pub(crate) fn write_error(status: StatusCode, msg: &str) -> Response {
    write_json(status, serde_json::json!({"error": msg}))
}

/// The longest `/pattern/` search a listing accepts.
const MAX_SEARCH_REGEX_LEN: usize = 256;
/// The compiled-program cap for a search regex, a tenth of the regex crate's
/// 10 MiB default.
const SEARCH_REGEX_SIZE_LIMIT: usize = 1 << 20;

/// Compiles a user-supplied, case-insensitive search regex. The regex crate
/// matches in linear time, so there is no catastrophic backtracking to guard
/// against; what is left to bound is the pattern length and the size of the
/// program it compiles to.
pub(crate) fn search_regex(pattern: &str) -> Result<regex::Regex, String> {
    if pattern.len() > MAX_SEARCH_REGEX_LEN {
        return Err(format!("longer than {MAX_SEARCH_REGEX_LEN} bytes"));
    }
    regex::RegexBuilder::new(pattern)
        .case_insensitive(true)
        .size_limit(SEARCH_REGEX_SIZE_LIMIT)
        .dfa_size_limit(SEARCH_REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| e.to_string())
}

/// Translates store errors into HTTP responses.
pub(crate) fn map_error(err: meta::Error) -> Response {
    match err {
        meta::Error::NotFound => write_error(StatusCode::NOT_FOUND, "not found"),
        meta::Error::Conflict => write_error(StatusCode::CONFLICT, "already exists"),
        meta::Error::ManagedArtifact => write_error(
            StatusCode::CONFLICT,
            "managed artifact requires a publication lifecycle operation",
        ),
        other => write_error(StatusCode::INTERNAL_SERVER_ERROR, &other.to_string()),
    }
}

/// The authenticated principal's name, or `""` for an anonymous request.
pub(crate) fn principal_name(parts: &Parts) -> String {
    auth_service::from_request_parts(parts)
        .map(|p| p.username.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_regex_is_case_insensitive_and_bounded() {
        let re = search_regex("^widget-[0-9]+$").expect("plain pattern");
        assert!(re.is_match("Widget-12"));

        let long = "a".repeat(MAX_SEARCH_REGEX_LEN + 1);
        assert!(search_regex(&long).is_err(), "over-long pattern accepted");

        // Short to type, but it compiles far past the size limit.
        assert!(
            search_regex(r"(?:\w{100}){100}").is_err(),
            "oversized program accepted"
        );
        assert!(search_regex("(unclosed").is_err());
    }
}
