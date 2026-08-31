//! API Registry: external OpenAPI specs registered by URL.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::catalog::encode_segment;
use super::{Paging, ToolResult, json, matches_eq, matches_text, page, respond, str_field};
use crate::server::BackstageMcp;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListRegistrationsArgs {
    /// Substring match on name, title, description, owner or spec URL
    #[serde(default)]
    pub text: Option<String>,
    /// Exact owner value
    #[serde(default)]
    pub owner: Option<String>,
    /// Exact lifecycle value
    #[serde(default)]
    pub lifecycle: Option<String>,
    #[serde(flatten)]
    pub paging: Paging,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RegistrationIdArgs {
    /// Registration id from openapi_registry_list_registrations
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RegistrationNameArgs {
    /// Registration name (the API entity name)
    pub name: String,
}

impl BackstageMcp {
    async fn list_registrations(&self, args: ListRegistrationsArgs) -> ToolResult {
        let rows: Vec<Value> = self
            .client
            .get_json("/api/openapi-registry/registrations", &[])
            .await?;
        let filtered: Vec<Value> = rows
            .into_iter()
            .filter(|row| {
                matches_eq(args.owner.as_deref(), str_field(row, "owner").unwrap_or(""))
                    && matches_eq(
                        args.lifecycle.as_deref(),
                        str_field(row, "lifecycle").unwrap_or(""),
                    )
                    && matches_text(
                        args.text.as_deref(),
                        &[
                            str_field(row, "name"),
                            str_field(row, "title"),
                            str_field(row, "description"),
                            str_field(row, "owner"),
                            str_field(row, "specUrl"),
                        ],
                    )
            })
            .collect();
        json(page(filtered, args.paging, 50))
    }

    async fn get_registration(&self, args: RegistrationIdArgs) -> ToolResult {
        let row: Value = self
            .client
            .get_json(
                &format!(
                    "/api/openapi-registry/registrations/{}",
                    encode_segment(&args.id)
                ),
                &[],
            )
            .await?;
        Ok(row)
    }

    async fn get_entity_yaml(&self, args: RegistrationNameArgs) -> ToolResult {
        let yaml = self
            .client
            .get_text(
                &format!(
                    "/api/openapi-registry/entity/{}",
                    encode_segment(&args.name)
                ),
                &[],
                "application/x-yaml, text/yaml, */*",
            )
            .await?;
        Ok(Value::String(yaml))
    }
}

#[tool_router(router = openapi_registry_router, vis = "pub(crate)")]
impl BackstageMcp {
    #[tool(
        name = "openapi_registry_list_registrations",
        description = "List external OpenAPI specs registered by URL through the API Registry page. Each row carries the spec URL, the catalog API entity it produced, owner, lifecycle, tags and the last sync time.",
        annotations(
            title = "List registered OpenAPI specs",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn openapi_registry_list_registrations(
        &self,
        Parameters(args): Parameters<ListRegistrationsArgs>,
    ) -> CallToolResult {
        respond(
            "openapi_registry_list_registrations",
            self.list_registrations(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "openapi_registry_get_registration",
        description = "Fetch one API Registry registration by its id.",
        annotations(
            title = "Get one OpenAPI registration",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn openapi_registry_get_registration(
        &self,
        Parameters(args): Parameters<RegistrationIdArgs>,
    ) -> CallToolResult {
        respond(
            "openapi_registry_get_registration",
            self.get_registration(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "openapi_registry_get_entity_yaml",
        description = "Return the catalog-info YAML the API Registry generated for a registered spec, by the registration name. The OpenAPI document itself is available through catalog_get_api_definition on the resulting API entity.",
        annotations(
            title = "Get generated API entity YAML",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn openapi_registry_get_entity_yaml(
        &self,
        Parameters(args): Parameters<RegistrationNameArgs>,
    ) -> CallToolResult {
        respond(
            "openapi_registry_get_entity_yaml",
            self.get_entity_yaml(args).await,
            self.max_result_chars,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testing::{json as result_json, mcp, text};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    #[tokio::test]
    async fn registry_tools() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/openapi-registry/registrations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": "1", "name": "orders", "owner": "team-a", "lifecycle": "production", "specUrl": "https://x/orders.yaml"},
                {"id": "2", "name": "payments", "owner": "team-b", "lifecycle": "production", "title": "Pay API"}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/openapi-registry/registrations/2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "2"})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/openapi-registry/entity/orders"))
            .respond_with(ResponseTemplate::new(200).set_body_string("kind: API\n"))
            .mount(&server)
            .await;

        let list = result_json(
            &handler
                .openapi_registry_list_registrations(Parameters(ListRegistrationsArgs {
                    text: Some("pay".into()),
                    owner: None,
                    lifecycle: Some("production".into()),
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(list["total"], 1);
        assert_eq!(list["items"][0]["id"], "2");

        let owner = result_json(
            &handler
                .openapi_registry_list_registrations(Parameters(ListRegistrationsArgs {
                    text: None,
                    owner: Some("team-a".into()),
                    lifecycle: None,
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(owner["items"][0]["name"], "orders");

        let one = result_json(
            &handler
                .openapi_registry_get_registration(Parameters(RegistrationIdArgs {
                    id: "2".into(),
                }))
                .await,
        );
        assert_eq!(one["id"], "2");

        let yaml = handler
            .openapi_registry_get_entity_yaml(Parameters(RegistrationNameArgs {
                name: "orders".into(),
            }))
            .await;
        assert_eq!(text(&yaml), "kind: API\n");
    }
}
