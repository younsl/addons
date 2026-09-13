//! Catalog Health: catalog-info.yaml coverage across GitLab projects.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::{Paging, ToolResult, json, matches_eq, matches_text, page, respond, str_field};
use crate::server::BackstageMcp;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListProjectsArgs {
    /// true for projects that have catalog-info.yaml, false for the ones missing it
    #[serde(default)]
    pub covered: Option<bool>,
    /// Exact GitLab group path
    #[serde(default)]
    pub namespace: Option<String>,
    /// Substring match on project name or path
    #[serde(default)]
    pub text: Option<String>,
    /// Include archived projects, default false
    #[serde(default)]
    pub include_archived: Option<bool>,
    /// Include catalogInfoContent (the file body) for covered projects, default false
    #[serde(default)]
    pub include_catalog_info: Option<bool>,
    #[serde(flatten)]
    pub paging: Paging,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HistoryArgs {
    /// Window in days, default 90
    #[serde(default)]
    pub days: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BranchesArgs {
    /// GitLab project id from catalog_health_list_projects
    pub project_id: u64,
}

fn bool_field(row: &Value, key: &str) -> Option<bool> {
    row.get(key).and_then(Value::as_bool)
}

impl BackstageMcp {
    async fn coverage(&self) -> ToolResult {
        let mut response: Map<String, Value> = self
            .client
            .get_json("/api/catalog-health/coverage", &[])
            .await?;
        let project_count = response
            .remove("projects")
            .and_then(|p| p.as_array().map(Vec::len))
            .unwrap_or(0);
        response.insert("projectCount".to_string(), Value::from(project_count));
        Ok(Value::Object(response))
    }

    async fn list_projects(&self, args: ListProjectsArgs) -> ToolResult {
        let response: Value = self
            .client
            .get_json("/api/catalog-health/coverage", &[])
            .await?;
        let include_archived = args.include_archived.unwrap_or(false);
        let include_content = args.include_catalog_info.unwrap_or(false);
        let filtered: Vec<Value> = response
            .get("projects")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|row| {
                matches_eq(
                    args.covered.as_ref(),
                    &bool_field(row, "hasCatalogInfo").unwrap_or(false),
                ) && matches_eq(
                    args.namespace.as_deref(),
                    str_field(row, "namespace").unwrap_or(""),
                ) && (include_archived || !bool_field(row, "archived").unwrap_or(false))
                    && matches_text(
                        args.text.as_deref(),
                        &[str_field(row, "name"), str_field(row, "pathWithNamespace")],
                    )
            })
            .map(|mut row| {
                if !include_content && let Some(object) = row.as_object_mut() {
                    object.remove("catalogInfoContent");
                }
                row
            })
            .collect();
        json(page(filtered, args.paging, 50))
    }

    async fn groups(&self) -> ToolResult {
        let rows: Value = self
            .client
            .get_json("/api/catalog-health/coverage/groups", &[])
            .await?;
        Ok(rows)
    }

    async fn history(&self, args: HistoryArgs) -> ToolResult {
        let query = args.days.map(|d| ("days", d.clamp(1, 365).to_string()));
        let rows: Value = self
            .client
            .get_json("/api/catalog-health/coverage/history", query.as_slice())
            .await?;
        Ok(rows)
    }

    async fn branches(&self, args: BranchesArgs) -> ToolResult {
        let rows: Value = self
            .client
            .get_json(
                &format!("/api/catalog-health/branches/{}", args.project_id),
                &[],
            )
            .await?;
        Ok(rows)
    }
}

#[tool_router(router = catalog_health_router, vis = "pub(crate)")]
impl BackstageMcp {
    #[tool(
        name = "catalog_health_get_coverage",
        description = "Summary of catalog-info.yaml coverage across GitLab projects: totals, percentage, last scan time and whether a scan is running. Use catalog_health_list_projects for the per-project rows.",
        annotations(
            title = "catalog-info.yaml coverage summary",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn catalog_health_get_coverage(&self) -> CallToolResult {
        respond(
            "catalog_health_get_coverage",
            self.coverage().await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "catalog_health_list_projects",
        description = "Per-project catalog-info.yaml status from the last Catalog Health scan. Filter by coverage, GitLab namespace or text. Set include_catalog_info to return the file content of covered projects.",
        annotations(
            title = "List scanned GitLab projects",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn catalog_health_list_projects(
        &self,
        Parameters(args): Parameters<ListProjectsArgs>,
    ) -> CallToolResult {
        respond(
            "catalog_health_list_projects",
            self.list_projects(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "catalog_health_list_groups",
        description = "catalog-info.yaml coverage aggregated per GitLab group (namespace).",
        annotations(
            title = "Coverage per GitLab group",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn catalog_health_list_groups(&self) -> CallToolResult {
        respond(
            "catalog_health_list_groups",
            self.groups().await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "catalog_health_get_history",
        description = "Daily coverage snapshots over the last N days, for trend questions.",
        annotations(
            title = "Coverage history",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn catalog_health_get_history(
        &self,
        Parameters(args): Parameters<HistoryArgs>,
    ) -> CallToolResult {
        respond(
            "catalog_health_get_history",
            self.history(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "catalog_health_list_branches",
        description = "Branch names of one GitLab project known to Catalog Health, with the default branch flagged.",
        annotations(
            title = "List branches of a scanned project",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn catalog_health_list_branches(
        &self,
        Parameters(args): Parameters<BranchesArgs>,
    ) -> CallToolResult {
        respond(
            "catalog_health_list_branches",
            self.branches(args).await,
            self.max_result_chars,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testing::{json as result_json, mcp};
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, ResponseTemplate};

    #[tokio::test]
    async fn coverage_and_projects() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/catalog-health/coverage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 3, "covered": 1, "percent": 33.3, "scanning": false,
                "projects": [
                    {"id": 1, "name": "a", "pathWithNamespace": "g/a", "namespace": "g", "hasCatalogInfo": true, "archived": false, "catalogInfoContent": "kind: Component"},
                    {"id": 2, "name": "b", "pathWithNamespace": "g/b", "namespace": "g", "hasCatalogInfo": false, "archived": false, "catalogInfoContent": null},
                    {"id": 3, "name": "old", "pathWithNamespace": "h/old", "namespace": "h", "hasCatalogInfo": false, "archived": true, "catalogInfoContent": null}
                ]
            })))
            .mount(&server)
            .await;

        let summary = result_json(&handler.catalog_health_get_coverage().await);
        assert_eq!(summary["projectCount"], 3);
        assert!(summary.get("projects").is_none());
        assert_eq!(summary["total"], 3);

        let uncovered = result_json(
            &handler
                .catalog_health_list_projects(Parameters(ListProjectsArgs {
                    covered: Some(false),
                    namespace: None,
                    text: None,
                    include_archived: None,
                    include_catalog_info: None,
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(uncovered["total"], 1);
        assert_eq!(uncovered["items"][0]["name"], "b");
        assert!(uncovered["items"][0].get("catalogInfoContent").is_none());

        let all = result_json(
            &handler
                .catalog_health_list_projects(Parameters(ListProjectsArgs {
                    covered: None,
                    namespace: Some("g".into()),
                    text: Some("A".into()),
                    include_archived: Some(true),
                    include_catalog_info: Some(true),
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(all["total"], 1);
        assert_eq!(all["items"][0]["catalogInfoContent"], "kind: Component");
    }

    #[tokio::test]
    async fn passthrough_tools() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/catalog-health/coverage/groups"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"namespace": "g"}])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/catalog-health/coverage/history"))
            .and(query_param("days", "30"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"percent": 1}])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/catalog-health/branches/7"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([{"name": "main", "default": true}])),
            )
            .mount(&server)
            .await;

        assert_eq!(
            result_json(&handler.catalog_health_list_groups().await)[0]["namespace"],
            "g"
        );
        let history = result_json(
            &handler
                .catalog_health_get_history(Parameters(HistoryArgs { days: Some(30) }))
                .await,
        );
        assert_eq!(history[0]["percent"], 1);
        let branches = result_json(
            &handler
                .catalog_health_list_branches(Parameters(BranchesArgs { project_id: 7 }))
                .await,
        );
        assert_eq!(branches[0]["name"], "main");
    }
}
