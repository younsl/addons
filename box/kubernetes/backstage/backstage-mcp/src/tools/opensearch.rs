//! OpenSearch: internal user accounts, account requests and mapping-conflict
//! scans (the OpenSearch page).

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::catalog::encode_segment;
use super::{
    Paging, ToolResult, json as to_json, matches_eq, matches_text, page, respond,
    sort_newest_first, str_field,
};
use crate::server::BackstageMcp;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListAccountsArgs {
    /// Substring match on username
    #[serde(default)]
    pub text: Option<String>,
    /// Keep users holding this security role or backend role
    #[serde(default)]
    pub role: Option<String>,
    /// Include reserved system users, default false
    #[serde(default)]
    pub include_reserved: Option<bool>,
    #[serde(flatten)]
    pub paging: Paging,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListRequestsArgs {
    /// Exact status, for example pending, approved, rejected, completed, failed
    #[serde(default)]
    pub status: Option<String>,
    /// Exact action, for example create, delete, modify
    #[serde(default)]
    pub action: Option<String>,
    /// Exact target username
    #[serde(default)]
    pub username: Option<String>,
    #[serde(flatten)]
    pub paging: Paging,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RequestIdArgs {
    /// Request id
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SnapshotArgs {
    /// Target id from opensearch_viewer_get_config or opensearch_viewer_list_snapshots
    pub target_id: String,
    #[serde(flatten)]
    pub paging: Paging,
}

fn has_role(row: &Value, role: &str) -> bool {
    ["securityRoles", "backendRoles"].iter().any(|key| {
        row.get(key)
            .and_then(Value::as_array)
            .is_some_and(|roles| roles.iter().any(|r| r.as_str() == Some(role)))
    })
}

impl BackstageMcp {
    async fn account_config(&self) -> ToolResult {
        Ok(self
            .client
            .get_json("/api/opensearch-account/config", &[])
            .await?)
    }

    async fn list_accounts(&self, args: ListAccountsArgs) -> ToolResult {
        let rows: Vec<Value> = self
            .client
            .get_json("/api/opensearch-account/accounts", &[])
            .await?;
        let include_reserved = args.include_reserved.unwrap_or(false);
        let filtered: Vec<Value> = rows
            .into_iter()
            .filter(|row| {
                (include_reserved
                    || !row
                        .get("reserved")
                        .and_then(Value::as_bool)
                        .unwrap_or(false))
                    && args.role.as_deref().is_none_or(|role| has_role(row, role))
                    && matches_text(args.text.as_deref(), &[str_field(row, "username")])
            })
            .collect();
        to_json(page(filtered, args.paging, 50))
    }

    async fn list_roles(&self) -> ToolResult {
        let security: Value = self
            .client
            .get_json("/api/opensearch-account/roles", &[])
            .await?;
        let backend: Value = self
            .client
            .get_json("/api/opensearch-account/backend-roles", &[])
            .await?;
        Ok(json!({ "securityRoles": security, "backendRoles": backend }))
    }

    async fn list_account_requests(&self, args: ListRequestsArgs) -> ToolResult {
        let rows: Vec<Value> = self
            .client
            .get_json("/api/opensearch-account/requests", &[])
            .await?;
        let mut filtered: Vec<Value> = rows
            .into_iter()
            .filter(|row| {
                matches_eq(
                    args.status.as_deref(),
                    str_field(row, "status").unwrap_or(""),
                ) && matches_eq(
                    args.action.as_deref(),
                    str_field(row, "action").unwrap_or(""),
                ) && matches_eq(
                    args.username.as_deref(),
                    str_field(row, "username").unwrap_or(""),
                )
            })
            .collect();
        sort_newest_first(&mut filtered, "createdAt");
        to_json(page(filtered, args.paging, 50))
    }

    async fn get_account_request(&self, args: RequestIdArgs) -> ToolResult {
        Ok(self
            .client
            .get_json(
                &format!(
                    "/api/opensearch-account/requests/{}",
                    encode_segment(&args.id)
                ),
                &[],
            )
            .await?)
    }

    async fn viewer_config(&self) -> ToolResult {
        Ok(self
            .client
            .get_json("/api/opensearch-viewer/config", &[])
            .await?)
    }

    async fn list_snapshots(&self) -> ToolResult {
        let rows: Vec<Value> = self
            .client
            .get_json("/api/opensearch-viewer/snapshots", &[])
            .await?;
        let summaries: Vec<Value> = rows
            .into_iter()
            .map(|mut row| {
                let count = row
                    .as_object_mut()
                    .and_then(|object| object.remove("conflicts"))
                    .and_then(|c| c.as_array().map(Vec::len))
                    .unwrap_or(0);
                if let Some(object) = row.as_object_mut() {
                    object.insert("conflictCount".to_string(), Value::from(count));
                }
                row
            })
            .collect();
        Ok(Value::Array(summaries))
    }

    async fn get_snapshot(&self, args: SnapshotArgs) -> ToolResult {
        let mut snapshot: Value = self
            .client
            .get_json(
                &format!(
                    "/api/opensearch-viewer/snapshots/{}",
                    encode_segment(&args.target_id)
                ),
                &[],
            )
            .await?;
        let conflicts: Vec<Value> = snapshot
            .as_object_mut()
            .and_then(|object| object.remove("conflicts"))
            .and_then(|c| c.as_array().cloned())
            .unwrap_or_default();
        if let Some(object) = snapshot.as_object_mut() {
            object.insert(
                "conflicts".to_string(),
                serde_json::to_value(page(conflicts, args.paging, 50))?,
            );
        }
        Ok(snapshot)
    }
}

#[tool_router(router = opensearch_router, vis = "pub(crate)")]
impl BackstageMcp {
    #[tool(
        name = "opensearch_account_get_config",
        description = "Whether the OpenSearch Security API is configured, whether account requests need admin approval, and the master user name.",
        annotations(
            title = "OpenSearch account plugin config",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opensearch_account_get_config(&self) -> CallToolResult {
        respond(
            "opensearch_account_get_config",
            self.account_config().await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "opensearch_account_list_accounts",
        description = "Internal users defined in OpenSearch Security with their backend roles and security roles. Reserved and hidden users are flagged and reserved ones hidden by default.",
        annotations(
            title = "List OpenSearch internal users",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opensearch_account_list_accounts(
        &self,
        Parameters(args): Parameters<ListAccountsArgs>,
    ) -> CallToolResult {
        respond(
            "opensearch_account_list_accounts",
            self.list_accounts(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "opensearch_account_list_roles",
        description = "Security role names and known backend role names that can be assigned to internal users.",
        annotations(
            title = "List OpenSearch roles",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opensearch_account_list_roles(&self) -> CallToolResult {
        respond(
            "opensearch_account_list_roles",
            self.list_roles().await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "opensearch_account_list_requests",
        description = "Create, modify and delete requests for OpenSearch internal users with approval status, requester, reviewer and audit events. Newest first.",
        annotations(
            title = "List OpenSearch account requests",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opensearch_account_list_requests(
        &self,
        Parameters(args): Parameters<ListRequestsArgs>,
    ) -> CallToolResult {
        respond(
            "opensearch_account_list_requests",
            self.list_account_requests(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "opensearch_account_get_request",
        description = "Full record of one OpenSearch account request by id, including its audit events.",
        annotations(
            title = "Get one OpenSearch account request",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opensearch_account_get_request(
        &self,
        Parameters(args): Parameters<RequestIdArgs>,
    ) -> CallToolResult {
        respond(
            "opensearch_account_get_request",
            self.get_account_request(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "opensearch_viewer_get_config",
        description = "Targets scanned for field mapping conflicts by the OpenSearch viewer and the scan cron.",
        annotations(
            title = "OpenSearch mapping-conflict scanner config",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opensearch_viewer_get_config(&self) -> CallToolResult {
        respond(
            "opensearch_viewer_get_config",
            self.viewer_config().await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "opensearch_viewer_list_snapshots",
        description = "Latest field mapping conflict scan per target: status, scan time and summary counts. Conflict detail is omitted here; use opensearch_viewer_get_snapshot for it.",
        annotations(
            title = "Mapping-conflict scan results",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opensearch_viewer_list_snapshots(&self) -> CallToolResult {
        respond(
            "opensearch_viewer_list_snapshots",
            self.list_snapshots().await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "opensearch_viewer_get_snapshot",
        description = "Full conflict list of the latest scan for one target: each field with the conflicting types, affected indices and document counts, paged with offset and limit.",
        annotations(
            title = "Mapping conflicts of one target",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opensearch_viewer_get_snapshot(
        &self,
        Parameters(args): Parameters<SnapshotArgs>,
    ) -> CallToolResult {
        respond(
            "opensearch_viewer_get_snapshot",
            self.get_snapshot(args).await,
            self.max_result_chars,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testing::{json as result_json, mcp};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    #[tokio::test]
    async fn account_tools() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/opensearch-account/accounts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"username": "admin", "reserved": true, "securityRoles": ["all_access"], "backendRoles": []},
                {"username": "app-reader", "reserved": false, "securityRoles": ["readall"], "backendRoles": ["readers"]},
                {"username": "app-writer", "reserved": false, "securityRoles": [], "backendRoles": ["writers"]}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/opensearch-account/config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"configured": true})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/opensearch-account/roles"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!(["readall"])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/opensearch-account/backend-roles"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!(["readers"])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/opensearch-account/requests"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": "1", "action": "create", "username": "x", "status": "pending", "createdAt": "2026-01-01"},
                {"id": "2", "action": "delete", "username": "x", "status": "approved", "createdAt": "2026-02-01"}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/opensearch-account/requests/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "1"})))
            .mount(&server)
            .await;

        let accounts = result_json(
            &handler
                .opensearch_account_list_accounts(Parameters(ListAccountsArgs {
                    text: Some("APP".into()),
                    role: Some("readers".into()),
                    include_reserved: None,
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(accounts["total"], 1);
        assert_eq!(accounts["items"][0]["username"], "app-reader");
        let with_reserved = result_json(
            &handler
                .opensearch_account_list_accounts(Parameters(ListAccountsArgs {
                    text: None,
                    role: None,
                    include_reserved: Some(true),
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(with_reserved["total"], 3);
        assert_eq!(
            result_json(&handler.opensearch_account_get_config().await)["configured"],
            true
        );
        let roles = result_json(&handler.opensearch_account_list_roles().await);
        assert_eq!(roles["securityRoles"][0], "readall");
        assert_eq!(roles["backendRoles"][0], "readers");
        let requests = result_json(
            &handler
                .opensearch_account_list_requests(Parameters(ListRequestsArgs {
                    status: None,
                    action: None,
                    username: Some("x".into()),
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(requests["items"][0]["id"], "2");
        let pending = result_json(
            &handler
                .opensearch_account_list_requests(Parameters(ListRequestsArgs {
                    status: Some("pending".into()),
                    action: Some("create".into()),
                    username: None,
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(pending["total"], 1);
        assert_eq!(
            result_json(
                &handler
                    .opensearch_account_get_request(Parameters(RequestIdArgs { id: "1".into() }))
                    .await
            )["id"],
            "1"
        );
    }

    #[tokio::test]
    async fn viewer_tools() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/opensearch-viewer/config"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"targets": [{"id": "logs"}]})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/opensearch-viewer/snapshots"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"target": {"id": "logs"}, "status": "ok", "conflicts": [{"field": "a"}, {"field": "b"}]},
                {"target": {"id": "empty"}, "status": "error"}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/opensearch-viewer/snapshots/logs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "target": {"id": "logs"}, "conflicts": [{"field": "a"}, {"field": "b"}, {"field": "c"}]
            })))
            .mount(&server)
            .await;

        assert_eq!(
            result_json(&handler.opensearch_viewer_get_config().await)["targets"][0]["id"],
            "logs"
        );
        let list = result_json(&handler.opensearch_viewer_list_snapshots().await);
        assert_eq!(list[0]["conflictCount"], 2);
        assert_eq!(list[1]["conflictCount"], 0);
        assert!(list[0].get("conflicts").is_none());
        let snapshot = result_json(
            &handler
                .opensearch_viewer_get_snapshot(Parameters(SnapshotArgs {
                    target_id: "logs".into(),
                    paging: Paging {
                        offset: Some(1),
                        limit: Some(1),
                    },
                }))
                .await,
        );
        assert_eq!(snapshot["conflicts"]["total"], 3);
        assert_eq!(snapshot["conflicts"]["items"][0]["field"], "b");
    }
}
