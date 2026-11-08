//! Kubernetes plumbing shared by every role: the client, object labels,
//! agent Leases, and detection Events.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::api::core::v1::ObjectReference;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta, Time};
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::events::{Event, EventType, Recorder, Reporter};

use crate::policy::{Detection, Response};
use crate::rollout::AgentReport;

pub const APP_NAME: &str = "ward";
pub const LABEL_NAME: &str = "app.kubernetes.io/name";
pub const LABEL_INSTANCE: &str = "app.kubernetes.io/instance";
pub const LABEL_COMPONENT: &str = "app.kubernetes.io/component";
pub const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
pub const COMPONENT_AGENT: &str = "agent";
pub const COMPONENT_MANAGER: &str = "manager";
pub const MANAGER_FIELD: &str = "ward-operator";

pub const ANN_VERSION: &str = "ward.sh/version";
pub const ANN_EVENT_SOURCE: &str = "ward.sh/event-source";
pub const ANN_POLICY: &str = "ward.sh/policy-revision";
pub const ANN_CONTENT: &str = "ward.sh/content-digest";
pub const ANN_CONTENT_ERROR: &str = "ward.sh/content-error";
pub const ANN_DETECTIONS: &str = "ward.sh/detections";
pub const ANN_DETECTIONS_BY_CONTENT: &str = "ward.sh/detections-by-content";
pub const ANN_STARTED: &str = "ward.sh/started-at";

/// Seconds after which an agent Lease counts as stale.
pub const LEASE_SECONDS: i32 = 120;

#[derive(Debug, thiserror::Error)]
pub enum K8sError {
    #[error("no in-cluster config or kubeconfig: {0}")]
    Config(String),
    #[error(transparent)]
    Api(#[from] kube::Error),
}

pub async fn client() -> Result<kube::Client, K8sError> {
    kube::Client::try_default()
        .await
        .map_err(|e| K8sError::Config(e.to_string()))
}

pub fn time(t: DateTime<Utc>) -> Time {
    Time(k8s_openapi::jiff::Timestamp::from_millisecond(t.timestamp_millis()).unwrap_or_default())
}

pub fn micro_time(t: DateTime<Utc>) -> MicroTime {
    MicroTime(
        k8s_openapi::jiff::Timestamp::from_microsecond(t.timestamp_micros()).unwrap_or_default(),
    )
}

pub fn from_micro_time(t: &MicroTime) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_micros(t.0.as_microsecond())
}

/// Labels of every object the operator creates for the `Ward` `instance`.
pub fn labels(instance: &str, component: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (LABEL_NAME.to_string(), APP_NAME.to_string()),
        (LABEL_INSTANCE.to_string(), instance.to_string()),
        (LABEL_COMPONENT.to_string(), component.to_string()),
        (LABEL_MANAGED_BY.to_string(), MANAGER_FIELD.to_string()),
    ])
}

pub fn selector(instance: &str, component: &str) -> String {
    format!("{LABEL_NAME}={APP_NAME},{LABEL_INSTANCE}={instance},{LABEL_COMPONENT}={component}")
}

pub fn lease_name(node: &str) -> String {
    format!("ward-{node}")
}

/// What an agent publishes on its Lease.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentStatus {
    pub node: String,
    pub version: String,
    pub event_source: String,
    pub policy_revision: String,
    pub content_digest: Option<String>,
    pub content_error: Option<String>,
    pub detections: u64,
    pub detections_by_content: BTreeMap<String, u64>,
    pub started_at: Option<DateTime<Utc>>,
}

impl AgentStatus {
    pub fn lease(&self, instance: &str, now: DateTime<Utc>) -> Lease {
        let mut ann = BTreeMap::from([
            (ANN_VERSION.to_string(), self.version.clone()),
            (ANN_EVENT_SOURCE.to_string(), self.event_source.clone()),
            (ANN_POLICY.to_string(), self.policy_revision.clone()),
            (ANN_DETECTIONS.to_string(), self.detections.to_string()),
            (
                ANN_DETECTIONS_BY_CONTENT.to_string(),
                serde_json::to_string(&self.detections_by_content).unwrap_or_default(),
            ),
        ]);
        if let Some(d) = &self.content_digest {
            ann.insert(ANN_CONTENT.to_string(), d.clone());
        }
        if let Some(e) = &self.content_error {
            ann.insert(ANN_CONTENT_ERROR.to_string(), e.chars().take(512).collect());
        }
        if let Some(t) = self.started_at {
            ann.insert(ANN_STARTED.to_string(), t.to_rfc3339());
        }
        Lease {
            metadata: ObjectMeta {
                name: Some(lease_name(&self.node)),
                labels: Some(labels(instance, COMPONENT_AGENT)),
                annotations: Some(ann),
                ..ObjectMeta::default()
            },
            spec: Some(LeaseSpec {
                holder_identity: Some(self.node.clone()),
                lease_duration_seconds: Some(LEASE_SECONDS),
                renew_time: Some(micro_time(now)),
                ..LeaseSpec::default()
            }),
        }
    }

    /// Reads a Lease back. `None` when it is not an agent Lease or is stale.
    pub fn from_lease(lease: &Lease, now: DateTime<Utc>) -> Option<Self> {
        let spec = lease.spec.as_ref()?;
        let node = spec.holder_identity.clone()?;
        if lease.metadata.name.as_deref() != Some(lease_name(&node).as_str()) {
            return None;
        }
        let renewed = spec.renew_time.as_ref().and_then(from_micro_time)?;
        if (now - renewed).num_seconds() > i64::from(LEASE_SECONDS) {
            return None;
        }
        let ann = lease.metadata.annotations.as_ref()?;
        let get = |k: &str| ann.get(k).cloned();
        Some(Self {
            node,
            version: get(ANN_VERSION).unwrap_or_default(),
            event_source: get(ANN_EVENT_SOURCE).unwrap_or_default(),
            policy_revision: get(ANN_POLICY).unwrap_or_default(),
            content_digest: get(ANN_CONTENT),
            content_error: get(ANN_CONTENT_ERROR),
            detections: get(ANN_DETECTIONS)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            detections_by_content: get(ANN_DETECTIONS_BY_CONTENT)
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default(),
            started_at: get(ANN_STARTED)
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|t| t.with_timezone(&Utc)),
        })
    }

    pub fn report(&self) -> AgentReport {
        AgentReport {
            node: self.node.clone(),
            content_digest: self.content_digest.clone(),
            content_error: self.content_error.clone(),
            detections_by_content: self.detections_by_content.clone(),
        }
    }
}

/// Writes the agent's own Lease with server-side apply.
pub async fn apply_lease(
    client: &kube::Client,
    namespace: &str,
    lease: &Lease,
) -> Result<(), K8sError> {
    let api: Api<Lease> = Api::namespaced(client.clone(), namespace);
    let name = lease.metadata.name.clone().unwrap_or_default();
    api.patch(
        &name,
        &PatchParams::apply("ward-agent").force(),
        &Patch::Apply(lease),
    )
    .await?;
    Ok(())
}

pub const REASON_THREAT: &str = "ThreatDetected";
pub const REASON_BLOCKED: &str = "ExecBlocked";

/// The Event a detection publishes on its Pod.
pub fn detection_event(d: &Detection) -> Event {
    let blocked = d.actions.contains(&Response::Block);
    let actions: Vec<&str> = d.actions.iter().map(|a| a.as_str()).collect();
    let mut note = format!(
        "{} {} {} on {}: {}",
        d.severity.as_str(),
        d.engine.as_str(),
        d.rule,
        d.node,
        d.description
    );
    if let Some(f) = &d.file {
        let _ = write!(note, " file={}", f.path);
    }
    if let Some(p) = &d.process {
        let _ = write!(note, " pid={} comm={}", p.pid, p.comm);
    }
    let _ = write!(note, " actions={}", actions.join(","));
    Event {
        type_: EventType::Warning,
        reason: if blocked {
            REASON_BLOCKED
        } else {
            REASON_THREAT
        }
        .to_string(),
        note: Some(note.chars().take(1024).collect()),
        action: actions.last().copied().unwrap_or("Alert").to_string(),
        secondary: None,
    }
}

/// Publishes detection Events on the affected Pods.
pub struct Events {
    recorder: Recorder,
}

impl Events {
    pub fn new(client: kube::Client, controller: &str, instance: Option<String>) -> Self {
        Self {
            recorder: Recorder::new(
                client,
                Reporter {
                    controller: controller.to_string(),
                    instance,
                },
            ),
        }
    }

    pub async fn publish(&self, d: &Detection) -> Result<(), K8sError> {
        let Some(w) = &d.workload else {
            return Ok(());
        };
        let reference = ObjectReference {
            api_version: Some("v1".into()),
            kind: Some("Pod".into()),
            name: Some(w.pod.clone()),
            namespace: Some(w.namespace.clone()),
            uid: w.pod_uid.clone(),
            ..ObjectReference::default()
        };
        self.recorder
            .publish(&detection_event(d), &reference)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{FileInfo, ProcessInfo, Severity, Workload};

    #[test]
    fn lease_round_trips_and_goes_stale() {
        let now = Utc::now();
        let s = AgentStatus {
            node: "node-a".into(),
            version: "0.1.0".into(),
            event_source: "ebpf-lsm".into(),
            policy_revision: "abc".into(),
            content_digest: Some("sha256:x".into()),
            content_error: Some("boom".into()),
            detections: 7,
            detections_by_content: BTreeMap::from([("sha256:x".into(), 3)]),
            started_at: Some(DateTime::from_timestamp(1_700_000_000, 0).unwrap()),
        };
        let lease = s.lease("default", now);
        assert_eq!(lease.metadata.name.as_deref(), Some("ward-node-a"));
        assert_eq!(
            lease.metadata.labels.as_ref().unwrap()[LABEL_COMPONENT],
            COMPONENT_AGENT
        );
        assert_eq!(AgentStatus::from_lease(&lease, now).unwrap(), s);
        assert!(AgentStatus::from_lease(&lease, now + chrono::Duration::minutes(5)).is_none());
        let r = s.report();
        assert_eq!(r.node, "node-a");
        assert_eq!(r.detections_by_content["sha256:x"], 3);
        assert!(AgentStatus::from_lease(&Lease::default(), now).is_none());
        let mut forged = lease;
        forged.metadata.name = Some("ward-other-node".into());
        assert!(
            AgentStatus::from_lease(&forged, now).is_none(),
            "name must match the holder"
        );
        assert_eq!(
            selector("w", "agent"),
            "app.kubernetes.io/name=ward,app.kubernetes.io/instance=w,app.kubernetes.io/component=agent"
        );
        let t = time(now);
        assert_eq!(t.0.as_millisecond(), now.timestamp_millis());
    }

    #[test]
    fn events_describe_detections() {
        let mut d = Detection::new(
            "node-a",
            crate::policy::Engine::Signature,
            "Ward.Test.EICAR",
            Severity::High,
            "test".into(),
        );
        d.file = Some(FileInfo {
            path: "/tmp/eicar".into(),
            ..FileInfo::default()
        });
        d.process = Some(ProcessInfo {
            pid: 42,
            comm: "sh".into(),
            ..ProcessInfo::default()
        });
        d.workload = Some(Workload {
            namespace: "ns".into(),
            pod: "p".into(),
            ..Workload::default()
        });
        d.actions = vec![Response::Alert, Response::Quarantine];
        let e = detection_event(&d);
        assert_eq!(e.reason, REASON_THREAT);
        assert_eq!(e.action, "Quarantine");
        let note = e.note.unwrap();
        assert!(
            note.contains("file=/tmp/eicar") && note.contains("pid=42"),
            "{note}"
        );
        d.actions = vec![Response::Block];
        assert_eq!(detection_event(&d).reason, REASON_BLOCKED);
    }
}
