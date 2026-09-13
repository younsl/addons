//! IAM Audit: inactive AWS IAM users, password reset requests and mutes.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::catalog::encode_segment;
use super::{
    Paging, ToolResult, join_csv, json, matches_eq, matches_text, page, respond, sort_newest_first,
    str_field,
};
use crate::server::BackstageMcp;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListUsersArgs {
    /// Keep users inactive for at least this many days
    #[serde(default)]
    pub min_inactive_days: Option<i64>,
    /// Filter by console (password) access
    #[serde(default)]
    pub has_console_access: Option<bool>,
    /// true for users with at least one access key
    #[serde(default)]
    pub has_access_keys: Option<bool>,
    /// Exact owner entity ref, for example user:default/jane
    #[serde(default)]
    pub owner: Option<String>,
    /// Substring match on user name, ARN or owner
    #[serde(default)]
    pub text: Option<String>,
    /// Include the accessKeys detail array, default false
    #[serde(default)]
    pub include_access_keys: Option<bool>,
    #[serde(flatten)]
    pub paging: Paging,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RequestStatus {
    Pending,
    Approved,
    Rejected,
}

impl RequestStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListRequestsArgs {
    /// Approval status
    #[serde(default)]
    pub status: Option<RequestStatus>,
    /// Exact IAM user name
    #[serde(default)]
    pub iam_user_name: Option<String>,
    #[serde(flatten)]
    pub paging: Paging,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RequestIdArgs {
    /// Request id
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WarningLogsArgs {
    /// IAM user names to look up
    pub user_names: Vec<String>,
}

fn i64_field(row: &Value, key: &str) -> i64 {
    row.get(key).and_then(Value::as_i64).unwrap_or(0)
}

impl BackstageMcp {
    async fn iam_status(&self) -> ToolResult {
        Ok(self
            .client
            .get_json("/api/iam-user-audit/status", &[])
            .await?)
    }

    async fn list_users(&self, args: ListUsersArgs) -> ToolResult {
        let rows: Vec<Value> = self
            .client
            .get_json("/api/iam-user-audit/users", &[])
            .await?;
        let include_keys = args.include_access_keys.unwrap_or(false);
        let mut filtered: Vec<Value> = rows
            .into_iter()
            .filter(|row| {
                args.min_inactive_days
                    .is_none_or(|min| i64_field(row, "inactiveDays") >= min)
                    && matches_eq(
                        args.has_console_access.as_ref(),
                        &row.get("hasConsoleAccess")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    )
                    && matches_eq(
                        args.has_access_keys.as_ref(),
                        &(i64_field(row, "accessKeyCount") > 0),
                    )
                    && matches_eq(
                        args.owner.as_deref(),
                        str_field(row, "ownerRef").unwrap_or(""),
                    )
                    && matches_text(
                        args.text.as_deref(),
                        &[
                            str_field(row, "userName"),
                            str_field(row, "arn"),
                            str_field(row, "ownerRef"),
                        ],
                    )
            })
            .map(|mut row| {
                if !include_keys && let Some(object) = row.as_object_mut() {
                    object.remove("accessKeys");
                }
                row
            })
            .collect();
        filtered.sort_by_key(|row| std::cmp::Reverse(i64_field(row, "inactiveDays")));
        json(page(filtered, args.paging, 50))
    }

    async fn list_requests(&self, args: ListRequestsArgs) -> ToolResult {
        let rows: Vec<Value> = self
            .client
            .get_json("/api/iam-user-audit/password-reset/requests", &[])
            .await?;
        let status = args.status.map(RequestStatus::as_str);
        let mut filtered: Vec<Value> = rows
            .into_iter()
            .filter(|row| {
                matches_eq(status, str_field(row, "status").unwrap_or(""))
                    && matches_eq(
                        args.iam_user_name.as_deref(),
                        str_field(row, "iamUserName").unwrap_or(""),
                    )
            })
            .collect();
        sort_newest_first(&mut filtered, "createdAt");
        json(page(filtered, args.paging, 50))
    }

    async fn get_request(&self, args: RequestIdArgs) -> ToolResult {
        Ok(self
            .client
            .get_json(
                &format!(
                    "/api/iam-user-audit/password-reset/requests/{}",
                    encode_segment(&args.id)
                ),
                &[],
            )
            .await?)
    }

    async fn muted_users(&self) -> ToolResult {
        let response: Value = self
            .client
            .get_json("/api/iam-user-audit/admin/muted-users", &[])
            .await?;
        Ok(response
            .get("items")
            .cloned()
            .unwrap_or(Value::Array(Vec::new())))
    }

    async fn warning_logs(&self, args: WarningLogsArgs) -> ToolResult {
        let Some(names) = join_csv(Some(&args.user_names)) else {
            return Err(super::ToolError::Input(
                "user_names must contain at least one name".to_string(),
            ));
        };
        Ok(self
            .client
            .get_json(
                "/api/iam-user-audit/status/warning-dm-logs",
                &[("userNames", names)],
            )
            .await?)
    }

    async fn slack_health(&self) -> ToolResult {
        Ok(self
            .client
            .get_json("/api/iam-user-audit/status/slack-health", &[])
            .await?)
    }
}

#[tool_router(router = iam_user_audit_router, vis = "pub(crate)")]
impl BackstageMcp {
    #[tool(
        name = "iam_audit_get_status",
        description = "IAM user audit configuration and counters: inactivity thresholds, cron schedules, Slack configuration, last fetch time, total and inactive user counts.",
        annotations(
            title = "IAM audit status",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn iam_audit_get_status(&self) -> CallToolResult {
        respond(
            "iam_audit_get_status",
            self.iam_status().await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "iam_audit_list_users",
        description = "AWS IAM users from the last audit fetch with owner, creation date, last activity, days inactive, access key count and console access. Filter by inactivity, console access, access keys, owner or text. Sorted by most inactive first.",
        annotations(
            title = "List audited IAM users",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn iam_audit_list_users(
        &self,
        Parameters(args): Parameters<ListUsersArgs>,
    ) -> CallToolResult {
        respond(
            "iam_audit_list_users",
            self.list_users(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "iam_audit_list_password_reset_requests",
        description = "Console password reset requests raised from the IAM Audit page with their approval status, requester, reviewer and timestamps. Newest first.",
        annotations(
            title = "List password reset requests",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn iam_audit_list_password_reset_requests(
        &self,
        Parameters(args): Parameters<ListRequestsArgs>,
    ) -> CallToolResult {
        respond(
            "iam_audit_list_password_reset_requests",
            self.list_requests(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "iam_audit_get_password_reset_request",
        description = "Full record of one password reset request by id.",
        annotations(
            title = "Get one password reset request",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn iam_audit_get_password_reset_request(
        &self,
        Parameters(args): Parameters<RequestIdArgs>,
    ) -> CallToolResult {
        respond(
            "iam_audit_get_password_reset_request",
            self.get_request(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "iam_audit_list_muted_users",
        description = "IAM users excluded from inactivity warning DMs, with who muted them, why and when.",
        annotations(
            title = "List muted IAM users",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn iam_audit_list_muted_users(&self) -> CallToolResult {
        respond(
            "iam_audit_list_muted_users",
            self.muted_users().await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "iam_audit_get_warning_dm_logs",
        description = "When the last Slack inactivity warning DM was sent to each of the given IAM users, if ever.",
        annotations(
            title = "Last inactivity warning per user",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn iam_audit_get_warning_dm_logs(
        &self,
        Parameters(args): Parameters<WarningLogsArgs>,
    ) -> CallToolResult {
        respond(
            "iam_audit_get_warning_dm_logs",
            self.warning_logs(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "iam_audit_get_slack_health",
        description = "Whether the Slack webhook and bot used by the IAM audit are configured and the bot token is valid.",
        annotations(
            title = "Slack integration health",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn iam_audit_get_slack_health(&self) -> CallToolResult {
        respond(
            "iam_audit_get_slack_health",
            self.slack_health().await,
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
    use wiremock::{Mock, ResponseTemplate};

    #[tokio::test]
    async fn user_tools() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/iam-user-audit/users"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"userName": "alice", "arn": "arn:a", "ownerRef": "user:default/alice", "inactiveDays": 10, "accessKeyCount": 1, "hasConsoleAccess": true, "accessKeys": [{"id": "AKIA"}]},
                {"userName": "svc-batch", "arn": "arn:b", "inactiveDays": 200, "accessKeyCount": 0, "hasConsoleAccess": false, "accessKeys": []},
                {"userName": "bob", "arn": "arn:c", "inactiveDays": 95, "accessKeyCount": 2, "hasConsoleAccess": true, "accessKeys": []}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/iam-user-audit/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"totalUsers": 3})))
            .mount(&server)
            .await;

        let all = result_json(
            &handler
                .iam_audit_list_users(Parameters(ListUsersArgs {
                    min_inactive_days: None,
                    has_console_access: None,
                    has_access_keys: None,
                    owner: None,
                    text: None,
                    include_access_keys: None,
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(all["items"][0]["userName"], "svc-batch");
        assert!(all["items"][0].get("accessKeys").is_none());

        let filtered = result_json(
            &handler
                .iam_audit_list_users(Parameters(ListUsersArgs {
                    min_inactive_days: Some(90),
                    has_console_access: Some(true),
                    has_access_keys: Some(true),
                    owner: None,
                    text: Some("arn:c".into()),
                    include_access_keys: Some(true),
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(filtered["total"], 1);
        assert_eq!(filtered["items"][0]["userName"], "bob");
        assert!(filtered["items"][0].get("accessKeys").is_some());

        let owned = result_json(
            &handler
                .iam_audit_list_users(Parameters(ListUsersArgs {
                    min_inactive_days: None,
                    has_console_access: None,
                    has_access_keys: None,
                    owner: Some("user:default/alice".into()),
                    text: None,
                    include_access_keys: None,
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(owned["total"], 1);
        assert_eq!(
            result_json(&handler.iam_audit_get_status().await)["totalUsers"],
            3
        );
    }

    #[tokio::test]
    async fn request_and_admin_tools() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/iam-user-audit/password-reset/requests"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": "1", "iamUserName": "alice", "status": "pending", "createdAt": "2026-08-01T00:00:00Z"},
                {"id": "2", "iamUserName": "alice", "status": "approved", "createdAt": "2026-08-02T00:00:00Z"},
                {"id": "3", "iamUserName": "bob", "status": "pending", "createdAt": "2026-08-03T00:00:00Z"}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/iam-user-audit/password-reset/requests/2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "2"})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/iam-user-audit/admin/muted-users"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"items": [{"iamUserName": "svc"}]})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/iam-user-audit/status/warning-dm-logs"))
            .and(query_param("userNames", "alice,bob"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"alice": {"lastDm": null}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/iam-user-audit/status/slack-health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"bot": {"valid": true}})))
            .mount(&server)
            .await;

        let pending = result_json(
            &handler
                .iam_audit_list_password_reset_requests(Parameters(ListRequestsArgs {
                    status: Some(RequestStatus::Pending),
                    iam_user_name: None,
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(pending["total"], 2);
        assert_eq!(pending["items"][0]["id"], "3");
        let alice = result_json(
            &handler
                .iam_audit_list_password_reset_requests(Parameters(ListRequestsArgs {
                    status: None,
                    iam_user_name: Some("alice".into()),
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(alice["total"], 2);
        assert_eq!(
            result_json(
                &handler
                    .iam_audit_get_password_reset_request(Parameters(RequestIdArgs {
                        id: "2".into()
                    }))
                    .await
            )["id"],
            "2"
        );
        assert_eq!(
            result_json(&handler.iam_audit_list_muted_users().await)[0]["iamUserName"],
            "svc"
        );
        let logs = result_json(
            &handler
                .iam_audit_get_warning_dm_logs(Parameters(WarningLogsArgs {
                    user_names: vec!["alice".into(), "bob".into()],
                }))
                .await,
        );
        assert!(logs["alice"].is_object());
        let empty = handler
            .iam_audit_get_warning_dm_logs(Parameters(WarningLogsArgs { user_names: vec![] }))
            .await;
        assert!(text(&empty).contains("at least one"));
        assert_eq!(
            result_json(&handler.iam_audit_get_slack_health().await)["bot"]["valid"],
            true
        );
    }
}
