//! The Slack Web API surface: DMs, cards, thread replies, and edits.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tracing::{error, warn};

use super::level::Notice;

/// The Slack Web API base.
pub const DEFAULT_API_URL: &str = "https://slack.com/api";

/// Locates one posted Slack message. Progress updates are posted as replies
/// to it, and the original card is edited in place when the request is
/// resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRef {
    /// The DM channel with one approver.
    #[serde(rename = "channelID")]
    pub channel_id: String,
    /// The message timestamp, which is also the thread ID for replies.
    pub ts: String,
}

/// A configured approver with the name to print for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approver {
    /// The Slack user ID, which stays the authorization identity.
    pub id: String,
    /// For humans reading a log line. Falls back to the ID, so it is never
    /// empty.
    pub name: String,
}

/// A Web API call that failed.
#[derive(Debug, Error)]
pub enum SlackError {
    #[error("slack {method}: {source}")]
    Transport {
        method: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("slack {method}: HTTP {status}")]
    Status { method: &'static str, status: u16 },
    #[error("slack {method}: {error}")]
    Api { method: &'static str, error: String },
    #[error("could not open a DM channel with any of the {0} configured approvers")]
    NoApprovers(usize),
}

/// Posts to Slack over the Web API.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    api_url: String,
    bot_token: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Envelope {
    ok: bool,
    error: String,
    #[serde(flatten)]
    rest: Value,
}

impl Client {
    /// Builds a client. `bot_token` (`xoxb-`) authorizes posting.
    ///
    /// # Panics
    ///
    /// Panics when the HTTP client cannot be built, which only happens when
    /// the TLS backend is unusable.
    #[must_use]
    pub fn new(api_url: &str, bot_token: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .expect("build http client"),
            api_url: api_url.trim_end_matches('/').to_string(),
            bot_token: bot_token.to_string(),
        }
    }

    async fn call(&self, method: &'static str, body: Value) -> Result<Value, SlackError> {
        let resp = self
            .http
            .post(format!("{}/{method}", self.api_url))
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .map_err(|source| SlackError::Transport { method, source })?;
        let status = resp.status();
        if status != reqwest::StatusCode::OK {
            return Err(SlackError::Status {
                method,
                status: status.as_u16(),
            });
        }
        let env: Envelope = resp
            .json()
            .await
            .map_err(|source| SlackError::Transport { method, source })?;
        if !env.ok {
            return Err(SlackError::Api {
                method,
                error: env.error,
            });
        }
        Ok(env.rest)
    }

    /// Verifies the bot token at startup, so a revoked token fails loudly
    /// rather than when maintenance is first queued. Returns the bot user name.
    pub async fn auth_test(&self) -> Result<String, SlackError> {
        let out = self.call("auth.test", json!({})).await?;
        Ok(out["user"].as_str().unwrap_or_default().to_string())
    }

    /// Resolves each Slack user ID to a DM channel ID. A partial failure is
    /// not fatal: one reachable approver is enough to authorize maintenance.
    pub async fn open_dms(&self, user_ids: &[String]) -> Result<Vec<String>, SlackError> {
        let mut channels = Vec::with_capacity(user_ids.len());
        for uid in user_ids {
            match self.call("conversations.open", json!({"users": uid})).await {
                Ok(out) => match out["channel"]["id"].as_str() {
                    Some(id) if !id.is_empty() => channels.push(id.to_string()),
                    _ => error!(user_id = %uid, "Slack returned no DM channel ID"),
                },
                Err(err) => error!(user_id = %uid, error = %err, "failed to open Slack DM channel"),
            }
        }
        if channels.is_empty() {
            return Err(SlackError::NoApprovers(user_ids.len()));
        }
        Ok(channels)
    }

    /// Puts a name to each configured user ID, so an operator can check the
    /// approver list against people rather than against opaque IDs.
    ///
    /// Cosmetic by design. A failed lookup, including the `users:read` scope
    /// not being granted, degrades to the bare ID instead of failing.
    pub async fn resolve_approvers(&self, user_ids: &[String]) -> Vec<Approver> {
        let mut out = Vec::with_capacity(user_ids.len());
        for uid in user_ids {
            let mut approver = Approver {
                id: uid.clone(),
                name: uid.clone(),
            };
            match self.call("users.info", json!({"user": uid})).await {
                Ok(v) => {
                    let name = display_name(&v["user"]);
                    if !name.is_empty() {
                        approver.name = name;
                    }
                }
                Err(err) => warn!(
                    user_id = %uid,
                    error = %err,
                    hint = "grant the users:read scope to print approver names",
                    "could not resolve the name of a configured approver; logging the ID only"
                ),
            }
            out.push(approver);
        }
        out
    }

    /// Sends a message to a channel and returns its reference.
    pub async fn post(
        &self,
        channel_id: &str,
        fallback: &str,
        blocks: &[Value],
    ) -> Result<MessageRef, SlackError> {
        let out = self
            .call(
                "chat.postMessage",
                json!({"channel": channel_id, "text": fallback, "blocks": blocks}),
            )
            .await?;
        Ok(MessageRef {
            channel_id: channel_id.to_string(),
            ts: out["ts"].as_str().unwrap_or_default().to_string(),
        })
    }

    /// Posts to every channel and returns the references that succeeded, so
    /// one unreachable approver does not block the others.
    pub async fn broadcast(
        &self,
        channel_ids: &[String],
        fallback: &str,
        blocks: &[Value],
    ) -> Vec<MessageRef> {
        let mut refs = Vec::with_capacity(channel_ids.len());
        for ch in channel_ids {
            match self.post(ch, fallback, blocks).await {
                Ok(r) => refs.push(r),
                Err(err) => error!(channel = %ch, error = %err, "failed to post Slack message"),
            }
        }
        refs
    }

    /// Posts a threaded reply under each referenced message, so every progress
    /// step lands under the card the approver clicked. The level and the VPN
    /// connection are rendered here, so a reply cannot reach Slack without
    /// either.
    pub async fn reply(&self, refs: &[MessageRef], notice: &Notice) {
        let text = notice.render();
        for r in refs {
            if let Err(err) = self
                .call(
                    "chat.postMessage",
                    json!({"channel": r.channel_id, "text": text, "thread_ts": r.ts}),
                )
                .await
            {
                error!(channel = %r.channel_id, thread_ts = %r.ts, error = %err, "failed to post Slack thread reply");
            }
        }
    }

    /// Rewrites each referenced message in place, replacing the buttons with
    /// the outcome so a resolved card cannot be clicked again.
    pub async fn update(&self, refs: &[MessageRef], fallback: &str, blocks: &[Value]) {
        for r in refs {
            if let Err(err) = self
                .call(
                    "chat.update",
                    json!({"channel": r.channel_id, "ts": r.ts, "text": fallback, "blocks": blocks}),
                )
                .await
            {
                error!(channel = %r.channel_id, ts = %r.ts, error = %err, "failed to update Slack message");
            }
        }
    }
}

/// Prefers what the person chose to be called, then their real name, then
/// the handle.
fn display_name(user: &Value) -> String {
    for key in [
        &user["profile"]["display_name"],
        &user["real_name"],
        &user["name"],
    ] {
        if let Some(s) = key.as_str()
            && !s.is_empty()
        {
            return s.to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::slack::level::Level;

    async fn ok(server: &MockServer, method_name: &str, body: Value) {
        Mock::given(method("POST"))
            .and(path(format!("/{method_name}")))
            .and(header("authorization", "Bearer xoxb-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn auth_test_and_errors() {
        let server = MockServer::start().await;
        let c = Client::new(&format!("{}/", server.uri()), "xoxb-test");
        ok(&server, "auth.test", json!({"ok": true, "user": "vpn-bot"})).await;
        assert_eq!(c.auth_test().await.unwrap(), "vpn-bot");

        server.reset().await;
        ok(
            &server,
            "auth.test",
            json!({"ok": false, "error": "invalid_auth"}),
        )
        .await;
        let err = c.auth_test().await.unwrap_err();
        assert_eq!(err.to_string(), "slack auth.test: invalid_auth");

        server.reset().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        assert!(matches!(
            c.auth_test().await,
            Err(SlackError::Status { status: 429, .. })
        ));

        let dead = Client::new("http://127.0.0.1:9", "xoxb-test");
        assert!(matches!(
            dead.auth_test().await,
            Err(SlackError::Transport { .. })
        ));
    }

    #[tokio::test]
    async fn open_dms_keeps_partial_success() {
        let server = MockServer::start().await;
        let c = Client::new(&server.uri(), "xoxb-test");
        Mock::given(method("POST"))
            .and(path("/conversations.open"))
            .and(body_partial_json(json!({"users": "U1"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": true, "channel": {"id": "D1"}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/conversations.open"))
            .and(body_partial_json(json!({"users": "U2"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "user_not_found"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/conversations.open"))
            .and(body_partial_json(json!({"users": "U3"})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "channel": {}})),
            )
            .mount(&server)
            .await;
        let channels = c
            .open_dms(&["U1".to_string(), "U2".to_string(), "U3".to_string()])
            .await
            .unwrap();
        assert_eq!(channels, vec!["D1".to_string()]);
        let err = c.open_dms(&["U2".to_string()]).await.unwrap_err();
        assert!(matches!(err, SlackError::NoApprovers(1)));
    }

    #[tokio::test]
    async fn resolve_approvers_degrades_to_ids() {
        let server = MockServer::start().await;
        let c = Client::new(&server.uri(), "xoxb-test");
        Mock::given(method("POST"))
            .and(path("/users.info"))
            .and(body_partial_json(json!({"user": "U1"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "user": {"name": "handle", "real_name": "Real", "profile": {"display_name": "younsl"}}})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/users.info"))
            .and(body_partial_json(json!({"user": "U2"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"ok": true, "user": {"name": "handle", "real_name": "Real"}}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/users.info"))
            .and(body_partial_json(json!({"user": "U3"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "missing_scope"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/users.info"))
            .and(body_partial_json(json!({"user": "U4"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "user": {}})))
            .mount(&server)
            .await;
        let out = c
            .resolve_approvers(&["U1".into(), "U2".into(), "U3".into(), "U4".into()])
            .await;
        let names: Vec<&str> = out.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["younsl", "Real", "U3", "U4"]);
        assert_eq!(display_name(&json!({"name": "h"})), "h");
    }

    #[tokio::test]
    async fn post_broadcast_reply_update() {
        let server = MockServer::start().await;
        let c = Client::new(&server.uri(), "xoxb-test");
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .and(body_partial_json(json!({"channel": "D1"})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "ts": "1.1"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .and(body_partial_json(json!({"channel": "D2"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "channel_not_found"})),
            )
            .mount(&server)
            .await;
        let refs = c
            .broadcast(
                &["D1".into(), "D2".into()],
                "fallback",
                &[json!({"type": "divider"})],
            )
            .await;
        assert_eq!(
            refs,
            vec![MessageRef {
                channel_id: "D1".into(),
                ts: "1.1".into()
            }]
        );

        server.reset().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .and(body_partial_json(json!({"channel": "D1", "thread_ts": "1.1", "text": "[INFO] VPN connection prod. step"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "ts": "1.2"})))
            .expect(1)
            .mount(&server)
            .await;
        c.reply(
            &refs,
            &Notice {
                level: Level::Info,
                target: "prod".into(),
                text: "step".into(),
            },
        )
        .await;
        // A failed reply is logged, not returned.
        c.reply(
            &[MessageRef {
                channel_id: "D2".into(),
                ts: "9".into(),
            }],
            &Notice {
                level: Level::Info,
                target: String::new(),
                text: "x".into(),
            },
        )
        .await;

        Mock::given(method("POST"))
            .and(path("/chat.update"))
            .and(body_partial_json(
                json!({"channel": "D1", "ts": "1.1", "text": "done"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .expect(1)
            .mount(&server)
            .await;
        c.update(&refs, "done", &[]).await;
        c.update(
            &[MessageRef {
                channel_id: "D3".into(),
                ts: "9".into(),
            }],
            "done",
            &[],
        )
        .await;
        let json = serde_json::to_string(&refs[0]).unwrap();
        assert_eq!(json, r#"{"channelID":"D1","ts":"1.1"}"#);
    }
}
