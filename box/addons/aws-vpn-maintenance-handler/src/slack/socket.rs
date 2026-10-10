//! Receives approval clicks over Socket Mode.
//!
//! Socket Mode is an outbound WebSocket, so the controller needs no public
//! endpoint, no request URL, and no signing secret: `apps.connections.open`
//! returns a single use wss URL, and every click arrives on it as an envelope
//! that has to be acknowledged within a few seconds.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Action IDs on the approval buttons, matched on the way back in.
pub const ACTION_APPROVE: &str = "vtr_approve";
pub const ACTION_DENY: &str = "vtr_deny";

/// Bounds one acknowledgement write. Slack redelivers an envelope that is not
/// acknowledged within 3 seconds.
const ACK_TIMEOUT: Duration = Duration::from_secs(2);
/// Caps the wait between reconnect attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Bounds one envelope. Interaction payloads are small.
const READ_LIMIT: usize = 1 << 20;

/// One approve/deny click, normalized from the Slack callback.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Interaction {
    /// The button value: which proposed replacement this refers to.
    pub request_id: String,
    pub approved: bool,
    /// The clicker, checked by the caller against the configured approvers so
    /// a forwarded card cannot become an authorization.
    pub user_id: String,
    /// The display name, for the audit trail.
    pub user_name: String,
}

/// Receives clicks and connection state. Implementations must not block.
pub trait Handler: Send + Sync {
    fn handle(&self, interaction: Interaction);
    fn connected(&self, connected: bool);
}

/// Opens Socket Mode connections and reads envelopes off them.
pub struct SocketClient {
    http: reqwest::Client,
    api_url: String,
    app_token: String,
    backoff: Duration,
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Deserialize, Default)]
#[serde(default)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "envelope_id")]
    id: String,
    reason: String,
    payload: Payload,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Payload {
    #[serde(rename = "type")]
    kind: String,
    user: User,
    actions: Vec<Action>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct User {
    id: String,
    username: String,
    name: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Action {
    action_id: String,
    value: String,
}

impl SocketClient {
    /// Returns a client for the Web API at `api_url` authenticating with an
    /// app-level token (`xapp-...`).
    ///
    /// # Panics
    ///
    /// Panics when the HTTP client cannot be built, which only happens when
    /// the TLS backend is unusable.
    #[must_use]
    pub fn new(api_url: &str, app_token: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("build http client"),
            api_url: api_url.trim_end_matches('/').to_string(),
            app_token: app_token.to_string(),
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
    /// reconnecting with backoff after every drop. Losing the socket delays
    /// approvals but never lets an unapproved replacement through: the
    /// executor only runs on a decision that arrived here.
    pub async fn run(&self, handler: Arc<dyn Handler>, shutdown: CancellationToken) {
        let mut attempt = 0_u32;
        loop {
            if shutdown.is_cancelled() {
                return;
            }
            let outcome = tokio::select! {
                () = shutdown.cancelled() => {
                    handler.connected(false);
                    return;
                }
                outcome = self.session(handler.as_ref()) => outcome,
            };
            handler.connected(false);
            let wait = match outcome {
                Ok(requested) => {
                    attempt = 0;
                    requested
                }
                Err(err) => {
                    attempt += 1;
                    error!(error = %err, attempt, "Slack Socket Mode connection error; will retry");
                    self.wait(attempt)
                }
            };
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = sleep(wait) => {}
            }
        }
    }

    async fn session(&self, handler: &dyn Handler) -> Result<Duration> {
        let started = Instant::now();
        info!("connecting to Slack over Socket Mode");
        let url = self.open().await.context("open connection")?;
        let ws = dial(&url).await.context("dial socket")?;
        info!(took = ?started.elapsed(), "Slack Socket Mode connected; approvals are live");
        handler.connected(true);
        let out = self.envelope_loop(ws, handler).await;
        warn!("Slack Socket Mode disconnected; approvals cannot be received until it reconnects");
        out
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
                    error!(error = %err, "Slack Socket Mode error: bad message");
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
                    info!(reason = env.reason, "socket mode disconnect requested");
                    let _ = ws.close(None).await;
                    return Ok(Duration::from_secs(1));
                }
                _ => {}
            }

            // Acking first matters: Slack retries an unacked envelope,
            // delivering the approval twice.
            if !env.id.is_empty() {
                let ack = serde_json::json!({"envelope_id": env.id}).to_string();
                timeout(ACK_TIMEOUT, ws.send(WsMessage::text(ack)))
                    .await
                    .map_err(|_| anyhow!("acknowledgement timed out"))?
                    .context("acknowledge envelope")?;
            }
            if env.kind != "interactive" || env.payload.kind != "block_actions" {
                continue;
            }
            for interaction in decode(&env.payload) {
                handler.handle(interaction);
            }
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

    /// Exponential backoff up to the cap, with jitter so a controller restart
    /// does not line every replica up on the same reconnect instant.
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

/// Forwards recognized clicks; other block actions are ignored.
fn decode(payload: &Payload) -> Vec<Interaction> {
    let user_name = if payload.user.username.is_empty() {
        payload.user.name.clone()
    } else {
        payload.user.username.clone()
    };
    payload
        .actions
        .iter()
        .filter_map(|a| {
            let approved = match a.action_id.as_str() {
                ACTION_APPROVE => true,
                ACTION_DENY => false,
                _ => return None,
            };
            Some(Interaction {
                request_id: a.value.clone(),
                approved,
                user_id: payload.user.id.clone(),
                user_name: user_name.clone(),
            })
        })
        .collect()
}

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
        clicks: Mutex<Vec<Interaction>>,
        states: Mutex<Vec<bool>>,
        notify: Notify,
    }

    impl Handler for Recorder {
        fn handle(&self, i: Interaction) {
            self.clicks.lock().unwrap().push(i);
            self.notify.notify_one();
        }
        fn connected(&self, c: bool) {
            self.states.lock().unwrap().push(c);
        }
    }

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

    fn click(envelope_id: &str, action: &str, value: &str) -> String {
        json!({"type": "interactive", "envelope_id": envelope_id, "payload": {
            "type": "block_actions",
            "user": {"id": "U1", "username": "younsl"},
            "actions": [{"action_id": action, "value": value}]
        }})
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
    async fn acknowledges_and_dispatches_clicks() {
        let acks: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = acks.clone();
        let url = ws_server(move |mut ws| {
            let acks = seen.clone();
            async move {
                ws.send(WsMessage::text(json!({"type": "hello"}).to_string()))
                    .await
                    .unwrap();
                ws.send(WsMessage::text("not json")).await.unwrap();
                // An events_api envelope is acked and otherwise ignored.
                ws.send(WsMessage::text(
                    json!({"type": "events_api", "envelope_id": "e0"}).to_string(),
                ))
                .await
                .unwrap();
                {
                    let ack = read_ack(&mut ws).await;
                    acks.lock().unwrap().push(ack);
                }
                ws.send(WsMessage::text(click("e1", ACTION_APPROVE, "req-1")))
                    .await
                    .unwrap();
                {
                    let ack = read_ack(&mut ws).await;
                    acks.lock().unwrap().push(ack);
                }
                ws.send(WsMessage::text(click("e2", "other", "req-1")))
                    .await
                    .unwrap();
                {
                    let ack = read_ack(&mut ws).await;
                    acks.lock().unwrap().push(ack);
                }
                ws.send(WsMessage::text(click("e3", ACTION_DENY, "req-2")))
                    .await
                    .unwrap();
                {
                    let ack = read_ack(&mut ws).await;
                    acks.lock().unwrap().push(ack);
                }
                ws.send(WsMessage::Binary(
                    click("e4", ACTION_APPROVE, "req-3").into(),
                ))
                .await
                .unwrap();
                {
                    let ack = read_ack(&mut ws).await;
                    acks.lock().unwrap().push(ack);
                }
                ws.send(WsMessage::text(
                    json!({"type": "disconnect", "reason": "refresh"}).to_string(),
                ))
                .await
                .unwrap();
                let _ = ws.close(None).await;
            }
        })
        .await;
        let server = slack(&url).await;
        let client =
            SocketClient::new(&server.uri(), "xapp-test").with_backoff(Duration::from_millis(10));
        let rec = Arc::new(Recorder::default());
        let shutdown = CancellationToken::new();
        let run = {
            let rec = rec.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move { client.run(rec, shutdown).await })
        };
        for _ in 0..3 {
            timeout(Duration::from_secs(5), rec.notify.notified())
                .await
                .unwrap();
        }
        shutdown.cancel();
        run.await.unwrap();

        let clicks = rec.clicks.lock().unwrap().clone();
        assert_eq!(clicks.len(), 3);
        assert_eq!(
            clicks[0],
            Interaction {
                request_id: "req-1".into(),
                approved: true,
                user_id: "U1".into(),
                user_name: "younsl".into()
            }
        );
        assert!(!clicks[1].approved);
        assert_eq!(clicks[2].request_id, "req-3");
        // The server records the last ack on its own task, so give it a moment.
        let deadline = Instant::now() + Duration::from_secs(5);
        while acks.lock().unwrap().len() < 5 && Instant::now() < deadline {
            sleep(Duration::from_millis(10)).await;
        }
        let acks = acks.lock().unwrap().clone();
        assert_eq!(acks.len(), 5);
        assert!(acks[0].contains("\"envelope_id\":\"e0\""));
        let states = rec.states.lock().unwrap().clone();
        assert_eq!(states, vec![true, false]);
    }

    #[tokio::test]
    async fn open_failures_are_retried_until_shutdown() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/apps.connections.open"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "invalid_auth"})),
            )
            .expect(2..)
            .mount(&server)
            .await;
        let client =
            SocketClient::new(&server.uri(), "xapp-test").with_backoff(Duration::from_millis(5));
        let rec = Arc::new(Recorder::default());
        let shutdown = CancellationToken::new();
        let run = {
            let rec = rec.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move { client.run(rec, shutdown).await })
        };
        sleep(Duration::from_millis(200)).await;
        shutdown.cancel();
        run.await.unwrap();
        assert!(rec.states.lock().unwrap().iter().all(|c| !c));
    }

    #[tokio::test]
    async fn open_error_shapes() {
        let server = MockServer::start().await;
        let client = SocketClient::new(&server.uri(), "xapp-test");
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        assert!(
            client
                .open()
                .await
                .unwrap_err()
                .to_string()
                .contains("HTTP 500")
        );
        server.reset().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "url": ""})))
            .mount(&server)
            .await;
        assert!(
            client
                .open()
                .await
                .unwrap_err()
                .to_string()
                .contains("no socket url")
        );
        let dead = SocketClient::new("http://127.0.0.1:9", "xapp-test");
        assert!(dead.open().await.is_err());
    }

    #[test]
    fn backoff_grows_and_caps() {
        let c = SocketClient::new("http://x", "xapp").with_backoff(Duration::from_secs(1));
        assert_eq!(c.wait(0), Duration::from_secs(1));
        let w1 = c.wait(1);
        assert!(
            w1 >= Duration::from_millis(500) && w1 <= Duration::from_secs(1),
            "{w1:?}"
        );
        let w9 = c.wait(9);
        assert!(w9 >= Duration::from_secs(15) && w9 <= MAX_BACKOFF, "{w9:?}");
        let w40 = c.wait(40);
        assert!(w40 <= MAX_BACKOFF);
    }

    #[test]
    fn decode_falls_back_to_name() {
        let payload = Payload {
            kind: "block_actions".into(),
            user: User {
                id: "U9".into(),
                username: String::new(),
                name: "legacy".into(),
            },
            actions: vec![Action {
                action_id: ACTION_DENY.into(),
                value: "r".into(),
            }],
        };
        let out = decode(&payload);
        assert_eq!(out[0].user_name, "legacy");
        assert!(!out[0].approved);
    }
}
