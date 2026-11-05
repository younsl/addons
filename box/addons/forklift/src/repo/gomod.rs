//! Serves the Go module proxy protocol (GOPROXY). Paths under `/go/{repo}/`
//! mirror the GOPROXY layout:
//!
//! ```text
//! <module>/@v/list            list of versions          (metadata)
//! <module>/@v/<version>.info  version metadata + Time   (metadata)
//! <module>/@v/<version>.mod   the go.mod file           (artifact)
//! <module>/@v/<version>.zip   the module zip            (artifact)
//! <module>/@latest            latest version info       (metadata)
//! ```

use std::sync::Arc;

use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use http::{Method, StatusCode};

use crate::meta;
use crate::server::http_error;

use super::maven::last_modified;
use super::router::{action_for_method, join_upstream};
use super::{FetchSpec, Kind, Manager, request_body};

/// Serves the Go module proxy protocol.
pub(crate) async fn handle_go(m: Arc<Manager>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let res = match m.resolve(&parts, meta::FORMAT_GO).await {
        Ok(res) => res,
        Err(resp) => return resp,
    };
    if let Err(resp) = m.authorize(
        &parts,
        &res.repo.name,
        action_for_method(&parts.method),
        res.cfg.public,
    ) {
        return resp;
    }
    let (pkg, version) = (go_package(&res.path), go_version(&res.path));
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
                kind: go_kind(&res.path),
                version: version.clone(),
                content_type: go_content_type(&res.path),
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
                    &go_version(&res.path),
                    &go_content_type(&res.path),
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

pub(crate) fn go_kind(p: &str) -> Kind {
    if p.ends_with("/@v/list") || p.ends_with("/@latest") || p.ends_with(".info") {
        Kind::Metadata
    } else {
        Kind::Artifact
    }
}

/// Extracts the module path from a GOPROXY protocol path (everything before
/// `/@v/` or `/@latest`), keeping the `!`-escaped form as the canonical key.
pub(crate) fn go_package(p: &str) -> String {
    if let Some((before, _)) = p.split_once("/@v/") {
        return before.to_string();
    }
    if let Some(module) = p.strip_suffix("/@latest") {
        return module.to_string();
    }
    String::new()
}

pub(crate) fn go_version(p: &str) -> String {
    let Some((_, rest)) = p.split_once("/@v/") else {
        return String::new();
    };
    for ext in [".info", ".mod", ".zip"] {
        if let Some(before) = rest.strip_suffix(ext) {
            return before.to_string();
        }
    }
    String::new()
}

pub(crate) fn go_content_type(p: &str) -> String {
    let ct = if p.ends_with(".info") || p.ends_with("/@latest") {
        "application/json"
    } else if p.ends_with(".mod") {
        "text/plain; charset=utf-8"
    } else if p.ends_with(".zip") {
        "application/zip"
    } else {
        "text/plain; charset=utf-8"
    };
    ct.to_string()
}
