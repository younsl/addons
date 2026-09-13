//! Full-text search across the Backstage search index.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ToolResult, respond, str_field};
use crate::server::BackstageMcp;

const SNIPPET_CHARS: usize = 400;

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
pub enum SearchType {
    #[serde(rename = "software-catalog")]
    SoftwareCatalog,
    #[serde(rename = "techdocs")]
    Techdocs,
}

impl SearchType {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SoftwareCatalog => "software-catalog",
            Self::Techdocs => "techdocs",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Search terms
    pub term: String,
    /// Restrict to one or more index types, default all
    #[serde(default)]
    pub types: Option<Vec<SearchType>>,
    /// Page size, default 25, maximum 100
    #[serde(default)]
    pub limit: Option<u32>,
    /// nextCursor from a previous call
    #[serde(default)]
    pub cursor: Option<String>,
}

fn snippet(result: &Value) -> Option<String> {
    let highlighted = result
        .pointer("/highlight/fields/text")
        .and_then(Value::as_str);
    let text = highlighted.or_else(|| result.pointer("/document/text").and_then(Value::as_str))?;
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > SNIPPET_CHARS {
        Some(format!(
            "{}...",
            flat.chars().take(SNIPPET_CHARS).collect::<String>()
        ))
    } else {
        Some(flat)
    }
}

fn entity_ref(document: &Value) -> Option<String> {
    if let Some(reference) = str_field(document, "entityRef") {
        return Some(reference.to_string());
    }
    let kind = str_field(document, "kind")?;
    let name = str_field(document, "name")?;
    let namespace = str_field(document, "namespace").unwrap_or("default");
    Some(format!("{}:{namespace}/{name}", kind.to_lowercase()))
}

/// Flattens one search hit into the fields an agent needs to follow up.
#[must_use]
pub fn summarize_hit(result: &Value) -> Value {
    let document = result.get("document").cloned().unwrap_or(Value::Null);
    let hit_type = str_field(result, "type").unwrap_or("");
    json!({
        "type": hit_type,
        "title": str_field(&document, "title"),
        "entityRef": entity_ref(&document),
        "location": str_field(&document, "location"),
        "techdocsPath": if hit_type == "techdocs" { str_field(&document, "path") } else { None },
        "snippet": snippet(result),
    })
}

impl BackstageMcp {
    async fn query(&self, args: SearchArgs) -> ToolResult {
        let mut query = vec![
            ("term", args.term.trim().to_string()),
            (
                "pageLimit",
                args.limit.unwrap_or(25).clamp(1, 100).to_string(),
            ),
        ];
        for search_type in args.types.unwrap_or_default() {
            query.push(("types[]", search_type.as_str().to_string()));
        }
        if let Some(cursor) = args.cursor.filter(|c| !c.is_empty()) {
            query.push(("pageCursor", cursor));
        }
        let response: Value = self.client.get_json("/api/search/query", &query).await?;
        let results: Vec<Value> = response
            .get("results")
            .and_then(Value::as_array)
            .map(|hits| hits.iter().map(summarize_hit).collect())
            .unwrap_or_default();
        Ok(json!({
            "numberOfResults": response.get("numberOfResults"),
            "nextCursor": str_field(&response, "nextPageCursor"),
            "results": results,
        }))
    }
}

#[tool_router(router = search_router, vis = "pub(crate)")]
impl BackstageMcp {
    #[tool(
        name = "search_query",
        description = "Full-text search across the Backstage search index: catalog entities (software-catalog) and TechDocs pages (techdocs). Returns ranked hits with a text snippet, the entity reference and, for TechDocs, the page path to pass to techdocs_get_page.",
        annotations(
            title = "Full-text search",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn search_query(&self, Parameters(args): Parameters<SearchArgs>) -> CallToolResult {
        respond(
            "search_query",
            self.query(args).await,
            self.max_result_chars,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testing::{json as result_json, mcp};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn hits_are_flattened() {
        let (server, handler): (MockServer, BackstageMcp) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/search/query"))
            .and(query_param("term", "deploy"))
            .and(query_param("types[]", "techdocs"))
            .and(query_param("pageCursor", "p2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [
                    {"type": "techdocs", "document": {"title": "Deploy", "text": "  long   text ", "location": "/docs/x", "kind": "Component", "namespace": "default", "name": "x", "path": "guide/"}},
                    {"type": "software-catalog", "document": {"title": "X", "entityRef": "component:default/x"}, "highlight": {"fields": {"text": "<b>hit</b>"}}}
                ],
                "nextPageCursor": "p3",
                "numberOfResults": 2
            })))
            .mount(&server)
            .await;
        let value = result_json(
            &handler
                .search_query(Parameters(SearchArgs {
                    term: " deploy ".into(),
                    types: Some(vec![SearchType::Techdocs]),
                    limit: Some(0),
                    cursor: Some("p2".into()),
                }))
                .await,
        );
        assert_eq!(value["nextCursor"], "p3");
        assert_eq!(value["numberOfResults"], 2);
        assert_eq!(value["results"][0]["entityRef"], "component:default/x");
        assert_eq!(value["results"][0]["techdocsPath"], "guide/");
        assert_eq!(value["results"][0]["snippet"], "long text");
        assert_eq!(value["results"][1]["snippet"], "<b>hit</b>");
        assert!(value["results"][1]["techdocsPath"].is_null());
    }

    #[test]
    fn snippet_is_bounded() {
        let long = "w ".repeat(1000);
        let hit = json!({"type": "techdocs", "document": {"text": long}});
        let summary = summarize_hit(&hit);
        let snippet = summary["snippet"].as_str().unwrap();
        assert!(snippet.ends_with("..."));
        assert!(snippet.chars().count() <= SNIPPET_CHARS + 3);
        assert!(summarize_hit(&json!({"type": "x"}))["snippet"].is_null());
    }
}
