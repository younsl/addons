use std::time::Duration;

use reqwest::{Client, StatusCode, Url};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::slack::error::SlackError;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone)]
pub struct SlackClient {
    http: Client,
    webhook_url: Url,
}

impl SlackClient {
    /// Builds a client for the given webhook URL.
    ///
    /// The webhook URL carries the Slack secret in its path, so it is only
    /// accepted over `https`. Plain `http` is allowed for loopback hosts so
    /// local mock servers still work.
    pub fn new(webhook_url: &SecretString, timeout: Duration) -> AppResult<Self> {
        let webhook_url = validate_webhook_url(webhook_url.expose_secret())?;
        let http = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT.min(timeout))
            .timeout(timeout)
            .user_agent(concat!(
                "aws-health-event-notifier/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(|e| AppError::Other(anyhow::anyhow!("build slack http client: {e}")))?;
        Ok(Self { http, webhook_url })
    }

    pub async fn post(&self, payload: &Value) -> Result<(), SlackError> {
        let resp = self
            .http
            .post(self.webhook_url.clone())
            .json(payload)
            .send()
            .await
            .map_err(classify_transport)?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(classify_status(status, &body))
    }
}

/// Only a failure to connect proves the request never reached Slack.
/// The webhook URL path is the secret, so it is stripped before formatting.
fn classify_transport(e: reqwest::Error) -> SlackError {
    let e = e.without_url();
    if e.is_connect() || e.is_builder() {
        SlackError::NotPosted(format!("request failed: {e}"))
    } else {
        SlackError::Ambiguous(format!("request failed: {e}"))
    }
}

fn classify_status(status: StatusCode, body: &str) -> SlackError {
    let detail = format!("status={status} body={body}");
    if status == StatusCode::TOO_MANY_REQUESTS {
        SlackError::RateLimited(detail)
    } else if status == StatusCode::BAD_REQUEST && body.contains("invalid_payload") {
        SlackError::InvalidPayload(detail)
    } else if status.is_client_error() {
        SlackError::NotPosted(detail)
    } else {
        SlackError::Ambiguous(detail)
    }
}

fn validate_webhook_url(raw: &str) -> AppResult<Url> {
    let url = Url::parse(raw).map_err(|e| AppError::Slack(format!("invalid webhook url: {e}")))?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    match url.scheme() {
        "https" => Ok(url),
        "http" if loopback => Ok(url),
        other => Err(AppError::Slack(format!(
            "webhook url must use https, got scheme {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(url: &str) -> SlackClient {
        SlackClient::new(&SecretString::from(url.to_string()), Duration::from_secs(5)).unwrap()
    }

    #[test]
    fn new_accepts_https() {
        let c = SlackClient::new(
            &SecretString::from("https://hooks.slack.com/services/T/B/x".to_string()),
            Duration::from_secs(5),
        );
        assert!(c.is_ok());
    }

    #[test]
    fn new_rejects_cleartext_http_to_remote_host() {
        let err = SlackClient::new(
            &SecretString::from("http://hooks.slack.com/services/T/B/x".to_string()),
            Duration::from_secs(5),
        )
        .err()
        .unwrap();
        assert!(matches!(err, AppError::Slack(m) if m.contains("must use https")));
    }

    #[test]
    fn new_rejects_unparsable_url() {
        let err = SlackClient::new(
            &SecretString::from("not a url".to_string()),
            Duration::from_secs(5),
        )
        .err()
        .unwrap();
        assert!(matches!(err, AppError::Slack(m) if m.contains("invalid webhook url")));
    }

    #[tokio::test]
    async fn post_ok_on_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;
        let c = client(&format!("{}/hook", server.uri()));
        assert!(c.post(&json!({"text": "hi"})).await.is_ok());
    }

    async fn post_with_status(status: u16, body: &str) -> SlackError {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&server)
            .await;
        client(&server.uri())
            .post(&json!({"text": "hi"}))
            .await
            .unwrap_err()
    }

    #[tokio::test]
    async fn post_4xx_is_not_posted() {
        let err = post_with_status(403, "invalid_token").await;
        assert!(matches!(&err, SlackError::NotPosted(m) if m.contains("403")));
        assert!(err.is_retry_safe());
    }

    #[tokio::test]
    async fn post_429_is_rate_limited() {
        let err = post_with_status(429, "rate_limited").await;
        assert!(matches!(err, SlackError::RateLimited(_)));
        assert!(err.is_retry_safe());
    }

    #[tokio::test]
    async fn post_invalid_payload_is_permanent() {
        let err = post_with_status(400, "invalid_payload").await;
        assert!(matches!(err, SlackError::InvalidPayload(_)));
        assert!(!err.is_retry_safe());
    }

    #[tokio::test]
    async fn post_5xx_is_ambiguous() {
        let err = post_with_status(500, "rollup_error").await;
        assert!(matches!(err, SlackError::Ambiguous(_)));
        assert!(!err.is_retry_safe());
    }

    #[tokio::test]
    async fn post_timeout_after_send_is_ambiguous() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)))
            .mount(&server)
            .await;
        let c = SlackClient::new(
            &SecretString::from(server.uri()),
            Duration::from_millis(200),
        )
        .unwrap();
        let err = c.post(&json!({"text": "hi"})).await.unwrap_err();
        assert!(matches!(err, SlackError::Ambiguous(_)));
    }

    #[tokio::test]
    async fn post_connection_failure_is_not_posted() {
        // Unroutable port: request itself fails before any response.
        let c = client("http://127.0.0.1:1/hook");
        let err = c.post(&json!({"text": "hi"})).await.unwrap_err();
        assert!(
            matches!(&err, SlackError::NotPosted(m) if m.contains("request failed") && !m.contains("/hook"))
        );
    }
}
