//! The A2A exchange with the kagent controller.
//!
//! The analysis is submitted as a non-blocking `message/send` and its task is
//! then polled with `tasks/get` until it reaches a terminal state. Polling
//! keeps every HTTP request short, which no load balancer idle timeout can
//! kill, and it leaves a task ID behind that a deadline can cancel with
//! `tasks/cancel` instead of abandoning the run to keep burning tokens.
//! Streaming would let the gateway post partial output, but the reply is one
//! Slack message, so the poll loop reports task state through
//! [`Request::on_progress`] instead, which is enough to keep a status line
//! current without a second transport.

use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use async_trait::async_trait;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::time::{Instant, sleep, sleep_until, timeout_at};
use tracing::{debug, error, info, warn};

use super::Error;
use super::error::fail;
use crate::observability::Metrics;

/// Bounds how many `tasks/get` calls may fail in a row before the run is
/// abandoned. Combined with the poll interval this tolerates roughly half a
/// minute of controller unavailability, which covers a restart.
const MAX_CONSECUTIVE_POLL_FAILURES: u32 = 6;

/// Caps how much of one controller reply is read. A larger body is reported
/// as its own error rather than handed to the decoder, where a cut-off reply
/// is indistinguishable from a malformed one.
const READ_LIMIT: usize = 4 << 20;

/// One call to an agent.
#[derive(Clone, Default)]
pub struct Request {
    pub agent: String,
    pub text: String,
    /// Continues an existing session. `None` starts a new one, which is what
    /// the alert path always does: an alert is not a conversation.
    pub context_id: Option<String>,
    /// Called as the task moves, so a caller can report the run while it is
    /// still running. It runs on the polling task, so an implementation must
    /// not block: the mention path only records the state and lets its own
    /// ticker do the Slack call.
    pub on_progress: Option<ProgressHook>,
}

/// A progress observer; see [`Request::on_progress`].
pub type ProgressHook = Arc<dyn Fn(Progress) + Send + Sync>;

/// One observation of a running task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub task_id: String,
    /// The task state the controller last reported, e.g. `submitted`,
    /// `working`, or `completed`.
    pub state: String,
    /// How many `tasks/get` reads have been spent so far.
    pub polls: u32,
}

/// The outcome of one analysis run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reply {
    pub text: String,
    pub task_id: String,
    pub context_id: String,
}

/// Runs one request on an agent and returns its reply.
#[async_trait]
pub trait AgentClient: Send + Sync {
    /// Submits `req` and waits for the reply, giving up at `deadline`.
    async fn send(&self, req: Request, deadline: Instant) -> Result<Reply, Error>;
}

/// Talks to the kagent controller over A2A. One client serves every agent in
/// a namespace: the controller exposes each agent under its own path, so the
/// agent is chosen per call rather than per connection.
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    namespace: String,
    user_id: String,
    metrics: Arc<Metrics>,
    poll_interval: Duration,
}

#[derive(Serialize)]
struct RpcRequest<'a, P> {
    jsonrpc: &'static str,
    id: String,
    method: &'a str,
    params: P,
}

#[derive(Serialize)]
struct SendParams {
    message: OutgoingMessage,
    configuration: RequestConfiguration,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OutgoingMessage {
    kind: &'static str,
    role: &'static str,
    message_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_id: Option<String>,
    parts: Vec<Part>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskParams<'a> {
    id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    history_length: Option<u32>,
}

#[derive(Serialize)]
struct RequestConfiguration {
    blocking: bool,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(default)]
struct Part {
    kind: String,
    text: String,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default)]
struct Response {
    error: Option<RpcError>,
    result: Option<RpcResult>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, rename_all = "camelCase")]
struct RpcResult {
    /// Distinguishes a Task result from a direct Message result, which the
    /// spec allows a server to return for an instant reply.
    kind: String,
    id: String,
    context_id: String,
    /// Only set on a message-kind result.
    parts: Vec<Part>,
    artifacts: Vec<Artifact>,
    history: Vec<HistoryEntry>,
    status: Status,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default)]
struct Artifact {
    parts: Vec<Part>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default)]
struct HistoryEntry {
    role: String,
    parts: Vec<Part>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default)]
struct Status {
    state: String,
    message: Option<StatusMessage>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default)]
struct StatusMessage {
    parts: Vec<Part>,
}

#[derive(Deserialize, Debug)]
struct RpcError {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

impl Client {
    /// Returns a client for the controller at `base_url`, e.g.
    /// `http://kagent-controller.kagent:8083`, serving agents in `namespace`.
    /// `request_timeout` bounds one HTTP call (submit, poll, or cancel), not
    /// the whole analysis; the analysis deadline is given to
    /// [`AgentClient::send`].
    ///
    /// # Panics
    ///
    /// Panics when the HTTP client cannot be built, which only happens when
    /// the TLS backend is unusable.
    #[must_use]
    pub fn new(
        base_url: &str,
        namespace: &str,
        user_id: &str,
        request_timeout: Duration,
        poll_interval: Duration,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(request_timeout)
                .build()
                .expect("build http client"),
            base_url: base_url.trim_end_matches('/').to_string(),
            namespace: namespace.to_string(),
            user_id: user_id.to_string(),
            metrics,
            poll_interval,
        }
    }

    /// Returns the same client submitting as a different session owner.
    /// `reqwest::Client` is a handle onto a shared connection pool, so the
    /// clone opens no new connections; only the `X-User-Id` differs.
    #[must_use]
    pub fn with_user_id(&self, user_id: &str) -> Self {
        Self {
            http: self.http.clone(),
            base_url: self.base_url.clone(),
            namespace: self.namespace.clone(),
            user_id: user_id.to_string(),
            metrics: Arc::clone(&self.metrics),
            poll_interval: self.poll_interval,
        }
    }

    /// The A2A JSON-RPC endpoint one agent is served on.
    #[must_use]
    pub fn endpoint(&self, agent: &str) -> String {
        format!("{}/api/a2a/{}/{}", self.base_url, self.namespace, agent)
    }

    /// Reads the task state every poll interval until it turns terminal or the
    /// deadline passes. Transient read failures are tolerated up to
    /// [`MAX_CONSECUTIVE_POLL_FAILURES`] so one dropped poll does not abandon a
    /// paid run.
    async fn poll(
        &self,
        agent: &str,
        task_id: &str,
        submitted: Instant,
        deadline: Instant,
        on_progress: Option<&ProgressHook>,
    ) -> Result<Reply, Error> {
        let (mut polls, mut failures) = (0_u32, 0_u32);
        loop {
            tokio::select! {
                () = sleep_until(deadline) => return self.deadline_hit(agent, task_id, polls, submitted).await,
                () = sleep(self.poll_interval) => {}
            }
            polls += 1;
            let params = TaskParams {
                id: task_id,
                history_length: Some(50),
            };
            let out = match timeout_at(deadline, self.call(agent, "tasks/get", &params)).await {
                Err(_) => return self.deadline_hit(agent, task_id, polls, submitted).await,
                Ok(Err(err)) => {
                    failures += 1;
                    if failures >= MAX_CONSECUTIVE_POLL_FAILURES {
                        self.metrics.observe_agent_task(
                            agent,
                            "unreachable",
                            polls,
                            submitted.elapsed(),
                        );
                        // The retry count and the poll loop itself are the
                        // gateway's own business. What reaches Slack is that
                        // the controller went quiet.
                        return Err(fail(
                            "the controller stopped responding",
                            anyhow!(err).context(format!(
                                "gave up polling task after {failures} consecutive failures"
                            )),
                        ));
                    }
                    warn!(agent, task_id, failures, error = %err, "task poll failed, retrying");
                    continue;
                }
                Ok(Ok(out)) => out,
            };
            failures = 0;
            let state = out.status.state.clone();
            report(
                on_progress,
                Progress {
                    task_id: task_id.to_string(),
                    state: state.clone(),
                    polls,
                },
            );

            if terminal(&state) {
                self.metrics
                    .observe_agent_task(agent, &state, polls, submitted.elapsed());
                return finalize(&out);
            }
            debug!(agent, task_id, state, "task still running");
        }
    }

    /// The deadline path: the task is cancelled so an abandoned run stops
    /// consuming model tokens, and the timeout is recorded as a task state of
    /// its own.
    async fn deadline_hit(
        &self,
        agent: &str,
        task_id: &str,
        polls: u32,
        submitted: Instant,
    ) -> Result<Reply, Error> {
        self.metrics
            .observe_agent_task(agent, "timeout", polls, submitted.elapsed());
        self.cancel_task(agent, task_id).await;
        Err(fail(
            "the analysis ran past its deadline and was cancelled",
            anyhow!("analysis deadline exceeded, task cancelled"),
        ))
    }

    /// Tells the controller to stop a task the gateway no longer waits for.
    /// It runs on the request timeout alone because the caller's deadline is
    /// already gone.
    async fn cancel_task(&self, agent: &str, task_id: &str) {
        let params = TaskParams {
            id: task_id,
            history_length: None,
        };
        match self.call(agent, "tasks/cancel", &params).await {
            Ok(_) => info!(agent, task_id, "cancelled abandoned task"),
            Err(err) => warn!(agent, task_id, error = %err, "failed to cancel task"),
        }
    }

    /// Performs one JSON-RPC request and decodes the envelope, recording the
    /// attempt so a slow or failing controller is visible per method.
    async fn call<P: Serialize + Sync>(
        &self,
        agent: &str,
        method: &str,
        params: &P,
    ) -> Result<RpcResult, Error> {
        let started = Instant::now();
        let out = self.do_call(agent, method, params).await;
        let result = if out.is_ok() { "ok" } else { "error" };
        self.metrics
            .observe_agent_request(method, result, started.elapsed());
        out
    }

    async fn do_call<P: Serialize + Sync>(
        &self,
        agent: &str,
        method: &str,
        params: &P,
    ) -> Result<RpcResult, Error> {
        let body = serde_json::to_vec(&RpcRequest {
            jsonrpc: "2.0",
            id: random_id(),
            method,
            params,
        })
        .map_err(|err| {
            fail(
                "the gateway could not build its request",
                anyhow!(err).context("encode request"),
            )
        })?;

        let resp = self
            .http
            .post(self.endpoint(agent))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            // The controller runs with auth.mode=unsecure, where X-User-Id
            // selects the session owner. A stable ID keeps every analysis
            // under one kagent user, and tasks/get only sees tasks submitted
            // by the same user.
            .header("X-User-Id", &self.user_id)
            .body(body)
            .send()
            .await
            .map_err(|err| {
                fail(
                    "the controller could not be reached",
                    anyhow!(err).context("call agent"),
                )
            })?;

        let status = resp.status();
        let raw = resp.bytes().await.map_err(|err| {
            fail(
                "the controller reply could not be read",
                anyhow!(err).context("read response"),
            )
        })?;
        if raw.len() > READ_LIMIT {
            return Err(fail(
                "the controller reply was too large",
                anyhow!("response exceeds the {READ_LIMIT} byte read limit"),
            ));
        }
        if status != reqwest::StatusCode::OK {
            // The body stays out of the summary. A controller that answers an
            // error status with a page of JSON would otherwise paste all of it
            // to Slack.
            let body = snippet(&String::from_utf8_lossy(&raw));
            error!(
                agent,
                method,
                status = status.as_u16(),
                body,
                "controller returned an error status"
            );
            return Err(fail(
                format!("the controller returned HTTP {}", status.as_u16()),
                anyhow!("agent returned HTTP {}: {body}", status.as_u16()),
            ));
        }

        let out: Response = match serde_json::from_slice(&raw) {
            Ok(out) => out,
            Err(err) => {
                // The offending bytes go to the log. Which byte broke the
                // parse is a question for whoever fixes the controller, not
                // for the thread waiting on an answer.
                error!(
                    agent,
                    method,
                    bytes = raw.len(),
                    near = decode_context(&raw, &err),
                    error = %err,
                    "controller reply is not valid JSON"
                );
                return Err(fail(
                    "the controller reply was not valid JSON",
                    anyhow!(err).context("decode response"),
                ));
            }
        };
        if let Some(rpc) = out.error {
            // The controller's own message names what it rejected, which is
            // the one detail worth carrying through to Slack.
            return Err(fail(
                format!("the agent rejected the request. {}", snippet(&rpc.message)),
                anyhow!("agent error {}: {}", rpc.code, rpc.message),
            ));
        }
        Ok(out.result.unwrap_or_default())
    }
}

#[async_trait]
impl AgentClient for Client {
    /// Submits a request to an agent and polls the resulting task until it
    /// completes, fails, or `deadline` passes. On the deadline the task is
    /// cancelled so an abandoned run stops consuming model tokens.
    async fn send(&self, req: Request, deadline: Instant) -> Result<Reply, Error> {
        let agent = req.agent.as_str();
        let params = SendParams {
            message: OutgoingMessage {
                kind: "message",
                role: "user",
                message_id: random_id(),
                context_id: req.context_id.clone().filter(|id| !id.is_empty()),
                parts: vec![Part {
                    kind: "text".into(),
                    text: req.text.clone(),
                }],
            },
            configuration: RequestConfiguration { blocking: false },
        };
        let out = match timeout_at(deadline, self.call(agent, "message/send", &params)).await {
            Ok(Ok(out)) => out,
            Ok(Err(err)) => {
                return Err(Error::new(
                    err.user_message().to_string(),
                    anyhow!(err).context("submit analysis"),
                ));
            }
            Err(_) => {
                return Err(fail(
                    "the analysis ran past its deadline",
                    anyhow!("analysis deadline exceeded while submitting the request"),
                ));
            }
        };

        // A server may answer a trivial request with a plain message instead
        // of a task, in which case there is nothing to poll.
        if out.kind == "message" {
            let text = join_parts(&out.parts);
            if text.is_empty() {
                return Err(fail(
                    "the agent replied with nothing",
                    anyhow!("agent returned an empty message"),
                ));
            }
            return Ok(Reply {
                text,
                task_id: String::new(),
                context_id: out.context_id,
            });
        }

        let task_id = out.id.clone();
        if task_id.is_empty() {
            return Err(fail(
                "the controller did not start a task",
                anyhow!("agent returned no task id (state {:?})", out.status.state),
            ));
        }
        // Timing the task from the accepted submission separates the
        // controller's own processing time from the queueing and Slack work
        // around it.
        let submitted = Instant::now();
        let state = out.status.state.clone();
        info!(agent, task_id, state, "analysis task submitted");
        report(
            req.on_progress.as_ref(),
            Progress {
                task_id: task_id.clone(),
                state: state.clone(),
                polls: 0,
            },
        );

        if terminal(&state) {
            self.metrics
                .observe_agent_task(agent, &state, 0, submitted.elapsed());
            return finalize(&out);
        }
        self.poll(
            agent,
            &task_id,
            submitted,
            deadline,
            req.on_progress.as_ref(),
        )
        .await
    }
}

/// Calls a progress hook when the caller supplied one.
fn report(hook: Option<&ProgressHook>, p: Progress) {
    if let Some(hook) = hook {
        hook(p);
    }
}

/// Reports whether a task state can still change. `input-required` and
/// `auth-required` cannot progress either: the gateway is a one-shot caller
/// with nobody to answer a follow-up question.
fn terminal(state: &str) -> bool {
    matches!(
        state,
        "completed" | "failed" | "canceled" | "rejected" | "input-required" | "auth-required"
    )
}

/// Turns a terminal task into a [`Reply`] or an error.
fn finalize(out: &RpcResult) -> Result<Reply, Error> {
    let reply = Reply {
        text: answer(out),
        task_id: out.id.clone(),
        context_id: out.context_id.clone(),
    };
    let state = out.status.state.as_str();
    match state {
        // input-required carries the agent's question as its final text,
        // which is still worth posting: it usually names what was missing.
        "completed" | "input-required" => {
            if reply.text.is_empty() {
                return Err(fail(
                    "the agent finished without an answer",
                    anyhow!("agent returned no text (task state {state:?})"),
                ));
            }
            debug!(
                task_id = reply.task_id,
                context_id = reply.context_id,
                state,
                chars = reply.text.chars().count(),
                "agent replied"
            );
            Ok(reply)
        }
        _ if !reply.text.is_empty() => {
            // The agent's own last words say what went wrong better than the
            // state name does, so they are the summary.
            let words = snippet(&reply.text);
            Err(fail(
                format!("the agent stopped in state {state:?}. {words}"),
                anyhow!("task ended in state {state:?}: {words}"),
            ))
        }
        _ => Err(fail(
            format!("the agent stopped in state {state:?}"),
            anyhow!("task ended in state {state:?}"),
        )),
    }
}

/// Extracts the reply text. The controller puts the final answer in
/// artifacts, but a task that ends in an input-required or failed state
/// carries its message under status instead, and older agents only fill
/// history.
fn answer(out: &RpcResult) -> String {
    let from_artifacts: Vec<&Part> = out.artifacts.iter().flat_map(|a| a.parts.iter()).collect();
    let text = join_part_refs(&from_artifacts);
    if !text.is_empty() {
        return text;
    }
    if let Some(msg) = &out.status.message {
        let text = join_parts(&msg.parts);
        if !text.is_empty() {
            return text;
        }
    }
    for entry in out.history.iter().rev().filter(|h| h.role == "agent") {
        let text = join_parts(&entry.parts);
        if !text.is_empty() {
            return text;
        }
    }
    String::new()
}

fn join_parts(parts: &[Part]) -> String {
    join_part_refs(&parts.iter().collect::<Vec<_>>())
}

fn join_part_refs(parts: &[&Part]) -> String {
    parts
        .iter()
        .filter(|p| (p.kind.is_empty() || p.kind == "text") && !p.text.is_empty())
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn random_id() -> String {
    use std::fmt::Write as _;
    let mut buf = [0_u8; 16];
    rand::rng().fill_bytes(&mut buf);
    buf.iter().fold(String::with_capacity(32), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Returns the part of `raw` a decode error points at. A syntax error
/// carries the line and column it failed on, and a single bad byte megabytes
/// into a reply leaves no trace in a snippet of the reply's head.
fn decode_context(raw: &[u8], err: &serde_json::Error) -> String {
    const WINDOW: usize = 100;
    let text = String::from_utf8_lossy(raw);
    if err.line() == 0 {
        return snippet(&text);
    }
    let offset = text
        .split_inclusive('\n')
        .take(err.line() - 1)
        .map(str::len)
        .sum::<usize>()
        .saturating_add(err.column().saturating_sub(1));
    let chars: Vec<char> = text.chars().collect();
    let start = offset.saturating_sub(WINDOW).min(chars.len());
    let end = offset.saturating_add(WINDOW).min(chars.len());
    snippet(&chars[start..end].iter().collect::<String>())
}

/// Trims text to a size a log line or a Slack summary can carry.
fn snippet(raw: &str) -> String {
    const MAX: usize = 200;
    let s = raw.trim();
    if s.chars().count() > MAX {
        return format!("{}...", s.chars().take(MAX).collect::<String>());
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::{Value, json};
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::observability::metrics::MethodResultLabels;

    const TICK: Duration = Duration::from_millis(10);

    fn client(server: &MockServer, metrics: Arc<Metrics>) -> Client {
        Client::new(
            &server.uri(),
            "kagent",
            "gateway@kagent.dev",
            Duration::from_secs(5),
            TICK,
            metrics,
        )
    }

    fn task(state: &str, text: Option<&str>) -> Value {
        let mut result = json!({"kind": "task", "id": "task-1", "contextId": "ctx-1", "status": {"state": state}});
        if let Some(text) = text {
            result["artifacts"] = json!([{"parts": [{"kind": "text", "text": text}]}]);
        }
        json!({"jsonrpc": "2.0", "id": "1", "result": result})
    }

    fn rpc(server: &MockServer, rpc_method: &str) -> wiremock::MockBuilder {
        Mock::given(method("POST"))
            .and(path("/api/a2a/kagent/agent"))
            .and(body_partial_json(json!({"method": rpc_method})))
            .and(header("X-User-Id", "gateway@kagent.dev"))
            .and(header("content-type", "application/json"))
            .and(wiremock::matchers::query_param_is_missing("unused"))
            .and(wiremock::matchers::any())
            .and(matcher_server(server))
    }

    /// wiremock has no "always" matcher that accepts a server reference; this
    /// keeps the helper signature honest without matching on anything.
    fn matcher_server(_: &MockServer) -> wiremock::matchers::AnyMatcher {
        wiremock::matchers::any()
    }

    fn task_count(metrics: &Metrics, state: &str) -> u64 {
        let out = metrics.encode().unwrap();
        let needle = format!(
            "kagent_gateway_agent_task_duration_seconds_count{{agent=\"agent\",state=\"{state}\"}} "
        );
        out.lines()
            .find_map(|l| l.strip_prefix(needle.as_str()))
            .map_or(0, |v| v.trim().parse().unwrap())
    }

    fn request(text: &str) -> Request {
        Request {
            agent: "agent".into(),
            text: text.into(),
            ..Request::default()
        }
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    #[tokio::test]
    async fn with_user_id_changes_the_session_owner() {
        let server = MockServer::start().await;
        let metrics = Arc::new(Metrics::new());
        Mock::given(method("POST"))
            .and(path("/api/a2a/kagent/agent"))
            .and(header("X-User-Id", "chat@kagent.dev"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(task("completed", Some("all good"))),
            )
            .expect(1)
            .mount(&server)
            .await;
        let reply = client(&server, metrics)
            .with_user_id("chat@kagent.dev")
            .send(request("hi"), far())
            .await
            .unwrap();
        assert_eq!(reply.text, "all good");
    }

    #[tokio::test]
    async fn immediate_completion_needs_no_poll() {
        let server = MockServer::start().await;
        let metrics = Arc::new(Metrics::new());
        rpc(&server, "message/send")
            .and(body_partial_json(
                json!({"params": {"configuration": {"blocking": false}}}),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(task("completed", Some("all good"))),
            )
            .expect(1)
            .mount(&server)
            .await;
        let reply = client(&server, metrics.clone())
            .send(request("hi"), far())
            .await
            .unwrap();
        assert_eq!(
            reply,
            Reply {
                text: "all good".into(),
                task_id: "task-1".into(),
                context_id: "ctx-1".into()
            }
        );
        assert_eq!(task_count(&metrics, "completed"), 1);
        let ok = MethodResultLabels {
            method: "message/send".into(),
            result: "ok".into(),
        };
        assert_eq!(Metrics::counter(&metrics.agent_requests, &ok), 1);
    }

    #[tokio::test]
    async fn polls_until_completed_and_reports_progress() {
        let server = MockServer::start().await;
        rpc(&server, "message/send")
            .respond_with(ResponseTemplate::new(200).set_body_json(task("submitted", None)))
            .mount(&server)
            .await;
        rpc(&server, "tasks/get")
            .and(body_partial_json(
                json!({"params": {"id": "task-1", "historyLength": 50}}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(task("working", None)))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        rpc(&server, "tasks/get")
            .respond_with(ResponseTemplate::new(200).set_body_json(task("completed", Some("done"))))
            .mount(&server)
            .await;

        let seen = Arc::new(Mutex::new(Vec::new()));
        let hook: ProgressHook = {
            let seen = seen.clone();
            Arc::new(move |p: Progress| seen.lock().unwrap().push(p))
        };
        let req = Request {
            on_progress: Some(hook),
            ..request("q")
        };
        let reply = client(&server, Arc::new(Metrics::new()))
            .send(req, far())
            .await
            .unwrap();
        assert_eq!(reply.text, "done");
        let states: Vec<(String, u32)> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|p| (p.state.clone(), p.polls))
            .collect();
        assert_eq!(
            states,
            vec![
                ("submitted".into(), 0),
                ("working".into(), 1),
                ("working".into(), 2),
                ("completed".into(), 3)
            ]
        );
    }

    #[tokio::test]
    async fn tolerates_transient_poll_failures() {
        let server = MockServer::start().await;
        rpc(&server, "message/send")
            .respond_with(ResponseTemplate::new(200).set_body_json(task("submitted", None)))
            .mount(&server)
            .await;
        rpc(&server, "tasks/get")
            .respond_with(ResponseTemplate::new(502))
            .up_to_n_times(3)
            .mount(&server)
            .await;
        rpc(&server, "tasks/get")
            .respond_with(ResponseTemplate::new(200).set_body_json(task("completed", Some("late"))))
            .mount(&server)
            .await;
        let reply = client(&server, Arc::new(Metrics::new()))
            .send(request("q"), far())
            .await
            .unwrap();
        assert_eq!(reply.text, "late");
    }

    #[tokio::test]
    async fn gives_up_after_consecutive_poll_failures() {
        let server = MockServer::start().await;
        let metrics = Arc::new(Metrics::new());
        rpc(&server, "message/send")
            .respond_with(ResponseTemplate::new(200).set_body_json(task("submitted", None)))
            .mount(&server)
            .await;
        rpc(&server, "tasks/get")
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let err = client(&server, metrics.clone())
            .send(request("q"), far())
            .await
            .unwrap_err();
        assert_eq!(err.user_message(), "the controller stopped responding");
        assert!(
            err.to_string()
                .contains("gave up polling task after 6 consecutive failures")
        );
        assert_eq!(task_count(&metrics, "unreachable"), 1);
    }

    #[tokio::test]
    async fn cancels_the_task_on_the_deadline() {
        let server = MockServer::start().await;
        let metrics = Arc::new(Metrics::new());
        rpc(&server, "message/send")
            .respond_with(ResponseTemplate::new(200).set_body_json(task("submitted", None)))
            .mount(&server)
            .await;
        rpc(&server, "tasks/get")
            .respond_with(ResponseTemplate::new(200).set_body_json(task("working", None)))
            .mount(&server)
            .await;
        rpc(&server, "tasks/cancel")
            .and(body_partial_json(json!({"params": {"id": "task-1"}})))
            .respond_with(ResponseTemplate::new(200).set_body_json(task("canceled", None)))
            .expect(1)
            .mount(&server)
            .await;
        let deadline = Instant::now() + Duration::from_millis(60);
        let err = client(&server, metrics.clone())
            .send(request("q"), deadline)
            .await
            .unwrap_err();
        assert_eq!(
            err.user_message(),
            "the analysis ran past its deadline and was cancelled"
        );
        assert_eq!(task_count(&metrics, "timeout"), 1);
    }

    #[tokio::test]
    async fn deadline_during_submit_is_reported() {
        let server = MockServer::start().await;
        rpc(&server, "message/send")
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(2))
                    .set_body_json(task("completed", Some("x"))),
            )
            .mount(&server)
            .await;
        let deadline = Instant::now() + Duration::from_millis(30);
        let err = client(&server, Arc::new(Metrics::new()))
            .send(request("q"), deadline)
            .await
            .unwrap_err();
        assert_eq!(err.user_message(), "the analysis ran past its deadline");
    }

    #[tokio::test]
    async fn accepts_a_direct_message_result() {
        let server = MockServer::start().await;
        rpc(&server, "message/send")
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": {
                "kind": "message", "contextId": "ctx-9",
                "parts": [{"kind": "text", "text": "first"}, {"kind": "data", "text": "skip"}, {"text": "second"}]
            }})))
            .mount(&server)
            .await;
        let reply = client(&server, Arc::new(Metrics::new()))
            .send(request("q"), far())
            .await
            .unwrap();
        assert_eq!(reply.text, "first\nsecond");
        assert_eq!(reply.context_id, "ctx-9");
        assert_eq!(reply.task_id, "");
    }

    #[tokio::test]
    async fn empty_direct_message_is_an_error() {
        let server = MockServer::start().await;
        rpc(&server, "message/send")
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"result": {"kind": "message", "parts": []}})),
            )
            .mount(&server)
            .await;
        let err = client(&server, Arc::new(Metrics::new()))
            .send(request("q"), far())
            .await
            .unwrap_err();
        assert_eq!(err.user_message(), "the agent replied with nothing");
    }

    #[tokio::test]
    async fn falls_back_to_status_and_history() {
        let server = MockServer::start().await;
        rpc(&server, "message/send")
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": {
                "kind": "task", "id": "t", "status": {"state": "input-required",
                    "message": {"parts": [{"kind": "text", "text": "which cluster?"}]}}
            }})))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        let reply = client(&server, Arc::new(Metrics::new()))
            .send(request("q"), far())
            .await
            .unwrap();
        assert_eq!(reply.text, "which cluster?");

        let server = MockServer::start().await;
        rpc(&server, "message/send")
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": {
                "kind": "task", "id": "t", "status": {"state": "completed"},
                "history": [
                    {"role": "user", "parts": [{"kind": "text", "text": "question"}]},
                    {"role": "agent", "parts": [{"kind": "text", "text": "older"}]},
                    {"role": "agent", "parts": [{"kind": "text", "text": "newest"}]}
                ]
            }})))
            .mount(&server)
            .await;
        let reply = client(&server, Arc::new(Metrics::new()))
            .send(request("q"), far())
            .await
            .unwrap();
        assert_eq!(reply.text, "newest");
    }

    #[tokio::test]
    async fn submit_errors_carry_summaries() {
        let cases: Vec<(ResponseTemplate, &str, &str)> = vec![
            (
                ResponseTemplate::new(200).set_body_json(json!({"error": {"code": -32600, "message": "bad agent"}})),
                "the agent rejected the request. bad agent",
                "agent error -32600: bad agent",
            ),
            (
                ResponseTemplate::new(503).set_body_string("<html>down</html>"),
                "the controller returned HTTP 503",
                "agent returned HTTP 503: <html>down</html>",
            ),
            (
                ResponseTemplate::new(200).set_body_string("{not json"),
                "the controller reply was not valid JSON",
                "decode response",
            ),
            (
                ResponseTemplate::new(200).set_body_json(json!({"result": {"kind": "task", "status": {"state": "submitted"}}})),
                "the controller did not start a task",
                "agent returned no task id",
            ),
            (
                ResponseTemplate::new(200).set_body_json(json!({"result": {"kind": "task", "id": "t", "status": {"state": "completed"}}})),
                "the agent finished without an answer",
                "agent returned no text",
            ),
            (
                ResponseTemplate::new(200).set_body_json(json!({"result": {"kind": "task", "id": "t", "status": {"state": "failed",
                    "message": {"parts": [{"kind": "text", "text": "tool exploded"}]}}}})),
                "the agent stopped in state \"failed\". tool exploded",
                "task ended in state \"failed\": tool exploded",
            ),
            (
                ResponseTemplate::new(200).set_body_json(json!({"result": {"kind": "task", "id": "t", "status": {"state": "rejected"}}})),
                "the agent stopped in state \"rejected\"",
                "task ended in state \"rejected\"",
            ),
        ];
        for (template, summary, detail) in cases {
            let server = MockServer::start().await;
            rpc(&server, "message/send")
                .respond_with(template)
                .mount(&server)
                .await;
            let err = client(&server, Arc::new(Metrics::new()))
                .send(request("q"), far())
                .await
                .unwrap_err();
            assert_eq!(err.user_message(), summary);
            assert!(err.to_string().contains(detail), "{summary}: {err}");
        }
    }

    #[tokio::test]
    async fn unreachable_controller_is_reported() {
        let uri = "http://127.0.0.1:1".to_string();
        let c = Client::new(
            &uri,
            "kagent",
            "u",
            Duration::from_secs(1),
            TICK,
            Arc::new(Metrics::new()),
        );
        let err = c.send(request("q"), far()).await.unwrap_err();
        assert_eq!(err.user_message(), "the controller could not be reached");
        assert!(err.to_string().starts_with("submit analysis: call agent"));
    }

    #[tokio::test]
    async fn context_id_is_forwarded_only_when_set() {
        let server = MockServer::start().await;
        rpc(&server, "message/send")
            .and(body_partial_json(
                json!({"params": {"message": {"contextId": "ctx-7"}}}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(task("completed", Some("ok"))))
            .expect(1)
            .mount(&server)
            .await;
        let req = Request {
            context_id: Some("ctx-7".into()),
            ..request("q")
        };
        client(&server, Arc::new(Metrics::new()))
            .send(req, far())
            .await
            .unwrap();
        let received = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["params"]["message"]["contextId"], "ctx-7");
        assert_eq!(body["params"]["message"]["parts"][0]["text"], "q");
        assert_eq!(body["jsonrpc"], "2.0");

        server.reset().await;
        rpc(&server, "message/send")
            .respond_with(ResponseTemplate::new(200).set_body_json(task("completed", Some("ok"))))
            .mount(&server)
            .await;
        client(&server, Arc::new(Metrics::new()))
            .send(request("q"), far())
            .await
            .unwrap();
        let received = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&received[0].body).unwrap();
        assert!(body["params"]["message"].get("contextId").is_none());
    }

    #[test]
    fn endpoint_is_built_per_agent() {
        let c = Client::new(
            "http://ctrl:8083/",
            "ns",
            "u",
            Duration::from_secs(1),
            TICK,
            Arc::new(Metrics::new()),
        );
        assert_eq!(c.endpoint("a"), "http://ctrl:8083/api/a2a/ns/a");
    }

    #[test]
    fn helpers() {
        assert!(terminal("completed") && terminal("auth-required") && !terminal("working"));
        let ids: std::collections::HashSet<String> = (0..50).map(|_| random_id()).collect();
        assert_eq!(ids.len(), 50);
        assert_eq!(random_id().len(), 32);
        let long = "x".repeat(300);
        assert_eq!(snippet(&long).chars().count(), 203);
        assert_eq!(snippet("  short  "), "short");

        let raw = format!("{{\"a\": \"{}\", \"b\": tru}}", "y".repeat(500));
        let err = serde_json::from_str::<Value>(&raw).unwrap_err();
        let near = decode_context(raw.as_bytes(), &err);
        assert!(near.contains("tru"), "{near}");
        assert!(near.chars().count() <= 203);
    }
}
