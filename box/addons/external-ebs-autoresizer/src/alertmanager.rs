//! Posts notifications about resize operations to the Alertmanager v2 API
//! (`POST /api/v2/alerts`). Alerts are sent with only a `startsAt` timestamp,
//! so Alertmanager auto-resolves them after its configured `resolve_timeout`:
//! each resize is a one-shot event rather than a long-lived firing alert.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use tracing::warn;

/// The Alertmanager v2 endpoint for posting alerts.
const ALERTS_PATH: &str = "/api/v2/alerts";
/// The Alertmanager v2 status endpoint used by the startup preflight check.
/// `/-/healthy` only proves the process is up and is not served by Mimir,
/// whose Alertmanager lives under the `/alertmanager` prefix; `/api/v2/status`
/// exists on both and exercises the same API the alerts are posted to.
const HEALTH_PATH: &str = "/api/v2/status";

/// The outcome of a preflight check: the checked endpoint, the HTTP status
/// (0 when the request never completed), and the request latency.
#[derive(Debug, Clone)]
pub struct Preflight {
    pub endpoint: String,
    pub status: u16,
    pub latency: Duration,
}

/// Posts alerts to an Alertmanager v2 endpoint. Delivery is best-effort:
/// failures are logged, never returned, so alerting never blocks or fails a
/// reconcile.
pub struct Client {
    base: String,
    endpoint: String,
    http: reqwest::Client,
    extra_labels: BTreeMap<String, String>,
    dashboard_url_tmpl: String,
}

impl Client {
    /// Builds a client targeting `base_url` (e.g. `http://alertmanager:9093`).
    /// `timeout` bounds each request. `extra_labels` are merged into every
    /// alert's labels for routing; per-alert labels take precedence.
    /// `dashboard_url_tmpl` is an optional URL template appended to each
    /// alert's description as a Slack mrkdwn link; `{key}` placeholders are
    /// substituted with the alert's labels. An empty template disables the
    /// link.
    #[must_use]
    pub fn new(
        base_url: &str,
        timeout: Duration,
        extra_labels: BTreeMap<String, String>,
        dashboard_url_tmpl: &str,
    ) -> Self {
        let timeout = if timeout.is_zero() {
            Duration::from_secs(5)
        } else {
            timeout
        };
        let base = base_url.trim_end_matches('/').to_string();
        Self {
            endpoint: format!("{base}{ALERTS_PATH}"),
            base,
            http: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .unwrap_or_default(),
            extra_labels,
            dashboard_url_tmpl: dashboard_url_tmpl.to_string(),
        }
    }

    /// Performs a one-time connectivity check against the Alertmanager v2
    /// status endpoint. It never mutates state; the caller decides what to do
    /// with a failure.
    pub async fn preflight(&self) -> Result<Preflight, (Preflight, String)> {
        let endpoint = format!("{}{HEALTH_PATH}", self.base);
        let start = Instant::now();
        let resp = self.http.get(&endpoint).send().await;
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
                    return Err((p, format!("alertmanager health returned {status}: {msg}")));
                }
                Ok(p)
            }
        }
    }

    /// Builds and posts a single alert. `severity` and `alertname` become
    /// labels; `summary` and `description` become annotations (description is
    /// omitted when empty). `labels` are per-alert identifying labels merged
    /// on top of the client's static extra labels.
    pub async fn notify(
        &self,
        severity: &str,
        alertname: &str,
        summary: &str,
        description: &str,
        labels: &BTreeMap<String, String>,
        starts_at: DateTime<Utc>,
    ) {
        let mut merged = self.extra_labels.clone();
        merged.extend(labels.iter().map(|(k, v)| (k.clone(), v.clone())));
        merged.insert("alertname".into(), alertname.into());
        merged.insert("severity".into(), severity.into());

        let mut description = description.to_string();
        if !self.dashboard_url_tmpl.is_empty() {
            let link = format!(
                "(<{}|Dashboard>)",
                render_dashboard_url(&self.dashboard_url_tmpl, &merged)
            );
            if description.is_empty() {
                description = link;
            } else {
                description.push(' ');
                description.push_str(&link);
            }
        }

        let mut annotations = BTreeMap::from([("summary".to_string(), summary.to_string())]);
        if !description.is_empty() {
            annotations.insert("description".into(), description);
        }
        let alert = WireAlert {
            labels: merged,
            annotations,
            starts_at: starts_at.to_rfc3339_opts(SecondsFormat::Secs, true),
        };
        if let Err(err) = self.post(&[alert]).await {
            warn!(alertname, error = %err, "failed to send Alertmanager alert");
        }
    }

    async fn post(&self, alerts: &[WireAlert]) -> Result<(), String> {
        let resp = self
            .http
            .post(&self.endpoint)
            .json(alerts)
            .send()
            .await
            .map_err(|err| format!("post {}: {err}", self.endpoint))?;
        let status = resp.status();
        if status.as_u16() >= 300 {
            let msg = snippet(&resp.text().await.unwrap_or_default(), 512);
            return Err(format!("alertmanager returned {status}: {msg}"));
        }
        Ok(())
    }
}

/// The JSON shape of a single Alertmanager v2 postable alert.
#[derive(Debug, Serialize)]
struct WireAlert {
    labels: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    annotations: BTreeMap<String, String>,
    #[serde(rename = "startsAt", skip_serializing_if = "String::is_empty")]
    starts_at: String,
}

/// Substitutes `{key}` placeholders in `tmpl` with the matching label values.
/// Placeholders without a matching label are left untouched.
fn render_dashboard_url(tmpl: &str, labels: &BTreeMap<String, String>) -> String {
    let mut out = tmpl.to_string();
    for (k, v) in labels {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

/// Trims a response body down to a loggable length.
pub(crate) fn snippet(body: &str, limit: usize) -> String {
    let s = body.trim();
    if s.len() > limit {
        let mut end = limit;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        return format!("{}...", &s[..end]);
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn labels() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("instance_id".to_string(), "i-1".to_string()),
            ("volume_id".to_string(), "vol-1".to_string()),
        ])
    }

    #[tokio::test]
    async fn notify_posts_a_v2_alert() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/alerts"))
            .and(header("content-type", "application/json"))
            .and(body_partial_json(serde_json::json!([{
                "labels": {"alertname": "EBSRootVolumeAutoresizeCompleted", "severity": "info", "cluster": "prod", "instance_id": "i-1", "volume_id": "vol-1"},
                "annotations": {"summary": "done", "description": "desc"},
                "startsAt": "2024-01-02T03:04:05Z"
            }])))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let c = Client::new(
            &format!("{}/", server.uri()),
            Duration::ZERO,
            BTreeMap::from([
                ("cluster".to_string(), "prod".to_string()),
                ("instance_id".to_string(), "overridden".to_string()),
            ]),
            "",
        );
        assert_eq!(
            c.endpoint,
            format!("{}/api/v2/alerts", server.uri()),
            "trailing slash trimmed"
        );
        let at = DateTime::parse_from_rfc3339("2024-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&Utc);
        c.notify(
            "info",
            "EBSRootVolumeAutoresizeCompleted",
            "done",
            "desc",
            &labels(),
            at,
        )
        .await;
    }

    #[tokio::test]
    async fn notify_appends_dashboard_link_and_omits_empty_description() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(serde_json::json!([{
                "annotations": {"summary": "s", "description": "(<https://g/d?i=i-1&v=vol-1&x={unknown}|Dashboard>)"}
            }])))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let c = Client::new(
            &server.uri(),
            Duration::from_secs(1),
            BTreeMap::new(),
            "https://g/d?i={instance_id}&v={volume_id}&x={unknown}",
        );
        c.notify("warning", "a", "s", "", &labels(), Utc::now())
            .await;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(serde_json::json!([{
                "annotations": {"summary": "s", "description": "text (<https://g|Dashboard>)"}
            }])))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let c = Client::new(
            &server.uri(),
            Duration::from_secs(1),
            BTreeMap::new(),
            "https://g",
        );
        c.notify("warning", "a", "s", "text", &labels(), Utc::now())
            .await;
    }

    #[tokio::test]
    async fn notify_swallows_failures() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .expect(1)
            .mount(&server)
            .await;
        let c = Client::new(&server.uri(), Duration::from_secs(1), BTreeMap::new(), "");
        c.notify("info", "a", "s", "d", &labels(), Utc::now()).await;
        let c = Client::new(
            "http://127.0.0.1:1",
            Duration::from_secs(1),
            BTreeMap::new(),
            "",
        );
        c.notify("info", "a", "s", "d", &labels(), Utc::now()).await;
    }

    #[tokio::test]
    async fn preflight_outcomes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v2/status"))
            .respond_with(ResponseTemplate::new(200).set_body_string("OK"))
            .mount(&server)
            .await;
        let c = Client::new(&server.uri(), Duration::from_secs(1), BTreeMap::new(), "");
        let p = c.preflight().await.unwrap();
        assert_eq!(p.status, 200);
        assert_eq!(p.endpoint, format!("{}/api/v2/status", server.uri()));

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503).set_body_string("  down  "))
            .mount(&server)
            .await;
        let c = Client::new(&server.uri(), Duration::from_secs(1), BTreeMap::new(), "");
        let (p, err) = c.preflight().await.unwrap_err();
        assert_eq!(p.status, 503);
        assert!(
            err.contains("alertmanager health returned 503") && err.ends_with("down"),
            "{err}"
        );

        let c = Client::new(
            "http://127.0.0.1:1",
            Duration::from_secs(1),
            BTreeMap::new(),
            "",
        );
        let (p, err) = c.preflight().await.unwrap_err();
        assert_eq!(p.status, 0);
        assert!(
            err.starts_with("get http://127.0.0.1:1/api/v2/status"),
            "{err}"
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(
            render_dashboard_url(
                "{a}-{b}-{c}",
                &BTreeMap::from([
                    ("a".to_string(), "1".to_string()),
                    ("b".to_string(), "2".to_string())
                ])
            ),
            "1-2-{c}"
        );
        assert_eq!(snippet("  abc  ", 10), "abc");
        assert_eq!(snippet("abcdef", 3), "abc...");
        assert_eq!(snippet("héllo", 2), "h...");
    }
}
