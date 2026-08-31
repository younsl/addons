//! S3 Log Extract: log inventory, extraction estimates and request history.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::catalog::encode_segment;
use super::{
    Paging, ToolError, ToolResult, join_csv, json, matches_eq, page, respond, sort_newest_first,
    str_field,
};
use crate::server::BackstageMcp;

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LogSource {
    K8s,
    Ec2,
}

impl LogSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::K8s => "k8s",
            Self::Ec2 => "ec2",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListAppsArgs {
    /// Environment name, for example prd or stg
    pub env: String,
    /// Log date as YYYY-MM-DD
    pub date: String,
    /// Log source, default k8s
    #[serde(default)]
    pub source: Option<LogSource>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PrecheckArgs {
    /// Environment name, for example prd or stg
    pub env: String,
    /// Log date as YYYY-MM-DD
    pub date: String,
    /// Log source, default k8s
    #[serde(default)]
    pub source: Option<LogSource>,
    /// Window start as HH:mm
    pub start_time: String,
    /// Window end as HH:mm
    pub end_time: String,
    /// Application names from s3_log_extract_list_apps
    pub apps: Vec<String>,
    /// EC2 log stream category, only for source ec2
    #[serde(default)]
    pub log_type: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListRequestsArgs {
    /// Exact status, for example pending, approved, rejected, extracting, completed, failed, expired
    #[serde(default)]
    pub status: Option<String>,
    /// Exact environment name
    #[serde(default)]
    pub env: Option<String>,
    /// Exact requester entity ref
    #[serde(default)]
    pub requester: Option<String>,
    #[serde(flatten)]
    pub paging: Paging,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RequestIdArgs {
    /// Request id
    pub id: String,
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

impl BackstageMcp {
    async fn extract_config(&self) -> ToolResult {
        let config: Value = self
            .client
            .get_json("/api/s3-log-extract/config", &[])
            .await?;
        let health: Value = self
            .client
            .get_json("/api/s3-log-extract/s3-health", &[])
            .await?;
        let mut merged = config.as_object().cloned().unwrap_or_default();
        merged.insert("s3Health".to_string(), health);
        Ok(Value::Object(merged))
    }

    async fn list_apps(&self, args: ListAppsArgs) -> ToolResult {
        validate_date(&args.date)?;
        Ok(self
            .client
            .get_json(
                "/api/s3-log-extract/apps",
                &[
                    ("env", args.env),
                    ("date", args.date),
                    (
                        "source",
                        args.source.unwrap_or(LogSource::K8s).as_str().to_string(),
                    ),
                ],
            )
            .await?)
    }

    async fn precheck(&self, args: PrecheckArgs) -> ToolResult {
        validate_date(&args.date)?;
        let Some(apps) = join_csv(Some(&args.apps)) else {
            return Err(ToolError::Input(
                "apps must contain at least one application".to_string(),
            ));
        };
        let mut query = vec![
            ("env", args.env),
            ("date", args.date),
            (
                "source",
                args.source.unwrap_or(LogSource::K8s).as_str().to_string(),
            ),
            ("startTime", args.start_time),
            ("endTime", args.end_time),
            ("apps", apps),
        ];
        if let Some(log_type) = args.log_type.filter(|t| !t.trim().is_empty()) {
            query.push(("logType", log_type));
        }
        Ok(self
            .client
            .get_json("/api/s3-log-extract/precheck", &query)
            .await?)
    }

    async fn list_extract_requests(&self, args: ListRequestsArgs) -> ToolResult {
        let rows: Vec<Value> = self
            .client
            .get_json("/api/s3-log-extract/requests", &[])
            .await?;
        let mut filtered: Vec<Value> = rows
            .into_iter()
            .filter(|row| {
                matches_eq(
                    args.status.as_deref(),
                    str_field(row, "status").unwrap_or(""),
                ) && matches_eq(args.env.as_deref(), str_field(row, "env").unwrap_or(""))
                    && matches_eq(
                        args.requester.as_deref(),
                        str_field(row, "requesterRef").unwrap_or(""),
                    )
            })
            .collect();
        sort_newest_first(&mut filtered, "createdAt");
        json(page(filtered, args.paging, 50))
    }

    async fn get_extract_request(&self, args: RequestIdArgs) -> ToolResult {
        Ok(self
            .client
            .get_json(
                &format!("/api/s3-log-extract/requests/{}", encode_segment(&args.id)),
                &[],
            )
            .await?)
    }
}

#[tool_router(router = s3_log_extract_router, vis = "pub(crate)")]
impl BackstageMcp {
    #[tool(
        name = "s3_log_extract_get_config",
        description = "Bucket, region, prefix and the maximum time range a single extraction may cover, plus the current S3 connectivity check.",
        annotations(
            title = "S3 Log Extract config",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn s3_log_extract_get_config(&self) -> CallToolResult {
        respond(
            "s3_log_extract_get_config",
            self.extract_config().await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "s3_log_extract_list_apps",
        description = "Application names that have log objects in S3 for an environment and date. EC2 entries are app/category pairs.",
        annotations(
            title = "Applications with logs on a date",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn s3_log_extract_list_apps(
        &self,
        Parameters(args): Parameters<ListAppsArgs>,
    ) -> CallToolResult {
        respond(
            "s3_log_extract_list_apps",
            self.list_apps(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "s3_log_extract_precheck",
        description = "Count the S3 log objects an extraction request would cover for the given apps and time window, without extracting anything. Rate limited server-side, so call it once per candidate request.",
        annotations(
            title = "Estimate log objects for a request",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn s3_log_extract_precheck(
        &self,
        Parameters(args): Parameters<PrecheckArgs>,
    ) -> CallToolResult {
        respond(
            "s3_log_extract_precheck",
            self.precheck(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "s3_log_extract_list_requests",
        description = "Log extraction requests with approval and processing status, requester, time window, apps, archive size and progress. Newest first.",
        annotations(
            title = "List log extraction requests",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn s3_log_extract_list_requests(
        &self,
        Parameters(args): Parameters<ListRequestsArgs>,
    ) -> CallToolResult {
        respond(
            "s3_log_extract_list_requests",
            self.list_extract_requests(args).await,
            self.max_result_chars,
        )
    }

    #[tool(
        name = "s3_log_extract_get_request",
        description = "Full record of one log extraction request by id. The archive itself and its password are never returned.",
        annotations(
            title = "Get one log extraction request",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn s3_log_extract_get_request(
        &self,
        Parameters(args): Parameters<RequestIdArgs>,
    ) -> CallToolResult {
        respond(
            "s3_log_extract_get_request",
            self.get_extract_request(args).await,
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
    #[allow(clippy::too_many_lines)]
    async fn log_extract_tools() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/s3-log-extract/config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"bucket": "logs"})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/s3-log-extract/s3-health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"connected": true})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/s3-log-extract/apps"))
            .and(query_param("source", "ec2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!(["app/java"])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/s3-log-extract/precheck"))
            .and(query_param("apps", "a,b"))
            .and(query_param("logType", "java"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"candidateCount": 4})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/s3-log-extract/requests"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": "1", "env": "prd", "status": "completed", "requesterRef": "user:default/a", "createdAt": "2026-01-01"},
                {"id": "2", "env": "stg", "status": "pending", "requesterRef": "user:default/b", "createdAt": "2026-02-01"}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/s3-log-extract/requests/2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "2"})))
            .mount(&server)
            .await;

        let config = result_json(&handler.s3_log_extract_get_config().await);
        assert_eq!(config["bucket"], "logs");
        assert_eq!(config["s3Health"]["connected"], true);

        let apps = result_json(
            &handler
                .s3_log_extract_list_apps(Parameters(ListAppsArgs {
                    env: "prd".into(),
                    date: "2026-08-31".into(),
                    source: Some(LogSource::Ec2),
                }))
                .await,
        );
        assert_eq!(apps[0], "app/java");
        let bad_date = handler
            .s3_log_extract_list_apps(Parameters(ListAppsArgs {
                env: "prd".into(),
                date: "31/08".into(),
                source: None,
            }))
            .await;
        assert!(text(&bad_date).contains("YYYY-MM-DD"));

        let estimate = result_json(
            &handler
                .s3_log_extract_precheck(Parameters(PrecheckArgs {
                    env: "prd".into(),
                    date: "2026-08-31".into(),
                    source: None,
                    start_time: "10:00".into(),
                    end_time: "10:30".into(),
                    apps: vec!["a".into(), "b".into()],
                    log_type: Some("java".into()),
                }))
                .await,
        );
        assert_eq!(estimate["candidateCount"], 4);
        let no_apps = handler
            .s3_log_extract_precheck(Parameters(PrecheckArgs {
                env: "prd".into(),
                date: "2026-08-31".into(),
                source: None,
                start_time: "10:00".into(),
                end_time: "10:30".into(),
                apps: vec![],
                log_type: None,
            }))
            .await;
        assert!(text(&no_apps).contains("apps"));

        let requests = result_json(
            &handler
                .s3_log_extract_list_requests(Parameters(ListRequestsArgs {
                    status: None,
                    env: None,
                    requester: None,
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(requests["items"][0]["id"], "2");
        let filtered = result_json(
            &handler
                .s3_log_extract_list_requests(Parameters(ListRequestsArgs {
                    status: Some("completed".into()),
                    env: Some("prd".into()),
                    requester: Some("user:default/a".into()),
                    paging: Paging::default(),
                }))
                .await,
        );
        assert_eq!(filtered["total"], 1);
        assert_eq!(
            result_json(
                &handler
                    .s3_log_extract_get_request(Parameters(RequestIdArgs { id: "2".into() }))
                    .await
            )["id"],
            "2"
        );
    }
}
