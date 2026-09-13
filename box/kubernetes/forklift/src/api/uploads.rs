use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as UrlPath, Request, State};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use http::StatusCode;
use http::request::Parts;
use serde::{Deserialize, Serialize};

use crate::audit;
use crate::auth::{self, Principal};
use crate::meta::{self, Repository};
use crate::repo::{ArtifactUploadResult, UploadProblem};
use crate::repoconfig;

use super::repositories::path_id;
use super::{Handler, map_error, principal_name, write_json};

/// Receives a managed upload: the multipart body is validated and committed by
/// the uploader, which the API only gates and audits.
pub(super) async fn receive(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let uploader = h.uploader();
    let enabled = h.upload_enabled() && uploader.as_ref().is_some_and(|u| u.enabled());
    if !enabled {
        return write_upload_problem(&problem(
            "upload-disabled",
            "Artifact upload disabled",
            404,
            "upload_disabled",
            "UI/API artifact upload is not enabled",
        ));
    }
    let uploader = uploader.expect("checked above");
    let (parts, body) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let repository = match h.store.get_repository(id).await {
        Ok(repository) => repository,
        Err(err) => return map_error(err),
    };
    let Some(principal) = auth::from_request_parts(&parts) else {
        return auth::unauthorized();
    };
    // Hide repositories the caller cannot read; disclose an existing readable
    // repository but deny upload when write is missing.
    if h.authz.is_some() && !principal.can(&repository.name, auth::ACTION_READ) {
        return write_upload_problem(&problem(
            "repository-not-found",
            "Repository not found",
            404,
            "repository_not_found",
            "The repository was not found",
        ));
    }
    if h.authz.is_some() && !principal.can(&repository.name, auth::ACTION_WRITE) {
        return write_upload_problem(&problem(
            "forbidden",
            "Upload forbidden",
            403,
            "forbidden",
            "Write permission is required for this repository",
        ));
    }
    let Ok(config) = repoconfig::parse(&repository.config_json) else {
        return write_upload_problem(&internal_upload_problem());
    };
    if config.ip_acl.enabled && !config.ip_acl.allowed(&audit::client_ip_parts(&parts)) {
        return write_upload_problem(&problem(
            "ip-not-allowed",
            "Source IP denied",
            403,
            "ip_not_allowed",
            "The source IP is not allowed for this repository",
        ));
    }
    if !h.valid_upload_csrf(&parts) {
        return write_upload_problem(&problem(
            "csrf-invalid",
            "Invalid CSRF token",
            403,
            "csrf_invalid",
            "Refresh the session and retry the upload",
        ));
    }

    let body = Body::new(http_body_util::Limited::new(
        body,
        uploader.max_request_bytes().max(0) as usize,
    ));
    let allow_maven_replace =
        h.authz.is_none() || principal.can(&repository.name, auth::ACTION_DELETE);
    let outcome = uploader
        .receive_authorized(
            &repository,
            &principal.username,
            &principal.source,
            &super::header_str(&parts, http::HeaderName::from_static("idempotency-key")),
            &super::header_str(&parts, http::header::CONTENT_TYPE),
            body,
            allow_maven_replace,
        )
        .await;
    if let Some(problem) = outcome.problem {
        h.audit_upload(
            &parts,
            &repository.name,
            meta::EVENT_UPLOAD_REJECT,
            &problem.upload_id,
            "",
            problem.status,
            serde_json::json!({"code": problem.code, "format": repository.format}),
        );
        let mut response = write_upload_problem(&problem);
        if problem.status == 429 {
            response.headers_mut().insert(
                http::header::RETRY_AFTER,
                http::HeaderValue::from_static("5"),
            );
        }
        return response;
    }
    let result = outcome.result;
    if outcome.replay {
        let mut response = write_json(StatusCode::OK, &result);
        response.headers_mut().insert(
            http::HeaderName::from_static("idempotency-replayed"),
            http::HeaderValue::from_static("true"),
        );
        return response;
    }
    for artifact in &result.created {
        if artifact.role == "checksum" {
            continue;
        }
        h.audit_upload(
            &parts,
            &repository.name,
            meta::EVENT_UPLOAD,
            &result.upload_id,
            &artifact.path,
            201,
            serde_json::json!({
                "coordinate": result.coordinate,
                "digest_prefix": digest_prefix(&artifact.sha256),
                "format": result.format,
                "mutation": "created",
                "role": artifact.role,
            }),
        );
    }
    if !result.derived.is_empty() {
        h.audit_upload(
            &parts,
            &repository.name,
            meta::EVENT_UPLOAD_METADATA,
            &result.upload_id,
            "",
            201,
            serde_json::json!({
                "coordinate": result.coordinate,
                "format": result.format,
                "paths": result.derived.len(),
            }),
        );
    }
    write_json(StatusCode::CREATED, &result)
}

/// A resumable upload's progress. Exactly one of `result` and `problem` is
/// present once the session reaches a terminal state — the committed artifacts,
/// or the conflict that stopped it — and neither while it is still receiving,
/// which is why both are omitted when absent.
#[derive(Debug, Clone, Serialize)]
struct UploadSessionDTO {
    upload_id: String,
    state: String,
    expires_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<ArtifactUploadResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    problem: Option<UploadProblem>,
}

pub(super) async fn get(
    State(h): State<Arc<Handler>>,
    UrlPath((id, upload_id)): UrlPath<(String, String)>,
    request: Request,
) -> Response {
    if !h.upload_enabled() || h.uploader().is_none() {
        return not_found();
    }
    let (parts, _) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let repository = match h.store.get_repository(id).await {
        Ok(repository) => repository,
        Err(err) => return map_error(err),
    };
    let principal = auth::from_request_parts(&parts);
    let readable = principal
        .as_ref()
        .is_some_and(|p| h.authz.is_none() || p.can(&repository.name, auth::ACTION_READ));
    if !readable {
        return not_found();
    }
    let principal = principal.expect("checked above");
    let Ok(request_row) = h.store.get_upload_request_by_id(&upload_id).await else {
        return not_found();
    };
    if request_row.repo_id != id
        || request_row.principal_name != principal.username
        || request_row.principal_source != principal.source
    {
        return not_found();
    }
    let mut response = UploadSessionDTO {
        upload_id: request_row.upload_id.clone(),
        state: request_row.state.clone(),
        expires_at: request_row.expires_at,
        result: None,
        problem: None,
    };
    if request_row.state == meta::UPLOAD_COMMITTED {
        response.result = serde_json::from_str(&request_row.result_json).ok();
    } else if request_row.state == meta::UPLOAD_CONFLICT {
        #[derive(Deserialize)]
        struct ConflictPlan {
            problem: UploadProblem,
        }
        response.problem = serde_json::from_str::<ConflictPlan>(&request_row.plan_json)
            .ok()
            .map(|plan| plan.problem);
    }
    write_json(StatusCode::OK, response)
}

pub(super) async fn commit(
    State(h): State<Arc<Handler>>,
    UrlPath((id, upload_id)): UrlPath<(String, String)>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let (repository, principal) = match h.authorize_upload_mutation(&parts, &id, true).await {
        Ok(pair) => pair,
        Err(response) => return *response,
    };
    let uploader = h.uploader().expect("checked by the authorization step");
    let result = match uploader
        .commit_maven_conflict(
            &repository,
            &upload_id,
            &principal.username,
            &principal.source,
        )
        .await
    {
        Ok(result) => result,
        Err(problem) => return write_upload_problem(&problem),
    };
    for artifact in &result.replaced {
        h.audit_upload(
            &parts,
            &repository.name,
            meta::EVENT_UPLOAD,
            &result.upload_id,
            &artifact.path,
            200,
            serde_json::json!({
                "coordinate": result.coordinate,
                "format": result.format,
                "mutation": "replaced",
                "role": artifact.role,
            }),
        );
    }
    write_json(StatusCode::OK, &result)
}

pub(super) async fn cancel(
    State(h): State<Arc<Handler>>,
    UrlPath((id, upload_id)): UrlPath<(String, String)>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let (repository, principal) = match h.authorize_upload_mutation(&parts, &id, false).await {
        Ok(pair) => pair,
        Err(response) => return *response,
    };
    let uploader = h.uploader().expect("checked by the authorization step");
    if let Some(problem) = uploader
        .cancel_maven_conflict(
            &repository,
            &upload_id,
            &principal.username,
            &principal.source,
        )
        .await
    {
        return write_upload_problem(&problem);
    }
    StatusCode::NO_CONTENT.into_response()
}

pub(super) async fn delete_publication(
    State(h): State<Arc<Handler>>,
    UrlPath((id, publication_id)): UrlPath<(String, String)>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let (repository, principal) = match h
        .authorize_publication_mutation(&parts, &id, auth::ACTION_DELETE)
        .await
    {
        Ok(pair) => pair,
        Err(response) => return *response,
    };
    let uploader = h.uploader().expect("checked by the authorization step");
    let result = match uploader
        .delete_publication(&repository, &publication_id, &principal.username)
        .await
    {
        Ok(result) => result,
        Err(problem) => return write_upload_problem(&problem),
    };
    h.audit_upload(
        &parts,
        &repository.name,
        meta::EVENT_DELETE,
        "",
        "",
        200,
        serde_json::json!({
            "coordinate": result.coordinate,
            "paths": result.deleted.len(),
            "publication_id": result.publication_id,
        }),
    );
    write_json(StatusCode::OK, &result)
}

/// Flips a Cargo publication's yanked flag. The optional field distinguishes
/// "set it to false" from "the caller omitted the field", which is rejected
/// rather than guessed at.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct YankPublicationReq {
    #[serde(default)]
    yanked: Option<bool>,
}

pub(super) async fn yank_publication(
    State(h): State<Arc<Handler>>,
    UrlPath((id, publication_id)): UrlPath<(String, String)>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let (repository, principal) = match h
        .authorize_publication_mutation(&parts, &id, auth::ACTION_WRITE)
        .await
    {
        Ok(pair) => pair,
        Err(response) => return *response,
    };
    let invalid = || {
        write_upload_problem(&problem(
            "request-invalid",
            "Invalid request",
            400,
            "request_invalid",
            "A boolean yanked field is required",
        ))
    };
    let Ok(bytes) = axum::body::to_bytes(body, 4 << 10).await else {
        return invalid();
    };
    let yanked = match serde_json::from_slice::<YankPublicationReq>(&bytes) {
        Ok(request) => match request.yanked {
            Some(yanked) => yanked,
            None => return invalid(),
        },
        Err(_) => return invalid(),
    };
    let uploader = h.uploader().expect("checked by the authorization step");
    let result = match uploader
        .set_cargo_yanked(&repository, &publication_id, &principal.username, yanked)
        .await
    {
        Ok(result) => result,
        Err(problem) => return write_upload_problem(&problem),
    };
    h.audit_upload(
        &parts,
        &repository.name,
        meta::EVENT_UPLOAD_METADATA,
        "",
        "",
        200,
        serde_json::json!({
            "coordinate": result.coordinate,
            "publication_id": result.publication_id,
            "yanked": yanked,
        }),
    );
    write_json(StatusCode::OK, &result)
}

impl Handler {
    async fn authorize_upload_mutation(
        &self,
        parts: &Parts,
        id: &str,
        require_delete: bool,
    ) -> Result<(Repository, Arc<Principal>), Box<Response>> {
        if !self.upload_enabled() || self.uploader().is_none() {
            return Err(Box::new(not_found()));
        }
        let id = path_id(id)?;
        let repository = self
            .store
            .get_repository(id)
            .await
            .map_err(|err| Box::new(map_error(err)))?;
        let Some(principal) = auth::from_request_parts(parts) else {
            return Err(Box::new(auth::unauthorized()));
        };
        if self.authz.is_some()
            && (!principal.can(&repository.name, auth::ACTION_WRITE)
                || (require_delete && !principal.can(&repository.name, auth::ACTION_DELETE)))
        {
            return Err(Box::new(write_upload_problem(&problem(
                "forbidden",
                "Upload forbidden",
                403,
                "forbidden",
                "This lifecycle operation requires repository write permission and replacement also requires delete permission",
            ))));
        }
        let denied = match repoconfig::parse(&repository.config_json) {
            Err(_) => true,
            Ok(config) => {
                config.ip_acl.enabled && !config.ip_acl.allowed(&audit::client_ip_parts(parts))
            }
        };
        if denied {
            return Err(Box::new(write_upload_problem(&problem(
                "forbidden",
                "Upload forbidden",
                403,
                "forbidden",
                "The repository policy denied this operation",
            ))));
        }
        if !self.valid_upload_csrf(parts) {
            return Err(Box::new(write_upload_problem(&problem(
                "csrf-invalid",
                "Invalid CSRF token",
                403,
                "csrf_invalid",
                "Refresh the session and retry",
            ))));
        }
        Ok((repository, principal))
    }

    async fn authorize_publication_mutation(
        &self,
        parts: &Parts,
        id: &str,
        action: &str,
    ) -> Result<(Repository, Arc<Principal>), Box<Response>> {
        if !self.upload_enabled() || self.uploader().is_none() {
            return Err(Box::new(not_found()));
        }
        let id = path_id(id)?;
        let repository = self
            .store
            .get_repository(id)
            .await
            .map_err(|err| Box::new(map_error(err)))?;
        let Some(principal) = auth::from_request_parts(parts) else {
            return Err(Box::new(auth::unauthorized()));
        };
        if self.authz.is_some() && !principal.can(&repository.name, action) {
            return Err(Box::new(write_upload_problem(&problem(
                "forbidden",
                "Lifecycle operation forbidden",
                403,
                "forbidden",
                "The required repository permission is missing",
            ))));
        }
        let denied = match repoconfig::parse(&repository.config_json) {
            Err(_) => true,
            Ok(config) => {
                (config.ip_acl.enabled && !config.ip_acl.allowed(&audit::client_ip_parts(parts)))
                    || !self.valid_upload_csrf(parts)
            }
        };
        if denied {
            return Err(Box::new(write_upload_problem(&problem(
                "forbidden",
                "Lifecycle operation forbidden",
                403,
                "forbidden",
                "The request did not satisfy repository or CSRF policy",
            ))));
        }
        Ok((repository, principal))
    }

    fn valid_upload_csrf(&self, parts: &Parts) -> bool {
        let Some(authz) = &self.authz else {
            return true;
        };
        if !super::header_str(parts, http::header::AUTHORIZATION).is_empty() {
            return true;
        }
        if !authz.validate_csrf(parts)
            || super::header_str(parts, http::HeaderName::from_static("sec-fetch-site"))
                .eq_ignore_ascii_case("cross-site")
        {
            return false;
        }
        let origin = super::header_str(parts, http::header::ORIGIN);
        let origin = origin.trim();
        if origin.is_empty() {
            return true;
        }
        let mut want = self.external_url().trim_end_matches('/').to_string();
        if want.is_empty() {
            let forwarded =
                super::header_str(parts, http::HeaderName::from_static("x-forwarded-proto"));
            let forwarded = forwarded.split(',').next().unwrap_or("").trim().to_string();
            let scheme = if forwarded == "http" || forwarded == "https" {
                forwarded
            } else if parts.uri.scheme_str() == Some("https") {
                "https".to_string()
            } else {
                "http".to_string()
            };
            want = format!(
                "{scheme}://{}",
                super::header_str(parts, http::header::HOST)
            );
        }
        let (Ok(origin_url), Ok(want_url)) = (url::Url::parse(origin), url::Url::parse(&want))
        else {
            return false;
        };
        if origin_url.scheme().is_empty() || origin_url.host_str().unwrap_or("").is_empty() {
            return false;
        }
        let left = format!("{}://{}", origin_url.scheme(), authority(&origin_url));
        let right = format!("{}://{}", want_url.scheme(), authority(&want_url));
        left.len() == right.len()
            && left
                .bytes()
                .zip(right.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    }

    #[allow(clippy::too_many_arguments)] // Domain operation parameters.
    fn audit_upload(
        &self,
        parts: &Parts,
        repository: &str,
        event: &str,
        request_id: &str,
        path: &str,
        status: i64,
        detail: serde_json::Value,
    ) {
        let Some(rec) = &self.rec else {
            return;
        };
        rec.record(audit::Event {
            repo: repository.to_string(),
            action: event.to_string(),
            path: path.to_string(),
            username: principal_name(parts),
            method: parts.method.to_string(),
            status,
            client_ip: audit::client_ip_parts(parts),
            user_agent: super::header_str(parts, http::header::USER_AGENT),
            request_id: request_id.to_string(),
            detail_json: detail.to_string(),
        });
    }
}

fn authority(u: &url::Url) -> String {
    match u.port() {
        Some(port) => format!("{}:{port}", u.host_str().unwrap_or("")),
        None => u.host_str().unwrap_or("").to_string(),
    }
}

fn problem(kind: &str, title: &str, status: i64, code: &str, detail: &str) -> UploadProblem {
    UploadProblem {
        type_: format!("https://forklift.dev/problems/{kind}"),
        title: title.to_string(),
        status,
        code: code.to_string(),
        detail: detail.to_string(),
        ..Default::default()
    }
}

fn internal_upload_problem() -> UploadProblem {
    problem(
        "internal-error",
        "Upload unavailable",
        500,
        "internal_error",
        "The upload could not be processed",
    )
}

fn not_found() -> Response {
    crate::server::http_error(StatusCode::NOT_FOUND, "404 page not found")
}

fn write_upload_problem(problem: &UploadProblem) -> Response {
    let status =
        StatusCode::from_u16(problem.status as u16).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut body = serde_json::to_vec(problem).unwrap_or_default();
    body.push(b'\n');
    (
        status,
        [(
            http::header::CONTENT_TYPE,
            "application/problem+json; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

fn digest_prefix(digest: &str) -> String {
    digest.chars().take(12).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use base64::Engine as _;
    use http::{Method, Request, StatusCode};
    use serde_json::Value;
    use tempfile::TempDir;

    use crate::auth;
    use crate::config::UploadConfig;
    use crate::meta::{self, Repository, Store};
    use crate::repo::{Engine, Uploader};
    use crate::storage::FsStore;

    use crate::api::Handler;
    use crate::testing::api::{ADMIN_PASS, ADMIN_USER, TestResponse, send_on};

    pub(crate) struct UploadApiHarness {
        pub(crate) app: Router,
        authz: Arc<auth::Service>,
        pub(crate) store: Arc<Store>,
        pub(crate) repository: Repository,
        _dir: TempDir,
    }

    pub(crate) async fn upload_api_harness(enabled: bool) -> UploadApiHarness {
        // `Engine::new` builds reqwest clients, which need a crypto provider.
        crate::server::install_crypto_provider();
        auth::set_test_hash_cost();
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Arc::new(
            Store::open(dir.path().join("api-upload.db"))
                .await
                .expect("open store"),
        );
        let authz = auth::Service::new(
            Arc::clone(&store),
            auth::Options {
                session_secret: b"test-secret-test-secret-test-secret".to_vec(),
                ..Default::default()
            },
        );
        authz
            .bootstrap_admin(ADMIN_USER, ADMIN_PASS)
            .await
            .expect("bootstrap admin");
        let repository = store
            .create_repository(Repository {
                name: "maven-local".to_string(),
                format: meta::FORMAT_MAVEN.to_string(),
                r#type: meta::TYPE_HOSTED.to_string(),
                ..Default::default()
            })
            .await
            .expect("create repository");
        let blobs = Arc::new(FsStore::new(dir.path()).expect("blob store"));
        let engine = Engine::new(Arc::clone(&store), blobs, &prometheus::Registry::new());
        let upload_config = UploadConfig {
            enabled,
            max_duration: std::time::Duration::from_secs(30 * 60),
            max_concurrent: 4,
            max_concurrent_user: 2,
            max_assets: 16,
            max_manifest_bytes: 64 << 10,
            max_field_bytes: 1 << 20,
            max_file_bytes: 256 << 20,
            max_batch_bytes: 512 << 20,
            go_max_zip_bytes: 500 << 20,
            archive_max_entries: 100_000,
            archive_max_meta_bytes: 16 << 20,
            idempotency_ttl: std::time::Duration::from_secs(24 * 60 * 60),
        };
        let handler = Handler::new(Arc::clone(&store), Some(Arc::clone(&authz)), None);
        handler.set_upload_enabled(enabled);
        handler.set_uploader(Uploader::new(engine, upload_config), "");
        let app = crate::api::routes(Arc::clone(&handler)).layer(
            axum::middleware::from_fn_with_state(Arc::clone(&authz), auth::middleware),
        );
        UploadApiHarness {
            app,
            authz,
            store,
            repository,
            _dir: dir,
        }
    }

    fn api_maven_body() -> (String, Vec<u8>) {
        let boundary = "forkliftapitestboundary";
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"manifest\"\r\n\r\n");
        body.extend_from_slice(
        br#"{"schema_version":1,"format":"maven","overwrite":false,"assets":[{"part":"asset0","extension":"jar"}],"maven":{"group_id":"com.acme","artifact_id":"widget","version":"1.0.0","generate_pom":true,"packaging":"jar"}}"#,
    );
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"asset0\"; filename=\"widget.jar\"\r\nContent-Type: application/octet-stream\r\n\r\n",
    );
        body.extend_from_slice(b"jar");
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        (format!("multipart/form-data; boundary={boundary}"), body)
    }

    /// Posts the fixed Maven body with admin Basic auth and an idempotency key.
    async fn post_upload(app: &Router, repo_id: i64, key: &str) -> TestResponse {
        let (content_type, body) = api_maven_body();
        let credential = base64::engine::general_purpose::STANDARD
            .encode(format!("{ADMIN_USER}:{ADMIN_PASS}").as_bytes());
        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("/repositories/{repo_id}/uploads"))
            .header(http::header::AUTHORIZATION, format!("Basic {credential}"))
            .header(http::header::CONTENT_TYPE, content_type)
            .header(http::header::CONTENT_LENGTH, body.len())
            .header("Idempotency-Key", key)
            .body(Body::from(body))
            .expect("build request");
        send_on(app, request).await
    }

    async fn admin_plain(app: &Router, method: Method, uri: &str) -> TestResponse {
        let credential = base64::engine::general_purpose::STANDARD
            .encode(format!("{ADMIN_USER}:{ADMIN_PASS}").as_bytes());
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(http::header::AUTHORIZATION, format!("Basic {credential}"))
            .body(Body::empty())
            .expect("build request");
        send_on(app, request).await
    }

    /// Sends a JSON body with admin Basic auth.
    async fn admin_json(
        app: &Router,
        method: Method,
        uri: &str,
        body: &'static str,
    ) -> TestResponse {
        let credential = base64::engine::general_purpose::STANDARD
            .encode(format!("{ADMIN_USER}:{ADMIN_PASS}").as_bytes());
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(http::header::AUTHORIZATION, format!("Basic {credential}"))
            .header(http::header::CONTENT_LENGTH, body.len())
            .body(Body::from(body))
            .expect("build request");
        send_on(app, request).await
    }

    #[tokio::test]
    async fn upload_route_feature_gate_and_maven_commit() {
        let disabled = upload_api_harness(false).await;
        let resp = post_upload(
            &disabled.app,
            disabled.repository.id,
            "11111111-1111-4111-8111-111111111111",
        )
        .await;
        assert!(
            resp.status == StatusCode::NOT_FOUND
                && resp.header("content-type") == "application/problem+json; charset=utf-8",
            "disabled upload = {} {}",
            resp.status,
            resp.text()
        );

        let enabled = upload_api_harness(true).await;
        let resp = post_upload(
            &enabled.app,
            enabled.repository.id,
            "22222222-2222-4222-8222-222222222222",
        )
        .await;
        assert_eq!(
            resp.status,
            StatusCode::CREATED,
            "enabled upload = {} {}",
            resp.status,
            resp.text()
        );
        let result = resp.json();
        assert_eq!(
            result["coordinate"], "com.acme:widget:1.0.0",
            "result = {result}"
        );

        let resp = admin_plain(&enabled.app, Method::GET, "/repositories").await;
        let repositories = resp.json();
        let repositories = repositories.as_array().expect("repositories");
        assert_eq!(repositories.len(), 1, "repositories = {repositories:?}");
        assert!(
            repositories[0]["capabilities"]["upload"] == true
                && repositories[0]["publish_methods"][0] == "mvn",
            "upload capabilities = {}",
            repositories[0]
        );
    }

    #[tokio::test]
    async fn upload_route_requires_session_csrf() {
        let h = upload_api_harness(true).await;
        let cookie_value = h
            .authz
            .issue_session(ADMIN_USER, meta::SOURCE_LOCAL, &[])
            .expect("issue session");

        let send_with_session = async |key: &str, csrf: Option<&str>| -> TestResponse {
            let (content_type, body) = api_maven_body();
            let mut request = Request::builder()
                .method(Method::POST)
                .uri(format!("/repositories/{}/uploads", h.repository.id))
                .header(
                    http::header::COOKIE,
                    format!("forklift_session={cookie_value}"),
                )
                .header(http::header::CONTENT_TYPE, content_type)
                .header(http::header::CONTENT_LENGTH, body.len())
                .header("Idempotency-Key", key);
            if let Some(csrf) = csrf {
                request = request.header("X-CSRF-Token", csrf);
            }
            send_on(
                &h.app,
                request.body(Body::from(body)).expect("build request"),
            )
            .await
        };

        let resp = send_with_session("33333333-3333-4333-8333-333333333333", None).await;
        assert_eq!(
            resp.status,
            StatusCode::FORBIDDEN,
            "missing CSRF = {} {}",
            resp.status,
            resp.text()
        );
        let problem = resp.json();
        assert_eq!(problem["code"], "csrf_invalid", "problem = {problem}");

        let parts = Request::builder()
            .uri("/")
            .header(
                http::header::COOKIE,
                format!("forklift_session={cookie_value}"),
            )
            .body(Body::empty())
            .expect("build request")
            .into_parts()
            .0;
        let csrf_token = h
            .authz
            .csrf_token(&parts)
            .expect("session CSRF token unavailable");

        let resp = send_with_session(
            "44444444-4444-4444-8444-444444444444",
            Some(csrf_token.as_str()),
        )
        .await;
        assert_eq!(
            resp.status,
            StatusCode::CREATED,
            "valid session upload = {} {}",
            resp.status,
            resp.text()
        );
    }

    /// Publishes one Maven artifact through the enabled route and returns the
    /// harness plus the committed upload id.
    async fn upload_committed_maven(key: &str) -> (UploadApiHarness, String) {
        let h = upload_api_harness(true).await;
        let resp = post_upload(&h.app, h.repository.id, key).await;
        assert_eq!(
            resp.status,
            StatusCode::CREATED,
            "seed upload = {} {}",
            resp.status,
            resp.text()
        );
        let upload_id = resp.json()["upload_id"]
            .as_str()
            .expect("decode result")
            .to_string();
        (h, upload_id)
    }

    /// Returns the id of the single publication in a fresh store.
    async fn first_publication_id(store: &Store, repo_id: i64) -> String {
        let publications = store
            .list_artifact_publications(repo_id)
            .await
            .expect("list publications");
        assert!(!publications.is_empty(), "list publications: none");
        publications[0].id.clone()
    }

    #[tokio::test]
    async fn get_upload_returns_committed_state() {
        let (h, upload_id) = upload_committed_maven("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").await;

        let resp = admin_plain(
            &h.app,
            Method::GET,
            &format!("/repositories/{}/uploads/{upload_id}", h.repository.id),
        )
        .await;
        assert_eq!(
            resp.status,
            StatusCode::OK,
            "get_upload = {} {}",
            resp.status,
            resp.text()
        );
        let payload: Value = resp.json();
        assert_eq!(
            payload["state"],
            meta::UPLOAD_COMMITTED,
            "state = {}",
            payload["state"]
        );
        assert!(
            payload.get("result").is_some(),
            "committed upload missing result: {payload}"
        );

        // An unknown upload id is indistinguishable from a foreign one: 404.
        let resp = admin_plain(
            &h.app,
            Method::GET,
            &format!("/repositories/{}/uploads/does-not-exist", h.repository.id),
        )
        .await;
        assert_eq!(
            resp.status,
            StatusCode::NOT_FOUND,
            "missing upload = {}",
            resp.status
        );
    }

    #[tokio::test]
    async fn delete_publication_removes_files() {
        let (h, _) = upload_committed_maven("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").await;
        let pub_id = first_publication_id(&h.store, h.repository.id).await;

        let resp = admin_plain(
            &h.app,
            Method::DELETE,
            &format!("/repositories/{}/publications/{pub_id}", h.repository.id),
        )
        .await;
        assert_eq!(
            resp.status,
            StatusCode::OK,
            "delete_publication = {} {}",
            resp.status,
            resp.text()
        );
        let result = resp.json();
        assert_eq!(
            result["coordinate"], "com.acme:widget:1.0.0",
            "delete result = {result}"
        );
    }

    #[tokio::test]
    async fn cancel_unknown_upload_returns_problem() {
        let h = upload_api_harness(true).await;
        let resp = admin_plain(
            &h.app,
            Method::DELETE,
            &format!("/repositories/{}/uploads/no-such-upload", h.repository.id),
        )
        .await;
        assert_ne!(
            resp.status,
            StatusCode::NO_CONTENT,
            "expected a problem for unknown upload, got 204"
        );
        assert_eq!(
            resp.header("content-type"),
            "application/problem+json; charset=utf-8",
            "content-type body={}",
            resp.text()
        );
    }

    #[tokio::test]
    async fn yank_rejects_invalid_body_and_non_cargo() {
        let (h, _) = upload_committed_maven("cccccccc-cccc-4ccc-8ccc-cccccccccccc").await;
        let pub_id = first_publication_id(&h.store, h.repository.id).await;

        // Missing yanked field -> 400 request_invalid.
        let resp = admin_json(
            &h.app,
            Method::POST,
            &format!(
                "/repositories/{}/publications/{pub_id}/yank",
                h.repository.id
            ),
            "{}",
        )
        .await;
        assert_eq!(
            resp.status,
            StatusCode::BAD_REQUEST,
            "invalid yank body = {} {}",
            resp.status,
            resp.text()
        );

        // Well-formed request against a Maven publication -> a problem (yank is
        // Cargo-only).
        let resp = admin_json(
            &h.app,
            Method::POST,
            &format!(
                "/repositories/{}/publications/{pub_id}/yank",
                h.repository.id
            ),
            r#"{"yanked":true}"#,
        )
        .await;
        assert!(
            resp.status.as_u16() >= 400,
            "yank on maven expected a client/server error, got {} {}",
            resp.status,
            resp.text()
        );
    }

    mod uploads_authz {
        use crate::testing::api::mk_upload_user;
        use std::net::SocketAddr;

        use axum::Router;
        use axum::body::Body;
        use axum::extract::ConnectInfo;
        use base64::Engine as _;
        use http::{Method, Request, StatusCode};
        use serde_json::Value;

        use super::upload_api_harness;
        use crate::testing::api::{ADMIN_PASS, ADMIN_USER, send_on};

        /// Sends one upload-lifecycle request and returns the status with the problem
        /// document, which is the shape every refusal on these routes takes.
        ///
        async fn lifecycle_request(
            app: &Router,
            method: Method,
            path: &str,
            user: &str,
            pass: &str,
        ) -> (StatusCode, Value) {
            let credential = base64::engine::general_purpose::STANDARD
                .encode(format!("{user}:{pass}").as_bytes());
            let mut request = Request::builder()
                .method(method)
                .uri(path)
                .header(http::header::AUTHORIZATION, format!("Basic {credential}"))
                .body(Body::empty())
                .expect("build request");
            request
                .extensions_mut()
                .insert(ConnectInfo(SocketAddr::from(([192, 0, 2, 1], 1234))));
            let resp = send_on(app, request).await;
            let problem = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
            (resp.status, problem)
        }

        /// The lifecycle routes (commit, cancel) share one authorization gate, and the
        /// asymmetry in it is the point: cancelling a conflicted upload needs write,
        /// while committing it replaces an already-published file and so needs delete as
        /// well. A principal with write alone must not be able to overwrite a release.
        #[tokio::test]
        async fn upload_lifecycle_authorization() {
            let h = upload_api_harness(true).await;
            mk_upload_user(&h.store, "writer", "read,write").await;
            mk_upload_user(&h.store, "replacer", "read,write,delete").await;
            mk_upload_user(&h.store, "reader", "read").await;

            let commit = format!(
                "/repositories/{}/uploads/missing-upload/commit",
                h.repository.id
            );
            let cancel = format!("/repositories/{}/uploads/missing-upload", h.repository.id);

            // A reader is refused on both, with the problem document naming the missing
            // permission rather than a bare 403.
            for (method, path) in [
                (Method::POST, commit.as_str()),
                (Method::DELETE, cancel.as_str()),
            ] {
                let (code, problem) =
                    lifecycle_request(&h.app, method.clone(), path, "reader", "pw123456").await;
                assert!(
                    code == StatusCode::FORBIDDEN && problem["code"] == "forbidden",
                    "{method} {path} as reader = {code} {problem}, want 403 forbidden"
                );
            }

            // Write alone cancels but does not commit: committing is a replacement.
            let (code, problem) =
                lifecycle_request(&h.app, Method::POST, &commit, "writer", "pw123456").await;
            assert!(
                code == StatusCode::FORBIDDEN && problem["code"] == "forbidden",
                "commit with write only = {code} {problem}, want 403 forbidden"
            );
            // Past the gate, an unknown upload id is a problem about the upload, not the
            // permission, which is how we know the gate let it through.
            let (code, problem) =
                lifecycle_request(&h.app, Method::DELETE, &cancel, "writer", "pw123456").await;
            assert!(
                code != StatusCode::FORBIDDEN && problem["code"] != "forbidden",
                "cancel with write = {code} {problem}, want the gate to pass"
            );
            let (code, problem) =
                lifecycle_request(&h.app, Method::POST, &commit, "replacer", "pw123456").await;
            assert!(
                code != StatusCode::FORBIDDEN && problem["code"] != "forbidden",
                "commit with write and delete = {code} {problem}, want the gate to pass"
            );

            // An unknown repository is a 404 before any permission is considered.
            let (code, _) = lifecycle_request(
                &h.app,
                Method::POST,
                "/repositories/4242/uploads/x/commit",
                ADMIN_USER,
                ADMIN_PASS,
            )
            .await;
            assert_eq!(
                code,
                StatusCode::NOT_FOUND,
                "unknown repository = {code}, want 404"
            );
        }

        /// With the upload feature off, the lifecycle routes must not merely refuse:
        /// they answer 404, so a disabled instance looks like one where the surface does
        /// not exist rather than one where the caller lacks a permission.
        #[tokio::test]
        async fn upload_lifecycle_feature_gate() {
            let h = upload_api_harness(false).await;
            let id = h.repository.id;
            for (method, path) in [
                (Method::POST, format!("/repositories/{id}/uploads/x/commit")),
                (Method::DELETE, format!("/repositories/{id}/uploads/x")),
                (Method::DELETE, format!("/repositories/{id}/publications/x")),
                (
                    Method::POST,
                    format!("/repositories/{id}/publications/x/yank"),
                ),
            ] {
                let (code, _) =
                    lifecycle_request(&h.app, method.clone(), &path, ADMIN_USER, ADMIN_PASS).await;
                assert_eq!(
                    code,
                    StatusCode::NOT_FOUND,
                    "{method} {path} with upload disabled = {code}, want 404"
                );
            }
        }

        /// The publication routes take the action they actually perform: deleting needs
        /// delete, yanking needs write. Both are refused for a principal holding only
        /// the other one, so a yank cannot be used as a back door to removal.
        #[tokio::test]
        async fn publication_lifecycle_authorization() {
            let h = upload_api_harness(true).await;
            mk_upload_user(&h.store, "writer", "read,write").await;
            mk_upload_user(&h.store, "deleter", "read,delete").await;

            let delete_path = format!(
                "/repositories/{}/publications/missing-publication",
                h.repository.id
            );
            let yank_path = format!("{delete_path}/yank");

            let (code, problem) =
                lifecycle_request(&h.app, Method::DELETE, &delete_path, "writer", "pw123456").await;
            assert!(
                code == StatusCode::FORBIDDEN && problem["code"] == "forbidden",
                "delete publication with write only = {code} {problem}, want 403 forbidden"
            );
            let (code, problem) =
                lifecycle_request(&h.app, Method::POST, &yank_path, "deleter", "pw123456").await;
            assert!(
                code == StatusCode::FORBIDDEN && problem["code"] == "forbidden",
                "yank with delete only = {code} {problem}, want 403 forbidden"
            );
            // Holding the right action reaches the handler, which then answers about the
            // unknown publication instead of the permission.
            let (code, problem) =
                lifecycle_request(&h.app, Method::DELETE, &delete_path, "deleter", "pw123456")
                    .await;
            assert!(
                code != StatusCode::FORBIDDEN && problem["code"] != "forbidden",
                "delete publication with delete = {code} {problem}, want the gate to pass"
            );
        }

        /// A repository whose source-IP ACL excludes the caller refuses the mutation
        /// even when the permission is there: the ACL is a property of the repository,
        /// not of the principal, so it has to be enforced on this path too.
        #[tokio::test]
        async fn upload_lifecycle_honours_ip_acl() {
            let h = upload_api_harness(true).await;
            // Requests carry 192.0.2.1, so an ACL allowing only a different range denies
            // them.
            h.store
                .update_repository_config(
                    h.repository.id,
                    "",
                    r#"{"ip_acl":{"enabled":true,"allow":["10.0.0.0/8"]}}"#,
                )
                .await
                .expect("set ip acl");
            let (code, problem) = lifecycle_request(
                &h.app,
                Method::DELETE,
                &format!("/repositories/{}/uploads/x", h.repository.id),
                ADMIN_USER,
                ADMIN_PASS,
            )
            .await;
            assert!(
                code == StatusCode::FORBIDDEN && problem["code"] == "forbidden",
                "denied source IP = {code} {problem}, want 403 forbidden"
            );
        }
    }
}
