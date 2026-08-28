//! The mention path: a question asked in a Slack thread runs the agent and is
//! answered in that thread.

use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use tokio::time::{Instant, sleep, timeout, timeout_at};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, info_span, warn};

use super::{Gateway, GaugeGuard, korean_duration};
use crate::a2a::{Progress, Request};
use crate::observability::Metrics;
use crate::slack::socket::{Event, Handler};
use crate::slack::{self, Message, SlackClient};

/// Marks the mention while the agent works and is removed when the answer
/// lands. Unlike the alert reactions it is not configurable: the status
/// message already carries the state in words, so the emoji is only there to
/// mark the message somebody has scrolled past, and one fixed choice is one
/// less thing to keep consistent across deployments. It makes
/// `reactions:write` a requirement wherever mentions are enabled.
const WORKING_REACTION: &str = "eyes";

/// Bounds the Slack calls a finished turn still owes.
const FINISH_TIMEOUT: Duration = Duration::from_secs(60);
const REACTION_TIMEOUT: Duration = Duration::from_secs(30);
const STATUS_UPDATE_TIMEOUT: Duration = Duration::from_secs(15);

/// Matches the leading user mention, which Slack renders as a link rather
/// than as the handle people typed. The anchor is what keeps this to the
/// leading one: a mention further in the text names somebody the question is
/// about, and stripping it would turn "did @younsl deploy this?" into a
/// question about nobody.
///
/// It matches on the link form rather than on a handle, so the bot's display
/// name is Slack's business alone and renaming the app changes nothing here.
static MENTION_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*<@[A-Z0-9]+(\|[^>]*)?>").expect("mention regex"));

/// Headings that shape a mention prompt. They are deliberately plain: the
/// alert is quoted material somebody else wrote, not an instruction, and the
/// heading is what tells the agent so.
const ALERT_HEADING: &str = "[Alert this question was asked under]";
const CONTEXT_HEADING: &str = "[Slack context]";
const QUESTION_HEADING: &str = "[Question]";

/// Caps the quoted alert. An Alertmanager notification is a few hundred
/// characters; anything past this is a template that got away, and truncating
/// it is better than letting it crowd out the question. It is not
/// configurable because there is nothing an operator would tune it to.
const PARENT_MAX: usize = 2000;

#[async_trait]
impl Handler for Arc<Gateway> {
    /// Routes one Socket Mode event. It runs on the read loop, so everything
    /// past the gating happens on a task of its own.
    async fn handle_event(&self, ev: Event) {
        debug!(
            kind = ev.kind,
            channel = ev.channel_id,
            envelope_id = ev.envelope_id,
            raw = ev.raw,
            "socket event received"
        );

        let text = match self.mention_text(&ev).await {
            Ok(text) => text,
            Err(reason) => {
                self.metrics.chat_event(reason);
                self.hint(&ev, reason).await;
                debug!(
                    reason,
                    channel = ev.channel_id,
                    user = ev.user,
                    "dropped mention"
                );
                return;
            }
        };

        let agent = self.resolve_chat_agent(&ev.channel_id).await;
        self.metrics.chat_event("accepted");
        let span = info_span!(
            "mention",
            channel = ev.channel_id,
            thread_ts = ev.thread_ts,
            user = ev.user,
            agent
        );
        let g = self.clone();
        self.tracker
            .spawn(async move { g.handle_mention(ev, text, agent).await }.instrument(span));
    }
}

/// The last task state the agent reported. It is written from the polling
/// task and read by the status ticker.
type ProgressState = Arc<Mutex<String>>;

impl Gateway {
    /// Applies the drop rules and returns the question with the leading
    /// mention stripped. The error is the drop reason, and doubles as the
    /// metric label so every drop is visible without a log line.
    async fn mention_text(&self, ev: &Event) -> Result<String, &'static str> {
        let bot_user_id = self.bot_user_id();
        if ev.kind != "app_mention" {
            return Err("subtype");
        }
        // The gateway posts into the same channels it listens to, so its own
        // messages must never start a turn. A post by the app carries bot_id
        // even when the mention path never sent it.
        if !ev.bot_id.is_empty() || (!bot_user_id.is_empty() && ev.user == bot_user_id) {
            return Err("bot");
        }
        // thread_broadcast is a thread reply somebody chose to also send to
        // the channel, which is a real mention. Every other subtype is an
        // edit, a deletion, or a join.
        if !ev.subtype.is_empty() && ev.subtype != "thread_broadcast" {
            return Err("subtype");
        }
        // No DM scope is requested and a DM arrives as message.im rather than
        // app_mention, so this is unreachable today. It guards against a later
        // scope change opening DMs by omission, since an empty channel allow
        // list means every channel.
        if ev.channel_type == "im" || ev.channel_type == "mpim" || ev.channel_id.starts_with('D') {
            return Err("dm");
        }
        if !self.cfg.chat_allowed_users.is_empty()
            && !self.cfg.chat_allowed_users.contains(&ev.user)
        {
            return Err("user_denied");
        }
        if ev.thread_ts.is_empty() {
            return Err("not_in_thread");
        }
        if !self
            .envelopes
            .lock()
            .expect("envelope store lock")
            .allow(&ev.envelope_id, Instant::now())
        {
            return Err("duplicate");
        }
        if !self.chat_channel_allowed(&ev.channel_id).await {
            return Err("channel_denied");
        }
        let text = Self::question(&ev.text, &bot_user_id);
        if text.is_empty() {
            return Err("empty");
        }
        Ok(text)
    }

    /// Turns the mention text into the prompt: the handle that addressed the
    /// bot is dropped, and every other mention is kept because it names
    /// somebody the question is about.
    ///
    /// A trailing "look at this @bot" is stripped too, which the leading
    /// pattern alone cannot do. That one needs the bot's own id, so it only
    /// applies when `auth.test` resolved it.
    fn question(text: &str, bot_user_id: &str) -> String {
        let mut text = MENTION_PATTERN.replace(text, " ").into_owned();
        if !bot_user_id.is_empty() {
            text = text.replace(&format!("<@{bot_user_id}>"), " ");
        }
        text.trim().to_string()
    }

    /// Answers the two drops a person has no way to tell from an outage. The
    /// note is ephemeral, so the rule is discoverable without leaving anything
    /// in channel history, and it costs no agent run.
    async fn hint(&self, ev: &Event, reason: &str) {
        let text = match reason {
            "not_in_thread" => &self.cfg.chat_thread_hint,
            // The hint says only that this channel is not served. Naming the
            // channels that are would turn the bot into a directory of where
            // it may be used.
            "channel_denied" => &self.cfg.chat_denied_hint,
            _ => return,
        };
        if text.is_empty() || ev.user.is_empty() {
            return;
        }
        // The event's own thread is used when there is one, so a denied
        // mention inside a thread is answered where it was asked.
        match self
            .slack
            .post_ephemeral(&ev.channel_id, &ev.thread_ts, &ev.user, text)
            .await
        {
            Ok(()) => self.metrics.slack_message("hint", "ok"),
            Err(err) => {
                self.metrics.slack_message("hint", "error");
                warn!(reason, error = %err, "failed to post mention hint");
            }
        }
    }

    /// Runs one turn: a status message goes up straight away, the agent runs
    /// against the thread's session, and the status message is rewritten with
    /// the answer.
    #[allow(clippy::too_many_lines, clippy::significant_drop_tightening)]
    async fn handle_mention(self: Arc<Self>, ev: Event, text: String, agent: String) {
        let deadline = Instant::now() + self.cfg.chat_timeout;
        let started = Instant::now();

        // The status message is posted before the slot is taken, so a mention
        // is acknowledged in the thread even while every slot is busy. It is
        // rewritten in place from here on, which keeps one message per turn
        // instead of one per state change.
        let status = Arc::new(StatusMessage {
            slack: self.slack.clone(),
            metrics: self.metrics.clone(),
            channel: ev.channel_id.clone(),
            thread: ev.thread_ts.clone(),
            started,
            interval: self.cfg.chat_status_interval,
            ts: Mutex::new(None),
        });
        status.post(&queued_status()).await;

        let permit = timeout_at(deadline, self.chat_sem.clone().acquire_owned()).await;
        let Ok(Ok(_permit)) = permit else {
            self.metrics.chat_turn(&agent, "queue_timeout");
            error!(timeout = ?self.cfg.chat_timeout, "gave up waiting for a chat slot");
            status
                .finish(":warning: 답변을 시작하지 못했습니다: 대화 슬롯이 모두 사용 중입니다.")
                .await;
            self.metrics
                .chat_turn_duration
                .observe(started.elapsed().as_secs_f64());
            return;
        };

        self.metrics.chat_inflight.inc();
        let _inflight = GaugeGuard(&self.metrics.chat_inflight);

        if let Err(err) = self
            .slack
            .add_reaction(&ev.channel_id, &ev.ts, WORKING_REACTION)
            .await
        {
            warn!(error = %err, "failed to add working reaction");
        }

        let session_key = format!("{}/{}", ev.channel_id, ev.thread_ts);
        let context_id = self
            .sessions
            .lock()
            .expect("session store lock")
            .get(&session_key, Instant::now());

        // The alert is sent on every turn, not just the first: it is a few
        // hundred characters, and a session that has it already loses nothing
        // by seeing it again, while a session that lost it would answer about
        // nothing.
        let alert_block = self.alert_context(&ev).await;
        let with_alert = alert_block.is_some();
        let prompt = alert_block.map_or_else(
            || self.chat_prompt(&text),
            |block| self.chat_prompt(&format!("{block}\n\n{QUESTION_HEADING}\n{text}")),
        );

        // The agent's own state drives the status line, and a ticker does the
        // Slack call: the progress hook runs on the polling task, where a
        // blocking write would stall the poll it is reporting on.
        let state: ProgressState = Arc::default();
        let tracker = status
            .clone()
            .track(state.clone(), agent.clone(), with_alert);
        let hook_state = state.clone();
        let hook: crate::a2a::ProgressHook =
            Arc::new(move |p: Progress| *hook_state.lock().expect("progress state lock") = p.state);

        info!(
            context_id = context_id.as_deref().unwrap_or(""),
            chars = text.chars().count(),
            "requesting chat turn"
        );
        let req = Request {
            agent: agent.clone(),
            text: prompt,
            context_id: context_id.clone(),
            on_progress: Some(hook),
        };
        let result = self.agent.send(req, deadline).await;
        tracker.stop().await;
        let elapsed = started.elapsed();

        match result {
            Ok(reply) => {
                {
                    let mut sessions = self.sessions.lock().expect("session store lock");
                    sessions.put(&session_key, &reply.context_id, Instant::now());
                    self.metrics
                        .chat_sessions
                        .set(i64::try_from(sessions.len()).unwrap_or(i64::MAX));
                }
                self.metrics.chat_turn(&agent, "ok");
                info!(
                    duration = ?elapsed,
                    task_id = reply.task_id,
                    context_id = reply.context_id,
                    chars = reply.text.chars().count(),
                    "chat turn completed"
                );
                status.finish(&self.truncate("chat", &reply.text)).await;
            }
            Err(err) => {
                self.metrics.chat_turn(&agent, "error");
                error!(error = %err, duration = ?elapsed, "chat turn failed");
                status
                    .finish(&format!(
                        ":warning: 답변을 만들지 못했습니다. {}",
                        err.user_message()
                    ))
                    .await;
            }
        }

        // The removal runs on its own budget: by the time it runs, the turn's
        // deadline has often been consumed by the agent call the reaction was
        // covering.
        let removal = self
            .slack
            .remove_reaction(&ev.channel_id, &ev.ts, WORKING_REACTION);
        match timeout(REACTION_TIMEOUT, removal).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => warn!(error = %err, "failed to remove working reaction"),
            Err(_) => warn!("failed to remove working reaction: timed out"),
        }
        self.metrics
            .chat_turn_duration
            .observe(started.elapsed().as_secs_f64());
    }

    /// Renders what a mention carries besides the question: the alert the
    /// thread hangs from, and the identifiers an agent needs to read the rest
    /// of the thread through its own Slack tools.
    ///
    /// Only the parent is sent. Everything else in the thread is the agent's
    /// to fetch, which keeps the gateway out of the business of guessing how
    /// much of a conversation a question needs. The parent is the one
    /// exception because a question about an alert is unanswerable without
    /// it, and an agent that has to discover it through a tool call may simply
    /// not make one.
    ///
    /// A failure is not fatal: the turn continues with the question alone.
    async fn alert_context(&self, ev: &Event) -> Option<String> {
        let parent = match self
            .slack
            .thread_parent(&ev.channel_id, &ev.thread_ts)
            .await
        {
            Ok(parent) => parent,
            Err(err) => {
                warn!(error = %err, "failed to read the thread parent, answering the question alone");
                return None;
            }
        };
        if parent.text.is_empty() {
            return None;
        }
        info!(
            chars = parent.text.chars().count(),
            "read the thread parent"
        );
        Some(format!(
            "{ALERT_HEADING}\n{}\n\n{CONTEXT_HEADING}\nchannel_id: {}\nthread_ts: {}",
            slack::truncate(&parent.text, PARENT_MAX),
            ev.channel_id,
            ev.thread_ts
        ))
    }

    /// Appends the chat instructions to the question. They are separate from
    /// the analysis instructions because a question has no alert sections to
    /// fill.
    fn chat_prompt(&self, text: &str) -> String {
        if self.cfg.chat_instructions.is_empty() {
            return text.to_string();
        }
        format!("{text}\n\n{}", self.cfg.chat_instructions)
    }

    /// Reports whether the channel may invoke the bot. An empty allow list
    /// allows every channel the bot is a member of, which Slack already
    /// bounds: an `app_mention` only arrives from a channel the bot was
    /// invited to.
    ///
    /// The allow list holds names or IDs while the event only carries an ID,
    /// so a name is resolved through the Slack client, whose channel cache
    /// makes the repeat lookups free.
    async fn chat_channel_allowed(&self, channel_id: &str) -> bool {
        if self.cfg.chat_channels.is_empty() {
            return true;
        }
        for entry in &self.cfg.chat_channels {
            if self.chat_channel_id(entry).await.as_deref() == Some(channel_id) {
                return true;
            }
        }
        false
    }

    /// Picks the agent that answers in a channel. Unlike the alert path's
    /// routing there is no label to read, so the table is keyed by channel.
    async fn resolve_chat_agent(&self, channel_id: &str) -> String {
        for (entry, agent) in &self.cfg.chat_agent_map {
            if self.chat_channel_id(entry).await.as_deref() == Some(channel_id) {
                return agent.clone();
            }
        }
        self.cfg.chat_agent.clone()
    }

    /// Maps a configured channel name or ID to its conversation ID, `None`
    /// when it cannot be resolved. A misspelt entry must not match anything,
    /// so a failure is a non-match rather than an error.
    async fn chat_channel_id(&self, entry: &str) -> Option<String> {
        match self.slack.resolve_channel_id(entry).await {
            Ok(id) => Some(id),
            Err(err) => {
                warn!(channel = entry, error = %err, "failed to resolve configured chat channel");
                None
            }
        }
    }
}

/// The single thread reply a turn owns. It starts as a "queued" note, is
/// rewritten while the agent works, and ends as the answer itself, so the
/// asker watches one message rather than a thread of progress notes.
struct StatusMessage {
    slack: Arc<dyn SlackClient>,
    metrics: Arc<Metrics>,
    channel: String,
    thread: String,
    started: Instant,
    /// How often the line is rewritten while the agent works. Zero leaves the
    /// first status standing until the answer replaces it.
    interval: Duration,
    /// The message being rewritten, `None` when the initial post failed.
    ts: Mutex<Option<String>>,
}

/// Stops the status ticker.
struct StatusTracker {
    token: CancellationToken,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl StatusTracker {
    async fn stop(self) {
        self.token.cancel();
        if let Some(handle) = self.handle {
            let _ = handle.await;
        }
    }
}

impl StatusMessage {
    fn ts(&self) -> Option<String> {
        self.ts.lock().expect("status ts lock").clone()
    }

    /// Publishes the first status line. A failure is not fatal: the turn
    /// still runs, and its answer is posted as a new message at the end.
    async fn post(&self, text: &str) {
        let msg = Message {
            channel: self.channel.clone(),
            thread_ts: self.thread.clone(),
            text: text.to_string(),
            ..Message::default()
        };
        match self.slack.post(msg).await {
            Ok(ts) => {
                self.metrics.slack_message("status", "ok");
                *self.ts.lock().expect("status ts lock") = Some(ts);
            }
            Err(err) => {
                self.metrics.slack_message("status", "error");
                warn!(error = %err, "failed to post chat status");
            }
        }
    }

    /// Rewrites the status line in place. Nothing is posted when the initial
    /// status message never landed, because a status without a message to
    /// replace would turn every refresh into a new thread reply.
    async fn set(&self, text: &str) {
        let Some(ts) = self.ts() else { return };
        if let Err(err) = self.slack.update(&self.channel, &ts, text).await {
            warn!(error = %err, "failed to update chat status");
        }
    }

    /// Replaces the status line with the final text, which is the answer or
    /// the reason there is none. It runs on its own budget: the turn's
    /// deadline is usually what brought it here.
    async fn finish(&self, text: &str) {
        let deadline = Instant::now() + FINISH_TIMEOUT;
        if let Some(ts) = self.ts() {
            match timeout_at(deadline, self.slack.update(&self.channel, &ts, text)).await {
                Ok(Ok(())) => {
                    self.metrics.slack_message("chat", "ok");
                    return;
                }
                Ok(Err(err)) => {
                    warn!(error = %err, "failed to rewrite chat status with the answer, posting it instead");
                }
                Err(_) => warn!(
                    "failed to rewrite chat status with the answer: timed out, posting it instead"
                ),
            }
        }
        // The turn is already paid for, so the answer is posted as a new
        // message rather than discarded when the status message cannot carry
        // it.
        let msg = Message {
            channel: self.channel.clone(),
            thread_ts: self.thread.clone(),
            text: text.to_string(),
            ..Message::default()
        };
        match timeout_at(deadline, self.slack.post(msg)).await {
            Ok(Ok(_)) => self.metrics.slack_message("chat", "ok"),
            Ok(Err(err)) => {
                self.metrics.slack_message("chat", "error");
                error!(error = %err, "failed to post chat reply");
            }
            Err(_) => {
                self.metrics.slack_message("chat", "error");
                error!("failed to post chat reply: timed out");
            }
        }
    }

    /// Refreshes the status line on a ticker until the tracker is stopped. The
    /// ticker owns the Slack call so the agent's polling task never waits on
    /// one.
    fn track(
        self: Arc<Self>,
        state: ProgressState,
        agent: String,
        with_alert: bool,
    ) -> StatusTracker {
        let token = CancellationToken::new();
        if self.interval.is_zero() {
            return StatusTracker {
                token,
                handle: None,
            };
        }
        let child = token.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = child.cancelled() => return,
                    () = sleep(self.interval) => {}
                }
                let current = state.lock().expect("progress state lock").clone();
                let text = working_status(&agent, self.started.elapsed(), &current, with_alert);
                if timeout(STATUS_UPDATE_TIMEOUT, self.set(&text))
                    .await
                    .is_err()
                {
                    warn!("chat status update timed out");
                }
            }
        });
        StatusTracker {
            token,
            handle: Some(handle),
        }
    }
}

/// The note posted the moment a mention is accepted, before a slot is even
/// held, so the asker sees the bot took the question.
fn queued_status() -> String {
    ":hourglass_flowing_sand: 질문을 받았습니다. 실행 슬롯이 비는 대로 확인을 시작합니다."
        .to_string()
}

/// Renders the live status line: what the agent is doing and how long it has
/// been at it, so a turn that takes minutes never looks like a bot that
/// stopped answering.
///
/// The poll count the progress hook also carries is deliberately absent. It
/// is the elapsed time divided by `KAGENT_POLL_INTERVAL`, so it repeats a
/// number the line already shows, and the one case where it diverges, a
/// controller that stopped answering, is what `agent_requests_total` reports.
fn working_status(agent: &str, elapsed: Duration, state: &str, with_alert: bool) -> String {
    format!(
        ":mag: kagent의 {agent} 에이전트가 {} (경과 {})",
        state_phrase(state, with_alert),
        korean_duration(elapsed)
    )
}

/// Turns an A2A task state into the sentence it belongs in. The raw state is
/// a protocol token, not something a reader in Slack can act on, so only an
/// unrecognised one is shown verbatim, where it is a debugging aid rather
/// than noise.
///
/// `with_alert` changes the sentence rather than adding one after it. A
/// second sentence would read as something the agent did, and reading the
/// alert is the gateway's own doing.
fn state_phrase(state: &str, with_alert: bool) -> String {
    match (state, with_alert) {
        ("" | "submitted", true) => "이 스레드의 알람으로 작업을 시작했습니다.".to_string(),
        ("" | "submitted", false) => "작업을 시작했습니다.".to_string(),
        ("working", true) => "이 스레드의 알람을 확인 중입니다.".to_string(),
        ("working", false) => "확인 중입니다.".to_string(),
        (other, _) => format!("확인 중입니다. (상태 {other})"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{Call, FakeSlack, config, gateway};
    use super::*;
    use crate::config::Config;
    use crate::observability::metrics::{AgentResultLabels, KindResultLabels, ResultLabels};
    use crate::slack::client::ThreadMessage;

    fn chat_config() -> Config {
        let mut cfg = config();
        cfg.slack_app_token = "xapp".into();
        cfg.chat_status_interval = Duration::from_millis(20);
        cfg
    }

    fn mention(text: &str) -> Event {
        Event {
            envelope_id: format!("env-{}", rand::random::<u32>()),
            kind: "app_mention".into(),
            channel_id: "C1".into(),
            user: "U1".into(),
            text: text.into(),
            ts: "2.0".into(),
            thread_ts: "1.0".into(),
            ..Event::default()
        }
    }

    async fn run(g: &Arc<Gateway>, ev: Event) {
        g.handle_event(ev).await;
        assert!(g.wait(Duration::from_secs(5)).await, "turn still running");
    }

    fn updates(slack: &FakeSlack) -> Vec<String> {
        slack
            .calls()
            .into_iter()
            .filter_map(|c| {
                if let Call::Update { text, .. } = c {
                    Some(text)
                } else {
                    None
                }
            })
            .collect()
    }

    fn chat_events(m: &Metrics, result: &str) -> u64 {
        Metrics::counter(
            &m.chat_events,
            &ResultLabels {
                result: result.into(),
            },
        )
    }

    #[tokio::test]
    async fn mention_answers_in_thread_with_the_alert() {
        let (g, slack, agent, metrics) = gateway(chat_config());
        g.set_bot_user_id("UBOT");
        run(&g, mention("<@UBOT> why did this fire?")).await;

        let calls = slack.calls();
        assert!(
            matches!(&calls[0], Call::Post(m) if m.channel == "C1" && m.thread_ts == "1.0" && m.text == queued_status())
        );
        assert!(
            matches!(&calls[1], Call::AddReaction { ts, name, .. } if ts == "2.0" && name == "eyes")
        );
        assert!(
            matches!(&calls[2], Call::ThreadParent { channel, thread_ts } if channel == "C1" && thread_ts == "1.0")
        );
        assert!(
            matches!(&calls[3], Call::Update { ts, text, .. } if ts == "1.0" && text == "*Summary* all good")
        );
        assert!(matches!(&calls[4], Call::RemoveReaction { name, .. } if name == "eyes"));

        let reqs = agent.requests();
        assert_eq!(reqs.len(), 1);
        let prompt = &reqs[0].text;
        assert!(prompt.starts_with(ALERT_HEADING), "{prompt}");
        assert!(prompt.contains("alert-id fp-1"));
        assert!(prompt.contains("channel_id: C1\nthread_ts: 1.0"));
        assert!(prompt.contains(&format!("{QUESTION_HEADING}\nwhy did this fire?")));
        assert!(prompt.ends_with(&chat_config().chat_instructions));
        assert_eq!(reqs[0].context_id, None);
        assert!(reqs[0].on_progress.is_some());

        assert_eq!(chat_events(&metrics, "accepted"), 1);
        assert_eq!(
            Metrics::counter(
                &metrics.chat_turns,
                &AgentResultLabels {
                    agent: "alert-triage-agent".into(),
                    result: "ok".into()
                }
            ),
            1
        );
        assert_eq!(
            Metrics::counter(
                &metrics.slack_messages,
                &KindResultLabels {
                    kind: "status".into(),
                    result: "ok".into()
                }
            ),
            1
        );
        assert_eq!(
            Metrics::counter(
                &metrics.slack_messages,
                &KindResultLabels {
                    kind: "chat".into(),
                    result: "ok".into()
                }
            ),
            1
        );
        assert_eq!(metrics.chat_sessions.get(), 1);
        assert_eq!(metrics.chat_slots.get(), 2);
        assert_eq!(metrics.chat_inflight.get(), 0);
    }

    #[tokio::test]
    async fn mention_answers_alone_when_the_thread_cannot_be_read() {
        let (g, slack, agent, _) = gateway(chat_config());
        *slack.parent.lock().unwrap() = Some(Err(slack::Error::MessageNotFound));
        run(&g, mention("<@UBOT> hello")).await;
        let prompt = &agent.requests()[0].text;
        assert!(prompt.starts_with("hello\n\n"), "{prompt}");
        assert!(!prompt.contains(ALERT_HEADING));

        let (g, slack, agent, _) = gateway(chat_config());
        *slack.parent.lock().unwrap() = Some(Ok(ThreadMessage::default()));
        run(&g, mention("<@UBOT> hello")).await;
        assert!(!agent.requests()[0].text.contains(ALERT_HEADING));
    }

    #[tokio::test]
    async fn question_keeps_inner_mentions_and_strips_the_trailing_handle() {
        let (g, _, agent, _) = gateway(chat_config());
        g.set_bot_user_id("UBOT");
        run(&g, mention("<@UBOT|kagent> did <@U2> deploy this? <@UBOT>")).await;
        let prompt = &agent.requests()[0].text;
        assert!(
            prompt.contains(&format!("{QUESTION_HEADING}\ndid <@U2> deploy this?")),
            "{prompt}"
        );
        assert_eq!(Gateway::question("<@UBOT>   ", "UBOT"), "");
        assert_eq!(Gateway::question("plain", ""), "plain");
    }

    #[tokio::test]
    async fn mention_reports_progress_while_the_agent_works() {
        let (g, slack, agent, _) = gateway(chat_config());
        *agent.delay.lock().unwrap() = Duration::from_millis(120);
        *agent.states.lock().unwrap() = vec!["submitted", "working"];
        run(&g, mention("<@UBOT> q")).await;
        let updates = updates(&slack);
        assert!(updates.len() >= 2, "{updates:?}");
        assert!(
            updates[0].contains("이 스레드의 알람을 확인 중입니다."),
            "{}",
            updates[0]
        );
        assert!(updates[0].starts_with(":mag: kagent의 alert-triage-agent 에이전트가"));
        assert_eq!(updates.last().unwrap(), "*Summary* all good");
    }

    #[tokio::test]
    async fn mention_continues_and_expires_sessions() {
        let (g, _, agent, _) = gateway(chat_config());
        run(&g, mention("<@UBOT> first")).await;
        run(&g, mention("<@UBOT> second")).await;
        let reqs = agent.requests();
        assert_eq!(reqs[0].context_id, None);
        assert_eq!(reqs[1].context_id, Some("ctx-1".into()));

        let mut cfg = chat_config();
        cfg.chat_session_ttl = Duration::ZERO;
        let (g, _, agent, metrics) = gateway(cfg);
        run(&g, mention("<@UBOT> first")).await;
        run(&g, mention("<@UBOT> second")).await;
        assert_eq!(agent.requests()[1].context_id, None);
        assert_eq!(metrics.chat_sessions.get(), 0);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn drop_rules() {
        let mut cfg = chat_config();
        cfg.chat_allowed_users.insert("U1".into());
        cfg.chat_channels = vec!["C1".into(), "dev".into()];
        let (g, slack, agent, metrics) = gateway(cfg);
        g.set_bot_user_id("UBOT");
        slack
            .channel_ids
            .lock()
            .unwrap()
            .insert("C1".into(), "C1".into());
        slack
            .channel_ids
            .lock()
            .unwrap()
            .insert("dev".into(), "C2".into());

        let cases: Vec<(&str, Event)> = vec![
            (
                "subtype",
                Event {
                    kind: "message".into(),
                    ..mention("x")
                },
            ),
            (
                "bot",
                Event {
                    bot_id: "B1".into(),
                    ..mention("x")
                },
            ),
            (
                "bot",
                Event {
                    user: "UBOT".into(),
                    ..mention("x")
                },
            ),
            (
                "subtype",
                Event {
                    subtype: "message_changed".into(),
                    ..mention("x")
                },
            ),
            (
                "dm",
                Event {
                    channel_type: "im".into(),
                    ..mention("x")
                },
            ),
            (
                "dm",
                Event {
                    channel_id: "D123".into(),
                    ..mention("x")
                },
            ),
            (
                "user_denied",
                Event {
                    user: "U9".into(),
                    ..mention("x")
                },
            ),
            (
                "not_in_thread",
                Event {
                    thread_ts: String::new(),
                    ..mention("x")
                },
            ),
            (
                "channel_denied",
                Event {
                    channel_id: "C3".into(),
                    ..mention("x")
                },
            ),
            ("empty", mention("<@UBOT>")),
        ];
        for (reason, ev) in cases {
            let before = chat_events(&metrics, reason);
            run(&g, ev).await;
            assert_eq!(chat_events(&metrics, reason), before + 1, "{reason}");
        }
        assert!(agent.requests().is_empty());
        assert_eq!(chat_events(&metrics, "accepted"), 0);

        // Hints for the two drops a person cannot tell from an outage.
        let hints: Vec<Call> = slack
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Ephemeral { .. }))
            .collect();
        assert_eq!(hints.len(), 2);
        assert!(
            matches!(&hints[0], Call::Ephemeral { thread_ts, text, user, .. } if thread_ts.is_empty() && text == crate::config::DEFAULT_THREAD_HINT && user == "U1")
        );
        assert!(
            matches!(&hints[1], Call::Ephemeral { channel, thread_ts, text, .. } if channel == "C3" && thread_ts == "1.0" && text == crate::config::DEFAULT_DENIED_HINT)
        );
        assert_eq!(
            Metrics::counter(
                &metrics.slack_messages,
                &KindResultLabels {
                    kind: "hint".into(),
                    result: "ok".into()
                }
            ),
            2
        );

        // Accepted shapes: a thread broadcast and a name-resolved channel.
        run(
            &g,
            Event {
                subtype: "thread_broadcast".into(),
                ..mention("<@UBOT> ok")
            },
        )
        .await;
        run(
            &g,
            Event {
                channel_id: "C2".into(),
                ..mention("<@UBOT> ok")
            },
        )
        .await;
        assert_eq!(agent.requests().len(), 2);
    }

    #[tokio::test]
    async fn redelivered_envelopes_are_ignored() {
        let (g, _, agent, metrics) = gateway(chat_config());
        let ev = mention("<@UBOT> once");
        run(&g, ev.clone()).await;
        run(&g, ev).await;
        assert_eq!(agent.requests().len(), 1);
        assert_eq!(chat_events(&metrics, "duplicate"), 1);
    }

    #[tokio::test]
    async fn hints_can_be_disabled() {
        let mut cfg = chat_config();
        cfg.chat_thread_hint = String::new();
        let (g, slack, _, _) = gateway(cfg);
        run(
            &g,
            Event {
                thread_ts: String::new(),
                ..mention("x")
            },
        )
        .await;
        run(
            &g,
            Event {
                thread_ts: String::new(),
                user: String::new(),
                ..mention("x")
            },
        )
        .await;
        assert!(slack.calls().is_empty());
    }

    #[tokio::test]
    async fn channel_routes_to_its_own_agent() {
        let mut cfg = chat_config();
        cfg.chat_agent_map.insert("ops".into(), "ops-agent".into());
        cfg.chat_agent_map.insert("typo".into(), "never".into());
        let (g, slack, agent, _) = gateway(cfg);
        slack
            .channel_ids
            .lock()
            .unwrap()
            .insert("ops".into(), "C1".into());
        run(&g, mention("<@UBOT> q")).await;
        run(
            &g,
            Event {
                channel_id: "C7".into(),
                ..mention("<@UBOT> q")
            },
        )
        .await;
        let agents: Vec<String> = agent.requests().into_iter().map(|r| r.agent).collect();
        assert_eq!(agents, vec!["ops-agent", "alert-triage-agent"]);
    }

    #[tokio::test]
    async fn agent_failure_is_reported_in_the_status_line() {
        let (g, slack, agent, metrics) = gateway(chat_config());
        *agent.error.lock().unwrap() = Some("the controller stopped responding".into());
        run(&g, mention("<@UBOT> q")).await;
        assert_eq!(
            updates(&slack).last().unwrap(),
            ":warning: 답변을 만들지 못했습니다. the controller stopped responding"
        );
        assert_eq!(
            Metrics::counter(
                &metrics.chat_turns,
                &AgentResultLabels {
                    agent: "alert-triage-agent".into(),
                    result: "error".into()
                }
            ),
            1
        );
    }

    #[tokio::test]
    async fn queue_timeout_is_reported() {
        let mut cfg = chat_config();
        cfg.max_concurrent_chats = 1;
        cfg.chat_timeout = Duration::from_millis(80);
        let (g, slack, agent, metrics) = gateway(cfg);
        *agent.delay.lock().unwrap() = Duration::from_millis(200);
        agent
            .ignore_deadline
            .store(true, std::sync::atomic::Ordering::SeqCst);
        g.handle_event(mention("<@UBOT> a")).await;
        g.handle_event(mention("<@UBOT> b")).await;
        assert!(g.wait(Duration::from_secs(5)).await);
        assert!(
            updates(&slack)
                .iter()
                .any(|t| t.contains("대화 슬롯이 모두 사용 중입니다"))
        );
        assert_eq!(
            Metrics::counter(
                &metrics.chat_turns,
                &AgentResultLabels {
                    agent: "alert-triage-agent".into(),
                    result: "queue_timeout".into()
                }
            ),
            1
        );
    }

    #[tokio::test]
    async fn answer_is_posted_when_the_status_cannot_be_updated() {
        let (g, slack, _, _) = gateway(chat_config());
        *slack.update_error.lock().unwrap() = Some("cant_update_message".into());
        run(&g, mention("<@UBOT> q")).await;
        let posts = slack.posts();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[1].text, "*Summary* all good");
        assert_eq!(posts[1].thread_ts, "1.0");

        let (g, slack, _, metrics) = gateway(chat_config());
        *slack.post_error.lock().unwrap() = Some("channel_not_found".into());
        run(&g, mention("<@UBOT> q")).await;
        assert!(updates(&slack).is_empty());
        assert_eq!(
            Metrics::counter(
                &metrics.slack_messages,
                &KindResultLabels {
                    kind: "chat".into(),
                    result: "error".into()
                }
            ),
            1
        );
    }

    #[tokio::test]
    async fn long_replies_are_truncated() {
        let mut cfg = chat_config();
        cfg.slack_max_text_chars = 500;
        let (g, slack, agent, _) = gateway(cfg);
        *agent.reply.lock().unwrap() = Some(crate::a2a::client::Reply {
            text: "y".repeat(700),
            context_id: "c".into(),
            ..Default::default()
        });
        run(&g, mention("<@UBOT> q")).await;
        let last = updates(&slack).pop().unwrap();
        assert_eq!(last.chars().count(), 500);
        assert!(last.ends_with("_(truncated)_"));
    }

    #[test]
    fn working_status_reads_as_a_sentence() {
        let s = working_status("triage", Duration::from_secs(95), "working", true);
        assert_eq!(
            s,
            ":mag: kagent의 triage 에이전트가 이 스레드의 알람을 확인 중입니다. (경과 1분 35초)"
        );
        assert_eq!(state_phrase("", false), "작업을 시작했습니다.");
        assert_eq!(
            state_phrase("submitted", true),
            "이 스레드의 알람으로 작업을 시작했습니다."
        );
        assert_eq!(state_phrase("working", false), "확인 중입니다.");
        assert_eq!(
            state_phrase("input-required", false),
            "확인 중입니다. (상태 input-required)"
        );
    }

    #[tokio::test]
    async fn status_tracker_without_interval_is_inert() {
        let (g, slack, agent, _) = gateway(Config {
            chat_status_interval: Duration::ZERO,
            ..chat_config()
        });
        *agent.delay.lock().unwrap() = Duration::from_millis(50);
        run(&g, mention("<@UBOT> q")).await;
        assert_eq!(updates(&slack).len(), 1);
    }
}
