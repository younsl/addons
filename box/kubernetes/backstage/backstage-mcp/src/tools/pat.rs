//! Personal access tokens: issuing policy, token inventory and the audit log.
//!
//! The backend returns token metadata only. The secret is shown once at
//! creation and never stored, so nothing reachable here can be replayed as a
//! credential: `tokenPrefix` is the short, non-secret head already displayed
//! in the admin UI.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::catalog::encode_segment;
use super::{
    Page, Paging, QueryPairs, ToolResult, json, matches_eq, matches_text, page, respond, str_field,
};
use crate::server::BackstageMcp;

/// Largest audit page the backend will serve, mirrored here so the model is
/// told the real ceiling instead of silently getting fewer rows.
const AUDIT_MAX_LIMIT: usize = 200;
const AUDIT_DEFAULT_LIMIT: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TokenState {
    Active,
    Expired,
    Revoked,
}

impl TokenState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
pub enum AuditEventType {
    #[serde(rename = "token.created")]
    TokenCreated,
    #[serde(rename = "token.updated")]
    TokenUpdated,
    #[serde(rename = "token.revoked")]
    TokenRevoked,
    #[serde(rename = "token.deleted")]
    TokenDeleted,
    #[serde(rename = "api.request")]
    ApiRequest,
    #[serde(rename = "api.denied")]
    ApiDenied,
}

impl AuditEventType {
    const fn as_str(self) -> &'static str {
        match self {
            Self::TokenCreated => "token.created",
            Self::TokenUpdated => "token.updated",
            Self::TokenRevoked => "token.revoked",
            Self::TokenDeleted => "token.deleted",
            Self::ApiRequest => "api.request",
            Self::ApiDenied => "api.denied",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AuditOutcome {
    Allowed,
    Denied,
}

impl AuditOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Denied => "denied",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListTokensArgs {
    /// Token state
    #[serde(default)]
    pub state: Option<TokenState>,
    /// Keep tokens scoped to this backend plugin id, for example catalog
    #[serde(default)]
    pub plugin: Option<String>,
    /// Keep only tokens that may write through at least one of their scopes
    #[serde(default)]
    pub writable_only: Option<bool>,
    /// Substring match on token name, description, prefix or the admin who issued it
    #[serde(default)]
    pub text: Option<String>,
    #[serde(flatten)]
    pub paging: Paging,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TokenIdArgs {
    /// Token id as reported by pat_list_tokens
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListAuditArgs {
    /// Event type
    #[serde(default)]
    pub event_type: Option<AuditEventType>,
    /// Whether the call was allowed or denied
    #[serde(default)]
    pub outcome: Option<AuditOutcome>,
    /// Keep events for one token id
    #[serde(default)]
    pub token_id: Option<String>,
    /// Substring match applied by the backend across token name, path and actor
    #[serde(default)]
    pub text: Option<String>,
    #[serde(flatten)]
    pub paging: Paging,
}

fn expires_at(row: &Value) -> &str {
    str_field(row, "expiresAt").unwrap_or("")
}

fn scopes(row: &Value) -> &[Value] {
    row.get("scopes")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn has_plugin(row: &Value, plugin: &str) -> bool {
    scopes(row)
        .iter()
        .any(|scope| str_field(scope, "plugin") == Some(plugin))
}

fn can_write(row: &Value) -> bool {
    scopes(row)
        .iter()
        .any(|scope| str_field(scope, "access") == Some("write"))
}

impl BackstageMcp {
    async fn pat_settings(&self) -> ToolResult {
        Ok(self.client.get_json("/api/pat/settings", &[]).await?)
    }

    async fn pat_tokens(&self, args: ListTokensArgs) -> ToolResult {
        let rows: Vec<Value> = self.client.get_json("/api/pat/tokens", &[]).await?;
        let state = args.state.map(TokenState::as_str);
        let mut filtered: Vec<Value> = rows
            .into_iter()
            .filter(|row| {
                matches_eq(state, str_field(row, "state").unwrap_or(""))
                    && args
                        .plugin
                        .as_deref()
                        .is_none_or(|plugin| has_plugin(row, plugin))
                    && (args.writable_only != Some(true) || can_write(row))
                    && matches_text(
                        args.text.as_deref(),
                        &[
                            str_field(row, "name"),
                            str_field(row, "description"),
                            str_field(row, "tokenPrefix"),
                            str_field(row, "createdBy"),
                        ],
                    )
            })
            .collect();
        // Soonest expiry first, so the page an agent reads without paging is
        // the one worth acting on.
        filtered.sort_by(|a, b| expires_at(a).cmp(expires_at(b)));
        json(page(filtered, args.paging, 50))
    }

    async fn pat_token(&self, args: TokenIdArgs) -> ToolResult {
        Ok(self
            .client
            .get_json(
                &format!("/api/pat/tokens/{}", encode_segment(&args.id)),
                &[],
            )
            .await?)
    }

    async fn pat_audit(&self, args: ListAuditArgs) -> ToolResult {
        let offset = args.paging.offset.unwrap_or(0);
        let limit = args
            .paging
            .limit
            .unwrap_or(AUDIT_DEFAULT_LIMIT)
            .clamp(1, AUDIT_MAX_LIMIT);
        // The audit log is filtered and paged in the backend, which reads it
        // from an indexed table; re-doing it here would pull every row.
        let mut query: QueryPairs =
            vec![("offset", offset.to_string()), ("limit", limit.to_string())];
        if let Some(event_type) = args.event_type {
            query.push(("eventType", event_type.as_str().to_string()));
        }
        if let Some(outcome) = args.outcome {
            query.push(("outcome", outcome.as_str().to_string()));
        }
        if let Some(token_id) = args.token_id.as_deref() {
            query.push(("tokenId", token_id.to_string()));
        }
        if let Some(text) = args.text.as_deref() {
            query.push(("search", text.to_string()));
        }

        let response: Value = self.client.get_json("/api/pat/audit", &query).await?;
        let items: Vec<Value> = response
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let total = response
            .get("total")
            .and_then(Value::as_u64)
            .and_then(|total| usize::try_from(total).ok())
            .unwrap_or(items.len());
        json(Page {
            total,
            offset,
            limit,
            truncated: offset + items.len() < total,
            items,
        })
    }

    async fn pat_audit_summary(&self) -> ToolResult {
        Ok(self.client.get_json("/api/pat/audit/summary", &[]).await?)
    }
}

#[tool_router(router = pat_router, vis = "pub(crate)")]
impl BackstageMcp {
    #[tool(
        name = "pat_get_settings",
        description = "Personal access token policy: the longest lifetime an admin may pick, how long audit events are kept and the backend plugins a token may be scoped to. Read this first to learn which plugin ids are valid scope targets.",
        annotations(
            title = "Personal access token settings",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn pat_get_settings(&self) -> CallToolResult {
        respond(
            "pat_get_settings",
            self.pat_settings().await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "pat_list_tokens",
        description = "Issued personal access tokens with their scopes, state, expiry and last use. Filter by state, scoped plugin, write access or text. Sorted by soonest expiry first. The secret is never returned, only the short non-secret prefix.",
        annotations(
            title = "List personal access tokens",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn pat_list_tokens(
        &self,
        Parameters(args): Parameters<ListTokensArgs>,
    ) -> CallToolResult {
        respond(
            "pat_list_tokens",
            self.pat_tokens(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "pat_get_token",
        description = "One personal access token by id: scopes, state, who issued it, expiry, revocation and last use. The secret is never returned.",
        annotations(
            title = "Get a personal access token",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn pat_get_token(&self, Parameters(args): Parameters<TokenIdArgs>) -> CallToolResult {
        respond(
            "pat_get_token",
            self.pat_token(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "pat_list_audit_events",
        description = "Audit log of personal access token use and lifecycle, newest first. Covers allowed and denied API calls with method, path, status and deny reason, plus every issue, update, revoke and delete. Filter by event type, outcome, token id or text. The backend pages this, so limit is capped at 200.",
        annotations(
            title = "List personal access token audit events",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn pat_list_audit_events(
        &self,
        Parameters(args): Parameters<ListAuditArgs>,
    ) -> CallToolResult {
        respond(
            "pat_list_audit_events",
            self.pat_audit(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "pat_get_audit_summary",
        description = "Rolling counts over the audit window: calls made, calls denied, tokens still active and tokens expiring soon. Use it before paging the audit log to see whether anything needs attention.",
        annotations(
            title = "Personal access token audit summary",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn pat_get_audit_summary(&self) -> CallToolResult {
        respond(
            "pat_get_audit_summary",
            self.pat_audit_summary().await,
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

    fn list_args() -> ListTokensArgs {
        ListTokensArgs {
            state: None,
            plugin: None,
            writable_only: None,
            text: None,
            paging: Paging::default(),
        }
    }

    fn audit_args() -> ListAuditArgs {
        ListAuditArgs {
            event_type: None,
            outcome: None,
            token_id: None,
            text: None,
            paging: Paging::default(),
        }
    }

    async fn tokens_server() -> (wiremock::MockServer, BackstageMcp) {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/pat/tokens"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "id": "1", "name": "ci-deploy", "description": "deploy bot",
                    "tokenPrefix": "bs_abcd1234", "state": "active",
                    "scopes": [{"plugin": "catalog", "access": "write"}],
                    "createdBy": "user:default/admin1", "expiresAt": "2027-01-01T00:00:00.000Z"
                },
                {
                    "id": "2", "name": "reporting", "description": null,
                    "tokenPrefix": "bs_efgh5678", "state": "active",
                    "scopes": [{"plugin": "opencost", "access": "read"}],
                    "createdBy": "user:default/admin2", "expiresAt": "2026-10-01T00:00:00.000Z"
                },
                {
                    "id": "3", "name": "old-bot", "description": null,
                    "tokenPrefix": "bs_ijkl9012", "state": "revoked",
                    "scopes": [], "createdBy": "user:default/admin1",
                    "expiresAt": "2026-01-01T00:00:00.000Z"
                }
            ])))
            .mount(&server)
            .await;
        (server, handler)
    }

    #[tokio::test]
    async fn tokens_sort_by_expiry_and_filter() {
        let (_server, handler) = tokens_server().await;

        let all = result_json(&handler.pat_list_tokens(Parameters(list_args())).await);
        assert_eq!(all["total"], 3);
        assert_eq!(all["items"][0]["name"], "old-bot");
        assert_eq!(all["items"][2]["name"], "ci-deploy");

        let writable = result_json(
            &handler
                .pat_list_tokens(Parameters(ListTokensArgs {
                    state: Some(TokenState::Active),
                    plugin: Some("catalog".into()),
                    writable_only: Some(true),
                    text: Some("DEPLOY".into()),
                    ..list_args()
                }))
                .await,
        );
        assert_eq!(writable["total"], 1);
        assert_eq!(writable["items"][0]["id"], "1");

        let revoked = result_json(
            &handler
                .pat_list_tokens(Parameters(ListTokensArgs {
                    state: Some(TokenState::Revoked),
                    ..list_args()
                }))
                .await,
        );
        assert_eq!(revoked["total"], 1);
        assert_eq!(revoked["items"][0]["name"], "old-bot");

        let none = result_json(
            &handler
                .pat_list_tokens(Parameters(ListTokensArgs {
                    plugin: Some("search".into()),
                    ..list_args()
                }))
                .await,
        );
        assert_eq!(none["total"], 0);
    }

    #[tokio::test]
    async fn token_list_never_carries_a_secret() {
        let (_server, handler) = tokens_server().await;
        let body = text(&handler.pat_list_tokens(Parameters(list_args())).await);
        assert!(body.contains("bs_abcd1234"));
        assert!(!body.contains("\"token\""));
        assert!(!body.contains("hash"));
    }

    #[tokio::test]
    async fn settings_token_and_summary() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/pat/settings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "maxExpiryDays": 365,
                "auditRetentionDays": 365,
                "scopablePlugins": [{"id": "catalog", "label": "Catalog", "description": null}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/pat/tokens/a%2Fb"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "a/b"})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/pat/audit/summary"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "windowHours": 24, "requests": 12, "denied": 2,
                "activeTokens": 3, "expiringSoonTokens": 1
            })))
            .mount(&server)
            .await;

        let settings = result_json(&handler.pat_get_settings().await);
        assert_eq!(settings["maxExpiryDays"], 365);
        assert_eq!(settings["scopablePlugins"][0]["id"], "catalog");

        let token = result_json(
            &handler
                .pat_get_token(Parameters(TokenIdArgs { id: "a/b".into() }))
                .await,
        );
        assert_eq!(token["id"], "a/b");

        let summary = result_json(&handler.pat_get_audit_summary().await);
        assert_eq!(summary["denied"], 2);
    }

    #[tokio::test]
    async fn audit_pages_in_the_backend() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/pat/audit"))
            .and(query_param("limit", "200"))
            .and(query_param("offset", "10"))
            .and(query_param("eventType", "api.denied"))
            .and(query_param("outcome", "denied"))
            .and(query_param("tokenId", "t1"))
            .and(query_param("search", "catalog"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [{"id": 11, "eventType": "api.denied", "reason": "scope_missing"}],
                "total": 40
            })))
            .mount(&server)
            .await;

        let denied = result_json(
            &handler
                .pat_list_audit_events(Parameters(ListAuditArgs {
                    event_type: Some(AuditEventType::ApiDenied),
                    outcome: Some(AuditOutcome::Denied),
                    token_id: Some("t1".into()),
                    text: Some("catalog".into()),
                    paging: Paging {
                        offset: Some(10),
                        // Above the backend ceiling, clamped to 200.
                        limit: Some(5_000),
                    },
                }))
                .await,
        );
        assert_eq!(denied["total"], 40);
        assert_eq!(denied["limit"], 200);
        assert_eq!(denied["truncated"], true);
        assert_eq!(denied["items"][0]["reason"], "scope_missing");
    }

    #[tokio::test]
    async fn audit_defaults_and_missing_total() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/pat/audit"))
            .and(query_param("limit", "50"))
            .and(query_param("offset", "0"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"items": [{"id": 1}, {"id": 2}]})),
            )
            .mount(&server)
            .await;
        let events = result_json(
            &handler
                .pat_list_audit_events(Parameters(audit_args()))
                .await,
        );
        assert_eq!(events["total"], 2);
        assert_eq!(events["truncated"], false);
    }

    #[tokio::test]
    async fn a_denied_token_read_is_a_tool_error() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/pat/tokens"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({"error": "Admin only"})))
            .mount(&server)
            .await;
        let denied = handler.pat_list_tokens(Parameters(list_args())).await;
        assert_eq!(denied.is_error, Some(true));
        assert!(text(&denied).contains("403"));
        assert!(text(&denied).contains("not allowed to read"));
    }
}
