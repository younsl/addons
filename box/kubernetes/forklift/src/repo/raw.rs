//! Serves the raw repository layout: arbitrary files addressed by their literal
//! path. Raw artifacts carry no package coordinate, version convention, or
//! ecosystem scanning; the path is the whole identity.

use std::sync::Arc;

use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use http::header::{CONTENT_DISPOSITION, CONTENT_SECURITY_POLICY, X_CONTENT_TYPE_OPTIONS};
use http::{HeaderValue, Method, StatusCode};

use crate::meta;
use crate::server::http_error;

use super::maven::last_modified;
use super::router::{action_for_method, join_upstream};
use super::{FetchSpec, Kind, Manager, path_base, path_ext, request_body};

/// Serves the raw repository layout. Artifacts are served on GET/HEAD and
/// stored on PUT (hosted only), mirroring the Maven handler.
pub(crate) async fn handle_raw(m: Arc<Manager>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let res = match m.resolve(&parts, meta::FORMAT_RAW).await {
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
    let (pkg, version) = (raw_package(&res.path), raw_version(&res.path));
    let parts = Arc::new(parts);
    if let Some(resp) = m
        .policy_gates(Arc::clone(&parts), Arc::new(res.clone()), &pkg, &version)
        .await
    {
        return resp;
    }

    match parts.method {
        Method::GET | Method::HEAD => {
            // Raw artifacts are arbitrary user uploads served from the same
            // origin as the console, so force download semantics: an attachment
            // disposition, no MIME sniffing, and a locked-down CSP prevent a
            // stored HTML/SVG/JS payload from executing as same-origin script.
            let filename: String = path_base(&res.path)
                .chars()
                .filter(|c| !matches!(c, '"' | '\\' | '\r' | '\n'))
                .collect();
            let spec = FetchSpec {
                repo: res.repo.clone(),
                cfg: res.cfg.clone(),
                path: res.path.clone(),
                upstream_url: join_upstream(&res.repo.upstream_url, &res.path),
                kind: Kind::Artifact,
                version: version.clone(),
                content_type: raw_content_type(&res.path),
                extract_published: Some(Arc::new(last_modified)),
                final_gate: Some(m.final_policy_gate(res, &pkg, &version)),
                ..FetchSpec::blank()
            };
            let mut resp = m.engine.serve(Arc::clone(&parts), spec).await;
            let headers = resp.headers_mut();
            if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
                headers.insert(CONTENT_DISPOSITION, v);
            }
            headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
            headers.insert(
                CONTENT_SECURITY_POLICY,
                HeaderValue::from_static("default-src 'none'; sandbox"),
            );
            resp
        }
        Method::PUT => {
            if res.repo.r#type != meta::TYPE_HOSTED {
                return http_error(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "uploads are only allowed on local repositories",
                );
            }
            let username = super::username_from_context(&parts);
            let content_type = raw_content_type(&res.path);
            if m.engine
                .put(
                    &res.repo,
                    &res.path,
                    &raw_version(&res.path),
                    &content_type,
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

/// Uses the full artifact path as the identity, so approval and audit key on the
/// exact file rather than a synthetic coordinate.
pub(crate) fn raw_package(p: &str) -> String {
    p.to_string()
}

/// Always empty: raw artifacts have no version convention, which also makes
/// vuln/license scanning a no-op for the format.
pub(crate) fn raw_version(_p: &str) -> String {
    String::new()
}

/// Guesses from the file extension, defaulting to a generic binary type when the
/// extension is unknown. Types a browser would render inline are neutralized to
/// octet-stream so a stored payload cannot execute as same-origin script; this
/// is stored with the artifact and reused on serve.
pub(crate) fn raw_content_type(p: &str) -> String {
    let ct = match builtin_type_by_extension(path_ext(p)) {
        Some(ct) => ct,
        None => return "application/octet-stream".to_string(),
    };
    let base = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match base.as_str() {
        "text/html"
        | "application/xhtml+xml"
        | "image/svg+xml"
        | "application/javascript"
        | "text/javascript"
        | "application/xml"
        | "text/xml" => "application/octet-stream".to_string(),
        _ => ct.to_string(),
    }
}

///
/// The release image is `FROM scratch`, so none of those files exist there and the builtin
/// table is the whole map; reproducing it literally keeps the stored content types identical.
fn builtin_type_by_extension(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        ".avif" => Some("image/avif"),
        ".css" => Some("text/css; charset=utf-8"),
        ".gif" => Some("image/gif"),
        ".htm" | ".html" => Some("text/html; charset=utf-8"),
        ".jpeg" | ".jpg" => Some("image/jpeg"),
        ".js" | ".mjs" => Some("text/javascript; charset=utf-8"),
        ".json" => Some("application/json"),
        ".pdf" => Some("application/pdf"),
        ".png" => Some("image/png"),
        ".svg" => Some("image/svg+xml"),
        ".wasm" => Some("application/wasm"),
        ".webp" => Some("image/webp"),
        ".xml" => Some("text/xml; charset=utf-8"),
        _ => None,
    }
}
