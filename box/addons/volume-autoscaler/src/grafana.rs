//! Posts annotations about resize operations to the Grafana HTTP API
//! (`POST /api/annotations`). Annotations are tag-based and global (no
//! dashboard UID), so any dashboard that subscribes to the configured tags
//! renders the resize markers. A completed resize is a region annotation
//! spanning its duration; a failure is a point annotation.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use tracing::warn;

use crate::alertmanager::{Preflight, snippet};

const ANNOTATIONS_PATH: &str = "/api/annotations";
const HEALTH_PATH: &str = "/api/health";

/// Posts annotations to a Grafana endpoint. Delivery is best-effort: failures
/// are logged, never returned, so annotating never blocks or fails a
/// reconcile.
pub struct Client {
    base: String,
    endpoint: String,
    token: String,
    http: reqwest::Client,
    base_tags: Vec<String>,
}

impl Client {
    /// Builds a client targeting `base_url`. `token` is a Grafana service
    /// account token sent as a Bearer credential. `base_tags` are merged into
    /// every annotation's tags and are what dashboards subscribe to;
    /// per-annotation tags are appended after them.
    #[must_use]
    pub fn new(base_url: &str, token: &str, timeout: Duration, base_tags: Vec<String>) -> Self {
        let timeout = if timeout.is_zero() {
            Duration::from_secs(5)
        } else {
            timeout
        };
        let base = base_url.trim_end_matches('/').to_string();
        Self {
            endpoint: format!("{base}{ANNOTATIONS_PATH}"),
            base,
            token: token.to_string(),
            http: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .unwrap_or_default(),
            base_tags,
        }
    }

    fn authorized(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.token.is_empty() {
            req
        } else {
            req.bearer_auth(&self.token)
        }
    }

    /// Performs a one-time health check against the Grafana health endpoint.
    /// The token is sent so the request also exercises the auth path, though
    /// `/api/health` itself does not require authentication.
    pub async fn preflight(&self) -> Result<Preflight, (Preflight, String)> {
        let endpoint = format!("{}{HEALTH_PATH}", self.base);
        let start = Instant::now();
        let resp = self.authorized(self.http.get(&endpoint)).send().await;
        let latency = start.elapsed();
        match resp {
            Err(err) => Err((
                Preflight {
                    endpoint: endpoint.clone(),
                    status: 0,
                    latency,
                },
                format!("get {endpoint}: {err}"),
            )),
            Ok(resp) => {
                let status = resp.status();
                let p = Preflight {
                    endpoint,
                    status: status.as_u16(),
                    latency,
                };
                if status.as_u16() >= 300 {
                    let msg = snippet(&resp.text().await.unwrap_or_default(), 256);
                    return Err((p, format!("grafana health returned {status}: {msg}")));
                }
                Ok(p)
            }
        }
    }

    /// Builds and posts a single annotation. `text` is the marker body; `tags`
    /// are appended to the client's base tags. `start` is the marker time;
    /// when `end` is set the annotation is a region spanning start..end,
    /// otherwise it is a point annotation at `start`.
    pub async fn annotate(
        &self,
        text: &str,
        tags: &[String],
        start: DateTime<Utc>,
        end: Option<DateTime<Utc>>,
    ) {
        let mut merged = self.base_tags.clone();
        merged.extend(tags.iter().cloned());
        let ann = WireAnnotation {
            time: start.timestamp_millis(),
            time_end: end.map(|e| e.timestamp_millis()),
            tags: merged,
            text: text.to_string(),
        };
        if let Err(err) = self.post(&ann).await {
            warn!(error = %err, "failed to post Grafana annotation");
        }
    }

    async fn post(&self, ann: &WireAnnotation) -> Result<(), String> {
        let resp = self
            .authorized(self.http.post(&self.endpoint).json(ann))
            .send()
            .await
            .map_err(|err| format!("post {}: {err}", self.endpoint))?;
        let status = resp.status();
        if status.as_u16() >= 300 {
            let msg = snippet(&resp.text().await.unwrap_or_default(), 512);
            return Err(format!("grafana returned {status}: {msg}"));
        }
        Ok(())
    }
}

/// The JSON shape of a single Grafana annotation. `time` and `timeEnd` are
/// epoch milliseconds; `timeEnd` is omitted for a point annotation.
#[derive(Debug, Serialize)]
struct WireAnnotation {
    time: i64,
    #[serde(rename = "timeEnd", skip_serializing_if = "Option::is_none")]
    time_end: Option<i64>,
    tags: Vec<String>,
    text: String,
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_json, header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn at(ms: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(ms).unwrap()
    }

    #[tokio::test]
    async fn annotate_posts_region_and_point() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/annotations"))
            .and(header("authorization", "Bearer tok"))
            .and(body_json(serde_json::json!({
                "time": 1000, "timeEnd": 5000, "tags": ["event:ebs-resize", "instance_id:i-1"], "text": "resized"
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_json(serde_json::json!({
                "time": 7000, "tags": ["event:ebs-resize"], "text": "failed"
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let c = Client::new(
            &format!("{}/", server.uri()),
            "tok",
            Duration::ZERO,
            vec!["event:ebs-resize".into()],
        );
        c.annotate(
            "resized",
            &["instance_id:i-1".into()],
            at(1000),
            Some(at(5000)),
        )
        .await;
        c.annotate("failed", &[], at(7000), None).await;
    }

    #[tokio::test]
    async fn no_token_omits_auth_and_failures_are_swallowed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("no"))
            .expect(1)
            .mount(&server)
            .await;
        let c = Client::new(&server.uri(), "", Duration::from_secs(1), vec![]);
        c.annotate("x", &[], at(1), None).await;
        let c = Client::new("http://127.0.0.1:1", "", Duration::from_secs(1), vec![]);
        c.annotate("x", &[], at(1), None).await;
    }

    #[tokio::test]
    async fn preflight_outcomes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/health"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let c = Client::new(&server.uri(), "tok", Duration::from_secs(1), vec![]);
        let p = c.preflight().await.unwrap();
        assert_eq!(p.status, 200);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500).set_body_string("bad"))
            .mount(&server)
            .await;
        let c = Client::new(&server.uri(), "", Duration::from_secs(1), vec![]);
        let (p, err) = c.preflight().await.unwrap_err();
        assert_eq!(p.status, 500);
        assert!(
            err.contains("grafana health returned 500") && err.ends_with("bad"),
            "{err}"
        );

        let c = Client::new("http://127.0.0.1:1", "", Duration::from_secs(1), vec![]);
        let (p, err) = c.preflight().await.unwrap_err();
        assert_eq!(p.status, 0);
        assert!(err.starts_with("get "), "{err}");
    }
}
