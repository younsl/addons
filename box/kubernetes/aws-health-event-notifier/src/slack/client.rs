use std::time::Duration;

use reqwest::{Client, Url};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;

use crate::error::{AppError, AppResult};

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
            .timeout(timeout)
            .user_agent(concat!(
                "aws-health-event-notifier/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(|e| AppError::Other(anyhow::anyhow!("build slack http client: {e}")))?;
        Ok(Self { http, webhook_url })
    }

    pub async fn post(&self, payload: &Value) -> AppResult<()> {
        let resp = self
            .http
            .post(self.webhook_url.clone())
            .json(payload)
            .send()
            .await
            .map_err(|e| AppError::Slack(format!("request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AppError::Slack(format!("status={status} body={body}")));
        }
        Ok(())
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

    #[tokio::test]
    async fn post_errors_on_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_string("invalid_token"))
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let err = c.post(&json!({"text": "hi"})).await.unwrap_err();
        assert!(matches!(err, AppError::Slack(m) if m.contains("403")));
    }

    #[tokio::test]
    async fn post_errors_on_connection_failure() {
        // Unroutable port: request itself fails before any response.
        let c = client("http://127.0.0.1:1/hook");
        let err = c.post(&json!({"text": "hi"})).await.unwrap_err();
        assert!(matches!(err, AppError::Slack(m) if m.contains("request failed")));
    }
}
