//! ConfigMap-backed notes, joined into report responses at read time.
//!
//! Notes are the one piece of report state a human types, so they cannot live
//! on an `emptyDir` that is discarded on every scraper restart. One ConfigMap
//! holds them all, mirroring how `alerts::store` keeps every alert rule in one
//! object.
//!
//! Report identity is `(cluster, report_type, namespace, name)`, which can
//! contain characters a ConfigMap key rejects, so the key is the SHA-256 hex
//! of that tuple and the value carries the tuple back alongside the note.
//! Nothing in the query layer filters or sorts on notes — they are only
//! projected into list and detail responses — so the whole object is watched
//! into memory and merged after the store call returns.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use futures::StreamExt;
use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::{
    Client,
    api::{Api, Patch, PatchParams, PostParams},
    runtime::watcher::{Config as WatcherConfig, Event, watcher},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tracing::{debug, error, info, warn};

use super::models::ReportMeta;

/// A ConfigMap caps at roughly 1MiB in total. That is ample for the current
/// volume but it is a hard wall rather than a soft one, so writes are rejected
/// before the object can become untruncatable.
pub const MAX_NOTE_BYTES: usize = 8 * 1024;
pub const MAX_TOTAL_BYTES: usize = 800 * 1024;

#[derive(Debug, Error)]
pub enum NotesError {
    #[error("note exceeds {MAX_NOTE_BYTES} bytes")]
    NoteTooLarge,
    #[error(
        "notes ConfigMap would exceed {MAX_TOTAL_BYTES} bytes; delete some notes before adding more"
    )]
    StoreFull,
    #[error("kube API error: {0}")]
    Kube(#[from] kube::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// One stored note. The identity tuple is carried in the value because the key
/// is a digest and cannot be reversed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub cluster: String,
    pub report_type: String,
    pub namespace: String,
    pub name: String,
    pub notes: String,
    pub notes_created_at: Option<String>,
    pub notes_updated_at: Option<String>,
}

/// Stable ConfigMap key for a report identity.
pub fn note_key(cluster: &str, report_type: &str, namespace: &str, name: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [cluster, report_type, namespace, name] {
        hasher.update(part.as_bytes());
        hasher.update(b"/");
    }
    hex::encode(hasher.finalize())
}

/// In-memory projection of the notes ConfigMap. Split from `NotesStore` so the
/// merge and accounting rules are testable without a cluster.
#[derive(Debug, Default)]
pub struct NotesCache {
    notes: RwLock<HashMap<String, Note>>,
}

impl NotesCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Note>> {
        self.notes.read().expect("notes cache poisoned")
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Note>> {
        self.notes.write().expect("notes cache poisoned")
    }

    pub fn get(&self, key: &str) -> Option<Note> {
        self.read().get(key).cloned()
    }

    pub fn insert(&self, key: String, note: Note) {
        self.write().insert(key, note);
    }

    pub fn remove(&self, key: &str) {
        self.write().remove(key);
    }

    pub fn clear(&self) {
        self.write().clear();
    }

    /// Serialized size of the whole object, used for the size metric and for
    /// the pre-write ceiling check.
    pub fn bytes(&self) -> usize {
        self.read()
            .iter()
            .map(|(k, v)| k.len() + serde_json::to_string(v).map(|s| s.len()).unwrap_or(0))
            .sum()
    }

    /// Replace the contents with a freshly observed ConfigMap.
    pub fn absorb(&self, cm: &ConfigMap) {
        let mut next = HashMap::new();
        for (key, raw) in cm.data.iter().flatten() {
            match serde_json::from_str::<Note>(raw) {
                Ok(note) => {
                    next.insert(key.clone(), note);
                }
                Err(e) => warn!(key = %key, error = %e, "Skipping malformed note"),
            }
        }
        let count = next.len();
        *self.write() = next;
        debug!(notes = count, "Notes cache refreshed");
    }

    /// Join a note onto one report's metadata.
    pub fn merge_meta(&self, meta: &mut ReportMeta) {
        let key = note_key(
            &meta.cluster,
            &meta.report_type,
            &meta.namespace,
            &meta.name,
        );
        if let Some(note) = self.get(&key) {
            meta.notes = note.notes;
            meta.notes_created_at = note.notes_created_at;
            meta.notes_updated_at = note.notes_updated_at;
        }
    }

    /// Join notes onto a whole page of report metadata.
    pub fn merge_all(&self, metas: &mut [ReportMeta]) {
        if self.is_empty() {
            return;
        }
        for meta in metas.iter_mut() {
            self.merge_meta(meta);
        }
    }
}

#[derive(Clone)]
pub struct NotesStore {
    client: Client,
    namespace: String,
    configmap_name: String,
    cache: Arc<NotesCache>,
}

impl NotesStore {
    pub fn new(client: Client, namespace: String, configmap_name: String) -> Self {
        Self {
            client,
            namespace,
            configmap_name,
            cache: Arc::new(NotesCache::new()),
        }
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn configmap_name(&self) -> &str {
        &self.configmap_name
    }

    pub fn cache(&self) -> &NotesCache {
        &self.cache
    }

    fn api(&self) -> Api<ConfigMap> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// Create the backing ConfigMap if it is missing. Concurrent creates from
    /// multiple server replicas race safely: the loser sees `AlreadyExists`.
    pub async fn ensure_exists(&self) -> Result<(), NotesError> {
        let api = self.api();
        if api.get_opt(&self.configmap_name).await?.is_some() {
            return Ok(());
        }
        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some(self.configmap_name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(super::managed_labels("trivy-collector-notes")),
                ..Default::default()
            },
            data: Some(Default::default()),
            ..Default::default()
        };
        match api.create(&PostParams::default(), &cm).await {
            Ok(_) => {
                info!(
                    namespace = %self.namespace,
                    name = %self.configmap_name,
                    "Created empty notes ConfigMap"
                );
                Ok(())
            }
            Err(kube::Error::Api(e)) if e.code == 409 => Ok(()),
            Err(e) => Err(NotesError::Kube(e)),
        }
    }

    /// Write (or clear) the note for one report. An empty note removes the key
    /// rather than storing a blank value, so the object does not accumulate
    /// dead weight against the 1MiB wall.
    pub async fn upsert(
        &self,
        cluster: &str,
        report_type: &str,
        namespace: &str,
        name: &str,
        notes: &str,
    ) -> Result<(), NotesError> {
        let key = note_key(cluster, report_type, namespace, name);

        if notes.trim().is_empty() {
            return self.delete(&key).await;
        }
        if notes.len() > MAX_NOTE_BYTES {
            return Err(NotesError::NoteTooLarge);
        }

        let existing = self.cache.get(&key);
        let note = build_note(
            cluster,
            report_type,
            namespace,
            name,
            notes,
            existing.as_ref(),
        );
        let json = serde_json::to_string(&note)?;

        // Reject a write that would push the object past the ceiling with a
        // clear error rather than letting the API server truncate it.
        if projected_bytes(self.cache.bytes(), &json, existing.as_ref()) > MAX_TOTAL_BYTES {
            return Err(NotesError::StoreFull);
        }

        self.ensure_exists().await?;
        let patch = serde_json::json!({ "data": { key.clone(): json } });
        self.api()
            .patch(
                &self.configmap_name,
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await?;

        // Update the cache eagerly so a read-after-write on the same replica
        // reflects the note without waiting for the watch to catch up.
        self.cache.insert(key, note);
        debug!(cluster = %cluster, name = %name, "Note patched into ConfigMap");
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), NotesError> {
        // Setting the key to null in a JSON merge patch removes it.
        let patch = serde_json::json!({ "data": { key: serde_json::Value::Null } });
        match self
            .api()
            .patch(
                &self.configmap_name,
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await
        {
            Ok(_) => {}
            // Nothing to clear if the object was never created.
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => return Err(NotesError::Kube(e)),
        }
        self.cache.remove(key);
        Ok(())
    }

    /// Watch the ConfigMap into memory. An API server GET per report render is
    /// not acceptable, so the object is streamed and cached the same way
    /// `hub::secret_watcher` handles cluster registrations.
    pub async fn run_watch(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let cfg =
            WatcherConfig::default().fields(&format!("metadata.name={}", self.configmap_name));
        let mut stream = watcher(self.api(), cfg).boxed();

        info!(
            namespace = %self.namespace,
            configmap = %self.configmap_name,
            "Notes ConfigMap watcher started"
        );

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    info!("Notes ConfigMap watcher shutting down");
                    break;
                }
                ev = stream.next() => {
                    match ev {
                        Some(Ok(Event::Apply(cm))) | Some(Ok(Event::InitApply(cm))) => {
                            self.cache.absorb(&cm);
                        }
                        Some(Ok(Event::Delete(_))) => {
                            warn!("Notes ConfigMap deleted — clearing cache");
                            self.cache.clear();
                        }
                        Some(Ok(Event::Init)) | Some(Ok(Event::InitDone)) => {}
                        Some(Err(e)) => error!(error = %e, "Notes ConfigMap watcher error"),
                        None => {
                            warn!("Notes ConfigMap watcher stream ended");
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// Build the note to store, preserving the original creation timestamp so an
/// edit does not look like a fresh note.
fn build_note(
    cluster: &str,
    report_type: &str,
    namespace: &str,
    name: &str,
    notes: &str,
    existing: Option<&Note>,
) -> Note {
    let now = chrono::Utc::now().to_rfc3339();
    Note {
        cluster: cluster.to_string(),
        report_type: report_type.to_string(),
        namespace: namespace.to_string(),
        name: name.to_string(),
        notes: notes.to_string(),
        notes_created_at: existing
            .and_then(|n| n.notes_created_at.clone())
            .or_else(|| Some(now.clone())),
        notes_updated_at: Some(now),
    }
}

/// Size the object would reach after this write: current total, plus the new
/// value, minus whatever the same key held before.
fn projected_bytes(current: usize, new_json: &str, existing: Option<&Note>) -> usize {
    let replaced = existing
        .and_then(|n| serde_json::to_string(n).ok())
        .map(|s| s.len())
        .unwrap_or(0);
    current
        .saturating_sub(replaced)
        .saturating_add(new_json.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(notes: &str) -> Note {
        Note {
            cluster: "prod".into(),
            report_type: "sbomreport".into(),
            namespace: "default".into(),
            name: "nginx".into(),
            notes: notes.into(),
            notes_created_at: Some("2026-01-01T00:00:00Z".into()),
            notes_updated_at: Some("2026-01-02T00:00:00Z".into()),
        }
    }

    fn meta(cluster: &str, report_type: &str, namespace: &str, name: &str) -> ReportMeta {
        ReportMeta {
            id: 1,
            cluster: cluster.to_string(),
            namespace: namespace.to_string(),
            name: name.to_string(),
            app: "app".to_string(),
            image: "img".to_string(),
            report_type: report_type.to_string(),
            summary: None,
            components_count: None,
            received_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            notes: String::new(),
            notes_created_at: None,
            notes_updated_at: None,
        }
    }

    #[test]
    fn note_key_is_stable_and_identity_scoped() {
        let a = note_key("prod", "sbomreport", "default", "nginx");
        assert_eq!(a, note_key("prod", "sbomreport", "default", "nginx"));
        assert_ne!(
            a,
            note_key("prod", "vulnerabilityreport", "default", "nginx")
        );
        assert_ne!(a, note_key("stage", "sbomreport", "default", "nginx"));
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn note_key_survives_names_a_configmap_key_would_reject() {
        let k = note_key("prod/eu", "sbomreport", "kube-system", "a:b/c");
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn merge_meta_joins_the_matching_note_only() {
        let cache = NotesCache::new();
        cache.insert(
            note_key("prod", "sbomreport", "default", "nginx"),
            note("check me"),
        );

        let mut m = meta("prod", "sbomreport", "default", "nginx");
        cache.merge_meta(&mut m);
        assert_eq!(m.notes, "check me");
        assert_eq!(m.notes_created_at.as_deref(), Some("2026-01-01T00:00:00Z"));

        let mut other = meta("prod", "sbomreport", "default", "redis");
        cache.merge_meta(&mut other);
        assert!(other.notes.is_empty());
    }

    #[test]
    fn merge_all_is_a_noop_on_an_empty_cache() {
        let cache = NotesCache::new();
        let mut metas = vec![meta("prod", "sbomreport", "default", "nginx")];
        cache.merge_all(&mut metas);
        assert!(metas[0].notes.is_empty());
    }

    #[test]
    fn merge_all_joins_every_row_it_knows() {
        let cache = NotesCache::new();
        cache.insert(
            note_key("prod", "sbomreport", "default", "nginx"),
            note("first"),
        );
        let mut metas = vec![
            meta("prod", "sbomreport", "default", "nginx"),
            meta("prod", "sbomreport", "default", "redis"),
        ];
        cache.merge_all(&mut metas);
        assert_eq!(metas[0].notes, "first");
        assert!(metas[1].notes.is_empty());
    }

    #[test]
    fn bytes_tracks_stored_notes() {
        let cache = NotesCache::new();
        assert_eq!(cache.bytes(), 0);
        cache.insert("k".into(), note("hello"));
        assert!(cache.bytes() > 0);
        assert_eq!(cache.len(), 1);
        cache.remove("k");
        assert_eq!(cache.bytes(), 0);
    }

    #[test]
    fn absorb_replaces_cache_and_skips_malformed_values() {
        let cache = NotesCache::new();
        cache.insert("stale".into(), note("gone"));

        let mut data = std::collections::BTreeMap::new();
        data.insert(
            "good".to_string(),
            serde_json::to_string(&note("n")).unwrap(),
        );
        data.insert("bad".to_string(), "{not json".to_string());
        let cm = ConfigMap {
            data: Some(data),
            ..Default::default()
        };
        cache.absorb(&cm);

        assert_eq!(cache.len(), 1);
        assert!(cache.get("good").is_some());
        assert!(cache.get("stale").is_none());
    }

    #[test]
    fn absorb_of_an_empty_configmap_clears_everything() {
        let cache = NotesCache::new();
        cache.insert("k".into(), note("x"));
        cache.absorb(&ConfigMap::default());
        assert!(cache.is_empty());
    }

    #[test]
    fn build_note_preserves_created_at_across_edits() {
        let first = build_note("prod", "sbomreport", "default", "nginx", "one", None);
        assert!(first.notes_created_at.is_some());

        let second = build_note(
            "prod",
            "sbomreport",
            "default",
            "nginx",
            "two",
            Some(&first),
        );
        assert_eq!(second.notes_created_at, first.notes_created_at);
        assert_eq!(second.notes, "two");
    }

    #[test]
    fn projected_bytes_subtracts_the_value_being_replaced() {
        let existing = note("old");
        let existing_len = serde_json::to_string(&existing).unwrap().len();
        // Replacing a key nets out to just the new value.
        assert_eq!(
            projected_bytes(existing_len, "abcd", Some(&existing)),
            "abcd".len()
        );
        // A brand new key adds on top of the current total.
        assert_eq!(projected_bytes(100, "abcd", None), 104);
    }

    #[test]
    fn note_roundtrips_through_json() {
        let n = note("keep me");
        let back: Note = serde_json::from_str(&serde_json::to_string(&n).unwrap()).unwrap();
        assert_eq!(back, n);
    }

    #[test]
    fn errors_name_the_limit_they_enforce() {
        assert!(NotesError::NoteTooLarge.to_string().contains("8192"));
        assert!(NotesError::StoreFull.to_string().contains("819200"));
    }
}
