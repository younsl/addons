//! Routes Slack button clicks to whoever is waiting on them. The broker is the
//! authorization boundary: a click only counts if it names an outstanding
//! request and comes from a configured approver.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
#[cfg(test)]
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::slack::Interaction;

/// A resolved approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub approved: bool,
    pub user_id: String,
    pub user_name: String,
    pub at: DateTime<Utc>,
}

/// Nobody answered in time. The tunnel is left untouched.
#[cfg(test)]
#[derive(Debug, Error, PartialEq, Eq)]
pub enum WaitError {
    #[error("approval request timed out")]
    Timeout,
    #[error("wait cancelled")]
    Cancelled,
}

/// Tracks outstanding approval requests.
#[derive(Debug)]
pub struct Broker {
    pending: Mutex<HashMap<String, mpsc::Sender<Decision>>>,
    allowed: HashSet<String>,
}

/// The receiving half of a registration. Dropping it unregisters the request,
/// so a click landing afterwards is reported as no longer outstanding rather
/// than delivered to nobody.
pub struct Watch {
    request_id: String,
    rx: mpsc::Receiver<Decision>,
    broker: Arc<Broker>,
}

impl Watch {
    /// Waits for the decision. `None` means the broker itself went away.
    pub async fn recv(&mut self) -> Option<Decision> {
        self.rx.recv().await
    }

    /// Takes a decision that has already arrived, without waiting.
    pub fn try_recv(&mut self) -> Option<Decision> {
        self.rx.try_recv().ok()
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.broker.unregister(&self.request_id);
    }
}

impl Broker {
    /// Builds a broker that only accepts decisions from the given Slack user
    /// IDs.
    #[must_use]
    pub fn new(allowed_user_ids: &[String]) -> Arc<Self> {
        Arc::new(Self {
            pending: Mutex::new(HashMap::new()),
            allowed: allowed_user_ids.iter().cloned().collect(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, mpsc::Sender<Decision>>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Registers the request and hands back the channel its decision arrives
    /// on. Holding one registration across a whole wait is what lets a caller
    /// re-check preconditions on a ticker without a click landing between two
    /// short waits being dropped as no longer outstanding.
    #[must_use]
    pub fn watch(self: &Arc<Self>, request_id: &str) -> Watch {
        // A buffer of one keeps `handle` from blocking on a waiter that has not
        // reached its receive yet.
        let (tx, rx) = mpsc::channel(1);
        self.lock().insert(request_id.to_string(), tx);
        Watch {
            request_id: request_id.to_string(),
            rx,
            broker: self.clone(),
        }
    }

    /// Blocks until the request is answered or the timeout expires.
    #[cfg(test)]
    pub async fn wait(
        self: &Arc<Self>,
        request_id: &str,
        timeout: std::time::Duration,
    ) -> Result<Decision, WaitError> {
        let mut watch = self.watch(request_id);
        match tokio::time::timeout(timeout, watch.recv()).await {
            Ok(Some(d)) => Ok(d),
            Ok(None) => Err(WaitError::Cancelled),
            Err(_) => Err(WaitError::Timeout),
        }
    }

    fn unregister(&self, request_id: &str) {
        self.lock().remove(request_id);
    }

    /// The request IDs awaiting a decision, so the planner skips a tunnel
    /// already in front of an approver.
    #[must_use]
    pub fn pending(&self) -> HashSet<String> {
        self.lock().keys().cloned().collect()
    }

    /// Delivers one interaction. It is the Socket Mode callback and must not
    /// block. Clicks from unconfigured users, or for requests no longer
    /// outstanding, are logged and dropped; both are normal, since resolved
    /// cards stay clickable.
    pub fn handle(&self, i: &Interaction) {
        if !self.allowed.contains(&i.user_id) {
            warn!(
                user_id = %i.user_id,
                user_name = %i.user_name,
                request_id = %i.request_id,
                approved = i.approved,
                "ignoring Slack approval click from an unconfigured user"
            );
            return;
        }
        let Some(tx) = self.lock().remove(&i.request_id) else {
            info!(
                user_id = %i.user_id,
                request_id = %i.request_id,
                "ignoring Slack approval click for a request that is no longer outstanding"
            );
            return;
        };
        info!(
            user_id = %i.user_id,
            user_name = %i.user_name,
            request_id = %i.request_id,
            approved = i.approved,
            "received Slack approval decision"
        );
        // try_send: the buffer is one and this is the only send, so it cannot
        // be full; a closed receiver means the waiter already left.
        let _ = tx.try_send(Decision {
            approved: i.approved,
            user_id: i.user_id.clone(),
            user_name: i.user_name.clone(),
            at: Utc::now(),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn click(request_id: &str, user: &str, approved: bool) -> Interaction {
        Interaction {
            request_id: request_id.into(),
            approved,
            user_id: user.into(),
            user_name: "name".into(),
        }
    }

    #[tokio::test]
    async fn delivers_allowed_decisions() {
        let b = Broker::new(&["U1".to_string()]);
        let mut watch = b.watch("r1");
        assert!(b.pending().contains("r1"));
        b.handle(&click("r1", "U1", true));
        let d = watch.recv().await.unwrap();
        assert!(d.approved);
        assert_eq!(d.user_id, "U1");
        assert!(!b.pending().contains("r1"), "consumed on delivery");
        drop(watch);
        assert!(b.pending().is_empty());
    }

    #[tokio::test]
    async fn ignores_strangers_and_unknown_requests() {
        let b = Broker::new(&["U1".to_string()]);
        let mut watch = b.watch("r1");
        b.handle(&click("r1", "U2", true));
        assert!(watch.try_recv().is_none());
        assert!(
            b.pending().contains("r1"),
            "a stranger's click does not consume the request"
        );
        b.handle(&click("other", "U1", true));
        assert!(watch.try_recv().is_none());
        b.handle(&click("r1", "U1", false));
        assert!(!watch.try_recv().unwrap().approved);
    }

    #[tokio::test]
    async fn wait_times_out_and_unregisters() {
        let b = Broker::new(&["U1".to_string()]);
        let err = b.wait("r1", Duration::from_millis(20)).await.unwrap_err();
        assert_eq!(err, WaitError::Timeout);
        assert!(b.pending().is_empty());

        let b2 = b.clone();
        let handle = tokio::spawn(async move { b2.wait("r2", Duration::from_secs(5)).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        b.handle(&click("r2", "U1", true));
        assert!(handle.await.unwrap().unwrap().approved);
    }

    #[tokio::test]
    async fn early_decision_is_buffered() {
        let b = Broker::new(&["U1".to_string()]);
        let mut watch = b.watch("r1");
        b.handle(&click("r1", "U1", true));
        // The click arrived before anyone awaited; it is still there.
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(watch.recv().await.is_some());
        assert_eq!(WaitError::Cancelled.to_string(), "wait cancelled");
    }
}
