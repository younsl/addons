//! Talks to the Slack Web API with a bot token.
//!
//! The write calls are `chat.postMessage` to publish, `chat.update` to rewrite
//! a message the bot already posted, and `chat.postEphemeral` for a note only
//! one reader sees. The read calls are `conversations.list` plus
//! `conversations.history` to locate a message somebody else posted, and
//! `auth.test` for the bot's own user ID. The lookup exists because an
//! Alertmanager incoming webhook does not return the message timestamp that
//! `thread_ts` requires, so the only way to reply under an alert Alertmanager
//! posted is to find it again.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::anyhow;
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::time::{Instant, sleep};
use tracing::warn;

use crate::observability::Metrics;

const MAX_ATTEMPTS: u32 = 3;
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);
const DEFAULT_BACKOFF: Duration = Duration::from_secs(1);
/// Page size for `conversations.history`. Most lookups hit in the first page,
/// so it stays small; an alert storm that buries the notification deeper is
/// followed through cursor pagination instead of a bigger page.
const HISTORY_LIMIT: u32 = 50;
/// Bounds the pagination so a runaway channel cannot spin the search forever.
/// The `oldest` parameter usually ends the scan much earlier by cutting at
/// the lookup window.
const MAX_HISTORY_PAGES: u32 = 20;
/// Bounds how long a resolved name to ID mapping is reused. Channel renames
/// are rare, and a stale entry only costs one failed lookup.
const CHANNEL_CACHE_TTL: Duration = Duration::from_secs(3600);

/// Matches the Slack conversation ID format, which the API accepts as-is and
/// which must never be prefixed with a hash.
static CHANNEL_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[CGD][A-Z0-9]{6,}$").expect("channel id regex"));

/// A Slack API failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No message in the searched window carried the marker.
    #[error("no matching message found")]
    MessageNotFound,
    /// Slack pushed back with a rate limit, kept apart so the metrics can
    /// separate being told to slow down from actually failing.
    #[error("slack rate limited")]
    RateLimited,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Error {
    fn other(msg: impl Into<String>) -> Self {
        Self::Other(anyhow!(msg.into()))
    }

    /// Labels one API attempt for the metrics.
    const fn attempt_result(&self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            _ => "error",
        }
    }
}

/// One failed attempt: the error plus whether it is worth retrying. `None`
/// marks the failure permanent; `Some(Duration::ZERO)` retries after the
/// default backoff; anything else is the wait Slack asked for.
struct Attempt {
    err: Error,
    retry_after: Option<Duration>,
}

impl Attempt {
    const fn permanent(err: Error) -> Self {
        Self {
            err,
            retry_after: None,
        }
    }

    const fn transient(err: Error) -> Self {
        Self {
            err,
            retry_after: Some(Duration::ZERO),
        }
    }
}

/// Describes one `chat.postMessage` call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Message {
    pub channel: String,
    /// `title` and `color` render an attachment; leave both empty to post
    /// `text` as a plain message, which is what thread replies do.
    pub title: String,
    pub color: String,
    pub text: String,
    /// Threads the message under an existing parent when set.
    pub thread_ts: String,
}

/// One message in a thread, flattened to the text a prompt needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadMessage {
    pub ts: String,
    pub user: String,
    /// Set when an app posted the message, which is how an alert notification
    /// and a gateway reply are told from a person's own words.
    pub bot_id: String,
    pub text: String,
}

/// What the gateway needs from Slack: posting, locating a message to thread
/// under, managing reactions, and rewriting a status message in place.
#[async_trait]
pub trait SlackClient: Send + Sync {
    /// Sends the message and returns its timestamp, which is the `thread_ts`
    /// for any reply that should hang under it.
    async fn post(&self, msg: Message) -> Result<String, Error>;
    /// Rewrites a message the bot posted earlier. `channel` must be a
    /// conversation ID.
    async fn update(&self, channel: &str, ts: &str, text: &str) -> Result<(), Error>;
    /// Sends a message only `user` can see, leaving nothing in channel
    /// history.
    async fn post_ephemeral(
        &self,
        channel: &str,
        thread_ts: &str,
        user: &str,
        text: &str,
    ) -> Result<(), Error>;
    /// Finds the most recent message in `channel` that carries `marker` and is
    /// not older than `since`. `channel` accepts a name, a `#name`, or an ID.
    async fn find_thread_parent(
        &self,
        channel: &str,
        marker: &str,
        since: SystemTime,
    ) -> Result<String, Error>;
    /// Returns the message a thread hangs from. `channel` must be an ID.
    async fn thread_parent(&self, channel: &str, thread_ts: &str) -> Result<ThreadMessage, Error>;
    /// Puts an emoji reaction on the message at `ts`.
    async fn add_reaction(&self, channel: &str, ts: &str, name: &str) -> Result<(), Error>;
    /// Removes an emoji reaction the bot added earlier.
    async fn remove_reaction(&self, channel: &str, ts: &str, name: &str) -> Result<(), Error>;
    /// Maps a channel name to its conversation ID, returning an ID unchanged.
    async fn resolve_channel_id(&self, channel: &str) -> Result<String, Error>;
}

/// Calls the Slack Web API.
pub struct Client {
    http: reqwest::Client,
    api_url: String,
    token: String,
    metrics: Arc<Metrics>,
    backoff: Duration,
    cache_ttl: Duration,
    channels: Mutex<HashMap<String, (String, Instant)>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Envelope {
    ok: bool,
    error: String,
    ts: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Attachment {
    title: String,
    text: String,
    footer: String,
    fallback: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct HistoryMessage {
    ts: String,
    user: String,
    bot_id: String,
    text: String,
    attachments: Vec<Attachment>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ResponseMetadata {
    next_cursor: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct HistoryPage {
    messages: Vec<HistoryMessage>,
    has_more: bool,
    response_metadata: ResponseMetadata,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ChannelPage {
    channels: Vec<ChannelEntry>,
    response_metadata: ResponseMetadata,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ChannelEntry {
    id: String,
    name: String,
}

impl Client {
    /// Returns a client for the given API base URL (no trailing slash
    /// needed).
    ///
    /// # Panics
    ///
    /// Panics when the HTTP client cannot be built, which only happens when
    /// the TLS backend is unusable.
    #[must_use]
    pub fn new(api_url: &str, token: &str, timeout: Duration, metrics: Arc<Metrics>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("build http client"),
            api_url: api_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            metrics,
            backoff: DEFAULT_BACKOFF,
            cache_ttl: CHANNEL_CACHE_TTL,
            channels: Mutex::new(HashMap::new()),
        }
    }

    /// Overrides the base retry wait, which tests shorten.
    #[cfg(test)]
    pub const fn with_backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    /// Overrides the channel cache lifetime, which tests shorten.
    #[cfg(test)]
    pub const fn with_cache_ttl(mut self, ttl: Duration) -> Self {
        self.cache_ttl = ttl;
        self
    }

    /// Returns the bot's own user ID, which the mention path needs as its
    /// loop guard: the gateway posts into the channels it listens to. It needs
    /// no scope of its own.
    ///
    /// # Errors
    ///
    /// Returns an error when the call fails or Slack reports no identity.
    pub async fn auth_test(&self) -> Result<String, Error> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Identity {
            user_id: String,
        }
        let out: Identity = self.get("auth.test", &[]).await?;
        if out.user_id.is_empty() {
            return Err(Error::other("auth.test returned no user id"));
        }
        Ok(out.user_id)
    }

    /// Posts a JSON body to a write method and retries the transient
    /// failures, returning the message timestamp when the method reports one.
    async fn write(&self, method: &str, body: &Value) -> Result<String, Error> {
        let mut attempt = 1;
        loop {
            let started = Instant::now();
            let out = self.write_once(method, body).await;
            self.metrics.observe_slack_request(
                method,
                out.as_ref()
                    .map_or_else(|a| a.err.attempt_result(), |_| "ok"),
                started.elapsed(),
            );
            match out {
                Ok(ts) => return Ok(ts),
                Err(a) => {
                    let Some(wait) = a.retry_after.filter(|_| attempt < MAX_ATTEMPTS) else {
                        return Err(a.err);
                    };
                    let wait = if wait.is_zero() {
                        self.backoff * attempt
                    } else {
                        wait
                    };
                    warn!(method, attempt, wait = ?wait, error = %a.err, "retrying slack write");
                    sleep(wait).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn write_once(&self, method: &str, body: &Value) -> Result<String, Attempt> {
        let resp = self
            .http
            .post(format!("{}/{method}", self.api_url))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/json; charset=utf-8",
            )
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|err| Attempt::transient(Error::Other(anyhow!(err).context("call slack"))))?;
        let out = decode_response(resp).await?;
        Ok(out
            .get("ts")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    }

    /// Calls a read-only Web API method and decodes its payload. Reads sit on
    /// the parent-lookup path, where one unretried rate limit turns the
    /// analysis into an orphan channel message, so transient failures are
    /// retried the same way writes are.
    async fn get<T: DeserializeOwned>(
        &self,
        method: &str,
        query: &[(&str, String)],
    ) -> Result<T, Error> {
        let mut attempt = 1;
        loop {
            let started = Instant::now();
            let out = self.get_once(method, query).await;
            self.metrics.observe_slack_request(
                method,
                out.as_ref()
                    .map_or_else(|a| a.err.attempt_result(), |_| "ok"),
                started.elapsed(),
            );
            match out {
                Ok(value) => {
                    return serde_json::from_value(value)
                        .map_err(|err| Error::Other(anyhow!(err).context("decode response")));
                }
                Err(a) => {
                    let Some(wait) = a.retry_after.filter(|_| attempt < MAX_ATTEMPTS) else {
                        return Err(a.err);
                    };
                    let wait = if wait.is_zero() {
                        self.backoff * attempt
                    } else {
                        wait
                    };
                    warn!(method, attempt, wait = ?wait, error = %a.err, "retrying slack read");
                    sleep(wait).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn get_once(&self, method: &str, query: &[(&str, String)]) -> Result<Value, Attempt> {
        let resp = self
            .http
            .get(format!("{}/{method}", self.api_url))
            .query(query)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|err| Attempt::transient(Error::Other(anyhow!(err).context("call slack"))))?;
        decode_response(resp).await
    }

    /// Maps a channel name to its ID, which `conversations.history` requires.
    /// `chat.postMessage` accepts a name, that call does not.
    ///
    /// Public channels are searched first because that only needs
    /// `channels:read`. Private channels are searched second and only when the
    /// name was not public, so a bot that threads exclusively in public
    /// channels never needs `groups:read`.
    async fn resolve(&self, channel: &str) -> Result<String, Error> {
        let name = channel.trim().trim_start_matches('#');
        if name.is_empty() {
            return Err(Error::other("channel is empty"));
        }
        if CHANNEL_ID.is_match(name) {
            return Ok(name.to_string());
        }

        let cached = self
            .channels
            .lock()
            .expect("channel cache lock")
            .get(name)
            .cloned();
        if let Some((id, resolved)) = cached
            && resolved.elapsed() < self.cache_ttl
        {
            return Ok(id);
        }

        let mut found = self.find_channel(name, "public_channel").await;
        if matches!(found, Ok(None)) {
            found = self.find_channel(name, "private_channel").await;
        }
        let id = match found {
            Ok(Some(id)) => id,
            Ok(None) => {
                return Err(Error::other(format!(
                    "channel {channel:?} not found; the bot must be able to see it"
                )));
            }
            Err(err) if format!("{err:#}").contains("missing_scope") => {
                return Err(Error::Other(anyhow!(err).context(format!(
                    "channel {channel:?} is not a visible public channel, and listing private channels needs the groups:read scope"
                ))));
            }
            Err(err) => return Err(err),
        };

        self.channels
            .lock()
            .expect("channel cache lock")
            .insert(name.to_string(), (id.clone(), Instant::now()));
        Ok(id)
    }

    /// Sweeps `conversations.list` for the given channel types and returns
    /// the ID whose name matches.
    async fn find_channel(&self, name: &str, types: &str) -> Result<Option<String>, Error> {
        let mut cursor = String::new();
        // Bounded so a paging bug cannot spin forever.
        for _ in 0..20 {
            let mut query = vec![
                ("limit", "1000".to_string()),
                ("exclude_archived", "true".to_string()),
                ("types", types.to_string()),
            ];
            if !cursor.is_empty() {
                query.push(("cursor", cursor.clone()));
            }
            let page: ChannelPage = self
                .get("conversations.list", &query)
                .await
                .map_err(|err| context(err, "list channels"))?;
            if let Some(ch) = page.channels.iter().find(|ch| ch.name == name) {
                return Ok(Some(ch.id.clone()));
            }
            cursor = page.response_metadata.next_cursor;
            if cursor.is_empty() {
                break;
            }
        }
        Ok(None)
    }

    /// Resolves the channel and then calls the reaction method, recording only
    /// the reaction call: the channel resolution is a `conversations.list`
    /// call that records itself.
    async fn react(&self, method: &str, channel: &str, ts: &str, name: &str) -> Result<(), Error> {
        let id = self.resolve(channel).await?;
        let started = Instant::now();
        let out = self.react_once(method, &id, ts, name).await;
        self.metrics.observe_slack_request(
            method,
            if out.is_ok() { "ok" } else { "error" },
            started.elapsed(),
        );
        out
    }

    async fn react_once(&self, method: &str, id: &str, ts: &str, name: &str) -> Result<(), Error> {
        let body = json!({"channel": id, "timestamp": ts, "name": name.trim().trim_matches(':')});
        let resp = self
            .http
            .post(format!("{}/{method}", self.api_url))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/json; charset=utf-8",
            )
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|err| Error::Other(anyhow!(err).context("call slack")))?;
        if resp.status() != reqwest::StatusCode::OK {
            return Err(Error::other(format!(
                "slack returned HTTP {}",
                resp.status().as_u16()
            )));
        }
        let out: Envelope = resp
            .json()
            .await
            .map_err(|err| Error::Other(anyhow!(err).context("decode response")))?;
        // Both states describe the outcome the caller wanted, so a duplicate
        // add or a remove of something already gone is not a failure.
        if !out.ok && out.error != "already_reacted" && out.error != "no_reaction" {
            return Err(Error::other(format!("slack error: {}", out.error)));
        }
        Ok(())
    }
}

/// Reads one Web API response and classifies it: HTTP 429 and 5xx retry, so
/// do the `ok=false` codes Slack documents as transient; everything else is
/// permanent. Slack signals transient conditions through the error string,
/// not the status code.
async fn decode_response(resp: reqwest::Response) -> Result<Value, Attempt> {
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(parse_retry_after);
    let payload = resp
        .bytes()
        .await
        .map_err(|err| Attempt::transient(Error::Other(anyhow!(err).context("read response"))))?;
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(Attempt {
            err: Error::RateLimited,
            retry_after: Some(retry_after.unwrap_or(Duration::ZERO)),
        });
    }
    if status.is_server_error() {
        return Err(Attempt::transient(Error::other(format!(
            "slack returned HTTP {}",
            status.as_u16()
        ))));
    }
    if status != reqwest::StatusCode::OK {
        return Err(Attempt::permanent(Error::other(format!(
            "slack returned HTTP {}",
            status.as_u16()
        ))));
    }
    let value: Value = serde_json::from_slice(&payload)
        .map_err(|err| Attempt::permanent(Error::Other(anyhow!(err).context("decode response"))))?;
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        let code = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        return Err(match code.as_str() {
            "ratelimited" => Attempt::transient(Error::RateLimited),
            "service_unavailable" | "internal_error" => {
                Attempt::transient(Error::other(format!("slack error: {code}")))
            }
            _ => Attempt::permanent(Error::other(format!("slack error: {code}"))),
        });
    }
    Ok(value)
}

fn parse_retry_after(header: &str) -> Duration {
    header.trim().parse::<u64>().map_or(Duration::ZERO, |secs| {
        Duration::from_secs(secs).min(MAX_RETRY_WAIT)
    })
}

fn context(err: Error, msg: &'static str) -> Error {
    match err {
        Error::Other(inner) => Error::Other(inner.context(msg)),
        other => Error::Other(anyhow!(other).context(msg)),
    }
}

fn attachments_carry(att: &Attachment, marker: &str) -> bool {
    att.footer.contains(marker)
        || att.text.contains(marker)
        || att.title.contains(marker)
        || att.fallback.contains(marker)
}

#[async_trait]
impl SlackClient for Client {
    async fn post(&self, msg: Message) -> Result<String, Error> {
        let mut body = json!({"channel": msg.channel});
        if !msg.thread_ts.is_empty() {
            body["thread_ts"] = json!(msg.thread_ts);
        }
        if msg.title.is_empty() && msg.color.is_empty() {
            body["text"] = json!(msg.text);
        } else {
            // The title doubles as the notification text: Slack shows the
            // top-level text field in the sidebar and in push notifications,
            // and attachment content alone would leave both blank.
            body["text"] = json!(msg.title);
            body["attachments"] = json!([{
                "color": msg.color,
                "title": msg.title,
                "text": msg.text,
                "mrkdwn_in": ["text"],
            }]);
        }
        self.write("chat.postMessage", &body).await
    }

    /// The mention path posts a placeholder and rewrites it as the turn
    /// progresses, instead of filling the thread with one message per state
    /// change.
    async fn update(&self, channel: &str, ts: &str, text: &str) -> Result<(), Error> {
        self.write(
            "chat.update",
            &json!({"channel": channel, "ts": ts, "text": text}),
        )
        .await
        .map(|_| ())
    }

    /// Carries the hints that explain a dropped mention, so a rule the asker
    /// cannot otherwise distinguish from an outage stays discoverable without
    /// the channel paying for it.
    async fn post_ephemeral(
        &self,
        channel: &str,
        thread_ts: &str,
        user: &str,
        text: &str,
    ) -> Result<(), Error> {
        let mut body = json!({"channel": channel, "user": user, "text": text});
        if !thread_ts.is_empty() {
            body["thread_ts"] = json!(thread_ts);
        }
        self.write("chat.postEphemeral", &body).await.map(|_| ())
    }

    /// Alertmanager renders the marker into its Slack template, which is the
    /// only join key available: an incoming webhook tells nobody what it
    /// posted, and `slack_configs` cannot carry a hidden identifier because it
    /// has no access to `block_id`.
    async fn find_thread_parent(
        &self,
        channel: &str,
        marker: &str,
        since: SystemTime,
    ) -> Result<String, Error> {
        if marker.is_empty() {
            return Err(Error::other("marker is empty"));
        }
        let id = self.resolve(channel).await?;

        // An alert storm can push the notification past any single page, so
        // the scan follows the cursor until the window is exhausted.
        // conversations.history returns newest first, so the first hit is the
        // most recent notification for this alert group.
        let mut cursor = String::new();
        for _ in 0..MAX_HISTORY_PAGES {
            let mut query = vec![
                ("channel", id.clone()),
                ("limit", HISTORY_LIMIT.to_string()),
                ("inclusive", "true".to_string()),
            ];
            if let Ok(oldest) = since.duration_since(UNIX_EPOCH)
                && !oldest.is_zero()
            {
                query.push(("oldest", oldest.as_secs().to_string()));
            }
            if !cursor.is_empty() {
                query.push(("cursor", cursor.clone()));
            }
            let page: HistoryPage = self
                .get("conversations.history", &query)
                .await
                .map_err(|err| context(err, "read channel history"))?;
            for msg in &page.messages {
                if msg.text.contains(marker)
                    || msg
                        .attachments
                        .iter()
                        .any(|att| attachments_carry(att, marker))
                {
                    return Ok(msg.ts.clone());
                }
            }
            cursor = page.response_metadata.next_cursor;
            if !page.has_more || cursor.is_empty() {
                break;
            }
        }
        Err(Error::MessageNotFound)
    }

    /// For an alert thread the parent is the alert itself. It is the one
    /// piece of a thread the gateway sends on every mention: without it the
    /// agent is asked to analyse an event it has never seen, and everything
    /// else in the thread the agent can fetch for itself through its Slack
    /// tools. The call needs the same history scope the parent lookup uses.
    async fn thread_parent(&self, channel: &str, thread_ts: &str) -> Result<ThreadMessage, Error> {
        if thread_ts.is_empty() {
            return Err(Error::other("thread timestamp is empty"));
        }
        // conversations.replies returns the parent first, so one message is
        // all this needs to ask for.
        let query = [
            ("channel", channel.to_string()),
            ("ts", thread_ts.to_string()),
            ("limit", "1".to_string()),
        ];
        let page: HistoryPage = self
            .get("conversations.replies", &query)
            .await
            .map_err(|err| context(err, "read thread parent"))?;
        let Some(msg) = page.messages.into_iter().next() else {
            return Err(Error::MessageNotFound);
        };

        let mut parts: Vec<&str> = vec![msg.text.trim()];
        for att in &msg.attachments {
            // An Alertmanager notification carries the whole alert in its
            // attachment rather than in the message body. Fallback repeats
            // the title and text, so it is only used when those are empty.
            if att.title.is_empty() && att.text.is_empty() {
                parts.extend([att.fallback.trim(), att.footer.trim()]);
            } else {
                parts.extend([att.title.trim(), att.text.trim(), att.footer.trim()]);
            }
        }
        let text = parts
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ThreadMessage {
            ts: msg.ts,
            user: msg.user,
            bot_id: msg.bot_id,
            text,
        })
    }

    async fn add_reaction(&self, channel: &str, ts: &str, name: &str) -> Result<(), Error> {
        self.react("reactions.add", channel, ts, name).await
    }

    async fn remove_reaction(&self, channel: &str, ts: &str, name: &str) -> Result<(), Error> {
        self.react("reactions.remove", channel, ts, name).await
    }

    /// The mention path compares a configured allow list against the ID the
    /// event carries, which is the only form Slack sends.
    async fn resolve_channel_id(&self, channel: &str) -> Result<String, Error> {
        self.resolve(channel).await
    }
}

/// Prefixes a bare channel name with `#`, leaving an ID or an
/// already-prefixed name untouched.
#[must_use]
pub fn normalize_channel(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() || value.starts_with('#') || CHANNEL_ID.is_match(value) {
        return value.to_string();
    }
    format!("#{value}")
}

/// Shortens text to `max_chars`, appending a marker when it had to cut. Slack
/// rejects oversized messages outright, so a long agent reply must be
/// trimmed rather than dropped.
#[must_use]
pub fn truncate(text: &str, max_chars: usize) -> String {
    const MARKER: &str = "\n_(truncated)_";
    if max_chars == 0 || text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(MARKER.chars().count());
    format!("{}{MARKER}", text.chars().take(keep).collect::<String>())
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::observability::metrics::MethodResultLabels;

    fn client(server: &MockServer, metrics: Arc<Metrics>) -> Client {
        Client::new(&server.uri(), "xoxb-test", Duration::from_secs(5), metrics)
            .with_backoff(Duration::from_millis(1))
    }

    fn ok_ts(ts: &str) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .set_body_json(json!({"ok": true, "ts": ts, "channel": "C0000123"}))
    }

    fn requests(metrics: &Metrics, m: &str, result: &str) -> u64 {
        Metrics::counter(
            &metrics.slack_api_requests,
            &MethodResultLabels {
                method: m.into(),
                result: result.into(),
            },
        )
    }

    #[tokio::test]
    async fn posts_parent_with_attachment_and_thread_reply_without() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .and(header("authorization", "Bearer xoxb-test"))
            .respond_with(ok_ts("1700000000.000100"))
            .mount(&server)
            .await;
        let c = client(&server, Arc::new(Metrics::new()));
        let ts = c
            .post(Message {
                channel: "#alerts".into(),
                title: "🚨 [FIRING] X".into(),
                color: "danger".into(),
                text: "*Severity:* critical".into(),
                ..Message::default()
            })
            .await
            .unwrap();
        assert_eq!(ts, "1700000000.000100");
        c.post(Message {
            channel: "#alerts".into(),
            text: "reply".into(),
            thread_ts: ts.clone(),
            ..Message::default()
        })
        .await
        .unwrap();

        let received = server.received_requests().await.unwrap();
        let parent: Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(parent["text"], "🚨 [FIRING] X");
        assert_eq!(parent["attachments"][0]["color"], "danger");
        assert_eq!(parent["attachments"][0]["text"], "*Severity:* critical");
        assert_eq!(parent["attachments"][0]["mrkdwn_in"][0], "text");
        assert!(parent.get("thread_ts").is_none());
        let reply: Value = serde_json::from_slice(&received[1].body).unwrap();
        assert_eq!(reply["text"], "reply");
        assert_eq!(reply["thread_ts"], "1700000000.000100");
        assert!(reply.get("attachments").is_none());
    }

    #[tokio::test]
    async fn retries_rate_limits_and_server_errors() {
        let server = MockServer::start().await;
        let metrics = Arc::new(Metrics::new());
        Mock::given(path("/chat.postMessage"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(path("/chat.postMessage"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(path("/chat.postMessage"))
            .respond_with(ok_ts("1.2"))
            .mount(&server)
            .await;
        let ts = client(&server, metrics.clone())
            .post(Message {
                channel: "C0000001".into(),
                text: "x".into(),
                ..Message::default()
            })
            .await
            .unwrap();
        assert_eq!(ts, "1.2");
        assert_eq!(requests(&metrics, "chat.postMessage", "rate_limited"), 1);
        assert_eq!(requests(&metrics, "chat.postMessage", "error"), 1);
        assert_eq!(requests(&metrics, "chat.postMessage", "ok"), 1);
    }

    #[tokio::test]
    async fn transient_payload_errors_retry_and_permanent_ones_do_not() {
        let server = MockServer::start().await;
        Mock::given(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "ratelimited"})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "internal_error"})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(path("/chat.postMessage"))
            .respond_with(ok_ts("9"))
            .mount(&server)
            .await;
        let c = client(&server, Arc::new(Metrics::new()));
        assert_eq!(
            c.post(Message {
                channel: "C0000001".into(),
                ..Message::default()
            })
            .await
            .unwrap(),
            "9"
        );

        let server = MockServer::start().await;
        Mock::given(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "channel_not_found"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let err = client(&server, Arc::new(Metrics::new()))
            .post(Message {
                channel: "C0000001".into(),
                ..Message::default()
            })
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "slack error: channel_not_found");

        let server = MockServer::start().await;
        Mock::given(path("/chat.postMessage"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;
        let err = client(&server, Arc::new(Metrics::new()))
            .post(Message {
                channel: "C0000001".into(),
                ..Message::default()
            })
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "slack returned HTTP 403");
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let server = MockServer::start().await;
        Mock::given(path("/chat.postMessage"))
            .respond_with(ResponseTemplate::new(500))
            .expect(3)
            .mount(&server)
            .await;
        let err = client(&server, Arc::new(Metrics::new()))
            .post(Message {
                channel: "C0000001".into(),
                ..Message::default()
            })
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "slack returned HTTP 500");
    }

    #[test]
    fn retry_after_is_capped() {
        assert_eq!(parse_retry_after("5"), Duration::from_secs(5));
        assert_eq!(parse_retry_after(" 7 "), Duration::from_secs(7));
        assert_eq!(parse_retry_after("999"), MAX_RETRY_WAIT);
        assert_eq!(parse_retry_after("soon"), Duration::ZERO);
        assert_eq!(parse_retry_after(""), Duration::ZERO);
    }

    fn history(messages: &Value, next_cursor: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "messages": messages, "has_more": !next_cursor.is_empty(),
            "response_metadata": {"next_cursor": next_cursor}
        }))
    }

    #[tokio::test]
    async fn finds_the_parent_by_marker_in_any_field() {
        let server = MockServer::start().await;
        let metrics = Arc::new(Metrics::new());
        Mock::given(method("GET"))
            .and(path("/conversations.history"))
            .and(query_param("channel", "C0000123"))
            .and(query_param("limit", "50"))
            .and(query_param("inclusive", "true"))
            .and(query_param("oldest", "1700000000"))
            .respond_with(history(
                &json!([
                    {"ts": "3", "text": "unrelated"},
                    {"ts": "2", "text": "", "attachments": [{"footer": "alert-id fp-1"}]},
                    {"ts": "1", "text": "fp-1 in text"}
                ]),
                "",
            ))
            .mount(&server)
            .await;
        let since = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let c = client(&server, metrics.clone());
        assert_eq!(
            c.find_thread_parent("C0000123", "fp-1", since)
                .await
                .unwrap(),
            "2"
        );
        assert_eq!(requests(&metrics, "conversations.history", "ok"), 1);

        for field in ["title", "text", "fallback"] {
            let server = MockServer::start().await;
            Mock::given(path("/conversations.history"))
                .respond_with(history(
                    &json!([{"ts": "7", "attachments": [{field: "x fp-2 y"}]}]),
                    "",
                ))
                .mount(&server)
                .await;
            let c = client(&server, Arc::new(Metrics::new()));
            assert_eq!(
                c.find_thread_parent("C0000123", "fp-2", UNIX_EPOCH)
                    .await
                    .unwrap(),
                "7",
                "{field}"
            );
        }
    }

    #[tokio::test]
    async fn follows_pagination_and_stops_at_the_cap() {
        let server = MockServer::start().await;
        Mock::given(path("/conversations.history"))
            .and(query_param("cursor", "next"))
            .respond_with(history(&json!([{"ts": "5", "text": "fp-1"}]), ""))
            .mount(&server)
            .await;
        Mock::given(path("/conversations.history"))
            .respond_with(history(&json!([{"ts": "9", "text": "no"}]), "next"))
            .mount(&server)
            .await;
        let c = client(&server, Arc::new(Metrics::new()));
        assert!(
            c.find_thread_parent("#name-less", "fp-1", UNIX_EPOCH)
                .await
                .is_err()
        );

        // A channel ID skips the resolution and reaches the history.
        assert_eq!(
            c.find_thread_parent("C0000123", "fp-1", UNIX_EPOCH)
                .await
                .unwrap(),
            "5"
        );

        let server = MockServer::start().await;
        Mock::given(path("/conversations.history"))
            .respond_with(history(&json!([{"ts": "9", "text": "no"}]), "more"))
            .expect(u64::from(MAX_HISTORY_PAGES))
            .mount(&server)
            .await;
        let c = client(&server, Arc::new(Metrics::new()));
        assert!(matches!(
            c.find_thread_parent("C0000123", "fp-1", UNIX_EPOCH).await,
            Err(Error::MessageNotFound)
        ));
    }

    #[tokio::test]
    async fn lookup_errors() {
        let server = MockServer::start().await;
        Mock::given(path("/conversations.history"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "missing_scope"})),
            )
            .mount(&server)
            .await;
        let c = client(&server, Arc::new(Metrics::new()));
        let err = c
            .find_thread_parent("C0000123", "fp", UNIX_EPOCH)
            .await
            .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "read channel history: slack error: missing_scope"
        );
        let err = c
            .find_thread_parent("C0000123", "", UNIX_EPOCH)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "marker is empty");
        let err = c
            .find_thread_parent("  ", "fp", UNIX_EPOCH)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "channel is empty");
    }

    fn channel_list(types: &str, channels: &Value) -> Mock {
        Mock::given(method("GET"))
            .and(path("/conversations.list"))
            .and(query_param("types", types))
            .and(query_param("exclude_archived", "true"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "channels": channels})),
            )
    }

    #[tokio::test]
    async fn resolves_and_caches_channel_names() {
        let server = MockServer::start().await;
        channel_list(
            "public_channel",
            &json!([{"id": "C0000777", "name": "alerts"}]),
        )
        .expect(1)
        .mount(&server)
        .await;
        Mock::given(path("/conversations.history"))
            .and(query_param("channel", "C0000777"))
            .respond_with(history(&json!([{"ts": "4", "text": "fp"}]), ""))
            .expect(2)
            .mount(&server)
            .await;
        let c = client(&server, Arc::new(Metrics::new()));
        assert_eq!(
            c.find_thread_parent("#alerts", "fp", UNIX_EPOCH)
                .await
                .unwrap(),
            "4"
        );
        assert_eq!(
            c.find_thread_parent("alerts", "fp", UNIX_EPOCH)
                .await
                .unwrap(),
            "4"
        );
        assert_eq!(c.resolve_channel_id("alerts").await.unwrap(), "C0000777");
        assert_eq!(c.resolve_channel_id("C000000").await.unwrap(), "C000000");
    }

    #[tokio::test]
    async fn cache_expires() {
        let server = MockServer::start().await;
        channel_list("public_channel", &json!([{"id": "C0000001", "name": "a"}]))
            .expect(2)
            .mount(&server)
            .await;
        let c = client(&server, Arc::new(Metrics::new())).with_cache_ttl(Duration::from_millis(10));
        assert_eq!(c.resolve_channel_id("a").await.unwrap(), "C0000001");
        sleep(Duration::from_millis(30)).await;
        assert_eq!(c.resolve_channel_id("a").await.unwrap(), "C0000001");
    }

    #[tokio::test]
    async fn resolves_private_channels_second() {
        let server = MockServer::start().await;
        channel_list(
            "public_channel",
            &json!([{"id": "C0000001", "name": "other"}]),
        )
        .mount(&server)
        .await;
        channel_list(
            "private_channel",
            &json!([{"id": "G0000009", "name": "secret"}]),
        )
        .mount(&server)
        .await;
        let c = client(&server, Arc::new(Metrics::new()));
        assert_eq!(c.resolve_channel_id("secret").await.unwrap(), "G0000009");
        let err = c.resolve_channel_id("missing").await.unwrap_err();
        assert!(
            err.to_string()
                .contains("not found; the bot must be able to see it"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn private_sweep_missing_scope_is_explained() {
        let server = MockServer::start().await;
        channel_list("public_channel", &json!([]))
            .mount(&server)
            .await;
        Mock::given(path("/conversations.list"))
            .and(query_param("types", "private_channel"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "missing_scope"})),
            )
            .mount(&server)
            .await;
        let err = client(&server, Arc::new(Metrics::new()))
            .resolve_channel_id("secret")
            .await
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("groups:read"), "{text}");
        assert!(text.contains("missing_scope"), "{text}");
    }

    #[tokio::test]
    async fn channel_list_pagination_is_followed() {
        let server = MockServer::start().await;
        Mock::given(path("/conversations.list"))
            .and(query_param("cursor", "p2"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"ok": true, "channels": [{"id": "C0000002", "name": "b"}]}),
                ),
            )
            .mount(&server)
            .await;
        Mock::given(path("/conversations.list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "channels": [{"id": "C0000001", "name": "a"}], "response_metadata": {"next_cursor": "p2"}
            })))
            .mount(&server)
            .await;
        assert_eq!(
            client(&server, Arc::new(Metrics::new()))
                .resolve_channel_id("b")
                .await
                .unwrap(),
            "C0000002"
        );
    }

    #[tokio::test]
    async fn reads_retry_like_writes() {
        let server = MockServer::start().await;
        let metrics = Arc::new(Metrics::new());
        Mock::given(path("/auth.test"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(path("/auth.test"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "service_unavailable"})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(path("/auth.test"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "user_id": "UBOT"})),
            )
            .mount(&server)
            .await;
        assert_eq!(
            client(&server, metrics.clone()).auth_test().await.unwrap(),
            "UBOT"
        );
        assert_eq!(requests(&metrics, "auth.test", "rate_limited"), 1);
        assert_eq!(requests(&metrics, "auth.test", "error"), 1);
        assert_eq!(requests(&metrics, "auth.test", "ok"), 1);

        let server = MockServer::start().await;
        Mock::given(path("/auth.test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&server)
            .await;
        let err = client(&server, Arc::new(Metrics::new()))
            .auth_test()
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "auth.test returned no user id");

        let server = MockServer::start().await;
        Mock::given(path("/auth.test"))
            .respond_with(ResponseTemplate::new(502))
            .expect(3)
            .mount(&server)
            .await;
        let err = client(&server, Arc::new(Metrics::new()))
            .auth_test()
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "slack returned HTTP 502");

        let server = MockServer::start().await;
        Mock::given(path("/auth.test"))
            .respond_with(ResponseTemplate::new(200).set_body_string("nope"))
            .mount(&server)
            .await;
        let err = client(&server, Arc::new(Metrics::new()))
            .auth_test()
            .await
            .unwrap_err();
        assert!(format!("{err:#}").starts_with("decode response"));
    }

    #[tokio::test]
    async fn transport_failures_are_errors() {
        let uri = "http://127.0.0.1:1".to_string();
        let c = Client::new(&uri, "t", Duration::from_secs(1), Arc::new(Metrics::new()))
            .with_backoff(Duration::from_millis(1));
        let err = c.auth_test().await.unwrap_err();
        assert!(format!("{err:#}").starts_with("call slack"));
        let err = c
            .post(Message {
                channel: "C0000001".into(),
                ..Message::default()
            })
            .await
            .unwrap_err();
        assert!(format!("{err:#}").starts_with("call slack"));
        let err = c.add_reaction("C0000001", "1", "eyes").await.unwrap_err();
        assert!(format!("{err:#}").starts_with("call slack"));
    }

    #[tokio::test]
    async fn update_and_ephemeral() {
        let server = MockServer::start().await;
        Mock::given(path("/chat.update"))
            .and(body_partial_json(
                json!({"channel": "C0000001", "ts": "1.1", "text": "new"}),
            ))
            .respond_with(ok_ts("1.1"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/chat.postEphemeral"))
            .and(body_partial_json(
                json!({"channel": "C0000001", "user": "U1", "text": "hint", "thread_ts": "2.2"}),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "message_ts": "3"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/chat.postEphemeral"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&server)
            .await;
        let c = client(&server, Arc::new(Metrics::new()));
        c.update("C0000001", "1.1", "new").await.unwrap();
        c.post_ephemeral("C0000001", "2.2", "U1", "hint")
            .await
            .unwrap();
        c.post_ephemeral("C0000001", "", "U1", "hint")
            .await
            .unwrap();
        let received = server.received_requests().await.unwrap();
        let last: Value = serde_json::from_slice(&received[2].body).unwrap();
        assert!(last.get("thread_ts").is_none());

        let server = MockServer::start().await;
        Mock::given(path("/chat.update"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "cant_update_message"})),
            )
            .mount(&server)
            .await;
        let err = client(&server, Arc::new(Metrics::new()))
            .update("C0000001", "1", "x")
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "slack error: cant_update_message");
    }

    #[tokio::test]
    async fn thread_parent_flattens_attachments() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/conversations.replies"))
            .and(query_param("channel", "C0000001"))
            .and(query_param("ts", "1.0"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"ok": true, "messages": [
                    {"ts": "1.0", "bot_id": "B1", "text": " body ", "attachments": [
                        {"title": "T", "text": "X", "footer": "alert-id fp", "fallback": "ignored"},
                        {"fallback": "only fallback", "footer": ""}
                    ]},
                    {"ts": "1.1", "text": "reply"}
                ]}),
            ))
            .mount(&server)
            .await;
        let c = client(&server, Arc::new(Metrics::new()));
        let parent = c.thread_parent("C0000001", "1.0").await.unwrap();
        assert_eq!(
            parent,
            ThreadMessage {
                ts: "1.0".into(),
                user: String::new(),
                bot_id: "B1".into(),
                text: "body\nT\nX\nalert-id fp\nonly fallback".into()
            }
        );
        assert_eq!(
            c.thread_parent("C0000001", "")
                .await
                .unwrap_err()
                .to_string(),
            "thread timestamp is empty"
        );

        let server = MockServer::start().await;
        Mock::given(path("/conversations.replies"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "messages": []})),
            )
            .mount(&server)
            .await;
        assert!(matches!(
            client(&server, Arc::new(Metrics::new()))
                .thread_parent("C0000001", "1")
                .await,
            Err(Error::MessageNotFound)
        ));
    }

    #[tokio::test]
    async fn reactions_tolerate_duplicates() {
        let server = MockServer::start().await;
        let metrics = Arc::new(Metrics::new());
        Mock::given(path("/reactions.add"))
            .and(body_partial_json(
                json!({"channel": "C0000001", "timestamp": "1.0", "name": "eyes"}),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "already_reacted"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/reactions.remove"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "no_reaction"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let c = client(&server, metrics.clone());
        c.add_reaction("C0000001", "1.0", ":eyes:").await.unwrap();
        c.remove_reaction("C0000001", "1.0", "eyes").await.unwrap();
        assert_eq!(requests(&metrics, "reactions.add", "ok"), 1);

        let server = MockServer::start().await;
        Mock::given(path("/reactions.add"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "invalid_name"})),
            )
            .mount(&server)
            .await;
        Mock::given(path("/reactions.remove"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let c = client(&server, metrics.clone());
        assert_eq!(
            c.add_reaction("C0000001", "1", "x")
                .await
                .unwrap_err()
                .to_string(),
            "slack error: invalid_name"
        );
        assert_eq!(
            c.remove_reaction("C0000001", "1", "x")
                .await
                .unwrap_err()
                .to_string(),
            "slack returned HTTP 500"
        );
        assert_eq!(requests(&metrics, "reactions.add", "error"), 1);
    }

    #[test]
    fn normalize_and_truncate() {
        assert_eq!(normalize_channel("alerts"), "#alerts");
        assert_eq!(normalize_channel(" #alerts "), "#alerts");
        assert_eq!(normalize_channel("C0123456"), "C0123456");
        assert_eq!(normalize_channel("G0123456"), "G0123456");
        assert_eq!(normalize_channel(""), "");
        assert_eq!(normalize_channel("Cshort"), "#Cshort");

        assert_eq!(truncate("short", 100), "short");
        assert_eq!(truncate("anything", 0), "anything");
        let long = "가".repeat(50);
        let cut = truncate(&long, 20);
        assert!(cut.ends_with("\n_(truncated)_"));
        assert_eq!(cut.chars().count(), 20);
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcd", 3), "\n_(truncated)_");
    }
}
