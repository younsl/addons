//! Platforms page usage statistics.

use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use serde_json::{Value, json};

use super::{ToolResult, respond};
use crate::server::BackstageMcp;

impl BackstageMcp {
    async fn platform_stats(&self) -> ToolResult {
        let response: Value = self.client.get_json("/api/platforms/stats", &[]).await?;
        let mut platforms: Vec<Value> = response
            .get("stats")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let rank = |row: &Value| row.get("rank").and_then(Value::as_u64).unwrap_or(u64::MAX);
        platforms.sort_by_key(rank);
        Ok(json!({
            "generatedAt": response.get("generatedAt"),
            "rankedCount": response.get("rankedCount"),
            "platforms": platforms,
        }))
    }
}

#[tool_router(router = platforms_router, vis = "pub(crate)")]
impl BackstageMcp {
    #[tool(
        name = "platforms_get_stats",
        description = "Usage statistics for the internal platform links on the Platforms page: daily and weekly unique visitors per platform, week-over-week trend and popularity rank, best ranked first.",
        annotations(
            title = "Platform link usage",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn platforms_get_stats(&self) -> CallToolResult {
        respond(
            "platforms_get_stats",
            self.platform_stats().await,
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
    async fn stats_sorted_by_rank() {
        let (server, handler) = mcp().await;
        Mock::given(method("GET"))
            .and(path("/api/platforms/stats"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "stats": [
                    {"platform": "grafana", "rank": null},
                    {"platform": "argocd", "rank": 2},
                    {"platform": "gitlab", "rank": 1}
                ],
                "rankedCount": 2,
                "generatedAt": "2026-08-31T00:00:00Z"
            })))
            .mount(&server)
            .await;
        let value = result_json(&handler.platforms_get_stats().await);
        assert_eq!(value["platforms"][0]["platform"], "gitlab");
        assert_eq!(value["platforms"][2]["platform"], "grafana");
        assert_eq!(value["rankedCount"], 2);
    }
}
