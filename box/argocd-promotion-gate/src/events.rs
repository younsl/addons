//! Records gate verdicts as Kubernetes Events on the Application they were
//! about.
//!
//! The denial message already reaches whoever pressed Sync, through the Argo CD
//! error toast. It does not reach anybody looking at the Application
//! afterwards: the toast is gone, the webhook's own logs are in another
//! namespace, and the Application itself carries no trace of having been
//! refused. An Event closes that gap, so `kubectl describe application
//! prd-payment-api` answers "why did this not deploy" without anyone needing
//! access to the gate.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use k8s_openapi::api::core::v1::{Event, EventSource, ObjectReference};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
use kube::api::{Api, PostParams};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::gate::Decision;
use crate::observability::Metrics;

/// The Event source, so a reader can tell the gate's events apart from the
/// application controller's on the same Application.
const COMPONENT: &str = "argocd-promotion-gate";

/// Address the Application an Event hangs off. Written out rather than resolved
/// through a scheme because the gate reads Applications unstructured.
const APPLICATION_API_VERSION: &str = "argoproj.io/v1alpha1";
const APPLICATION_KIND: &str = "Application";

/// Marks a sync the gate refused.
pub const REASON_BLOCKED: &str = "PromotionBlocked";
/// Marks a sync the gate allowed while recording something about it.
pub const REASON_WARNED: &str = "PromotionWarning";

const TYPE_WARNING: &str = "Warning";
const TYPE_NORMAL: &str = "Normal";

/// Caps the Event message.
///
/// The verdict messages are written as prose for the Argo CD toast and run to
/// a few hundred bytes. The cap is not about those. It is about never handing
/// the API server an object whose size depends on how many images an
/// Application happens to declare.
const MAX_MESSAGE_BYTES: usize = 1024;

/// How long shutdown waits for queued events to be written.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Records a verdict. The admission handler holds one, or none when event
/// emission is switched off.
pub trait Emitter: Send + Sync {
    /// Records `verdict` against the named Application. It must not block: the
    /// caller is on the admission path.
    fn emit(&self, namespace: &str, name: &str, uid: &str, verdict: &Decision);
}

/// Writes one Event. The Kubernetes implementation is the only one in
/// production. The trait exists so the recorder can be tested without a
/// cluster.
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn create(&self, event: Event) -> Result<(), kube::Error>;
}

/// The Kubernetes-backed [`EventSink`].
pub struct KubeSink {
    api: Api<Event>,
}

impl KubeSink {
    #[must_use]
    pub fn new(client: kube::Client, namespace: &str) -> Self {
        Self {
            api: Api::namespaced(client, namespace),
        }
    }
}

#[async_trait]
impl EventSink for KubeSink {
    async fn create(&self, event: Event) -> Result<(), kube::Error> {
        self.api
            .create(&PostParams::default(), &event)
            .await
            .map(|_| ())
    }
}

/// The asynchronous [`Emitter`].
///
/// Delivery is asynchronous: a worker task owns the queue, which is why `emit`
/// can be called straight from the admission path. It also means an Event is
/// best effort. A verdict is never withheld because its Event could not be
/// written.
pub struct Recorder {
    tx: Mutex<Option<mpsc::UnboundedSender<Event>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    metrics: Arc<Metrics>,
}

impl Recorder {
    /// Builds a recorder writing Events through `sink`.
    #[must_use]
    pub fn new(sink: Arc<dyn EventSink>, metrics: Arc<Metrics>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
        let worker = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let app = event.involved_object.name.clone().unwrap_or_default();
                let namespace = event.involved_object.namespace.clone().unwrap_or_default();
                let event_type = event.type_.clone().unwrap_or_default();
                let reason = event.reason.clone().unwrap_or_default();
                // The success line is the point of the exercise. It is the only
                // confirmation that the reason for a refusal actually reached
                // the Application, as opposed to being queued and then quietly
                // dropped by RBAC.
                match sink.create(event).await {
                    Ok(()) => tracing::info!(
                        verb = "create",
                        app = %app,
                        namespace = %namespace,
                        r#type = %event_type,
                        reason = %reason,
                        "wrote a promotion gate event"
                    ),
                    Err(err) => tracing::warn!(
                        verb = "create",
                        app = %app,
                        namespace = %namespace,
                        r#type = %event_type,
                        reason = %reason,
                        error = %err,
                        "could not write a promotion gate event. The verdict itself was unaffected"
                    ),
                }
            }
        });
        Self {
            tx: Mutex::new(Some(tx)),
            worker: Mutex::new(Some(worker)),
            metrics,
        }
    }

    /// Drains the queue so events queued by the last requests are written
    /// before the process exits.
    pub async fn shutdown(&self) {
        if let Ok(mut tx) = self.tx.lock() {
            tx.take();
        }
        let worker = self.worker.lock().ok().and_then(|mut w| w.take());
        if let Some(worker) = worker
            && tokio::time::timeout(DRAIN_TIMEOUT, worker).await.is_err()
        {
            tracing::warn!("event queue did not drain before the shutdown deadline");
        }
    }
}

impl Emitter for Recorder {
    /// Records one verdict, or nothing when the verdict is unremarkable.
    ///
    /// A plain allow produces no Event on purpose. Most syncs pass, and an
    /// Event per pass would bury the denials it is meant to surface and cost a
    /// write on the admission path for nothing.
    fn emit(&self, namespace: &str, name: &str, uid: &str, verdict: &Decision) {
        let Some((event_type, reason, message)) = describe(verdict) else {
            tracing::debug!(
                app = %name,
                namespace = %namespace,
                code = %verdict.code,
                allowed = verdict.allowed,
                reason = "the verdict neither blocked the sync nor carried a warning",
                "no kubernetes event for this verdict"
            );
            return;
        };

        let event = build_event(namespace, name, uid, event_type, reason, &message);
        let queued = self
            .tx
            .lock()
            .ok()
            .and_then(|tx| tx.as_ref().map(|tx| tx.send(event).is_ok()))
            .unwrap_or(false);
        if !queued {
            tracing::warn!(app = %name, namespace = %namespace, "event queue is closed, dropping the event");
            return;
        }
        self.metrics.record_event(reason, event_type);

        // Queued rather than written. The write itself is reported by the
        // worker, well after this returns.
        tracing::debug!(
            app = %name,
            namespace = %namespace,
            r#type = %event_type,
            reason = %reason,
            uid = %uid,
            "queued a kubernetes event"
        );
    }
}

/// Builds the Event object for one verdict.
fn build_event(
    namespace: &str,
    name: &str,
    uid: &str,
    event_type: &str,
    reason: &str,
    message: &str,
) -> Event {
    let now = Time(k8s_openapi::jiff::Timestamp::now());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    Event {
        metadata: ObjectMeta {
            name: Some(format!("{name}.{nanos:x}")),
            namespace: Some(namespace.to_string()),
            ..ObjectMeta::default()
        },
        involved_object: ObjectReference {
            api_version: Some(APPLICATION_API_VERSION.to_string()),
            kind: Some(APPLICATION_KIND.to_string()),
            namespace: Some(namespace.to_string()),
            name: Some(name.to_string()),
            uid: (!uid.is_empty()).then(|| uid.to_string()),
            ..ObjectReference::default()
        },
        reason: Some(reason.to_string()),
        message: Some(message.to_string()),
        type_: Some(event_type.to_string()),
        source: Some(EventSource {
            component: Some(COMPONENT.to_string()),
            ..EventSource::default()
        }),
        reporting_component: Some(COMPONENT.to_string()),
        first_timestamp: Some(now.clone()),
        last_timestamp: Some(now),
        count: Some(1),
        ..Event::default()
    }
}

/// Maps a verdict onto an Event, or `None` when one is not warranted.
///
/// A verdict that simply passed writes nothing. Argo CD already records the
/// sync itself, and a line saying the gate agreed would add a duplicate that
/// the event TTL deletes within the hour anyway. The two places that answer
/// "was this checked" are `decisions_total` and the allow line in the log.
fn describe(verdict: &Decision) -> Option<(&'static str, &'static str, String)> {
    if !verdict.allowed {
        return Some((TYPE_WARNING, REASON_BLOCKED, truncate(&verdict.message)));
    }
    if !verdict.warnings.is_empty() {
        return Some((TYPE_NORMAL, REASON_WARNED, truncate(&verdict.message)));
    }
    None
}

/// Bounds the message and says so in a sentence.
///
/// The cut ends the text mid-word, so it is followed by a statement rather than
/// by an ellipsis or a dash. Everything an event carries has to read as plain
/// prose: punctuation that stands in for words renders inconsistently across
/// kubectl, the Argo CD UI, and whatever ships the event onward.
fn truncate(message: &str) -> String {
    const NOTE: &str = " The rest of this message was cut because the event was too long.";
    if message.len() <= MAX_MESSAGE_BYTES {
        return message.to_string();
    }
    let mut cut = MAX_MESSAGE_BYTES - NOTE.len();
    while !message.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{NOTE}", &message[..cut])
}

#[cfg(test)]
pub mod testing {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use k8s_openapi::api::core::v1::Event;

    use super::EventSink;

    /// Records every Event handed to it, optionally failing each write.
    #[derive(Default)]
    pub struct FakeSink {
        pub events: Mutex<Vec<Event>>,
        pub fail: bool,
    }

    #[async_trait]
    impl EventSink for FakeSink {
        async fn create(&self, event: Event) -> Result<(), kube::Error> {
            if self.fail {
                return Err(kube::Error::Api(Box::new(kube::core::Status::failure(
                    "forbidden",
                    "Forbidden",
                ))));
            }
            self.events.lock().unwrap().push(event);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeSink;
    use super::*;
    use crate::gate::Code;

    fn verdict(allowed: bool, warnings: &[&str]) -> Decision {
        Decision {
            allowed,
            code: if allowed {
                Code::Passed
            } else {
                Code::UpstreamOutOfSync
            },
            message: "Sync of prd-api is blocked.".into(),
            warnings: warnings.iter().map(ToString::to_string).collect(),
            ..Decision::not_gated("prd-api", "prd", "api")
        }
    }

    async fn settle(recorder: &Recorder, sink: &FakeSink, want: usize) {
        for _ in 0..50 {
            if sink.events.lock().unwrap().len() >= want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        recorder.shutdown().await;
    }

    #[test]
    fn describe_only_blocked_and_warned() {
        assert!(describe(&verdict(true, &[])).is_none());
        let (t, r, _) = describe(&verdict(false, &[])).unwrap();
        assert_eq!((t, r), (TYPE_WARNING, REASON_BLOCKED));
        let (t, r, _) = describe(&verdict(true, &["careful"])).unwrap();
        assert_eq!((t, r), (TYPE_NORMAL, REASON_WARNED));
    }

    #[test]
    fn truncate_bounds_and_explains() {
        let short = "fits";
        assert_eq!(truncate(short), short);
        let long = "é".repeat(2000);
        let cut = truncate(&long);
        assert!(cut.len() <= MAX_MESSAGE_BYTES, "{}", cut.len());
        assert!(cut.ends_with("too long."));
    }

    #[tokio::test]
    async fn recorder_writes_blocked_and_warned_verdicts_only() {
        let sink = Arc::new(FakeSink::default());
        let metrics = Arc::new(Metrics::new());
        let recorder = Recorder::new(
            Arc::clone(&sink) as Arc<dyn EventSink>,
            Arc::clone(&metrics),
        );

        recorder.emit("argocd", "prd-api", "uid-1", &verdict(true, &[]));
        recorder.emit("argocd", "prd-api", "uid-1", &verdict(false, &[]));
        recorder.emit("argocd", "prd-api", "", &verdict(true, &["warned"]));
        settle(&recorder, &sink, 2).await;

        let events = sink.events.lock().unwrap().clone();
        assert_eq!(events.len(), 2, "plain allow writes nothing");
        let blocked = &events[0];
        assert_eq!(blocked.reason.as_deref(), Some(REASON_BLOCKED));
        assert_eq!(blocked.type_.as_deref(), Some(TYPE_WARNING));
        assert_eq!(
            blocked.involved_object.kind.as_deref(),
            Some(APPLICATION_KIND)
        );
        assert_eq!(blocked.involved_object.uid.as_deref(), Some("uid-1"));
        assert!(
            blocked
                .metadata
                .name
                .as_deref()
                .unwrap()
                .starts_with("prd-api.")
        );
        assert_eq!(
            blocked.source.as_ref().unwrap().component.as_deref(),
            Some(COMPONENT)
        );
        assert!(events[1].involved_object.uid.is_none());

        let text = metrics.encode().unwrap();
        assert!(
            text.contains(r#"events_total{reason="PromotionBlocked",type="Warning"} 1"#),
            "{text}"
        );
        assert!(
            text.contains(r#"events_total{reason="PromotionWarning",type="Normal"} 1"#),
            "{text}"
        );
    }

    #[tokio::test]
    async fn recorder_survives_sink_failures_and_shutdown() {
        let sink = Arc::new(FakeSink {
            fail: true,
            ..FakeSink::default()
        });
        let metrics = Arc::new(Metrics::new());
        let recorder = Recorder::new(
            Arc::clone(&sink) as Arc<dyn EventSink>,
            Arc::clone(&metrics),
        );
        recorder.emit("argocd", "prd-api", "uid", &verdict(false, &[]));
        recorder.shutdown().await;
        recorder.shutdown().await;
        recorder.emit("argocd", "prd-api", "uid", &verdict(false, &[]));
        let text = metrics.encode().unwrap();
        assert!(
            text.contains(r#"events_total{reason="PromotionBlocked",type="Warning"} 1"#),
            "after shutdown nothing is counted: {text}"
        );
    }
}
