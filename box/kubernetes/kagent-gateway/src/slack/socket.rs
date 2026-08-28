//! Receives Slack events over Socket Mode.
//!
//! Socket Mode is an outbound WebSocket, so the gateway needs no public
//! endpoint, no request URL, and no signing secret: `apps.connections.open`
//! returns a single use wss URL, and every event arrives on it as an envelope
//! that has to be acknowledged within a few seconds.
//!
//! The module owns the connection and the envelope loop and knows nothing
//! about agents. It hands each event to a [`Handler`], which keeps the routing
//! and the agent logic in one place and leaves this transport testable against
//! a local WebSocket server.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::observability::Metrics;

/// Bounds one acknowledgement write. Slack redelivers an envelope that is not
/// acknowledged within 3 seconds, so a write that blocks longer than that has
/// already lost the race and only holds up the read loop.
const ACK_TIMEOUT: Duration = Duration::from_secs(2);
/// Caps the wait between reconnect attempts. Slack recycles a connection
/// roughly every hour, so a reconnect is routine rather than a failure, and
/// the ceiling keeps a real outage from stretching the retry interval past the
/// point of noticing a recovery.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Bounds one envelope. Slack event payloads are small; anything larger is a
/// protocol surprise rather than a mention.
const READ_LIMIT: usize = 1 << 20;

/// One Slack event carried by an envelope, flattened to the fields the
/// mention path routes on. `raw` keeps the original payload for debug
/// logging, which is what pins the real shape of an event during a rollout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Event {
    pub envelope_id: String,
    pub kind: String,
    pub channel_id: String,
    pub channel_type: String,
    pub user: String,
    pub bot_id: String,
    pub subtype: String,
    pub text: String,
    /// Timestamp of the message carrying the mention, which a reaction goes
    /// on. `thread_ts` is the thread it lives in, empty at channel level.
    pub ts: String,
    pub thread_ts: String,
    pub raw: String,
}

/// Receives an event after its envelope has been acknowledged.
/// Implementations must not hold the read loop for long: one turn's work
/// belongs on a task of its own.
#[async_trait]
pub trait Handler: Send + Sync {
    async fn handle_event(&self, ev: Event);
}

/// Opens Socket Mode connections and reads envelopes off them.
pub struct Client {
    http: reqwest::Client,
    api_url: String,
    app_token: String,
    metrics: Arc<Metrics>,
    backoff: Duration,
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One Socket Mode frame. Only the fields the loop acts on are decoded; the
/// event payload is kept raw and flattened separately.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "envelope_id")]
    id: String,
    reason: String,
    payload: EnvelopePayload,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct EnvelopePayload {
    event: Option<serde_json::Value>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct EventBody {
    #[serde(rename = "type")]
    kind: String,
    channel: String,
    channel_type: String,
    user: String,
    bot_id: String,
    subtype: String,
    text: String,
    ts: String,
    thread_ts: String,
}

/// Keeps the connected gauge truthful even when the session future is
/// dropped by a shutdown mid-read.
struct ConnectedGuard(Arc<Metrics>);

impl Drop for ConnectedGuard {
    fn drop(&mut self) {
        self.0.set_socket_connected(false);
    }
}

impl Client {
    /// Returns a client for the Web API at `api_url` authenticating with an
    /// app-level token (`xapp-...`).
    ///
    /// # Panics
    ///
    /// Panics when the HTTP client cannot be built, which only happens when
    /// the TLS backend is unusable.
    #[must_use]
    pub fn new(api_url: &str, app_token: &str, metrics: Arc<Metrics>) -> Self {
        Self {
            // The connection open call is a normal Web API request; the
            // WebSocket itself is not bounded by this timeout.
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("build http client"),
            api_url: api_url.trim_end_matches('/').to_string(),
            app_token: app_token.to_string(),
            metrics,
            backoff: Duration::from_secs(1),
        }
    }

    /// Overrides the base reconnect wait, which tests shorten.
    #[cfg(test)]
    pub const fn with_backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    /// Keeps a Socket Mode connection open until `shutdown` fires,
    /// reconnecting with backoff after every drop. A connection that cannot be
    /// established is retried rather than reported, because the alert path
    /// must keep working when Slack is unreachable.
    pub async fn run(&self, handler: Arc<dyn Handler>, shutdown: CancellationToken) {
        let mut attempt = 0_u32;
        loop {
            if shutdown.is_cancelled() {
                return;
            }
            let outcome = tokio::select! {
                () = shutdown.cancelled() => return,
                outcome = self.session(handler.as_ref()) => outcome,
            };
            let wait = match outcome {
                Ok(requested) => {
                    // A clean end is a reconnect Slack asked for, so the next
                    // attempt starts from the shortest wait rather than
                    // inheriting a backoff.
                    attempt = 0;
                    requested
                }
                Err(err) => {
                    attempt += 1;
                    warn!(error = %err, attempt, "socket mode session ended");
                    self.wait(attempt)
                }
            };
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = sleep(wait) => {}
            }
        }
    }

    /// Opens one connection and reads it to its end. The returned duration is
    /// the wait before the next attempt when Slack asked for a prompt
    /// reconnect.
    async fn session(&self, handler: &dyn Handler) -> Result<Duration> {
        let url = match self.open().await {
            Ok(url) => url,
            Err(err) => {
                self.metrics.observe_socket_connection("error");
                return Err(err.context("open connection"));
            }
        };
        let ws = match dial(&url).await {
            Ok(ws) => ws,
            Err(err) => {
                self.metrics.observe_socket_connection("error");
                return Err(err.context("dial socket"));
            }
        };

        self.metrics.observe_socket_connection("ok");
        self.metrics.set_socket_connected(true);
        let _guard = ConnectedGuard(self.metrics.clone());
        info!("socket mode connected");

        self.envelope_loop(ws, handler).await
    }

    async fn envelope_loop(&self, mut ws: Socket, handler: &dyn Handler) -> Result<Duration> {
        loop {
            let raw = match ws.next().await {
                Some(Ok(WsMessage::Text(text))) => text.to_string(),
                Some(Ok(WsMessage::Binary(bytes))) => String::from_utf8_lossy(&bytes).into_owned(),
                Some(Ok(WsMessage::Close(frame))) => bail!("connection closed by slack: {frame:?}"),
                Some(Ok(_)) => continue,
                Some(Err(err)) => return Err(anyhow!(err).context("read socket")),
                None => bail!("connection closed"),
            };

            let env: Envelope = match serde_json::from_str(&raw) {
                Ok(env) => env,
                Err(err) => {
                    warn!(error = %err, "failed to decode socket envelope");
                    continue;
                }
            };

            match env.kind.as_str() {
                "hello" => {
                    debug!("socket mode session confirmed");
                    continue;
                }
                "disconnect" => {
                    // Slack recycles connections and warns before it does.
                    // Reconnecting at once keeps the gap short instead of
                    // paying a backoff for a drop that was scheduled.
                    info!(reason = env.reason, "socket mode disconnect requested");
                    self.metrics
                        .observe_socket_connection("disconnect_requested");
                    let _ = ws.close(None).await;
                    return Ok(Duration::from_secs(1));
                }
                _ => {}
            }

            // The acknowledgement never waits on the handler: Slack redelivers
            // an envelope that is not acknowledged within 3 seconds, and the
            // work one event triggers outlives that by two orders of
            // magnitude.
            if !env.id.is_empty() {
                let ack = serde_json::json!({"envelope_id": env.id}).to_string();
                timeout(ACK_TIMEOUT, ws.send(WsMessage::text(ack)))
                    .await
                    .map_err(|_| anyhow!("acknowledgement timed out"))?
                    .context("acknowledge envelope")?;
            }
            let Some(event) = env.payload.event.filter(|_| env.kind == "events_api") else {
                continue;
            };
            let Some(ev) = decode_event(&env.id, event) else {
                continue;
            };
            handler.handle_event(ev).await;
        }
    }

    /// Asks Slack for a single use WebSocket URL.
    async fn open(&self) -> Result<String> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Opened {
            ok: bool,
            url: String,
            error: String,
        }
        let resp = self
            .http
            .post(format!("{}/apps.connections.open", self.api_url))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .bearer_auth(&self.app_token)
            .send()
            .await
            .context("call slack")?;
        if resp.status() != reqwest::StatusCode::OK {
            bail!("slack returned HTTP {}", resp.status().as_u16());
        }
        let out: Opened = resp.json().await.context("decode response")?;
        if !out.ok {
            bail!("slack error: {}", out.error);
        }
        if out.url.is_empty() {
            bail!("slack returned no socket url");
        }
        Ok(out.url)
    }

    /// The backoff before the next attempt: exponential up to the cap, with
    /// jitter so a controller restart does not line every replica up on the
    /// same reconnect instant.
    fn wait(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return self.backoff;
        }
        let shift = (attempt - 1).min(16);
        let base_ms = u64::try_from(self.backoff.as_millis()).unwrap_or(u64::MAX);
        let max_ms = u64::try_from(MAX_BACKOFF.as_millis()).unwrap_or(u64::MAX);
        let d_ms = base_ms.checked_shl(shift).map_or(max_ms, |v| v.min(max_ms));
        let half = d_ms / 2;
        Duration::from_millis(half + rand::random_range(0..=half))
    }
}

/// Flattens the event payload of an envelope.
fn decode_event(envelope_id: &str, event: serde_json::Value) -> Option<Event> {
    let raw = event.to_string();
    let body: EventBody = match serde_json::from_value(event) {
        Ok(body) => body,
        Err(err) => {
            warn!(error = %err, "failed to decode socket event");
            return None;
        }
    };
    Some(Event {
        envelope_id: envelope_id.to_string(),
        kind: body.kind,
        channel_id: body.channel,
        channel_type: body.channel_type,
        user: body.user,
        bot_id: body.bot_id,
        subtype: body.subtype,
        text: body.text,
        ts: body.ts,
        thread_ts: body.thread_ts,
        raw,
    })
}

/// Opens the wss URL with the envelope read limit applied.
async fn dial(url: &str) -> Result<Socket> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(READ_LIMIT))
        .max_frame_size(Some(READ_LIMIT));
    let (ws, _) = tokio_tungstenite::connect_async_with_config(url, Some(config), false).await?;
    Ok(ws)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;
    use tokio::net::TcpListener;
    use tokio::sync::Notify;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<Event>>,
        notify: Notify,
    }

    #[async_trait]
    impl Handler for Recorder {
        async fn handle_event(&self, ev: Event) {
            self.events.lock().unwrap().push(ev);
            self.notify.notify_one();
        }
    }

    /// A local Socket Mode server that runs `script` against each accepted
    /// connection.
    async fn ws_server<F, Fut>(script: F) -> String
    where
        F: Fn(Socket) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let script = Arc::new(script);
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let ws = tokio_tungstenite::accept_async(MaybeTlsStream::Plain(stream))
                    .await
                    .unwrap();
                let script = script.clone();
                tokio::spawn(async move { script(ws).await });
            }
        });
        format!("ws://{addr}")
    }

    async fn slack(ws_url: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/apps.connections.open"))
            .and(header("authorization", "Bearer xapp-test"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "url": ws_url})),
            )
            .mount(&server)
            .await;
        server
    }

    fn mention(envelope_id: &str) -> String {
        json!({"type": "events_api", "envelope_id": envelope_id, "payload": {"event": {
            "type": "app_mention", "channel": "C1", "user": "U1", "text": "<@UBOT> hi", "ts": "2.0", "thread_ts": "1.0"
        }}})
        .to_string()
    }

    async fn read_ack(ws: &mut Socket) -> String {
        loop {
            if let WsMessage::Text(text) = ws.next().await.unwrap().unwrap() {
                return text.to_string();
            }
        }
    }

    #[tokio::test]
    async fn acknowledges_and_dispatches_mentions() {
        let acks: Arc<Mutex<Vec<String>>> = Arc::default();
        let acks_seen = acks.clone();
        let url = ws_server(move |mut ws| {
            let acks = acks_seen.clone();
            async move {
                ws.send(WsMessage::text(json!({"type": "hello"}).to_string()))
                    .await
                    .unwrap();
                ws.send(WsMessage::text("not json")).await.unwrap();
                ws.send(WsMessage::text(
                    json!({"type": "events_api", "envelope_id": "e0"}).to_string(),
                ))
                .await
                .unwrap();
                let ack = read_ack(&mut ws).await;
                acks.lock().unwrap().push(ack);
                ws.send(WsMessage::text(mention("e1"))).await.unwrap();
                let ack = read_ack(&mut ws).await;
                acks.lock().unwrap().push(ack);
                ws.send(WsMessage::text(
                    json!({"type": "events_api", "envelope_id": "e2", "payload": {"event": "bad"}})
                        .to_string(),
                ))
                .await
                .unwrap();
                let ack = read_ack(&mut ws).await;
                acks.lock().unwrap().push(ack);
                ws.send(WsMessage::text(
                    json!({"type": "disconnect", "reason": "refresh"}).to_string(),
                ))
                .await
                .unwrap();
                // Hold the connection until the client closes it.
                while ws.next().await.is_some() {}
            }
        })
        .await;
        let slack = slack(&url).await;
        let metrics = Arc::new(Metrics::new());
        let client = Client::new(&slack.uri(), "xapp-test", metrics.clone());
        let recorder = Arc::new(Recorder::default());
        let shutdown = CancellationToken::new();
        let run = tokio::spawn({
            let (recorder, shutdown) = (recorder.clone(), shutdown.clone());
            async move { client.run(recorder, shutdown).await }
        });

        timeout(Duration::from_secs(5), recorder.notify.notified())
            .await
            .unwrap();
        let events = recorder.events.lock().unwrap().clone();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].envelope_id, "e1");
        assert_eq!(events[0].kind, "app_mention");
        assert_eq!(events[0].channel_id, "C1");
        assert_eq!(events[0].thread_ts, "1.0");
        assert!(events[0].raw.contains("app_mention"));
        assert_eq!(metrics.socket_connected.get(), 1);

        // The disconnect lands and a reconnect follows without a backoff.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while metrics
            .socket_connections
            .get_or_create(&crate::observability::metrics::ResultLabels {
                result: "ok".into(),
            })
            .get()
            < 2
        {
            assert!(tokio::time::Instant::now() < deadline, "no reconnect");
            sleep(Duration::from_millis(20)).await;
        }
        shutdown.cancel();
        timeout(Duration::from_secs(5), run).await.unwrap().unwrap();
        // The reconnect replays the script, so more acks may follow the
        // first three.
        let acks = acks.lock().unwrap().clone();
        assert!(acks.len() >= 3, "{acks:?}");
        assert!(acks[0].contains("\"envelope_id\":\"e0\""));
        assert!(acks[1].contains("\"envelope_id\":\"e1\""));
        assert!(acks[2].contains("\"envelope_id\":\"e2\""));
        assert_eq!(metrics.socket_connected.get(), 0);
        assert!(
            metrics
                .socket_connections
                .get_or_create(&crate::observability::metrics::ResultLabels {
                    result: "disconnect_requested".into()
                })
                .get()
                >= 1
        );
    }

    #[tokio::test]
    async fn reconnects_after_a_dropped_connection() {
        let url = ws_server(|mut ws| async move {
            ws.send(WsMessage::text(json!({"type": "hello"}).to_string()))
                .await
                .unwrap();
            ws.close(None).await.unwrap();
        })
        .await;
        let slack = slack(&url).await;
        let metrics = Arc::new(Metrics::new());
        let client = Client::new(&slack.uri(), "xapp-test", metrics.clone())
            .with_backoff(Duration::from_millis(5));
        let shutdown = CancellationToken::new();
        let run = tokio::spawn({
            let shutdown = shutdown.clone();
            async move { client.run(Arc::new(Recorder::default()), shutdown).await }
        });
        let ok = crate::observability::metrics::ResultLabels {
            result: "ok".into(),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while metrics.socket_connections.get_or_create(&ok).get() < 3 {
            assert!(tokio::time::Instant::now() < deadline, "no reconnects");
            sleep(Duration::from_millis(10)).await;
        }
        shutdown.cancel();
        timeout(Duration::from_secs(5), run).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn retries_when_the_connection_cannot_be_opened() {
        let server = MockServer::start().await;
        Mock::given(path("/apps.connections.open"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "invalid_auth"})),
            )
            .mount(&server)
            .await;
        let metrics = Arc::new(Metrics::new());
        let client = Client::new(&server.uri(), "xapp-test", metrics.clone())
            .with_backoff(Duration::from_millis(2));
        let shutdown = CancellationToken::new();
        let run = tokio::spawn({
            let shutdown = shutdown.clone();
            async move { client.run(Arc::new(Recorder::default()), shutdown).await }
        });
        let error = crate::observability::metrics::ResultLabels {
            result: "error".into(),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while metrics.socket_connections.get_or_create(&error).get() < 2 {
            assert!(tokio::time::Instant::now() < deadline, "no retries");
            sleep(Duration::from_millis(10)).await;
        }
        shutdown.cancel();
        timeout(Duration::from_secs(5), run).await.unwrap().unwrap();
        assert_eq!(metrics.socket_connected.get(), 0);
    }

    #[tokio::test]
    async fn open_reports_every_failure_shape() {
        let server = MockServer::start().await;
        Mock::given(path("/apps.connections.open"))
            .respond_with(ResponseTemplate::new(401))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(path("/apps.connections.open"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "url": ""})))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(path("/apps.connections.open"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": true, "url": "ws://127.0.0.1:1"})),
            )
            .mount(&server)
            .await;
        let client = Client::new(&server.uri(), "xapp-test", Arc::new(Metrics::new()));
        assert_eq!(
            client.open().await.unwrap_err().to_string(),
            "slack returned HTTP 401"
        );
        assert_eq!(
            client.open().await.unwrap_err().to_string(),
            "slack returned no socket url"
        );
        let err = client.session(&Recorder::default()).await.unwrap_err();
        assert!(format!("{err:#}").starts_with("dial socket"), "{err:#}");
    }

    #[test]
    fn wait_grows_and_stays_capped() {
        let client = Client::new("http://slack", "t", Arc::new(Metrics::new()));
        assert_eq!(client.wait(0), Duration::from_secs(1));
        let w1 = client.wait(1);
        assert!(
            w1 >= Duration::from_millis(500) && w1 <= Duration::from_secs(1),
            "{w1:?}"
        );
        let w3 = client.wait(3);
        assert!(
            w3 >= Duration::from_secs(2) && w3 <= Duration::from_secs(4),
            "{w3:?}"
        );
        for attempt in [6, 10, 40] {
            let w = client.wait(attempt);
            assert!(w >= MAX_BACKOFF / 2 && w <= MAX_BACKOFF, "{attempt}: {w:?}");
        }
    }

    #[test]
    fn decode_event_flattens_fields() {
        let ev = decode_event(
            "e",
            json!({"type": "message", "channel": "C1", "subtype": "bot_message", "bot_id": "B1"}),
        )
        .unwrap();
        assert_eq!(ev.kind, "message");
        assert_eq!(ev.subtype, "bot_message");
        assert_eq!(ev.bot_id, "B1");
        assert!(decode_event("e", json!(42)).is_none());
    }
}
