//! HTTP client for the Backstage backend.
//!
//! Every call is a GET. The server this client backs is read-only by design,
//! so there is deliberately no way to issue anything else through it.

use std::time::Duration;

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::de::DeserializeOwned;
use thiserror::Error;
use tracing::{debug, warn};

/// Longest error body echoed back into a tool result.
const MAX_ERROR_BODY: usize = 512;

/// One query string pair.
pub type QueryPair = (&'static str, String);

#[derive(Debug, Error)]
pub enum BackstageError {
    #[error("Backstage returned {status} for GET {path}{}", fmt_detail(.message))]
    Status {
        status: u16,
        path: String,
        message: String,
    },
    #[error("request to Backstage failed for GET {path}: {reason}")]
    Transport { path: String, reason: String },
    #[error("Backstage returned an unreadable body for GET {path}: {reason}")]
    Decode { path: String, reason: String },
}

impl BackstageError {
    /// HTTP status of the failed call, when the call reached Backstage.
    #[must_use]
    pub const fn status(&self) -> Option<u16> {
        match self {
            Self::Status { status, .. } => Some(*status),
            Self::Transport { .. } | Self::Decode { .. } => None,
        }
    }
}

fn fmt_detail(message: &str) -> String {
    if message.is_empty() {
        String::new()
    } else {
        format!(": {message}")
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
}

impl Client {
    /// Builds a client for the Backstage backend at `base_url`. `token` is
    /// sent as a bearer token when non-empty.
    ///
    /// # Errors
    ///
    /// Returns an error when the token is not a valid header value or the
    /// underlying HTTP client cannot be built.
    pub fn new(
        base_url: &str,
        token: &str,
        timeout: Duration,
        user_agent: &str,
    ) -> anyhow::Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_str(user_agent)?);
        if !token.is_empty() {
            let mut value = HeaderValue::from_str(&format!("Bearer {token}"))?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(timeout)
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    /// GETs `path` and decodes the JSON body.
    ///
    /// # Errors
    ///
    /// Returns an error for transport failures, non-2xx statuses and
    /// undecodable bodies.
    pub async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[QueryPair],
    ) -> Result<T, BackstageError> {
        let response = self.get(path, query, "application/json").await?;
        let body = response
            .text()
            .await
            .map_err(|err| BackstageError::Decode {
                path: path.to_string(),
                reason: err.to_string(),
            })?;
        serde_json::from_str(&body).map_err(|err| BackstageError::Decode {
            path: path.to_string(),
            reason: format!("{err} (body starts with {:?})", truncate(&body, 80)),
        })
    }

    /// GETs `path` and returns the raw body as text.
    ///
    /// # Errors
    ///
    /// Returns an error for transport failures and non-2xx statuses.
    pub async fn get_text(
        &self,
        path: &str,
        query: &[QueryPair],
        accept: &str,
    ) -> Result<String, BackstageError> {
        let response = self.get(path, query, accept).await?;
        response.text().await.map_err(|err| BackstageError::Decode {
            path: path.to_string(),
            reason: err.to_string(),
        })
    }

    async fn get(
        &self,
        path: &str,
        query: &[QueryPair],
        accept: &str,
    ) -> Result<reqwest::Response, BackstageError> {
        let url = format!("{}{}", self.base_url, path);
        let started = std::time::Instant::now();
        let response = self
            .http
            .get(&url)
            .header(ACCEPT, accept)
            .query(query)
            .send()
            .await
            .map_err(|err| {
                let reason = if err.is_timeout() {
                    "timed out".to_string()
                } else {
                    // reqwest error text includes the full URL, which may
                    // carry query values; keep only the kind of failure.
                    err.without_url().to_string()
                };
                warn!(path, reason, "backstage request failed");
                BackstageError::Transport {
                    path: path.to_string(),
                    reason,
                }
            })?;

        let status = response.status();
        debug!(
            path,
            status = status.as_u16(),
            duration_ms = started.elapsed().as_millis(),
            "backstage request"
        );
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().await.unwrap_or_default();
        Err(BackstageError::Status {
            status: status.as_u16(),
            path: path.to_string(),
            message: extract_error_message(&body),
        })
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}...", text.chars().take(max).collect::<String>())
    }
}

/// Backstage errors are usually `{ error: { name, message } }` or
/// `{ error: "..." }`; anything else is flattened to one line.
#[must_use]
pub fn extract_error_message(body: &str) -> String {
    if body.is_empty() {
        return String::new();
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(text) = value.get("error").and_then(serde_json::Value::as_str) {
            return truncate(text, MAX_ERROR_BODY);
        }
        if let Some(inner) = value.get("error").filter(|inner| inner.is_object()) {
            let message = inner
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let name = inner.get("name").and_then(serde_json::Value::as_str);
            return truncate(
                &name.map_or_else(|| message.to_string(), |n| format!("{n}: {message}")),
                MAX_ERROR_BODY,
            );
        }
        if let Some(text) = value.get("message").and_then(serde_json::Value::as_str) {
            return truncate(text, MAX_ERROR_BODY);
        }
    }
    truncate(
        &body.split_whitespace().collect::<Vec<_>>().join(" "),
        MAX_ERROR_BODY,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer, token: &str) -> Client {
        Client::new(
            &server.uri(),
            token,
            Duration::from_secs(5),
            "backstage-mcp/test",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn sends_bearer_token_and_query() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/catalog/entities"))
            .and(header("authorization", "Bearer secret"))
            .and(query_param("filter", "kind=component"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;
        let value: serde_json::Value = client(&server, "secret")
            .get_json(
                "/api/catalog/entities",
                &[("filter", "kind=component".to_string())],
            )
            .await
            .unwrap();
        assert_eq!(value["ok"], true);
    }

    #[tokio::test]
    async fn status_errors_carry_backstage_message() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/x"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": {"name": "NotFoundError", "message": "no such entity"}
            })))
            .mount(&server)
            .await;
        let err = client(&server, "")
            .get_json::<serde_json::Value>("/api/x", &[])
            .await
            .unwrap_err();
        assert_eq!(err.status(), Some(404));
        assert_eq!(
            err.to_string(),
            "Backstage returned 404 for GET /api/x: NotFoundError: no such entity"
        );
    }

    #[tokio::test]
    async fn text_and_decode_paths() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/yaml"))
            .respond_with(ResponseTemplate::new(200).set_body_string("kind: API\n"))
            .mount(&server)
            .await;
        let text = client(&server, "")
            .get_text("/yaml", &[], "text/yaml")
            .await
            .unwrap();
        assert_eq!(text, "kind: API\n");
        let err = client(&server, "")
            .get_json::<serde_json::Value>("/yaml", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, BackstageError::Decode { .. }));
        assert_eq!(err.status(), None);
    }

    #[tokio::test]
    async fn transport_errors_do_not_leak_the_url() {
        let client = Client::new("http://127.0.0.1:1", "", Duration::from_secs(1), "t").unwrap();
        let err = client
            .get_json::<serde_json::Value>("/api/x?token=abc", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, BackstageError::Transport { .. }));
        assert!(err.to_string().contains("GET /api/x"));
    }

    #[test]
    fn error_message_extraction() {
        assert_eq!(extract_error_message(""), "");
        assert_eq!(
            extract_error_message(r#"{"error":"Admin only"}"#),
            "Admin only"
        );
        assert_eq!(extract_error_message(r#"{"message":"nope"}"#), "nope");
        assert_eq!(extract_error_message(r#"{"error":{"message":"m"}}"#), "m");
        assert_eq!(
            extract_error_message("<html>\n  Bad   Gateway\n</html>"),
            "<html> Bad Gateway </html>"
        );
        assert!(extract_error_message(&"x".repeat(1000)).len() < 600);
    }
}
