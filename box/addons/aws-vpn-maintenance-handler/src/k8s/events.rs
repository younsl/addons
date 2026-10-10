//! Publishes Kubernetes Events about tunnel replacements. The objects changed
//! are AWS tunnels, not cluster resources, so Events attach to the
//! controller's own Pod, giving an audit trail readable without Slack or
//! `CloudTrail`:
//!
//! ```text
//! kubectl -n kube-system describe pod <aws-vpn-maintenance-handler-pod>
//! ```

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use k8s_openapi::api::core::v1::{Event, EventSource, ObjectReference};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::api::{ObjectMeta, PostParams};
use thiserror::Error;
use tracing::warn;

const COMPONENT: &str = "aws-vpn-maintenance-handler";

/// Event reasons, stable because operators write selectors and alerts against
/// them.
pub mod reason {
    pub const MAINTENANCE_DETECTED: &str = "MaintenanceDetected";
    pub const APPROVAL_REQUESTED: &str = "ApprovalRequested";
    pub const APPROVED: &str = "ReplacementApproved";
    pub const DENIED: &str = "ReplacementDenied";
    pub const APPROVAL_TIMEOUT: &str = "ApprovalTimedOut";
    pub const APPROVAL_EXPIRED: &str = "ApprovalExpired";
    pub const REPLACING: &str = "ReplacingTunnel";
    pub const REPLACED: &str = "TunnelReplaced";
    pub const REPLACE_FAILED: &str = "TunnelReplaceFailed";
    pub const PEER_LOST: &str = "PeerTunnelLost";
    pub const HELD_BACK: &str = "MaintenanceHeldBack";
}

#[derive(Debug, Error)]
pub enum EventsError {
    #[error("POD_NAME and POD_NAMESPACE must be set (downward API)")]
    MissingPodIdentity,
}

/// Where Events go. The kube client satisfies it; tests capture them.
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn create(&self, event: Event) -> Result<(), String>;
}

/// Publishes the audit trail. Implementations must not block the caller.
pub trait Recorder: Send + Sync {
    /// Records an expected step.
    fn normal(&self, reason: &'static str, message: String);
    /// Records something that needs attention.
    fn warning(&self, reason: &'static str, message: String);
}

/// Publishes Events against the controller's own Pod.
pub struct Emitter {
    sink: Arc<dyn EventSink>,
    reference: ObjectReference,
    namespace: String,
}

impl Emitter {
    /// Builds an emitter from downward API values. The UID lets kubectl
    /// associate Events with the live Pod without granting permission to read
    /// Pods.
    pub fn new(
        sink: Arc<dyn EventSink>,
        pod_name: &str,
        pod_namespace: &str,
        pod_uid: &str,
    ) -> Result<Self, EventsError> {
        if pod_name.is_empty() || pod_namespace.is_empty() {
            return Err(EventsError::MissingPodIdentity);
        }
        Ok(Self {
            sink,
            reference: ObjectReference {
                kind: Some("Pod".into()),
                name: Some(pod_name.into()),
                namespace: Some(pod_namespace.into()),
                uid: Some(pod_uid.into()),
                ..ObjectReference::default()
            },
            namespace: pod_namespace.to_string(),
        })
    }

    /// Posts in the background. Slack and the state `ConfigMap` are the durable
    /// record; Events are the convenient one, so a failure is logged and
    /// dropped.
    fn emit(&self, kind: &str, reason: &'static str, message: String) {
        let now = Time(
            k8s_openapi::jiff::Timestamp::from_second(Utc::now().timestamp()).unwrap_or_default(),
        );
        let event = Event {
            metadata: ObjectMeta {
                generate_name: Some(format!(
                    "{}.",
                    self.reference.name.clone().unwrap_or_default()
                )),
                namespace: Some(self.namespace.clone()),
                ..ObjectMeta::default()
            },
            involved_object: self.reference.clone(),
            reason: Some(reason.to_string()),
            message: Some(message),
            type_: Some(kind.to_string()),
            source: Some(EventSource {
                component: Some(COMPONENT.into()),
                ..EventSource::default()
            }),
            reporting_component: Some(COMPONENT.into()),
            reporting_instance: self.reference.name.clone(),
            first_timestamp: Some(now.clone()),
            last_timestamp: Some(now),
            count: Some(1),
            ..Event::default()
        };
        let sink = self.sink.clone();
        tokio::spawn(async move {
            if let Err(err) = sink.create(event).await {
                warn!(reason, error = %err, "failed to publish a Kubernetes Event");
            }
        });
    }
}

impl Recorder for Emitter {
    fn normal(&self, reason: &'static str, message: String) {
        self.emit("Normal", reason, message);
    }

    fn warning(&self, reason: &'static str, message: String) {
        self.emit("Warning", reason, message);
    }
}

/// The kube-backed sink.
pub struct KubeEvents(kube::Api<Event>);

impl KubeEvents {
    #[must_use]
    pub fn new(client: kube::Client, namespace: &str) -> Self {
        Self(kube::Api::namespaced(client, namespace))
    }
}

#[async_trait]
impl EventSink for KubeEvents {
    async fn create(&self, event: Event) -> Result<(), String> {
        self.0
            .create(&PostParams::default(), &event)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Capture {
        events: Mutex<Vec<Event>>,
        fail: bool,
        seen: tokio::sync::Notify,
    }

    #[async_trait]
    impl EventSink for Capture {
        async fn create(&self, event: Event) -> Result<(), String> {
            self.events.lock().unwrap().push(event);
            self.seen.notify_one();
            if self.fail {
                Err("forbidden".into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn requires_pod_identity() {
        let sink = Arc::new(Capture::default());
        assert!(matches!(
            Emitter::new(sink.clone(), "", "ns", "uid"),
            Err(EventsError::MissingPodIdentity)
        ));
        assert!(matches!(
            Emitter::new(sink, "pod", "", "uid"),
            Err(EventsError::MissingPodIdentity)
        ));
    }

    #[tokio::test]
    async fn emits_against_the_pod() {
        let sink = Arc::new(Capture::default());
        let emitter = Emitter::new(sink.clone(), "pod-0", "kube-system", "uid-1").unwrap();
        emitter.normal(reason::REPLACED, "Tunnel 1.1.1.1 of prod: succeeded".into());
        sink.seen.notified().await;
        emitter.warning(reason::PEER_LOST, "peer dropped".into());
        sink.seen.notified().await;
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        let e = &events[0];
        assert_eq!(e.type_.as_deref(), Some("Normal"));
        assert_eq!(e.reason.as_deref(), Some("TunnelReplaced"));
        assert_eq!(e.involved_object.uid.as_deref(), Some("uid-1"));
        assert_eq!(e.involved_object.kind.as_deref(), Some("Pod"));
        assert_eq!(e.metadata.namespace.as_deref(), Some("kube-system"));
        assert_eq!(e.metadata.generate_name.as_deref(), Some("pod-0."));
        assert_eq!(e.reporting_component.as_deref(), Some(COMPONENT));
        assert_eq!(events[1].type_.as_deref(), Some("Warning"));
    }

    #[tokio::test]
    async fn sink_failures_are_logged_not_raised() {
        let sink = Arc::new(Capture {
            fail: true,
            ..Capture::default()
        });
        let emitter = Emitter::new(sink.clone(), "pod-0", "ns", "").unwrap();
        emitter.normal(reason::APPROVED, "x".into());
        sink.seen.notified().await;
        assert_eq!(sink.events.lock().unwrap().len(), 1);
    }
}
