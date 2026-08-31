//! Cost Report: OpenCost data collected per cluster, month and pod.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{
    Paging, QueryPairs, ToolError, ToolResult, join_csv, matches_eq, page, respond, sort_desc_by,
    str_field,
};
use crate::server::BackstageMcp;

/// Cluster, year and month, the key most cost endpoints take.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MonthKey {
    /// Cluster name as configured in opencost.clusters, for example prd or dev
    pub cluster: String,
    /// Four-digit year
    pub year: u32,
    /// Month 1-12
    pub month: u32,
}

impl MonthKey {
    fn query(&self) -> Result<QueryPairs, ToolError> {
        if !(1..=12).contains(&self.month) {
            return Err(ToolError::Input(format!(
                "month must be 1-12, got {}",
                self.month
            )));
        }
        Ok(vec![
            ("cluster", self.cluster.clone()),
            ("year", self.year.to_string()),
            ("month", self.month.to_string()),
        ])
    }
}

/// Controller and preset scoping shared by the cost tools.
#[derive(Debug, Default, Clone, Deserialize, JsonSchema)]
pub struct Scope {
    /// Restrict to these controller names (Deployment, StatefulSet, Job, ... names)
    #[serde(default)]
    pub controllers: Option<Vec<String>>,
    /// Name of a controller filter preset from opencost_list_filters
    #[serde(default)]
    pub filter: Option<String>,
}

impl Scope {
    fn query(&self) -> QueryPairs {
        let mut pairs = Vec::new();
        if let Some(controllers) = join_csv(self.controllers.as_deref()) {
            pairs.push(("controllers", controllers));
        }
        if let Some(filter) = self
            .filter
            .as_deref()
            .map(str::trim)
            .filter(|f| !f.is_empty())
        {
            pairs.push(("filter", filter.to_string()));
        }
        pairs
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClusterArgs {
    /// Cluster name as configured in opencost.clusters, for example prd or dev
    pub cluster: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchControllersArgs {
    /// Cluster name as configured in opencost.clusters, for example prd or dev
    pub cluster: String,
    /// Four-digit year
    pub year: u32,
    /// Month 1-12, default the whole year
    #[serde(default)]
    pub month: Option<u32>,
    /// Substring of the controller name
    #[serde(default)]
    pub q: Option<String>,
    /// Only these controller kinds, for example Deployment, StatefulSet, DaemonSet, Job, CronJob
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
    /// Kinds to drop, default Job
    #[serde(default)]
    pub exclude_kinds: Option<Vec<String>>,
    /// Name of a controller filter preset from opencost_list_filters
    #[serde(default)]
    pub filter: Option<String>,
    /// Maximum rows, default 50, maximum 500
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MonthScopeArgs {
    #[serde(flatten)]
    pub key: MonthKey,
    #[serde(flatten)]
    pub scope: Scope,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MonthlyPodCostsArgs {
    #[serde(flatten)]
    pub key: MonthKey,
    #[serde(flatten)]
    pub scope: Scope,
    /// Keep only pods in this namespace
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(flatten)]
    pub paging: Paging,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DailyPodCostsArgs {
    /// Cluster name as configured in opencost.clusters, for example prd or dev
    pub cluster: String,
    /// Date as YYYY-MM-DD
    pub date: String,
    #[serde(flatten)]
    pub scope: Scope,
    /// Keep only pods in this namespace
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(flatten)]
    pub paging: Paging,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PodHistoryArgs {
    #[serde(flatten)]
    pub key: MonthKey,
    /// Pod name
    pub pod: String,
}

fn data(response: Value) -> Value {
    match response {
        Value::Object(mut object) => object.remove("data").unwrap_or(Value::Null),
        other => other,
    }
}

fn validate_date(date: &str) -> Result<(), ToolError> {
    let ok = date.len() == 10
        && date.char_indices().all(|(i, c)| {
            if i == 4 || i == 7 {
                c == '-'
            } else {
                c.is_ascii_digit()
            }
        });
    if ok {
        Ok(())
    } else {
        Err(ToolError::Input(format!(
            "date must be YYYY-MM-DD, got {date:?}"
        )))
    }
}

fn pod_rows(response: Value, namespace: Option<&str>) -> Vec<Value> {
    let mut rows: Vec<Value> = data(response)
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|row| matches_eq(namespace, str_field(row, "namespace").unwrap_or("")))
        .collect();
    sort_desc_by(&mut rows, "totalCost");
    rows
}

impl BackstageMcp {
    async fn config(&self) -> ToolResult {
        let config: Value = self.client.get_json("/api/opencost/config", &[]).await?;
        let clusters: Value = self
            .client
            .get_json("/api/opencost/clusters/status", &[])
            .await?;
        let mut merged = config.as_object().cloned().unwrap_or_default();
        merged.insert("clusters".to_string(), data(clusters));
        Ok(Value::Object(merged))
    }

    async fn filters(&self) -> ToolResult {
        Ok(data(
            self.client.get_json("/api/opencost/filters", &[]).await?,
        ))
    }

    async fn years(&self, args: ClusterArgs) -> ToolResult {
        Ok(data(
            self.client
                .get_json("/api/opencost/costs/years", &[("cluster", args.cluster)])
                .await?,
        ))
    }

    async fn search_controllers(&self, args: SearchControllersArgs) -> ToolResult {
        let mut query = vec![("cluster", args.cluster), ("year", args.year.to_string())];
        if let Some(month) = args.month {
            if !(1..=12).contains(&month) {
                return Err(ToolError::Input(format!("month must be 1-12, got {month}")));
            }
            query.push(("month", month.to_string()));
        }
        if let Some(q) = args.q.filter(|q| !q.trim().is_empty()) {
            query.push(("q", q));
        }
        if let Some(kinds) = join_csv(args.kinds.as_deref()) {
            query.push(("kinds", kinds));
        }
        if let Some(exclude) = join_csv(args.exclude_kinds.as_deref()) {
            query.push(("excludeKinds", exclude));
        }
        if let Some(filter) = args.filter.filter(|f| !f.trim().is_empty()) {
            query.push(("filter", filter));
        }
        query.push(("limit", args.limit.unwrap_or(50).clamp(1, 500).to_string()));
        Ok(self
            .client
            .get_json("/api/opencost/costs/controllers", &query)
            .await?)
    }

    async fn monthly_totals(&self, args: MonthScopeArgs) -> ToolResult {
        let mut query = args.key.query()?;
        query.extend(args.scope.query());
        Ok(data(
            self.client
                .get_json("/api/opencost/costs/monthly-totals", &query)
                .await?,
        ))
    }

    async fn daily_summary(&self, args: MonthScopeArgs) -> ToolResult {
        let mut query = args.key.query()?;
        query.extend(args.scope.query());
        Ok(data(
            self.client
                .get_json("/api/opencost/costs/daily-summary", &query)
                .await?,
        ))
    }

    async fn monthly_pod_costs(&self, args: MonthlyPodCostsArgs) -> ToolResult {
        let mut query = args.key.query()?;
        query.extend(args.scope.query());
        let response: Value = self.client.get_json("/api/opencost/costs", &query).await?;
        let days_covered = response.get("daysCovered").cloned();
        let source = response.get("source").cloned();
        let rows = pod_rows(response, args.namespace.as_deref());
        let paged = page(rows, args.paging, 50);
        let mut value = serde_json::to_value(paged)?;
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "daysCovered".to_string(),
                days_covered.unwrap_or(Value::Null),
            );
            object.insert("source".to_string(), source.unwrap_or(Value::Null));
        }
        Ok(value)
    }

    async fn daily_pod_costs(&self, args: DailyPodCostsArgs) -> ToolResult {
        validate_date(&args.date)?;
        let mut query = vec![("cluster", args.cluster), ("date", args.date)];
        query.extend(args.scope.query());
        let response: Value = self
            .client
            .get_json("/api/opencost/costs/pods", &query)
            .await?;
        let rows = pod_rows(response, args.namespace.as_deref());
        Ok(serde_json::to_value(page(rows, args.paging, 50))?)
    }

    async fn pod_history(&self, args: PodHistoryArgs) -> ToolResult {
        let mut query = args.key.query()?;
        query.push(("pod", args.pod));
        Ok(data(
            self.client
                .get_json("/api/opencost/costs/daily", &query)
                .await?,
        ))
    }

    async fn collection_runs(&self, args: MonthKey) -> ToolResult {
        let query = args.query()?;
        Ok(data(
            self.client
                .get_json("/api/opencost/costs/collection-runs", &query)
                .await?,
        ))
    }
}

#[tool_router(router = opencost_router, vis = "pub(crate)")]
impl BackstageMcp {
    #[tool(
        name = "opencost_get_config",
        description = "Timezone, collector cron and the list of clusters the Cost Report page covers with their live OpenCost reachability.",
        annotations(
            title = "Cost Report configuration",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opencost_get_config(&self) -> CallToolResult {
        respond(
            "opencost_get_config",
            self.config().await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "opencost_list_filters",
        description = "Named controller filter presets (lists of SQL LIKE patterns on controller names, optionally scoped to clusters) used for recurring billing views. Pass a preset name as filter to the cost tools.",
        annotations(
            title = "List controller filter presets",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opencost_list_filters(&self) -> CallToolResult {
        respond(
            "opencost_list_filters",
            self.filters().await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "opencost_list_years",
        description = "Years for which a cluster has collected cost rows.",
        annotations(
            title = "Years with cost data",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opencost_list_years(
        &self,
        Parameters(args): Parameters<ClusterArgs>,
    ) -> CallToolResult {
        respond(
            "opencost_list_years",
            self.years(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "opencost_search_controllers",
        description = "Find workload controllers in a cluster for a year (or one month) by name substring, ordered by total cost. Jobs are excluded unless kinds includes Job. Returns at most limit rows and flags truncation.",
        annotations(
            title = "Search controllers by cost",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opencost_search_controllers(
        &self,
        Parameters(args): Parameters<SearchControllersArgs>,
    ) -> CallToolResult {
        respond(
            "opencost_search_controllers",
            self.search_controllers(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "opencost_get_monthly_totals",
        description = "One aggregated row for a cluster and month: CPU, RAM, GPU, PV, network, carbon and total cost, pod count and days covered. Optionally restricted to controllers or a preset. Call once per month to build a yearly view.",
        annotations(
            title = "Monthly cost totals",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opencost_get_monthly_totals(
        &self,
        Parameters(args): Parameters<MonthScopeArgs>,
    ) -> CallToolResult {
        respond(
            "opencost_get_monthly_totals",
            self.monthly_totals(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "opencost_get_daily_summary",
        description = "Per-day totals (cost breakdown and pod count) for a cluster and month, optionally restricted to controllers or a preset.",
        annotations(
            title = "Daily cost summary for a month",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opencost_get_daily_summary(
        &self,
        Parameters(args): Parameters<MonthScopeArgs>,
    ) -> CallToolResult {
        respond(
            "opencost_get_daily_summary",
            self.daily_summary(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "opencost_list_monthly_pod_costs",
        description = "Monthly cost per pod (namespace, controller kind, controller, pod, cost breakdown, days covered) for a cluster, sorted by total cost descending. Large on busy clusters: restrict with controllers, filter or namespace and page with offset and limit.",
        annotations(
            title = "Per-pod costs for a month",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opencost_list_monthly_pod_costs(
        &self,
        Parameters(args): Parameters<MonthlyPodCostsArgs>,
    ) -> CallToolResult {
        respond(
            "opencost_list_monthly_pod_costs",
            self.monthly_pod_costs(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "opencost_list_daily_pod_costs",
        description = "Cost per pod on a single date for a cluster, sorted by total cost descending. Restrict with controllers, filter or namespace and page with offset and limit.",
        annotations(
            title = "Per-pod costs for one day",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opencost_list_daily_pod_costs(
        &self,
        Parameters(args): Parameters<DailyPodCostsArgs>,
    ) -> CallToolResult {
        respond(
            "opencost_list_daily_pod_costs",
            self.daily_pod_costs(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "opencost_get_pod_daily_costs",
        description = "Day-by-day cost rows of a single pod within a month.",
        annotations(
            title = "Daily cost history of one pod",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opencost_get_pod_daily_costs(
        &self,
        Parameters(args): Parameters<PodHistoryArgs>,
    ) -> CallToolResult {
        respond(
            "opencost_get_pod_daily_costs",
            self.pod_history(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "opencost_list_collection_runs",
        description = "Runs of the daily cost collector for a cluster and month, to check whether data for a day is missing or partial.",
        annotations(
            title = "Collector run history",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn opencost_list_collection_runs(
        &self,
        Parameters(args): Parameters<MonthKey>,
    ) -> CallToolResult {
        respond(
            "opencost_list_collection_runs",
            self.collection_runs(args).await,
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

    fn key() -> MonthKey {
        MonthKey {
            cluster: "prd".into(),
            year: 2026,
            month: 8,
        }
    }

    #[test]
    fn validation() {
        assert!(validate_date("2026-08-31").is_ok());
        assert!(validate_date("2026-8-31").is_err());
        assert!(validate_date("20260831").is_err());
        let bad = MonthKey {
            cluster: "prd".into(),
            year: 2026,
            month: 13,
        };
        assert!(bad.query().is_err());
        let scope = Scope {
            controllers: Some(vec!["a".into(), String::new()]),
            filter: Some(" ".into()),
        };
        assert_eq!(scope.query(), vec![("controllers", "a".to_string())]);
    }

    #[tokio::test]
    async fn config_and_lists() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/opencost/config"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"timezone": "Asia/Seoul"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/opencost/clusters/status"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": [{"name": "prd"}]})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/opencost/filters"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": [{"name": "vendor"}]})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/opencost/costs/years"))
            .and(query_param("cluster", "prd"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [2025, 2026]})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/opencost/costs/controllers"))
            .and(query_param("kinds", "Deployment,Job"))
            .and(query_param("month", "8"))
            .and(query_param("limit", "500"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": [], "truncated": false})),
            )
            .mount(&server)
            .await;

        let config = result_json(&handler.opencost_get_config().await);
        assert_eq!(config["timezone"], "Asia/Seoul");
        assert_eq!(config["clusters"][0]["name"], "prd");
        assert_eq!(
            result_json(&handler.opencost_list_filters().await)[0]["name"],
            "vendor"
        );
        assert_eq!(
            result_json(
                &handler
                    .opencost_list_years(Parameters(ClusterArgs {
                        cluster: "prd".into()
                    }))
                    .await
            ),
            json!([2025, 2026])
        );
        let controllers = result_json(
            &handler
                .opencost_search_controllers(Parameters(SearchControllersArgs {
                    cluster: "prd".into(),
                    year: 2026,
                    month: Some(8),
                    q: Some("api".into()),
                    kinds: Some(vec!["Deployment".into(), "Job".into()]),
                    exclude_kinds: None,
                    filter: Some("vendor".into()),
                    limit: Some(9999),
                }))
                .await,
        );
        assert_eq!(controllers["truncated"], false);
        let bad_month = handler
            .opencost_search_controllers(Parameters(SearchControllersArgs {
                cluster: "prd".into(),
                year: 2026,
                month: Some(0),
                q: None,
                kinds: None,
                exclude_kinds: None,
                filter: None,
                limit: None,
            }))
            .await;
        assert!(text(&bad_month).contains("month"));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn cost_tools() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/opencost/costs/monthly-totals"))
            .and(query_param("controllers", "api,web"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": {"totalCost": 12.5}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/opencost/costs/daily-summary"))
            .and(query_param("filter", "vendor"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": [{"date": "2026-08-01"}]})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/opencost/costs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"namespace": "a", "pod": "cheap", "totalCost": 1.0},
                    {"namespace": "b", "pod": "pricey", "totalCost": 9.0},
                    {"namespace": "a", "pod": "mid", "totalCost": 5.0}
                ],
                "daysCovered": 31, "source": "monthly"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/opencost/costs/pods"))
            .and(query_param("date", "2026-08-31"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
                {"namespace": "a", "pod": "x", "totalCost": 2.0}, {"namespace": "a", "pod": "y", "totalCost": 3.0}
            ]})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/opencost/costs/daily"))
            .and(query_param("pod", "x"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": [{"date": "2026-08-01"}]})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/opencost/costs/collection-runs"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": [{"status": "ok"}]})),
            )
            .mount(&server)
            .await;

        let totals = result_json(
            &handler
                .opencost_get_monthly_totals(Parameters(MonthScopeArgs {
                    key: key(),
                    scope: Scope {
                        controllers: Some(vec!["api".into(), "web".into()]),
                        filter: None,
                    },
                }))
                .await,
        );
        assert_eq!(totals["totalCost"], 12.5);

        let daily = result_json(
            &handler
                .opencost_get_daily_summary(Parameters(MonthScopeArgs {
                    key: key(),
                    scope: Scope {
                        controllers: None,
                        filter: Some("vendor".into()),
                    },
                }))
                .await,
        );
        assert_eq!(daily[0]["date"], "2026-08-01");

        let monthly = result_json(
            &handler
                .opencost_list_monthly_pod_costs(Parameters(MonthlyPodCostsArgs {
                    key: key(),
                    scope: Scope::default(),
                    namespace: Some("a".into()),
                    paging: Paging {
                        offset: None,
                        limit: Some(1),
                    },
                }))
                .await,
        );
        assert_eq!(monthly["total"], 2);
        assert_eq!(monthly["items"][0]["pod"], "mid");
        assert_eq!(monthly["truncated"], true);
        assert_eq!(monthly["daysCovered"], 31);
        assert_eq!(monthly["source"], "monthly");

        let day = result_json(
            &handler
                .opencost_list_daily_pod_costs(Parameters(DailyPodCostsArgs {
                    cluster: "prd".into(),
                    date: "2026-08-31".into(),
                    scope: Scope::default(),
                    namespace: None,
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(day["items"][0]["pod"], "y");
        let bad_date = handler
            .opencost_list_daily_pod_costs(Parameters(DailyPodCostsArgs {
                cluster: "prd".into(),
                date: "yesterday".into(),
                scope: Scope::default(),
                namespace: None,
                paging: Paging::default(),
            }))
            .await;
        assert!(text(&bad_date).contains("YYYY-MM-DD"));

        let history = result_json(
            &handler
                .opencost_get_pod_daily_costs(Parameters(PodHistoryArgs {
                    key: key(),
                    pod: "x".into(),
                }))
                .await,
        );
        assert_eq!(history[0]["date"], "2026-08-01");
        let runs = result_json(
            &handler
                .opencost_list_collection_runs(Parameters(key()))
                .await,
        );
        assert_eq!(runs[0]["status"], "ok");
    }
}
