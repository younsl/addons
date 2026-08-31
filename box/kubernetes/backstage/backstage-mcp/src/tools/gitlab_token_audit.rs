//! GitLab access-token audit: inventory, expiry and notification history.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::{Paging, ToolResult, json, matches_eq, matches_text, page, respond, str_field};
use crate::server::BackstageMcp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TokenKind {
    Personal,
    Project,
    Group,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TokenState {
    Active,
    Expired,
    Revoked,
    Inactive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NotificationStatus {
    Success,
    Failed,
}

fn enum_name<T: serde::Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

impl serde::Serialize for TokenKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Personal => "personal",
            Self::Project => "project",
            Self::Group => "group",
        })
    }
}

impl serde::Serialize for TokenState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Active => "active",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
            Self::Inactive => "inactive",
        })
    }
}

impl serde::Serialize for NotificationStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Success => "success",
            Self::Failed => "failed",
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListTokensArgs {
    /// Token kind
    #[serde(default)]
    pub kind: Option<TokenKind>,
    /// Token state
    #[serde(default)]
    pub state: Option<TokenState>,
    /// Keep tokens that expire within this many days (includes already expired ones)
    #[serde(default)]
    pub expiring_within_days: Option<i64>,
    /// Keep tokens granting this scope, for example api or write_repository
    #[serde(default)]
    pub scope: Option<String>,
    /// Substring match on token name, description, user name or owner scope
    #[serde(default)]
    pub text: Option<String>,
    #[serde(flatten)]
    pub paging: Paging,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListNotificationsArgs {
    /// Delivery status
    #[serde(default)]
    pub status: Option<NotificationStatus>,
    #[serde(flatten)]
    pub paging: Paging,
}

fn days_until_expiry(row: &Value) -> Option<i64> {
    row.get("daysUntilExpiry").and_then(Value::as_i64)
}

fn has_scope(row: &Value, scope: &str) -> bool {
    row.get("scopes")
        .and_then(Value::as_array)
        .is_some_and(|scopes| scopes.iter().any(|s| s.as_str() == Some(scope)))
}

impl BackstageMcp {
    async fn token_status(&self) -> ToolResult {
        Ok(self
            .client
            .get_json("/api/gitlab-token-audit/status", &[])
            .await?)
    }

    async fn list_tokens(&self, args: ListTokensArgs) -> ToolResult {
        let rows: Vec<Value> = self
            .client
            .get_json("/api/gitlab-token-audit/tokens", &[])
            .await?;
        let kind = args.kind.map(enum_name);
        let state = args.state.map(enum_name);
        let mut filtered: Vec<Value> = rows
            .into_iter()
            .filter(|row| {
                matches_eq(kind.as_deref(), str_field(row, "kind").unwrap_or(""))
                    && matches_eq(state.as_deref(), str_field(row, "state").unwrap_or(""))
                    && args.expiring_within_days.is_none_or(|within| {
                        days_until_expiry(row).is_some_and(|days| days <= within)
                    })
                    && args
                        .scope
                        .as_deref()
                        .is_none_or(|scope| has_scope(row, scope))
                    && matches_text(
                        args.text.as_deref(),
                        &[
                            str_field(row, "name"),
                            str_field(row, "description"),
                            str_field(row, "userName"),
                            str_field(row, "ownerScope"),
                        ],
                    )
            })
            .collect();
        filtered.sort_by_key(|row| days_until_expiry(row).unwrap_or(i64::MAX));
        json(page(filtered, args.paging, 50))
    }

    async fn webhook(&self) -> ToolResult {
        let webhook: Option<Map<String, Value>> = self
            .client
            .get_json("/api/gitlab-token-audit/webhook", &[])
            .await?;
        let Some(mut webhook) = webhook else {
            return Ok(Value::Null);
        };
        // The URL is a credential-bearing Slack webhook; only report that one
        // is set.
        let configured = webhook
            .remove("url")
            .and_then(|url| url.as_str().map(|u| !u.is_empty()))
            .unwrap_or(false);
        webhook.insert("urlConfigured".to_string(), Value::Bool(configured));
        Ok(Value::Object(webhook))
    }

    async fn notifications(&self, args: ListNotificationsArgs) -> ToolResult {
        let response: Value = self
            .client
            .get_json("/api/gitlab-token-audit/notifications", &[])
            .await?;
        let status = args.status.map(enum_name);
        let filtered: Vec<Value> = response
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|row| matches_eq(status.as_deref(), str_field(row, "status").unwrap_or("")))
            .collect();
        json(page(filtered, args.paging, 50))
    }
}

#[tool_router(router = gitlab_token_audit_router, vis = "pub(crate)")]
impl BackstageMcp {
    #[tool(
        name = "gitlab_token_audit_get_status",
        description = "Overview of the GitLab access-token audit: token totals, expired and expiring-soon counts, cron schedules, webhook configuration and GitLab server health.",
        annotations(
            title = "GitLab token audit status",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn gitlab_token_audit_get_status(&self) -> CallToolResult {
        respond(
            "gitlab_token_audit_get_status",
            self.token_status().await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "gitlab_token_audit_list_tokens",
        description = "Personal, project and group access tokens discovered by the audit, with owner, scopes, expiry and state. Filter by kind, state, scope, text or days until expiry. Sorted by soonest expiry first.",
        annotations(
            title = "List GitLab access tokens",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn gitlab_token_audit_list_tokens(
        &self,
        Parameters(args): Parameters<ListTokensArgs>,
    ) -> CallToolResult {
        respond(
            "gitlab_token_audit_list_tokens",
            self.list_tokens(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "gitlab_token_audit_get_webhook",
        description = "The Slack webhook configuration used for token expiry alerts: enabled flag, notification thresholds in days and who last changed it. The webhook URL itself is never returned, only whether one is set.",
        annotations(
            title = "Expiry notification webhook",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn gitlab_token_audit_get_webhook(&self) -> CallToolResult {
        respond(
            "gitlab_token_audit_get_webhook",
            self.webhook().await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "gitlab_token_audit_list_notifications",
        description = "Log of token expiry notifications already sent or failed, newest first, up to 200 entries.",
        annotations(
            title = "Sent expiry notifications",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn gitlab_token_audit_list_notifications(
        &self,
        Parameters(args): Parameters<ListNotificationsArgs>,
    ) -> CallToolResult {
        respond(
            "gitlab_token_audit_list_notifications",
            self.notifications(args).await,
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
    async fn token_tools() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/gitlab-token-audit/tokens"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"name": "ci", "kind": "project", "state": "active", "daysUntilExpiry": 40, "scopes": ["api"], "userName": null},
                {"name": "bot", "kind": "personal", "state": "active", "daysUntilExpiry": 3, "scopes": ["api", "write_repository"], "userName": "svc"},
                {"name": "old", "kind": "group", "state": "expired", "daysUntilExpiry": -2, "scopes": [], "ownerScope": "grp"},
                {"name": "never", "kind": "personal", "state": "active", "daysUntilExpiry": null, "scopes": ["read_api"]}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/gitlab-token-audit/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"totalTokens": 4})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/gitlab-token-audit/webhook"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"url": "https://hooks.slack.com/x", "enabled": true})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/gitlab-token-audit/notifications"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": [
                {"tokenKey": "a", "status": "success"}, {"tokenKey": "b", "status": "failed"}
            ]})))
            .mount(&server)
            .await;

        let all = result_json(
            &handler
                .gitlab_token_audit_list_tokens(Parameters(ListTokensArgs {
                    kind: None,
                    state: None,
                    expiring_within_days: None,
                    scope: None,
                    text: None,
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(all["total"], 4);
        assert_eq!(all["items"][0]["name"], "old");
        assert_eq!(all["items"][3]["name"], "never");

        let soon = result_json(
            &handler
                .gitlab_token_audit_list_tokens(Parameters(ListTokensArgs {
                    kind: Some(TokenKind::Personal),
                    state: Some(TokenState::Active),
                    expiring_within_days: Some(7),
                    scope: Some("api".into()),
                    text: Some("SVC".into()),
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(soon["total"], 1);
        assert_eq!(soon["items"][0]["name"], "bot");

        assert_eq!(
            result_json(&handler.gitlab_token_audit_get_status().await)["totalTokens"],
            4
        );

        let webhook = result_json(&handler.gitlab_token_audit_get_webhook().await);
        assert_eq!(webhook["urlConfigured"], true);
        assert!(webhook.get("url").is_none());
        assert!(!text(&handler.gitlab_token_audit_get_webhook().await).contains("hooks.slack.com"));

        let failed = result_json(
            &handler
                .gitlab_token_audit_list_notifications(Parameters(ListNotificationsArgs {
                    status: Some(NotificationStatus::Failed),
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(failed["total"], 1);
        assert_eq!(failed["items"][0]["tokenKey"], "b");
    }

    #[tokio::test]
    async fn webhook_may_be_unset_and_admin_denied() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/gitlab-token-audit/webhook"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Value::Null))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/gitlab-token-audit/tokens"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({"error": "Admin only"})))
            .mount(&server)
            .await;
        assert!(result_json(&handler.gitlab_token_audit_get_webhook().await).is_null());
        let denied = handler
            .gitlab_token_audit_list_tokens(Parameters(ListTokensArgs {
                kind: None,
                state: None,
                expiring_within_days: None,
                scope: None,
                text: None,
                paging: Paging::default(),
            }))
            .await;
        assert_eq!(denied.is_error, Some(true));
        assert!(text(&denied).contains("403"));
    }
}
