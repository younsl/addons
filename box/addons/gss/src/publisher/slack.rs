use crate::models::ScanResult;
use crate::publisher::Publisher;
use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::json;
use std::fmt::Write as _;
use tracing::info;

const KST_OFFSET_HOURS: i32 = 9;

pub struct SlackCanvasPublisher {
    client: Client,
    token: String,
    canvas_id: String,
}

impl SlackCanvasPublisher {
    pub fn new(token: String, _channel_id: String, canvas_id: String) -> Self {
        Self {
            client: Client::new(),
            token,
            canvas_id,
        }
    }

    fn convert_cron_to_kst(cron: &str) -> String {
        let parts: Vec<&str> = cron.split_whitespace().collect();
        if parts.len() != 5 {
            return cron.to_string();
        }

        let minute = parts[0];
        let hour = parts[1];
        let day = parts[2];
        let month = parts[3];
        let dow = parts[4];

        // Parse hour field (simplified version for Slack)
        let kst_hour = if hour == "*" {
            "*".to_string()
        } else if hour.contains('/') {
            hour.to_string()
        } else if let Ok(h) = hour.parse::<i32>() {
            ((h + KST_OFFSET_HOURS) % 24).to_string()
        } else {
            hour.to_string()
        };

        format!("{minute} {kst_hour} {day} {month} {dow}")
    }

    fn format_canvas_content(result: &ScanResult) -> String {
        let mut content = String::new();

        // Header
        content.push_str("# GitHub Scheduled Workflows Report\n\n");

        // Build information
        let _ = writeln!(content, "**Version:** {}", env!("CARGO_PKG_VERSION"));
        let _ = writeln!(
            content,
            "**Build Date:** {}",
            option_env!("BUILD_DATE").unwrap_or("unknown")
        );
        let _ = write!(
            content,
            "**Git Commit:** {}\n\n",
            option_env!("GIT_COMMIT").unwrap_or("unknown")
        );

        // Summary
        content.push_str("## Summary\n\n");
        let _ = writeln!(content, "- **Total Workflows:** {}", result.workflows.len());
        let _ = writeln!(content, "- **Total Repositories:** {}", result.total_repos);
        let _ = writeln!(
            content,
            "- **Excluded Repositories:** {}",
            result.excluded_repos_count
        );
        let _ = write!(
            content,
            "- **Scan Duration:** {:?}\n\n",
            result.scan_duration
        );

        // Workflows table
        if result.workflows.is_empty() {
            content.push_str("No scheduled workflows found.\n");
        } else {
            content.push_str("## Scheduled Workflows\n\n");

            for (idx, workflow) in result.workflows.iter().enumerate() {
                let schedules = workflow.cron_schedules.join(", ");
                let kst_schedules = workflow
                    .cron_schedules
                    .iter()
                    .map(|s| Self::convert_cron_to_kst(s))
                    .collect::<Vec<_>>()
                    .join(", ");

                let status_emoji = match workflow.last_status.as_str() {
                    "success" | "completed" => "✅",
                    "failure" | "failed" => "❌",
                    "cancelled" => "🚫",
                    "never_run" => "⏸️",
                    _ => "❓",
                };

                let user_status = if workflow.is_active_user {
                    "✅ Active"
                } else {
                    "⚠️ Inactive"
                };

                let _ = writeln!(content, "### {}. {}", idx + 1, workflow.workflow_name);
                let _ = writeln!(content, "- **Repository:** `{}`", workflow.repo_name);
                let _ = writeln!(
                    content,
                    "- **Workflow File:** `{}`",
                    workflow.workflow_file_name
                );
                let _ = writeln!(content, "- **UTC Schedule:** `{schedules}`");
                let _ = writeln!(content, "- **KST Schedule:** `{kst_schedules}`");
                let _ = writeln!(
                    content,
                    "- **Last Status:** {} {}",
                    status_emoji, workflow.last_status
                );
                let _ = writeln!(
                    content,
                    "- **Workflow Last Author:** {} ({})",
                    workflow.workflow_last_author, user_status
                );
                content.push('\n');
            }
        }

        content
    }

    async fn update_canvas(&self, content: &str) -> Result<()> {
        let url = "https://slack.com/api/canvases.edit";

        let payload = json!({
            "canvas_id": self.canvas_id,
            "changes": [{
                "operation": "replace",
                "document_content": {
                    "type": "markdown",
                    "markdown": content
                }
            }]
        });

        let response = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .context("Failed to send request to Slack API")?;

        let status = response.status();
        let body: serde_json::Value = response
            .json()
            .await
            .context("Failed to parse Slack API response")?;

        if !status.is_success() {
            anyhow::bail!("Slack API request failed with status {status}: {body}");
        }

        if let Some(ok) = body.get("ok").and_then(serde_json::Value::as_bool)
            && !ok
        {
            let error = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            anyhow::bail!("Slack API returned error: {error}");
        }

        Ok(())
    }
}

#[async_trait]
impl Publisher for SlackCanvasPublisher {
    async fn publish(&self, result: &ScanResult) -> Result<()> {
        info!("Publishing results to Slack Canvas");

        let content = Self::format_canvas_content(result);

        self.update_canvas(&content)
            .await
            .context("Failed to update Slack Canvas")?;

        info!("Successfully published to Slack Canvas");
        Ok(())
    }

    fn name(&self) -> &'static str {
        "slack-canvas"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::WorkflowInfo;
    use chrono::Duration;

    #[test]
    fn test_convert_cron_to_kst() {
        assert_eq!(
            SlackCanvasPublisher::convert_cron_to_kst("0 9 * * *"),
            "0 18 * * *"
        );
        assert_eq!(
            SlackCanvasPublisher::convert_cron_to_kst("0 0 * * *"),
            "0 9 * * *"
        );
        assert_eq!(
            SlackCanvasPublisher::convert_cron_to_kst("30 15 * * 1"),
            "30 0 * * 1"
        );
    }

    #[test]
    fn test_convert_cron_to_kst_invalid_parts() {
        assert_eq!(
            SlackCanvasPublisher::convert_cron_to_kst("0 9 * *"),
            "0 9 * *"
        );
    }

    #[test]
    fn test_convert_cron_to_kst_step_values() {
        assert_eq!(
            SlackCanvasPublisher::convert_cron_to_kst("0 */6 * * *"),
            "0 */6 * * *"
        );
    }

    #[test]
    fn test_convert_cron_to_kst_wildcard_hour() {
        assert_eq!(
            SlackCanvasPublisher::convert_cron_to_kst("0 * * * *"),
            "0 * * * *"
        );
    }

    #[test]
    fn test_convert_cron_to_kst_non_numeric_hour() {
        assert_eq!(
            SlackCanvasPublisher::convert_cron_to_kst("0 abc * * *"),
            "0 abc * * *"
        );
    }

    #[test]
    fn test_format_canvas_content() {
        let mut result = ScanResult::new();
        result.total_repos = 10;

        let content = SlackCanvasPublisher::format_canvas_content(&result);
        assert!(content.contains("# GitHub Scheduled Workflows Report"));
        assert!(content.contains("Total Repositories:** 10"));
        assert!(content.contains("No scheduled workflows found."));
    }

    #[test]
    fn test_format_canvas_content_with_workflows() {
        let mut result = ScanResult::new();
        result.total_repos = 10;
        result.excluded_repos_count = 2;
        result.scan_duration = Duration::seconds(30);

        let mut wf1 = WorkflowInfo::new(
            "repo-a".to_string(),
            "Deploy".to_string(),
            1,
            ".github/workflows/deploy.yml".to_string(),
        );
        wf1.cron_schedules = vec!["0 9 * * *".to_string()];
        wf1.last_status = "success".to_string();
        wf1.workflow_last_author = "alice".to_string();
        wf1.is_active_user = true;
        result.workflows.push(wf1);

        let mut wf2 = WorkflowInfo::new(
            "repo-b".to_string(),
            "Cleanup".to_string(),
            2,
            ".github/workflows/cleanup.yml".to_string(),
        );
        wf2.cron_schedules = vec!["0 0 * * *".to_string()];
        wf2.last_status = "failure".to_string();
        wf2.workflow_last_author = "bob".to_string();
        wf2.is_active_user = false;
        result.workflows.push(wf2);

        let content = SlackCanvasPublisher::format_canvas_content(&result);
        assert!(content.contains("repo-a"));
        assert!(content.contains("Deploy"));
        assert!(content.contains("✅"));
        assert!(content.contains("❌"));
        assert!(content.contains("Active"));
        assert!(content.contains("Inactive"));
        assert!(content.contains("0 18 * * *"));
        assert!(content.contains("0 9 * * *"));
    }

    #[test]
    fn test_format_canvas_content_various_statuses() {
        let mut result = ScanResult::new();
        result.total_repos = 5;

        for (i, status) in [
            "cancelled",
            "never_run",
            "unknown_status",
            "completed",
            "failed",
        ]
        .iter()
        .enumerate()
        {
            let mut wf = WorkflowInfo::new(
                format!("repo-{status}"),
                format!("wf-{status}"),
                i as u64,
                ".github/workflows/test.yml".to_string(),
            );
            wf.cron_schedules = vec!["0 0 * * *".to_string()];
            wf.last_status = status.to_string();
            result.workflows.push(wf);
        }

        let content = SlackCanvasPublisher::format_canvas_content(&result);
        assert!(content.contains("🚫"));
        assert!(content.contains("⏸️"));
        assert!(content.contains("❓"));
        assert!(content.contains("✅"));
        assert!(content.contains("❌"));
    }

    #[test]
    fn test_slack_canvas_publisher_name() {
        crate::install_crypto_provider();
        let publisher = SlackCanvasPublisher::new(
            "xoxb-test".to_string(),
            "C123".to_string(),
            "F456".to_string(),
        );
        assert_eq!(publisher.name(), "slack-canvas");
    }
}
