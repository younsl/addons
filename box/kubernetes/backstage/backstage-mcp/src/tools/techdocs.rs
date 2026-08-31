//! TechDocs tools: site metadata and page content as text.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::catalog::{EntityName, encode_segment, parse_entity_ref};
use super::{ToolError, ToolResult, respond};
use crate::html;
use crate::server::BackstageMcp;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MetadataArgs {
    /// Entity that owns the docs, as kind:namespace/name, for example component:default/payments-api
    pub entity_ref: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageArgs {
    /// Entity that owns the docs, as kind:namespace/name, for example component:default/payments-api
    pub entity_ref: String,
    /// Page path relative to the docs root, default index.html. Directories resolve to their index.html.
    #[serde(default)]
    pub path: Option<String>,
}

/// TechDocs stores sites under `namespace/kind/name`, unlike the catalog.
fn docs_segments(name: &EntityName) -> String {
    format!(
        "{}/{}/{}",
        encode_segment(&name.namespace),
        encode_segment(&name.kind),
        encode_segment(&name.name)
    )
}

/// Normalizes a docs path: empty and `/` mean the site root, directories get
/// `index.html`, and traversal is refused.
///
/// # Errors
///
/// Returns an input error when the path contains `..`.
pub fn normalize_path(path: Option<&str>) -> Result<String, ToolError> {
    let mut clean = path
        .unwrap_or("")
        .trim()
        .trim_start_matches('/')
        .to_string();
    if clean.split('/').any(|segment| segment == "..") {
        return Err(ToolError::Input("path must not contain \"..\"".to_string()));
    }
    let has_extension = clean
        .rsplit('/')
        .next()
        .is_some_and(|last| last.contains('.'));
    if clean.is_empty() || clean.ends_with('/') {
        clean.push_str("index.html");
    } else if !has_extension {
        clean.push_str("/index.html");
    }
    Ok(clean)
}

impl BackstageMcp {
    async fn metadata(&self, args: MetadataArgs) -> ToolResult {
        let name = parse_entity_ref(&args.entity_ref)?;
        let metadata: Value = self
            .client
            .get_json(
                &format!("/api/techdocs/metadata/techdocs/{}", docs_segments(&name)),
                &[],
            )
            .await?;
        let pages: Vec<&str> = metadata
            .get("files")
            .and_then(Value::as_array)
            .map(|files| {
                files
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|file| {
                        std::path::Path::new(file)
                            .extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("html"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let built = metadata
            .get("build_timestamp")
            .and_then(Value::as_i64)
            .map(|seconds| {
                std::time::UNIX_EPOCH
                    .checked_add(std::time::Duration::from_secs(seconds.unsigned_abs()))
                    .map_or_else(|| seconds.to_string(), humantime_iso)
            });
        Ok(json!({
            "entityRef": args.entity_ref,
            "siteName": metadata.get("site_name"),
            "siteDescription": metadata.get("site_description"),
            "buildTimestamp": built,
            "pages": pages,
        }))
    }

    async fn page(&self, args: PageArgs) -> ToolResult {
        let name = parse_entity_ref(&args.entity_ref)?;
        let path = normalize_path(args.path.as_deref())?;
        let body = self
            .client
            .get_text(
                &format!("/api/techdocs/static/docs/{}/{path}", docs_segments(&name)),
                &[],
                "text/html, */*",
            )
            .await?;
        Ok(json!({
            "entityRef": args.entity_ref,
            "path": path,
            "title": html::title(&body),
            "text": html::to_text(&body),
        }))
    }
}

/// Formats a system time as an RFC 3339 UTC timestamp without pulling in a
/// date crate.
fn humantime_iso(time: std::time::SystemTime) -> String {
    let secs = time
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days (Howard Hinnant), valid for the Unix era.
    let z = i64::try_from(days).unwrap_or(i64::MAX / 2) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[tool_router(router = techdocs_router, vis = "pub(crate)")]
impl BackstageMcp {
    #[tool(
        name = "techdocs_get_metadata",
        description = "Return the built TechDocs site metadata for an entity: site name, description, build timestamp and the list of page files that can be passed to techdocs_get_page. Use catalog_search_entities with has_techdocs=true to find entities that publish docs.",
        annotations(
            title = "Get TechDocs site metadata",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn techdocs_get_metadata(
        &self,
        Parameters(args): Parameters<MetadataArgs>,
    ) -> CallToolResult {
        respond(
            "techdocs_get_metadata",
            self.metadata(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "techdocs_get_page",
        description = "Fetch one TechDocs page of an entity and return its content as plain text with Markdown-style headings and fenced code blocks. path is relative to the docs site root: omit it for the front page, or pass a value such as guides/deploy/ or guides/deploy/index.html from search_query or techdocs_get_metadata.",
        annotations(
            title = "Read a TechDocs page",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn techdocs_get_page(&self, Parameters(args): Parameters<PageArgs>) -> CallToolResult {
        respond(
            "techdocs_get_page",
            self.page(args).await,
            self.max_result_chars,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testing::{json as result_json, mcp, text};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    #[test]
    fn path_normalization() {
        assert_eq!(normalize_path(None).unwrap(), "index.html");
        assert_eq!(normalize_path(Some("/")).unwrap(), "index.html");
        assert_eq!(
            normalize_path(Some("guides/deploy/")).unwrap(),
            "guides/deploy/index.html"
        );
        assert_eq!(
            normalize_path(Some("guides/deploy")).unwrap(),
            "guides/deploy/index.html"
        );
        assert_eq!(
            normalize_path(Some("/guides/deploy/index.html")).unwrap(),
            "guides/deploy/index.html"
        );
        assert_eq!(
            normalize_path(Some("assets/img.png")).unwrap(),
            "assets/img.png"
        );
        assert!(normalize_path(Some("../etc/passwd")).is_err());
    }

    #[test]
    fn iso_timestamps() {
        assert_eq!(humantime_iso(std::time::UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_756_600_000);
        assert_eq!(humantime_iso(t), "2025-08-31T00:26:40Z");
    }

    #[tokio::test]
    async fn metadata_and_page() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path(
                "/api/techdocs/metadata/techdocs/default/component/payments-api",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "site_name": "Payments", "build_timestamp": 0,
                "files": ["index.html", "guide/index.html", "assets/x.css"]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/techdocs/static/docs/default/component/payments-api/guide/index.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "<html><head><title>Guide</title></head><body><article><h1>Guide</h1><p>Hello</p></article></body></html>",
            ))
            .mount(&server)
            .await;

        let meta = result_json(
            &handler
                .techdocs_get_metadata(Parameters(MetadataArgs {
                    entity_ref: "component:default/payments-api".into(),
                }))
                .await,
        );
        assert_eq!(meta["siteName"], "Payments");
        assert_eq!(meta["buildTimestamp"], "1970-01-01T00:00:00Z");
        assert_eq!(meta["pages"], json!(["index.html", "guide/index.html"]));

        let page = result_json(
            &handler
                .techdocs_get_page(Parameters(PageArgs {
                    entity_ref: "component:default/payments-api".into(),
                    path: Some("guide/".into()),
                }))
                .await,
        );
        assert_eq!(page["title"], "Guide");
        assert_eq!(page["text"], "# Guide\n\nHello");
        assert_eq!(page["path"], "guide/index.html");

        let bad = handler
            .techdocs_get_page(Parameters(PageArgs {
                entity_ref: "component:default/payments-api".into(),
                path: Some("../x".into()),
            }))
            .await;
        assert!(text(&bad).contains(".."));
    }
}
