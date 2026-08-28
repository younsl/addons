//! Turns an Alertmanager webhook into a Slack thread: the alert is posted as
//! the parent message, and the agent's analysis is posted as a reply under it.

pub mod chat;
pub mod store;

use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::post;
use tokio::sync::Semaphore;
use tokio::time::{Instant, sleep, timeout, timeout_at};
use tokio_util::task::TaskTracker;
use tracing::{Instrument, error, info, info_span, warn};

use crate::a2a::{AgentClient, Request};
use crate::alert::Payload;
use crate::config::{Config, ParentMode};
use crate::observability::Metrics;
use crate::observability::metrics::{KindLabels, SeverityStatusLabels};
use crate::observability::server::health_router;
use crate::slack::{self, Message, SlackClient};
use store::{SessionStore, Store};

/// Caps the request body. Alertmanager truncates large groups itself, so
/// anything past this is a misconfiguration rather than a real alert.
const MAX_WEBHOOK_BODY: usize = 4 << 20;

/// How long a handled envelope id is remembered. Slack redelivers an envelope
/// whose acknowledgement did not arrive in time, and the acknowledgement can
/// be lost after the turn has already started, so the window only has to
/// outlive Slack's own retry.
const ENVELOPE_TTL: Duration = Duration::from_mins(5);

/// Bounds the Slack calls a finished run still owes: the reply, and the
/// reaction swap.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);
const REACTION_TIMEOUT: Duration = Duration::from_secs(30);

/// The thread note that tells readers an analysis is underway. It names the
/// agent and the analysis deadline, so the on-call engineer knows who is
/// investigating and how long to wait before treating silence as a gateway
/// failure. It accompanies the investigating reaction, so it is only posted
/// when the reactions are enabled.
fn investigating_message(agent: &str, timeout: Duration) -> String {
    format!(
        "kagent의 {agent} 에이전트가 조사를 시작했습니다. 최대 {} 내 원인 분석 결과 또는 실패 사유가 이 스레드에 게시됩니다.",
        korean_duration(timeout)
    )
}

/// Renders a duration the way the message's Korean sentence expects, dropping
/// zero components: 300s -> "5분", 90s -> "1분 30초".
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn korean_duration(d: Duration) -> String {
    let total = d.as_secs_f64().round() as u64;
    let (minutes, seconds) = (total / 60, total % 60);
    match (minutes, seconds) {
        (0, s) => format!("{s}초"),
        (m, 0) => format!("{m}분"),
        (m, s) => format!("{m}분 {s}초"),
    }
}

/// Holds the wiring shared by every webhook request and every mention.
pub struct Gateway {
    cfg: Config,
    slack: Arc<dyn SlackClient>,
    agent: Arc<dyn AgentClient>,
    metrics: Arc<Metrics>,
    store: Mutex<Store>,
    sem: Arc<Semaphore>,
    tracker: TaskTracker,
    /// Base wait between parent lookups, scaled by the attempt number.
    lookup_backoff: Duration,

    /// Mention invocation. `chat_sem` is deliberately separate from `sem`:
    /// sharing one would let a question queue behind an alert analysis, or
    /// worse, delay an analysis behind questions.
    chat_sem: Arc<Semaphore>,
    sessions: Mutex<SessionStore>,
    envelopes: Mutex<Store>,
    /// The bot's own user id, resolved once at startup. It is the loop guard:
    /// the gateway posts into the channels it listens to.
    bot_user_id: RwLock<String>,
}

/// Decrements a gauge when a run ends, however it ends.
struct GaugeGuard<'a>(&'a prometheus_client::metrics::gauge::Gauge);

impl Drop for GaugeGuard<'_> {
    fn drop(&mut self) {
        self.0.dec();
    }
}

impl Gateway {
    /// Wires a gateway. The caller owns the lifecycle of the clients.
    #[must_use]
    pub fn new(
        cfg: Config,
        slack: Arc<dyn SlackClient>,
        agent: Arc<dyn AgentClient>,
        metrics: Arc<Metrics>,
    ) -> Arc<Self> {
        // Publishing the limits as series lets a dashboard express saturation
        // as inflight over slots, without the query repeating a value that
        // lives in the deployment's environment.
        metrics
            .analysis_slots
            .set(i64::try_from(cfg.max_concurrent).unwrap_or(i64::MAX));
        metrics
            .chat_slots
            .set(i64::try_from(cfg.max_concurrent_chats).unwrap_or(i64::MAX));
        Arc::new(Self {
            store: Mutex::new(Store::new(cfg.dedupe_ttl)),
            sem: Arc::new(Semaphore::new(cfg.max_concurrent.max(1))),
            tracker: TaskTracker::new(),
            lookup_backoff: Duration::from_secs(2),
            chat_sem: Arc::new(Semaphore::new(cfg.max_concurrent_chats.max(1))),
            sessions: Mutex::new(SessionStore::new(cfg.chat_session_ttl)),
            envelopes: Mutex::new(Store::new(ENVELOPE_TTL)),
            bot_user_id: RwLock::new(String::new()),
            cfg,
            slack,
            agent,
            metrics,
        })
    }

    /// Shortens the wait between parent lookups, which tests need.
    #[cfg(test)]
    pub fn with_lookup_backoff(self: Arc<Self>, backoff: Duration) -> Arc<Self> {
        let mut g = Arc::try_unwrap(self).ok().expect("gateway not shared yet");
        g.lookup_backoff = backoff;
        Arc::new(g)
    }

    /// Records the bot's own user id for the mention loop guard. Resolution
    /// needs a Slack call, so it is done by the caller at startup rather than
    /// inside `new`, and a failure there only weakens the guard: a message the
    /// app posted still carries a bot id.
    pub fn set_bot_user_id(&self, id: &str) {
        *self.bot_user_id.write().expect("bot user id lock") = id.to_string();
    }

    fn bot_user_id(&self) -> String {
        self.bot_user_id.read().expect("bot user id lock").clone()
    }

    /// The router serving the webhook and the health endpoints.
    pub fn router(self: &Arc<Self>) -> Router {
        health_router()
            .route(&self.cfg.webhook_path, post(handle_webhook))
            .layer(DefaultBodyLimit::max(MAX_WEBHOOK_BODY))
            .with_state(self.clone())
    }

    /// Blocks until every in-flight run finishes or `limit` elapses. It
    /// reports whether the drain completed, so shutdown can log a truthful
    /// message instead of claiming a clean stop it did not achieve.
    pub async fn wait(&self, limit: Duration) -> bool {
        self.tracker.close();
        timeout(limit, self.tracker.wait()).await.is_ok()
    }

    /// Counts the tasks still running, which tests read.
    #[cfg(test)]
    pub fn inflight(&self) -> usize {
        self.tracker.len()
    }

    async fn webhook(self: &Arc<Self>, headers: &HeaderMap, body: &Bytes) -> StatusCode {
        // Alertmanager retries a receiver that answers too slowly, so the
        // handler's own latency is worth watching even though the analysis
        // runs detached.
        let started = Instant::now();
        let status = self.webhook_inner(headers, body).await;
        self.metrics
            .webhook_duration
            .observe(started.elapsed().as_secs_f64());
        status
    }

    async fn webhook_inner(self: &Arc<Self>, headers: &HeaderMap, body: &Bytes) -> StatusCode {
        if !self.authorized(headers) {
            self.metrics.webhook("unauthorized");
            warn!(reason = "bad bearer token", "rejected webhook");
            return StatusCode::UNAUTHORIZED;
        }

        let payload: Payload = match serde_json::from_slice(body) {
            Ok(payload) => payload,
            Err(err) => {
                self.metrics.webhook("bad_request");
                error!(error = %err, "failed to decode webhook payload");
                return StatusCode::BAD_REQUEST;
            }
        };
        if payload.alerts.is_empty() {
            self.metrics.webhook("empty");
            warn!(
                group_key = payload.group_key,
                receiver = payload.receiver,
                "webhook carried no alerts"
            );
            return StatusCode::NO_CONTENT;
        }

        self.count_alerts(&payload);

        let channel = self.resolve_channel(&payload);
        let agent = self.resolve_agent(&payload);
        let span = info_span!(
            "alert",
            alertname = payload.name(),
            severity = payload.severity(),
            cluster = payload.cluster(),
            status = payload.status,
            group_key = payload.group_key,
            alert_count = payload.alerts.len(),
            channel,
            agent,
        );
        let _enter = span.enter();

        // In lookup mode Alertmanager owns the notification, so an alert
        // nobody analyses costs the gateway nothing at all.
        let mut thread_ts = String::new();
        if self.cfg.parent_mode == ParentMode::Post {
            let msg = Message {
                channel: channel.clone(),
                title: payload.title(),
                color: payload.color().to_string(),
                text: self.truncate("parent", &payload.slack_text(self.cfg.max_alerts_in_prompt)),
                thread_ts: String::new(),
            };
            match self.slack.post(msg).await {
                Ok(ts) => {
                    self.metrics.slack_message("parent", "ok");
                    info!(thread_ts = ts, "posted alert to slack");
                    thread_ts = ts;
                }
                Err(err) => {
                    self.metrics.slack_message("parent", "error");
                    self.metrics.webhook("slack_error");
                    error!(error = %err, "failed to post alert to slack");
                    // Report the failure so Alertmanager retries the
                    // notification instead of dropping an alert nobody ever
                    // saw.
                    return StatusCode::BAD_GATEWAY;
                }
            }
        }

        let verdict = self.should_analyze(&payload);
        self.publish_dedupe_size();
        if let Err(reason) = verdict {
            self.metrics
                .analyses_skipped
                .get_or_create(&crate::observability::metrics::ReasonLabels {
                    reason: reason.into(),
                })
                .inc();
            self.metrics.webhook("posted");
            info!(reason, "skipped analysis");
            return StatusCode::ACCEPTED;
        }

        self.metrics.webhook("analyzing");
        let g = self.clone();
        let span = span.clone();
        self.tracker.spawn(
            async move { g.analyze(payload, agent, channel, thread_ts).await }.instrument(span),
        );
        StatusCode::ACCEPTED
    }

    /// Runs the agent and posts its reply in the alert's thread. It runs
    /// detached from the webhook request because a blocking agent call
    /// outlives the Alertmanager HTTP timeout.
    #[allow(clippy::too_many_lines, clippy::significant_drop_tightening)]
    async fn analyze(
        self: Arc<Self>,
        payload: Payload,
        agent: String,
        channel: String,
        mut thread_ts: String,
    ) {
        let deadline = Instant::now() + self.cfg.kagent_timeout;

        let queued = Instant::now();
        self.metrics.analyses_queued.inc();
        let permit = timeout_at(deadline, self.sem.clone().acquire_owned()).await;
        self.metrics.analyses_queued.dec();
        let Ok(Ok(_permit)) = permit else {
            self.forget(&payload);
            self.metrics.analysis(&agent, "queue_timeout");
            error!(timeout = ?self.cfg.kagent_timeout, "gave up waiting for an analysis slot");
            self.reply(
                &payload,
                &channel,
                &thread_ts,
                ":warning: Automated analysis was skipped: all analysis slots were busy.",
            )
            .await;
            return;
        };
        self.metrics
            .analysis_queue_wait
            .observe(queued.elapsed().as_secs_f64());
        self.metrics.analyses_inflight.inc();
        let _inflight = GaugeGuard(&self.metrics.analyses_inflight);

        // The reactions mark the alert notification while the agent works and
        // once it is done, which needs the parent located before the run
        // instead of after. The early lookup result is reused by reply, so
        // nothing is paid twice.
        let reactions_wanted =
            !self.cfg.investigating_reaction.is_empty() || !self.cfg.completed_reaction.is_empty();
        if thread_ts.is_empty() && reactions_wanted {
            match self.find_parent(&payload, &channel, deadline).await {
                Ok(ts) => thread_ts = ts,
                Err(err) => {
                    warn!(error = %err, "alert notification not found before analysis, skipping reactions");
                }
            }
        }
        let reacting = !thread_ts.is_empty() && reactions_wanted;
        if reacting {
            if !self.cfg.investigating_reaction.is_empty()
                && let Err(err) = self
                    .slack
                    .add_reaction(&channel, &thread_ts, &self.cfg.investigating_reaction)
                    .await
            {
                warn!(error = %err, "failed to add investigating reaction");
            }
            let note = Message {
                channel: channel.clone(),
                thread_ts: thread_ts.clone(),
                text: investigating_message(&agent, self.cfg.kagent_timeout),
                ..Message::default()
            };
            if let Err(err) = self.slack.post(note).await {
                warn!(error = %err, "failed to post investigating message");
            }
        }

        let started = Instant::now();
        info!(thread_ts, "requesting analysis");
        // No context id: an alert is not a conversation, so every analysis
        // starts a session of its own, unlike a mention continuing a thread.
        let req = Request {
            agent: agent.clone(),
            text: payload.prompt(&self.cfg.instructions, self.cfg.max_alerts_in_prompt),
            ..Request::default()
        };
        let result = self.agent.send(req, deadline).await;
        let elapsed = started.elapsed();
        self.metrics
            .analysis_duration
            .observe(elapsed.as_secs_f64());

        match result {
            Ok(reply) => {
                self.metrics.analysis(&agent, "ok");
                info!(duration = ?elapsed, task_id = reply.task_id, chars = reply.text.chars().count(), "analysis completed");
                self.reply(&payload, &channel, &thread_ts, &reply.text)
                    .await;
            }
            Err(err) => {
                // Drop the dedupe entry so the next resend of this group
                // retries instead of inheriting the suppression of a run that
                // produced nothing.
                self.forget(&payload);
                self.metrics.analysis(&agent, "error");
                error!(error = %err, duration = ?elapsed, "analysis failed");
                let text = format!(
                    ":warning: Automated analysis failed. {}",
                    err.user_message()
                );
                self.reply(&payload, &channel, &thread_ts, &text).await;
            }
        }

        if reacting {
            // The swap runs on its own budget: by the time it runs, the
            // analysis deadline has often been consumed by the agent call the
            // reaction was covering.
            let swap = async {
                if !self.cfg.investigating_reaction.is_empty()
                    && let Err(err) = self
                        .slack
                        .remove_reaction(&channel, &thread_ts, &self.cfg.investigating_reaction)
                        .await
                {
                    warn!(error = %err, "failed to remove investigating reaction");
                }
                if !self.cfg.completed_reaction.is_empty()
                    && let Err(err) = self
                        .slack
                        .add_reaction(&channel, &thread_ts, &self.cfg.completed_reaction)
                        .await
                {
                    warn!(error = %err, "failed to add completed reaction");
                }
            };
            if timeout(REACTION_TIMEOUT, swap).await.is_err() {
                warn!("reaction swap timed out");
            }
        }
    }

    /// Posts a thread message under the alert. Failures are logged and counted
    /// only: there is nowhere left to report them.
    async fn reply(&self, payload: &Payload, channel: &str, thread_ts: &str, text: &str) {
        let deadline = Instant::now() + REPLY_TIMEOUT;
        let mut msg = Message {
            channel: channel.to_string(),
            thread_ts: thread_ts.to_string(),
            text: self.truncate("thread", text),
            ..Message::default()
        };
        if msg.thread_ts.is_empty() {
            // Lookup mode: Alertmanager posted the alert, so the parent has to
            // be found before there is a thread to reply in. Searching after
            // the analysis rather than before means the notification has had
            // the whole agent run to arrive, which is why one attempt usually
            // suffices.
            match self.find_parent(payload, channel, deadline).await {
                Ok(ts) => msg.thread_ts = ts,
                Err(err) => {
                    // The analysis is already paid for, so it is posted at
                    // channel level rather than discarded. It carries the
                    // alert title so a reader can still tell which alert it
                    // belongs to.
                    self.metrics.slack_message("orphan", "attempted");
                    error!(error = %err, "posting analysis without a thread");
                    msg.title = payload.title();
                    msg.color = payload.color().to_string();
                }
            }
        }
        let thread_ts = msg.thread_ts.clone();
        match timeout_at(deadline, self.slack.post(msg)).await {
            Ok(Ok(_)) => {
                self.metrics.slack_message("thread", "ok");
                info!(thread_ts, "posted analysis to slack thread");
            }
            Ok(Err(err)) => {
                self.metrics.slack_message("thread", "error");
                error!(error = %err, "failed to post analysis to slack thread");
            }
            Err(_) => {
                self.metrics.slack_message("thread", "error");
                error!("failed to post analysis to slack thread: timed out");
            }
        }
    }

    /// Locates the notification Alertmanager posted for this alert group.
    /// Slack indexes nothing by alert, so the search is a scan of recent
    /// channel history for the marker the Alertmanager template renders.
    async fn find_parent(
        &self,
        payload: &Payload,
        channel: &str,
        deadline: Instant,
    ) -> Result<String, slack::Error> {
        let marker = payload.marker();
        if marker.is_empty() {
            return Err(slack::Error::Other(anyhow::anyhow!(
                "alert carries no fingerprint or group key to match on"
            )));
        }

        let attempts = self.cfg.lookup_attempts;
        let mut last_err = slack::Error::MessageNotFound;
        for attempt in 1..=attempts {
            let since = SystemTime::now() - self.cfg.lookup_window;
            match timeout_at(
                deadline,
                self.slack.find_thread_parent(channel, marker, since),
            )
            .await
            {
                Ok(Ok(ts)) => {
                    self.metrics.parent_lookup("found", attempt);
                    return Ok(ts);
                }
                Ok(Err(slack::Error::MessageNotFound)) => last_err = slack::Error::MessageNotFound,
                Ok(Err(err)) => {
                    self.metrics.parent_lookup("error", attempt);
                    return Err(err);
                }
                Err(_) => {
                    self.metrics.parent_lookup("error", attempt);
                    return Err(slack::Error::Other(anyhow::anyhow!(
                        "parent lookup deadline exceeded"
                    )));
                }
            }
            // Alertmanager sends the Slack notification and the webhook
            // independently, so a not-found result can simply mean the
            // notification has not landed yet.
            if attempt == attempts {
                break;
            }
            warn!(
                attempt,
                marker, "alert notification not in channel history yet, retrying"
            );
            if timeout_at(deadline, sleep(self.lookup_backoff * attempt))
                .await
                .is_err()
            {
                self.metrics.parent_lookup("error", attempt);
                return Err(slack::Error::Other(anyhow::anyhow!(
                    "parent lookup deadline exceeded"
                )));
            }
        }
        self.metrics.parent_lookup("not_found", attempts);
        Err(last_err)
    }

    /// Records the individual alerts inside a group. The webhook counter only
    /// sees groups, which hides an alert storm that Alertmanager batches into
    /// a handful of notifications.
    fn count_alerts(&self, p: &Payload) {
        for a in &p.alerts {
            let severity = a
                .labels
                .get("severity")
                .map_or_else(|| p.severity().to_string(), Clone::clone);
            let status = if a.status.is_empty() {
                p.status.clone()
            } else {
                a.status.clone()
            };
            self.metrics
                .alerts_received
                .get_or_create(&SeverityStatusLabels { severity, status })
                .inc();
        }
    }

    /// Cuts text to the Slack limit and counts the cut. A truncated thread
    /// reply is the one case where the gateway silently drops content the
    /// reader wanted, so it is worth a series of its own.
    fn truncate(&self, kind: &str, text: &str) -> String {
        let out = slack::truncate(text, self.cfg.slack_max_text_chars);
        if out != text {
            self.metrics
                .slack_truncations
                .get_or_create(&KindLabels {
                    kind: kind.to_string(),
                })
                .inc();
        }
        out
    }

    /// Decides whether an alert group is worth an agent run and returns the
    /// skip reason when it is not.
    fn should_analyze(&self, p: &Payload) -> Result<(), &'static str> {
        if p.resolved() && !self.cfg.analyze_resolved {
            return Err("resolved");
        }
        if !self.severity_wanted(p) {
            return Err("severity");
        }
        if !self
            .store
            .lock()
            .expect("dedupe store lock")
            .allow(&p.dedupe_key(), Instant::now())
        {
            return Err("deduplicated");
        }
        Ok(())
    }

    fn forget(&self, p: &Payload) {
        self.store
            .lock()
            .expect("dedupe store lock")
            .forget(&p.dedupe_key());
        self.publish_dedupe_size();
    }

    fn publish_dedupe_size(&self) {
        let size = self.store.lock().expect("dedupe store lock").len();
        self.metrics
            .dedupe_entries
            .set(i64::try_from(size).unwrap_or(i64::MAX));
    }

    /// Applies the severity filter. The opt-in label overrides it in both
    /// directions so a single rule can request or refuse analysis on its own.
    fn severity_wanted(&self, p: &Payload) -> bool {
        if let Some(v) = p.common_labels.get(&self.cfg.analyze_label) {
            return v == "true";
        }
        if self.cfg.analyze_severities.is_empty() {
            return true;
        }
        self.cfg.analyze_severities.contains(p.severity())
    }

    /// Picks the destination channel from the routing label that Alertmanager
    /// already uses, falling back to the configured default.
    fn resolve_channel(&self, p: &Payload) -> String {
        let value = routing_value(p, &self.cfg.channel_label);
        if value.is_empty() {
            return slack::normalize_channel(&self.cfg.slack_channel);
        }
        if let Some(mapped) = self.cfg.slack_channel_map.get(value) {
            return slack::normalize_channel(mapped);
        }
        slack::normalize_channel(value)
    }

    /// Picks the agent that analyses this alert. Unlike the channel, an
    /// unmapped label value falls back to the default agent instead of being
    /// used as a name: an agent that does not exist on the controller would
    /// turn every alert in that category into a failed analysis.
    fn resolve_agent(&self, p: &Payload) -> String {
        let value = routing_value(p, &self.cfg.kagent_agent_routing_label);
        if !value.is_empty()
            && let Some(mapped) = self.cfg.kagent_agent_routing_map.get(value)
        {
            return mapped.clone();
        }
        self.cfg.kagent_agent.clone()
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        if self.cfg.webhook_token.is_empty() {
            return true;
        }
        let got = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map_or("", |v| v.strip_prefix("Bearer ").unwrap_or(v));
        constant_time_eq(got.as_bytes(), self.cfg.webhook_token.as_bytes())
    }
}

/// Reads a routing label from the group, preferring the labels every alert in
/// it shares over the ones it was grouped by.
fn routing_value<'a>(p: &'a Payload, label: &str) -> &'a str {
    match p.common_labels.get(label) {
        Some(v) if !v.is_empty() => v,
        _ => p.group_labels.get(label).map_or("", String::as_str),
    }
}

/// Compares two byte strings without an early exit on the first mismatch, so
/// a wrong token cannot be guessed one byte at a time from the response time.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn handle_webhook(
    State(g): State<Arc<Gateway>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    g.webhook(&headers, &body).await
}

#[cfg(test)]
pub mod testing {
    //! Fakes shared by the gateway and chat tests.

    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use tokio::sync::Notify;

    use super::*;
    use crate::a2a::client::Reply;
    use crate::a2a::{self, Progress};
    use crate::slack::client::ThreadMessage;

    /// One Slack call as the fake saw it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Call {
        Post(Message),
        Update {
            channel: String,
            ts: String,
            text: String,
        },
        Ephemeral {
            channel: String,
            thread_ts: String,
            user: String,
            text: String,
        },
        Find {
            channel: String,
            marker: String,
        },
        ThreadParent {
            channel: String,
            thread_ts: String,
        },
        AddReaction {
            channel: String,
            ts: String,
            name: String,
        },
        RemoveReaction {
            channel: String,
            ts: String,
            name: String,
        },
    }

    #[derive(Default)]
    pub struct FakeSlack {
        pub calls: Mutex<Vec<Call>>,
        pub notify: Notify,
        /// Fails every post when set.
        pub post_error: Mutex<Option<String>>,
        /// Fails every update when set.
        pub update_error: Mutex<Option<String>>,
        /// Results for successive lookups; empty means found at "1700.1".
        pub lookups: Mutex<Vec<Result<String, slack::Error>>>,
        pub parent: Mutex<Option<Result<ThreadMessage, slack::Error>>>,
        pub channel_ids: Mutex<HashMap<String, String>>,
        pub reaction_error: Mutex<Option<String>>,
    }

    impl FakeSlack {
        pub fn record(&self, call: Call) {
            self.calls.lock().unwrap().push(call);
            self.notify.notify_waiters();
        }

        pub fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        pub fn posts(&self) -> Vec<Message> {
            self.calls()
                .into_iter()
                .filter_map(|c| if let Call::Post(m) = c { Some(m) } else { None })
                .collect()
        }

        pub fn set_lookups(&self, results: Vec<Result<String, slack::Error>>) {
            *self.lookups.lock().unwrap() = results;
        }
    }

    #[async_trait]
    impl SlackClient for FakeSlack {
        async fn post(&self, msg: Message) -> Result<String, slack::Error> {
            self.record(Call::Post(msg));
            let failure = self.post_error.lock().unwrap().clone();
            if let Some(err) = failure {
                return Err(slack::Error::Other(anyhow::anyhow!(err)));
            }
            Ok(format!("{}.0", self.calls.lock().unwrap().len()))
        }

        async fn update(&self, channel: &str, ts: &str, text: &str) -> Result<(), slack::Error> {
            self.record(Call::Update {
                channel: channel.into(),
                ts: ts.into(),
                text: text.into(),
            });
            let failure = self.update_error.lock().unwrap().clone();
            if let Some(err) = failure {
                return Err(slack::Error::Other(anyhow::anyhow!(err)));
            }
            Ok(())
        }

        async fn post_ephemeral(
            &self,
            channel: &str,
            thread_ts: &str,
            user: &str,
            text: &str,
        ) -> Result<(), slack::Error> {
            self.record(Call::Ephemeral {
                channel: channel.into(),
                thread_ts: thread_ts.into(),
                user: user.into(),
                text: text.into(),
            });
            Ok(())
        }

        async fn find_thread_parent(
            &self,
            channel: &str,
            marker: &str,
            _since: SystemTime,
        ) -> Result<String, slack::Error> {
            self.record(Call::Find {
                channel: channel.into(),
                marker: marker.into(),
            });
            let mut lookups = self.lookups.lock().unwrap();
            if lookups.is_empty() {
                return Ok("1700.1".into());
            }
            lookups.remove(0)
        }

        async fn thread_parent(
            &self,
            channel: &str,
            thread_ts: &str,
        ) -> Result<ThreadMessage, slack::Error> {
            self.record(Call::ThreadParent {
                channel: channel.into(),
                thread_ts: thread_ts.into(),
            });
            match self.parent.lock().unwrap().as_ref() {
                Some(Ok(m)) => Ok(m.clone()),
                Some(Err(_)) => Err(slack::Error::MessageNotFound),
                None => Ok(ThreadMessage {
                    ts: thread_ts.into(),
                    text: "🚨 [FIRING] Alert\nalert-id fp-1".into(),
                    ..ThreadMessage::default()
                }),
            }
        }

        async fn add_reaction(
            &self,
            channel: &str,
            ts: &str,
            name: &str,
        ) -> Result<(), slack::Error> {
            self.record(Call::AddReaction {
                channel: channel.into(),
                ts: ts.into(),
                name: name.into(),
            });
            let failure = self.reaction_error.lock().unwrap().clone();
            if let Some(err) = failure {
                return Err(slack::Error::Other(anyhow::anyhow!(err)));
            }
            Ok(())
        }

        async fn remove_reaction(
            &self,
            channel: &str,
            ts: &str,
            name: &str,
        ) -> Result<(), slack::Error> {
            self.record(Call::RemoveReaction {
                channel: channel.into(),
                ts: ts.into(),
                name: name.into(),
            });
            Ok(())
        }

        async fn resolve_channel_id(&self, channel: &str) -> Result<String, slack::Error> {
            let ids = self.channel_ids.lock().unwrap();
            ids.get(channel)
                .cloned()
                .ok_or_else(|| slack::Error::Other(anyhow::anyhow!("unknown channel {channel}")))
        }
    }

    /// A scripted agent: replies with `reply`, or fails with `error`, after
    /// `delay`, reporting `states` through the progress hook first.
    #[derive(Default)]
    pub struct FakeAgent {
        pub requests: Mutex<Vec<Request>>,
        pub reply: Mutex<Option<Reply>>,
        pub error: Mutex<Option<String>>,
        pub delay: Mutex<Duration>,
        pub states: Mutex<Vec<&'static str>>,
        pub inflight: std::sync::atomic::AtomicUsize,
        pub max_inflight: std::sync::atomic::AtomicUsize,
        /// Holds the slot for the whole delay even past the deadline, which
        /// is how the queue timeout tests starve the next run.
        pub ignore_deadline: std::sync::atomic::AtomicBool,
    }

    impl FakeAgent {
        pub fn requests(&self) -> Vec<Request> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AgentClient for FakeAgent {
        async fn send(&self, req: Request, deadline: Instant) -> Result<Reply, a2a::Error> {
            use std::sync::atomic::Ordering;
            self.requests.lock().unwrap().push(req.clone());
            let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_inflight.fetch_max(now, Ordering::SeqCst);
            for (i, state) in self.states.lock().unwrap().iter().enumerate() {
                if let Some(hook) = &req.on_progress {
                    hook(Progress {
                        task_id: "task-1".into(),
                        state: (*state).into(),
                        polls: u32::try_from(i).unwrap(),
                    });
                }
            }
            let delay = *self.delay.lock().unwrap();
            let timed_out = if self.ignore_deadline.load(Ordering::SeqCst) {
                sleep(delay).await;
                false
            } else {
                timeout_at(deadline, sleep(delay)).await.is_err()
            };
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            if timed_out {
                return Err(a2a::Error::new(
                    "the analysis ran past its deadline and was cancelled",
                    anyhow::anyhow!("deadline"),
                ));
            }
            let failure = self.error.lock().unwrap().clone();
            if let Some(err) = failure {
                return Err(a2a::Error::new(err.clone(), anyhow::anyhow!(err)));
            }
            let reply = self.reply.lock().unwrap().clone();
            Ok(reply.unwrap_or_else(|| Reply {
                text: "*Summary* all good".into(),
                task_id: "task-1".into(),
                context_id: "ctx-1".into(),
            }))
        }
    }

    pub fn config() -> Config {
        let mut cfg = Config::from_env(|key| match key {
            "SLACK_BOT_TOKEN" => Some("xoxb".into()),
            "SLACK_DEFAULT_CHANNEL" => Some("alerts".into()),
            "SLACK_PARENT_MODE" => Some("post".into()),
            "SLACK_INVESTIGATING_REACTION" | "SLACK_COMPLETED_REACTION" => Some(String::new()),
            _ => None,
        })
        .unwrap();
        cfg.kagent_timeout = Duration::from_millis(500);
        cfg.chat_timeout = Duration::from_millis(500);
        cfg
    }

    pub fn gateway(cfg: Config) -> (Arc<Gateway>, Arc<FakeSlack>, Arc<FakeAgent>, Arc<Metrics>) {
        let slack = Arc::new(FakeSlack::default());
        let agent = Arc::new(FakeAgent::default());
        let metrics = Arc::new(Metrics::new());
        let g = Gateway::new(cfg, slack.clone(), agent.clone(), metrics.clone())
            .with_lookup_backoff(Duration::from_millis(1));
        (g, slack, agent, metrics)
    }

    pub fn payload(alertname: &str, severity: &str) -> serde_json::Value {
        serde_json::json!({
            "groupKey": format!("{{}}:{{alertname=\"{alertname}\"}}"),
            "status": "firing",
            "receiver": "kagent-gateway",
            "commonLabels": {"alertname": alertname, "severity": severity, "cluster": "prd"},
            "alerts": [{"status": "firing", "fingerprint": "fp-1",
                "labels": {"alertname": alertname, "severity": severity},
                "annotations": {"summary": "something broke"}}]
        })
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use serde_json::json;
    use tower::ServiceExt;

    use super::testing::{Call, config, gateway, payload};
    use super::*;
    use crate::observability::metrics::{
        AgentResultLabels, KindResultLabels, ReasonLabels, ResultLabels,
    };

    async fn post(g: &Arc<Gateway>, body: serde_json::Value, token: Option<&str>) -> StatusCode {
        let mut req = HttpRequest::builder()
            .method("POST")
            .uri("/alert")
            .header("content-type", "application/json");
        if let Some(token) = token {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let req = req.body(Body::from(body.to_string())).unwrap();
        g.router().oneshot(req).await.unwrap().status()
    }

    async fn drain(g: &Arc<Gateway>) {
        assert!(g.wait(Duration::from_secs(5)).await, "runs still in flight");
    }

    fn webhooks(m: &Metrics, result: &str) -> u64 {
        Metrics::counter(
            &m.webhooks_received,
            &ResultLabels {
                result: result.into(),
            },
        )
    }

    #[tokio::test]
    async fn post_mode_posts_alert_then_analysis() {
        let (g, slack, agent, metrics) = gateway(config());
        assert_eq!(
            post(&g, payload("KubePodCrashLooping", "critical"), None).await,
            StatusCode::ACCEPTED
        );
        drain(&g).await;

        let posts = slack.posts();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0].channel, "#alerts");
        assert_eq!(posts[0].title, "🚨 [FIRING] KubePodCrashLooping");
        assert_eq!(posts[0].color, "danger");
        assert!(posts[0].text.contains("*Summary:* something broke"));
        assert_eq!(posts[1].thread_ts, "1.0");
        assert_eq!(posts[1].text, "*Summary* all good");
        assert!(posts[1].title.is_empty());

        let reqs = agent.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].agent, "alert-triage-agent");
        assert!(reqs[0].text.contains("alertname: KubePodCrashLooping"));
        assert!(reqs[0].text.contains(&config().instructions));
        assert_eq!(reqs[0].context_id, None);

        assert_eq!(webhooks(&metrics, "analyzing"), 1);
        assert_eq!(
            Metrics::counter(
                &metrics.slack_messages,
                &KindResultLabels {
                    kind: "parent".into(),
                    result: "ok".into()
                }
            ),
            1
        );
        assert_eq!(
            Metrics::counter(
                &metrics.slack_messages,
                &KindResultLabels {
                    kind: "thread".into(),
                    result: "ok".into()
                }
            ),
            1
        );
        assert_eq!(
            Metrics::counter(
                &metrics.analyses,
                &AgentResultLabels {
                    agent: "alert-triage-agent".into(),
                    result: "ok".into()
                }
            ),
            1
        );
        assert_eq!(
            Metrics::counter(
                &metrics.alerts_received,
                &SeverityStatusLabels {
                    severity: "critical".into(),
                    status: "firing".into()
                }
            ),
            1
        );
        assert_eq!(metrics.analysis_slots.get(), 2);
        assert_eq!(metrics.dedupe_entries.get(), 1);
        assert_eq!(metrics.analyses_inflight.get(), 0);
    }

    #[tokio::test]
    async fn reactions_wrap_the_analysis() {
        let mut cfg = config();
        cfg.investigating_reaction = "telescope".into();
        cfg.completed_reaction = "white_check_mark".into();
        let (g, slack, _, _) = gateway(cfg);
        post(&g, payload("A", "critical"), None).await;
        drain(&g).await;
        let calls = slack.calls();
        let kinds: Vec<String> = calls
            .iter()
            .map(|c| match c {
                Call::Post(m) if m.thread_ts.is_empty() => "parent".to_string(),
                Call::Post(m) => format!("reply:{}", &m.text[..m.text.len().min(6)]),
                Call::AddReaction { name, .. } => format!("add:{name}"),
                Call::RemoveReaction { name, .. } => format!("remove:{name}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "parent",
                "add:telescope",
                "reply:kagent",
                "reply:*Summa",
                "remove:telescope",
                "add:white_check_mark"
            ]
        );
        assert!(
            matches!(&calls[2], Call::Post(m) if m.text == investigating_message("alert-triage-agent", Duration::from_millis(500)))
        );
    }

    #[test]
    fn investigating_message_reads_korean_durations() {
        assert_eq!(korean_duration(Duration::from_secs(300)), "5분");
        assert_eq!(korean_duration(Duration::from_secs(90)), "1분 30초");
        assert_eq!(korean_duration(Duration::from_secs(45)), "45초");
        assert!(
            investigating_message("triage", Duration::from_secs(120))
                .contains("triage 에이전트가 조사를 시작했습니다. 최대 2분 내")
        );
    }

    #[tokio::test]
    async fn reaction_failure_does_not_block_analysis() {
        let mut cfg = config();
        cfg.investigating_reaction = "telescope".into();
        let (g, slack, agent, _) = gateway(cfg);
        *slack.reaction_error.lock().unwrap() = Some("missing_scope".into());
        post(&g, payload("A", "critical"), None).await;
        drain(&g).await;
        assert_eq!(agent.requests().len(), 1);
        assert_eq!(slack.posts().len(), 3);
    }

    #[tokio::test]
    async fn agent_failure_is_reported_in_thread_and_retried_on_resend() {
        let (g, slack, agent, metrics) = gateway(config());
        *agent.error.lock().unwrap() = Some("the controller returned HTTP 502".into());
        post(&g, payload("A", "critical"), None).await;
        drain(&g).await;
        let posts = slack.posts();
        assert_eq!(
            posts[1].text,
            ":warning: Automated analysis failed. the controller returned HTTP 502"
        );
        assert_eq!(posts[1].thread_ts, "1.0");
        assert_eq!(
            Metrics::counter(
                &metrics.analyses,
                &AgentResultLabels {
                    agent: "alert-triage-agent".into(),
                    result: "error".into()
                }
            ),
            1
        );
        assert_eq!(metrics.dedupe_entries.get(), 0);

        *agent.error.lock().unwrap() = None;
        post(&g, payload("A", "critical"), None).await;
        drain(&g).await;
        assert_eq!(agent.requests().len(), 2);
    }

    #[tokio::test]
    async fn repeated_groups_are_deduplicated() {
        let (g, _, agent, metrics) = gateway(config());
        post(&g, payload("A", "critical"), None).await;
        post(&g, payload("A", "critical"), None).await;
        drain(&g).await;
        assert_eq!(agent.requests().len(), 1);
        assert_eq!(
            Metrics::counter(
                &metrics.analyses_skipped,
                &ReasonLabels {
                    reason: "deduplicated".into()
                }
            ),
            1
        );
        assert_eq!(webhooks(&metrics, "posted"), 1);
    }

    #[tokio::test]
    async fn unwanted_alerts_are_posted_but_not_analysed() {
        let (g, slack, agent, metrics) = gateway(config());
        assert_eq!(
            post(&g, payload("W", "warning"), None).await,
            StatusCode::ACCEPTED
        );
        let mut resolved = payload("R", "critical");
        resolved["status"] = json!("resolved");
        post(&g, resolved, None).await;
        drain(&g).await;
        assert_eq!(slack.posts().len(), 2);
        assert!(agent.requests().is_empty());
        assert_eq!(
            Metrics::counter(
                &metrics.analyses_skipped,
                &ReasonLabels {
                    reason: "severity".into()
                }
            ),
            1
        );
        assert_eq!(
            Metrics::counter(
                &metrics.analyses_skipped,
                &ReasonLabels {
                    reason: "resolved".into()
                }
            ),
            1
        );
    }

    #[tokio::test]
    async fn opt_in_label_and_filters() {
        let (g, _, agent, _) = gateway(config());
        let mut wanted = payload("W", "warning");
        wanted["commonLabels"]["analyze"] = json!("true");
        let mut refused = payload("C", "critical");
        refused["commonLabels"]["analyze"] = json!("false");
        post(&g, wanted, None).await;
        post(&g, refused, None).await;
        drain(&g).await;
        assert_eq!(agent.requests().len(), 1);
        assert!(agent.requests()[0].text.contains("alertname: W"));

        let mut cfg = config();
        cfg.analyze_resolved = true;
        cfg.analyze_severities.clear();
        let (g, _, agent, _) = gateway(cfg);
        let mut resolved = payload("R", "info");
        resolved["status"] = json!("resolved");
        post(&g, resolved, None).await;
        post(&g, payload("I", "info"), None).await;
        drain(&g).await;
        assert_eq!(agent.requests().len(), 2);
    }

    #[tokio::test]
    async fn slack_failure_returns_502() {
        let (g, slack, agent, metrics) = gateway(config());
        *slack.post_error.lock().unwrap() = Some("slack error: channel_not_found".into());
        assert_eq!(
            post(&g, payload("A", "critical"), None).await,
            StatusCode::BAD_GATEWAY
        );
        drain(&g).await;
        assert!(agent.requests().is_empty());
        assert_eq!(webhooks(&metrics, "slack_error"), 1);
    }

    #[tokio::test]
    async fn bad_input_and_auth() {
        let (g, _, _, metrics) = gateway(config());
        assert_eq!(
            post(&g, json!("not an object"), None).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            post(&g, json!({"alerts": []}), None).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(webhooks(&metrics, "bad_request"), 1);
        assert_eq!(webhooks(&metrics, "empty"), 1);

        let mut cfg = config();
        cfg.webhook_token = "secret".into();
        let (g, _, _, metrics) = gateway(cfg);
        assert_eq!(
            post(&g, payload("A", "critical"), None).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            post(&g, payload("A", "critical"), Some("wrong")).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            post(&g, payload("A", "critical"), Some("secret")).await,
            StatusCode::ACCEPTED
        );
        assert_eq!(webhooks(&metrics, "unauthorized"), 2);
        drain(&g).await;
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[tokio::test]
    async fn health_endpoints_are_served_next_to_the_webhook() {
        let (g, _, _, _) = gateway(config());
        for path in ["/healthz", "/readyz"] {
            let req = HttpRequest::builder()
                .uri(path)
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                g.router().oneshot(req).await.unwrap().status(),
                StatusCode::OK
            );
        }
        let req = HttpRequest::builder()
            .method("GET")
            .uri("/alert")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            g.router().oneshot(req).await.unwrap().status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    #[tokio::test]
    async fn routing_picks_channel_and_agent() {
        let mut cfg = config();
        cfg.slack_channel_map
            .insert("infra".into(), "infra-alerts".into());
        cfg.kagent_agent_routing_map
            .insert("infra".into(), "infra-agent".into());
        let (g, slack, agent, _) = gateway(cfg);

        let mut p = payload("A", "critical");
        p["commonLabels"]["slack_channel"] = json!("infra");
        post(&g, p, None).await;
        let mut p = payload("B", "critical");
        p["groupLabels"] = json!({"slack_channel": "C0123456"});
        post(&g, p, None).await;
        let mut p = payload("C", "critical");
        p["commonLabels"]["slack_channel"] = json!("other");
        post(&g, p, None).await;
        drain(&g).await;

        let parents: Vec<String> = slack
            .posts()
            .into_iter()
            .filter(|m| m.thread_ts.is_empty())
            .map(|m| m.channel)
            .collect();
        assert_eq!(parents, vec!["#infra-alerts", "C0123456", "#other"]);
        let agents: Vec<String> = agent.requests().into_iter().map(|r| r.agent).collect();
        assert_eq!(
            agents,
            vec!["infra-agent", "alert-triage-agent", "alert-triage-agent"]
        );
    }

    #[tokio::test]
    async fn concurrency_is_capped_and_wait_reports_timeouts() {
        let mut cfg = config();
        cfg.max_concurrent = 2;
        cfg.kagent_timeout = Duration::from_secs(5);
        let (g, _, agent, metrics) = gateway(cfg);
        *agent.delay.lock().unwrap() = Duration::from_millis(100);
        for name in ["A", "B", "C", "D"] {
            post(&g, payload(name, "critical"), None).await;
        }
        assert_eq!(g.inflight(), 4);
        assert!(!g.wait(Duration::from_millis(10)).await);
        drain(&g).await;
        assert_eq!(
            agent.max_inflight.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert_eq!(agent.requests().len(), 4);
        assert_eq!(metrics.analyses_queued.get(), 0);
    }

    #[tokio::test]
    async fn queue_timeout_is_reported() {
        let mut cfg = config();
        cfg.max_concurrent = 1;
        cfg.kagent_timeout = Duration::from_millis(80);
        let (g, slack, agent, metrics) = gateway(cfg);
        *agent.delay.lock().unwrap() = Duration::from_millis(200);
        agent
            .ignore_deadline
            .store(true, std::sync::atomic::Ordering::SeqCst);
        post(&g, payload("A", "critical"), None).await;
        post(&g, payload("B", "critical"), None).await;
        drain(&g).await;
        let texts: Vec<String> = slack
            .posts()
            .into_iter()
            .filter(|m| !m.thread_ts.is_empty())
            .map(|m| m.text)
            .collect();
        assert!(
            texts
                .iter()
                .any(|t| t.contains("all analysis slots were busy")),
            "{texts:?}"
        );
        assert_eq!(
            Metrics::counter(
                &metrics.analyses,
                &AgentResultLabels {
                    agent: "alert-triage-agent".into(),
                    result: "queue_timeout".into()
                }
            ),
            1
        );
    }

    #[tokio::test]
    async fn agent_deadline_is_reported() {
        let mut cfg = config();
        cfg.kagent_timeout = Duration::from_millis(50);
        let (g, slack, agent, _) = gateway(cfg);
        *agent.delay.lock().unwrap() = Duration::from_secs(5);
        post(&g, payload("A", "critical"), None).await;
        drain(&g).await;
        assert!(slack.posts()[1].text.contains("ran past its deadline"));
    }

    fn lookup_config() -> Config {
        let mut cfg = config();
        cfg.parent_mode = ParentMode::Lookup;
        cfg
    }

    #[tokio::test]
    async fn lookup_mode_only_posts_the_thread_reply() {
        let (g, slack, _, metrics) = gateway(lookup_config());
        post(&g, payload("A", "critical"), None).await;
        drain(&g).await;
        let calls = slack.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0],
            Call::Find {
                channel: "#alerts".into(),
                marker: "fp-1".into()
            }
        );
        assert!(
            matches!(&calls[1], Call::Post(m) if m.thread_ts == "1700.1" && m.title.is_empty())
        );
        assert_eq!(
            Metrics::counter(
                &metrics.parent_lookups,
                &ResultLabels {
                    result: "found".into()
                }
            ),
            1
        );

        // Nothing at all touches Slack when the alert is not analysed.
        post(&g, payload("W", "warning"), None).await;
        drain(&g).await;
        assert_eq!(slack.calls().len(), 2);
    }

    #[tokio::test]
    async fn lookup_mode_retries_late_notifications_and_falls_back() {
        let (g, slack, _, metrics) = gateway(lookup_config());
        slack.set_lookups(vec![
            Err(slack::Error::MessageNotFound),
            Err(slack::Error::MessageNotFound),
            Ok("42.0".into()),
        ]);
        post(&g, payload("A", "critical"), None).await;
        drain(&g).await;
        let finds = slack
            .calls()
            .iter()
            .filter(|c| matches!(c, Call::Find { .. }))
            .count();
        assert_eq!(finds, 3);
        assert_eq!(slack.posts()[0].thread_ts, "42.0");

        let (g, slack, _, metrics2) = gateway(lookup_config());
        slack.set_lookups((0..3).map(|_| Err(slack::Error::MessageNotFound)).collect());
        post(&g, payload("A", "critical"), None).await;
        drain(&g).await;
        let orphan = &slack.posts()[0];
        assert!(orphan.thread_ts.is_empty());
        assert_eq!(orphan.title, "🚨 [FIRING] A");
        assert_eq!(orphan.color, "danger");
        assert_eq!(
            Metrics::counter(
                &metrics2.slack_messages,
                &KindResultLabels {
                    kind: "orphan".into(),
                    result: "attempted".into()
                }
            ),
            1
        );
        assert_eq!(
            Metrics::counter(
                &metrics2.parent_lookups,
                &ResultLabels {
                    result: "not_found".into()
                }
            ),
            1
        );
        drop(metrics);
    }

    #[tokio::test]
    async fn lookup_mode_does_not_retry_permanent_errors() {
        let (g, slack, _, metrics) = gateway(lookup_config());
        slack.set_lookups(vec![Err(slack::Error::Other(anyhow::anyhow!(
            "missing_scope"
        )))]);
        post(&g, payload("A", "critical"), None).await;
        drain(&g).await;
        assert_eq!(
            slack
                .calls()
                .iter()
                .filter(|c| matches!(c, Call::Find { .. }))
                .count(),
            1
        );
        assert_eq!(
            Metrics::counter(
                &metrics.parent_lookups,
                &ResultLabels {
                    result: "error".into()
                }
            ),
            1
        );
        assert!(slack.posts()[0].thread_ts.is_empty());
    }

    #[tokio::test]
    async fn lookup_mode_threads_failure_notices_and_uses_group_key_marker() {
        let (g, slack, agent, _) = gateway(lookup_config());
        *agent.error.lock().unwrap() = Some("the agent stopped in state \"failed\"".into());
        let mut p = payload("A", "critical");
        p["alerts"][0]["fingerprint"] = json!("");
        post(&g, p, None).await;
        drain(&g).await;
        let calls = slack.calls();
        assert!(matches!(&calls[0], Call::Find { marker, .. } if marker == "{}:{alertname=\"A\"}"));
        assert!(
            matches!(&calls[1], Call::Post(m) if m.thread_ts == "1700.1" && m.text.starts_with(":warning: Automated analysis failed."))
        );

        let (g, slack, _, _) = gateway(lookup_config());
        let mut p = payload("A", "critical");
        p["alerts"][0]["fingerprint"] = json!("");
        p["groupKey"] = json!("");
        post(&g, p, None).await;
        drain(&g).await;
        assert!(
            slack
                .calls()
                .iter()
                .all(|c| !matches!(c, Call::Find { .. }))
        );
        assert!(slack.posts()[0].thread_ts.is_empty());
    }

    #[tokio::test]
    async fn truncation_is_counted() {
        let mut cfg = config();
        cfg.slack_max_text_chars = 500;
        let (g, slack, agent, metrics) = gateway(cfg);
        *agent.reply.lock().unwrap() = Some(crate::a2a::client::Reply {
            text: "x".repeat(600),
            ..Default::default()
        });
        post(&g, payload("A", "critical"), None).await;
        drain(&g).await;
        assert!(slack.posts()[1].text.ends_with("_(truncated)_"));
        assert_eq!(
            Metrics::counter(
                &metrics.slack_truncations,
                &KindLabels {
                    kind: "thread".into()
                }
            ),
            1
        );
    }

    #[tokio::test]
    async fn individual_alerts_are_counted() {
        let (g, _, _, metrics) = gateway(config());
        let mut p = payload("A", "critical");
        p["alerts"]
            .as_array_mut()
            .unwrap()
            .push(json!({"labels": {"severity": "warning"}}));
        p["alerts"]
            .as_array_mut()
            .unwrap()
            .push(json!({"status": "resolved", "labels": {}}));
        post(&g, p, None).await;
        drain(&g).await;
        let count = |sev: &str, status: &str| {
            Metrics::counter(
                &metrics.alerts_received,
                &SeverityStatusLabels {
                    severity: sev.into(),
                    status: status.into(),
                },
            )
        };
        assert_eq!(count("critical", "firing"), 1);
        assert_eq!(count("warning", "firing"), 1);
        assert_eq!(count("critical", "resolved"), 1);
    }
}
