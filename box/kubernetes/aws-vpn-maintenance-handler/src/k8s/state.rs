//! Persists the controller's memory in a `ConfigMap`, so no `PersistentVolume` is
//! needed. Three facts must survive a restart: a running replacement, which
//! still needs verifying and reporting, the per-connection cooldown, which
//! otherwise re-arms a connection that was just replaced, and which
//! maintenance the approvers have already been notified about.

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{ObjectMeta, PostParams};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::time;
use crate::slack::MessageRef;

/// The `ConfigMap` key holding the serialized snapshot.
const DATA_KEY: &str = "state.json";

/// How many conflict retries a mutation gets before giving up.
const MAX_RETRIES: usize = 5;

/// Where an in-flight replacement had got to when it was last recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// The AWS call is about to be made, or was made with an unknown result. A
    /// restart here must not assume it did not happen.
    #[default]
    Requested,
    /// Accepted, and the tunnel is being watched.
    Verifying,
    /// Nothing is in flight at AWS. One tunnel of an approved run is done and
    /// the next is waiting for the replaced one to become a peer worth failing
    /// over to.
    Waiting,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Requested => "requested",
            Self::Verifying => "verifying",
            Self::Waiting => "waiting",
        })
    }
}

/// The single replacement currently in progress. There is at most one across
/// all managed connections.
///
/// `tunnel_ip` means different things by phase: the tunnel being replaced in
/// `requested` and `verifying`, the tunnel about to be replaced in `waiting`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InFlight {
    #[serde(rename = "requestID")]
    pub request_id: String,
    #[serde(rename = "connectionID")]
    pub connection_id: String,
    #[serde(rename = "tunnelIP")]
    pub tunnel_ip: String,
    #[serde(rename = "peerIP")]
    pub peer_ip: String,
    pub phase: Phase,
    #[serde(with = "time::zero_as_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// When the whole approved run began, earlier than `started_at` for every
    /// tunnel after the first.
    #[serde(
        default,
        with = "time::optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub run_started_at: Option<DateTime<Utc>>,
    /// The Slack user who authorized this replacement.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub approved_by: String,
    /// Where progress goes, so a resumed run keeps reporting into the same
    /// Slack conversation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thread: Vec<MessageRef>,
    /// The connection's remaining tunnels, still covered by the same approval.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queue: Vec<String>,
    /// Tunnels already replaced under this approval.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub done: usize,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// An outstanding approval request waiting on a human.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    #[serde(rename = "requestID")]
    pub request_id: String,
    #[serde(with = "time::zero_as_none")]
    pub posted_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thread: Vec<MessageRef>,
}

/// The replacement history of one VPN connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_field_names)]
pub struct ConnectionRecord {
    /// Starts the cooldown. A failed attempt sets it too.
    #[serde(with = "time::zero_as_none")]
    pub last_replacement_at: Option<DateTime<Utc>>,
    #[serde(
        rename = "lastTunnelIP",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub last_tunnel_ip: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_result: String,
}

/// The whole persisted state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_flight: Option<InFlight>,
    /// Keyed by request ID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub approvals: BTreeMap<String, Approval>,
    /// When the approvers were first told about a request ID, so the detection
    /// notice is sent once per maintenance cycle rather than every pass.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub notices: BTreeMap<String, DateTime<Utc>>,
    /// Keyed by VPN connection ID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub connections: BTreeMap<String, ConnectionRecord>,
    #[serde(with = "time::zero_as_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// A state read or write that failed.
#[derive(Debug, Error)]
pub enum StateError {
    #[error("get state configmap {0}: {1}")]
    Get(String, String),
    #[error("create state configmap {0}: {1}")]
    Create(String, String),
    #[error("update state configmap {0}: {1}")]
    Update(String, String),
    #[error("decode {DATA_KEY} from configmap {0}: {1}")]
    Decode(String, String),
    #[error("encode state: {0}")]
    Encode(String),
}

/// The `ConfigMap` operations the store needs. The kube client satisfies it;
/// tests provide an in-memory one.
#[async_trait]
pub trait ConfigMapApi: Send + Sync {
    /// `Ok(None)` when the `ConfigMap` does not exist.
    async fn get(&self) -> Result<Option<ConfigMap>, String>;
    async fn create(&self, cm: ConfigMap) -> Result<ConfigMap, String>;
    /// `Err` with `conflict = true` when the resourceVersion is stale.
    async fn update(&self, cm: ConfigMap) -> Result<ConfigMap, (bool, String)>;
}

/// Reads and writes the snapshot in a `ConfigMap`.
pub struct Store {
    api: Box<dyn ConfigMapApi>,
    namespace: String,
    name: String,
}

impl Store {
    /// Builds a store over the given API.
    #[must_use]
    pub fn new(api: Box<dyn ConfigMapApi>, namespace: &str, name: &str) -> Self {
        Self {
            api,
            namespace: namespace.to_string(),
            name: name.to_string(),
        }
    }

    fn qualified(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }

    /// Reads the snapshot, returning an empty one when the `ConfigMap` or its key
    /// does not exist yet. A first run is not an error.
    pub async fn load(&self) -> Result<Snapshot, StateError> {
        match self.api.get().await {
            Ok(None) => Ok(Snapshot::default()),
            Ok(Some(cm)) => self.decode(&cm),
            Err(err) => Err(StateError::Get(self.qualified(), err)),
        }
    }

    /// Applies `f` to the current snapshot and persists it, retrying on
    /// conflict so a racing write cannot silently drop the in-flight record.
    pub async fn mutate(
        &self,
        f: impl Fn(&mut Snapshot) + Send + Sync,
    ) -> Result<Snapshot, StateError> {
        let mut last_err = String::new();
        for _ in 0..MAX_RETRIES {
            let existing = self
                .api
                .get()
                .await
                .map_err(|e| StateError::Get(self.qualified(), e))?;
            let mut snap = match &existing {
                Some(cm) => self.decode(cm)?,
                None => Snapshot::default(),
            };
            f(&mut snap);
            snap.updated_at = Some(Utc::now());
            let encoded = encode(&snap)?;

            match existing {
                None => {
                    let cm = ConfigMap {
                        metadata: ObjectMeta {
                            name: Some(self.name.clone()),
                            namespace: Some(self.namespace.clone()),
                            ..ObjectMeta::default()
                        },
                        data: Some(BTreeMap::from([(DATA_KEY.to_string(), encoded)])),
                        ..ConfigMap::default()
                    };
                    return match self.api.create(cm).await {
                        Ok(_) => Ok(snap),
                        Err(err) => Err(StateError::Create(self.qualified(), err)),
                    };
                }
                Some(mut cm) => {
                    cm.data
                        .get_or_insert_with(BTreeMap::new)
                        .insert(DATA_KEY.to_string(), encoded);
                    match self.api.update(cm).await {
                        Ok(_) => return Ok(snap),
                        Err((true, err)) => last_err = err,
                        Err((false, err)) => return Err(StateError::Update(self.qualified(), err)),
                    }
                }
            }
        }
        Err(StateError::Update(
            self.qualified(),
            format!("gave up after {MAX_RETRIES} conflicts: {last_err}"),
        ))
    }

    /// Records the start of a replacement, before the AWS call. A crash between
    /// the two then leaves state saying one may have started, so the controller
    /// verifies instead of issuing a second.
    pub async fn set_in_flight(&self, f: InFlight) -> Result<(), StateError> {
        self.mutate(|snap| {
            snap.approvals.remove(&f.request_id);
            snap.in_flight = Some(f.clone());
        })
        .await
        .map(|_| ())
    }

    /// Advances the phase of the in-flight replacement.
    pub async fn set_phase(&self, phase: Phase) -> Result<(), StateError> {
        self.mutate(|snap| {
            if let Some(f) = &mut snap.in_flight {
                f.phase = phase;
            }
        })
        .await
        .map(|_| ())
    }

    /// Clears the in-flight record and starts the cooldown in one write, so a
    /// crash cannot leave no in-flight and no cooldown.
    pub async fn finish_in_flight(
        &self,
        connection_id: &str,
        tunnel_ip: &str,
        result: &str,
        at: DateTime<Utc>,
    ) -> Result<(), StateError> {
        self.mutate(|snap| {
            snap.in_flight = None;
            snap.connections.insert(
                connection_id.to_string(),
                ConnectionRecord {
                    last_replacement_at: Some(at),
                    last_tunnel_ip: tunnel_ip.to_string(),
                    last_result: result.to_string(),
                },
            );
        })
        .await
        .map(|_| ())
    }

    /// Finishes one tunnel of an approved run and records the next in a single
    /// write: the cooldown for what was just replaced, and the waiting record
    /// for what comes next.
    pub async fn advance_chain(
        &self,
        connection_id: &str,
        tunnel_ip: &str,
        result: &str,
        at: DateTime<Utc>,
        next: InFlight,
    ) -> Result<(), StateError> {
        self.mutate(|snap| {
            snap.in_flight = Some(next.clone());
            snap.connections.insert(
                connection_id.to_string(),
                ConnectionRecord {
                    last_replacement_at: Some(at),
                    last_tunnel_ip: tunnel_ip.to_string(),
                    last_result: result.to_string(),
                },
            );
        })
        .await
        .map(|_| ())
    }

    /// Drops the record without touching the cooldown, for a run that ends
    /// between tunnels.
    pub async fn clear_in_flight(&self) -> Result<(), StateError> {
        self.mutate(|snap| snap.in_flight = None).await.map(|_| ())
    }

    /// Records an outstanding approval request.
    pub async fn add_approval(&self, a: Approval) -> Result<(), StateError> {
        self.mutate(|snap| {
            snap.approvals.insert(a.request_id.clone(), a.clone());
        })
        .await
        .map(|_| ())
    }

    /// Drops an approval request that was answered or expired.
    pub async fn remove_approval(&self, request_id: &str) -> Result<(), StateError> {
        self.mutate(|snap| {
            snap.approvals.remove(request_id);
        })
        .await
        .map(|_| ())
    }

    /// Records that the approvers have been told about `request_id`. Storing
    /// it is what makes the notice once-per-cycle across restarts and leader
    /// handovers.
    pub async fn add_notice(&self, request_id: &str, at: DateTime<Utc>) -> Result<(), StateError> {
        self.mutate(|snap| {
            snap.notices.insert(request_id.to_string(), at);
        })
        .await
        .map(|_| ())
    }

    /// Drops every notice whose request ID is no longer among the tunnels AWS
    /// reports maintenance for, so the `ConfigMap` does not grow forever.
    pub async fn prune_notices(&self, live: &HashSet<String>) -> Result<(), StateError> {
        self.mutate(|snap| snap.notices.retain(|id, _| live.contains(id)))
            .await
            .map(|_| ())
    }

    fn decode(&self, cm: &ConfigMap) -> Result<Snapshot, StateError> {
        let Some(raw) = cm
            .data
            .as_ref()
            .and_then(|d| d.get(DATA_KEY))
            .filter(|s| !s.is_empty())
        else {
            return Ok(Snapshot::default());
        };
        serde_json::from_str(raw).map_err(|e| StateError::Decode(self.qualified(), e.to_string()))
    }
}

/// Indented so `kubectl get cm -o yaml` stays readable.
fn encode(snap: &Snapshot) -> Result<String, StateError> {
    serde_json::to_string_pretty(snap).map_err(|e| StateError::Encode(e.to_string()))
}

/// The kube-backed API.
pub struct KubeConfigMaps {
    api: kube::Api<ConfigMap>,
    name: String,
}

impl KubeConfigMaps {
    #[must_use]
    pub fn new(client: kube::Client, namespace: &str, name: &str) -> Self {
        Self {
            api: kube::Api::namespaced(client, namespace),
            name: name.to_string(),
        }
    }
}

#[async_trait]
impl ConfigMapApi for KubeConfigMaps {
    async fn get(&self) -> Result<Option<ConfigMap>, String> {
        self.api
            .get_opt(&self.name)
            .await
            .map_err(|e| e.to_string())
    }

    async fn create(&self, cm: ConfigMap) -> Result<ConfigMap, String> {
        self.api
            .create(&PostParams::default(), &cm)
            .await
            .map_err(|e| e.to_string())
    }

    async fn update(&self, cm: ConfigMap) -> Result<ConfigMap, (bool, String)> {
        self.api
            .replace(&self.name, &PostParams::default(), &cm)
            .await
            .map_err(|e| {
                let conflict = matches!(&e, kube::Error::Api(resp) if resp.code == 409);
                (conflict, e.to_string())
            })
    }
}

#[cfg(test)]
pub mod fake {
    //! An in-memory `ConfigMap` with optimistic concurrency.

    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Default)]
    pub struct MemoryConfigMap {
        pub cm: Mutex<Option<ConfigMap>>,
        /// How many updates to reject with a conflict before accepting.
        pub conflicts: Mutex<usize>,
        pub fail_get: Mutex<Option<String>>,
        pub fail_write: Mutex<Option<String>>,
        pub writes: Mutex<usize>,
    }

    impl MemoryConfigMap {
        pub fn shared() -> Arc<Self> {
            Arc::default()
        }

        /// The stored snapshot, decoded.
        pub fn snapshot(&self) -> Snapshot {
            self.cm
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|cm| cm.data.as_ref())
                .and_then(|d| d.get(DATA_KEY))
                .map(|raw| serde_json::from_str(raw).unwrap())
                .unwrap_or_default()
        }

        pub fn seed(&self, snap: &Snapshot) {
            *self.cm.lock().unwrap() = Some(ConfigMap {
                metadata: ObjectMeta {
                    name: Some("state".into()),
                    resource_version: Some("1".into()),
                    ..ObjectMeta::default()
                },
                data: Some(BTreeMap::from([(
                    DATA_KEY.to_string(),
                    encode(snap).unwrap(),
                )])),
                ..ConfigMap::default()
            });
        }
    }

    #[async_trait]
    impl ConfigMapApi for MemoryConfigMap {
        async fn get(&self) -> Result<Option<ConfigMap>, String> {
            let err = self.fail_get.lock().unwrap().clone();
            if let Some(err) = err {
                return Err(err);
            }
            Ok(self.cm.lock().unwrap().clone())
        }

        async fn create(&self, mut cm: ConfigMap) -> Result<ConfigMap, String> {
            let err = self.fail_write.lock().unwrap().clone();
            if let Some(err) = err {
                return Err(err);
            }
            *self.writes.lock().unwrap() += 1;
            cm.metadata.resource_version = Some("1".into());
            *self.cm.lock().unwrap() = Some(cm.clone());
            Ok(cm)
        }

        async fn update(&self, mut cm: ConfigMap) -> Result<ConfigMap, (bool, String)> {
            let err = self.fail_write.lock().unwrap().clone();
            if let Some(err) = err {
                return Err((false, err));
            }
            let conflicted = {
                let mut conflicts = self.conflicts.lock().unwrap();
                let hit = *conflicts > 0;
                if hit {
                    *conflicts -= 1;
                }
                hit
            };
            if conflicted {
                return Err((true, "the object has been modified".into()));
            }
            *self.writes.lock().unwrap() += 1;
            let rv: u64 = cm
                .metadata
                .resource_version
                .as_deref()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            cm.metadata.resource_version = Some((rv + 1).to_string());
            *self.cm.lock().unwrap() = Some(cm.clone());
            Ok(cm)
        }
    }

    #[async_trait]
    impl<T: ConfigMapApi> ConfigMapApi for Arc<T> {
        async fn get(&self) -> Result<Option<ConfigMap>, String> {
            (**self).get().await
        }
        async fn create(&self, cm: ConfigMap) -> Result<ConfigMap, String> {
            (**self).create(cm).await
        }
        async fn update(&self, cm: ConfigMap) -> Result<ConfigMap, (bool, String)> {
            (**self).update(cm).await
        }
    }

    pub fn store(api: Arc<MemoryConfigMap>) -> Store {
        Store::new(Box::new(api), "kube-system", "state")
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    fn in_flight(phase: Phase) -> InFlight {
        InFlight {
            request_id: "vpn-1|1.1.1.1|1".into(),
            connection_id: "vpn-1".into(),
            tunnel_ip: "1.1.1.1".into(),
            peer_ip: "2.2.2.2".into(),
            phase,
            started_at: Some(at(1_785_000_000)),
            run_started_at: None,
            approved_by: "U1".into(),
            thread: vec![MessageRef {
                channel_id: "D1".into(),
                ts: "1.1".into(),
            }],
            queue: vec!["2.2.2.2".into()],
            done: 0,
        }
    }

    #[tokio::test]
    async fn first_run_is_empty_and_creates_on_write() {
        let api = MemoryConfigMap::shared();
        let store = store(api.clone());
        assert_eq!(store.load().await.unwrap(), Snapshot::default());

        store
            .set_in_flight(in_flight(Phase::Requested))
            .await
            .unwrap();
        let snap = store.load().await.unwrap();
        assert_eq!(snap.in_flight.as_ref().unwrap().phase, Phase::Requested);
        assert!(snap.updated_at.is_some());
        assert_eq!(*api.writes.lock().unwrap(), 1);

        store.set_phase(Phase::Verifying).await.unwrap();
        assert_eq!(api.snapshot().in_flight.unwrap().phase, Phase::Verifying);

        store
            .finish_in_flight("vpn-1", "1.1.1.1", "succeeded", at(1_785_000_600))
            .await
            .unwrap();
        let snap = api.snapshot();
        assert!(snap.in_flight.is_none());
        let rec = &snap.connections["vpn-1"];
        assert_eq!(rec.last_tunnel_ip, "1.1.1.1");
        assert_eq!(rec.last_result, "succeeded");
        assert_eq!(rec.last_replacement_at, Some(at(1_785_000_600)));
    }

    #[tokio::test]
    async fn approvals_notices_and_chain() {
        let api = MemoryConfigMap::shared();
        let store = store(api.clone());
        store
            .add_approval(Approval {
                request_id: "r1".into(),
                posted_at: Some(at(1)),
                thread: Vec::new(),
            })
            .await
            .unwrap();
        assert!(api.snapshot().approvals.contains_key("r1"));
        // Starting a replacement consumes its approval.
        let mut f = in_flight(Phase::Requested);
        f.request_id = "r1".into();
        store.set_in_flight(f).await.unwrap();
        assert!(api.snapshot().approvals.is_empty());

        store.remove_approval("missing").await.unwrap();

        let next = InFlight {
            phase: Phase::Waiting,
            tunnel_ip: "2.2.2.2".into(),
            peer_ip: "1.1.1.1".into(),
            queue: Vec::new(),
            done: 1,
            ..in_flight(Phase::Waiting)
        };
        store
            .advance_chain("vpn-1", "1.1.1.1", "succeeded", at(5), next.clone())
            .await
            .unwrap();
        let snap = api.snapshot();
        assert_eq!(snap.in_flight, Some(next));
        assert_eq!(snap.connections["vpn-1"].last_tunnel_ip, "1.1.1.1");
        store.clear_in_flight().await.unwrap();
        let snap = api.snapshot();
        assert!(snap.in_flight.is_none());
        assert!(snap.connections.contains_key("vpn-1"), "cooldown untouched");

        store.add_notice("n1", at(10)).await.unwrap();
        store.add_notice("n2", at(11)).await.unwrap();
        store
            .prune_notices(&HashSet::from(["n2".to_string()]))
            .await
            .unwrap();
        let notices = api.snapshot().notices;
        assert_eq!(notices.len(), 1);
        assert_eq!(notices["n2"], at(11));
    }

    #[tokio::test]
    async fn retries_conflicts_and_reports_failures() {
        let api = MemoryConfigMap::shared();
        api.seed(&Snapshot::default());
        let store = store(api.clone());
        *api.conflicts.lock().unwrap() = 2;
        store.add_notice("n", at(1)).await.unwrap();
        assert_eq!(*api.writes.lock().unwrap(), 1);

        *api.conflicts.lock().unwrap() = 99;
        let err = store.add_notice("n", at(1)).await.unwrap_err();
        assert!(
            err.to_string().contains("gave up after 5 conflicts"),
            "{err}"
        );

        *api.conflicts.lock().unwrap() = 0;
        *api.fail_write.lock().unwrap() = Some("forbidden".into());
        let err = store.add_notice("n", at(1)).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "update state configmap kube-system/state: forbidden"
        );

        *api.cm.lock().unwrap() = None;
        let err = store.add_notice("n", at(1)).await.unwrap_err();
        assert!(matches!(err, StateError::Create(..)), "{err}");
        *api.fail_write.lock().unwrap() = None;

        *api.fail_get.lock().unwrap() = Some("timeout".into());
        assert!(matches!(store.load().await, Err(StateError::Get(..))));
        assert!(matches!(
            store.clear_in_flight().await,
            Err(StateError::Get(..))
        ));
    }

    #[tokio::test]
    async fn decodes_go_written_state_and_rejects_garbage() {
        let api = MemoryConfigMap::shared();
        let go_json = r#"{
  "inFlight": {
    "requestID": "vpn-1|1.1.1.1|1785000000",
    "connectionID": "vpn-1",
    "tunnelIP": "1.1.1.1",
    "peerIP": "2.2.2.2",
    "phase": "verifying",
    "startedAt": "2026-07-27T02:14:09.113Z",
    "approvedBy": "U1",
    "thread": [{"channelID": "D1", "ts": "1.1"}],
    "queue": ["2.2.2.2"],
    "done": 1
  },
  "approvals": {"r": {"requestID": "r", "postedAt": "2026-07-27T02:00:00Z"}},
  "notices": {"n": "2026-07-27T01:00:00Z"},
  "connections": {"vpn-1": {"lastReplacementAt": "0001-01-01T00:00:00Z"}},
  "updatedAt": "2026-07-27T02:14:09Z"
}"#;
        *api.cm.lock().unwrap() = Some(ConfigMap {
            data: Some(BTreeMap::from([(
                DATA_KEY.to_string(),
                go_json.to_string(),
            )])),
            ..ConfigMap::default()
        });
        let store = store(api.clone());
        let snap = store.load().await.unwrap();
        let f = snap.in_flight.unwrap();
        assert_eq!(f.phase, Phase::Verifying);
        assert_eq!(f.done, 1);
        assert!(f.run_started_at.is_none());
        assert_eq!(f.thread[0].channel_id, "D1");
        assert!(snap.connections["vpn-1"].last_replacement_at.is_none());
        assert!(snap.approvals["r"].thread.is_empty());
        assert_eq!(Phase::Waiting.to_string(), "waiting");

        // An empty key is a fresh start.
        *api.cm.lock().unwrap() = Some(ConfigMap {
            data: Some(BTreeMap::from([(DATA_KEY.to_string(), String::new())])),
            ..ConfigMap::default()
        });
        assert_eq!(store.load().await.unwrap(), Snapshot::default());

        *api.cm.lock().unwrap() = Some(ConfigMap {
            data: Some(BTreeMap::from([(
                DATA_KEY.to_string(),
                "{oops".to_string(),
            )])),
            ..ConfigMap::default()
        });
        assert!(matches!(store.load().await, Err(StateError::Decode(..))));
    }

    #[test]
    fn encoding_is_go_shaped() {
        let snap = Snapshot {
            in_flight: Some(in_flight(Phase::Waiting)),
            ..Snapshot::default()
        };
        let json = encode(&snap).unwrap();
        assert!(
            json.contains("\"requestID\": \"vpn-1|1.1.1.1|1\""),
            "{json}"
        );
        assert!(json.contains("\"phase\": \"waiting\""), "{json}");
        assert!(
            json.contains("\"startedAt\": \"2026-07-25T17:20:00Z\""),
            "{json}"
        );
        assert!(!json.contains("runStartedAt"), "{json}");
        assert!(!json.contains("\"done\""), "{json}");
        assert!(
            json.contains("\"updatedAt\": \"0001-01-01T00:00:00Z\""),
            "{json}"
        );
    }
}
