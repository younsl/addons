//! Publishes Kubernetes Events. Events about resize operations on standalone
//! EC2 instances attach to the controller's own Pod (resolved from the
//! downward API), so operators can read resize history via
//! `kubectl describe pod`. Events about cluster objects the addon observes
//! but does not own (Nodes, claims, volumes) attach to those objects and land
//! in the object's namespace, or in `default` for a cluster-scoped object,
//! the same as the kubelet's own Node Events.
//!
//! Repeating the same Event for the same object does not create a new Event
//! object: the emitter aggregates it into the existing one and increments its
//! count, which is what makes a per-object, per-pass Event affordable on a
//! large cluster. Delivery runs on a background task so a slow API server
//! never blocks a reconcile.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::{Event, EventSource, ObjectReference};
use kube::api::{ObjectMeta, Patch, PatchParams, PostParams};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::warn;

use super::k8s_time;

pub const COMPONENT: &str = "external-ebs-autoresizer";

pub const TYPE_NORMAL: &str = "Normal";
pub const TYPE_WARNING: &str = "Warning";

#[derive(Debug, Error)]
pub enum EventsError {
    #[error("POD_NAME and POD_NAMESPACE must be set (downward API)")]
    MissingPodIdentity,
}

/// Where Events go. The kube client satisfies it; tests capture them.
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn create(&self, namespace: &str, event: Event) -> Result<(), String>;
    /// Bumps the count and last timestamp of an existing Event.
    async fn bump(
        &self,
        namespace: &str,
        name: &str,
        count: i32,
        last: DateTime<Utc>,
    ) -> Result<(), String>;
}

/// The object an Event is recorded against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub kind: String,
    /// Empty for a cluster-scoped object, whose Events go to `default`.
    pub namespace: String,
    pub name: String,
    pub uid: String,
}

impl Target {
    #[must_use]
    pub fn pod(namespace: &str, name: &str, uid: &str) -> Self {
        Self::new("Pod", namespace, name, uid)
    }

    #[must_use]
    pub fn node(name: &str, uid: &str) -> Self {
        Self::new("Node", "", name, uid)
    }

    #[must_use]
    pub fn claim(namespace: &str, name: &str, uid: &str) -> Self {
        Self::new("PersistentVolumeClaim", namespace, name, uid)
    }

    #[must_use]
    pub fn volume(name: &str, uid: &str) -> Self {
        Self::new("PersistentVolume", "", name, uid)
    }

    fn new(kind: &str, namespace: &str, name: &str, uid: &str) -> Self {
        Self {
            kind: kind.into(),
            namespace: namespace.into(),
            name: name.into(),
            uid: uid.into(),
        }
    }

    /// The namespace the Event is stored in.
    fn event_namespace(&self) -> &str {
        if self.namespace.is_empty() {
            "default"
        } else {
            &self.namespace
        }
    }
}

/// One queued Event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub target: Target,
    pub event_type: String,
    pub reason: String,
    pub message: String,
}

/// Publishes Events through a background delivery task. Cloning shares the
/// queue.
#[derive(Clone)]
pub struct Emitter {
    /// Shared by every clone; `shutdown` takes it so the delivery task sees
    /// the queue close and drains it.
    tx: Arc<Mutex<Option<mpsc::UnboundedSender<Record>>>>,
    worker: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl Emitter {
    /// Starts the delivery task over `sink`.
    #[must_use]
    pub fn new(sink: Arc<dyn EventSink>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let worker = tokio::spawn(deliver(sink, rx));
        Self {
            tx: Arc::new(Mutex::new(Some(tx))),
            worker: Arc::new(Mutex::new(Some(worker))),
        }
    }

    /// Records an Event against `target`. Never blocks.
    pub fn event(&self, target: Target, event_type: &str, reason: &str, message: String) {
        if let Some(tx) = self
            .tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let _ = tx.send(Record {
                target,
                event_type: event_type.into(),
                reason: reason.into(),
                message,
            });
        }
    }

    /// Flushes queued Events and stops the delivery task. Call on exit so the
    /// final Events are delivered before the process terminates. Bounded, so
    /// an unreachable API server cannot hold shutdown hostage.
    pub async fn shutdown(&self) {
        // Closing the queue lets the delivery task drain what is left and
        // return; the timeout bounds how long that may take.
        self.tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let handle = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }
    }
}

/// One delivered Event's identity in the aggregation cache.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    namespace: String,
    kind: String,
    name: String,
    uid: String,
    event_type: String,
    reason: String,
    message: String,
}

async fn deliver(sink: Arc<dyn EventSink>, mut rx: mpsc::UnboundedReceiver<Record>) {
    let mut cache: HashMap<Key, (String, i32)> = HashMap::new();
    while let Some(rec) = rx.recv().await {
        let now = Utc::now();
        let ns = rec.target.event_namespace().to_string();
        let key = Key {
            namespace: ns.clone(),
            kind: rec.target.kind.clone(),
            name: rec.target.name.clone(),
            uid: rec.target.uid.clone(),
            event_type: rec.event_type.clone(),
            reason: rec.reason.clone(),
            message: rec.message.clone(),
        };
        if let Some((name, count)) = cache.get_mut(&key) {
            *count += 1;
            match sink.bump(&ns, name, *count, now).await {
                Ok(()) => continue,
                Err(err) => {
                    warn!(reason = %rec.reason, error = %err, "failed to update a Kubernetes Event, recreating it");
                    cache.remove(&key);
                }
            }
        }
        let name = format!(
            "{}.{:x}",
            rec.target.name,
            now.timestamp_nanos_opt().unwrap_or_default()
        );
        let event = build_event(&rec, &name, now);
        match sink.create(&ns, event).await {
            Ok(()) => {
                cache.insert(key, (name, 1));
            }
            Err(err) => {
                warn!(reason = %rec.reason, error = %err, "failed to publish a Kubernetes Event");
            }
        }
    }
}

fn build_event(rec: &Record, name: &str, now: DateTime<Utc>) -> Event {
    let t = rec.target.clone();
    let ts = k8s_time(now);
    Event {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(t.event_namespace().to_string()),
            ..ObjectMeta::default()
        },
        involved_object: ObjectReference {
            kind: Some(t.kind),
            api_version: Some("v1".into()),
            namespace: (!t.namespace.is_empty()).then_some(t.namespace),
            name: Some(t.name),
            uid: (!t.uid.is_empty()).then_some(t.uid),
            ..ObjectReference::default()
        },
        reason: Some(rec.reason.clone()),
        message: Some(rec.message.clone()),
        type_: Some(rec.event_type.clone()),
        source: Some(EventSource {
            component: Some(COMPONENT.into()),
            ..EventSource::default()
        }),
        reporting_component: Some(COMPONENT.into()),
        first_timestamp: Some(ts.clone()),
        last_timestamp: Some(ts),
        count: Some(1),
        ..Event::default()
    }
}

/// The kube-backed sink.
pub struct KubeEvents(kube::Client);

impl KubeEvents {
    #[must_use]
    pub const fn new(client: kube::Client) -> Self {
        Self(client)
    }
}

#[async_trait]
impl EventSink for KubeEvents {
    async fn create(&self, namespace: &str, event: Event) -> Result<(), String> {
        kube::Api::<Event>::namespaced(self.0.clone(), namespace)
            .create(&PostParams::default(), &event)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn bump(
        &self,
        namespace: &str,
        name: &str,
        count: i32,
        last: DateTime<Utc>,
    ) -> Result<(), String> {
        let patch = serde_json::json!({
            "count": count,
            "lastTimestamp": k8s_time(last),
        });
        kube::Api::<Event>::namespaced(self.0.clone(), namespace)
            .patch(name, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Builds the Pod-scoped emitter from downward API values. The UID lets
/// kubectl associate Events with the live Pod without granting permission to
/// read Pods.
pub fn pod_target(
    pod_name: &str,
    pod_namespace: &str,
    pod_uid: &str,
) -> Result<Target, EventsError> {
    if pod_name.is_empty() || pod_namespace.is_empty() {
        return Err(EventsError::MissingPodIdentity);
    }
    Ok(Target::pod(pod_namespace, pod_name, pod_uid))
}

#[cfg(test)]
pub(crate) mod capture {
    //! An in-memory sink for tests.

    use super::*;

    #[derive(Default)]
    pub struct Capture {
        pub events: Mutex<Vec<(String, Event)>>,
        pub bumps: Mutex<Vec<(String, String, i32)>>,
        pub fail_create: Mutex<bool>,
        pub fail_bump: Mutex<bool>,
    }

    #[async_trait]
    impl EventSink for Capture {
        async fn create(&self, namespace: &str, event: Event) -> Result<(), String> {
            let fail = *self.fail_create.lock().unwrap();
            if !fail {
                self.events
                    .lock()
                    .unwrap()
                    .push((namespace.to_string(), event));
            }
            if fail {
                Err("forbidden".into())
            } else {
                Ok(())
            }
        }

        async fn bump(
            &self,
            namespace: &str,
            name: &str,
            count: i32,
            _last: DateTime<Utc>,
        ) -> Result<(), String> {
            let fail = *self.fail_bump.lock().unwrap();
            if !fail {
                self.bumps
                    .lock()
                    .unwrap()
                    .push((namespace.to_string(), name.to_string(), count));
            }
            if fail { Err("gone".into()) } else { Ok(()) }
        }
    }

    impl Capture {
        /// Yields until `pred` holds, so a test can wait for the delivery task
        /// without sleeping blindly.
        pub async fn wait_until(&self, pred: impl Fn(&Self) -> bool) {
            for _ in 0..1000 {
                if pred(self) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            panic!("condition never held");
        }

        /// Collects `(kind, namespace, name, type, reason, message)` of every
        /// created Event.
        pub fn summary(&self) -> Vec<(String, String, String, String, String, String)> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|(ns, e)| {
                    (
                        e.involved_object.kind.clone().unwrap_or_default(),
                        ns.clone(),
                        e.involved_object.name.clone().unwrap_or_default(),
                        e.type_.clone().unwrap_or_default(),
                        e.reason.clone().unwrap_or_default(),
                        e.message.clone().unwrap_or_default(),
                    )
                })
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::capture::Capture;
    use super::*;

    #[test]
    fn pod_target_requires_identity() {
        assert!(matches!(
            pod_target("", "ns", "u"),
            Err(EventsError::MissingPodIdentity)
        ));
        assert!(matches!(
            pod_target("p", "", "u"),
            Err(EventsError::MissingPodIdentity)
        ));
        let t = pod_target("p", "ns", "u").unwrap();
        assert_eq!(t, Target::pod("ns", "p", "u"));
        assert_eq!(t.event_namespace(), "ns");
        assert_eq!(Target::node("n", "").event_namespace(), "default");
        assert_eq!(Target::volume("pv", "").event_namespace(), "default");
        assert_eq!(Target::claim("legacy", "c", "").event_namespace(), "legacy");
    }

    #[tokio::test]
    async fn emits_aggregates_and_recreates() {
        let sink = Arc::new(Capture::default());
        let emitter = Emitter::new(sink.clone());
        emitter.event(
            Target::pod("kube-system", "pod-0", "uid-1"),
            TYPE_NORMAL,
            "ResizeStarted",
            "a".into(),
        );
        emitter.event(
            Target::pod("kube-system", "pod-0", "uid-1"),
            TYPE_NORMAL,
            "ResizeStarted",
            "a".into(),
        );
        emitter.event(
            Target::node("node-1", ""),
            TYPE_WARNING,
            "VolumeModifyFailed",
            "b".into(),
        );
        emitter.event(
            Target::pod("kube-system", "pod-0", "uid-1"),
            TYPE_NORMAL,
            "ResizeStarted",
            "different".into(),
        );
        sink.wait_until(|s| s.events.lock().unwrap().len() == 3)
            .await;
        {
            let events = sink.events.lock().unwrap();
            assert_eq!(
                events.len(),
                3,
                "identical repeat is aggregated, different message is new"
            );
            let (ns, e) = &events[0];
            assert_eq!(ns, "kube-system");
            assert_eq!(e.metadata.namespace.as_deref(), Some("kube-system"));
            assert!(e.metadata.name.as_deref().unwrap().starts_with("pod-0."));
            assert_eq!(e.involved_object.kind.as_deref(), Some("Pod"));
            assert_eq!(e.involved_object.uid.as_deref(), Some("uid-1"));
            assert_eq!(e.involved_object.namespace.as_deref(), Some("kube-system"));
            assert_eq!(e.reporting_component.as_deref(), Some(COMPONENT));
            assert_eq!(
                e.source.as_ref().unwrap().component.as_deref(),
                Some(COMPONENT)
            );
            assert_eq!(e.count, Some(1));
            let (ns, e) = &events[1];
            assert_eq!(ns, "default", "cluster-scoped objects land in default");
            assert_eq!(e.involved_object.namespace, None);
            assert_eq!(e.involved_object.uid, None, "empty uid omitted");
            assert_eq!(e.type_.as_deref(), Some("Warning"));
        }
        let bumps = sink.bumps.lock().unwrap().clone();
        assert_eq!(bumps.len(), 1);
        assert_eq!(bumps[0].0, "kube-system");
        assert_eq!(bumps[0].2, 2);

        // A failed bump (the Event expired) recreates the Event.
        *sink.fail_bump.lock().unwrap() = true;
        emitter.event(
            Target::pod("kube-system", "pod-0", "uid-1"),
            TYPE_NORMAL,
            "ResizeStarted",
            "a".into(),
        );
        sink.wait_until(|s| s.events.lock().unwrap().len() == 4)
            .await;
        emitter.shutdown().await;
        emitter.event(
            Target::node("late", ""),
            TYPE_NORMAL,
            "Ignored",
            "after shutdown".into(),
        );
        assert_eq!(
            sink.events.lock().unwrap().len(),
            4,
            "events after shutdown are dropped"
        );
    }

    #[tokio::test]
    async fn create_failures_are_logged_not_raised() {
        let sink = Arc::new(Capture::default());
        *sink.fail_create.lock().unwrap() = true;
        let emitter = Emitter::new(sink.clone());
        emitter.event(
            Target::volume("pv-1", "u"),
            TYPE_NORMAL,
            "UnusedVolumeDetected",
            "x".into(),
        );
        emitter.shutdown().await;
        assert!(sink.events.lock().unwrap().is_empty());
        emitter.shutdown().await; // idempotent
    }
}
