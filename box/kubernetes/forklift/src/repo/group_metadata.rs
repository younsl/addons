//! Merges mutable indexes across every member of a group repository, so a
//! client sees one aggregate document instead of whichever member answered
//! first. Immutable asset requests keep the streaming first-hit path in
//! [`super::group::grouped`].

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use http::header::{CONTENT_TYPE, ETAG, VARY};
use http::request::Parts;
use http::{HeaderValue, Method, StatusCode, Uri};
use sha1::Digest as _;

use crate::meta::{self, GroupMetadataCache, Repository};
use crate::server::http_error;

use super::pypi::PYPI_JSON_TYPE;
use super::router::{HandlerFn, route_params};
use super::uiupload_go::GoInfo;
use super::uiupload_maven::{MavenMetadata, parse_maven_metadata, render_maven_metadata};
use super::{Manager, header_str, not_found};

/// Caps the bytes one member may contribute to an aggregate.
const MAX_GROUP_METADATA_BYTES: usize = 64 << 20;

/// Serialises group aggregate rebuilds for one cache key.
pub(crate) struct GroupMetadataLock {
    pub(crate) mu: tokio::sync::Mutex<()>,
}

/// One member's captured response.
struct CapturedGroupResponse {
    status: StatusCode,
    content_type: String,
    body: Vec<u8>,
    /// The member's body exceeded [`MAX_GROUP_METADATA_BYTES`].
    over: bool,
}

/// Classifies a request as an aggregatable index, or `""` when it addresses an
/// immutable asset.
fn group_metadata_kind(format: &str, wildcard: &str) -> &'static str {
    match format {
        meta::FORMAT_MAVEN => {
            let base = wildcard.to_lowercase();
            if base.ends_with("maven-metadata.xml")
                || base.ends_with("maven-metadata.xml.sha1")
                || base.ends_with("maven-metadata.xml.sha256")
            {
                return "maven";
            }
        }
        meta::FORMAT_NPM => {
            if !wildcard.is_empty() && !wildcard.contains("/-/") {
                return "npm";
            }
        }
        meta::FORMAT_CARGO => {
            // `api/v1/` also covers `api/v1/crates?q=`, whose path has no
            // trailing slash and is a search, not a sparse-index entry.
            if wildcard != "config.json"
                && !wildcard.contains("/api/v1/crates/")
                && !wildcard.starts_with("api/v1/")
            {
                return "cargo";
            }
        }
        meta::FORMAT_GO => {
            if wildcard.ends_with("/@v/list") {
                return "go-list";
            }
            if wildcard.ends_with("/@latest") {
                return "go-latest";
            }
        }
        meta::FORMAT_PYPI => {
            if wildcard.starts_with("simple/") {
                return "pypi";
            }
        }
        meta::FORMAT_OCI if wildcard.ends_with("/tags/list") => return "oci-tags",
        _ => {}
    }
    ""
}

impl Manager {
    /// Merges mutable indexes from all successful members while retaining member
    /// priority for duplicate coordinates. `None` falls through to the
    /// first-hit member fan-out.
    pub(crate) async fn serve_group_metadata(
        self: &Arc<Self>,
        parts: &Parts,
        group: &Repository,
        members: &[String],
        h: HandlerFn,
    ) -> Option<Response> {
        let (_, wildcard) = route_params(parts.uri.path());
        let wildcard = wildcard.trim_start_matches('/').to_string();
        let kind = group_metadata_kind(&group.format, &wildcard);
        if kind.is_empty() {
            return None;
        }
        let representation =
            group_metadata_representation(kind, &wildcard, header_str(&parts.headers, "Accept"));
        let revision = crate::meta::time::format_time(group.updated_at);
        let lock = self
            .acquire_group_metadata(&format!("{}\u{0}{wildcard}\u{0}{representation}", group.id));
        let _held = lock.mu.lock().await;

        if let Ok(cache) = self
            .store
            .get_group_metadata_cache(group.id, &wildcard, &representation, &revision)
            .await
            && let Ok((reader, _)) = self.engine.blobs.open(&cache.blob_sha256).await
        {
            return Some(serve_group_metadata_blob(
                parts,
                reader,
                &cache.blob_sha256,
                &representation,
            ));
        }

        let mut member_wildcard = wildcard.clone();
        let mut checksum = "";
        if kind == "maven" {
            for suffix in [".sha256", ".sha1"] {
                if let Some(before) = member_wildcard.strip_suffix(suffix) {
                    member_wildcard = before.to_string();
                    checksum = suffix;
                    break;
                }
            }
        }

        let mut responses: Vec<CapturedGroupResponse> = Vec::new();
        for member in members {
            let request = request_with_member(parts, member, &member_wildcard, &wildcard);
            let capture = capture(h(Arc::clone(self), request).await).await;
            if capture.over {
                return Some(http_error(
                    StatusCode::BAD_GATEWAY,
                    "group metadata exceeds aggregation limit",
                ));
            }
            if capture.status == StatusCode::NOT_FOUND {
                continue;
            }
            if !capture.status.is_success() {
                return Some(copy_captured_response(
                    capture,
                    parts.method == Method::HEAD,
                ));
            }
            responses.push(capture);
        }
        if responses.is_empty() {
            return Some(not_found());
        }
        let Some((mut body, mut content_type)) = merge_group_metadata(kind, &responses) else {
            return Some(http_error(
                StatusCode::BAD_GATEWAY,
                "member metadata cannot be aggregated",
            ));
        };
        if !checksum.is_empty() {
            body = if checksum == ".sha1" {
                let value = sha1::Sha1::digest(&body);
                format!("{}\n", hex::encode(value)).into_bytes()
            } else {
                let value = sha2::Sha256::digest(&body);
                format!("{}\n", hex::encode(value)).into_bytes()
            };
            content_type = "text/plain; charset=utf-8".to_string();
        }

        let mut cached_digest = None;
        if let Ok((digest, size)) = self
            .engine
            .blobs
            .put(Box::pin(std::io::Cursor::new(body.clone())))
            .await
        {
            let sources = serde_json::to_string(members).unwrap_or_else(|_| "[]".to_string());
            if self
                .store
                .put_group_metadata_cache(GroupMetadataCache {
                    group_repo_id: group.id,
                    path: wildcard.clone(),
                    representation: representation.clone(),
                    blob_sha256: digest.clone(),
                    size,
                    sources_json: sources,
                    config_revision: revision,
                    expires_at: Utc::now() + chrono::TimeDelta::seconds(30),
                    ..Default::default()
                })
                .await
                .is_ok()
            {
                cached_digest = Some(digest);
            }
        }

        let mut resp = if let Some(digest) = &cached_digest
            && etag_matches(header_str(&parts.headers, "If-None-Match"), digest)
        {
            StatusCode::NOT_MODIFIED.into_response()
        } else if parts.method == Method::HEAD {
            StatusCode::OK.into_response()
        } else {
            body.into_response()
        };
        let headers = resp.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&content_type) {
            headers.insert(CONTENT_TYPE, v);
        }
        headers.insert(VARY, HeaderValue::from_static("Accept"));
        if let Some(digest) = &cached_digest
            && let Ok(v) = HeaderValue::from_str(&format!("\"{digest}\""))
        {
            headers.insert(ETAG, v);
        }
        Some(resp)
    }

    /// Provides singleflight semantics without retaining keys after the last
    /// waiter completes.
    ///
    fn acquire_group_metadata(self: &Arc<Self>, key: &str) -> GroupMetadataGuard {
        let entry = {
            let mut locks = self.group_metadata_locks.lock();
            Arc::clone(locks.entry(key.to_string()).or_insert_with(|| {
                Arc::new(GroupMetadataLock {
                    mu: tokio::sync::Mutex::new(()),
                })
            }))
        };
        GroupMetadataGuard {
            manager: Arc::clone(self),
            key: key.to_string(),
            entry,
        }
    }
}

/// Drops the shared lock entry once no waiter holds it any more.
struct GroupMetadataGuard {
    manager: Arc<Manager>,
    key: String,
    entry: Arc<GroupMetadataLock>,
}

impl std::ops::Deref for GroupMetadataGuard {
    type Target = GroupMetadataLock;
    fn deref(&self) -> &GroupMetadataLock {
        &self.entry
    }
}

impl Drop for GroupMetadataGuard {
    fn drop(&mut self) {
        let mut locks = self.manager.group_metadata_locks.lock();
        // One reference here, one in the map: this was the last waiter.
        if let Some(entry) = locks.get(&self.key)
            && Arc::strong_count(entry) <= 2
        {
            locks.remove(&self.key);
        }
    }
}

/// Reads a member response into memory, capped at
/// [`MAX_GROUP_METADATA_BYTES`].
async fn capture(resp: Response) -> CapturedGroupResponse {
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = axum::body::to_bytes(resp.into_body(), MAX_GROUP_METADATA_BYTES + 1)
        .await
        .unwrap_or_default();
    CapturedGroupResponse {
        status,
        content_type,
        over: body.len() > MAX_GROUP_METADATA_BYTES,
        body: body.to_vec(),
    }
}

/// Rebuilds the request for one member: the repository segment is replaced, and
/// for Maven checksum requests so is the repo-relative remainder (members are
/// asked for the document, not its checksum).
fn request_with_member(
    parts: &Parts,
    member: &str,
    member_wildcard: &str,
    wildcard: &str,
) -> Request {
    let path = parts.uri.path();
    let mut segments: Vec<&str> = path.split('/').collect();
    if segments.len() > 2 {
        segments[2] = member;
    }
    let mut rebuilt = segments.join("/");
    if member_wildcard != wildcard
        && let Some(cut) = rebuilt.len().checked_sub(wildcard.len())
        && rebuilt.ends_with(wildcard)
    {
        rebuilt.truncate(cut);
        rebuilt.push_str(member_wildcard);
    }
    if let Some(q) = parts.uri.query() {
        rebuilt.push('?');
        rebuilt.push_str(q);
    }
    let mut req = Request::new(Body::empty());
    *req.method_mut() = parts.method.clone();
    *req.version_mut() = parts.version;
    *req.headers_mut() = parts.headers.clone();
    *req.extensions_mut() = parts.extensions.clone();
    *req.uri_mut() = rebuilt.parse::<Uri>().unwrap_or_else(|_| parts.uri.clone());
    req
}

pub(crate) fn group_metadata_representation(kind: &str, path: &str, accept: &str) -> String {
    if kind == "maven" {
        if path.ends_with(".sha1") {
            return format!("{kind}:sha1");
        }
        if path.ends_with(".sha256") {
            return format!("{kind}:sha256");
        }
    }
    if kind == "pypi" && accept.to_lowercase().contains(PYPI_JSON_TYPE) {
        return format!("{kind}:json");
    }
    format!("{kind}:default")
}

fn group_metadata_content_type(representation: &str) -> &'static str {
    match representation {
        "maven:default" => "application/xml",
        "maven:sha1" | "maven:sha256" => "text/plain; charset=utf-8",
        "npm:default" | "pypi:json" => "application/json",
        "cargo:default" | "go-list:default" => "text/plain; charset=utf-8",
        "go-latest:default" | "oci-tags:default" => "application/json",
        _ => "text/html; charset=utf-8",
    }
}

fn serve_group_metadata_blob(
    parts: &Parts,
    reader: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    digest: &str,
    representation: &str,
) -> Response {
    let mut resp = if etag_matches(header_str(&parts.headers, "If-None-Match"), digest) {
        StatusCode::NOT_MODIFIED.into_response()
    } else if parts.method == Method::HEAD {
        StatusCode::OK.into_response()
    } else {
        Body::from_stream(tokio_util::io::ReaderStream::new(reader)).into_response()
    };
    let headers = resp.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static(group_metadata_content_type(representation)),
    );
    headers.insert(VARY, HeaderValue::from_static("Accept"));
    if let Ok(v) = HeaderValue::from_str(&format!("\"{digest}\"")) {
        headers.insert(ETAG, v);
    }
    resp
}

pub(crate) fn etag_matches(header: &str, digest: &str) -> bool {
    header
        .split(',')
        .any(|value| value.trim() == format!("\"{digest}\""))
}

fn copy_captured_response(response: CapturedGroupResponse, head: bool) -> Response {
    let mut resp = if head {
        response.status.into_response()
    } else {
        (response.status, response.body).into_response()
    };
    if !response.content_type.is_empty()
        && let Ok(v) = HeaderValue::from_str(&response.content_type)
    {
        resp.headers_mut().insert(CONTENT_TYPE, v);
    }
    resp
}

/// Merges the captured member documents, returning the aggregate body and its
/// content type. `None` means the documents could not be aggregated.
fn merge_group_metadata(
    kind: &str,
    responses: &[CapturedGroupResponse],
) -> Option<(Vec<u8>, String)> {
    let values: Vec<&[u8]> = responses.iter().map(|r| r.body.as_slice()).collect();
    match kind {
        "maven" => merge_maven_group(&values),
        "npm" => merge_npm_group(&values),
        "cargo" => merge_cargo_group(&values),
        "go-list" => merge_go_list_group(&values),
        "go-latest" => merge_go_latest_group(&values),
        "pypi" => merge_pypi_group(&values, &responses[0].content_type),
        "oci-tags" => merge_oci_tags_group(&values),
        _ => None,
    }
}

pub(crate) fn merge_maven_group(values: &[&[u8]]) -> Option<(Vec<u8>, String)> {
    let mut merged = MavenMetadata::default();
    let mut seen: HashSet<String> = HashSet::new();
    for value in values {
        let document = parse_maven_metadata(value).ok()?;
        if merged.group_id.is_empty() {
            merged.group_id = document.group_id.clone();
            merged.artifact_id = document.artifact_id.clone();
        }
        if merged.group_id != document.group_id || merged.artifact_id != document.artifact_id {
            return None;
        }
        for version in document.versioning.versions {
            if seen.insert(version.clone()) {
                merged.versioning.versions.push(version);
            }
        }
        if document.versioning.last_updated > merged.versioning.last_updated {
            merged.versioning.last_updated = document.versioning.last_updated;
        }
    }
    merged.versioning.versions.sort();
    if let Some(last) = merged.versioning.versions.last() {
        merged.versioning.latest = last.clone();
        merged.versioning.release = last.clone();
    }
    Some((
        render_maven_metadata(&merged),
        "application/xml".to_string(),
    ))
}

type JsonObject = serde_json::Map<String, serde_json::Value>;

pub(crate) fn merge_npm_group(values: &[&[u8]]) -> Option<(Vec<u8>, String)> {
    let mut merged = JsonObject::new();
    let (mut versions, mut tags, mut times) =
        (JsonObject::new(), JsonObject::new(), JsonObject::new());
    for value in values {
        let document: JsonObject = serde_json::from_slice(value).ok()?;
        if merged.is_empty() {
            merged = document.clone();
        }
        for (key, target) in [
            ("versions", &mut versions),
            ("dist-tags", &mut tags),
            ("time", &mut times),
        ] {
            if let Some(serde_json::Value::Object(source)) = document.get(key) {
                for (k, item) in source {
                    target.entry(k.clone()).or_insert_with(|| item.clone());
                }
            }
        }
    }
    merged.insert("versions".to_string(), serde_json::Value::Object(versions));
    merged.insert("dist-tags".to_string(), serde_json::Value::Object(tags));
    merged.insert("time".to_string(), serde_json::Value::Object(times));
    let body = serde_json::to_vec(&sorted(merged)).ok()?;
    Some((body, "application/json".to_string()))
}

fn sorted(object: JsonObject) -> BTreeMap<String, serde_json::Value> {
    object.into_iter().collect()
}

pub(crate) fn merge_cargo_group(values: &[&[u8]]) -> Option<(Vec<u8>, String)> {
    #[derive(serde::Deserialize)]
    struct Entry {
        #[serde(default, rename = "vers")]
        version: String,
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut lines: Vec<String> = Vec::new();
    for value in values {
        let text = String::from_utf8_lossy(value);
        for line in text.trim().split('\n') {
            let entry: Entry = serde_json::from_str(line).ok()?;
            if entry.version.is_empty() {
                return None;
            }
            if seen.insert(entry.version.clone()) {
                lines.push(line.to_string());
            }
        }
    }
    Some((
        format!("{}\n", lines.join("\n")).into_bytes(),
        "text/plain; charset=utf-8".to_string(),
    ))
}

pub(crate) fn merge_go_list_group(values: &[&[u8]]) -> Option<(Vec<u8>, String)> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut versions: Vec<String> = Vec::new();
    for value in values {
        let text = String::from_utf8_lossy(value);
        for version in text.split_whitespace() {
            if go_semver_valid(version) && seen.insert(version.to_string()) {
                versions.push(version.to_string());
            }
        }
    }
    versions.sort_by(|a, b| go_semver_compare(a, b));
    Some((
        format!("{}\n", versions.join("\n")).into_bytes(),
        "text/plain; charset=utf-8".to_string(),
    ))
}

fn merge_go_latest_group(values: &[&[u8]]) -> Option<(Vec<u8>, String)> {
    let mut best = GoInfo::default();
    for value in values {
        let info: GoInfo = serde_json::from_slice(value).ok()?;
        if !go_semver_valid(&info.version) {
            return None;
        }
        if best.version.is_empty()
            || go_semver_compare(&info.version, &best.version) == std::cmp::Ordering::Greater
        {
            best = info;
        }
    }
    let mut body = serde_json::to_vec(&best).ok()?;
    body.push(b'\n');
    Some((body, "application/json".to_string()))
}

pub(crate) fn merge_pypi_group(values: &[&[u8]], content_type: &str) -> Option<(Vec<u8>, String)> {
    if content_type.contains("json") {
        let mut merged = BTreeMap::new();
        let mut meta_obj = JsonObject::new();
        meta_obj.insert(
            "api-version".to_string(),
            serde_json::Value::String("1.0".to_string()),
        );
        merged.insert("meta".to_string(), serde_json::Value::Object(meta_obj));
        let (mut files, mut projects) = (Vec::new(), Vec::new());
        let (mut seen_files, mut seen_projects) = (HashSet::new(), HashSet::new());
        for value in values {
            let document: JsonObject = serde_json::from_slice(value).ok()?;
            collect_unique(
                document.get("files"),
                "filename",
                &mut seen_files,
                &mut files,
            );
            collect_unique(
                document.get("projects"),
                "name",
                &mut seen_projects,
                &mut projects,
            );
        }
        if !files.is_empty() {
            merged.insert("files".to_string(), serde_json::Value::Array(files));
        }
        if !projects.is_empty() {
            merged.insert("projects".to_string(), serde_json::Value::Array(projects));
        }
        let body = serde_json::to_vec(&merged).ok()?;
        return Some((body, PYPI_JSON_TYPE.to_string()));
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut anchors: Vec<String> = Vec::new();
    for value in values {
        for anchor in find_anchors(&String::from_utf8_lossy(value)) {
            let mut key = unescape_html(&strip_html_tags(&anchor)).trim().to_string();
            if key.is_empty() {
                key = anchor.clone();
            }
            if seen.insert(key) {
                anchors.push(anchor);
            }
        }
    }
    Some((
        format!(
            "<!doctype html><html><body>\n{}\n</body></html>\n",
            anchors.join("\n")
        )
        .into_bytes(),
        "text/html; charset=utf-8".to_string(),
    ))
}

/// Appends the items of `raw` that carry a not-yet-seen value under `key`.
fn collect_unique(
    raw: Option<&serde_json::Value>,
    key: &str,
    seen: &mut HashSet<String>,
    out: &mut Vec<serde_json::Value>,
) {
    let Some(serde_json::Value::Array(items)) = raw else {
        return;
    };
    for item in items {
        let Some(value) = item.get(key).and_then(|v| v.as_str()) else {
            continue;
        };
        if !value.is_empty() && seen.insert(value.to_string()) {
            out.push(item.clone());
        }
    }
}

fn find_anchors(html: &str) -> Vec<String> {
    let lower = html.to_lowercase();
    let bytes = html.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(rel) = lower[i..].find("<a") {
        let start = i + rel;
        // `\b`: the tag name ends here.
        let after = start + 2;
        if bytes
            .get(after)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
        {
            i = after;
            continue;
        }
        let Some(open_end) = lower[start..].find('>').map(|p| start + p + 1) else {
            break;
        };
        let Some(close) = lower[open_end..].find("</a>").map(|p| open_end + p + 4) else {
            break;
        };
        out.push(html[start..close].to_string());
        i = close;
    }
    out
}

fn strip_html_tags(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut depth = 0usize;
    for c in value.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

fn unescape_html(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// The `tags/list` response document.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct OciTagsDoc {
    #[serde(default)]
    name: String,
    #[serde(default)]
    tags: Vec<String>,
}

/// Merges member `tags/list` responses: the union of tags, lexically sorted,
/// under the first member's name. A name mismatch between members is an error,
/// mirroring [`merge_maven_group`]'s identity check. Pagination (`n`/`last`) is
/// not applied to the merged document; the group serves the full list, which
/// every mainstream client accepts.
fn merge_oci_tags_group(values: &[&[u8]]) -> Option<(Vec<u8>, String)> {
    let mut merged = OciTagsDoc::default();
    let mut seen: HashSet<String> = HashSet::new();
    for value in values {
        let doc: OciTagsDoc = serde_json::from_slice(value).ok()?;
        if merged.name.is_empty() {
            merged.name = doc.name.clone();
        }
        if !doc.name.is_empty() && doc.name != merged.name {
            return None;
        }
        for tag in doc.tags {
            if seen.insert(tag.clone()) {
                merged.tags.push(tag);
            }
        }
    }
    merged.tags.sort();
    let body = serde_json::to_vec(&merged).ok()?;
    Some((body, "oci-tags".to_string()))
}

/// The field split mirrors `semver.parsed`: `prerelease` keeps its leading `-` and `build` its
/// leading `+`, and `short` holds the suffix [`go_semver_canonical`] appends when the version
/// omitted its minor or patch field.
struct GoSemver<'a> {
    major: &'a str,
    minor: &'a str,
    patch: &'a str,
    short: &'static str,
    prerelease: &'a str,
    build: &'a str,
}

pub(crate) fn go_semver_valid(v: &str) -> bool {
    go_semver_parse(v).is_some()
}

/// An invalid version canonicalises to `""`.
pub(crate) fn go_semver_canonical(v: &str) -> String {
    let Some(p) = go_semver_parse(v) else {
        return String::new();
    };
    if !p.build.is_empty() {
        return v[..v.len() - p.build.len()].to_string();
    }
    if !p.short.is_empty() {
        return format!("{v}{}", p.short);
    }
    v.to_string()
}

pub(crate) fn go_semver_major(v: &str) -> String {
    match go_semver_parse(v) {
        Some(p) => v[..1 + p.major.len()].to_string(),
        None => String::new(),
    }
}

pub(crate) fn go_semver_build(v: &str) -> String {
    go_semver_parse(v)
        .map(|p| p.build.to_string())
        .unwrap_or_default()
}

pub(crate) fn go_semver_prerelease(v: &str) -> String {
    go_semver_parse(v)
        .map(|p| p.prerelease.to_string())
        .unwrap_or_default()
}

/// Every identifier is ASCII, so the byte slicing below never splits a multi-byte character: a
/// non-ASCII byte fails the identifier check before it can be used as a slice boundary.
fn go_semver_parse(v: &str) -> Option<GoSemver<'_>> {
    let rest = v.strip_prefix('v')?;
    let (major, rest) = parse_int(rest)?;
    if rest.is_empty() {
        return Some(GoSemver {
            major,
            minor: "0",
            patch: "0",
            short: ".0.0",
            prerelease: "",
            build: "",
        });
    }
    let (minor, rest) = parse_int(rest.strip_prefix('.')?)?;
    if rest.is_empty() {
        return Some(GoSemver {
            major,
            minor,
            patch: "0",
            short: ".0",
            prerelease: "",
            build: "",
        });
    }
    let (patch, mut rest) = parse_int(rest.strip_prefix('.')?)?;
    let mut prerelease = "";
    let mut build = "";
    if rest.starts_with('-') {
        (prerelease, rest) = parse_prerelease(rest)?;
    }
    if rest.starts_with('+') {
        (build, rest) = parse_build(rest)?;
    }
    if !rest.is_empty() {
        return None;
    }
    Some(GoSemver {
        major,
        minor,
        patch,
        short: "",
        prerelease,
        build,
    })
}

/// A semver numeric identifier: digits with no leading zero (except "0").
fn parse_int(v: &str) -> Option<(&str, &str)> {
    let bytes = v.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() {
        return None;
    }
    let mut i = 1;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if bytes[0] == b'0' && i != 1 {
        return None;
    }
    Some((&v[..i], &v[i..]))
}

fn parse_prerelease(v: &str) -> Option<(&str, &str)> {
    let bytes = v.as_bytes();
    if bytes.is_empty() || bytes[0] != b'-' {
        return None;
    }
    let (mut i, mut start) = (1, 1);
    while i < bytes.len() && bytes[i] != b'+' {
        if !is_ident_char(bytes[i]) && bytes[i] != b'.' {
            return None;
        }
        if bytes[i] == b'.' {
            if start == i || is_bad_num(&v[start..i]) {
                return None;
            }
            start = i + 1;
        }
        i += 1;
    }
    if start == i || is_bad_num(&v[start..i]) {
        return None;
    }
    Some((&v[..i], &v[i..]))
}

/// Leading zeroes are allowed in build metadata.
fn parse_build(v: &str) -> Option<(&str, &str)> {
    let bytes = v.as_bytes();
    if bytes.is_empty() || bytes[0] != b'+' {
        return None;
    }
    let (mut i, mut start) = (1, 1);
    while i < bytes.len() {
        if !is_ident_char(bytes[i]) && bytes[i] != b'.' {
            return None;
        }
        if bytes[i] == b'.' {
            if start == i {
                return None;
            }
            start = i + 1;
        }
        i += 1;
    }
    if start == i {
        return None;
    }
    Some((&v[..i], &v[i..]))
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-'
}

/// A numeric identifier with a leading zero, which semver forbids.
fn is_bad_num(v: &str) -> bool {
    is_num(v) && v.len() > 1 && v.starts_with('0')
}

fn is_num(v: &str) -> bool {
    v.bytes().all(|b| b.is_ascii_digit())
}

/// Invalid versions sort before valid ones and compare equal to each other.
pub(crate) fn go_semver_compare(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (pa, pb) = (go_semver_parse(a), go_semver_parse(b));
    let (Some(pa), Some(pb)) = (pa, pb) else {
        return go_semver_valid(a).cmp(&go_semver_valid(b));
    };
    for (x, y) in [
        (pa.major, pb.major),
        (pa.minor, pb.minor),
        (pa.patch, pb.patch),
    ] {
        match compare_int(x, y) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    compare_prerelease(pa.prerelease, pb.prerelease)
}

fn compare_int(x: &str, y: &str) -> std::cmp::Ordering {
    x.len().cmp(&y.len()).then_with(|| x.cmp(y))
}

/// Both arguments carry their leading `-`.
fn compare_prerelease(x: &str, y: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if x == y {
        return Ordering::Equal;
    }
    if x.is_empty() {
        return Ordering::Greater;
    }
    if y.is_empty() {
        return Ordering::Less;
    }
    // Both strings still hold their separator, so each round drops one `-` or
    // `.` and then takes the identifier up to the next dot.
    let (mut x, mut y) = (&x[1..], &y[1..]);
    loop {
        let (dx, restx) = next_ident(x);
        let (dy, resty) = next_ident(y);
        if dx != dy {
            let (ix, iy) = (is_num(dx), is_num(dy));
            if ix != iy {
                return if ix {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            if ix {
                match dx.len().cmp(&dy.len()) {
                    Ordering::Equal => {}
                    other => return other,
                }
            }
            return dx.cmp(dy);
        }
        match (restx.is_empty(), resty.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            (false, false) => (x, y) = (&restx[1..], &resty[1..]),
        }
    }
}

fn next_ident(x: &str) -> (&str, &str) {
    match x.find('.') {
        Some(i) => (&x[..i], &x[i..]),
        None => (x, ""),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::repo::group_metadata::{
        etag_matches, group_metadata_representation, merge_cargo_group, merge_go_list_group,
        merge_maven_group, merge_npm_group, merge_pypi_group,
    };
    use crate::repo::pypi::PYPI_JSON_TYPE;

    #[test]
    fn group_metadata_representation_separates_accept_and_checksums() {
        assert_eq!(
            group_metadata_representation("pypi", "simple/widget/", PYPI_JSON_TYPE),
            "pypi:json"
        );
        assert_eq!(
            group_metadata_representation("pypi", "simple/widget/", "text/html"),
            "pypi:default"
        );
        assert_eq!(
            group_metadata_representation("maven", "a/maven-metadata.xml.sha256", ""),
            "maven:sha256"
        );
        assert!(
            etag_matches(r#"W/"other", "digest""#, "digest"),
            "strong matching ETag was not recognized"
        );
    }

    #[test]
    fn merge_maven_group_unions_versions() {
        let (body, _) = merge_maven_group(&[
        br#"<metadata><groupId>com.acme</groupId><artifactId>widget</artifactId><versioning><versions><version>1.0.0</version></versions></versioning></metadata>"#,
        br#"<metadata><groupId>com.acme</groupId><artifactId>widget</artifactId><versioning><versions><version>2.0.0</version></versions></versioning></metadata>"#,
    ])
    .expect("merge");
        let text = String::from_utf8_lossy(&body).to_string();
        assert!(text.contains("1.0.0"), "body={text}");
        assert!(text.contains("2.0.0"), "body={text}");
    }

    #[test]
    fn merge_npm_group_uses_member_priority_and_union() {
        let (body, _) = merge_npm_group(&[
        br#"{"name":"widget","versions":{"1.0.0":{"source":"hosted"}},"dist-tags":{"latest":"1.0.0"},"time":{}}"#,
        br#"{"name":"widget","versions":{"1.0.0":{"source":"proxy"},"2.0.0":{}},"dist-tags":{"latest":"2.0.0","next":"2.0.0"},"time":{}}"#,
    ])
    .expect("merge");
        let document: serde_json::Value = serde_json::from_slice(&body).expect("decode");
        let versions = document["versions"].as_object().expect("versions");
        assert_eq!(versions.len(), 2, "document={document}");
        assert_eq!(versions["1.0.0"]["source"], "hosted", "document={document}");
        assert_eq!(
            document["dist-tags"]["latest"], "1.0.0",
            "document={document}"
        );
    }

    #[test]
    fn merge_cargo_and_go_group_metadata() {
        let (cargo, _) = merge_cargo_group(&[
            b"{\"name\":\"a\",\"vers\":\"1.0.0\"}\n",
            b"{\"name\":\"a\",\"vers\":\"2.0.0\"}\n",
        ])
        .expect("cargo merge");
        let cargo = String::from_utf8_lossy(&cargo).to_string();
        assert!(cargo.contains("1.0.0"), "cargo={cargo}");
        assert!(cargo.contains("2.0.0"), "cargo={cargo}");

        let (versions, _) =
            merge_go_list_group(&[b"v2.0.0\n", b"v1.0.0\nv2.0.0\n"]).expect("go list merge");
        assert_eq!(String::from_utf8_lossy(&versions), "v1.0.0\nv2.0.0\n");
    }

    #[test]
    fn merge_pypi_group_unions_files() {
        let (body, _) = merge_pypi_group(
        &[
            br#"{"meta":{"api-version":"1.0"},"files":[{"filename":"a-1.whl","url":"a"}]}"#,
            br#"{"meta":{"api-version":"1.0"},"files":[{"filename":"a-1.whl","url":"other"},{"filename":"a-1.tar.gz","url":"b"}]}"#,
        ],
        PYPI_JSON_TYPE,
    )
    .expect("merge");
        let document: serde_json::Value = serde_json::from_slice(&body).expect("decode");
        assert_eq!(
            document["files"].as_array().expect("files").len(),
            2,
            "document={document}"
        );
    }

    #[test]
    fn merge_pypi_html_uses_first_member_for_duplicate_filename() {
        let (body, _) = merge_pypi_group(
        &[
            br#"<html><body><a href="hosted/a-1.whl">a-1.whl</a></body></html>"#,
            br#"<html><body><a href="proxy/a-1.whl">a-1.whl</a><a href="proxy/a-1.tar.gz">a-1.tar.gz</a></body></html>"#,
        ],
        "text/html",
    )
    .expect("merge");
        let text = String::from_utf8_lossy(&body).to_string();
        assert_eq!(text.matches(">a-1.whl</a>").count(), 1, "body={text}");
        assert!(!text.contains("proxy/a-1.whl"), "body={text}");
    }
}
