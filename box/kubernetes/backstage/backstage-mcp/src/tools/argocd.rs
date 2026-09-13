//! ArgoCD ApplicationSet inventory, upstream chart versions and audit log.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{
    Paging, ToolError, ToolResult, json, matches_eq, matches_text, page, respond, str_field,
};
use crate::server::BackstageMcp;

/// Per-application detail that is dropped from list rows.
const DETAIL_FIELDS: [&str; 4] = [
    "applicationInfos",
    "applicationStatuses",
    "applications",
    "syncedApplications",
];

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListAppSetsArgs {
    /// Substring match on name, namespace, repo name or chart
    #[serde(default)]
    pub text: Option<String>,
    /// Exact Kubernetes namespace of the ApplicationSet
    #[serde(default)]
    pub namespace: Option<String>,
    /// true to keep only ApplicationSets whose target revision is not the repository HEAD
    #[serde(default)]
    pub not_head: Option<bool>,
    /// Filter by mute state
    #[serde(default)]
    pub muted: Option<bool>,
    /// true to keep only ApplicationSets with at least one unsynced application
    #[serde(default)]
    pub out_of_sync: Option<bool>,
    #[serde(flatten)]
    pub paging: Paging,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppSetArgs {
    /// Kubernetes namespace of the ApplicationSet
    pub namespace: String,
    /// ApplicationSet name
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpstreamChartsArgs {
    /// Substring match on chart name or repository
    #[serde(default)]
    pub text: Option<String>,
    #[serde(flatten)]
    pub paging: Paging,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpstreamChartArgs {
    /// Chart repository URL, for example https://charts.example.com or oci://ghcr.io/org/charts
    pub repository: String,
    /// Chart name
    pub chart: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AuditLogArgs {
    /// Restrict to one ApplicationSet namespace
    #[serde(default)]
    pub namespace: Option<String>,
    /// Restrict to one ApplicationSet name
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BranchesArgs {
    /// Repository URL as reported in repoUrl by argocd_list_application_sets
    pub repo_url: String,
}

fn bool_field(row: &Value, key: &str) -> bool {
    row.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn u64_field(row: &Value, key: &str) -> u64 {
    row.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn charts(row: &Value) -> Vec<Option<&str>> {
    row.get("charts")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(Value::as_str).collect())
        .unwrap_or_default()
}

impl BackstageMcp {
    async fn application_sets(&self) -> Result<Vec<Value>, ToolError> {
        Ok(self
            .client
            .get_json("/api/argocd-appset/application-sets", &[])
            .await?)
    }

    async fn list_application_sets(&self, args: ListAppSetsArgs) -> ToolResult {
        let filtered: Vec<Value> = self
            .application_sets()
            .await?
            .into_iter()
            .filter(|row| {
                let out_of_sync =
                    u64_field(row, "syncedCount") < u64_field(row, "applicationCount");
                matches_eq(
                    args.namespace.as_deref(),
                    str_field(row, "namespace").unwrap_or(""),
                ) && matches_eq(args.not_head.as_ref(), &!bool_field(row, "isHeadRevision"))
                    && matches_eq(args.muted.as_ref(), &bool_field(row, "muted"))
                    && matches_eq(args.out_of_sync.as_ref(), &out_of_sync)
                    && {
                        let mut fields = vec![
                            str_field(row, "name"),
                            str_field(row, "namespace"),
                            str_field(row, "repoName"),
                        ];
                        fields.extend(charts(row));
                        matches_text(args.text.as_deref(), &fields)
                    }
            })
            .map(|mut row| {
                if let Some(object) = row.as_object_mut() {
                    for field in DETAIL_FIELDS {
                        object.remove(field);
                    }
                }
                row
            })
            .collect();
        json(page(filtered, args.paging, 50))
    }

    async fn get_application_set(&self, args: AppSetArgs) -> ToolResult {
        self.application_sets()
            .await?
            .into_iter()
            .find(|row| {
                str_field(row, "namespace") == Some(args.namespace.as_str())
                    && str_field(row, "name") == Some(args.name.as_str())
            })
            .ok_or_else(|| {
                ToolError::NotFound(format!(
                    "ApplicationSet {}/{} not found in the last collector run",
                    args.namespace, args.name
                ))
            })
    }

    async fn list_upstream_charts(&self, args: UpstreamChartsArgs) -> ToolResult {
        let rows: Vec<Value> = self
            .client
            .get_json("/api/argocd-appset/upstream-charts", &[])
            .await?;
        let filtered: Vec<Value> = rows
            .into_iter()
            .filter(|row| {
                matches_text(
                    args.text.as_deref(),
                    &[str_field(row, "chart"), str_field(row, "repository")],
                )
            })
            .collect();
        json(page(filtered, args.paging, 100))
    }

    async fn get_upstream_chart(&self, args: UpstreamChartArgs) -> ToolResult {
        let row: Value = self
            .client
            .get_json(
                "/api/argocd-appset/upstream-chart",
                &[("repository", args.repository), ("chart", args.chart)],
            )
            .await?;
        Ok(row)
    }

    async fn upstream_scan(&self) -> ToolResult {
        Ok(self
            .client
            .get_json("/api/argocd-appset/upstream-scan", &[])
            .await?)
    }

    async fn argocd_status(&self) -> ToolResult {
        Ok(self
            .client
            .get_json("/api/argocd-appset/status", &[])
            .await?)
    }

    async fn audit_logs(&self, args: AuditLogArgs) -> ToolResult {
        let mut query = Vec::new();
        if let Some(namespace) = args.namespace.filter(|v| !v.is_empty()) {
            query.push(("namespace", namespace));
        }
        if let Some(name) = args.name.filter(|v| !v.is_empty()) {
            query.push(("name", name));
        }
        Ok(self
            .client
            .get_json("/api/argocd-appset/audit-logs", &query)
            .await?)
    }

    async fn repo_branches(&self, args: BranchesArgs) -> ToolResult {
        Ok(self
            .client
            .get_json("/api/argocd-appset/branches", &[("repoUrl", args.repo_url)])
            .await?)
    }
}

#[tool_router(router = argocd_router, vis = "pub(crate)")]
impl BackstageMcp {
    #[tool(
        name = "argocd_get_status",
        description = "Collector status of the ArgoCD ApplicationSet plugin: refresh cron, last fetch time, Slack configuration and whether Application resources are readable.",
        annotations(
            title = "ArgoCD plugin status",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn argocd_get_status(&self) -> CallToolResult {
        respond(
            "argocd_get_status",
            self.argocd_status().await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "argocd_list_application_sets",
        description = "ApplicationSets from the last collector run with application counts, sync counts, charts, target revisions, whether the revision is HEAD, and mute state. Per-application detail is left out here; use argocd_get_application_set for it.",
        annotations(
            title = "List ArgoCD ApplicationSets",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn argocd_list_application_sets(
        &self,
        Parameters(args): Parameters<ListAppSetsArgs>,
    ) -> CallToolResult {
        respond(
            "argocd_list_application_sets",
            self.list_application_sets(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "argocd_get_application_set",
        description = "Full record of one ApplicationSet including every generated application, its sync status and chart information.",
        annotations(
            title = "Get one ArgoCD ApplicationSet",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn argocd_get_application_set(
        &self,
        Parameters(args): Parameters<AppSetArgs>,
    ) -> CallToolResult {
        respond(
            "argocd_get_application_set",
            self.get_application_set(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "argocd_list_upstream_charts",
        description = "Latest upstream version known for every Helm chart the ApplicationSets deploy, from the periodic upstream scan. Compare with chartVersions on an ApplicationSet to find upgradable deployments.",
        annotations(
            title = "Upstream Helm chart versions",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn argocd_list_upstream_charts(
        &self,
        Parameters(args): Parameters<UpstreamChartsArgs>,
    ) -> CallToolResult {
        respond(
            "argocd_list_upstream_charts",
            self.list_upstream_charts(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "argocd_get_upstream_chart",
        description = "Latest version and app version of one Helm chart in a repository (Helm index or OCI tags), as cached by the upstream scan.",
        annotations(
            title = "Look up one upstream chart",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn argocd_get_upstream_chart(
        &self,
        Parameters(args): Parameters<UpstreamChartArgs>,
    ) -> CallToolResult {
        respond(
            "argocd_get_upstream_chart",
            self.get_upstream_chart(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "argocd_get_upstream_scan_status",
        description = "Whether an upstream chart scan is running, its progress, failures and when the last one completed.",
        annotations(
            title = "Upstream scan progress",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn argocd_get_upstream_scan_status(&self) -> CallToolResult {
        respond(
            "argocd_get_upstream_scan_status",
            self.upstream_scan().await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "argocd_list_audit_logs",
        description = "Who muted, unmuted or changed the target revision of ApplicationSets and when. Newest first, at most 50 entries.",
        annotations(
            title = "ApplicationSet audit log",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn argocd_list_audit_logs(
        &self,
        Parameters(args): Parameters<AuditLogArgs>,
    ) -> CallToolResult {
        respond(
            "argocd_list_audit_logs",
            self.audit_logs(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "argocd_list_repo_branches",
        description = "Branches of the GitLab repository behind an ApplicationSet, with the latest commit of each. Uses the repoUrl field from argocd_list_application_sets.",
        annotations(
            title = "List branches of a GitOps repository",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn argocd_list_repo_branches(
        &self,
        Parameters(args): Parameters<BranchesArgs>,
    ) -> CallToolResult {
        respond(
            "argocd_list_repo_branches",
            self.repo_branches(args).await,
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

    fn appset(name: &str, synced: u64, head: bool, muted: bool) -> Value {
        json!({
            "name": name, "namespace": "argocd", "repoName": "gitops", "charts": ["nginx"],
            "applicationCount": 2, "syncedCount": synced, "isHeadRevision": head, "muted": muted,
            "applications": ["a", "b"], "applicationInfos": {"a": {}}, "applicationStatuses": {"a": "Synced"}, "syncedApplications": ["a"]
        })
    }

    #[tokio::test]
    async fn application_set_tools() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/argocd-appset/application-sets"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                appset("web", 2, true, false),
                appset("api", 1, false, true),
            ])))
            .mount(&server)
            .await;

        let list = result_json(
            &handler
                .argocd_list_application_sets(Parameters(ListAppSetsArgs {
                    text: Some("NGINX".into()),
                    namespace: Some("argocd".into()),
                    not_head: Some(true),
                    muted: Some(true),
                    out_of_sync: Some(true),
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(list["total"], 1);
        assert_eq!(list["items"][0]["name"], "api");
        assert!(list["items"][0].get("applicationInfos").is_none());

        let one = result_json(
            &handler
                .argocd_get_application_set(Parameters(AppSetArgs {
                    namespace: "argocd".into(),
                    name: "web".into(),
                }))
                .await,
        );
        assert_eq!(one["applicationInfos"]["a"], json!({}));

        let missing = handler
            .argocd_get_application_set(Parameters(AppSetArgs {
                namespace: "argocd".into(),
                name: "nope".into(),
            }))
            .await;
        assert_eq!(missing.is_error, Some(true));
        assert!(text(&missing).contains("not found"));
    }

    #[tokio::test]
    async fn upstream_and_passthrough_tools() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/argocd-appset/upstream-charts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"chart": "nginx", "repository": "https://charts.bitnami.com"},
                {"chart": "redis", "repository": "oci://ghcr.io/x"}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/argocd-appset/upstream-chart"))
            .and(query_param("chart", "nginx"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"latestVersion": "1.0.0"})),
            )
            .mount(&server)
            .await;
        for (route, body) in [
            (
                "/api/argocd-appset/upstream-scan",
                json!({"running": false}),
            ),
            ("/api/argocd-appset/status", json!({"cron": "* * * * *"})),
            ("/api/argocd-appset/branches", json!({"branches": []})),
        ] {
            Mock::given(method("GET"))
                .and(path(route))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/api/argocd-appset/audit-logs"))
            .and(query_param("name", "web"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"action": "mute"}])))
            .mount(&server)
            .await;

        let charts = result_json(
            &handler
                .argocd_list_upstream_charts(Parameters(UpstreamChartsArgs {
                    text: Some("ghcr".into()),
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(charts["items"][0]["chart"], "redis");
        let chart = result_json(
            &handler
                .argocd_get_upstream_chart(Parameters(UpstreamChartArgs {
                    repository: "https://charts.bitnami.com".into(),
                    chart: "nginx".into(),
                }))
                .await,
        );
        assert_eq!(chart["latestVersion"], "1.0.0");
        assert_eq!(
            result_json(&handler.argocd_get_upstream_scan_status().await)["running"],
            false
        );
        assert_eq!(
            result_json(&handler.argocd_get_status().await)["cron"],
            "* * * * *"
        );
        let logs = result_json(
            &handler
                .argocd_list_audit_logs(Parameters(AuditLogArgs {
                    namespace: Some(String::new()),
                    name: Some("web".into()),
                }))
                .await,
        );
        assert_eq!(logs[0]["action"], "mute");
        let branches = result_json(
            &handler
                .argocd_list_repo_branches(Parameters(BranchesArgs {
                    repo_url: "https://git/x".into(),
                }))
                .await,
        );
        assert!(branches["branches"].as_array().unwrap().is_empty());
    }
}
