//! Serves the Maven repository layout, which Gradle also consumes (Gradle
//! Module Metadata `.module` files are ordinary artifacts here). Artifacts live
//! at `<group-path>/<artifact>/<version>/<file>`; `maven-metadata.xml` documents
//! are mutable indexes revalidated on the metadata TTL.

use std::sync::Arc;

use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use http::{Method, StatusCode};

use crate::meta;
use crate::server::http_error;

use super::router::{action_for_method, join_upstream};
use super::{FetchSpec, Kind, Manager, path_base, request_body};

/// Serves the Maven repository layout.
pub(crate) async fn handle_maven(m: Arc<Manager>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let res = match m.resolve(&parts, meta::FORMAT_MAVEN).await {
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
    let (pkg, version) = (maven_package(&res.path), maven_version(&res.path));
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
                kind: maven_kind(&res.path),
                version: version.clone(),
                content_type: maven_content_type(&res.path),
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
                    &maven_version(&res.path),
                    &maven_content_type(&res.path),
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

/// Classifies `maven-metadata.xml` (and its checksums) as mutable metadata;
/// everything else is an immutable artifact.
pub(crate) fn maven_kind(p: &str) -> Kind {
    if path_base(p).starts_with("maven-metadata.xml") {
        Kind::Metadata
    } else {
        Kind::Artifact
    }
}

/// Best-effort extracts the version directory from an artifact path.
pub(crate) fn maven_version(p: &str) -> String {
    if maven_kind(p) == Kind::Metadata {
        return String::new();
    }
    let parts: Vec<&str> = p.split('/').collect();
    if parts.len() >= 2 {
        return parts[parts.len() - 2].to_string();
    }
    String::new()
}

/// Heuristically extracts the `group:artifact` coordinate from a repository
/// path. Artifacts live at `<group-path>/<artifact>/<version>/<file>`;
/// `maven-metadata.xml` sits at the artifact level, or inside a `-SNAPSHOT`
/// version directory. Returns `""` when too few segments remain (never block on
/// unknown).
pub(crate) fn maven_package(p: &str) -> String {
    let trimmed = p.trim_matches('/');
    let mut parts: Vec<&str> = trimmed.split('/').collect();
    if path_base(p).starts_with("maven-metadata.xml") {
        parts.pop();
        if parts.last().is_some_and(|s| s.ends_with("-SNAPSHOT")) {
            parts.pop();
        }
    } else if parts.len() >= 2 {
        parts.truncate(parts.len() - 2);
    } else {
        return String::new();
    }
    if parts.len() < 2 {
        return String::new();
    }
    let last = parts[parts.len() - 1];
    format!("{}:{}", parts[..parts.len() - 1].join("."), last)
}

pub(crate) fn maven_content_type(p: &str) -> String {
    let ct = if p.ends_with(".xml") || p.ends_with(".pom") {
        "application/xml"
    } else if p.ends_with(".jar") || p.ends_with(".war") {
        "application/java-archive"
    } else if p.ends_with(".module") || p.ends_with(".json") {
        "application/json"
    } else if p.ends_with(".sha1")
        || p.ends_with(".md5")
        || p.ends_with(".sha256")
        || p.ends_with(".sha512")
    {
        "text/plain"
    } else {
        "application/octet-stream"
    };
    ct.to_string()
}

/// Parses the upstream `Last-Modified` header into a release time for the age
/// policy. Maven artifacts carry no per-version timestamp natively, so the
/// upstream file mtime is the best available signal.
pub(crate) fn last_modified(resp: &reqwest::Response) -> Option<DateTime<Utc>> {
    let v = resp
        .headers()
        .get(http::header::LAST_MODIFIED)?
        .to_str()
        .ok()?;
    if v.is_empty() {
        return None;
    }
    let t = httpdate::parse_http_date(v).ok()?;
    Some(DateTime::<Utc>::from(t))
}
