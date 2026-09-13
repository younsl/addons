//! The console's Harbor-style tag listing for an OCI repository.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::meta;

use super::Manager;
use super::oci::{oci_index_media_type, oci_manifest_path};
use super::oci_push::OciManifestDoc;

/// One tagged image as the console's Harbor-style view presents it: the tag with
/// its manifest identity, total content size, the platforms an index covers, and
/// when the manifest landed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OCITagInfo {
    pub name: String,
    pub tag: String,
    pub digest: String,
    #[serde(rename = "media_type")]
    pub media_type: String,
    /// Classifies the artifact by its config media type: "chart" (Helm),
    /// "image" (container image), "artifact" (other ORAS payloads), or "index"
    /// (multi-platform manifest list). The manifest media type alone cannot
    /// distinguish these — a chart and an image both push as an OCI image
    /// manifest; only the config descriptor differs.
    pub kind: String,
    /// Sums the config and layer blobs (for an index, over every child manifest
    /// present in the store).
    pub size: i64,
    /// Lists "os/arch" pairs (index children, or the single image's platform
    /// when its config records one; empty when unknown).
    pub platforms: Vec<String>,
    pub pushed_at: DateTime<Utc>,
    /// Who stored the manifest (empty for anonymous or unknown).
    pub pushed_by: String,
    /// The manifest's last served time, `None` when the tag has not been pulled
    /// since it was pushed. Serving updates `last_accessed_at` at most once per
    /// touch interval, so this is minute-grained, not exact.
    pub pulled_at: Option<DateTime<Utc>>,
    /// Who last pulled (empty when never pulled or anonymous).
    pub pulled_by: String,
}

impl Manager {
    /// Resolves every tag in an OCI repository to the Harbor-style summary
    /// above. Manifest documents are read from the blob store; sizes come from
    /// their descriptors, so nothing is re-hashed. A tag whose manifest is
    /// missing (mid-push, or pruned underneath) is skipped rather than failing
    /// the listing.
    pub async fn list_oci_tags(&self, repo_id: i64) -> Result<Vec<OCITagInfo>, meta::Error> {
        let tags = self.store.list_oci_tag_rows(repo_id).await?;
        let mut out: Vec<OCITagInfo> = Vec::new();
        for tag in tags {
            let Ok(art) = self
                .store
                .get_artifact(repo_id, &oci_manifest_path(&tag.name, &tag.manifest_digest))
                .await
            else {
                continue;
            };
            let Ok(doc) = self.read_oci_manifest(&art).await else {
                continue;
            };
            let mut info = OCITagInfo {
                name: tag.name.clone(),
                tag: tag.tag.clone(),
                digest: tag.manifest_digest.clone(),
                media_type: art.content_type.clone(),
                kind: "image".to_string(),
                platforms: Vec::new(),
                pushed_at: art.cached_at,
                pushed_by: art.cached_by.clone(),
                ..Default::default()
            };
            // `last_accessed_at` is stamped at store time too, so it only counts
            // as a pull once it has moved past the push timestamp.
            if art.last_accessed_at > art.cached_at {
                info.pulled_at = Some(art.last_accessed_at);
                info.pulled_by = art.last_accessed_by.clone();
            }
            info.kind = oci_artifact_kind(&doc);
            if oci_index_media_type(&art.content_type) || !doc.manifests.is_empty() {
                info.kind = "index".to_string();
                for child in &doc.manifests {
                    if let Some(platform) = &child.platform
                        && !platform.os.is_empty()
                    {
                        info.platforms
                            .push(format!("{}/{}", platform.os, platform.architecture));
                    }
                    let Ok(child_art) = self
                        .store
                        .get_artifact(repo_id, &oci_manifest_path(&tag.name, &child.digest))
                        .await
                    else {
                        continue;
                    };
                    let Ok(child_doc) = self.read_oci_manifest(&child_art).await else {
                        continue;
                    };
                    info.size += child_art.size + manifest_content_size(&child_doc);
                }
            } else {
                info.size = art.size + manifest_content_size(&doc);
            }
            out.push(info);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.tag.cmp(&b.tag)));
        Ok(out)
    }
}

/// Classifies a manifest by its config media type (falling back to the OCI 1.1
/// `artifactType`): Helm charts and container images share the manifest media
/// type and differ only here.
pub(crate) fn oci_artifact_kind(doc: &OciManifestDoc) -> String {
    let mut mt = doc.artifact_type.as_str();
    if let Some(config) = &doc.config
        && !config.media_type.is_empty()
    {
        mt = config.media_type.as_str();
    }
    match mt {
        "application/vnd.cncf.helm.config.v1+json" => "chart",
        "application/vnd.oci.image.config.v1+json"
        | "application/vnd.docker.container.image.v1+json" => "image",
        "" => "image",
        _ => "artifact",
    }
    .to_string()
}

/// Sums the descriptor sizes a manifest references (config plus layers), the
/// number Harbor reports as the artifact size.
pub(crate) fn manifest_content_size(doc: &OciManifestDoc) -> i64 {
    let mut n = doc.config.as_ref().map(|c| c.size).unwrap_or(0);
    for layer in &doc.layers {
        n += layer.size;
    }
    n
}
