//! Delivers outbound alarms to configured receivers (named webhook channels).
//! Delivery is best-effort and asynchronous: a failure is logged, never
//! surfaced to the request path that triggered it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::{SecondsFormat, Utc};
use parking_lot::{Mutex, RwLock};
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

mod batch;
mod coverage;

pub use batch::PreviewPackage;
pub use coverage::{
    CoveragePayload, CoverageProject, CoverageReport, is_full_coverage, sample_coverage_report,
};

/// Errors returned by the synchronous senders.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The webhook could not be reached or the request failed in transport.
    #[error("{0}")]
    Http(#[from] reqwest::Error),
    /// A payload could not be encoded.
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    /// The webhook answered with a non-2xx status.
    #[error("webhook returned status {0}")]
    Status(u16),
    /// Callers holding an `Option<Arc<Notifier>>` return this when it is `None`.
    #[error("notifications are not configured")]
    NotConfigured,
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, Error>;

/// A resolved delivery destination: a receiver's display name and its webhook
/// URL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Target {
    pub name: String,
    pub url: String,
}

/// Returns a coordinate's stored scan verdict: its max severity ("none" when
/// Clean), the CVSS score and advisory id (CVE/GHSA) at that severity, and
/// whether it was scanned at all. Arguments are `(repo_id, pkg, version)`.
pub type CleanChecker =
    Arc<dyn Fn(i64, &str, &str) -> (String, String, String, bool) + Send + Sync>;

/// Invoked after each approval-alarm delivery attempt with the packages the
/// message covered, the outcome (`result`, `detail`) and the elapsed
/// milliseconds.
pub type DeliveryRecorder = Arc<dyn Fn(Vec<DeliveredPackage>, &str, &str, i64) + Send + Sync>;

/// Wiring set after construction, kept behind a lock so the setters work on a
/// shared notifier.
#[derive(Clone, Default)]
struct Settings {
    /// Forklift's externally-visible base (FORKLIFT_EXTERNAL_URL); when set,
    /// the "Forklift" keyword in alarm text links to it.
    external_url: String,
    /// Coalesces approval alarms per receiver over this window so an install
    /// burst yields one grouped message instead of one webhook per package.
    /// Zero disables batching (each event is delivered immediately).
    batch_window: Duration,
    /// Returns a coordinate's stored scan verdict. Injected from main (it reads
    /// stored vuln scans); `None` disables the Clean/Dirty breakdown in grouped
    /// alarms.
    clean_checker: Option<CleanChecker>,
    /// When set, invoked after each approval-alarm delivery attempt so the
    /// caller can persist the result on those approval rows. Injected from main
    /// (it writes to the store); `None` disables recording.
    delivery_recorder: Option<DeliveryRecorder>,
}

/// The buffered approval events and the timer that will flush them.
#[derive(Default)]
struct BatchState {
    /// Keyed by webhook URL.
    batch: Option<HashMap<String, TargetBatch>>,
    flush_timer: Option<JoinHandle<()>>,
}

/// Posts JSON alarms to webhook targets.
pub struct Notifier {
    client: reqwest::Client,
    timeout: Duration,
    settings: RwLock<Settings>,
    state: Mutex<BatchState>,
}

/// Identifies one quarantined package an approval alarm covered, so its
/// delivery outcome can be recorded on the matching approval row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeliveredPackage {
    pub repo: String,
    pub package: String,
}

/// Delivery result value recorded on an approval row: the webhook took it.
pub const DELIVERY_DELIVERED: &str = "delivered";
/// Delivery result value recorded on an approval row: the webhook did not.
pub const DELIVERY_FAILED: &str = "failed";

/// Maps buffered approval events to the packages they cover, for the delivery
/// recorder.
fn evt_packages(evts: &[ApprovalEvt]) -> Vec<DeliveredPackage> {
    evts.iter()
        .map(|e| DeliveredPackage {
            repo: e.repo.clone(),
            package: e.pkg.clone(),
        })
        .collect()
}

/// Renders an advisory id as a Slack/Mattermost mrkdwn link to its public
/// record: CVE ids to cve.org, GHSA ids to GitHub advisories, anything else to
/// osv.dev.
fn cve_link(id: &str) -> String {
    let url = if id.starts_with("CVE-") {
        format!("https://www.cve.org/CVERecord?id={id}")
    } else if id.starts_with("GHSA-") {
        format!("https://github.com/advisories/{id}")
    } else {
        format!("https://osv.dev/vulnerability/{id}")
    };
    format!("<{url}|{id}>")
}

/// Accumulates the approval events buffered for one receiver URL.
struct TargetBatch {
    name: String,
    evts: Vec<ApprovalEvt>,
}

/// One quarantined-package event awaiting (possibly grouped) delivery.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ApprovalEvt {
    pub(crate) repo: String,
    pub(crate) repo_id: i64,
    pub(crate) repo_format: String,
    pub(crate) pkg: String,
    pub(crate) version: String,
    pub(crate) requested_by: String,
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

impl Notifier {
    /// Returns a Notifier; `timeout` bounds each delivery attempt (5s when
    /// zero or negative).
    pub fn new(timeout: Duration) -> Self {
        let timeout = if timeout.is_zero() {
            Duration::from_secs(5)
        } else {
            timeout
        };
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_default();
        Notifier {
            client,
            timeout,
            settings: RwLock::new(Settings::default()),
            state: Mutex::new(BatchState::default()),
        }
    }

    /// Pins forklift's externally-visible base URL so alarm text can turn the
    /// "Forklift" keyword into a clickable link. Wired from main.
    pub fn set_external_url(&self, u: &str) {
        self.settings.write().external_url = u.trim_end_matches('/').to_string();
    }

    /// Enables grouping: approval alarms buffered within `d` are delivered as
    /// one message per receiver. Zero (the default) sends immediately.
    pub fn set_batch_window(&self, d: Duration) {
        self.settings.write().batch_window = d;
    }

    /// Injects the vulnerability-scan lookup used to split a grouped alarm's
    /// packages into Clean vs Dirty counts and surface the top CVE.
    pub fn set_clean_checker(&self, f: CleanChecker) {
        self.settings.write().clean_checker = Some(f);
    }

    /// Injects the callback used to persist approval-alarm delivery outcomes
    /// (send result and elapsed time) onto the approval rows.
    pub fn set_delivery_recorder(&self, f: DeliveryRecorder) {
        self.settings.write().delivery_recorder = Some(f);
    }

    fn external_url(&self) -> String {
        self.settings.read().external_url.clone()
    }

    /// Renders the product name as a Slack/Mattermost mrkdwn link to the
    /// console when an external URL is configured, falling back to plain text.
    fn forklift(&self) -> String {
        let ext = self.external_url();
        if ext.is_empty() {
            return "Forklift".to_string();
        }
        format!("<{ext}|Forklift>")
    }

    /// Deep-links a repository name to its console approvals page when an
    /// external URL and id are known, falling back to plain text.
    fn repo_link(&self, name: &str, id: i64) -> String {
        let ext = self.external_url();
        if ext.is_empty() || id == 0 {
            return name.to_string();
        }
        format!("<{ext}/workspace/repositories/{id}/approvals|{name}>")
    }

    /// Renders a repository for an alarm: its deep-link plus its format in
    /// parentheses (e.g. "npmjs (npm)").
    fn repo_field(&self, name: &str, id: i64, format: &str) -> String {
        let mut s = self.repo_link(name, id);
        if !format.is_empty() {
            s.push_str(" (");
            s.push_str(format);
            s.push(')');
        }
        s
    }
}

/// One labelled line in an alarm body.
struct AlarmField {
    label: &'static str,
    value: String,
}

impl AlarmField {
    fn new(label: &'static str, value: impl Into<String>) -> Self {
        AlarmField {
            label,
            value: value.into(),
        }
    }
}

/// Assembles a Slack/Mattermost mrkdwn alarm from a bold title, an optional
/// subtitle line, and labelled fields. Templates declare their structure as
/// data (title + subtitle + fields) instead of hardcoding markup or line
/// breaks, so adding or reordering a field is a one-line change and every
/// alarm renders consistently.
fn render_alarm(title: &str, subtitle: &str, fields: &[AlarmField]) -> String {
    let mut lines = Vec::with_capacity(fields.len() + 2);
    lines.push(format!("*{title}*"));
    if !subtitle.is_empty() {
        lines.push(subtitle.to_string());
    }
    for f in fields {
        lines.push(format!("*{}*: {}", f.label, f.value));
    }
    lines.join("\n")
}

/// The body posted for a package-approval alarm. `text` is a human-readable
/// summary accepted as-is by Slack/Mattermost incoming webhooks; the structured
/// fields let any other consumer route on the raw values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApprovalPayload {
    pub text: String,
    pub event: String,
    pub repository: String,
    pub package: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub requested_by: String,
    pub timestamp: String,
    /// `count` and `packages` are set only on a grouped alarm (more than one
    /// package quarantined within the batch window); `packages` lists
    /// "repo/pkg@version".
    #[serde(skip_serializing_if = "is_zero")]
    pub count: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<String>,
    /// The distinct set of usernames behind a grouped alarm ("anonymous" for
    /// unauthenticated requests).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub requesters: Vec<String>,
    /// `clean_count`/`dirty_count` split a grouped alarm's packages into those
    /// with a Clean scan (no known advisories) and the rest (vulnerable or
    /// unscanned). `max_severity`/`max_score` are the highest CVE severity and
    /// its CVSS score across the dirty packages (empty when none are
    /// vulnerable).
    #[serde(skip_serializing_if = "is_zero")]
    pub clean_count: i64,
    #[serde(skip_serializing_if = "is_zero")]
    pub dirty_count: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub max_severity: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub max_score: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub top_cve: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub top_package: String,
}

impl Notifier {
    /// Reports a package newly quarantined pending approval. With a batch window
    /// configured it buffers the event per receiver and delivers a single
    /// grouped message when the window elapses (so an install burst is not one
    /// webhook per package); otherwise it delivers immediately. Returns at
    /// once; deliveries run on spawned tasks.
    #[allow(clippy::too_many_arguments)]
    pub fn notify_approval_request(
        self: &Arc<Self>,
        targets: &[Target],
        repo: &str,
        repo_id: i64,
        repo_format: &str,
        pkg: &str,
        version: &str,
        requested_by: &str,
    ) {
        if targets.is_empty() {
            return;
        }
        let evt = ApprovalEvt {
            repo: repo.to_string(),
            repo_id,
            repo_format: repo_format.to_string(),
            pkg: pkg.to_string(),
            version: version.to_string(),
            requested_by: requested_by.to_string(),
        };
        if !self.settings.read().batch_window.is_zero() {
            self.enqueue(targets, evt);
            return;
        }
        let payload = self.build_approval_single(&evt);
        let body = match serde_json::to_vec(&payload) {
            Ok(b) => Bytes::from(b),
            Err(e) => {
                tracing::error!(err = %e, "notify: marshal approval payload failed");
                return;
            }
        };
        let pkgs = vec![DeliveredPackage {
            repo: evt.repo.clone(),
            package: evt.pkg.clone(),
        }];
        for t in targets {
            if t.url.is_empty() {
                continue;
            }
            tokio::spawn(Arc::clone(self).post(t.clone(), body.clone(), pkgs.clone()));
        }
    }

    /// Renders the alarm for exactly one quarantined package.
    pub(crate) fn build_approval_single(&self, e: &ApprovalEvt) -> ApprovalPayload {
        let who = if e.requested_by.is_empty() {
            "anonymous"
        } else {
            &e.requested_by
        };
        let coord = if e.version.is_empty() {
            e.pkg.clone()
        } else {
            format!("{}@{}", e.pkg, e.version)
        };
        let text = render_alarm(
            "Package pending approval",
            &format!(
                "A package is quarantined and awaiting review. Review in {}.",
                self.forklift()
            ),
            &[
                AlarmField::new(
                    "Repository",
                    self.repo_field(&e.repo, e.repo_id, &e.repo_format),
                ),
                AlarmField::new("Pending package", coord),
                AlarmField::new("Requested by", who),
            ],
        );
        ApprovalPayload {
            text,
            event: "approval.request".to_string(),
            repository: e.repo.clone(),
            package: e.pkg.clone(),
            version: e.version.clone(),
            requested_by: e.requested_by.clone(),
            timestamp: timestamp(),
            ..Default::default()
        }
    }

    /// Delivers a test alarm to a webhook URL synchronously and reports the
    /// outcome, so the "send test" button can give the admin immediate
    /// feedback. A transport error or a non-2xx response is returned as an
    /// error.
    pub async fn send_test(&self, name: &str, url: &str) -> Result<()> {
        // Resemble the real approval alarm (same shape and 📦 line) so an operator
        // sees exactly what production looks like, with an explicit TEST marker and
        // event "test" so it is never mistaken for a real request.
        let payload = ApprovalPayload {
            text: render_alarm(
                "[TEST] Package pending approval",
                &format!(
                    "Test alarm for receiver {name:?}. No action needed. Review in {}.",
                    self.forklift()
                ),
                &[
                    AlarmField::new("Repository", "example-proxy"),
                    AlarmField::new("Pending package", "com.example:sample@1.0.0"),
                    AlarmField::new("Requested by", "tester"),
                ],
            ),
            event: "test".to_string(),
            repository: "example-proxy".to_string(),
            package: "com.example:sample".to_string(),
            version: "1.0.0".to_string(),
            requested_by: "tester".to_string(),
            timestamp: timestamp(),
            ..Default::default()
        };
        let body = serde_json::to_vec(&payload)?;
        self.send_json(url, body).await
    }

    /// Returns a representative approval alarm for a repository, used to
    /// preview the message and to deliver a manual sample. It carries a clearly
    /// marked placeholder package so a delivered sample is not mistaken for a
    /// real pending approval.
    pub fn build_approval_sample(
        &self,
        repo: &str,
        repo_id: i64,
        requested_by: &str,
    ) -> ApprovalPayload {
        let who = if requested_by.is_empty() {
            "anonymous"
        } else {
            requested_by
        };
        // Mirror the real approval alarm exactly (same title, fields and Review
        // link), adding only an explicit TEST marker so a delivered sample reads like
        // production yet can never be mistaken for a real request.
        ApprovalPayload {
            text: render_alarm(
                "[TEST] Package pending approval",
                &format!(
                    "Sample alarm. No action needed. Review in {}.",
                    self.forklift()
                ),
                &[
                    AlarmField::new("Repository", self.repo_link(repo, repo_id)),
                    AlarmField::new("Pending package", "com.example:sample@1.0.0"),
                    AlarmField::new("Requested by", who),
                ],
            ),
            event: "approval.request".to_string(),
            repository: repo.to_string(),
            package: "com.example:sample".to_string(),
            version: "1.0.0".to_string(),
            requested_by: requested_by.to_string(),
            timestamp: timestamp(),
            ..Default::default()
        }
    }

    /// Delivers a prepared approval payload to a webhook URL synchronously and
    /// reports the outcome. Used for the manual sample send so each receiver's
    /// result can be reported.
    pub async fn send_approval_payload(&self, url: &str, p: &ApprovalPayload) -> Result<()> {
        let body = serde_json::to_vec(p)?;
        self.send_json(url, body).await
    }

    /// POSTs a JSON body and maps a transport failure or a non-2xx status to an
    /// error; shared by the synchronous senders.
    pub(crate) async fn send_json(&self, url: &str, body: Vec<u8>) -> Result<()> {
        let resp = self
            .client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let _ = resp.bytes().await;
        if status >= 300 {
            return Err(Error::Status(status));
        }
        Ok(())
    }

    /// Delivers one payload to one target, logging any failure. It runs detached
    /// from the triggering request, so it uses its own bounded timeout.
    async fn post(self: Arc<Self>, t: Target, body: Bytes, pkgs: Vec<DeliveredPackage>) {
        let start = Instant::now();
        let (ok, detail) = self.deliver(&t, body).await;
        let recorder = self.settings.read().delivery_recorder.clone();
        if let Some(rec) = recorder
            && !pkgs.is_empty()
        {
            let result = if ok {
                DELIVERY_DELIVERED
            } else {
                DELIVERY_FAILED
            };
            rec(pkgs, result, &detail, start.elapsed().as_millis() as i64);
        }
    }

    /// Performs the actual webhook POST and reports whether it succeeded
    /// (transport ok and a 2xx response) plus a concrete, human-readable
    /// detail: the HTTP status on a reachable webhook ("HTTP 200", "HTTP 500")
    /// or a short reason when it could not be reached. Failures are logged.
    async fn deliver(&self, t: &Target, body: Bytes) -> (bool, String) {
        let send = self
            .client
            .post(&t.url)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send();
        let resp = match tokio::time::timeout(self.timeout + Duration::from_secs(1), send).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) if e.is_builder() => {
                tracing::error!(receiver = %t.name, err = %e, "notify: build request failed");
                return (false, "could not build request".to_string());
            }
            Ok(Err(e)) => {
                tracing::warn!(receiver = %t.name, err = %e, "notify: webhook delivery failed");
                return (false, "no response from webhook".to_string());
            }
            Err(e) => {
                tracing::warn!(receiver = %t.name, err = %e, "notify: webhook delivery failed");
                return (false, "no response from webhook".to_string());
            }
        };
        let status = resp.status().as_u16();
        let _ = resp.bytes().await;
        let detail = format!("HTTP {status}");
        if status >= 300 {
            tracing::warn!(receiver = %t.name, status, "notify: webhook non-2xx response");
            return (false, detail);
        }
        (true, detail)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    pub(crate) mod webhook {
        use std::sync::Arc;
        use std::time::Duration;

        use tokio::sync::mpsc;
        use wiremock::matchers::any;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use crate::notify::*;

        pub(crate) fn install_crypto() {
            rustls::crypto::ring::default_provider()
                .install_default()
                .ok();
        }

        /// A webhook that answers every POST with `status`.
        pub(crate) async fn webhook(status: u16) -> MockServer {
            let srv = MockServer::start().await;
            Mock::given(any())
                .respond_with(ResponseTemplate::new(status))
                .mount(&srv)
                .await;
            srv
        }

        /// Waits briefly for `want` requests to land on `srv`, since delivery is
        /// asynchronous, and returns whatever arrived.
        pub(crate) async fn await_requests(
            srv: &MockServer,
            want: usize,
        ) -> Vec<wiremock::Request> {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            loop {
                let got = srv.received_requests().await.unwrap_or_default();
                if got.len() >= want || tokio::time::Instant::now() >= deadline {
                    return got;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        #[test]
        fn new_defaults_timeout() {
            install_crypto();
            let n = Notifier::new(Duration::ZERO);
            assert_eq!(
                n.timeout,
                Duration::from_secs(5),
                "default timeout, want 5s"
            );
            let n = Notifier::new(Duration::from_secs(2));
            assert_eq!(n.timeout, Duration::from_secs(2), "custom timeout, want 2s");
        }

        /// Verifies the delivery recorder fires after an approval alarm is sent,
        /// reporting the covered packages and the outcome (delivered on 2xx, failed on
        /// 5xx): the data the review page shows.
        #[tokio::test]
        async fn delivery_recorder_records_outcome() {
            install_crypto();
            let ok = webhook(200).await;
            let bad = webhook(500).await;

            // batch window 0: immediate delivery
            let n = Arc::new(Notifier::new(Duration::from_secs(1)));
            let (tx, mut rx) = mpsc::unbounded_channel::<(Vec<DeliveredPackage>, String, String)>();
            n.set_delivery_recorder(Arc::new(move |pkgs, result, detail, _ms| {
                let _ = tx.send((pkgs, result.to_string(), detail.to_string()));
            }));

            n.notify_approval_request(
                &[Target {
                    name: "slack".into(),
                    url: ok.uri(),
                }],
                "npm-proxy",
                1,
                "npm",
                "left-pad",
                "1.3.0",
                "alice",
            );
            let (pkgs, result, detail) = tokio::time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .expect("recorder not invoked for delivered alarm")
                .unwrap();
            assert_eq!(result, DELIVERY_DELIVERED);
            assert_eq!(detail, "HTTP 200");
            assert_eq!(
                pkgs,
                vec![DeliveredPackage {
                    repo: "npm-proxy".into(),
                    package: "left-pad".into()
                }]
            );

            n.notify_approval_request(
                &[Target {
                    name: "slack".into(),
                    url: bad.uri(),
                }],
                "npm-proxy",
                1,
                "npm",
                "evil",
                "1.0.0",
                "bob",
            );
            let (_, result, detail) = tokio::time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .expect("recorder not invoked for failed alarm")
                .unwrap();
            assert_eq!(result, DELIVERY_FAILED);
            assert_eq!(detail, "HTTP 500");
        }

        #[tokio::test]
        async fn send_test() {
            install_crypto();
            let srv = webhook(200).await;
            let n = Notifier::new(Duration::from_secs(1));
            n.send_test("slack", &srv.uri())
                .await
                .expect("send_test ok case");
            let reqs = srv.received_requests().await.unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(
                reqs[0]
                    .headers
                    .get("content-type")
                    .map(|v| v.to_str().unwrap()),
                Some("application/json"),
                "expected JSON content-type on delivered request"
            );
            let p: ApprovalPayload = serde_json::from_slice(&reqs[0].body).unwrap();
            assert_eq!(p.event, "test");
            assert!(
                p.text.contains("Test alarm for receiver \"slack\"."),
                "{}",
                p.text
            );

            // non-2xx -> error
            let bad = webhook(500).await;
            let err = n
                .send_test("slack", &bad.uri())
                .await
                .expect_err("send_test non-2xx: expected error");
            assert_eq!(err.to_string(), "webhook returned status 500");

            // bad URL -> transport error
            assert!(
                n.send_test("slack", "http://127.0.0.1:0").await.is_err(),
                "send_test bad url: expected error"
            );
        }

        #[test]
        fn build_approval_sample() {
            install_crypto();
            let n = Notifier::new(Duration::from_secs(1));
            let p = n.build_approval_sample("npm-proxy", 0, "");
            assert!(
                p.event == "approval.request"
                    && p.repository == "npm-proxy"
                    && p.package == "com.example:sample",
                "unexpected sample: {p:?}"
            );
            assert!(
                p.version == "1.0.0" && !p.timestamp.is_empty(),
                "sample missing fields: {p:?}"
            );
            // requested_by carried through when provided.
            assert_eq!(
                n.build_approval_sample("r", 0, "alice").requested_by,
                "alice"
            );
        }

        #[tokio::test]
        async fn send_approval_payload() {
            install_crypto();
            let n = Notifier::new(Duration::from_secs(1));
            let p = n.build_approval_sample("repo", 0, "bob");

            let ok = webhook(204).await;
            n.send_approval_payload(&ok.uri(), &p)
                .await
                .expect("send_approval_payload ok");
            let reqs = ok.received_requests().await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
            assert_eq!(body["event"], "approval.request");
            assert_eq!(body["requested_by"], "bob");
            for absent in ["count", "packages", "requesters", "clean_count", "top_cve"] {
                assert!(
                    body.get(absent).is_none(),
                    "{absent} should be omitted: {body}"
                );
            }

            let bad = webhook(502).await;
            assert!(
                n.send_approval_payload(&bad.uri(), &p).await.is_err(),
                "send_approval_payload non-2xx: expected error"
            );
        }

        #[tokio::test]
        async fn notify_approval_request() {
            install_crypto();
            let srv = webhook(200).await;
            let n = Arc::new(Notifier::new(Duration::from_secs(1)));
            // Two real targets plus one with an empty URL (skipped) and noise.
            let targets = vec![
                Target {
                    name: "a".into(),
                    url: srv.uri(),
                },
                Target {
                    name: "b".into(),
                    url: srv.uri(),
                },
                Target {
                    name: "empty".into(),
                    url: String::new(),
                },
            ];
            n.notify_approval_request(&targets, "npm-proxy", 0, "npm", "left-pad", "1.0.0", "");

            // Wait for the two async deliveries.
            let reqs = await_requests(&srv, 2).await;
            assert_eq!(
                reqs.len(),
                2,
                "delivered {} times, want 2 (empty URL skipped)",
                reqs.len()
            );
            let p: ApprovalPayload = serde_json::from_slice(&reqs[0].body).unwrap();
            assert_eq!(p.event, "approval.request");
            assert_eq!(p.package, "left-pad");
            assert!(p.text.contains("*Requested by*: anonymous"), "{}", p.text);

            // Empty targets are a no-op (must not panic).
            n.notify_approval_request(&[], "r", 0, "npm", "p", "", "");
        }

        #[test]
        fn approval_payload_json_shape() {
            install_crypto();
            let n = Notifier::new(Duration::ZERO);
            let p = n.build_approval_single(&ApprovalEvt {
                repo: "npmjs".into(),
                repo_id: 1,
                repo_format: "npm".into(),
                pkg: "lodash".into(),
                version: String::new(),
                requested_by: String::new(),
            });
            let v: serde_json::Value = serde_json::to_value(&p).unwrap();
            let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
            assert_eq!(
                keys,
                ["text", "event", "repository", "package", "timestamp"]
            );
            assert_eq!(
                p.text,
                "*Package pending approval*\nA package is quarantined and awaiting review. Review in Forklift.\n*Repository*: npmjs (npm)\n*Pending package*: lodash\n*Requested by*: anonymous"
            );
            assert_eq!(
                cve_link("GHSA-x"),
                "<https://github.com/advisories/GHSA-x|GHSA-x>"
            );
            assert_eq!(
                cve_link("PYSEC-1"),
                "<https://osv.dev/vulnerability/PYSEC-1|PYSEC-1>"
            );
        }
    }
}
