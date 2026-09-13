//! Tool modules, one per Backstage feature, plus the helpers they share.
//!
//! Every tool is a GET against the Backstage backend followed by filtering,
//! sorting and paging done here, because most plugin endpoints return every
//! row at once and an agent needs a bounded slice of them.

pub mod argocd;
pub mod catalog;
pub mod catalog_health;
pub mod gitlab_token_audit;
pub mod iam_user_audit;
pub mod openapi_registry;
pub mod opencost;
pub mod opensearch;
pub mod opensearch_scaling;
pub mod platforms;
pub mod s3_log_extract;
pub mod search;
pub mod techdocs;

use rmcp::model::{CallToolResult, ContentBlock};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, warn};

use crate::backstage::BackstageError;

/// Failure of one tool call, reported to the model as a tool error rather
/// than a protocol error so it can read what went wrong and adjust.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error(transparent)]
    Backstage(#[from] BackstageError),
    #[error("{0}")]
    Input(String),
    #[error("{0}")]
    NotFound(String),
    #[error("failed to encode result: {0}")]
    Encode(#[from] serde_json::Error),
}

impl ToolError {
    fn message(&self) -> String {
        match self {
            Self::Backstage(err) if matches!(err.status(), Some(401 | 403)) => {
                format!(
                    "{err}. The Backstage token backstage-mcp holds is not allowed to read this endpoint."
                )
            }
            other => other.to_string(),
        }
    }
}

pub type ToolResult = Result<Value, ToolError>;

/// Serializes a tool outcome into the MCP result, truncating oversized
/// payloads with a note that tells the model how to narrow the query.
#[must_use]
pub fn respond(tool: &str, outcome: ToolResult, max_chars: usize) -> CallToolResult {
    match outcome {
        Ok(value) => {
            let text = match value {
                Value::String(text) => text,
                other => other.to_string(),
            };
            debug!(tool, chars = text.len(), "tool ok");
            CallToolResult::success(vec![ContentBlock::text(truncate(&text, max_chars))])
        }
        Err(err) => {
            let message = err.message();
            warn!(tool, error = %message, "tool failed");
            CallToolResult::error(vec![ContentBlock::text(message)])
        }
    }
}

/// Cuts `text` to at most `max_chars` characters, appending a hint.
#[must_use]
pub fn truncate(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!(
        "{kept}\n\n[truncated {} characters; narrow the query with filters or a smaller limit]",
        total - max_chars
    )
}

/// Serializes a value as a tool outcome.
///
/// # Errors
///
/// Returns an error when the value cannot be serialized.
pub fn json<T: Serialize>(value: T) -> ToolResult {
    Ok(serde_json::to_value(value)?)
}

/// Offset and limit accepted by every list tool that pages client-side.
#[derive(Debug, Default, Clone, Copy, Deserialize, JsonSchema)]
pub struct Paging {
    /// Number of matching rows to skip, default 0
    #[serde(default)]
    pub offset: Option<usize>,
    /// Maximum rows to return, default 50
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One page of a client-side paged list.
#[derive(Debug, Serialize)]
pub struct Page<T> {
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub truncated: bool,
    pub items: Vec<T>,
}

/// Applies `paging` to `items`, reporting the total so the model knows how
/// much it did not see.
#[must_use]
pub fn page<T>(items: Vec<T>, paging: Paging, default_limit: usize) -> Page<T> {
    let total = items.len();
    let offset = paging.offset.unwrap_or(0).min(total);
    let limit = paging.limit.unwrap_or(default_limit).max(1);
    let page_items: Vec<T> = items.into_iter().skip(offset).take(limit).collect();
    let truncated = offset + page_items.len() < total;
    Page {
        total,
        offset,
        limit,
        truncated,
        items: page_items,
    }
}

/// Case-insensitive substring match across any of `fields`. An absent needle
/// matches everything.
#[must_use]
pub fn matches_text(needle: Option<&str>, fields: &[Option<&str>]) -> bool {
    let Some(needle) = needle.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    let needle = needle.to_lowercase();
    fields
        .iter()
        .flatten()
        .any(|field| field.to_lowercase().contains(&needle))
}

/// Exact match against an optional filter.
#[must_use]
pub fn matches_eq<T: PartialEq + ?Sized>(filter: Option<&T>, value: &T) -> bool {
    filter.is_none_or(|wanted| wanted == value)
}

/// Reads a string field off a JSON object, for rows kept as raw values.
#[must_use]
pub fn str_field<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key).and_then(Value::as_str)
}

/// Reads a number field off a JSON object.
#[must_use]
pub fn f64_field(row: &Value, key: &str) -> Option<f64> {
    row.get(key).and_then(Value::as_f64)
}

/// Joins a comma-separated list parameter, dropping empty entries.
#[must_use]
pub fn join_csv(values: Option<&[String]>) -> Option<String> {
    let joined = values?
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(",");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Sorts rows by an ISO timestamp field, newest first.
pub fn sort_newest_first(rows: &mut [Value], key: &str) {
    rows.sort_by(|a, b| {
        str_field(b, key)
            .unwrap_or("")
            .cmp(str_field(a, key).unwrap_or(""))
    });
}

/// Sorts rows by a numeric field, largest first.
pub fn sort_desc_by(rows: &mut [Value], key: &str) {
    rows.sort_by(|a, b| {
        f64_field(b, key)
            .unwrap_or(0.0)
            .partial_cmp(&f64_field(a, key).unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

#[cfg(test)]
pub mod testing {
    use std::sync::Arc;
    use std::time::Duration;

    use wiremock::MockServer;

    use crate::backstage::Client;
    use crate::server::BackstageMcp;

    /// A server wired to a mock Backstage.
    pub async fn mcp() -> (MockServer, BackstageMcp) {
        let server = MockServer::start().await;
        let client = Client::new(
            &server.uri(),
            "test-token",
            Duration::from_secs(5),
            "backstage-mcp/test",
        )
        .unwrap();
        (server, BackstageMcp::new(Arc::new(client), 100_000))
    }

    /// The text content of a tool result.
    pub fn text(result: &rmcp::model::CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| block.as_text().map(|t| t.text.clone()))
            .collect::<String>()
    }

    /// The tool result parsed as JSON, panicking on a tool error.
    pub fn json(result: &rmcp::model::CallToolResult) -> serde_json::Value {
        assert_ne!(result.is_error, Some(true), "tool error: {}", text(result));
        serde_json::from_str(&text(result)).expect("json result")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paging_reports_totals() {
        let rows: Vec<u32> = (0..10).collect();
        let p = page(
            rows.clone(),
            Paging {
                offset: Some(8),
                limit: Some(5),
            },
            50,
        );
        assert_eq!((p.total, p.offset, p.limit, p.truncated), (10, 8, 5, false));
        assert_eq!(p.items, vec![8, 9]);
        let p = page(rows.clone(), Paging::default(), 3);
        assert_eq!(p.items, vec![0, 1, 2]);
        assert!(p.truncated);
        let p = page(
            rows,
            Paging {
                offset: Some(99),
                limit: Some(0),
            },
            3,
        );
        assert_eq!((p.offset, p.limit), (10, 1));
        assert!(p.items.is_empty());
        assert!(!p.truncated);
    }

    #[test]
    fn text_matching() {
        assert!(matches_text(None, &[Some("x")]));
        assert!(matches_text(Some("  "), &[None]));
        assert!(matches_text(Some("PAY"), &[None, Some("team-payments")]));
        assert!(!matches_text(Some("pay"), &[Some("orders")]));
        assert!(matches_eq(None::<&String>, &"a".to_string()));
        assert!(!matches_eq(Some(&"b".to_string()), &"a".to_string()));
    }

    #[test]
    fn truncation_and_response() {
        assert_eq!(truncate("abc", 5), "abc");
        let cut = truncate("abcdef", 3);
        assert!(cut.starts_with("abc\n\n[truncated 3 characters"));

        let ok = respond("t", Ok(json!({"a": 1})), 100);
        assert_eq!(ok.is_error, Some(false));
        let ok = respond("t", Ok(Value::String("plain".into())), 100);
        assert_eq!(testing::text(&ok), "plain");

        let denied = respond(
            "t",
            Err(ToolError::Backstage(BackstageError::Status {
                status: 403,
                path: "/api/x".into(),
                message: "Admin only".into(),
            })),
            100,
        );
        assert_eq!(denied.is_error, Some(true));
        assert!(testing::text(&denied).contains("not allowed to read"));
        let input = respond("t", Err(ToolError::Input("bad".into())), 100);
        assert_eq!(testing::text(&input), "bad");
    }

    #[test]
    fn json_helpers() {
        let mut rows = vec![
            json!({"createdAt": "2026-01-01", "cost": 1.5}),
            json!({"createdAt": "2026-03-01", "cost": 0.5}),
            json!({"cost": 9.0}),
        ];
        sort_newest_first(&mut rows, "createdAt");
        assert_eq!(str_field(&rows[0], "createdAt"), Some("2026-03-01"));
        sort_desc_by(&mut rows, "cost");
        assert_eq!(f64_field(&rows[0], "cost"), Some(9.0));
        assert_eq!(join_csv(None), None);
        assert_eq!(
            join_csv(Some(&[" a".into(), String::new(), "b".into()])),
            Some("a,b".into())
        );
        assert_eq!(join_csv(Some(&[String::new()])), None);
    }
}

/// Query pairs collected for one request.
pub type QueryPairs = Vec<crate::backstage::QueryPair>;
