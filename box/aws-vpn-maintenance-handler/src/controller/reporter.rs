//! Binds a Slack thread to the executor's progress interface, so the
//! replacement reads as one conversation.

use std::sync::Arc;

use async_trait::async_trait;
use tracing::{error, info, warn};

use super::Notifier;
use super::progress::Progress;
use crate::executor::Reporter;
use crate::slack::{Level, MessageRef, Notice};

/// Posts each step as a reply under the card that authorized it. `target` is
/// bound here rather than passed per message, so no step can be reported
/// without naming the VPN connection it belongs to. The progress counter is
/// bound for the same reason, and stays silent until a run actually starts.
pub struct ThreadReporter {
    client: Arc<dyn Notifier>,
    refs: Vec<MessageRef>,
    target: String,
    pub progress: Arc<Progress>,
}

impl ThreadReporter {
    #[must_use]
    pub fn new(client: Arc<dyn Notifier>, refs: Vec<MessageRef>, target: &str) -> Self {
        Self {
            client,
            refs,
            target: target.to_string(),
            progress: Arc::new(Progress::default()),
        }
    }

    /// The level reaches both the log and the Slack reply, so the two can be
    /// read side by side. Posting is not tied to the shutdown token on
    /// purpose: progress matters most when the controller is shutting down.
    async fn post(&self, level: Level, msg: String) {
        match level {
            Level::Info | Level::Success => {
                info!(vpn_connection = %self.target, level = %level, message = %msg, "replacement progress");
            }
            Level::Warn => {
                warn!(vpn_connection = %self.target, level = %level, message = %msg, "replacement alert");
            }
            _ => {
                error!(vpn_connection = %self.target, level = %level, message = %msg, "replacement alert");
            }
        }
        let notice = Notice {
            level,
            target: self.target.clone(),
            text: self.progress.with_progress(&msg),
        };
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.client.reply(&self.refs, &notice),
        )
        .await;
    }

    /// Posts a line at the given level.
    pub async fn at(&self, level: Level, msg: String) {
        self.post(level, msg).await;
    }
}

#[async_trait]
impl Reporter for ThreadReporter {
    async fn info(&self, msg: String) {
        self.post(Level::Info, msg).await;
    }
    async fn success(&self, msg: String) {
        self.post(Level::Success, msg).await;
    }
    async fn warn(&self, msg: String) {
        self.post(Level::Warn, msg).await;
    }
    async fn error(&self, msg: String) {
        self.post(Level::Error, msg).await;
    }
    async fn critical(&self, msg: String) {
        self.post(Level::Critical, msg).await;
    }
}
