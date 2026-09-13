//! The MCP server exposing the forklift management API as tools.
//!
//! Tool coverage mirrors the `/api/v1` admin surface: repositories, artifacts,
//! approvals, version denies, users, roles, group mappings, tokens, audit logs,
//! coverage, storage and HA. Every tool runs with the caller's credential (see
//! [`Client`]), so what a tool may actually do is bounded by forklift RBAC.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use rmcp::handler::server::tool::parse_json_object;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    Implementation, JsonObject, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::mcp::client::{Client, path_escape, query_escape};
use crate::mcp::metrics::{Metrics, OUTCOME_ERROR, OUTCOME_OK, OUTCOME_TOOL_ERROR};

// --- query and body builders ------------------------------------------------

/// Assembles `url.Values`, skipping empty values so upstream defaults win.
///
#[derive(Debug, Default, Clone)]
pub struct Query(BTreeMap<String, String>);

impl Query {
    pub fn new() -> Query {
        Query(BTreeMap::new())
    }

    /// Sets `key` unless `value` is empty.
    pub fn str(mut self, key: &str, value: &str) -> Query {
        if !value.is_empty() {
            self.0.insert(key.to_string(), value.to_string());
        }
        self
    }

    /// Sets `key` unless `value` is zero.
    pub fn num(mut self, key: &str, value: i64) -> Query {
        if value != 0 {
            self.0.insert(key.to_string(), value.to_string());
        }
        self
    }

    /// Sets `key` to `"true"` when `value` is true; false is left out.
    pub fn boolean(mut self, key: &str, value: bool) -> Query {
        if value {
            self.0.insert(key.to_string(), "true".to_string());
        }
        self
    }

    /// True when no parameter was set, so no `?` is appended.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn encode(&self) -> String {
        let mut out = String::new();
        for (k, v) in &self.0 {
            if !out.is_empty() {
                out.push('&');
            }
            out.push_str(&query_escape(k));
            out.push('=');
            out.push_str(&query_escape(v));
        }
        out
    }
}

/// Assembles a JSON object, including only fields that were set, so upstream
/// update handlers treat missing fields as "leave unchanged".
///
#[derive(Debug, Default, Clone)]
pub struct Body(BTreeMap<String, Value>);

impl Body {
    /// An empty body. Unlike a missing body this is still sent, as `{}`.
    pub fn new() -> Body {
        Body(BTreeMap::new())
    }

    /// Sets `key` unless `value` is empty.
    pub fn str(mut self, key: &str, value: &str) -> Body {
        if !value.is_empty() {
            self.0
                .insert(key.to_string(), Value::String(value.to_string()));
        }
        self
    }

    /// Sets `key` unconditionally.
    pub fn set(mut self, key: &str, value: impl Into<Value>) -> Body {
        self.0.insert(key.to_string(), value.into());
        self
    }

    /// The underlying object, for serialization.
    pub fn as_map(&self) -> &BTreeMap<String, Value> {
        &self.0
    }
}

/// `/api/v1/repositories/{id}{suffix}`.
fn repo_path(id: i64, suffix: &str) -> String {
    format!("/api/v1/repositories/{id}{suffix}")
}

/// Wraps an upstream response (or error) as a tool result. API errors are
/// reported through `is_error` so the model can read the message and adjust,
/// rather than failing the whole MCP call.
fn result(res: Result<Vec<u8>, crate::mcp::client::Error>) -> Result<CallToolResult, ErrorData> {
    match res {
        Err(err @ crate::mcp::client::Error::Api { .. }) => {
            Ok(CallToolResult::error(vec![ContentBlock::text(
                err.to_string(),
            )]))
        }
        Err(err) => Err(ErrorData::internal_error(err.to_string(), None)),
        Ok(data) => {
            let text = String::from_utf8_lossy(&data).trim().to_string();
            let text = if text.is_empty() {
                "ok".to_string()
            } else {
                text
            };
            Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
        }
    }
}

// --- tool arguments ---------------------------------------------------------

/// The argument type of tools that take none.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct NoArgs {}

/// Arguments of `forklift_search`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    #[schemars(
        description = "search term matched against repositories, artifacts, artifact labels, approvals, users and roles"
    )]
    pub query: String,
    #[serde(default)]
    #[schemars(description = "maximum results per category, 1-20 (default 5)")]
    pub limit: i64,
}

/// Arguments of every tool addressing one repository by ID.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RepoIdArgs {
    #[schemars(description = "numeric repository ID (from forklift_list_repositories)")]
    pub repository_id: i64,
}

/// Arguments of `forklift_create_repository`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateRepositoryArgs {
    #[schemars(description = "repository name, unique")]
    pub name: String,
    #[schemars(description = "package format: maven, npm, cargo, go, pypi, raw or oci")]
    pub format: String,
    #[serde(rename = "type")]
    #[schemars(description = "repository type: hosted, proxy or group")]
    pub type_: String,
    #[serde(default)]
    #[schemars(description = "upstream registry URL, required for proxy repositories")]
    pub upstream_url: String,
    #[serde(default)]
    #[schemars(
        with = "serde_json::Map<String, Value>",
        description = "repository config object (cache TTL, group members, upstream auth, ...); same shape as the config field returned by forklift_get_repository"
    )]
    pub config: Option<serde_json::Map<String, Value>>,
}

/// Arguments of `forklift_update_repository`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateRepositoryArgs {
    #[schemars(description = "numeric repository ID")]
    pub repository_id: i64,
    #[serde(default)]
    #[schemars(description = "new upstream registry URL")]
    pub upstream_url: String,
    #[serde(default)]
    #[schemars(
        with = "serde_json::Map<String, Value>",
        description = "replacement repository config object"
    )]
    pub config: Option<serde_json::Map<String, Value>>,
}

/// Arguments of `forklift_update_repository_security`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RepositorySecurityArgs {
    #[schemars(description = "numeric repository ID")]
    pub repository_id: i64,
    #[schemars(
        description = "security config object: age_policy, approval, vuln, license, ip_acl, notify, public; same shape as the config field returned by forklift_get_repository"
    )]
    pub config: serde_json::Map<String, Value>,
}

/// Arguments of `forklift_set_repository_disabled`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetRepositoryDisabledArgs {
    #[schemars(description = "numeric repository ID")]
    pub repository_id: i64,
    #[schemars(description = "true to disable the repository, false to re-enable it")]
    pub disabled: bool,
}

/// Arguments of `forklift_list_artifacts`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListArtifactsArgs {
    #[schemars(description = "numeric repository ID")]
    pub repository_id: i64,
    #[serde(default)]
    #[schemars(description = "substring (or regex) filter on artifact path")]
    pub query: String,
    #[serde(default)]
    #[schemars(description = "treat query as a regular expression")]
    pub regex: bool,
    #[serde(default)]
    #[schemars(description = "page size, 1-500 (default 100)")]
    pub limit: i64,
    #[serde(default)]
    #[schemars(description = "pagination offset")]
    pub offset: i64,
}

/// Arguments of `forklift_delete_artifacts`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteArtifactsArgs {
    #[schemars(description = "numeric repository ID")]
    pub repository_id: i64,
    #[schemars(description = "path glob selecting the artifacts to delete; required")]
    pub query: String,
}

/// Arguments of `forklift_list_audit_logs`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListAuditLogsArgs {
    #[schemars(description = "numeric repository ID")]
    pub repository_id: i64,
    #[serde(default)]
    #[schemars(description = "filter by event type (download, upload, delete, config, ...)")]
    pub event: String,
    #[serde(default)]
    #[schemars(description = "page size, 1-500 (default 100)")]
    pub limit: i64,
    #[serde(default)]
    #[schemars(description = "pagination offset")]
    pub offset: i64,
}

/// Arguments of `forklift_get_oci_detail`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct OciDetailArgs {
    #[schemars(description = "numeric repository ID of an OCI repository")]
    pub repository_id: i64,
    #[schemars(description = "image or chart name inside the repository, e.g. library/nginx")]
    pub name: String,
    #[serde(rename = "ref")]
    #[schemars(description = "tag or digest to inspect")]
    pub ref_: String,
}

/// Arguments of `forklift_get_upload`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UploadArgs {
    #[schemars(description = "numeric repository ID")]
    pub repository_id: i64,
    #[schemars(description = "upload session ID returned when the upload was started")]
    pub upload_id: String,
}

/// Arguments of `forklift_list_artifact_labels`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ArtifactLabelsArgs {
    #[schemars(description = "numeric repository ID")]
    pub repository_id: i64,
    #[schemars(description = "artifact path inside the repository (from forklift_list_artifacts)")]
    pub path: String,
}

/// Labels or unlabels many artifacts in one call.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ArtifactBulkLabelArgs {
    #[schemars(description = "numeric repository ID")]
    pub repository_id: i64,
    #[schemars(description = "artifact paths to act on, at most 200 per call")]
    pub paths: Vec<String>,
    #[schemars(
        description = "label to apply or remove: a key or key:value of letters, digits, '-' and '_'"
    )]
    pub label: String,
    #[schemars(description = "add or remove")]
    pub action: String,
}

/// Removes many artifacts in one call.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ArtifactBulkDeleteArgs {
    #[schemars(description = "numeric repository ID")]
    pub repository_id: i64,
    #[schemars(description = "artifact paths to delete, at most 200 per call")]
    pub paths: Vec<String>,
}

/// Arguments of `forklift_list_approvals`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListApprovalsArgs {
    #[serde(default)]
    #[schemars(description = "filter by repository name")]
    pub repo: String,
    #[serde(default)]
    #[schemars(description = "filter by status: pending, approved or rejected")]
    pub status: String,
    #[serde(default)]
    #[schemars(description = "substring (or regex) filter on package")]
    pub query: String,
    #[serde(default)]
    #[schemars(description = "treat query as a regular expression")]
    pub regex: bool,
    #[serde(default)]
    #[schemars(description = "page size")]
    pub limit: i64,
    #[serde(default)]
    #[schemars(description = "pagination offset")]
    pub offset: i64,
}

/// Arguments of `forklift_get_approval`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ApprovalIdArgs {
    #[schemars(description = "numeric approval ID (from forklift_list_approvals)")]
    pub approval_id: i64,
}

/// Arguments of `forklift_approve_package` and `forklift_reject_package`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DecideApprovalArgs {
    #[schemars(description = "numeric approval ID")]
    pub approval_id: i64,
    #[serde(default)]
    #[schemars(description = "reviewer note recorded with the decision")]
    pub note: String,
}

/// Arguments of `forklift_approve_all_packages`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ApproveAllArgs {
    #[schemars(description = "repository name whose pending approvals are decided")]
    pub repo: String,
    #[serde(default)]
    #[schemars(description = "reviewer note recorded with each decision")]
    pub note: String,
    #[serde(default)]
    #[schemars(description = "approve only packages whose vulnerability scan is clean")]
    pub clean_only: bool,
}

/// Arguments of `forklift_create_approval`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateApprovalArgs {
    #[schemars(description = "repository name")]
    pub repo: String,
    #[schemars(description = "package coordinate, e.g. lodash or com.example:lib")]
    pub package: String,
    #[schemars(description = "initial status: pending, approved or rejected")]
    pub status: String,
    #[serde(default)]
    #[schemars(description = "note recorded with the entry")]
    pub note: String,
}

/// Arguments of `forklift_list_version_denies`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListVersionDeniesArgs {
    #[serde(default)]
    #[schemars(description = "filter by repository name")]
    pub repo: String,
    #[serde(default)]
    #[schemars(description = "page size")]
    pub limit: i64,
    #[serde(default)]
    #[schemars(description = "pagination offset")]
    pub offset: i64,
}

/// Arguments of `forklift_create_version_deny`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateVersionDenyArgs {
    #[schemars(description = "repository name")]
    pub repo: String,
    #[schemars(description = "package coordinate")]
    pub package: String,
    #[schemars(description = "exact version to block")]
    pub version: String,
    #[serde(default)]
    #[schemars(description = "why this version is blocked (poisoned release, IOC, ...)")]
    pub reason: String,
}

/// Arguments of `forklift_delete_version_deny`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct VersionDenyIdArgs {
    #[schemars(description = "numeric version-deny ID (from forklift_list_version_denies)")]
    pub deny_id: i64,
}

/// Arguments of `forklift_count_approvals`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CountApprovalsArgs {
    #[serde(default)]
    #[schemars(description = "filter by repository name")]
    pub repo: String,
    #[serde(default)]
    #[schemars(description = "filter by status: pending, approved or rejected")]
    pub status: String,
}

/// Arguments of every tool addressing one user by ID.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UserIdArgs {
    #[schemars(description = "numeric user ID (from forklift_list_users)")]
    pub user_id: i64,
}

/// Arguments of `forklift_create_user`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateUserArgs {
    #[schemars(description = "login name, unique")]
    pub username: String,
    #[serde(default)]
    #[schemars(description = "initial password; omit for robot accounts")]
    pub password: String,
    #[serde(default)]
    #[schemars(description = "email address")]
    pub email: String,
    #[serde(default)]
    #[schemars(
        with = "Vec<i64>",
        description = "role IDs to assign (from forklift_list_roles)"
    )]
    pub role_ids: Option<Vec<i64>>,
    #[serde(default)]
    #[schemars(description = "create a robot (machine) account")]
    pub robot: bool,
}

/// Arguments of `forklift_update_user`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateUserArgs {
    #[schemars(description = "numeric user ID")]
    pub user_id: i64,
    #[serde(default)]
    #[schemars(description = "new password")]
    pub password: String,
    #[serde(default)]
    #[schemars(description = "new email address")]
    pub email: String,
    #[serde(default)]
    #[schemars(with = "bool", description = "disable or re-enable the account")]
    pub disabled: Option<bool>,
    #[serde(default)]
    #[schemars(
        with = "bool",
        description = "enable or disable login lockout for the account"
    )]
    pub lockout_enabled: Option<bool>,
    #[serde(default)]
    #[schemars(with = "Vec<i64>", description = "replacement role ID list")]
    pub roles: Option<Vec<i64>>,
}

/// One repository permission granted by a role.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct RolePermission {
    #[schemars(description = "repository name glob the permission applies to, e.g. * or maven-*")]
    pub repo_pattern: String,
    #[schemars(
        description = "granted action: read, write, delete, admin, approve, audit or security"
    )]
    pub action: String,
}

/// Arguments of `forklift_create_role`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateRoleArgs {
    #[schemars(description = "role name, unique")]
    pub name: String,
    #[serde(default)]
    #[schemars(description = "human-readable description")]
    pub description: String,
    #[serde(default)]
    #[schemars(
        with = "Vec<RolePermission>",
        description = "repository permissions granted by the role"
    )]
    pub permissions: Option<Vec<RolePermission>>,
}

/// Arguments of `forklift_delete_role`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RoleIdArgs {
    #[schemars(description = "numeric role ID (from forklift_list_roles)")]
    pub role_id: i64,
}

/// Arguments of `forklift_create_group_mapping`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateGroupMappingArgs {
    #[schemars(description = "OIDC group name")]
    pub group_name: String,
    #[schemars(description = "role granted to members of the group")]
    pub role_id: i64,
}

/// Arguments of `forklift_delete_group_mapping`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GroupMappingIdArgs {
    #[schemars(description = "numeric group-mapping ID (from forklift_list_group_mappings)")]
    pub mapping_id: i64,
}

/// Arguments of `forklift_create_user_token`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateUserTokenArgs {
    #[schemars(description = "numeric user ID the token belongs to")]
    pub user_id: i64,
    #[schemars(description = "token name")]
    pub name: String,
    #[serde(default)]
    #[schemars(description = "what the token is for")]
    pub description: String,
    #[serde(default)]
    #[schemars(
        with = "Vec<String>",
        description = "token scopes as 'repo_pattern:action' entries, e.g. *:read"
    )]
    pub scopes: Option<Vec<String>>,
    #[serde(default)]
    #[schemars(description = "lifetime such as 720h; omit for no expiry")]
    pub expires_in: String,
}

/// Arguments of `forklift_revoke_user_token`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UserTokenIdArgs {
    #[schemars(description = "numeric user ID the token belongs to")]
    pub user_id: i64,
    #[schemars(description = "numeric token ID (from forklift_list_user_tokens)")]
    pub token_id: i64,
}

/// Arguments of `forklift_list_coverage_history`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CoverageHistoryArgs {
    #[serde(default)]
    #[schemars(description = "trend window in days, 1-90 (default: full retention)")]
    pub days: i64,
}

/// Arguments of the per-project coverage tools.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CoverageProjectArgs {
    #[schemars(description = "GitLab project path, e.g. group/subgroup/project")]
    pub path: String,
}

/// Arguments of `forklift_get_coverage_project_pipeline`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CoveragePipelineArgs {
    #[schemars(description = "GitLab project path, e.g. group/subgroup/project")]
    pub path: String,
    #[serde(default, rename = "ref")]
    #[schemars(
        description = "branch or tag to read the CI definition from (default: the branch the verdict came from)"
    )]
    pub ref_: String,
}

/// Arguments of `forklift_set_coverage_project_muted`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CoverageMuteArgs {
    #[schemars(description = "GitLab project path, e.g. group/subgroup/project")]
    pub path: String,
    #[schemars(
        description = "true excludes the whole project from the measurement, false brings every check back"
    )]
    pub muted: bool,
    #[serde(default)]
    #[schemars(
        with = "Vec<String>",
        description = "checks to mute (ci, registry); the whole state, so a check left out is unmuted. Overrides muted"
    )]
    pub scopes: Option<Vec<String>>,
}

/// Arguments of `forklift_update_coverage_settings`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CoverageSettingsArgs {
    #[serde(default)]
    #[schemars(description = "external forklift host a project must reference to count")]
    pub forklift_host: String,
    #[serde(default)]
    #[schemars(
        with = "Vec<String>",
        description = "GitLab topics whose projects are excluded from the scan"
    )]
    pub exclude_topics: Option<Vec<String>>,
    #[serde(default)]
    #[schemars(description = "cron expression for the scheduled scan")]
    pub scan_cron: String,
    #[serde(default)]
    #[schemars(description = "IANA timezone the cron runs in, e.g. Asia/Seoul")]
    pub timezone: String,
    #[serde(default)]
    #[schemars(with = "bool", description = "run the scheduled scan")]
    pub auto_scan_enabled: Option<bool>,
    #[serde(default)]
    #[schemars(
        with = "bool",
        description = "send the coverage report after a scheduled scan"
    )]
    pub report_enabled: Option<bool>,
    #[serde(default)]
    #[schemars(description = "notification receiver name the report is sent to")]
    pub receiver: String,
    #[serde(default)]
    #[schemars(
        with = "bool",
        description = "skip the report when every project is covered"
    )]
    pub skip_when_full_coverage: Option<bool>,
    #[serde(default)]
    #[schemars(description = "maximum branches inspected per project")]
    pub max_branches: i64,
    #[serde(default)]
    #[schemars(description = "only consider projects with activity in the last N days")]
    pub since_days: i64,
    #[serde(default)]
    #[schemars(
        with = "bool",
        description = "use the GitLab search API instead of reading each CI file"
    )]
    pub use_search: Option<bool>,
}

/// Arguments of `forklift_check_coverage_host`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CoverageHostCheckArgs {
    #[schemars(description = "host to validate and resolve, e.g. forklift.example.com")]
    pub forklift_host: String,
}

// --- the server -------------------------------------------------------------

/// The MCP server exposing the forklift management API as tools.
pub struct Server {
    client: Arc<Client>,
    version: String,
    metrics: Option<Arc<Metrics>>,
    tools: Vec<Tool>,
}

impl Server {
    /// Builds the MCP server. `metrics` may be `None` to disable tool-call
    /// instrumentation.
    pub fn new(client: Arc<Client>, version: &str, metrics: Option<Arc<Metrics>>) -> Arc<Server> {
        let mut tools = Server::tool_definitions();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        Arc::new(Server {
            client,
            version: version.to_string(),
            metrics,
            tools,
        })
    }

    /// Every tool this server exposes, sorted by name.
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    /// The input schema of the tool called `name`, if it exists.
    fn input_schema(&self, name: &str) -> Option<&JsonObject> {
        self.tools
            .binary_search_by(|t| t.name.as_ref().cmp(name))
            .ok()
            .map(|i| self.tools[i].input_schema.as_ref())
    }

    /// Runs one tool by name with the caller's raw `Authorization` header, and
    /// records the tool-call metrics.
    ///
    /// The MCP-level entry point is [`ServerHandler::call_tool`], which resolves the header off
    /// the HTTP request and then calls this.
    pub async fn call(
        &self,
        auth: Option<&str>,
        name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ErrorData> {
        let start = Instant::now();
        let res = self
            .dispatch(auth, name, arguments.unwrap_or_default())
            .await;
        if let Some(metrics) = &self.metrics {
            let outcome = match &res {
                Err(_) => OUTCOME_ERROR,
                Ok(r) if r.is_error == Some(true) => OUTCOME_TOOL_ERROR,
                Ok(_) => OUTCOME_OK,
            };
            metrics.record_tool_call(name, outcome, start.elapsed());
        }
        res
    }
}

/// Builds one tool's input schema, validating that its root is an object.
fn tool_input_schema<T: JsonSchema + std::any::Any>() -> Arc<JsonObject> {
    let schema = rmcp::handler::server::tool::schema_for_input::<T>()
        .expect("tool input schema must have root type object");
    let mut object = Value::Object(schema.as_ref().clone());
    strip_defaults(&mut object);
    match object {
        Value::Object(object) => Arc::new(object),
        _ => schema,
    }
}

/// Coerces argument values into the types the tool's input schema declares.
///
/// Some MCP clients (Google ADK's `adk-mcp-client`, and kagent on top of it)
/// serialise every tool argument as a string, so `"7"` arrives where the schema
/// says integer and `"true"` where it says boolean. serde would reject the whole
/// call with `invalid type: string "7", expected i64`; this rewrites such values
/// in place first. Values that already have the declared type, or that cannot
/// be converted, are left untouched so the normal deserialisation error still
/// surfaces.
pub(crate) fn coerce_arguments(schema: &JsonObject, arguments: &mut JsonObject) {
    let Some(Value::Object(properties)) = schema.get("properties") else {
        return;
    };
    for (key, value) in arguments.iter_mut() {
        if let Some(property) = properties.get(key) {
            coerce_value(property, value);
        }
    }
}

/// Coerces one value towards `schema`'s declared `type`.
fn coerce_value(schema: &Value, value: &mut Value) {
    let types: Vec<&str> = match schema.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => return,
    };
    let has = |t: &str| types.contains(&t);

    if let Value::String(s) = value {
        let s = s.trim();
        let converted = if has("integer") {
            parse_integer(s).map(Value::from)
        } else if has("number") {
            s.parse::<f64>().ok().map(Value::from)
        } else if has("boolean") {
            match s.to_ascii_lowercase().as_str() {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                _ => None,
            }
        } else if has("array") {
            serde_json::from_str::<Value>(s)
                .ok()
                .filter(Value::is_array)
        } else if has("object") {
            serde_json::from_str::<Value>(s)
                .ok()
                .filter(Value::is_object)
        } else {
            None
        };
        if let Some(converted) = converted {
            *value = converted;
        }
    } else if has("integer") && value.is_f64() {
        // `7.0` for an integer field: serde_json refuses floats for i64.
        if let Some(i) = value.as_f64().and_then(f64_to_i64) {
            *value = Value::from(i);
        }
    }

    if let (Value::Array(items), Some(item_schema)) = (&mut *value, schema.get("items")) {
        for item in items {
            coerce_value(item_schema, item);
        }
    }
}

/// Parses `"7"` and `"7.0"` as 7; anything with a fractional part is rejected.
fn parse_integer(s: &str) -> Option<i64> {
    s.parse::<i64>()
        .ok()
        .or_else(|| s.parse::<f64>().ok().and_then(f64_to_i64))
}

fn f64_to_i64(n: f64) -> Option<i64> {
    (n.is_finite() && n.fract() == 0.0 && n.abs() < 9.007_199_254_740_992e15).then_some(n as i64)
}

/// Removes schemars' `default` annotations.
fn strip_defaults(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("default");
            for (_, v) in map.iter_mut() {
                strip_defaults(v);
            }
        }
        Value::Array(items) => {
            for v in items {
                strip_defaults(v);
            }
        }
        _ => {}
    }
}

/// Declares the whole tool surface once, so the `tools/list` table and the
/// `tools/call` dispatch table can never drift apart.
macro_rules! tool_surface {
    ($( $name:literal, $args:ty, $handler:ident, $desc:literal; )*) => {
        impl Server {
            fn tool_definitions() -> Vec<Tool> {
                vec![$( Tool::new($name, $desc, tool_input_schema::<$args>()) ),*]
            }

            async fn dispatch(
                &self,
                auth: Option<&str>,
                name: &str,
                mut arguments: JsonObject,
            ) -> Result<CallToolResult, ErrorData> {
                if let Some(schema) = self.input_schema(name) {
                    coerce_arguments(schema, &mut arguments);
                }
                match name {
                    $( $name => self.$handler(auth, parse_json_object::<$args>(arguments)?).await, )*
                    _ => Err(ErrorData::invalid_params("tool not found", None)),
                }
            }
        }
    };
}

tool_surface! {
    // --- meta ---------------------------------------------------------------
    "forklift_version", NoArgs, forklift_version,
        "Get the forklift server version, commit and whether OIDC login is enabled.";
    "forklift_whoami", NoArgs, forklift_whoami,
        "Identify the calling principal: username, auth source and whether it holds admin, approver, auditor or security rights. Use to learn what the other tools will be allowed to do.";
    "forklift_get_landing_stats", NoArgs, forklift_get_landing_stats,
        "Get coarse instance-wide totals: repository count and artifact count.";
    "forklift_list_repository_names", NoArgs, forklift_list_repository_names,
        "List repository names with format and type only, cheaper than forklift_list_repositories when IDs and config are not needed.";
    "forklift_search", SearchArgs, forklift_search,
        "Global search across repositories, artifacts, artifact labels, pending approvals, users and roles.";

    // --- repositories -------------------------------------------------------
    "forklift_list_repositories", NoArgs, forklift_list_repositories,
        "List all repositories with artifact counts, sizes, pending approval counts and scan aggregates.";
    "forklift_get_repository", RepoIdArgs, forklift_get_repository,
        "Get one repository including its full config (cache, security policies, upstream auth masked).";
    "forklift_create_repository", CreateRepositoryArgs, forklift_create_repository,
        "Create a repository (hosted, proxy or group) for one package format.";
    "forklift_update_repository", UpdateRepositoryArgs, forklift_update_repository,
        "Update a repository's upstream URL and/or config. Omitted fields are left unchanged.";
    "forklift_delete_repository", RepoIdArgs, forklift_delete_repository,
        "Delete a repository and its artifact index. Irreversible; seeded default repositories cannot be deleted.";
    "forklift_set_repository_disabled", SetRepositoryDisabledArgs, forklift_set_repository_disabled,
        "Disable or re-enable a repository. A disabled repository rejects all package traffic.";
    "forklift_update_repository_security", RepositorySecurityArgs, forklift_update_repository_security,
        "Replace a repository's security policy config: age policy, package approval, vulnerability policy, license policy, IP ACL and notifications.";
    "forklift_list_artifacts", ListArtifactsArgs, forklift_list_artifacts,
        "Browse or search artifacts in one repository, with pagination.";
    "forklift_delete_artifacts", DeleteArtifactsArgs, forklift_delete_artifacts,
        "Delete artifacts matching a path glob from one repository. Irreversible.";
    "forklift_list_audit_logs", ListAuditLogsArgs, forklift_list_audit_logs,
        "Read a repository's audit log: downloads, uploads, deletes and config changes with user, status and client IP.";
    "forklift_get_upstream_health", RepoIdArgs, forklift_get_upstream_health,
        "Probe a proxy repository's upstream with the stored credentials and report whether it is reachable and what status it answers.";
    "forklift_list_oci_tags", RepoIdArgs, forklift_list_oci_tags,
        "List an OCI repository's tags with manifest digest, kind (image, index, chart), platforms, size, push time and labels. Empty for non-OCI repositories.";
    "forklift_get_oci_detail", OciDetailArgs, forklift_get_oci_detail,
        "Inspect one OCI artifact by name and tag or digest: overview, raw manifest and config, image config summary, index children, chart values and README.";
    "forklift_list_dangling_artifacts", RepoIdArgs, forklift_list_dangling_artifacts,
        "List a repository's artifacts whose blob bytes are missing from storage (would 5xx on download).";
    "forklift_list_artifact_labels", ArtifactLabelsArgs, forklift_list_artifact_labels,
        "List the labels attached to one artifact.";
    "forklift_bulk_label_artifacts", ArtifactBulkLabelArgs, forklift_bulk_label_artifacts,
        "Add or remove one label across many artifacts in a single call, up to 200 paths. Each path is judged on its own permission (admin on the repository, or having uploaded that artifact) and the paths that were refused come back in failed rather than failing the batch.";
    "forklift_bulk_delete_artifacts", ArtifactBulkDeleteArgs, forklift_bulk_delete_artifacts,
        "Delete many artifacts by exact path in a single call, up to 200 paths. Admin only. A path that refuses is reported in failed rather than failing the batch. Use forklift_delete_artifacts to purge by pattern instead.";
    "forklift_preview_repository_alarm", RepoIdArgs, forklift_preview_repository_alarm,
        "Render the approval alarm a repository would send, built from its current pending approvals, and the receivers it would target, without sending. Admin only.";
    "forklift_get_upload", UploadArgs, forklift_get_upload,
        "Get the state of one artifact upload session: received bytes, expiry and target path.";
    "forklift_list_repository_permissions", RepoIdArgs, forklift_list_repository_permissions,
        "List the role permissions that grant access to a repository: granting role, matched pattern, actions and how many users hold the role. Admin or auditor.";
    "forklift_list_repository_tokens", RepoIdArgs, forklift_list_repository_tokens,
        "List the personal access tokens that can reach a repository, scoped or unscoped, with their owners. Admin or auditor.";

    // --- approvals and version denies ---------------------------------------
    "forklift_list_approvals", ListApprovalsArgs, forklift_list_approvals,
        "List package approval requests (the quarantine queue), filterable by repository, status and package.";
    "forklift_count_approvals", CountApprovalsArgs, forklift_count_approvals,
        "Count approval requests matching a repository and status filter, without listing them.";
    "forklift_list_pending_approval_repos", NoArgs, forklift_list_pending_approval_repos,
        "List repositories that have a pending approval queue, with how many packages are pending and how many of those are vulnerability-clean.";
    "forklift_get_approval", ApprovalIdArgs, forklift_get_approval,
        "Get one approval request with reviewers, upstream URLs and notified receivers.";
    "forklift_approve_package", DecideApprovalArgs, forklift_approve_package,
        "Approve a pending package approval request so the proxy may serve the package.";
    "forklift_reject_package", DecideApprovalArgs, forklift_reject_package,
        "Reject a pending package approval request so the proxy keeps blocking the package.";
    "forklift_approve_all_packages", ApproveAllArgs, forklift_approve_all_packages,
        "Approve every pending approval in one repository at once, optionally only packages with a clean vulnerability scan.";
    "forklift_create_approval", CreateApprovalArgs, forklift_create_approval,
        "Create an approval entry for a package ahead of its first request (pre-approve or pre-reject).";
    "forklift_list_version_denies", ListVersionDeniesArgs, forklift_list_version_denies,
        "List version denies: exact package versions blocked while the package itself stays approved.";
    "forklift_create_version_deny", CreateVersionDenyArgs, forklift_create_version_deny,
        "Block one exact package version (poisoned release, IOC) and revoke cached copies immediately.";
    "forklift_delete_version_deny", VersionDenyIdArgs, forklift_delete_version_deny,
        "Remove a version deny so the version can be served again.";

    // --- users, roles, group mappings, tokens -------------------------------
    "forklift_list_users", NoArgs, forklift_list_users,
        "List all user accounts with roles, sources (local/OIDC), lock state and token counts. Admin only.";
    "forklift_create_user", CreateUserArgs, forklift_create_user,
        "Create a local user or robot account. Admin only.";
    "forklift_update_user", UpdateUserArgs, forklift_update_user,
        "Update a user's password, email, disabled state, lockout or role list. Omitted fields are left unchanged. Admin only.";
    "forklift_delete_user", UserIdArgs, forklift_delete_user,
        "Delete a user account and its tokens. Irreversible. Admin only.";
    "forklift_list_roles", NoArgs, forklift_list_roles,
        "List RBAC roles with their repository permissions and user counts.";
    "forklift_create_role", CreateRoleArgs, forklift_create_role,
        "Create an RBAC role granting repository-pattern permissions. Admin only.";
    "forklift_delete_role", RoleIdArgs, forklift_delete_role,
        "Delete an RBAC role. Users holding it lose the role's permissions. Admin only.";
    "forklift_list_group_mappings", NoArgs, forklift_list_group_mappings,
        "List OIDC group-to-role mappings.";
    "forklift_create_group_mapping", CreateGroupMappingArgs, forklift_create_group_mapping,
        "Map an OIDC group to a role so group members inherit it on login. Admin only.";
    "forklift_delete_group_mapping", GroupMappingIdArgs, forklift_delete_group_mapping,
        "Remove an OIDC group-to-role mapping. Admin only.";
    "forklift_list_user_tokens", UserIdArgs, forklift_list_user_tokens,
        "List a user's personal access tokens (metadata only, never the secret). Admin only.";
    "forklift_create_user_token", CreateUserTokenArgs, forklift_create_user_token,
        "Create a personal access token for a user. The plaintext token is returned once, only here. Admin only.";
    "forklift_list_my_tokens", NoArgs, forklift_list_my_tokens,
        "List the personal access tokens owned by the calling principal.";
    "forklift_revoke_user_token", UserTokenIdArgs, forklift_revoke_user_token,
        "Revoke (delete) a user's personal access token. Admin only.";

    // --- coverage -----------------------------------------------------------
    "forklift_get_coverage", NoArgs, forklift_get_coverage,
        "Get the GitLab build coverage dashboard: how many projects build through forklift, per-project verdicts, scan state and schedule.";
    "forklift_list_coverage_groups", NoArgs, forklift_list_coverage_groups,
        "List coverage per GitLab group, worst coverage first.";
    "forklift_list_coverage_history", CoverageHistoryArgs, forklift_list_coverage_history,
        "Get the coverage trend over the last N days.";
    "forklift_get_coverage_project", CoverageProjectArgs, forklift_get_coverage_project,
        "Get one project's coverage detail: verdict, branch, matched references and mute state. Falls back to a GitLab lookup for projects the last scan did not cover.";
    "forklift_get_coverage_project_last_commit", CoverageProjectArgs, forklift_get_coverage_project_last_commit,
        "Get the tip commit of the branch a project's coverage verdict came from.";
    "forklift_get_coverage_project_pipeline", CoveragePipelineArgs, forklift_get_coverage_project_pipeline,
        "Read a project's GitLab CI definition (.gitlab-ci.yml and includes) as the scan sees it. Admin only.";
    "forklift_set_coverage_project_muted", CoverageMuteArgs, forklift_set_coverage_project_muted,
        "Mute a project so it no longer counts towards coverage, or mute only one half of its wiring (ci, registry) so that check stops being required. Unmute by sending muted=false or an empty scopes list. Admin only.";
    "forklift_get_coverage_settings", NoArgs, forklift_get_coverage_settings,
        "Get coverage scan settings: forklift host, GitLab URL, excluded topics, schedule, report receiver and scan limits. Admin only.";
    "forklift_update_coverage_settings", CoverageSettingsArgs, forklift_update_coverage_settings,
        "Update coverage scan settings. Omitted fields are left unchanged. Admin only.";
    "forklift_check_coverage_host", CoverageHostCheckArgs, forklift_check_coverage_host,
        "Check whether a forklift host value is well-formed and resolves in DNS, before saving it as the coverage target. Admin only.";
    "forklift_get_coverage_gitlab_check", NoArgs, forklift_get_coverage_gitlab_check,
        "Check whether the configured GitLab instance answers and accepts the scan token. Admin only.";
    "forklift_start_coverage_scan", NoArgs, forklift_start_coverage_scan,
        "Start a coverage scan now. Fails with 409 while a scan is already running. Admin only.";
    "forklift_preview_coverage_alarm", NoArgs, forklift_preview_coverage_alarm,
        "Render the coverage report as it would be sent to the configured receiver, without sending it. Admin only.";
    "forklift_send_coverage_alarm", NoArgs, forklift_send_coverage_alarm,
        "Send the coverage report to the configured receiver now. Notifies real people; use only when asked to send it. Admin only.";

    // --- operations ---------------------------------------------------------
    "forklift_list_notification_receivers", NoArgs, forklift_list_notification_receivers,
        "List notification receivers (Slack, webhook, ...) that repository and coverage alarms can be sent to. Admin or auditor.";
    "forklift_get_announcement", NoArgs, forklift_get_announcement,
        "Get the site-wide announcement shown to every signed-in user (Markdown source; empty when none is set).";
    "forklift_get_storage_status", NoArgs, forklift_get_storage_status,
        "Get storage backend status: backend kind, blob counts and bytes, volume or bucket fullness, dangling blobs. Admin only.";
    "forklift_get_ha_status", NoArgs, forklift_get_ha_status,
        "Get high-availability status: mode, current leader, lease and fencing token. Admin only.";
    "forklift_ha_step_down", NoArgs, forklift_ha_step_down,
        "Ask the current HA leader to step down and trigger a failover. Disruptive; use only when asked to fail over. Admin only.";
}

/// Shorthand for one proxied call with no query and no body.
macro_rules! plain {
    ($self:ident, $auth:ident, $method:literal, $path:expr) => {
        result(
            $self
                .client
                .do_request($auth, $method, &$path, &Query::new(), None)
                .await,
        )
    };
}

// --- meta -------------------------------------------------------------------

impl Server {
    async fn forklift_version(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", "/api/v1/version")
    }

    async fn forklift_whoami(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", "/api/v1/me")
    }

    async fn forklift_get_landing_stats(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", "/api/v1/stats/landing")
    }

    async fn forklift_list_repository_names(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", "/api/v1/repository-names")
    }

    async fn forklift_search(
        &self,
        auth: Option<&str>,
        a: SearchArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let q = Query::new().str("q", &a.query).num("limit", a.limit);
        result(
            self.client
                .do_request(auth, "GET", "/api/v1/search", &q, None)
                .await,
        )
    }
}

// --- repositories -----------------------------------------------------------

impl Server {
    async fn forklift_list_repositories(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", "/api/v1/repositories")
    }

    async fn forklift_get_repository(
        &self,
        auth: Option<&str>,
        a: RepoIdArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", repo_path(a.repository_id, ""))
    }

    async fn forklift_create_repository(
        &self,
        auth: Option<&str>,
        a: CreateRepositoryArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let mut b = Body::new()
            .set("name", a.name)
            .set("format", a.format)
            .set("type", a.type_)
            .str("upstream_url", &a.upstream_url);
        if let Some(config) = a.config {
            b = b.set("config", Value::Object(config));
        }
        result(
            self.client
                .do_request(
                    auth,
                    "POST",
                    "/api/v1/repositories",
                    &Query::new(),
                    Some(&b),
                )
                .await,
        )
    }

    async fn forklift_update_repository(
        &self,
        auth: Option<&str>,
        a: UpdateRepositoryArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let mut b = Body::new().str("upstream_url", &a.upstream_url);
        if let Some(config) = a.config {
            b = b.set("config", Value::Object(config));
        }
        result(
            self.client
                .do_request(
                    auth,
                    "PUT",
                    &repo_path(a.repository_id, ""),
                    &Query::new(),
                    Some(&b),
                )
                .await,
        )
    }

    async fn forklift_delete_repository(
        &self,
        auth: Option<&str>,
        a: RepoIdArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "DELETE", repo_path(a.repository_id, ""))
    }

    async fn forklift_set_repository_disabled(
        &self,
        auth: Option<&str>,
        a: SetRepositoryDisabledArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let b = Body::new().set("disabled", a.disabled);
        result(
            self.client
                .do_request(
                    auth,
                    "POST",
                    &repo_path(a.repository_id, "/disabled"),
                    &Query::new(),
                    Some(&b),
                )
                .await,
        )
    }

    async fn forklift_update_repository_security(
        &self,
        auth: Option<&str>,
        a: RepositorySecurityArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let b = Body::new().set("config", Value::Object(a.config));
        result(
            self.client
                .do_request(
                    auth,
                    "PUT",
                    &repo_path(a.repository_id, "/security"),
                    &Query::new(),
                    Some(&b),
                )
                .await,
        )
    }

    async fn forklift_list_artifacts(
        &self,
        auth: Option<&str>,
        a: ListArtifactsArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let q = Query::new()
            .str("q", &a.query)
            .boolean("regex", a.regex)
            .num("limit", a.limit)
            .num("offset", a.offset);
        result(
            self.client
                .do_request(
                    auth,
                    "GET",
                    &repo_path(a.repository_id, "/artifacts"),
                    &q,
                    None,
                )
                .await,
        )
    }

    async fn forklift_delete_artifacts(
        &self,
        auth: Option<&str>,
        a: DeleteArtifactsArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let q = Query::new().str("q", &a.query);
        result(
            self.client
                .do_request(
                    auth,
                    "DELETE",
                    &repo_path(a.repository_id, "/artifacts"),
                    &q,
                    None,
                )
                .await,
        )
    }

    async fn forklift_list_audit_logs(
        &self,
        auth: Option<&str>,
        a: ListAuditLogsArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let q = Query::new()
            .str("event", &a.event)
            .num("limit", a.limit)
            .num("offset", a.offset);
        result(
            self.client
                .do_request(
                    auth,
                    "GET",
                    &repo_path(a.repository_id, "/audit-logs"),
                    &q,
                    None,
                )
                .await,
        )
    }

    async fn forklift_get_upstream_health(
        &self,
        auth: Option<&str>,
        a: RepoIdArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(
            self,
            auth,
            "GET",
            repo_path(a.repository_id, "/upstream-health")
        )
    }

    async fn forklift_list_oci_tags(
        &self,
        auth: Option<&str>,
        a: RepoIdArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", repo_path(a.repository_id, "/oci-tags"))
    }

    async fn forklift_get_oci_detail(
        &self,
        auth: Option<&str>,
        a: OciDetailArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let q = Query::new().str("name", &a.name).str("ref", &a.ref_);
        result(
            self.client
                .do_request(
                    auth,
                    "GET",
                    &repo_path(a.repository_id, "/oci-detail"),
                    &q,
                    None,
                )
                .await,
        )
    }

    async fn forklift_list_dangling_artifacts(
        &self,
        auth: Option<&str>,
        a: RepoIdArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", repo_path(a.repository_id, "/dangling"))
    }

    async fn forklift_list_artifact_labels(
        &self,
        auth: Option<&str>,
        a: ArtifactLabelsArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let q = Query::new().str("path", &a.path);
        result(
            self.client
                .do_request(
                    auth,
                    "GET",
                    &repo_path(a.repository_id, "/artifacts/labels"),
                    &q,
                    None,
                )
                .await,
        )
    }

    async fn forklift_bulk_label_artifacts(
        &self,
        auth: Option<&str>,
        a: ArtifactBulkLabelArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let b = Body::new()
            .set("paths", a.paths)
            .set("label", a.label)
            .set("action", a.action);
        result(
            self.client
                .do_request(
                    auth,
                    "POST",
                    &repo_path(a.repository_id, "/artifacts/labels/bulk"),
                    &Query::new(),
                    Some(&b),
                )
                .await,
        )
    }

    async fn forklift_bulk_delete_artifacts(
        &self,
        auth: Option<&str>,
        a: ArtifactBulkDeleteArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let b = Body::new().set("paths", a.paths);
        result(
            self.client
                .do_request(
                    auth,
                    "POST",
                    &repo_path(a.repository_id, "/artifacts/bulk-delete"),
                    &Query::new(),
                    Some(&b),
                )
                .await,
        )
    }

    async fn forklift_preview_repository_alarm(
        &self,
        auth: Option<&str>,
        a: RepoIdArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(
            self,
            auth,
            "GET",
            repo_path(a.repository_id, "/notification/sample")
        )
    }

    async fn forklift_get_upload(
        &self,
        auth: Option<&str>,
        a: UploadArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let path = repo_path(
            a.repository_id,
            &format!("/uploads/{}", path_escape(&a.upload_id)),
        );
        plain!(self, auth, "GET", path)
    }

    async fn forklift_list_repository_permissions(
        &self,
        auth: Option<&str>,
        a: RepoIdArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(
            self,
            auth,
            "GET",
            repo_path(a.repository_id, "/permissions")
        )
    }

    async fn forklift_list_repository_tokens(
        &self,
        auth: Option<&str>,
        a: RepoIdArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", repo_path(a.repository_id, "/tokens"))
    }
}

// --- approvals and version denies -------------------------------------------

impl Server {
    async fn forklift_list_approvals(
        &self,
        auth: Option<&str>,
        a: ListApprovalsArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let q = Query::new()
            .str("repo", &a.repo)
            .str("status", &a.status)
            .str("q", &a.query)
            .boolean("regex", a.regex)
            .num("limit", a.limit)
            .num("offset", a.offset);
        result(
            self.client
                .do_request(auth, "GET", "/api/v1/approvals", &q, None)
                .await,
        )
    }

    async fn forklift_count_approvals(
        &self,
        auth: Option<&str>,
        a: CountApprovalsArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let q = Query::new().str("repo", &a.repo).str("status", &a.status);
        result(
            self.client
                .do_request(auth, "GET", "/api/v1/approvals/count", &q, None)
                .await,
        )
    }

    async fn forklift_list_pending_approval_repos(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", "/api/v1/approvals/pending-repos")
    }

    async fn forklift_get_approval(
        &self,
        auth: Option<&str>,
        a: ApprovalIdArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let path = format!("/api/v1/approvals/{}", a.approval_id);
        plain!(self, auth, "GET", path)
    }

    async fn forklift_approve_package(
        &self,
        auth: Option<&str>,
        a: DecideApprovalArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let b = Body::new().str("note", &a.note);
        let path = format!("/api/v1/approvals/{}/approve", a.approval_id);
        result(
            self.client
                .do_request(auth, "POST", &path, &Query::new(), Some(&b))
                .await,
        )
    }

    async fn forklift_reject_package(
        &self,
        auth: Option<&str>,
        a: DecideApprovalArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let b = Body::new().str("note", &a.note);
        let path = format!("/api/v1/approvals/{}/reject", a.approval_id);
        result(
            self.client
                .do_request(auth, "POST", &path, &Query::new(), Some(&b))
                .await,
        )
    }

    async fn forklift_approve_all_packages(
        &self,
        auth: Option<&str>,
        a: ApproveAllArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let mut b = Body::new().set("repo", a.repo).str("note", &a.note);
        if a.clean_only {
            b = b.set("clean_only", true);
        }
        result(
            self.client
                .do_request(
                    auth,
                    "POST",
                    "/api/v1/approvals/approve-all",
                    &Query::new(),
                    Some(&b),
                )
                .await,
        )
    }

    async fn forklift_create_approval(
        &self,
        auth: Option<&str>,
        a: CreateApprovalArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let b = Body::new()
            .set("repo", a.repo)
            .set("package", a.package)
            .set("status", a.status)
            .str("note", &a.note);
        result(
            self.client
                .do_request(auth, "POST", "/api/v1/approvals", &Query::new(), Some(&b))
                .await,
        )
    }

    async fn forklift_list_version_denies(
        &self,
        auth: Option<&str>,
        a: ListVersionDeniesArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let q = Query::new()
            .str("repo", &a.repo)
            .num("limit", a.limit)
            .num("offset", a.offset);
        result(
            self.client
                .do_request(auth, "GET", "/api/v1/version-denies", &q, None)
                .await,
        )
    }

    async fn forklift_create_version_deny(
        &self,
        auth: Option<&str>,
        a: CreateVersionDenyArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let b = Body::new()
            .set("repo", a.repo)
            .set("package", a.package)
            .set("version", a.version)
            .str("reason", &a.reason);
        result(
            self.client
                .do_request(
                    auth,
                    "POST",
                    "/api/v1/version-denies",
                    &Query::new(),
                    Some(&b),
                )
                .await,
        )
    }

    async fn forklift_delete_version_deny(
        &self,
        auth: Option<&str>,
        a: VersionDenyIdArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let path = format!("/api/v1/version-denies/{}", a.deny_id);
        plain!(self, auth, "DELETE", path)
    }
}

// --- users, roles, group mappings, tokens ------------------------------------

impl Server {
    async fn forklift_list_users(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", "/api/v1/users")
    }

    async fn forklift_create_user(
        &self,
        auth: Option<&str>,
        a: CreateUserArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let mut b = Body::new()
            .set("username", a.username)
            .str("password", &a.password)
            .str("email", &a.email);
        if let Some(role_ids) = a.role_ids
            && !role_ids.is_empty()
        {
            b = b.set("role_ids", role_ids);
        }
        if a.robot {
            b = b.set("robot", true);
        }
        result(
            self.client
                .do_request(auth, "POST", "/api/v1/users", &Query::new(), Some(&b))
                .await,
        )
    }

    async fn forklift_update_user(
        &self,
        auth: Option<&str>,
        a: UpdateUserArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let mut b = Body::new()
            .str("password", &a.password)
            .str("email", &a.email);
        if let Some(disabled) = a.disabled {
            b = b.set("disabled", disabled);
        }
        if let Some(lockout_enabled) = a.lockout_enabled {
            b = b.set("lockout_enabled", lockout_enabled);
        }
        if let Some(roles) = a.roles {
            b = b.set("roles", roles);
        }
        let path = format!("/api/v1/users/{}", a.user_id);
        result(
            self.client
                .do_request(auth, "PUT", &path, &Query::new(), Some(&b))
                .await,
        )
    }

    async fn forklift_delete_user(
        &self,
        auth: Option<&str>,
        a: UserIdArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let path = format!("/api/v1/users/{}", a.user_id);
        plain!(self, auth, "DELETE", path)
    }

    async fn forklift_list_roles(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", "/api/v1/roles")
    }

    async fn forklift_create_role(
        &self,
        auth: Option<&str>,
        a: CreateRoleArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let mut b = Body::new()
            .set("name", a.name)
            .str("description", &a.description);
        if let Some(permissions) = a.permissions
            && !permissions.is_empty()
        {
            let encoded = serde_json::to_value(&permissions)
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
            b = b.set("permissions", encoded);
        }
        result(
            self.client
                .do_request(auth, "POST", "/api/v1/roles", &Query::new(), Some(&b))
                .await,
        )
    }

    async fn forklift_delete_role(
        &self,
        auth: Option<&str>,
        a: RoleIdArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let path = format!("/api/v1/roles/{}", a.role_id);
        plain!(self, auth, "DELETE", path)
    }

    async fn forklift_list_group_mappings(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", "/api/v1/group-mappings")
    }

    async fn forklift_create_group_mapping(
        &self,
        auth: Option<&str>,
        a: CreateGroupMappingArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let b = Body::new()
            .set("group_name", a.group_name)
            .set("role_id", a.role_id);
        result(
            self.client
                .do_request(
                    auth,
                    "POST",
                    "/api/v1/group-mappings",
                    &Query::new(),
                    Some(&b),
                )
                .await,
        )
    }

    async fn forklift_delete_group_mapping(
        &self,
        auth: Option<&str>,
        a: GroupMappingIdArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let path = format!("/api/v1/group-mappings/{}", a.mapping_id);
        plain!(self, auth, "DELETE", path)
    }

    async fn forklift_list_user_tokens(
        &self,
        auth: Option<&str>,
        a: UserIdArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let path = format!("/api/v1/users/{}/tokens", a.user_id);
        plain!(self, auth, "GET", path)
    }

    async fn forklift_create_user_token(
        &self,
        auth: Option<&str>,
        a: CreateUserTokenArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let mut b = Body::new()
            .set("name", a.name)
            .str("description", &a.description)
            .str("expires_in", &a.expires_in);
        if let Some(scopes) = a.scopes
            && !scopes.is_empty()
        {
            b = b.set("scopes", scopes);
        }
        let path = format!("/api/v1/users/{}/tokens", a.user_id);
        result(
            self.client
                .do_request(auth, "POST", &path, &Query::new(), Some(&b))
                .await,
        )
    }

    async fn forklift_list_my_tokens(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", "/api/v1/tokens")
    }

    async fn forklift_revoke_user_token(
        &self,
        auth: Option<&str>,
        a: UserTokenIdArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let path = format!("/api/v1/users/{}/tokens/{}", a.user_id, a.token_id);
        plain!(self, auth, "DELETE", path)
    }
}

// --- coverage ---------------------------------------------------------------

impl Server {
    async fn forklift_get_coverage(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", "/api/v1/coverage")
    }

    async fn forklift_list_coverage_groups(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", "/api/v1/coverage/groups")
    }

    async fn forklift_list_coverage_history(
        &self,
        auth: Option<&str>,
        a: CoverageHistoryArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let q = Query::new().num("days", a.days);
        result(
            self.client
                .do_request(auth, "GET", "/api/v1/coverage/history", &q, None)
                .await,
        )
    }

    async fn forklift_get_coverage_project(
        &self,
        auth: Option<&str>,
        a: CoverageProjectArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let q = Query::new().str("path", &a.path);
        result(
            self.client
                .do_request(auth, "GET", "/api/v1/coverage/project", &q, None)
                .await,
        )
    }

    async fn forklift_get_coverage_project_last_commit(
        &self,
        auth: Option<&str>,
        a: CoverageProjectArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let q = Query::new().str("path", &a.path);
        result(
            self.client
                .do_request(
                    auth,
                    "GET",
                    "/api/v1/coverage/project/last-commit",
                    &q,
                    None,
                )
                .await,
        )
    }

    async fn forklift_get_coverage_project_pipeline(
        &self,
        auth: Option<&str>,
        a: CoveragePipelineArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let q = Query::new().str("path", &a.path).str("ref", &a.ref_);
        result(
            self.client
                .do_request(auth, "GET", "/api/v1/coverage/project/pipeline", &q, None)
                .await,
        )
    }

    async fn forklift_set_coverage_project_muted(
        &self,
        auth: Option<&str>,
        a: CoverageMuteArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let q = Query::new().str("path", &a.path);
        let payload = match a.scopes {
            Some(scopes) => Body::new().set("scopes", scopes),
            None => Body::new().set("muted", a.muted),
        };
        result(
            self.client
                .do_request(
                    auth,
                    "PUT",
                    "/api/v1/coverage/project/mute",
                    &q,
                    Some(&payload),
                )
                .await,
        )
    }

    async fn forklift_get_coverage_settings(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", "/api/v1/coverage/settings")
    }

    async fn forklift_update_coverage_settings(
        &self,
        auth: Option<&str>,
        a: CoverageSettingsArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let mut b = Body::new()
            .str("forklift_host", &a.forklift_host)
            .str("scan_cron", &a.scan_cron)
            .str("timezone", &a.timezone)
            .str("receiver", &a.receiver);
        if let Some(exclude_topics) = a.exclude_topics {
            b = b.set("exclude_topics", exclude_topics);
        }
        if let Some(v) = a.auto_scan_enabled {
            b = b.set("auto_scan_enabled", v);
        }
        if let Some(v) = a.report_enabled {
            b = b.set("report_enabled", v);
        }
        if let Some(v) = a.skip_when_full_coverage {
            b = b.set("skip_when_full_coverage", v);
        }
        if a.max_branches != 0 {
            b = b.set("max_branches", a.max_branches);
        }
        if a.since_days != 0 {
            b = b.set("since_days", a.since_days);
        }
        if let Some(v) = a.use_search {
            b = b.set("use_search", v);
        }
        result(
            self.client
                .do_request(
                    auth,
                    "PUT",
                    "/api/v1/coverage/settings",
                    &Query::new(),
                    Some(&b),
                )
                .await,
        )
    }

    async fn forklift_check_coverage_host(
        &self,
        auth: Option<&str>,
        a: CoverageHostCheckArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let b = Body::new().set("forklift_host", a.forklift_host);
        result(
            self.client
                .do_request(
                    auth,
                    "POST",
                    "/api/v1/coverage/settings/check-host",
                    &Query::new(),
                    Some(&b),
                )
                .await,
        )
    }

    async fn forklift_get_coverage_gitlab_check(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", "/api/v1/coverage/gitlab-check")
    }

    async fn forklift_start_coverage_scan(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "POST", "/api/v1/coverage/scan")
    }

    async fn forklift_preview_coverage_alarm(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", "/api/v1/coverage/notification/preview")
    }

    async fn forklift_send_coverage_alarm(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "POST", "/api/v1/coverage/notification/send")
    }
}

// --- operations -------------------------------------------------------------

impl Server {
    async fn forklift_list_notification_receivers(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", "/api/v1/notification/receivers")
    }

    async fn forklift_get_announcement(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", "/api/v1/announcement")
    }

    async fn forklift_get_storage_status(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", "/api/v1/storage")
    }

    async fn forklift_get_ha_status(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "GET", "/api/v1/ha")
    }

    async fn forklift_ha_step_down(
        &self,
        auth: Option<&str>,
        _a: NoArgs,
    ) -> Result<CallToolResult, ErrorData> {
        plain!(self, auth, "POST", "/api/v1/ha/step-down")
    }
}

// --- MCP wiring -------------------------------------------------------------

/// Reads the incoming MCP HTTP request's `Authorization` header, which the
/// streamable-HTTP transport parks in the request context extensions.
fn auth_header(context: &RequestContext<RoleServer>) -> Option<String> {
    context
        .extensions
        .get::<http::request::Parts>()
        .and_then(|parts| parts.headers.get(http::header::AUTHORIZATION))
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string())
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerInfo {
        let mut server_info = Implementation::new("forklift-mcp", self.version.clone());
        server_info.title = Some("forklift artifact repository".to_string());
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.protocol_version = ProtocolVersion::default();
        info.server_info = server_info;
        info.instructions = None;
        info
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult {
            tools: self.tools.clone(),
            ..Default::default()
        }))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|t| t.name == name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let auth = auth_header(&context);
        let tool = request.name.to_string();
        self.call(auth.as_deref(), &tool, request.arguments)
            .await
            .map(CallToolResponse::from)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::http::{HeaderMap, StatusCode, Uri};
    use rmcp::model::{CallToolResult, ContentBlock};

    use crate::mcp::client::Client;
    use crate::mcp::metrics::Metrics;
    use crate::mcp::server::Server;

    /// Builds the server under test. `metrics` may be `None`.
    pub(crate) fn connect(c: Arc<Client>, metrics: Option<Arc<Metrics>>) -> Arc<Server> {
        Server::new(c, "test", metrics)
    }

    /// reqwest is built with `rustls-no-provider`; the binary installs the process
    /// default, so tests have to do it themselves before building a [`Client`].
    pub(crate) fn install_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    pub(crate) async fn spawn_upstream(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test upstream");
        let addr = listener.local_addr().expect("test upstream addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// The text of a tool result's first content block.
    pub(crate) fn text_of(res: &CallToolResult) -> String {
        match res.content.first() {
            Some(ContentBlock::Text(t)) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    pub(crate) fn no_args() -> Option<rmcp::model::JsonObject> {
        Some(rmcp::model::JsonObject::new())
    }

    #[tokio::test]
    async fn tools_registered() {
        install_crypto_provider();
        let session = connect(Client::new("http://localhost:0", "", None), None);
        let tools = session.tools();
        assert!(
            tools.len() >= 30,
            "expected full admin tool surface, got {} tools",
            tools.len()
        );
        for tool in tools {
            assert!(
                tool.name.starts_with("forklift_"),
                "tool {:?} lacks forklift_ prefix",
                tool.name
            );
            assert!(
                tool.description.as_ref().is_some_and(|d| !d.is_empty()),
                "tool {:?} has no description",
                tool.name
            );
        }
    }

    #[tokio::test]
    async fn tool_call_proxies_upstream() {
        install_crypto_provider();
        let seen: Arc<parking_lot::Mutex<(String, String, String)>> = Arc::default();
        let app = Router::new().fallback({
            let seen = seen.clone();
            move |uri: Uri, headers: HeaderMap| {
                let seen = seen.clone();
                async move {
                    *seen.lock() = (
                        uri.path().to_string(),
                        uri.query().unwrap_or("").to_string(),
                        headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string(),
                    );
                    (
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        r#"{"query":"lodash"}"#,
                    )
                }
            }
        });
        let upstream = spawn_upstream(app).await;

        let session = connect(Client::new(&upstream, "fallback-token", None), None);
        let res = session
            .call(
                None,
                "forklift_search",
                Some(
                    serde_json::json!({"query": "lodash", "limit": 3})
                        .as_object()
                        .expect("object")
                        .clone(),
                ),
            )
            .await
            .expect("call tool");
        assert_ne!(
            res.is_error,
            Some(true),
            "unexpected tool error: {:?}",
            res.content
        );

        let (got_path, got_query, got_auth) = seen.lock().clone();
        assert_eq!(got_path, "/api/v1/search", "path = {got_path:?}");
        assert_eq!(got_query, "limit=3&q=lodash", "query = {got_query:?}");
        // No Authorization header on a direct call: the fallback token applies.
        assert_eq!(
            got_auth, "Bearer fallback-token",
            "auth = {got_auth:?}, want fallback bearer"
        );

        let text = text_of(&res);
        serde_json::from_str::<serde_json::Value>(&text).expect("tool result is not JSON");
    }

    #[tokio::test]
    async fn tool_call_surfaces_api_error() {
        install_crypto_provider();
        let app = Router::new().fallback(|| async { (StatusCode::UNAUTHORIZED, "unauthorized\n") });
        let upstream = spawn_upstream(app).await;

        let session = connect(Client::new(&upstream, "", None), None);
        let res = session
            .call(None, "forklift_list_repositories", no_args())
            .await
            .expect("call tool");
        assert_eq!(
            res.is_error,
            Some(true),
            "expected is_error for upstream 401"
        );
        let text = text_of(&res);
        assert!(
            text.contains("401"),
            "error text {text:?} does not mention status"
        );
    }

    #[test]
    fn coerce_arguments_follows_schema_types() {
        use crate::mcp::server::coerce_arguments;
        use serde_json::json;

        let schema = json!({
            "type": "object",
            "properties": {
                "id": {"type": "integer"},
                "opt_id": {"type": ["integer", "null"]},
                "ratio": {"type": "number"},
                "flag": {"type": "boolean"},
                "ids": {"type": "array", "items": {"type": "integer"}},
                "config": {"type": "object"},
                "name": {"type": "string"},
            }
        });
        let mut args = json!({
            "id": "7",
            "opt_id": "8.0",
            "ratio": "0.5",
            "flag": "TRUE",
            "ids": "[1, \"2\"]",
            "config": "{\"ttl\": 3}",
            "name": "42",
            "unknown": "9",
            "bad": "x",
        });
        coerce_arguments(
            schema.as_object().expect("schema"),
            args.as_object_mut().expect("args"),
        );
        assert_eq!(
            args,
            json!({
                "id": 7,
                "opt_id": 8,
                "ratio": 0.5,
                "flag": true,
                "ids": [1, 2],
                "config": {"ttl": 3},
                "name": "42",
                "unknown": "9",
                "bad": "x",
            })
        );

        // Not convertible: left as-is so serde reports the real type error.
        let mut args = json!({"id": "7.5", "flag": "yes", "ids": "nope"});
        coerce_arguments(
            schema.as_object().expect("schema"),
            args.as_object_mut().expect("args"),
        );
        assert_eq!(args, json!({"id": "7.5", "flag": "yes", "ids": "nope"}));
    }

    mod tool_routes {
        use std::sync::Arc;

        use axum::Router;
        use axum::body::Bytes;
        use axum::http::{Method, Uri};
        use serde_json::{Value, json};

        use crate::mcp::client::Client;
        use crate::mcp::server::tests::{connect, install_crypto_provider, spawn_upstream};

        /// What the fake forklift API saw for one tool call.
        #[derive(Debug, Default, Clone, PartialEq)]
        struct RecordedCall {
            method: String,
            path: String,
            query: String,
            body: Option<Value>,
        }

        /// Answers every request with an empty JSON object and reports what it was
        /// asked for, which is the whole contract of a tool: an agent's tool call has
        /// to land on the right API operation.
        async fn recording_upstream(got: Arc<parking_lot::Mutex<RecordedCall>>) -> String {
            let app = Router::new().fallback(move |method: Method, uri: Uri, raw: Bytes| {
                let got = got.clone();
                async move {
                    let body = if raw.is_empty() {
                        None
                    } else {
                        Some(
                            serde_json::from_slice::<Value>(&raw)
                                .unwrap_or_else(|_| panic!("{method} {uri} sent a non-JSON body")),
                        )
                    };
                    *got.lock() = RecordedCall {
                        method: method.to_string(),
                        path: uri.path().to_string(),
                        query: uri.query().unwrap_or("").to_string(),
                        body,
                    };
                    (
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        "{}",
                    )
                }
            });
            spawn_upstream(app).await
        }

        /// One row of the parity table: tool name, arguments and the upstream
        /// operation it must reach.
        struct Case {
            tool: &'static str,
            args: Value,
            method: &'static str,
            path: &'static str,
            query: &'static str,
            body: Option<Value>,
        }

        fn case(tool: &'static str, args: Value, method: &'static str, path: &'static str) -> Case {
            Case {
                tool,
                args,
                method,
                path,
                query: "",
                body: None,
            }
        }

        impl Case {
            fn query(mut self, q: &'static str) -> Case {
                self.query = q;
                self
            }
            fn body(mut self, b: Value) -> Case {
                self.body = Some(b);
                self
            }
        }

        /// Each tool is a thin mapping onto one management-API operation, and a wrong
        /// mapping is silent: the agent gets a plausible answer from the wrong
        /// resource, or a mutation lands somewhere unintended. This pins method, path,
        /// query and body for the whole surface, including the "omitted fields are left
        /// unchanged" rule that the update tools rely on.
        #[tokio::test]
        async fn tools_map_to_api_operations() {
            install_crypto_provider();
            let got: Arc<parking_lot::Mutex<RecordedCall>> = Arc::default();
            let upstream = recording_upstream(got.clone()).await;
            let session = connect(Client::new(&upstream, "token", None), None);

            let cases = vec![
                case("forklift_version", json!({}), "GET", "/api/v1/version"),
                case(
                    "forklift_list_artifacts",
                    json!({"repository_id": 7, "query": "lodash", "regex": true, "limit": 50, "offset": 100}),
                    "GET",
                    "/api/v1/repositories/7/artifacts",
                )
                .query("limit=50&offset=100&q=lodash&regex=true"),
                case(
                    "forklift_delete_artifacts",
                    json!({"repository_id": 7, "query": "lodash/*"}),
                    "DELETE",
                    "/api/v1/repositories/7/artifacts",
                )
                .query("q=lodash%2F%2A"),
                case(
                    "forklift_bulk_label_artifacts",
                    json!({"repository_id": 7, "paths": ["a.tgz", "b.tgz"], "label": "keep", "action": "add"}),
                    "POST",
                    "/api/v1/repositories/7/artifacts/labels/bulk",
                )
                .body(json!({"paths": ["a.tgz", "b.tgz"], "label": "keep", "action": "add"})),
                case(
                    "forklift_bulk_delete_artifacts",
                    json!({"repository_id": 7, "paths": ["a.tgz"]}),
                    "POST",
                    "/api/v1/repositories/7/artifacts/bulk-delete",
                )
                .body(json!({"paths": ["a.tgz"]})),
                case(
                    "forklift_list_audit_logs",
                    json!({"repository_id": 7, "event": "download", "limit": 10}),
                    "GET",
                    "/api/v1/repositories/7/audit-logs",
                )
                .query("event=download&limit=10"),
                case(
                    "forklift_create_repository",
                    json!({"name": "npm-proxy", "format": "npm", "type": "proxy",
                           "upstream_url": "https://registry.npmjs.org"}),
                    "POST",
                    "/api/v1/repositories",
                )
                .body(json!({"name": "npm-proxy", "format": "npm", "type": "proxy",
                             "upstream_url": "https://registry.npmjs.org"})),
                case(
                    "forklift_set_repository_disabled",
                    json!({"repository_id": 7, "disabled": true}),
                    "POST",
                    "/api/v1/repositories/7/disabled",
                )
                .body(json!({"disabled": true})),
                case(
                    "forklift_get_repository",
                    json!({"repository_id": 7}),
                    "GET",
                    "/api/v1/repositories/7",
                ),
                case(
                    "forklift_delete_repository",
                    json!({"repository_id": 7}),
                    "DELETE",
                    "/api/v1/repositories/7",
                ),
                case(
                    "forklift_list_approvals",
                    json!({"repo": "npmjs", "status": "pending", "query": "left-pad", "limit": 20}),
                    "GET",
                    "/api/v1/approvals",
                )
                .query("limit=20&q=left-pad&repo=npmjs&status=pending"),
                case(
                    "forklift_approve_package",
                    json!({"approval_id": 3, "note": "reviewed"}),
                    "POST",
                    "/api/v1/approvals/3/approve",
                )
                .body(json!({"note": "reviewed"})),
                case(
                    "forklift_reject_package",
                    json!({"approval_id": 3}),
                    "POST",
                    "/api/v1/approvals/3/reject",
                )
                .body(json!({})),
                case(
                    "forklift_approve_all_packages",
                    json!({"repo": "npmjs", "clean_only": true}),
                    "POST",
                    "/api/v1/approvals/approve-all",
                )
                .body(json!({"repo": "npmjs", "clean_only": true})),
                case(
                    "forklift_create_version_deny",
                    json!({"repo": "npmjs", "package": "left-pad", "version": "1.0.0", "reason": "poisoned"}),
                    "POST",
                    "/api/v1/version-denies",
                )
                .body(json!({"repo": "npmjs", "package": "left-pad", "version": "1.0.0", "reason": "poisoned"})),
                case(
                    "forklift_delete_version_deny",
                    json!({"deny_id": 11}),
                    "DELETE",
                    "/api/v1/version-denies/11",
                ),
                case(
                    "forklift_create_user",
                    json!({"username": "robot-ci", "robot": true, "role_ids": [2]}),
                    "POST",
                    "/api/v1/users",
                )
                .body(json!({"username": "robot-ci", "robot": true, "role_ids": [2]})),
                // An update carries only what was asked for: a false must still be sent
                // (it is a change), while an omitted field must not appear at all.
                case(
                    "forklift_update_user",
                    json!({"user_id": 5, "disabled": false}),
                    "PUT",
                    "/api/v1/users/5",
                )
                .body(json!({"disabled": false})),
                case(
                    "forklift_create_user_token",
                    json!({"user_id": 5, "name": "ci", "expires_in": "720h", "scopes": ["*:read"]}),
                    "POST",
                    "/api/v1/users/5/tokens",
                )
                .body(json!({"name": "ci", "expires_in": "720h", "scopes": ["*:read"]})),
                case(
                    "forklift_revoke_user_token",
                    json!({"user_id": 5, "token_id": 9}),
                    "DELETE",
                    "/api/v1/users/5/tokens/9",
                ),
                case(
                    "forklift_create_role",
                    json!({"name": "readers", "permissions": [{"repo_pattern": "maven-*", "action": "read"}]}),
                    "POST",
                    "/api/v1/roles",
                )
                .body(json!({"name": "readers",
                             "permissions": [{"repo_pattern": "maven-*", "action": "read"}]})),
                case(
                    "forklift_create_group_mapping",
                    json!({"group_name": "/devs", "role_id": 2}),
                    "POST",
                    "/api/v1/group-mappings",
                )
                .body(json!({"group_name": "/devs", "role_id": 2})),
                case(
                    "forklift_list_user_tokens",
                    json!({"user_id": 5}),
                    "GET",
                    "/api/v1/users/5/tokens",
                ),
                case("forklift_whoami", json!({}), "GET", "/api/v1/me"),
                case(
                    "forklift_get_landing_stats",
                    json!({}),
                    "GET",
                    "/api/v1/stats/landing",
                ),
                case(
                    "forklift_list_repository_names",
                    json!({}),
                    "GET",
                    "/api/v1/repository-names",
                ),
                case(
                    "forklift_preview_repository_alarm",
                    json!({"repository_id": 7}),
                    "GET",
                    "/api/v1/repositories/7/notification/sample",
                ),
                case(
                    "forklift_get_upload",
                    json!({"repository_id": 7, "upload_id": "u-123"}),
                    "GET",
                    "/api/v1/repositories/7/uploads/u-123",
                ),
                case(
                    "forklift_get_upstream_health",
                    json!({"repository_id": 7}),
                    "GET",
                    "/api/v1/repositories/7/upstream-health",
                ),
                case(
                    "forklift_list_oci_tags",
                    json!({"repository_id": 7}),
                    "GET",
                    "/api/v1/repositories/7/oci-tags",
                ),
                case(
                    "forklift_get_oci_detail",
                    json!({"repository_id": 7, "name": "library/nginx", "ref": "1.27"}),
                    "GET",
                    "/api/v1/repositories/7/oci-detail",
                )
                .query("name=library%2Fnginx&ref=1.27"),
                case(
                    "forklift_list_dangling_artifacts",
                    json!({"repository_id": 7}),
                    "GET",
                    "/api/v1/repositories/7/dangling",
                ),
                case(
                    "forklift_list_artifact_labels",
                    json!({"repository_id": 7, "path": "lodash/-/lodash-4.17.21.tgz"}),
                    "GET",
                    "/api/v1/repositories/7/artifacts/labels",
                )
                .query("path=lodash%2F-%2Flodash-4.17.21.tgz"),
                case(
                    "forklift_list_repository_permissions",
                    json!({"repository_id": 7}),
                    "GET",
                    "/api/v1/repositories/7/permissions",
                ),
                case(
                    "forklift_list_repository_tokens",
                    json!({"repository_id": 7}),
                    "GET",
                    "/api/v1/repositories/7/tokens",
                ),
                case(
                    "forklift_count_approvals",
                    json!({"repo": "npm-proxy", "status": "pending"}),
                    "GET",
                    "/api/v1/approvals/count",
                )
                .query("repo=npm-proxy&status=pending"),
                case(
                    "forklift_list_pending_approval_repos",
                    json!({}),
                    "GET",
                    "/api/v1/approvals/pending-repos",
                ),
                case("forklift_list_my_tokens", json!({}), "GET", "/api/v1/tokens"),
                case(
                    "forklift_list_notification_receivers",
                    json!({}),
                    "GET",
                    "/api/v1/notification/receivers",
                ),
                case(
                    "forklift_get_announcement",
                    json!({}),
                    "GET",
                    "/api/v1/announcement",
                ),
                case("forklift_get_coverage", json!({}), "GET", "/api/v1/coverage"),
                case(
                    "forklift_list_coverage_groups",
                    json!({}),
                    "GET",
                    "/api/v1/coverage/groups",
                ),
                case(
                    "forklift_list_coverage_history",
                    json!({"days": 30}),
                    "GET",
                    "/api/v1/coverage/history",
                )
                .query("days=30"),
                case(
                    "forklift_get_coverage_project",
                    json!({"path": "team/app"}),
                    "GET",
                    "/api/v1/coverage/project",
                )
                .query("path=team%2Fapp"),
                case(
                    "forklift_get_coverage_project_last_commit",
                    json!({"path": "team/app"}),
                    "GET",
                    "/api/v1/coverage/project/last-commit",
                )
                .query("path=team%2Fapp"),
                case(
                    "forklift_get_coverage_project_pipeline",
                    json!({"path": "team/app", "ref": "main"}),
                    "GET",
                    "/api/v1/coverage/project/pipeline",
                )
                .query("path=team%2Fapp&ref=main"),
                case(
                    "forklift_set_coverage_project_muted",
                    json!({"path": "team/app", "muted": true}),
                    "PUT",
                    "/api/v1/coverage/project/mute",
                )
                .query("path=team%2Fapp")
                .body(json!({"muted": true})),
                case(
                    "forklift_get_coverage_settings",
                    json!({}),
                    "GET",
                    "/api/v1/coverage/settings",
                ),
                case(
                    "forklift_update_coverage_settings",
                    json!({"scan_cron": "0 9 * * 1", "auto_scan_enabled": false,
                           "exclude_topics": ["archived"], "max_branches": 3}),
                    "PUT",
                    "/api/v1/coverage/settings",
                )
                .body(json!({"scan_cron": "0 9 * * 1", "auto_scan_enabled": false,
                             "exclude_topics": ["archived"], "max_branches": 3})),
                case(
                    "forklift_check_coverage_host",
                    json!({"forklift_host": "forklift.example.com"}),
                    "POST",
                    "/api/v1/coverage/settings/check-host",
                )
                .body(json!({"forklift_host": "forklift.example.com"})),
                case(
                    "forklift_get_coverage_gitlab_check",
                    json!({}),
                    "GET",
                    "/api/v1/coverage/gitlab-check",
                ),
                case(
                    "forklift_start_coverage_scan",
                    json!({}),
                    "POST",
                    "/api/v1/coverage/scan",
                ),
                case(
                    "forklift_preview_coverage_alarm",
                    json!({}),
                    "GET",
                    "/api/v1/coverage/notification/preview",
                ),
                case(
                    "forklift_send_coverage_alarm",
                    json!({}),
                    "POST",
                    "/api/v1/coverage/notification/send",
                ),
                case(
                    "forklift_get_storage_status",
                    json!({}),
                    "GET",
                    "/api/v1/storage",
                ),
                case("forklift_get_ha_status", json!({}), "GET", "/api/v1/ha"),
                case(
                    "forklift_ha_step_down",
                    json!({}),
                    "POST",
                    "/api/v1/ha/step-down",
                ),
            ];

            for tc in &cases {
                *got.lock() = RecordedCall::default();
                let args = tc.args.as_object().expect("args object").clone();
                let res = session
                    .call(None, tc.tool, Some(args))
                    .await
                    .unwrap_or_else(|e| panic!("{}: call: {e}", tc.tool));
                assert_ne!(
                    res.is_error,
                    Some(true),
                    "{}: tool reported an error: {:?}",
                    tc.tool,
                    res.content
                );
                let recorded = got.lock().clone();
                assert_eq!(
                    (recorded.method.as_str(), recorded.path.as_str()),
                    (tc.method, tc.path),
                    "{}: reached {} {}, want {} {}",
                    tc.tool,
                    recorded.method,
                    recorded.path,
                    tc.method,
                    tc.path
                );
                assert_eq!(
                    recorded.query, tc.query,
                    "{}: query = {:?}, want {:?}",
                    tc.tool, recorded.query, tc.query
                );
                assert_eq!(
                    recorded.body, tc.body,
                    "{}: body = {:?}, want {:?}",
                    tc.tool, recorded.body, tc.body
                );
            }
        }

        /// Google ADK's MCP client (and kagent on top of it) sends every tool argument
        /// as a string: `"7"`, `"7.0"`, `"true"`, even `"[2]"`. The server must accept
        /// those exactly as if they had been typed, because the agent has no way to fix
        /// its own serialisation.
        #[tokio::test]
        async fn stringified_arguments_are_coerced_to_schema_types() {
            install_crypto_provider();
            let got: Arc<parking_lot::Mutex<RecordedCall>> = Arc::default();
            let upstream = recording_upstream(got.clone()).await;
            let session = connect(Client::new(&upstream, "token", None), None);

            let cases = vec![
                case(
                    "forklift_list_artifacts",
                    json!({"repository_id": "7", "query": "lodash", "regex": "true", "limit": "50", "offset": "7.0"}),
                    "GET",
                    "/api/v1/repositories/7/artifacts",
                )
                .query("limit=50&offset=7&q=lodash&regex=true"),
                case(
                    "forklift_list_coverage_history",
                    json!({"days": 30.0}),
                    "GET",
                    "/api/v1/coverage/history",
                )
                .query("days=30"),
                case(
                    "forklift_create_user",
                    json!({"username": "robot-ci", "robot": "True", "role_ids": "[2, \"3\"]"}),
                    "POST",
                    "/api/v1/users",
                )
                .body(json!({"username": "robot-ci", "robot": true, "role_ids": [2, 3]})),
                case(
                    "forklift_update_user",
                    json!({"user_id": " 5 ", "disabled": "false", "roles": ["1", 2]}),
                    "PUT",
                    "/api/v1/users/5",
                )
                .body(json!({"disabled": false, "roles": [1, 2]})),
            ];

            for tc in &cases {
                *got.lock() = RecordedCall::default();
                let args = tc.args.as_object().expect("args object").clone();
                let res = session
                    .call(None, tc.tool, Some(args))
                    .await
                    .unwrap_or_else(|e| panic!("{}: call: {e}", tc.tool));
                assert_ne!(
                    res.is_error,
                    Some(true),
                    "{}: tool reported an error: {:?}",
                    tc.tool,
                    res.content
                );
                let recorded = got.lock().clone();
                assert_eq!(
                    (
                        recorded.method.as_str(),
                        recorded.path.as_str(),
                        recorded.query.as_str()
                    ),
                    (tc.method, tc.path, tc.query),
                    "{}: reached {recorded:?}",
                    tc.tool,
                );
                assert_eq!(recorded.body, tc.body, "{}: body", tc.tool);
            }
        }

        /// A string that is not convertible must be left alone so the caller still
        /// gets the precise deserialisation error, not a silently defaulted value.
        #[tokio::test]
        async fn unconvertible_argument_still_rejected() {
            install_crypto_provider();
            let session = connect(Client::new("http://localhost:0", "", None), None);
            let err = session
                .call(
                    None,
                    "forklift_get_repository",
                    Some(
                        json!({"repository_id": "seven"})
                            .as_object()
                            .expect("object")
                            .clone(),
                    ),
                )
                .await
                .expect_err("non-numeric repository_id must be rejected");
            assert!(
                err.message.contains("expected i64"),
                "unexpected error: {err:?}"
            );
        }
    }
}
