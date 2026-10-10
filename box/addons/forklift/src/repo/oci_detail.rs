//! The artifact drill-down the console shows when an OCI tag is clicked.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use tokio::io::AsyncReadExt as _;

use crate::meta;

use super::Manager;
use super::oci::{OCI_DIGEST_RE, oci_blob_path, oci_index_media_type, oci_manifest_path};
use super::oci_images::{OCITagInfo, manifest_content_size, oci_artifact_kind};
use super::oci_push::OciManifestDoc;

// Caps for detail-view payloads. The console renders these inline, so they are
// bounded well below the manifest/blob caps.
/// Config blob (image config or `Chart.yaml` JSON).
const OCI_DETAIL_CONFIG_CAP: i64 = 1 << 20;
/// One extracted chart file (`values.yaml`, README).
const OCI_DETAIL_ADDITION_CAP: i64 = 256 << 10;

/// Carries the Harbor-style "Additions" for a Helm chart: files extracted from
/// the chart archive layer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OCIChartAdditions {
    pub values_yaml: String,
    pub readme_md: String,
}

/// The overview subset of an image config blob.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OCIImageConfigSummary {
    pub os: String,
    pub architecture: String,
    pub created: String,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    pub layers: i64,
}

/// One entry of a multi-platform index.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OCIIndexChild {
    pub digest: String,
    pub platform: String,
    pub size: i64,
}

/// The artifact drill-down: the summary row, the raw manifest, and per-kind
/// additions.
#[derive(Debug, Serialize)]
pub struct OCIArtifactDetail {
    pub info: OCITagInfo,
    pub manifest_json: Box<RawValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_json: Option<Box<RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<OCIImageConfigSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chart: Option<OCIChartAdditions>,
    /// Lists an index's referenced manifests (digest + platform).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<OCIIndexChild>,
}

impl Manager {
    /// Resolves one tagged (or digest-addressed) artifact to its detail view.
    /// `reference` may be a tag or a sha256 digest.
    pub async fn oci_artifact_detail(
        &self,
        repo_id: i64,
        name: &str,
        reference: &str,
    ) -> Result<OCIArtifactDetail, meta::Error> {
        let mut digest = reference.to_string();
        let mut tag_name = String::new();
        if !OCI_DIGEST_RE.is_match(reference) {
            let tag = self.store.get_oci_tag(repo_id, name, reference).await?;
            digest = tag.manifest_digest;
            tag_name = tag.tag;
        }
        let art = self
            .store
            .get_artifact(repo_id, &oci_manifest_path(name, &digest))
            .await?;
        let manifest_bytes = self
            .read_blob_capped(&art.blob_sha256, self.oci_manifest_cap())
            .await?;
        let doc: OciManifestDoc = serde_json::from_slice(&manifest_bytes)?;
        let manifest_json = raw_value(&manifest_bytes)
            .ok_or_else(|| meta::Error::Other("manifest is not valid JSON".to_string()))?;

        let mut detail = OCIArtifactDetail {
            info: OCITagInfo {
                name: name.to_string(),
                tag: tag_name,
                digest: digest.clone(),
                media_type: art.content_type.clone(),
                kind: oci_artifact_kind(&doc),
                platforms: Vec::new(),
                pushed_at: art.cached_at,
                size: art.size + manifest_content_size(&doc),
                pushed_by: art.cached_by.clone(),
                ..Default::default()
            },
            manifest_json,
            config_json: None,
            image: None,
            chart: None,
            children: Vec::new(),
        };
        if art.last_accessed_at > art.cached_at {
            detail.info.pulled_at = Some(art.last_accessed_at);
            detail.info.pulled_by = art.last_accessed_by.clone();
        }

        if oci_index_media_type(&art.content_type) || !doc.manifests.is_empty() {
            detail.info.kind = "index".to_string();
            detail.info.size = art.size;
            for child in &doc.manifests {
                let mut platform = String::new();
                if let Some(p) = &child.platform
                    && !p.os.is_empty()
                {
                    platform = format!("{}/{}", p.os, p.architecture);
                    detail.info.platforms.push(platform.clone());
                }
                detail.children.push(OCIIndexChild {
                    digest: child.digest.clone(),
                    platform,
                    size: child.size,
                });
                if let Ok(child_art) = self
                    .store
                    .get_artifact(repo_id, &oci_manifest_path(name, &child.digest))
                    .await
                    && let Ok(child_doc) = self.read_oci_manifest(&child_art).await
                {
                    detail.info.size += child_art.size + manifest_content_size(&child_doc);
                }
            }
            return Ok(detail);
        }

        // Config blob: raw JSON for the viewer, plus a parsed summary for
        // images.
        if let Some(config) = &doc.config
            && let Ok(config_art) = self
                .store
                .get_artifact(repo_id, &oci_blob_path(name, &config.digest))
                .await
            && let Ok(config_bytes) = self
                .read_blob_capped(&config_art.blob_sha256, OCI_DETAIL_CONFIG_CAP)
                .await
            && let Some(raw) = raw_value(&config_bytes)
        {
            detail.config_json = Some(raw);
            if detail.info.kind == "image" {
                detail.image = summarize_image_config(&config_bytes, doc.layers.len() as i64);
                if let Some(image) = &detail.image
                    && !image.os.is_empty()
                {
                    detail.info.platforms = vec![format!("{}/{}", image.os, image.architecture)];
                }
            }
        }

        if detail.info.kind == "chart" {
            detail.chart = self.extract_chart_additions(repo_id, name, &doc).await;
        }
        Ok(detail)
    }

    /// Reads a blob whole, refusing anything over `cap`.
    async fn read_blob_capped(&self, sha: &str, cap: i64) -> Result<Vec<u8>, meta::Error> {
        let (reader, size) = self
            .engine
            .blobs
            .open(sha)
            .await
            .map_err(|e| meta::Error::Other(e.to_string()))?;
        if size > cap {
            return Err(meta::Error::Other("blob exceeds detail cap".to_string()));
        }
        let mut out = Vec::new();
        tokio::io::AsyncReadExt::take(reader, cap as u64)
            .read_to_end(&mut out)
            .await
            .map_err(|e| meta::Error::Other(e.to_string()))?;
        Ok(out)
    }

    /// Pulls `values.yaml` and `README.md` out of the chart archive layer (a
    /// gzipped tar rooted at the chart name). Best-effort: a missing or
    /// oversized file simply leaves its field empty.
    async fn extract_chart_additions(
        &self,
        repo_id: i64,
        name: &str,
        doc: &OciManifestDoc,
    ) -> Option<OCIChartAdditions> {
        let layer_digest = doc
            .layers
            .iter()
            .find(|layer| layer.media_type == "application/vnd.cncf.helm.chart.content.v1.tar+gzip")
            .map(|layer| layer.digest.clone())?;
        let art = self
            .store
            .get_artifact(repo_id, &oci_blob_path(name, &layer_digest))
            .await
            .ok()?;
        let (reader, _) = self.engine.blobs.open(&art.blob_sha256).await.ok()?;
        // The archive walk is blocking (flate2 + tar), so it runs on the
        // blocking pool over a synchronous bridge instead of buffering the
        // layer.
        let bridged = tokio_util::io::SyncIoBridge::new(reader);
        tokio::task::spawn_blocking(move || read_chart_additions(bridged))
            .await
            .ok()
            .flatten()
    }
}

/// Walks the chart tarball, collecting the two files the console renders.
fn read_chart_additions<R: std::io::Read>(reader: R) -> Option<OCIChartAdditions> {
    let gz = flate2::read::GzDecoder::new(reader);
    let mut archive = tar::Archive::new(gz);
    let mut additions = OCIChartAdditions::default();
    for entry in archive.entries().ok()? {
        let Ok(mut entry) = entry else { break };
        let Ok(path) = entry.path() else { continue };
        let path = path.to_string_lossy().to_string();
        // Entries are "<chart>/values.yaml" etc; match on the top-level file
        // name so subchart files never shadow the root ones.
        let trimmed = path.strip_prefix("./").unwrap_or(&path);
        let parts: Vec<&str> = trimmed.split('/').collect();
        let size = entry.header().size().unwrap_or(u64::MAX) as i64;
        if parts.len() != 2 || size > OCI_DETAIL_ADDITION_CAP {
            continue;
        }
        let field = match parts[1].to_lowercase().as_str() {
            "values.yaml" => &mut additions.values_yaml,
            "readme.md" => &mut additions.readme_md,
            _ => continue,
        };
        let mut body = String::new();
        if std::io::Read::take(&mut entry, OCI_DETAIL_ADDITION_CAP as u64)
            .read_to_string(&mut body)
            .is_ok()
        {
            *field = body;
        }
        if !additions.values_yaml.is_empty() && !additions.readme_md.is_empty() {
            break;
        }
    }
    Some(additions)
}

/// Extracts the overview fields from an image config blob.
fn summarize_image_config(raw: &[u8], layers: i64) -> Option<OCIImageConfigSummary> {
    #[derive(serde::Deserialize, Default)]
    struct ImageConfig {
        #[serde(default)]
        os: String,
        #[serde(default)]
        architecture: String,
        #[serde(default)]
        created: String,
        #[serde(default)]
        config: InnerConfig,
    }
    #[derive(serde::Deserialize, Default)]
    struct InnerConfig {
        #[serde(default, rename = "Entrypoint")]
        entrypoint: Vec<String>,
        #[serde(default, rename = "Cmd")]
        cmd: Vec<String>,
        #[serde(default, rename = "Env")]
        env: Vec<String>,
    }
    let cfg: ImageConfig = serde_json::from_slice(raw).ok()?;
    Some(OCIImageConfigSummary {
        os: cfg.os,
        architecture: cfg.architecture,
        created: cfg.created,
        entrypoint: cfg.config.entrypoint,
        cmd: cfg.config.cmd,
        env: cfg.config.env,
        layers,
    })
}

fn raw_value(bytes: &[u8]) -> Option<Box<RawValue>> {
    let text = String::from_utf8(bytes.to_vec()).ok()?;
    RawValue::from_string(text).ok()
}

// std::io::Read::read_to_string is used through the trait above.
use std::io::Read as _;
