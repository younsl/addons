//! Catalog tools: entity search, lookup, facets and API definitions.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use serde_json::{Map, Value};

use super::{ToolError, ToolResult, json, respond};
use crate::server::BackstageMcp;

pub const TECHDOCS_ANNOTATION: &str = "backstage.io/techdocs-ref";
const SOURCE_ANNOTATION: &str = "backstage.io/source-location";

/// A parsed `kind:namespace/name` reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityName {
    pub kind: String,
    pub namespace: String,
    pub name: String,
}

impl EntityName {
    /// Path segments for the catalog `by-name` endpoint.
    #[must_use]
    pub fn catalog_path(&self) -> String {
        format!(
            "/api/catalog/entities/by-name/{}/{}/{}",
            encode(&self.kind),
            encode(&self.namespace),
            encode(&self.name)
        )
    }
}

fn encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

/// URL-encodes one path segment.
#[must_use]
pub fn encode_segment(segment: &str) -> String {
    encode(segment)
}

/// Parses `kind:namespace/name` or `kind:name` (namespace defaults to `default`).
///
/// # Errors
///
/// Returns an input error when the reference does not have that shape.
pub fn parse_entity_ref(reference: &str) -> Result<EntityName, ToolError> {
    let invalid = || {
        ToolError::Input(format!(
            "invalid entity ref {reference:?}, expected kind:namespace/name (for example component:default/payments-api)"
        ))
    };
    let trimmed = reference.trim();
    let (kind, rest) = trimmed.split_once(':').ok_or_else(invalid)?;
    let (namespace, name) = rest.split_once('/').unwrap_or(("default", rest));
    let valid = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !valid(kind) || !valid(namespace) || !valid(name) {
        return Err(invalid());
    }
    Ok(EntityName {
        kind: kind.to_lowercase(),
        namespace: namespace.to_lowercase(),
        name: name.to_string(),
    })
}

/// The subset of a catalog entity the summaries are built from. Unknown
/// fields are kept so the full record can still be returned untouched.
#[derive(Debug, Clone, Deserialize)]
pub struct Entity {
    pub kind: String,
    pub metadata: Metadata,
    #[serde(default)]
    pub spec: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    pub name: String,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub annotations: Map<String, Value>,
}

impl Entity {
    #[must_use]
    pub fn reference(&self) -> String {
        format!(
            "{}:{}/{}",
            self.kind.to_lowercase(),
            self.metadata.namespace.as_deref().unwrap_or("default"),
            self.metadata.name
        )
    }
}

/// The compact row returned by `catalog_search_entities`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntitySummary {
    pub r#ref: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub has_techdocs: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_location: Option<String>,
}

#[must_use]
pub fn summarize(entity: &Entity) -> EntitySummary {
    let spec_str = |key: &str| {
        entity
            .spec
            .get(key)
            .and_then(Value::as_str)
            .map(String::from)
    };
    EntitySummary {
        r#ref: entity.reference(),
        kind: entity.kind.clone(),
        namespace: entity
            .metadata
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_string()),
        name: entity.metadata.name.clone(),
        title: entity.metadata.title.clone(),
        description: entity.metadata.description.clone(),
        r#type: spec_str("type"),
        owner: spec_str("owner"),
        lifecycle: spec_str("lifecycle"),
        system: spec_str("system"),
        tags: entity.metadata.tags.clone(),
        has_techdocs: entity
            .metadata
            .annotations
            .contains_key(TECHDOCS_ANNOTATION),
        source_location: entity
            .metadata
            .annotations
            .get(SOURCE_ANNOTATION)
            .and_then(Value::as_str)
            .map(String::from),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EntitiesByQuery {
    items: Vec<Value>,
    #[serde(default)]
    total_items: Option<u64>,
    #[serde(default)]
    page_info: Option<PageInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchEntitiesArgs {
    /// Entity kind, case-insensitive: component, api, system, domain, resource, group, user, template, location
    #[serde(default)]
    pub kind: Option<String>,
    /// spec.type value, for example service, library, website, openapi, grpc, team
    #[serde(default)]
    pub r#type: Option<String>,
    /// spec.owner value exactly as written in catalog-info.yaml, for example team-payments or group:default/team-payments
    #[serde(default)]
    pub owner: Option<String>,
    /// spec.lifecycle value, for example production, experimental, deprecated
    #[serde(default)]
    pub lifecycle: Option<String>,
    /// spec.system the entity belongs to
    #[serde(default)]
    pub system: Option<String>,
    /// metadata.namespace, default any namespace
    #[serde(default)]
    pub namespace: Option<String>,
    /// A metadata.tags value the entity must carry
    #[serde(default)]
    pub tag: Option<String>,
    /// Free-text match against name, title and description
    #[serde(default)]
    pub text: Option<String>,
    /// When true, only entities that publish TechDocs
    #[serde(default)]
    pub has_techdocs: Option<bool>,
    /// Page size, default 25, maximum 100
    #[serde(default)]
    pub limit: Option<u32>,
    /// nextCursor from a previous call, to fetch the following page
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EntityRefArgs {
    /// Entity reference as kind:namespace/name, for example component:default/payments-api or api:default/orders-v1
    pub entity_ref: String,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
pub enum Facet {
    #[serde(rename = "kind")]
    Kind,
    #[serde(rename = "spec.type")]
    SpecType,
    #[serde(rename = "spec.owner")]
    SpecOwner,
    #[serde(rename = "spec.lifecycle")]
    SpecLifecycle,
    #[serde(rename = "spec.system")]
    SpecSystem,
    #[serde(rename = "metadata.namespace")]
    MetadataNamespace,
    #[serde(rename = "metadata.tags")]
    MetadataTags,
}

impl Facet {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Kind => "kind",
            Self::SpecType => "spec.type",
            Self::SpecOwner => "spec.owner",
            Self::SpecLifecycle => "spec.lifecycle",
            Self::SpecSystem => "spec.system",
            Self::MetadataNamespace => "metadata.namespace",
            Self::MetadataTags => "metadata.tags",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FacetArgs {
    /// Field to group by
    pub facet: Facet,
    /// Restrict the count to one entity kind, for example component or api
    #[serde(default)]
    pub kind: Option<String>,
}

/// Builds the catalog `filter` expression: comma-joined key=value pairs are
/// ANDed by the catalog API.
#[must_use]
pub fn build_filter(args: &SearchEntitiesArgs) -> Option<String> {
    let mut conditions: Vec<String> = Vec::new();
    let mut push = |key: &str, value: &Option<String>| {
        if let Some(value) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            conditions.push(format!("{key}={value}"));
        }
    };
    push("kind", &args.kind.as_ref().map(|k| k.to_lowercase()));
    push("spec.type", &args.r#type);
    push("spec.owner", &args.owner);
    push("spec.lifecycle", &args.lifecycle);
    push("spec.system", &args.system);
    push("metadata.namespace", &args.namespace);
    push("metadata.tags", &args.tag);
    if args.has_techdocs == Some(true) {
        conditions.push(format!("metadata.annotations.{TECHDOCS_ANNOTATION}"));
    }
    if conditions.is_empty() {
        None
    } else {
        Some(conditions.join(","))
    }
}

impl BackstageMcp {
    async fn search_entities(&self, args: SearchEntitiesArgs) -> ToolResult {
        let mut query = vec![
            ("limit", args.limit.unwrap_or(25).clamp(1, 100).to_string()),
            ("orderField", "metadata.name,asc".to_string()),
        ];
        if let Some(filter) = build_filter(&args) {
            query.push(("filter", filter));
        }
        if let Some(text) = args
            .text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            query.push(("fullTextFilter[term]", text.to_string()));
            for field in ["metadata.name", "metadata.title", "metadata.description"] {
                query.push(("fullTextFilter[fields]", field.to_string()));
            }
        }
        if let Some(cursor) = args.cursor.filter(|c| !c.is_empty()) {
            query.push(("cursor", cursor));
        }
        let response: EntitiesByQuery = self
            .client
            .get_json("/api/catalog/entities/by-query", &query)
            .await?;
        let items: Vec<EntitySummary> = response
            .items
            .iter()
            .filter_map(|raw| serde_json::from_value::<Entity>(raw.clone()).ok())
            .map(|entity| summarize(&entity))
            .collect();
        json(serde_json::json!({
            "totalItems": response.total_items,
            "nextCursor": response.page_info.and_then(|p| p.next_cursor),
            "items": items,
        }))
    }

    async fn get_entity(&self, args: EntityRefArgs) -> ToolResult {
        let name = parse_entity_ref(&args.entity_ref)?;
        let entity: Value = self.client.get_json(&name.catalog_path(), &[]).await?;
        Ok(entity)
    }

    async fn list_facets(&self, args: FacetArgs) -> ToolResult {
        let mut query = vec![("facet", args.facet.as_str().to_string())];
        if let Some(kind) = args
            .kind
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        {
            query.push(("filter", format!("kind={}", kind.to_lowercase())));
        }
        let response: Value = self
            .client
            .get_json("/api/catalog/entity-facets", &query)
            .await?;
        let mut values: Vec<Value> = response
            .get("facets")
            .and_then(|f| f.get(args.facet.as_str()))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        values.sort_by(|a, b| {
            b.get("count")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .cmp(&a.get("count").and_then(Value::as_u64).unwrap_or(0))
        });
        json(serde_json::json!({ "facet": args.facet.as_str(), "values": values }))
    }

    async fn get_api_definition(&self, args: EntityRefArgs) -> ToolResult {
        let name = parse_entity_ref(&args.entity_ref)?;
        if name.kind != "api" {
            return Err(ToolError::Input(format!(
                "entityRef must point at an API entity, got kind {:?}",
                name.kind
            )));
        }
        let entity: Entity = self.client.get_json(&name.catalog_path(), &[]).await?;
        let definition = match entity.spec.get("definition") {
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        let spec_str = |key: &str| entity.spec.get(key).and_then(Value::as_str);
        json(serde_json::json!({
            "ref": entity.reference(),
            "type": spec_str("type"),
            "owner": spec_str("owner"),
            "lifecycle": spec_str("lifecycle"),
            "definition": definition,
        }))
    }
}

#[tool_router(router = catalog_router, vis = "pub(crate)")]
impl BackstageMcp {
    #[tool(
        name = "catalog_search_entities",
        description = "List Backstage catalog entities (Component, API, System, Domain, Resource, Group, User, Template, Location) with optional filters. Returns one summary per entity plus a cursor for the next page. Use catalog_get_entity for the full record and catalog_list_facets to discover valid filter values.",
        annotations(
            title = "Search catalog entities",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn catalog_search_entities(
        &self,
        Parameters(args): Parameters<SearchEntitiesArgs>,
    ) -> CallToolResult {
        respond(
            "catalog_search_entities",
            self.search_entities(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "catalog_get_entity",
        description = "Fetch the full catalog record of one entity by reference, including metadata, annotations, spec, links and relations (ownedBy, dependsOn, providesApi, consumesApi, partOf, hasPart).",
        annotations(
            title = "Get catalog entity",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn catalog_get_entity(
        &self,
        Parameters(args): Parameters<EntityRefArgs>,
    ) -> CallToolResult {
        respond(
            "catalog_get_entity",
            self.get_entity(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "catalog_list_facets",
        description = "Count catalog entities grouped by one field, to discover what kinds, types, owners, lifecycles or tags exist before filtering. Returns value and count pairs sorted by count.",
        annotations(
            title = "List catalog facet values",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn catalog_list_facets(&self, Parameters(args): Parameters<FacetArgs>) -> CallToolResult {
        respond(
            "catalog_list_facets",
            self.list_facets(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "catalog_get_api_definition",
        description = "Return the API specification document (OpenAPI, AsyncAPI, GraphQL schema or gRPC proto) stored in spec.definition of an API entity, as shown on the APIs page. Use catalog_search_entities with kind=api to find API entities.",
        annotations(
            title = "Get API definition",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn catalog_get_api_definition(
        &self,
        Parameters(args): Parameters<EntityRefArgs>,
    ) -> CallToolResult {
        respond(
            "catalog_get_api_definition",
            self.get_api_definition(args).await,
            self.max_result_chars,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testing::{json as result_json, mcp, text};
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn component(name: &str) -> Value {
        json!({
            "apiVersion": "backstage.io/v1alpha1",
            "kind": "Component",
            "metadata": {
                "name": name,
                "namespace": "default",
                "title": "Payments API",
                "tags": ["java"],
                "annotations": {
                    "backstage.io/techdocs-ref": "dir:.",
                    "backstage.io/source-location": "url:https://git.example.com/p"
                }
            },
            "spec": {"type": "service", "owner": "team-payments", "lifecycle": "production", "system": "payments"}
        })
    }

    async fn mount_query(server: &MockServer, filter: &str) {
        Mock::given(method("GET"))
            .and(path("/api/catalog/entities/by-query"))
            .and(query_param("filter", filter))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [component("payments-api"), {"kind": "Broken"}],
                "totalItems": 1,
                "pageInfo": {"nextCursor": "abc"}
            })))
            .mount(server)
            .await;
    }

    #[test]
    fn entity_refs() {
        let name = parse_entity_ref("Component:default/payments-api").unwrap();
        assert_eq!(name.kind, "component");
        assert_eq!(
            name.catalog_path(),
            "/api/catalog/entities/by-name/component/default/payments-api"
        );
        assert_eq!(parse_entity_ref("api:orders").unwrap().namespace, "default");
        assert!(parse_entity_ref("payments-api").is_err());
        assert!(parse_entity_ref("component:default/").is_err());
        assert!(parse_entity_ref("component:a b").is_err());
        assert_eq!(encode_segment("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn filter_expression() {
        let args = SearchEntitiesArgs {
            kind: Some("Component".into()),
            r#type: Some("service".into()),
            owner: None,
            lifecycle: Some(" ".into()),
            system: None,
            namespace: None,
            tag: Some("java".into()),
            text: None,
            has_techdocs: Some(true),
            limit: None,
            cursor: None,
        };
        assert_eq!(
            build_filter(&args).unwrap(),
            "kind=component,spec.type=service,metadata.tags=java,metadata.annotations.backstage.io/techdocs-ref"
        );
        let empty = SearchEntitiesArgs {
            kind: None,
            r#type: None,
            owner: None,
            lifecycle: None,
            system: None,
            namespace: None,
            tag: None,
            text: None,
            has_techdocs: Some(false),
            limit: None,
            cursor: None,
        };
        assert_eq!(build_filter(&empty), None);
    }

    #[tokio::test]
    async fn search_summarizes_entities() {
        let (server, handler) = mcp().await;
        mount_query(&server, "kind=component,spec.owner=team-payments").await;
        let result = handler
            .catalog_search_entities(Parameters(SearchEntitiesArgs {
                kind: Some("component".into()),
                r#type: None,
                owner: Some("team-payments".into()),
                lifecycle: None,
                system: None,
                namespace: None,
                tag: None,
                text: Some("pay".into()),
                has_techdocs: None,
                limit: Some(500),
                cursor: Some("c1".into()),
            }))
            .await;
        let value = result_json(&result);
        assert_eq!(value["totalItems"], 1);
        assert_eq!(value["nextCursor"], "abc");
        assert_eq!(value["items"].as_array().unwrap().len(), 1);
        let item = &value["items"][0];
        assert_eq!(item["ref"], "component:default/payments-api");
        assert_eq!(item["owner"], "team-payments");
        assert_eq!(item["hasTechdocs"], true);
        assert_eq!(item["sourceLocation"], "url:https://git.example.com/p");
        assert_eq!(item["tags"][0], "java");
    }

    #[tokio::test]
    async fn get_entity_and_definition() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path(
                "/api/catalog/entities/by-name/component/default/payments-api",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(component("payments-api")))
            .mount(&server)
            .await;
        let mut api = component("orders");
        api["kind"] = json!("API");
        api["spec"] = json!({"type": "openapi", "owner": "team", "definition": "openapi: 3.0.0"});
        Mock::given(method("GET"))
            .and(path("/api/catalog/entities/by-name/api/default/orders"))
            .respond_with(ResponseTemplate::new(200).set_body_json(api))
            .mount(&server)
            .await;

        let entity = result_json(
            &handler
                .catalog_get_entity(Parameters(EntityRefArgs {
                    entity_ref: "component:default/payments-api".into(),
                }))
                .await,
        );
        assert_eq!(entity["metadata"]["name"], "payments-api");

        let definition = result_json(
            &handler
                .catalog_get_api_definition(Parameters(EntityRefArgs {
                    entity_ref: "api:default/orders".into(),
                }))
                .await,
        );
        assert_eq!(definition["definition"], "openapi: 3.0.0");
        assert_eq!(definition["type"], "openapi");

        let wrong = handler
            .catalog_get_api_definition(Parameters(EntityRefArgs {
                entity_ref: "component:default/payments-api".into(),
            }))
            .await;
        assert_eq!(wrong.is_error, Some(true));
        assert!(text(&wrong).contains("API entity"));

        let missing = handler
            .catalog_get_entity(Parameters(EntityRefArgs {
                entity_ref: "component:default/nope".into(),
            }))
            .await;
        assert_eq!(missing.is_error, Some(true));
        assert!(text(&missing).contains("404"));

        let invalid = handler
            .catalog_get_entity(Parameters(EntityRefArgs {
                entity_ref: "nope".into(),
            }))
            .await;
        assert!(text(&invalid).contains("invalid entity ref"));
    }

    #[tokio::test]
    async fn facets_sorted_by_count() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/catalog/entity-facets"))
            .and(query_param("facet", "spec.owner"))
            .and(query_param("filter", "kind=component"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "facets": {"spec.owner": [{"value": "a", "count": 1}, {"value": "b", "count": 7}]}
            })))
            .mount(&server)
            .await;
        let value = result_json(
            &handler
                .catalog_list_facets(Parameters(FacetArgs {
                    facet: Facet::SpecOwner,
                    kind: Some("Component".into()),
                }))
                .await,
        );
        assert_eq!(value["values"][0]["value"], "b");
        assert_eq!(value["facet"], "spec.owner");
    }
}
