//! The OCI format is excluded from the idle retention reaper and the LRU cache
//! eviction: both delete artifact rows individually, and deleting one layer row
//! out from under a still-tagged manifest breaks an image irreversibly once the
//! sweeper reclaims the bytes. Space is instead reclaimed here, by reachability:
//! a manifest is live while a tag points at it (directly, or through a tagged
//! index); a blob is live while a live manifest references it. Everything else
//! — untagged manifests, layers of deleted images, blobs of pushes whose
//! manifest never arrived — is deleted after a grace period, and the ordinary
//! blob sweeper reclaims the freed bytes.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::meta::{self, Artifact, Repository};

use super::Manager;
use super::oci::OCI_DIGEST_RE;
use super::oci_push::{DEFAULT_OCI_MAX_MANIFEST_BYTES, OciManifestDoc};

/// Bounds deletions per repository per pass, keeping the single-writer SQLite
/// responsive.
const OCI_PRUNE_BATCH: usize = 512;

/// How long an unreachable OCI artifact row must have existed before the prune
/// may delete it. It protects the push window: blobs upload before the manifest
/// that will reference them, so a row must never be collected merely because its
/// manifest has not arrived yet.
pub(crate) const OCI_PRUNE_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

impl Manager {
    /// Periodically prunes unreachable OCI objects and expired upload sessions.
    /// Leader-gated by the caller like `run_sweeper`, so only one instance
    /// mutates the store.
    pub async fn run_oci_prune(
        self: Arc<Self>,
        cancel: CancellationToken,
        interval: Duration,
        session_ttl: Duration,
    ) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => {
                    if let Err(err) = self.prune_oci_once(session_ttl).await {
                        tracing::error!(err = %err, "oci prune failed");
                    }
                }
            }
        }
    }

    /// Sweeps every OCI repository once and expires stale upload sessions,
    /// returning how many artifact rows it deleted.
    pub(crate) async fn prune_oci_once(&self, session_ttl: Duration) -> Result<usize, meta::Error> {
        let mut total = 0;
        let repos = self.store.list_repositories().await?;
        for repo in repos {
            if repo.format != meta::FORMAT_OCI || repo.r#type == meta::TYPE_GROUP {
                continue;
            }
            match self.prune_oci_repo(&repo).await {
                Ok(n) => total += n,
                Err(err) => {
                    tracing::error!(repo = %repo.name, err = %err, "oci prune repo failed");
                    continue;
                }
            }
        }
        if let Err(err) = self.expire_oci_sessions(session_ttl).await {
            tracing::error!(err = %err, "oci session expiry failed");
        }
        Ok(total)
    }

    /// Computes the live set for one repository and deletes what falls outside
    /// it.
    async fn prune_oci_repo(&self, repo: &Repository) -> Result<usize, meta::Error> {
        let tags = self.store.list_oci_tag_rows(repo.id).await?;
        let arts = self.store.list_artifacts(repo.id, "").await?;

        // Index stored manifests by (name, digest) so reachability can walk from
        // tagged digests through index documents without re-querying.
        let mut manifests: HashMap<String, Artifact> = HashMap::new();
        let mut blobs: HashMap<String, Artifact> = HashMap::new();
        for art in arts {
            if let Some((name, digest)) = split_oci_path(&art.path, "/manifests/") {
                manifests.insert(format!("{name}\u{0}{digest}"), art);
                continue;
            }
            if let Some((name, digest)) = split_oci_path(&art.path, "/blobs/") {
                blobs.insert(format!("{name}\u{0}{digest}"), art);
            }
        }

        // Walk: tagged manifests are roots; an index reaches its children; a
        // manifest reaches its config and layer blobs. Parsed documents are kept
        // so the referrer pass below can consult subjects without re-reading
        // blobs.
        let mut live_manifests: HashSet<String> = HashSet::new();
        let mut live_blobs: HashSet<String> = HashSet::new();
        let mut docs: HashMap<String, OciManifestDoc> = HashMap::new();

        let roots: Vec<String> = tags
            .iter()
            .map(|tag| format!("{}\u{0}{}", tag.name, tag.manifest_digest))
            .collect();
        if !self
            .mark_live(
                repo,
                roots,
                &manifests,
                &mut live_manifests,
                &mut live_blobs,
                &mut docs,
            )
            .await
        {
            return Ok(0);
        }
        // Referrers (OCI 1.1): a manifest whose subject points at a live
        // manifest — a signature or SBOM attached to a kept image — is live too,
        // transitively (a referrer may itself have referrers), so iterate to a
        // fixpoint.
        loop {
            let mut extra = Vec::new();
            for (key, art) in &manifests {
                if live_manifests.contains(key) {
                    continue;
                }
                let Some(doc) = self.read_doc(key, art, &mut docs).await else {
                    tracing::warn!(
                        repo = %repo.name, path = %art.path,
                        "oci prune: manifest unreadable, repository left unpruned"
                    );
                    return Ok(0);
                };
                let name = key.split('\u{0}').next().unwrap_or("");
                if let Some(subject) = &doc.subject
                    && live_manifests.contains(&format!("{name}\u{0}{}", subject.digest))
                {
                    extra.push(key.clone());
                }
            }
            if extra.is_empty() {
                break;
            }
            if !self
                .mark_live(
                    repo,
                    extra,
                    &manifests,
                    &mut live_manifests,
                    &mut live_blobs,
                    &mut docs,
                )
                .await
            {
                return Ok(0);
            }
        }

        let cutoff = self.engine.now()
            - chrono::TimeDelta::from_std(OCI_PRUNE_GRACE).unwrap_or(chrono::TimeDelta::MAX);
        let mut deleted = 0usize;
        for (live, table) in [(&live_manifests, &manifests), (&live_blobs, &blobs)] {
            for (key, art) in table {
                if live.contains(key) {
                    continue;
                }
                if deleted >= OCI_PRUNE_BATCH {
                    return Ok(deleted);
                }
                if art.cached_at > cutoff {
                    // Inside the push/pull grace window.
                    continue;
                }
                self.store.delete_artifact(repo.id, &art.path).await?;
                self.oci_prune_deleted
                    .with_label_values(&[&repo.name])
                    .inc();
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    /// Walks the reachability queue, marking manifests and their blobs live.
    /// `false` means a manifest could not be read: prune deletes nothing it
    /// cannot prove unreachable, so the caller gives up on the repository.
    async fn mark_live(
        &self,
        repo: &Repository,
        roots: Vec<String>,
        manifests: &HashMap<String, Artifact>,
        live_manifests: &mut HashSet<String>,
        live_blobs: &mut HashSet<String>,
        docs: &mut HashMap<String, OciManifestDoc>,
    ) -> bool {
        let mut queue = roots;
        while let Some(key) = queue.pop() {
            if live_manifests.contains(&key) {
                continue;
            }
            let Some(art) = manifests.get(&key) else {
                // Reference to a manifest that is gone; nothing to keep.
                continue;
            };
            live_manifests.insert(key.clone());
            let name = key.split('\u{0}').next().unwrap_or("").to_string();
            let Some(doc) = self.read_doc(&key, art, docs).await else {
                tracing::warn!(
                    repo = %repo.name, path = %art.path,
                    "oci prune: manifest unreadable, repository left unpruned"
                );
                return false;
            };
            for child in &doc.manifests {
                queue.push(format!("{name}\u{0}{}", child.digest));
            }
            if let Some(config) = &doc.config {
                live_blobs.insert(format!("{name}\u{0}{}", config.digest));
            }
            for layer in &doc.layers {
                live_blobs.insert(format!("{name}\u{0}{}", layer.digest));
            }
        }
        true
    }

    /// Reads a manifest document once and memoises it for the referrer pass.
    async fn read_doc(
        &self,
        key: &str,
        art: &Artifact,
        docs: &mut HashMap<String, OciManifestDoc>,
    ) -> Option<OciManifestDoc> {
        if let Some(doc) = docs.get(key) {
            return Some(doc.clone());
        }
        let doc = self.read_oci_manifest(art).await.ok()?;
        docs.insert(key.to_string(), doc.clone());
        Some(doc)
    }

    /// Loads and parses one stored manifest document.
    pub(crate) async fn read_oci_manifest(
        &self,
        art: &Artifact,
    ) -> Result<OciManifestDoc, meta::Error> {
        let (reader, _) = self
            .engine
            .blobs
            .open(&art.blob_sha256)
            .await
            .map_err(|e| meta::Error::Other(e.to_string()))?;
        super::npm::decode_json_stream::<OciManifestDoc, _>(
            reader,
            DEFAULT_OCI_MAX_MANIFEST_BYTES as u64,
        )
        .await
        .ok_or_else(|| meta::Error::Other("manifest is not a JSON document".to_string()))
    }

    /// Deletes upload sessions untouched past the TTL, with their temp files. An
    /// expired session's bytes never reached the blob store, so this is the only
    /// cleanup they need.
    pub(crate) async fn expire_oci_sessions(&self, ttl: Duration) -> Result<(), meta::Error> {
        if ttl.is_zero() || self.oci_upload_dir_value().is_empty() {
            return Ok(());
        }
        let cutoff =
            self.engine.now() - chrono::TimeDelta::from_std(ttl).unwrap_or(chrono::TimeDelta::MAX);
        let sessions = self
            .store
            .list_expired_oci_upload_sessions(cutoff, 256)
            .await?;
        for sess in sessions {
            self.store.delete_oci_upload_session(&sess.id).await?;
            let path = std::path::PathBuf::from(self.oci_upload_dir_value()).join(&sess.id);
            if let Err(err) = tokio::fs::remove_file(&path).await
                && err.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(id = %sess.id, err = %err, "oci session file remove failed");
            }
        }
        Ok(())
    }
}

/// Splits an artifact path on the LAST occurrence of `sep` into (name, digest),
/// mirroring `parse_oci_path`'s endpoint anchoring.
pub(crate) fn split_oci_path(path: &str, sep: &str) -> Option<(String, String)> {
    let i = path.rfind(sep)?;
    let (name, digest) = (&path[..i], &path[i + sep.len()..]);
    OCI_DIGEST_RE
        .is_match(digest)
        .then(|| (name.to_string(), digest.to_string()))
}
