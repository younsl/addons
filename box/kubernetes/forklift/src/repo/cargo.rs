//! Serves the Cargo sparse-registry protocol. Paths under `/cargo/{repo}/`:
//!
//! ```text
//! config.json                           registry config (synthesised)
//! <a>/<b>/<crate>                       sparse index entries  (metadata)
//! api/v1/crates/<crate>/<ver>/download  the .crate tarball    (artifact)
//! api/v1/crates/new                     cargo publish         (hosted, PUT)
//! api/v1/crates/<crate>/<ver>/yank      cargo yank            (hosted, DELETE)
//! api/v1/crates/<crate>/<ver>/unyank    cargo yank --undo     (hosted, PUT)
//! api/v1/crates?q=<query>               cargo search          (hosted, GET)
//! ```
//!
//! `config.json` is generated to point cargo at this repository's own download
//! endpoint so that artifacts are served (and cached/age-gated) through
//! forklift. Hosted and group repositories also advertise `api`, which enables
//! the Registry Web API subset above (a group only searches). Every other Web
//! API route (owners) answers with cargo's error envelope.

use std::sync::Arc;

use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use http::header::CONTENT_TYPE;
use http::request::Parts;
use http::{HeaderValue, Method, StatusCode};

use crate::auth;
use crate::meta;
use crate::server::http_error;

use super::maven::last_modified;
use super::router::{Resolved, action_for_method, join_upstream};
use super::{FetchSpec, Kind, Manager, path_base, request_body};

/// Serves the Cargo sparse-registry protocol.
pub(crate) async fn handle_cargo(m: Arc<Manager>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let res = match m.resolve(&parts, meta::FORMAT_CARGO).await {
        Ok(res) => res,
        Err(resp) => return resp,
    };
    if let Some(op) = cargo_api_op(&parts.method, &res.path) {
        return m.cargo_api(&parts, &res, body, op).await;
    }
    if let Err(resp) = m.authorize(
        &parts,
        &res.repo.name,
        action_for_method(&parts.method),
        res.cfg.public,
    ) {
        return resp;
    }

    if res.path == "config.json" && (parts.method == Method::GET || parts.method == Method::HEAD) {
        return m.cargo_config(&parts, &res);
    }
    let (pkg, version) = (cargo_package(&res.path), cargo_version(&res.path));
    let parts = Arc::new(parts);
    if let Some(resp) = m
        .policy_gates(Arc::clone(&parts), Arc::new(res.clone()), &pkg, &version)
        .await
    {
        return resp;
    }

    match parts.method {
        Method::GET | Method::HEAD => {
            let spec = FetchSpec {
                repo: res.repo.clone(),
                cfg: res.cfg.clone(),
                path: res.path.clone(),
                upstream_url: join_upstream(&res.repo.upstream_url, &res.path),
                kind: cargo_kind(&res.path),
                version: version.clone(),
                content_type: cargo_content_type(&res.path),
                extract_published: Some(Arc::new(last_modified)),
                final_gate: Some(m.final_policy_gate(res, &pkg, &version)),
                ..FetchSpec::blank()
            };
            m.engine.serve(parts, spec).await
        }
        Method::PUT => {
            if res.repo.r#type != meta::TYPE_HOSTED {
                return http_error(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "uploads are only allowed on local repositories",
                );
            }
            let username = super::username_from_context(&parts);
            if m.engine
                .put(
                    &res.repo,
                    &res.path,
                    &cargo_version(&res.path),
                    &cargo_content_type(&res.path),
                    None,
                    request_body(body),
                    &username,
                )
                .await
                .is_err()
            {
                return http_error(StatusCode::INTERNAL_SERVER_ERROR, "store failed");
            }
            m.scan_stored(&res.repo, &res.path);
            m.resolve_stored(&res.repo, &res.path);
            StatusCode::CREATED.into_response()
        }
        _ => http_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
    }
}

/// A Registry Web API call, as opposed to an index or download request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CargoApiOp {
    Search,
    Publish,
    SetYanked {
        name: String,
        version: String,
        yanked: bool,
    },
    Unsupported,
}

/// Classifies a repo-relative path as a Registry Web API call. Downloads are
/// excluded: they stay on the artifact path, including the raw compatibility
/// PUT.
pub(crate) fn cargo_api_op(method: &Method, path: &str) -> Option<CargoApiOp> {
    let rest = path.strip_prefix("api/v1/")?;
    if cargo_kind(path) == Kind::Artifact {
        return None;
    }
    let segments: Vec<&str> = rest.split('/').collect();
    let op = match (method, segments.as_slice()) {
        (&Method::GET | &Method::HEAD, ["crates"]) => CargoApiOp::Search,
        (&Method::PUT, ["crates", "new"]) => CargoApiOp::Publish,
        (&Method::DELETE, ["crates", name, version, "yank"]) => CargoApiOp::SetYanked {
            name: (*name).to_string(),
            version: (*version).to_string(),
            yanked: true,
        },
        (&Method::PUT, ["crates", name, version, "unyank"]) => CargoApiOp::SetYanked {
            name: (*name).to_string(),
            version: (*version).to_string(),
            yanked: false,
        },
        _ => CargoApiOp::Unsupported,
    };
    Some(op)
}

/// Renders an error in the Registry Web API envelope, which cargo prints as
/// the cause of a failed publish or yank.
pub(crate) fn cargo_api_error(status: StatusCode, detail: &str) -> Response {
    (status, axum::Json(cargo_error_body(detail))).into_response()
}

/// The Registry Web API error envelope (`CargoErrors` in the OpenAPI document).
pub(crate) fn cargo_error_body(detail: &str) -> serde_json::Value {
    serde_json::json!({"errors": [{"detail": detail}]})
}

/// The yank and unyank success body (`CargoOk`).
pub(crate) fn cargo_ok_body() -> serde_json::Value {
    serde_json::json!({"ok": true})
}

/// The publish success body (`CargoPublishResult`). Forklift never rewrites
/// categories or badges, so there is nothing to warn about.
pub(crate) fn cargo_publish_body() -> serde_json::Value {
    serde_json::json!({
        "warnings": {"invalid_categories": [], "invalid_badges": [], "other": []}
    })
}

impl Manager {
    /// Serves the Registry Web API subset. Search needs `read`. Publish and
    /// yank both need `write`, matching the UI's yank action, even though yank
    /// arrives as DELETE.
    async fn cargo_api(
        &self,
        parts: &Parts,
        res: &Resolved,
        body: axum::body::Body,
        op: CargoApiOp,
    ) -> Response {
        match op {
            CargoApiOp::Unsupported => cargo_api_error(
                StatusCode::NOT_FOUND,
                "this registry supports only search, publish, yank and unyank",
            ),
            CargoApiOp::Search => {
                if let Err(resp) =
                    self.authorize(parts, &res.repo.name, auth::ACTION_READ, res.cfg.public)
                {
                    return resp;
                }
                self.cargo_search(parts, res).await
            }
            CargoApiOp::Publish | CargoApiOp::SetYanked { .. } => {
                if let Err(resp) = self.authorize(parts, &res.repo.name, auth::ACTION_WRITE, false)
                {
                    return resp;
                }
                if res.repo.r#type != meta::TYPE_HOSTED {
                    return cargo_api_error(
                        StatusCode::METHOD_NOT_ALLOWED,
                        "publishing is only allowed on hosted repositories",
                    );
                }
                match op {
                    CargoApiOp::SetYanked {
                        name,
                        version,
                        yanked,
                    } => {
                        self.cargo_set_yanked(parts, res, &name, &version, yanked)
                            .await
                    }
                    _ => self.cargo_publish_atomic(parts, res, body).await,
                }
            }
        }
    }

    /// Flips a managed crate version's yanked flag through the same lifecycle
    /// operation the UI uses.
    async fn cargo_set_yanked(
        &self,
        parts: &Parts,
        res: &Resolved,
        name: &str,
        version: &str,
        yanked: bool,
    ) -> Response {
        let Some(uploader) = self.uploader.read().clone() else {
            return cargo_api_error(StatusCode::SERVICE_UNAVAILABLE, "yank is unavailable");
        };
        let Ok(parsed) = semver::Version::parse(version) else {
            return cargo_api_error(StatusCode::BAD_REQUEST, "invalid crate version");
        };
        // Publications are keyed by the lowercase name and the SemVer identity,
        // which excludes build metadata.
        let mut identity = parsed.to_string();
        if let Some(index) = identity.find('+') {
            identity.truncate(index);
        }
        let publication = match self
            .store
            .get_artifact_publication_by_identity(
                res.repo.id,
                meta::FORMAT_CARGO,
                &name.to_lowercase(),
                &identity,
            )
            .await
        {
            Ok(publication) => publication,
            Err(meta::Error::NotFound) => {
                return cargo_api_error(StatusCode::NOT_FOUND, "crate version not found");
            }
            Err(_) => {
                return cargo_api_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "crate version could not be looked up",
                );
            }
        };
        let (principal, _) = super::native_upload::native_upload_principal(parts);
        match uploader
            .set_cargo_yanked(&res.repo, &publication.id, &principal, yanked)
            .await
        {
            Ok(_) => axum::Json(cargo_ok_body()).into_response(),
            Err(problem) => cargo_api_error(
                StatusCode::from_u16(problem.status as u16)
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                &problem.detail,
            ),
        }
    }

    /// Synthesises the sparse registry `config.json`, pointing cargo's download
    /// URL at this repository so `.crate` fetches flow through forklift.
    fn cargo_config(&self, parts: &Parts, res: &Resolved) -> Response {
        // A group answers with its first member's config, so both URLs name the
        // group the client is talking to: downloads then fan out across every
        // member instead of reaching only the first one.
        let via_group = super::group::via_group(parts);
        let base = format!(
            "{}/cargo/{}",
            self.external_base(parts),
            super::group::serving_repo_name(parts, &res.repo.name)
        );
        let mut resp = if parts.method == Method::HEAD {
            StatusCode::OK.into_response()
        } else {
            let mut document = serde_json::Map::new();
            if self
                .authz
                .as_ref()
                .is_some_and(|authz| !authz.anonymous_read())
            {
                document.insert("auth-required".to_string(), serde_json::Value::Bool(true));
            }
            document.insert(
                "dl".to_string(),
                serde_json::Value::String(format!(
                    "{base}/api/v1/crates/{{crate}}/{{version}}/download"
                )),
            );
            // Cargo appends `/api/v1/crates/...` to `api` and refuses every
            // Web API command, search included, without it. A group advertises
            // its own API: search falls through to its hosted member and a
            // publish is refused as read-only. A proxy has neither.
            if (res.repo.r#type == meta::TYPE_HOSTED || via_group) && self.uploader.read().is_some()
            {
                document.insert("api".to_string(), serde_json::Value::String(base.clone()));
            }
            let mut body = serde_json::to_string(&serde_json::Value::Object(document))
                .unwrap_or_else(|_| "{}".to_string());
            body.push('\n');
            body.into_response()
        };
        resp.headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        resp
    }

    /// The externally-visible base URL used when synthesising URLs in responses.
    /// When `FORKLIFT_EXTERNAL_URL` is configured it is used verbatim; otherwise
    /// the base is derived from the request, taking reverse-proxy headers into
    /// account so synthesised URLs are reachable. Request-derived values (Host,
    /// `X-Forwarded-*`) are client-controlled, so they must never be embedded in
    /// cached bodies — metadata is cached in its original upstream form and
    /// rewritten per request.
    pub(crate) fn external_base(&self, parts: &Parts) -> String {
        let configured = self.external_url.read().clone();
        if !configured.is_empty() {
            return configured;
        }
        let scheme = if parts.uri.scheme_str() == Some("https")
            || super::header_str(&parts.headers, "X-Forwarded-Proto") == "https"
        {
            "https"
        } else {
            "http"
        };
        let mut host = super::header_str(&parts.headers, "Host").to_string();
        if host.is_empty() {
            host = parts
                .uri
                .authority()
                .map(|a| a.to_string())
                .unwrap_or_default();
        }
        let forwarded = super::header_str(&parts.headers, "X-Forwarded-Host");
        if !forwarded.is_empty() {
            host = forwarded.to_string();
        }
        format!("{scheme}://{host}")
    }
}

pub(crate) fn cargo_kind(p: &str) -> Kind {
    // Match "api/v1/crates/" unanchored: the download path arrives repo-relative
    // with the leading slash stripped (`resolve_repo`), so requiring
    // "/api/v1/..." would misclassify real downloads as metadata. Mirrors
    // `cargo_package`.
    if p.contains("api/v1/crates/") && p.ends_with("/download") {
        Kind::Artifact
    } else {
        Kind::Metadata
    }
}

pub(crate) fn cargo_version(p: &str) -> String {
    // .../api/v1/crates/<crate>/<version>/download — matched unanchored so the
    // leading-slash-stripped repo-relative download path also resolves a
    // version.
    if let Some((_, after)) = p.split_once("api/v1/crates/") {
        let rest = after.strip_suffix("/download").unwrap_or(after);
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() == 2 {
            return parts[1].to_string();
        }
    }
    String::new()
}

/// Extracts the crate name from a cargo protocol path: the crate segment of a
/// download URL, or the final segment of a sparse-index entry (layouts `1/<c>`,
/// `2/<cr>`, `3/<a>/<crate>`, `<aa>/<bb>/<crate>`). Crate names are
/// case-insensitive in the index, so the result is lowercased.
pub(crate) fn cargo_package(p: &str) -> String {
    if p == "config.json" {
        return String::new();
    }
    if let Some((_, after)) = p.split_once("api/v1/crates/") {
        let crate_name = after.split_once('/').map(|(c, _)| c).unwrap_or(after);
        return crate_name.to_lowercase();
    }
    path_base(p).to_lowercase()
}

pub(crate) fn cargo_content_type(p: &str) -> String {
    if cargo_kind(p) == Kind::Artifact {
        "application/gzip".to_string()
    } else {
        "text/plain; charset=utf-8".to_string()
    }
}
