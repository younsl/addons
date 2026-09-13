//! Publication lifecycle: deleting a managed Maven, PyPI or npm publication and
//! yanking a Cargo one. Every aggregate the removal touches is rewritten under
//! compare-and-set in the same transaction that drops the immutable assets, so
//! a concurrent publish never sees a half-deleted package.

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::meta::{
    self, Artifact, ArtifactCAS, ArtifactPublication, ArtifactPublicationTombstone,
    PublicationLifecycleBatch, Repository,
};

use super::uiupload::{UploadProblem, UploadedArtifact, Uploader, upload_problem};
use super::uiupload_cargo::cargo_sparse_path;
use super::uiupload_go::go_path_base;
use super::uiupload_maven::{
    parse_maven_metadata, parse_maven_plugin_metadata, render_maven_metadata,
    render_maven_plugin_metadata, uploaded_result, valid_maven_plugin_metadata_xml,
};
use super::uiupload_npm::sorted_json;

/// What one lifecycle operation removed and rewrote.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PublicationLifecycleResult {
    pub publication_id: String,
    pub coordinate: String,
    pub deleted: Vec<String>,
    pub derived_updated: Vec<UploadedArtifact>,
    pub derived_deleted: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yanked: Option<bool>,
}

impl Uploader {
    /// Deletes one managed publication with every aggregate it appears in.
    pub async fn delete_publication(
        &self,
        repository: &Repository,
        publication_id: &str,
        principal: &str,
    ) -> Result<PublicationLifecycleResult, Box<UploadProblem>> {
        let not_found = || {
            upload_problem(
                404,
                "publication_not_found",
                "Publication not found",
                "The managed publication was not found",
            )
        };
        let publication = self
            .store
            .get_artifact_publication(publication_id)
            .await
            .map_err(|_| not_found())?;
        if publication.repo_id != repository.id {
            return Err(not_found());
        }
        if publication.format != meta::FORMAT_MAVEN
            && publication.format != meta::FORMAT_PYPI
            && publication.format != meta::FORMAT_NPM
        {
            return Err(upload_problem(
                409,
                "lifecycle_not_supported",
                "Deletion is not supported",
                "This package format does not permit publication deletion",
            ));
        }
        let Ok(owned) = self.store.list_publication_artifacts(&publication.id).await else {
            return Err(upload_problem(
                503,
                "storage_unavailable",
                "Deletion unavailable",
                "Publication assets could not be listed",
            ));
        };
        let mut batch = PublicationLifecycleBatch {
            publication: publication.clone(),
            delete_publication: true,
            ..Default::default()
        };
        let mut result = PublicationLifecycleResult {
            publication_id: publication.id.clone(),
            coordinate: publication.coordinate.clone(),
            ..Default::default()
        };
        for artifact in &owned {
            batch.remove_paths.push(artifact.path.clone());
            result.deleted.push(artifact.path.clone());
        }
        match publication.format.as_str() {
            meta::FORMAT_MAVEN => {
                self.plan_maven_delete(
                    repository,
                    &publication,
                    &mut batch,
                    &mut result,
                    principal,
                )
                .await?;
                self.plan_maven_plugin_delete(
                    repository,
                    &publication,
                    &owned,
                    &mut batch,
                    &mut result,
                    principal,
                )
                .await?;
            }
            meta::FORMAT_PYPI => {
                for artifact in &owned {
                    batch.tombstones.push(ArtifactPublicationTombstone {
                        repo_id: repository.id,
                        format: meta::FORMAT_PYPI.to_string(),
                        package_name: publication.package_name.clone(),
                        version: publication.version.clone(),
                        asset_key: go_path_base(&artifact.path),
                        deleted_at: self.now(),
                        deleted_by: principal.to_string(),
                    });
                }
            }
            meta::FORMAT_NPM => {
                self.plan_npm_delete(repository, &publication, &mut batch, &mut result, principal)
                    .await?;
            }
            _ => {}
        }
        match self.store.apply_publication_lifecycle(batch).await {
            Ok(()) => Ok(result),
            Err(meta::Error::ArtifactConflict) | Err(meta::Error::DerivedMetadataChanged) => {
                Err(upload_problem(
                    409,
                    "concurrent_metadata_update",
                    "Publication changed",
                    "Refresh and retry the lifecycle operation",
                ))
            }
            Err(_) => Err(upload_problem(
                503,
                "storage_unavailable",
                "Deletion failed",
                "The publication could not be deleted atomically",
            )),
        }
    }

    /// Drops the deleted plugin from its group-level `maven-metadata.xml`, or
    /// removes that aggregate entirely with the last plugin of the group.
    async fn plan_maven_plugin_delete(
        &self,
        repository: &Repository,
        publication: &ArtifactPublication,
        owned: &[Artifact],
        batch: &mut PublicationLifecycleBatch,
        result: &mut PublicationLifecycleResult,
        principal: &str,
    ) -> Result<(), Box<UploadProblem>> {
        let mut plugin_prefix = String::new();
        for artifact in owned {
            let prefix = serde_json::from_str::<serde_json::Value>(&artifact.metadata_json)
                .ok()
                .and_then(|envelope| {
                    envelope
                        .get("format_metadata")?
                        .get("plugin_prefix")?
                        .as_str()
                        .map(str::to_string)
                })
                .unwrap_or_default();
            if !prefix.is_empty() {
                plugin_prefix = prefix;
                break;
            }
        }
        if plugin_prefix.is_empty() {
            return Ok(());
        }
        let Ok(publications) = self.store.list_artifact_publications(repository.id).await else {
            return Err(upload_problem(
                503,
                "storage_unavailable",
                "Maven plugin metadata unavailable",
                "Remaining plugin publications could not be listed",
            ));
        };
        // Another version of the same plugin still holds the group entry.
        for existing in &publications {
            if existing.id != publication.id
                && existing.format == meta::FORMAT_MAVEN
                && existing.package_name == publication.package_name
            {
                return Ok(());
            }
        }
        let parts: Vec<&str> = publication.package_name.split(':').collect();
        if parts.len() != 2 {
            return Err(upload_problem(
                409,
                "publication_invalid",
                "Maven publication is invalid",
                "The plugin coordinate is malformed",
            ));
        }
        let metadata_path = format!("{}/maven-metadata.xml", parts[0].replace('.', "/"));
        let unsupported = |detail: &str| {
            upload_problem(
                409,
                "derived_metadata_not_managed",
                "Maven plugin metadata is unsupported",
                detail,
            )
        };
        let Ok(artifact) = self.store.get_artifact(repository.id, &metadata_path).await else {
            return Err(unsupported("The group plugin metadata is unavailable"));
        };
        let value = self
            .read_bounded_blob(&artifact.blob_sha256, self.cfg.archive_max_meta_bytes)
            .await?;
        if !valid_maven_plugin_metadata_xml(&value) {
            return Err(unsupported(
                "The group plugin metadata cannot be safely rewritten",
            ));
        }
        let Ok(mut document) = parse_maven_plugin_metadata(&value) else {
            return Err(unsupported(
                "The group plugin metadata cannot be safely rewritten",
            ));
        };
        document
            .plugins
            .retain(|plugin| plugin.artifact_id != parts[1]);
        let paths = [
            metadata_path.clone(),
            format!("{metadata_path}.sha1"),
            format!("{metadata_path}.sha256"),
        ];
        if document.plugins.is_empty() {
            for aggregate_path in paths {
                if let Ok(current) = self
                    .store
                    .get_artifact(repository.id, &aggregate_path)
                    .await
                {
                    batch.mutable_remove_cas.push(ArtifactCAS {
                        artifact: Artifact {
                            repo_id: repository.id,
                            path: aggregate_path.clone(),
                            ..Default::default()
                        },
                        expected_sha256: current.blob_sha256,
                    });
                    result.derived_deleted.push(aggregate_path);
                }
            }
            return Ok(());
        }
        let encoded = render_maven_plugin_metadata(&document);
        let staged = self
            .stage_generated(&metadata_path, "index", &artifact.content_type, &encoded)
            .await
            .map_err(|_| {
                upload_problem(
                    503,
                    "storage_unavailable",
                    "Maven plugin metadata unavailable",
                    "The group metadata could not be staged",
                )
            })?;
        batch.mutable_cas.push(ArtifactCAS {
            artifact: Artifact {
                repo_id: repository.id,
                path: metadata_path.clone(),
                blob_sha256: staged.digest.clone(),
                size: staged.size,
                content_type: artifact.content_type.clone(),
                metadata_json: artifact.metadata_json.clone(),
                cached_by: principal.to_string(),
                artifact_role: "index".to_string(),
                ..Default::default()
            },
            expected_sha256: artifact.blob_sha256.clone(),
        });
        result.derived_updated.push(uploaded_result(&staged));
        for (suffix, value) in [
            (".sha1", format!("{}\n", staged.sha1)),
            (".sha256", format!("{}\n", staged.digest)),
        ] {
            let checksum_path = format!("{metadata_path}{suffix}");
            let Ok(current) = self.store.get_artifact(repository.id, &checksum_path).await else {
                return Err(unsupported("A group checksum is unavailable"));
            };
            let checksum_asset = self
                .stage_generated(
                    &checksum_path,
                    "checksum",
                    &current.content_type,
                    value.as_bytes(),
                )
                .await
                .map_err(|_| {
                    upload_problem(
                        503,
                        "storage_unavailable",
                        "Maven plugin metadata unavailable",
                        "A group checksum could not be staged",
                    )
                })?;
            batch.mutable_cas.push(ArtifactCAS {
                artifact: Artifact {
                    repo_id: repository.id,
                    path: checksum_asset.path.clone(),
                    blob_sha256: checksum_asset.digest.clone(),
                    size: checksum_asset.size,
                    content_type: current.content_type.clone(),
                    metadata_json: current.metadata_json.clone(),
                    cached_by: principal.to_string(),
                    artifact_role: "index".to_string(),
                    ..Default::default()
                },
                expected_sha256: current.blob_sha256,
            });
            result
                .derived_updated
                .push(uploaded_result(&checksum_asset));
        }
        Ok(())
    }

    /// Rewrites the packument around the deleted version, or removes it with the
    /// last version, and tombstones the version so it cannot be republished.
    async fn plan_npm_delete(
        &self,
        repository: &Repository,
        publication: &ArtifactPublication,
        batch: &mut PublicationLifecycleBatch,
        result: &mut PublicationLifecycleResult,
        principal: &str,
    ) -> Result<(), Box<UploadProblem>> {
        let unsupported = |detail: &str| {
            upload_problem(
                409,
                "derived_metadata_not_managed",
                "npm metadata is unsupported",
                detail,
            )
        };
        let artifact = match self
            .store
            .get_artifact(repository.id, &publication.package_name)
            .await
        {
            Ok(artifact) if meta::is_ui_managed_aggregate_metadata(&artifact.metadata_json) => {
                artifact
            }
            _ => return Err(unsupported("The managed packument is unavailable")),
        };
        let reader = match self.engine.blobs.open(&artifact.blob_sha256).await {
            Ok((reader, _)) => reader,
            Err(err) => {
                self.engine.note_blob_missing(
                    repository.id,
                    &repository.name,
                    &publication.package_name,
                    &artifact.blob_sha256,
                    "index",
                    503,
                    &err.to_string(),
                );
                return Err(upload_problem(
                    503,
                    "storage_unavailable",
                    "npm metadata unavailable",
                    "The packument could not be opened",
                ));
            }
        };
        let limit = self.cfg.archive_max_meta_bytes;
        let mut value = Vec::new();
        if AsyncReadExt::take(reader, (limit + 1) as u64)
            .read_to_end(&mut value)
            .await
            .is_err()
        {
            return Err(unsupported("The packument cannot be safely rewritten"));
        }
        let mut de = serde_json::Deserializer::from_slice(&value);
        let Ok(serde_json::Value::Object(mut document)) = serde_json::Value::deserialize(&mut de)
        else {
            return Err(unsupported("The packument cannot be safely rewritten"));
        };
        let has_object = |document: &serde_json::Map<String, serde_json::Value>, key: &str| {
            document.get(key).is_some_and(serde_json::Value::is_object)
        };
        if !has_object(&document, "versions")
            || !has_object(&document, "dist-tags")
            || !has_object(&document, "time")
        {
            return Err(unsupported("The packument cannot be safely rewritten"));
        }
        let take_object =
            |document: &mut serde_json::Map<String, serde_json::Value>, key: &str| match document
                .remove(key)
            {
                Some(serde_json::Value::Object(object)) => object,
                _ => serde_json::Map::new(),
            };
        let mut versions = take_object(&mut document, "versions");
        let mut tags = take_object(&mut document, "dist-tags");
        let mut times = take_object(&mut document, "time");
        versions.remove(&publication.version);
        times.remove(&publication.version);
        tags.retain(|_, value| value.as_str() != Some(publication.version.as_str()));
        if versions.is_empty() {
            batch.mutable_remove_cas.push(ArtifactCAS {
                artifact: Artifact {
                    repo_id: repository.id,
                    path: publication.package_name.clone(),
                    ..Default::default()
                },
                expected_sha256: artifact.blob_sha256.clone(),
            });
            result
                .derived_deleted
                .push(publication.package_name.clone());
        } else {
            if !tags.contains_key("latest") {
                tags.insert(
                    "latest".to_string(),
                    serde_json::Value::String(best_npm_version(&versions)),
                );
            }
            times.insert(
                "modified".to_string(),
                serde_json::Value::String(meta::time::format_time(self.now())),
            );
            document.insert("versions".to_string(), serde_json::Value::Object(versions));
            document.insert("dist-tags".to_string(), serde_json::Value::Object(tags));
            document.insert("time".to_string(), serde_json::Value::Object(times));
            let encoded = serde_json::to_vec(&sorted_json(serde_json::Value::Object(document)))
                .unwrap_or_default();
            let staged = self
                .stage_generated(
                    &publication.package_name,
                    "index",
                    "application/json",
                    &encoded,
                )
                .await
                .map_err(|_| {
                    upload_problem(
                        503,
                        "storage_unavailable",
                        "npm metadata unavailable",
                        "The updated packument could not be staged",
                    )
                })?;
            batch.mutable_cas.push(ArtifactCAS {
                artifact: Artifact {
                    repo_id: repository.id,
                    path: publication.package_name.clone(),
                    blob_sha256: staged.digest.clone(),
                    size: staged.size,
                    content_type: "application/json".to_string(),
                    metadata_json: artifact.metadata_json.clone(),
                    cached_by: principal.to_string(),
                    artifact_role: "index".to_string(),
                    ..Default::default()
                },
                expected_sha256: artifact.blob_sha256.clone(),
            });
            result.derived_updated.push(uploaded_result(&staged));
        }
        batch.tombstones.push(ArtifactPublicationTombstone {
            repo_id: repository.id,
            format: meta::FORMAT_NPM.to_string(),
            package_name: publication.package_name.clone(),
            version: publication.version.clone(),
            asset_key: "*".to_string(),
            deleted_at: self.now(),
            deleted_by: principal.to_string(),
        });
        Ok(())
    }

    /// Drops the deleted version from `maven-metadata.xml` and republishes it
    /// with its checksums, or removes all three with the last version.
    async fn plan_maven_delete(
        &self,
        repository: &Repository,
        publication: &ArtifactPublication,
        batch: &mut PublicationLifecycleBatch,
        result: &mut PublicationLifecycleResult,
        principal: &str,
    ) -> Result<(), Box<UploadProblem>> {
        let parts: Vec<&str> = publication.package_name.split(':').collect();
        if parts.len() != 2 {
            return Err(upload_problem(
                409,
                "publication_invalid",
                "Maven publication is invalid",
                "The publication coordinate is malformed",
            ));
        }
        let metadata_path = format!(
            "{}/{}/maven-metadata.xml",
            parts[0].replace('.', "/"),
            parts[1]
        );
        let unsupported = |detail: &str| {
            upload_problem(
                409,
                "derived_metadata_not_managed",
                "Maven metadata is unsupported",
                detail,
            )
        };
        let artifact = match self.store.get_artifact(repository.id, &metadata_path).await {
            Ok(artifact) if meta::is_ui_managed_aggregate_metadata(&artifact.metadata_json) => {
                artifact
            }
            _ => return Err(unsupported("The managed Maven metadata is unavailable")),
        };
        let Ok((reader, _)) = self.engine.blobs.open(&artifact.blob_sha256).await else {
            return Err(upload_problem(
                503,
                "storage_unavailable",
                "Maven metadata unavailable",
                "The metadata could not be opened",
            ));
        };
        let limit = self.cfg.archive_max_meta_bytes;
        let mut value = Vec::new();
        let read = AsyncReadExt::take(reader, (limit + 1) as u64)
            .read_to_end(&mut value)
            .await;
        if read.is_err() || value.len() as i64 > limit {
            return Err(unsupported("The metadata cannot be safely rewritten"));
        }
        let Ok(mut document) = parse_maven_metadata(&value) else {
            return Err(unsupported("The metadata cannot be safely rewritten"));
        };
        document
            .versioning
            .versions
            .retain(|version| *version != publication.version);
        let paths = [
            metadata_path.clone(),
            format!("{metadata_path}.sha1"),
            format!("{metadata_path}.sha256"),
        ];
        if document.versioning.versions.is_empty() {
            for aggregate_path in paths {
                if let Ok(current) = self
                    .store
                    .get_artifact(repository.id, &aggregate_path)
                    .await
                {
                    batch.mutable_remove_cas.push(ArtifactCAS {
                        artifact: Artifact {
                            repo_id: repository.id,
                            path: aggregate_path.clone(),
                            ..Default::default()
                        },
                        expected_sha256: current.blob_sha256,
                    });
                    result.derived_deleted.push(aggregate_path);
                }
            }
            return Ok(());
        }
        document.versioning.versions.sort();
        document.versioning.latest = document
            .versioning
            .versions
            .last()
            .cloned()
            .unwrap_or_default();
        document.versioning.release = document.versioning.latest.clone();
        document.versioning.last_updated = self.now().format("%Y%m%d%H%M%S").to_string();
        let encoded = render_maven_metadata(&document);
        let metadata_asset = self
            .stage_generated(&metadata_path, "index", &artifact.content_type, &encoded)
            .await
            .map_err(|_| {
                upload_problem(
                    503,
                    "storage_unavailable",
                    "Maven metadata unavailable",
                    "The metadata could not be staged",
                )
            })?;
        batch.mutable_cas.push(ArtifactCAS {
            artifact: Artifact {
                repo_id: repository.id,
                path: metadata_path.clone(),
                blob_sha256: metadata_asset.digest.clone(),
                size: metadata_asset.size,
                content_type: artifact.content_type.clone(),
                metadata_json: artifact.metadata_json.clone(),
                cached_by: principal.to_string(),
                artifact_role: "index".to_string(),
                ..Default::default()
            },
            expected_sha256: artifact.blob_sha256.clone(),
        });
        result
            .derived_updated
            .push(uploaded_result(&metadata_asset));
        for (suffix, value) in [
            (".sha1", format!("{}\n", metadata_asset.sha1)),
            (".sha256", format!("{}\n", metadata_asset.digest)),
        ] {
            let checksum_path = format!("{metadata_path}{suffix}");
            let Ok(current) = self.store.get_artifact(repository.id, &checksum_path).await else {
                return Err(unsupported("A managed metadata checksum is unavailable"));
            };
            let staged = self
                .stage_generated(
                    &checksum_path,
                    "checksum",
                    &current.content_type,
                    value.as_bytes(),
                )
                .await
                .map_err(|_| {
                    upload_problem(
                        503,
                        "storage_unavailable",
                        "Maven metadata unavailable",
                        "A checksum could not be staged",
                    )
                })?;
            batch.mutable_cas.push(ArtifactCAS {
                artifact: Artifact {
                    repo_id: repository.id,
                    path: staged.path.clone(),
                    blob_sha256: staged.digest.clone(),
                    size: staged.size,
                    content_type: current.content_type.clone(),
                    metadata_json: current.metadata_json.clone(),
                    cached_by: principal.to_string(),
                    artifact_role: "index".to_string(),
                    ..Default::default()
                },
                expected_sha256: current.blob_sha256,
            });
            result.derived_updated.push(uploaded_result(&staged));
        }
        Ok(())
    }

    /// Flips the `yanked` flag of one Cargo version in the sparse index and on
    /// its publication row, in one transaction.
    pub async fn set_cargo_yanked(
        &self,
        repository: &Repository,
        publication_id: &str,
        principal: &str,
        yanked: bool,
    ) -> Result<PublicationLifecycleResult, Box<UploadProblem>> {
        let not_found = || {
            upload_problem(
                404,
                "publication_not_found",
                "Publication not found",
                "The managed publication was not found",
            )
        };
        let publication = self
            .store
            .get_artifact_publication(publication_id)
            .await
            .map_err(|_| not_found())?;
        if publication.repo_id != repository.id {
            return Err(not_found());
        }
        if publication.format != meta::FORMAT_CARGO {
            return Err(upload_problem(
                409,
                "lifecycle_not_supported",
                "Yank is not supported",
                "Only Cargo publications support yank and unyank",
            ));
        }
        let index_path = cargo_sparse_path(&publication.package_name);
        let unsupported = |detail: &str| {
            upload_problem(
                409,
                "derived_metadata_not_managed",
                "Cargo index is unsupported",
                detail,
            )
        };
        let artifact = match self.store.get_artifact(repository.id, &index_path).await {
            Ok(artifact) if meta::is_ui_managed_aggregate_metadata(&artifact.metadata_json) => {
                artifact
            }
            _ => return Err(unsupported("The managed sparse index is unavailable")),
        };
        let Ok((reader, _)) = self.engine.blobs.open(&artifact.blob_sha256).await else {
            return Err(upload_problem(
                503,
                "storage_unavailable",
                "Cargo index unavailable",
                "The sparse index could not be opened",
            ));
        };
        let limit = self.cfg.archive_max_meta_bytes;
        let mut value = Vec::new();
        let read = AsyncReadExt::take(reader, (limit + 1) as u64)
            .read_to_end(&mut value)
            .await;
        let changed = || {
            upload_problem(
                409,
                "publication_changed",
                "Cargo publication changed",
                "The matching sparse-index entry is unavailable",
            )
        };
        if read.is_err() {
            return Err(changed());
        }
        let text = String::from_utf8_lossy(&value);
        let mut lines = Vec::new();
        let mut found = false;
        // `str::lines` splits exactly like `bufio.Scanner`: no terminators, a
        // stripped `\r`, and no empty tail after the final newline.
        for line in text.lines() {
            let Ok(serde_json::Value::Object(mut entry)) = serde_json::from_str(line) else {
                return Err(unsupported("A sparse-index line is malformed"));
            };
            let version = entry.get("vers").and_then(|v| v.as_str()).unwrap_or("");
            let identity = semver::Version::parse(version)
                .map(|parsed| {
                    let rendered = parsed.to_string();
                    match rendered.find('+') {
                        Some(index) => rendered[..index].to_string(),
                        None => rendered,
                    }
                })
                .unwrap_or_default();
            if identity == publication.version {
                entry.insert("yanked".to_string(), serde_json::Value::Bool(yanked));
                lines.push(
                    serde_json::to_string(&sorted_json(serde_json::Value::Object(entry)))
                        .unwrap_or_default(),
                );
                found = true;
            } else {
                lines.push(line.to_string());
            }
        }
        if !found {
            return Err(changed());
        }
        let staged = self
            .stage_generated(
                &index_path,
                "index",
                &artifact.content_type,
                format!("{}\n", lines.join("\n")).as_bytes(),
            )
            .await
            .map_err(|_| {
                upload_problem(
                    503,
                    "storage_unavailable",
                    "Cargo index unavailable",
                    "The updated sparse index could not be staged",
                )
            })?;
        let batch = PublicationLifecycleBatch {
            publication: publication.clone(),
            set_yanked: Some(yanked),
            mutable_cas: vec![ArtifactCAS {
                artifact: Artifact {
                    repo_id: repository.id,
                    path: index_path,
                    blob_sha256: staged.digest.clone(),
                    size: staged.size,
                    content_type: artifact.content_type.clone(),
                    metadata_json: artifact.metadata_json.clone(),
                    cached_by: principal.to_string(),
                    artifact_role: "index".to_string(),
                    ..Default::default()
                },
                expected_sha256: artifact.blob_sha256.clone(),
            }],
            ..Default::default()
        };
        if self.store.apply_publication_lifecycle(batch).await.is_err() {
            return Err(upload_problem(
                409,
                "concurrent_metadata_update",
                "Cargo index changed",
                "Refresh and retry yank or unyank",
            ));
        }
        Ok(PublicationLifecycleResult {
            publication_id: publication.id,
            coordinate: publication.coordinate,
            deleted: Vec::new(),
            derived_updated: vec![uploaded_result(&staged)],
            derived_deleted: Vec::new(),
            yanked: Some(yanked),
        })
    }
}

/// The highest strict-semver version left in a packument, which becomes the
/// `latest` tag when the deleted version held it.
///
fn best_npm_version(versions: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut best = String::new();
    let mut parsed_best: Option<semver::Version> = None;
    for value in versions.keys() {
        let Ok(parsed) = semver::Version::parse(value) else {
            continue;
        };
        if parsed_best.as_ref().is_none_or(|current| parsed > *current) {
            best = value.clone();
            parsed_best = Some(parsed);
        }
    }
    best
}
