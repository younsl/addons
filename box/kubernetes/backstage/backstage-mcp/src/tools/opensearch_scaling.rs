//! Capacity: Amazon OpenSearch Service domains and reserved scaling requests.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::catalog::encode_segment;
use super::{Paging, ToolResult, json, matches_eq, page, respond, sort_newest_first, str_field};
use crate::server::BackstageMcp;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DomainArgs {
    /// Domain name
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListRequestsArgs {
    /// Exact status, for example scheduled, in_progress, completed, failed, cancelled
    #[serde(default)]
    pub status: Option<String>,
    /// Exact domain name
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(flatten)]
    pub paging: Paging,
}

fn domain_of(row: &Value) -> &str {
    str_field(row, "domain")
        .or_else(|| str_field(row, "domainName"))
        .unwrap_or("")
}

impl BackstageMcp {
    async fn scaling_config(&self) -> ToolResult {
        Ok(self
            .client
            .get_json("/api/opensearch-scaling/config", &[])
            .await?)
    }

    async fn list_domains(&self) -> ToolResult {
        Ok(self
            .client
            .get_json("/api/opensearch-scaling/domains", &[])
            .await?)
    }

    async fn get_domain(&self, args: DomainArgs) -> ToolResult {
        Ok(self
            .client
            .get_json(
                &format!(
                    "/api/opensearch-scaling/domains/{}",
                    encode_segment(&args.name)
                ),
                &[],
            )
            .await?)
    }

    async fn list_scaling_requests(&self, args: ListRequestsArgs) -> ToolResult {
        let rows: Vec<Value> = self
            .client
            .get_json("/api/opensearch-scaling/requests", &[])
            .await?;
        let mut filtered: Vec<Value> = rows
            .into_iter()
            .filter(|row| {
                matches_eq(
                    args.status.as_deref(),
                    str_field(row, "status").unwrap_or(""),
                ) && matches_eq(args.domain.as_deref(), domain_of(row))
            })
            .collect();
        sort_newest_first(&mut filtered, "createdAt");
        json(page(filtered, args.paging, 50))
    }
}

#[tool_router(router = opensearch_scaling_router, vis = "pub(crate)")]
impl BackstageMcp {
    #[tool(
        name = "opensearch_scaling_get_config",
        description = "Whether the OpenSearch Service scaling plugin (Capacity page) is configured, the selectable instance types and timezones.",
        annotations(
            title = "Capacity plugin config",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opensearch_scaling_get_config(&self) -> CallToolResult {
        respond(
            "opensearch_scaling_get_config",
            self.scaling_config().await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "opensearch_scaling_list_domains",
        description = "Amazon OpenSearch Service domains visible to the plugin with their engine version.",
        annotations(
            title = "List OpenSearch Service domains",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opensearch_scaling_list_domains(&self) -> CallToolResult {
        respond(
            "opensearch_scaling_list_domains",
            self.list_domains().await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "opensearch_scaling_get_domain",
        description = "Current capacity of a domain: instance type, node count, EBS volume size, engine version and whether a configuration change is in progress.",
        annotations(
            title = "Get one OpenSearch Service domain",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opensearch_scaling_get_domain(
        &self,
        Parameters(args): Parameters<DomainArgs>,
    ) -> CallToolResult {
        respond(
            "opensearch_scaling_get_domain",
            self.get_domain(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "opensearch_scaling_list_requests",
        description = "Scheduled, in-progress, completed, failed and cancelled capacity change reservations with target settings, schedule and requester. Newest first.",
        annotations(
            title = "List reserved scaling requests",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opensearch_scaling_list_requests(
        &self,
        Parameters(args): Parameters<ListRequestsArgs>,
    ) -> CallToolResult {
        respond(
            "opensearch_scaling_list_requests",
            self.list_scaling_requests(args).await,
            self.max_result_chars,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testing::{json as result_json, mcp};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    #[tokio::test]
    async fn scaling_tools() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/opensearch-scaling/config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"configured": true})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/opensearch-scaling/domains"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"name": "logs"}])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/opensearch-scaling/domains/logs"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"name": "logs", "instanceCount": 3})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/opensearch-scaling/requests"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": "1", "domainName": "logs", "status": "scheduled", "createdAt": "2026-01-01"},
                {"id": "2", "domain": "logs", "status": "completed", "createdAt": "2026-02-01"},
                {"id": "3", "domain": "other", "status": "scheduled", "createdAt": "2026-03-01"}
            ])))
            .mount(&server)
            .await;

        assert_eq!(
            result_json(&handler.opensearch_scaling_get_config().await)["configured"],
            true
        );
        assert_eq!(
            result_json(&handler.opensearch_scaling_list_domains().await)[0]["name"],
            "logs"
        );
        assert_eq!(
            result_json(
                &handler
                    .opensearch_scaling_get_domain(Parameters(DomainArgs {
                        name: "logs".into()
                    }))
                    .await
            )["instanceCount"],
            3
        );
        let scheduled = result_json(
            &handler
                .opensearch_scaling_list_requests(Parameters(ListRequestsArgs {
                    status: Some("scheduled".into()),
                    domain: Some("logs".into()),
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(scheduled["total"], 1);
        assert_eq!(scheduled["items"][0]["id"], "1");
        let all = result_json(
            &handler
                .opensearch_scaling_list_requests(Parameters(ListRequestsArgs {
                    status: None,
                    domain: None,
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(all["items"][0]["id"], "3");
    }
}
