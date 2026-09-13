//! The Maven publisher: coordinate validation, POM handling, plugin-descriptor
//! inspection, derived `maven-metadata.xml` planning and the replace-conflict
//! workflow.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha1::Digest as _;

use crate::meta::{
    self, Artifact, ArtifactBatch, ArtifactCAS, ArtifactPublication, ArtifactUploadRequest,
    Repository, UPLOAD_CONFLICT, UPLOAD_RECEIVING, UploadRequestKey,
};

use super::maven::maven_content_type;
use super::uiupload::{
    ArtifactUploadManifest, ArtifactUploadResult, MavenUploadManifest, PublishResult,
    StagedUploadAsset, UploadProblem, UploadedArtifact, Uploader, upload_problem,
};

/// `<versioning>` inside a `maven-metadata.xml` document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MavenVersioning {
    pub(crate) latest: String,
    pub(crate) release: String,
    pub(crate) versions: Vec<String>,
    pub(crate) last_updated: String,
}

/// A `maven-metadata.xml` document.
///
/// The parser reads it with a pull parser and renders the same layout by hand, because the
/// output is a protocol artifact: Maven clients compare it against the `.sha1`/`.sha256`
/// checksums published alongside it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MavenMetadata {
    pub(crate) group_id: String,
    pub(crate) artifact_id: String,
    pub(crate) versioning: MavenVersioning,
}

/// One `<plugin>` entry of a group-level plugin metadata document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MavenPluginEntry {
    pub(crate) name: String,
    pub(crate) prefix: String,
    pub(crate) artifact_id: String,
}

/// A group-level `maven-metadata.xml` listing the plugins in a group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MavenPluginMetadata {
    pub(crate) group_id: String,
    pub(crate) plugins: Vec<MavenPluginEntry>,
}

/// The `META-INF/maven/plugin.xml` descriptor inside a plugin JAR.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct MavenPluginDescriptor {
    group_id: String,
    artifact_id: String,
    version: String,
    goal_prefix: String,
    name: String,
}

/// A `pom.xml` project document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct MavenPOM {
    root: String,
    model: String,
    group_id: String,
    artifact_id: String,
    version: String,
    packaging: String,
    parent_group_id: String,
    parent_version: String,
}

/// The stored conflict plan a Maven replacement records against its idempotency
/// key.
///
/// The Rust encoding uses the Rust field names: the row is internal, written and read back by
/// the same binary, and expires after 30 minutes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct MavenConflictPlan {
    pub(crate) problem: UploadProblem,
    pub(crate) batch: ArtifactBatch,
}

/// One asset the planner will write, staged or already received.
#[derive(Debug, Clone, Default)]
pub(crate) struct MavenPlannedAsset {
    pub(crate) path: String,
    pub(crate) role: String,
    pub(crate) digest: String,
    pub(crate) sha1: String,
    pub(crate) size: i64,
    pub(crate) content_type: String,
}

/// The `metadata_json` envelope stamped on every derived aggregate document.
pub(crate) const MAVEN_AGGREGATE_JSON: &str = r#"{"schema_version":1,"managed_by":"ui_upload_aggregate","format":"maven","aggregate_schema_version":1}"#;

impl Uploader {
    #[allow(clippy::too_many_arguments)] // Domain operation parameters.
    pub(crate) async fn publish_maven(
        self: &Arc<Self>,
        repository: &Repository,
        request: &ArtifactUploadRequest,
        key: UploadRequestKey,
        manifest: &ArtifactUploadManifest,
        staged: &[StagedUploadAsset],
        allow_replace: bool,
    ) -> PublishResult {
        let format_fields = || {
            upload_problem(
                422,
                "manifest_format_fields",
                "Invalid Maven manifest",
                "Exactly the Maven format object is required",
            )
        };
        let Some(maven) = &manifest.maven else {
            return Err(format_fields());
        };
        if manifest.pypi.is_some()
            || manifest.npm.is_some()
            || manifest.cargo.is_some()
            || manifest.go_.is_some()
        {
            return Err(format_fields());
        }
        if manifest.overwrite && !allow_replace {
            return Err(upload_problem(
                403,
                "delete_permission_required",
                "Replacement forbidden",
                "Maven replacement requires repository delete permission",
            ));
        }
        let m = MavenUploadManifest {
            group_id: maven.group_id.trim().to_string(),
            artifact_id: maven.artifact_id.trim().to_string(),
            version: maven.version.trim().to_string(),
            generate_pom: maven.generate_pom,
            packaging: maven.packaging.trim().to_string(),
        };
        if let Some(problem) = validate_maven_coordinates(&m) {
            return Err(problem);
        }
        if staged.len() != manifest.assets.len() {
            return Err(upload_problem(
                400,
                "asset_count_mismatch",
                "Asset count mismatch",
                "Received assets do not match the manifest",
            ));
        }

        let group_path = m.group_id.replace('.', "/");
        let artifact_dir = format!("{group_path}/{}", m.artifact_id);
        let base = format!(
            "{artifact_dir}/{}/{}-{}",
            m.version, m.artifact_id, m.version
        );
        let mut planned: Vec<MavenPlannedAsset> = Vec::with_capacity(staged.len() + 1);
        let mut seen = std::collections::HashSet::new();
        let mut pom_index: Option<usize> = None;
        for (i, asset) in staged.iter().enumerate() {
            let extension = asset.spec.extension.trim();
            let classifier = asset.spec.classifier.trim();
            if !safe_maven_segment(extension, true)
                || (!classifier.is_empty() && !safe_maven_segment(classifier, true))
            {
                return Err(upload_problem(
                    422,
                    "maven_asset_invalid",
                    "Invalid Maven asset",
                    "Extension or classifier is not a safe Maven path segment",
                ));
            }
            let mut filename = base.clone();
            if !classifier.is_empty() {
                filename.push('-');
                filename.push_str(classifier);
            }
            filename.push('.');
            filename.push_str(extension);
            if !seen.insert(filename.clone()) {
                return Err(upload_problem(
                    422,
                    "duplicate_asset",
                    "Duplicate Maven asset",
                    "Two assets resolve to the same canonical repository path",
                ));
            }
            if extension == "pom" && classifier.is_empty() {
                if pom_index.is_some() {
                    return Err(upload_problem(
                        422,
                        "duplicate_pom",
                        "Duplicate POM",
                        "At most one unclassified POM may be uploaded",
                    ));
                }
                pom_index = Some(i);
            }
            planned.push(MavenPlannedAsset {
                content_type: maven_content_type(&filename),
                path: filename,
                role: "primary".to_string(),
                digest: asset.digest.clone(),
                sha1: asset.sha1.clone(),
                size: asset.size,
            });
        }
        for asset in staged {
            let extension = asset.spec.extension.trim();
            let classifier = asset.spec.classifier.trim();
            if extension == "asc" && classifier.is_empty() {
                return Err(upload_problem(
                    422,
                    "maven_signature_ambiguous",
                    "Maven signature target is ambiguous",
                    "Use an extension such as jar.asc and the same classifier as its target asset",
                ));
            }
            if let Some(target_extension) = extension.strip_suffix(".asc") {
                let matched = staged.iter().any(|candidate| {
                    candidate.spec.extension.trim() == target_extension
                        && candidate.spec.classifier.trim() == classifier
                });
                if !matched {
                    return Err(upload_problem(
                        422,
                        "maven_signature_target_missing",
                        "Maven signature target missing",
                        "Every signature must map to an uploaded asset with the same classifier",
                    ));
                }
            }
        }

        let mut plugin: Option<MavenPluginDescriptor> = None;
        if m.packaging == "maven-plugin" {
            let mut jar_index: Option<usize> = None;
            for (index, asset) in staged.iter().enumerate() {
                if asset.spec.extension.trim().eq_ignore_ascii_case("jar")
                    && asset.spec.classifier.trim().is_empty()
                {
                    if jar_index.is_some() {
                        return Err(upload_problem(
                            422,
                            "maven_plugin_jar_count",
                            "Maven plugin JAR is ambiguous",
                            "Provide exactly one unclassified plugin JAR",
                        ));
                    }
                    jar_index = Some(index);
                }
            }
            let Some(jar_index) = jar_index else {
                return Err(upload_problem(
                    422,
                    "maven_plugin_jar_required",
                    "Maven plugin JAR required",
                    "Provide one unclassified JAR containing META-INF/maven/plugin.xml",
                ));
            };
            plugin = Some(self.inspect_maven_plugin(&staged[jar_index], &m).await?);
        }

        if let Some(pom_index) = pom_index {
            if m.generate_pom {
                return Err(upload_problem(
                    422,
                    "pom_mode_conflict",
                    "Invalid POM selection",
                    "generate_pom must be false when a POM is supplied",
                ));
            }
            self.validate_uploaded_pom(&staged[pom_index], &m).await?;
            let pom_path = format!("{base}.pom");
            for asset in &mut planned {
                if asset.path == pom_path {
                    asset.role = "metadata".to_string();
                }
            }
        } else {
            if !m.generate_pom {
                return Err(upload_problem(
                    422,
                    "pom_required",
                    "POM required",
                    "Supply an unclassified POM or enable POM generation",
                ));
            }
            let pom_bytes = render_generated_pom(&m);
            let pom_asset = self
                .stage_generated(
                    &format!("{base}.pom"),
                    "metadata",
                    "application/xml",
                    &pom_bytes,
                )
                .await
                .map_err(|_| {
                    upload_problem(
                        503,
                        "storage_unavailable",
                        "Upload storage failed",
                        "The generated POM could not be staged",
                    )
                })?;
            planned.push(pom_asset);
        }

        let publication_time = self.now();
        let Some(publication_id) = (self.new_id.read())() else {
            return Err(upload_problem(
                503,
                "storage_unavailable",
                "Upload unavailable",
                "Could not allocate a publication identifier",
            ));
        };
        let mut publication = ArtifactPublication {
            id: publication_id,
            repo_id: repository.id,
            format: meta::FORMAT_MAVEN.to_string(),
            package_name: format!("{}:{}", m.group_id, m.artifact_id),
            version: m.version.clone(),
            coordinate: format!("{}:{}:{}", m.group_id, m.artifact_id, m.version),
            upload_id: request.upload_id.clone(),
            created_by: request.principal_name.clone(),
            created_by_source: request.principal_source.clone(),
            created_at: publication_time,
            updated_at: publication_time,
            ..Default::default()
        };
        let mut managed_metadata = serde_json::Map::new();
        if let Some(plugin) = &plugin {
            managed_metadata.insert(
                "format_metadata".to_string(),
                serde_json::json!({
                    "plugin_name": plugin.name,
                    "plugin_prefix": plugin.goal_prefix,
                }),
            );
        }
        managed_metadata.insert("format".to_string(), serde_json::json!("maven"));
        managed_metadata.insert("managed_by".to_string(), serde_json::json!("ui_upload"));
        managed_metadata.insert("schema_version".to_string(), serde_json::json!(1));
        let managed_json =
            serde_json::to_string(&sorted_object(managed_metadata)).unwrap_or_default();

        let mut create: Vec<Artifact> = Vec::with_capacity(planned.len() * 3);
        let mut created_result: Vec<UploadedArtifact> = Vec::with_capacity(planned.len() * 3);
        for asset in &planned {
            create.push(Artifact {
                repo_id: repository.id,
                path: asset.path.clone(),
                version: m.version.clone(),
                blob_sha256: asset.digest.clone(),
                size: asset.size,
                content_type: asset.content_type.clone(),
                metadata_json: managed_json.clone(),
                published_at: Some(publication_time),
                cached_by: request.principal_name.clone(),
                publication_id: publication.id.clone(),
                artifact_role: asset.role.clone(),
                ..Default::default()
            });
            created_result.push(uploaded_result(asset));
            for (suffix, value) in [
                (".sha1", format!("{}\n", asset.sha1)),
                (".sha256", format!("{}\n", asset.digest)),
            ] {
                let checksum_asset = self
                    .stage_generated(
                        &format!("{}{suffix}", asset.path),
                        "checksum",
                        "text/plain; charset=utf-8",
                        value.as_bytes(),
                    )
                    .await
                    .map_err(|_| {
                        upload_problem(
                            503,
                            "storage_unavailable",
                            "Upload storage failed",
                            "A checksum could not be staged",
                        )
                    })?;
                create.push(Artifact {
                    repo_id: repository.id,
                    path: checksum_asset.path.clone(),
                    version: m.version.clone(),
                    blob_sha256: checksum_asset.digest.clone(),
                    size: checksum_asset.size,
                    content_type: checksum_asset.content_type.clone(),
                    metadata_json: managed_json.clone(),
                    published_at: Some(publication_time),
                    cached_by: request.principal_name.clone(),
                    publication_id: publication.id.clone(),
                    artifact_role: "checksum".to_string(),
                    ..Default::default()
                });
                created_result.push(uploaded_result(&checksum_asset));
            }
        }

        let metadata_path = format!("{artifact_dir}/maven-metadata.xml");
        let (mut mutable, mut derived_result) = self
            .plan_maven_mutable_assets(
                repository.id,
                &metadata_path,
                &m,
                publication_time,
                MAVEN_AGGREGATE_JSON,
                &request.principal_name,
            )
            .await?;
        if let Some(plugin) = &plugin {
            let (plugin_mutable, plugin_derived) = self
                .plan_maven_plugin_mutable(
                    repository.id,
                    &group_path,
                    &m,
                    plugin,
                    MAVEN_AGGREGATE_JSON,
                    &request.principal_name,
                )
                .await?;
            mutable.extend(plugin_mutable);
            derived_result.extend(plugin_derived);
        }

        let existing = self
            .store
            .get_artifact_publication_by_identity(
                repository.id,
                meta::FORMAT_MAVEN,
                &publication.package_name,
                &publication.version,
            )
            .await;
        let mut create_batch = create.clone();
        let mut replace_batch: Vec<Artifact> = Vec::new();
        let mut remove_paths: Vec<String> = Vec::new();
        let mut created_output = created_result.clone();
        let mut replaced_output: Vec<UploadedArtifact> = Vec::new();
        let existing_ok = match &existing {
            Ok(existing_publication) => {
                let Ok(owned) = self
                    .store
                    .list_publication_artifacts(&existing_publication.id)
                    .await
                else {
                    return Err(upload_problem(
                        503,
                        "storage_unavailable",
                        "Upload unavailable",
                        "The existing Maven publication could not be inspected",
                    ));
                };
                publication.id = existing_publication.id.clone();
                publication.created_at = existing_publication.created_at;
                let existing_paths: std::collections::HashSet<String> =
                    owned.iter().map(|a| a.path.clone()).collect();
                let mut planned_paths = std::collections::HashSet::new();
                create_batch = Vec::new();
                created_output = Vec::new();
                for (index, artifact) in create.iter().enumerate() {
                    let mut artifact = artifact.clone();
                    artifact.publication_id = publication.id.clone();
                    planned_paths.insert(artifact.path.clone());
                    if existing_paths.contains(&artifact.path) {
                        replace_batch.push(artifact);
                        replaced_output.push(created_result[index].clone());
                    } else {
                        create_batch.push(artifact);
                        created_output.push(created_result[index].clone());
                    }
                }
                for artifact in &owned {
                    if !planned_paths.contains(&artifact.path) {
                        remove_paths.push(artifact.path.clone());
                    }
                }
                true
            }
            Err(meta::Error::NotFound) => false,
            Err(_) => {
                return Err(upload_problem(
                    503,
                    "storage_unavailable",
                    "Upload unavailable",
                    "Existing Maven publication state could not be read",
                ));
            }
        };

        let mut result = ArtifactUploadResult {
            upload_id: request.upload_id.clone(),
            repository: repository.name.clone(),
            format: meta::FORMAT_MAVEN.to_string(),
            coordinate: publication.coordinate.clone(),
            created: created_output,
            replaced: replaced_output,
            derived: derived_result,
            scan_status: self.scan_status(),
            durability: self.durability_value(),
            warnings: Vec::new(),
        };
        let Ok(result_json) = serde_json::to_string(&result) else {
            return Err(upload_problem(
                503,
                "result_encoding_failed",
                "Upload unavailable",
                "The upload result could not be persisted",
            ));
        };
        let mut batch = ArtifactBatch {
            expected_upload_state: UPLOAD_RECEIVING.to_string(),
            publication: publication.clone(),
            create: create_batch.clone(),
            replace: replace_batch.clone(),
            remove_paths,
            mutable_cas: mutable,
            upload_result_json: result_json,
            ..Default::default()
        };
        if existing_ok && !manifest.overwrite {
            let conflicts: Vec<String> = batch.replace.iter().map(|a| a.path.clone()).collect();
            let mut p = upload_problem(
                409,
                "artifact_conflict",
                "Maven publication already exists",
                "Confirm replacement to atomically replace the complete publication",
            );
            p.conflicts = conflicts;
            p.conflict_action = "replace".to_string();
            batch.expected_upload_state = UPLOAD_CONFLICT.to_string();
            if self
                .retain_maven_conflict(&request.upload_id, &p, batch)
                .await
                .is_err()
            {
                return Err(upload_problem(
                    503,
                    "storage_unavailable",
                    "Upload unavailable",
                    "The Maven replacement plan could not be retained",
                ));
            }
            return Err(p);
        }
        if existing_ok && manifest.overwrite && !allow_replace {
            return Err(upload_problem(
                403,
                "delete_permission_required",
                "Replacement forbidden",
                "Maven replacement requires repository delete permission",
            ));
        }

        let mut err = Ok(());
        for _ in 0..3 {
            err = self
                .store
                .apply_artifact_batch(key.clone(), batch.clone())
                .await;
            if !matches!(err, Err(meta::Error::DerivedMetadataChanged)) {
                break;
            }
            let (mut replanned, mut replanned_derived) = self
                .plan_maven_mutable_assets(
                    repository.id,
                    &metadata_path,
                    &m,
                    publication_time,
                    MAVEN_AGGREGATE_JSON,
                    &request.principal_name,
                )
                .await?;
            if let Some(plugin) = &plugin {
                let (plugin_mutable, plugin_derived) = self
                    .plan_maven_plugin_mutable(
                        repository.id,
                        &group_path,
                        &m,
                        plugin,
                        MAVEN_AGGREGATE_JSON,
                        &request.principal_name,
                    )
                    .await?;
                replanned.extend(plugin_mutable);
                replanned_derived.extend(plugin_derived);
            }
            batch.mutable_cas = replanned;
            result.derived = replanned_derived;
            batch.upload_result_json = serde_json::to_string(&result).unwrap_or_default();
        }
        match err {
            Err(meta::Error::ArtifactConflict) => {
                let mut conflicts = Vec::new();
                for artifact in &create {
                    if self
                        .store
                        .get_artifact(repository.id, &artifact.path)
                        .await
                        .is_ok()
                    {
                        conflicts.push(artifact.path.clone());
                    }
                }
                let mut p = upload_problem(
                    409,
                    "artifact_conflict",
                    "Artifact already exists",
                    "One or more immutable Maven paths already exist",
                );
                p.conflicts = conflicts;
                p.conflict_action = "reject".to_string();
                Err(p)
            }
            Err(meta::Error::DerivedMetadataChanged) => {
                let mut p = upload_problem(
                    409,
                    "concurrent_metadata_update",
                    "Maven metadata changed",
                    "Retry the upload against the current package metadata",
                );
                p.retryable = true;
                Err(p)
            }
            Err(err) => {
                tracing::error!(
                    repo = %repository.name, upload_id = %request.upload_id, err = %err,
                    "commit Maven UI upload"
                );
                Err(upload_problem(
                    503,
                    "storage_unavailable",
                    "Upload commit failed",
                    "The artifact publication could not be committed",
                ))
            }
            Ok(()) => {
                let mut committed = create_batch;
                committed.extend(replace_batch);
                self.record_committed(repository, &committed);
                self.notify_published(repository, &publication, &request.principal_name)
                    .await;
                Ok(result)
            }
        }
    }

    /// Leases every staged blob and stores the replacement plan against the
    /// idempotency key so a later confirmation can commit it unchanged.
    async fn retain_maven_conflict(
        &self,
        upload_id: &str,
        problem: &UploadProblem,
        batch: ArtifactBatch,
    ) -> Result<(), meta::Error> {
        let mut leased = std::collections::HashSet::new();
        let mut pending: Vec<(String, i64)> = Vec::new();
        for artifact in batch.create.iter().chain(batch.replace.iter()) {
            if leased.insert(artifact.blob_sha256.clone()) {
                pending.push((artifact.blob_sha256.clone(), artifact.size));
            }
        }
        for cas in &batch.mutable_cas {
            if leased.insert(cas.artifact.blob_sha256.clone()) {
                pending.push((cas.artifact.blob_sha256.clone(), cas.artifact.size));
            }
        }
        for (digest, size) in pending {
            self.store
                .lease_upload_blob(upload_id, &digest, size)
                .await?;
        }
        let plan_json = serde_json::to_string(&MavenConflictPlan {
            problem: problem.clone(),
            batch,
        })
        .map_err(|e| meta::Error::Other(e.to_string()))?;
        self.store
            .set_upload_state(
                upload_id,
                UPLOAD_RECEIVING,
                UPLOAD_CONFLICT,
                &plan_json,
                self.now() + chrono::TimeDelta::minutes(30),
            )
            .await
    }

    /// Applies a retained replacement after the API adapter has rechecked
    /// write/delete permission, CSRF, repository identity and source ACL.
    pub async fn commit_maven_conflict(
        self: &Arc<Self>,
        repository: &Repository,
        upload_id: &str,
        principal_name: &str,
        principal_source: &str,
    ) -> PublishResult {
        let upload_not_found = || {
            upload_problem(
                404,
                "upload_not_found",
                "Upload not found",
                "The retained upload was not found",
            )
        };
        let Ok(request) = self.store.get_upload_request_by_id(upload_id).await else {
            return Err(upload_not_found());
        };
        if request.repo_id != repository.id
            || request.principal_name != principal_name
            || request.principal_source != principal_source
        {
            return Err(upload_not_found());
        }
        if request.state != UPLOAD_CONFLICT {
            return Err(upload_problem(
                409,
                "upload_not_confirmable",
                "Upload cannot be confirmed",
                "Only a retained Maven conflict can be confirmed",
            ));
        }
        if request.expires_at <= self.now() {
            return Err(upload_problem(
                409,
                "conflict_plan_expired",
                "Replacement plan expired",
                "Upload the files again to create a current replacement plan",
            ));
        }
        let plan_invalid = || {
            upload_problem(
                409,
                "conflict_plan_invalid",
                "Replacement plan unavailable",
                "The retained replacement plan could not be decoded",
            )
        };
        let Ok(mut plan) = serde_json::from_str::<MavenConflictPlan>(&request.plan_json) else {
            return Err(plan_invalid());
        };
        if plan.batch.publication.format != meta::FORMAT_MAVEN {
            return Err(plan_invalid());
        }
        let key = UploadRequestKey {
            repo_id: repository.id,
            principal_name: principal_name.to_string(),
            principal_source: principal_source.to_string(),
            idempotency_key: request.idempotency_key.clone(),
        };
        let mut commit_err = Ok(());
        for _ in 0..3 {
            commit_err = self
                .store
                .apply_artifact_batch(key.clone(), plan.batch.clone())
                .await;
            if !matches!(commit_err, Err(meta::Error::DerivedMetadataChanged)) {
                break;
            }
            let parts: Vec<&str> = plan.batch.publication.package_name.split(':').collect();
            if parts.len() != 2 {
                break;
            }
            let metadata_path = format!(
                "{}/{}/maven-metadata.xml",
                parts[0].replace('.', "/"),
                parts[1]
            );
            let replan = self
                .plan_maven_mutable_assets(
                    repository.id,
                    &metadata_path,
                    &MavenUploadManifest {
                        group_id: parts[0].to_string(),
                        artifact_id: parts[1].to_string(),
                        version: plan.batch.publication.version.clone(),
                        ..Default::default()
                    },
                    self.now(),
                    MAVEN_AGGREGATE_JSON,
                    principal_name,
                )
                .await;
            let Ok((mutable, derived)) = replan else {
                break;
            };
            plan.batch.mutable_cas = mutable;
            if let Ok(mut result) =
                serde_json::from_str::<ArtifactUploadResult>(&plan.batch.upload_result_json)
            {
                result.derived = derived;
                plan.batch.upload_result_json = serde_json::to_string(&result).unwrap_or_default();
            }
        }
        match commit_err {
            Err(meta::Error::ArtifactConflict) | Err(meta::Error::DerivedMetadataChanged) => {
                let mut p = upload_problem(
                    409,
                    "conflict_plan_changed",
                    "Maven publication changed",
                    "The publication changed after review; upload again before replacing it",
                );
                p.retryable = true;
                return Err(p);
            }
            Err(_) => {
                return Err(upload_problem(
                    503,
                    "storage_unavailable",
                    "Replacement failed",
                    "The Maven replacement could not be committed",
                ));
            }
            Ok(()) => {}
        }
        let Ok(result) =
            serde_json::from_str::<ArtifactUploadResult>(&plan.batch.upload_result_json)
        else {
            return Err(upload_problem(
                503,
                "stored_result_invalid",
                "Stored result unavailable",
                "The committed upload result could not be decoded",
            ));
        };
        let mut committed = plan.batch.create;
        committed.extend(plan.batch.replace);
        self.record_committed(repository, &committed);
        Ok(result)
    }

    pub async fn cancel_maven_conflict(
        &self,
        repository: &Repository,
        upload_id: &str,
        principal_name: &str,
        principal_source: &str,
    ) -> Option<Box<UploadProblem>> {
        let upload_not_found = || {
            Some(upload_problem(
                404,
                "upload_not_found",
                "Upload not found",
                "The retained upload was not found",
            ))
        };
        let Ok(request) = self.store.get_upload_request_by_id(upload_id).await else {
            return upload_not_found();
        };
        if request.repo_id != repository.id
            || request.principal_name != principal_name
            || request.principal_source != principal_source
        {
            return upload_not_found();
        }
        match self.store.cancel_upload_conflict(upload_id).await {
            Ok(()) => None,
            Err(meta::Error::UploadState) => Some(upload_problem(
                409,
                "upload_not_cancellable",
                "Upload cannot be cancelled",
                "Only a retained conflict can be cancelled",
            )),
            Err(_) => Some(upload_problem(
                503,
                "storage_unavailable",
                "Cancellation failed",
                "The retained upload could not be cancelled",
            )),
        }
    }

    async fn validate_uploaded_pom(
        &self,
        asset: &StagedUploadAsset,
        m: &MavenUploadManifest,
    ) -> Result<(), Box<UploadProblem>> {
        let value = self
            .read_bounded_blob(&asset.digest, self.cfg.archive_max_meta_bytes)
            .await
            .map_err(|_| {
                upload_problem(
                    422,
                    "pom_too_large",
                    "POM too large",
                    "The uploaded POM exceeds the metadata parsing limit",
                )
            })?;
        if contains_xml_directive(&value) {
            return Err(upload_problem(
                422,
                "pom_invalid",
                "Invalid POM",
                "XML directives and external entities are not allowed",
            ));
        }
        let pom_invalid = || {
            upload_problem(
                422,
                "pom_invalid",
                "Invalid POM",
                "The uploaded file is not a valid Maven project POM",
            )
        };
        let pom = parse_maven_pom(&value).map_err(|_| pom_invalid())?;
        if pom.root != "project" {
            return Err(pom_invalid());
        }
        let mut group_id = pom.group_id.trim().to_string();
        let mut version = pom.version.trim().to_string();
        if group_id.is_empty() {
            group_id = pom.parent_group_id.trim().to_string();
        }
        if version.is_empty() {
            version = pom.parent_version.trim().to_string();
        }
        for value in [&group_id, &pom.artifact_id, &version] {
            if value.contains("${") {
                return Err(upload_problem(
                    422,
                    "pom_requires_flattening",
                    "POM requires flattening",
                    "Effective Maven coordinates may not contain unresolved properties",
                ));
            }
        }
        if group_id != m.group_id || pom.artifact_id.trim() != m.artifact_id || version != m.version
        {
            return Err(upload_problem(
                422,
                "pom_coordinate_mismatch",
                "POM coordinates do not match",
                "The supplied POM GAV differs from the manifest",
            ));
        }
        let packaging = pom.packaging.trim();
        if !packaging.is_empty() && packaging != m.packaging {
            return Err(upload_problem(
                422,
                "pom_packaging_mismatch",
                "POM packaging does not match",
                "The supplied POM packaging differs from the manifest",
            ));
        }
        Ok(())
    }

    async fn inspect_maven_plugin(
        &self,
        asset: &StagedUploadAsset,
        manifest: &MavenUploadManifest,
    ) -> Result<MavenPluginDescriptor, Box<UploadProblem>> {
        let Some(seekable) = self.engine.blobs.as_seekable() else {
            return Err(upload_problem(
                503,
                "seekable_staging_unavailable",
                "Archive staging unavailable",
                "The blob store cannot validate a Maven plugin JAR",
            ));
        };
        let (file, _size) = seekable.open_seekable(&asset.digest).await.map_err(|_| {
            upload_problem(
                503,
                "storage_unavailable",
                "Plugin JAR unavailable",
                "The staged plugin JAR could not be opened",
            )
        })?;
        let max_entries = self.cfg.archive_max_entries;
        let max_meta = self.cfg.archive_max_meta_bytes;
        // ZIP parsing is blocking; the staged file is a local handle.
        let descriptor_bytes = tokio::task::spawn_blocking(move || {
            read_plugin_descriptor(file, max_entries, max_meta)
        })
        .await
        .unwrap_or(Err(PluginJarError::Invalid))
        .map_err(|err| match err {
            PluginJarError::Invalid => upload_problem(
                422,
                "maven_plugin_jar_invalid",
                "Invalid Maven plugin JAR",
                "The plugin JAR is malformed or exceeds the entry limit",
            ),
            PluginJarError::DescriptorInvalid => upload_problem(
                422,
                "maven_plugin_descriptor_invalid",
                "Invalid Maven plugin descriptor",
                "Exactly one bounded regular META-INF/maven/plugin.xml is required",
            ),
            PluginJarError::DescriptorMissing => upload_problem(
                422,
                "maven_plugin_descriptor_missing",
                "Maven plugin descriptor missing",
                "The JAR must contain META-INF/maven/plugin.xml",
            ),
        })?;
        if contains_xml_directive(&descriptor_bytes) {
            return Err(upload_problem(
                422,
                "maven_plugin_descriptor_missing",
                "Maven plugin descriptor missing",
                "The JAR must contain META-INF/maven/plugin.xml",
            ));
        }
        let mut descriptor = parse_plugin_descriptor(&descriptor_bytes).map_err(|_| {
            upload_problem(
                422,
                "maven_plugin_descriptor_invalid",
                "Invalid Maven plugin descriptor",
                "The plugin descriptor is not valid XML",
            )
        })?;
        descriptor.group_id = descriptor.group_id.trim().to_string();
        descriptor.artifact_id = descriptor.artifact_id.trim().to_string();
        descriptor.version = descriptor.version.trim().to_string();
        descriptor.goal_prefix = descriptor.goal_prefix.trim().to_string();
        descriptor.name = descriptor.name.trim().to_string();
        if descriptor.group_id != manifest.group_id
            || descriptor.artifact_id != manifest.artifact_id
            || descriptor.version != manifest.version
        {
            return Err(upload_problem(
                422,
                "maven_plugin_identity_mismatch",
                "Maven plugin identity mismatch",
                "plugin.xml GAV must match the upload coordinates",
            ));
        }
        if !safe_maven_segment(&descriptor.goal_prefix, true) {
            return Err(upload_problem(
                422,
                "maven_plugin_prefix_invalid",
                "Invalid Maven plugin prefix",
                "plugin.xml goalPrefix must be a safe non-empty segment",
            ));
        }
        if descriptor.name.is_empty() {
            descriptor.name = manifest.artifact_id.clone();
        }
        Ok(descriptor)
    }

    async fn plan_maven_plugin_mutable(
        &self,
        repo_id: i64,
        group_path: &str,
        manifest: &MavenUploadManifest,
        descriptor: &MavenPluginDescriptor,
        aggregate_json: &str,
        principal: &str,
    ) -> Result<(Vec<ArtifactCAS>, Vec<UploadedArtifact>), Box<UploadProblem>> {
        let metadata_path = format!("{group_path}/maven-metadata.xml");
        let mut document = MavenPluginMetadata {
            group_id: manifest.group_id.clone(),
            plugins: Vec::new(),
        };
        let mut expected = String::new();
        match self.store.get_artifact(repo_id, &metadata_path).await {
            Ok(artifact) => {
                expected = artifact.blob_sha256.clone();
                let value = self
                    .read_bounded_blob(&artifact.blob_sha256, self.cfg.archive_max_meta_bytes)
                    .await?;
                let unsupported = || {
                    upload_problem(
                        409,
                        "derived_metadata_not_managed",
                        "Maven plugin metadata is unsupported",
                        "Existing group plugin metadata cannot be safely merged",
                    )
                };
                if contains_xml_directive(&value) || !valid_maven_plugin_metadata_xml(&value) {
                    return Err(unsupported());
                }
                document = parse_maven_plugin_metadata(&value).map_err(|_| unsupported())?;
                if document.group_id != manifest.group_id {
                    return Err(unsupported());
                }
            }
            Err(meta::Error::NotFound) => {}
            Err(_) => {
                return Err(upload_problem(
                    503,
                    "storage_unavailable",
                    "Maven plugin metadata unavailable",
                    "Existing group metadata could not be read",
                ));
            }
        }
        let mut found = false;
        for plugin in &mut document.plugins {
            if plugin.prefix == descriptor.goal_prefix && plugin.artifact_id != manifest.artifact_id
            {
                return Err(upload_problem(
                    409,
                    "plugin_prefix_conflict",
                    "Maven plugin prefix already exists",
                    "Another plugin owns the requested goal prefix",
                ));
            }
            if plugin.artifact_id == manifest.artifact_id {
                plugin.name = descriptor.name.clone();
                plugin.prefix = descriptor.goal_prefix.clone();
                found = true;
            }
        }
        if !found {
            document.plugins.push(MavenPluginEntry {
                name: descriptor.name.clone(),
                prefix: descriptor.goal_prefix.clone(),
                artifact_id: manifest.artifact_id.clone(),
            });
        }
        document.plugins.sort_by(|a, b| a.prefix.cmp(&b.prefix));
        let value = render_maven_plugin_metadata(&document);
        let metadata_asset = self
            .stage_generated(&metadata_path, "index", "application/xml", &value)
            .await
            .map_err(|_| {
                upload_problem(
                    503,
                    "storage_unavailable",
                    "Maven plugin metadata unavailable",
                    "Group metadata could not be staged",
                )
            })?;
        let mut mutable = vec![ArtifactCAS {
            artifact: Artifact {
                repo_id,
                path: metadata_path.clone(),
                blob_sha256: metadata_asset.digest.clone(),
                size: metadata_asset.size,
                content_type: "application/xml".to_string(),
                metadata_json: aggregate_json.to_string(),
                cached_by: principal.to_string(),
                artifact_role: "index".to_string(),
                ..Default::default()
            },
            expected_sha256: expected,
        }];
        let mut derived = vec![uploaded_result(&metadata_asset)];
        for (suffix, value) in [
            (".sha1", format!("{}\n", metadata_asset.sha1)),
            (".sha256", format!("{}\n", metadata_asset.digest)),
        ] {
            let checksum_path = format!("{metadata_path}{suffix}");
            let expected_checksum = match self.store.get_artifact(repo_id, &checksum_path).await {
                Ok(current) => current.blob_sha256,
                Err(meta::Error::NotFound) => String::new(),
                Err(_) => {
                    return Err(upload_problem(
                        503,
                        "storage_unavailable",
                        "Maven plugin metadata unavailable",
                        "A group checksum could not be read",
                    ));
                }
            };
            let staged = self
                .stage_generated(
                    &checksum_path,
                    "checksum",
                    "text/plain; charset=utf-8",
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
            mutable.push(ArtifactCAS {
                artifact: Artifact {
                    repo_id,
                    path: checksum_path,
                    blob_sha256: staged.digest.clone(),
                    size: staged.size,
                    content_type: "text/plain; charset=utf-8".to_string(),
                    metadata_json: aggregate_json.to_string(),
                    cached_by: principal.to_string(),
                    artifact_role: "index".to_string(),
                    ..Default::default()
                },
                expected_sha256: expected_checksum,
            });
            derived.push(uploaded_result(&staged));
        }
        Ok((mutable, derived))
    }

    pub(crate) async fn plan_maven_mutable_assets(
        &self,
        repo_id: i64,
        metadata_path: &str,
        m: &MavenUploadManifest,
        publication_time: chrono::DateTime<chrono::Utc>,
        aggregate_json: &str,
        principal: &str,
    ) -> Result<(Vec<ArtifactCAS>, Vec<UploadedArtifact>), Box<UploadProblem>> {
        let (metadata, expected_metadata) = self
            .plan_maven_metadata(repo_id, metadata_path, m, publication_time)
            .await?;
        let metadata_bytes = render_maven_metadata(&metadata);
        let metadata_asset = self
            .stage_generated(metadata_path, "index", "application/xml", &metadata_bytes)
            .await
            .map_err(|_| {
                upload_problem(
                    503,
                    "storage_unavailable",
                    "Upload storage failed",
                    "Maven metadata could not be staged",
                )
            })?;
        let mut mutable = vec![ArtifactCAS {
            artifact: Artifact {
                repo_id,
                path: metadata_path.to_string(),
                blob_sha256: metadata_asset.digest.clone(),
                size: metadata_asset.size,
                content_type: metadata_asset.content_type.clone(),
                metadata_json: aggregate_json.to_string(),
                cached_by: principal.to_string(),
                artifact_role: "index".to_string(),
                ..Default::default()
            },
            expected_sha256: expected_metadata,
        }];
        let mut derived = vec![uploaded_result(&metadata_asset)];
        for (suffix, value) in [
            (".sha1", format!("{}\n", metadata_asset.sha1)),
            (".sha256", format!("{}\n", metadata_asset.digest)),
        ] {
            let checksum_path = format!("{metadata_path}{suffix}");
            let asset = self
                .stage_generated(
                    &checksum_path,
                    "checksum",
                    "text/plain; charset=utf-8",
                    value.as_bytes(),
                )
                .await
                .map_err(|_| {
                    upload_problem(
                        503,
                        "storage_unavailable",
                        "Upload storage failed",
                        "A metadata checksum could not be staged",
                    )
                })?;
            let expected = self
                .expected_artifact_digest(repo_id, &checksum_path)
                .await?;
            mutable.push(ArtifactCAS {
                artifact: Artifact {
                    repo_id,
                    path: checksum_path,
                    blob_sha256: asset.digest.clone(),
                    size: asset.size,
                    content_type: asset.content_type.clone(),
                    metadata_json: aggregate_json.to_string(),
                    cached_by: principal.to_string(),
                    artifact_role: "index".to_string(),
                    ..Default::default()
                },
                expected_sha256: expected,
            });
            derived.push(uploaded_result(&asset));
        }
        Ok((mutable, derived))
    }

    async fn plan_maven_metadata(
        &self,
        repo_id: i64,
        metadata_path: &str,
        m: &MavenUploadManifest,
        publication_time: chrono::DateTime<chrono::Utc>,
    ) -> Result<(MavenMetadata, String), Box<UploadProblem>> {
        let mut metadata = MavenMetadata {
            group_id: m.group_id.clone(),
            artifact_id: m.artifact_id.clone(),
            versioning: MavenVersioning::default(),
        };
        let mut expected = String::new();
        match self.store.get_artifact(repo_id, metadata_path).await {
            Ok(artifact) => {
                if !meta::is_ui_managed_aggregate_metadata(&artifact.metadata_json) {
                    return Err(upload_problem(
                        409,
                        "derived_metadata_not_managed",
                        "Existing Maven metadata is unsupported",
                        "Existing metadata is not managed by artifact upload",
                    ));
                }
                expected = artifact.blob_sha256.clone();
                let value = self
                    .read_bounded_blob(&artifact.blob_sha256, self.cfg.archive_max_meta_bytes)
                    .await
                    .map_err(|_| {
                        upload_problem(
                            503,
                            "storage_unavailable",
                            "Maven metadata unavailable",
                            "Existing metadata bytes could not be opened",
                        )
                    })?;
                let unsupported = || {
                    upload_problem(
                        409,
                        "derived_metadata_not_managed",
                        "Existing Maven metadata is unsupported",
                        "Existing metadata cannot be safely merged",
                    )
                };
                if contains_xml_directive(&value) || !valid_maven_metadata_xml(&value) {
                    return Err(unsupported());
                }
                metadata = parse_maven_metadata(&value).map_err(|_| unsupported())?;
                if metadata.group_id != m.group_id || metadata.artifact_id != m.artifact_id {
                    return Err(upload_problem(
                        409,
                        "derived_metadata_not_managed",
                        "Existing Maven metadata is inconsistent",
                        "Existing metadata coordinates do not match the package path",
                    ));
                }
            }
            Err(meta::Error::NotFound) => {}
            Err(_) => {
                return Err(upload_problem(
                    503,
                    "storage_unavailable",
                    "Maven metadata unavailable",
                    "Existing package metadata could not be read",
                ));
            }
        }
        if !metadata.versioning.versions.contains(&m.version) {
            metadata.versioning.versions.push(m.version.clone());
        }
        metadata.versioning.latest = m.version.clone();
        metadata.versioning.release = m.version.clone();
        metadata.versioning.last_updated = publication_time.format("%Y%m%d%H%M%S").to_string();
        Ok((metadata, expected))
    }

    /// Stages a generated document into the blob store and records its lease.
    pub(crate) async fn stage_generated(
        &self,
        path: &str,
        role: &str,
        content_type: &str,
        value: &[u8],
    ) -> Result<MavenPlannedAsset, meta::Error> {
        let (digest, size) = self
            .engine
            .blobs
            .put(Box::pin(std::io::Cursor::new(value.to_vec())))
            .await
            .map_err(|e| meta::Error::Other(e.to_string()))?;
        self.store.ensure_blob(&digest, size).await?;
        Ok(MavenPlannedAsset {
            path: path.to_string(),
            role: role.to_string(),
            content_type: content_type.to_string(),
            digest,
            sha1: hex::encode(sha1::Sha1::digest(value)),
            size,
        })
    }

    async fn expected_artifact_digest(
        &self,
        repo_id: i64,
        artifact_path: &str,
    ) -> Result<String, Box<UploadProblem>> {
        match self.store.get_artifact(repo_id, artifact_path).await {
            Err(meta::Error::NotFound) => Ok(String::new()),
            Err(_) => Err(upload_problem(
                503,
                "storage_unavailable",
                "Metadata unavailable",
                "Existing derived metadata could not be read",
            )),
            Ok(artifact) => {
                if !meta::is_ui_managed_aggregate_metadata(&artifact.metadata_json) {
                    return Err(upload_problem(
                        409,
                        "derived_metadata_not_managed",
                        "Existing Maven metadata is unsupported",
                        "An existing metadata checksum is not managed by artifact upload",
                    ));
                }
                Ok(artifact.blob_sha256)
            }
        }
    }
}

fn sorted_object(
    object: serde_json::Map<String, serde_json::Value>,
) -> std::collections::BTreeMap<String, serde_json::Value> {
    object.into_iter().collect()
}

pub(crate) fn uploaded_result(asset: &MavenPlannedAsset) -> UploadedArtifact {
    UploadedArtifact {
        path: asset.path.clone(),
        role: asset.role.clone(),
        size: asset.size,
        sha256: asset.digest.clone(),
    }
}

fn validate_maven_coordinates(m: &MavenUploadManifest) -> Option<Box<UploadProblem>> {
    let invalid = |detail: &str| {
        Some(upload_problem(
            422,
            "maven_coordinates_invalid",
            "Invalid Maven coordinates",
            detail,
        ))
    };
    for group in m.group_id.split('.') {
        if group.is_empty() || !maven_identifier(group) {
            return invalid("group_id must contain safe dot-separated segments");
        }
    }
    if !safe_maven_segment(&m.artifact_id, true) || !safe_maven_segment(&m.version, true) {
        return invalid("artifact_id and version must be safe path segments");
    }
    if m.version.to_uppercase().ends_with("-SNAPSHOT") {
        return Some(upload_problem(
            422,
            "maven_snapshot_not_supported",
            "Maven snapshots require native deployment",
            "UI upload currently supports Maven release versions only",
        ));
    }
    if m.packaging.is_empty() || !safe_maven_segment(&m.packaging, true) {
        return Some(upload_problem(
            422,
            "maven_packaging_invalid",
            "Invalid Maven packaging",
            "packaging must be a safe non-empty value",
        ));
    }
    None
}

fn maven_identifier(value: &str) -> bool {
    if value == "." || value == ".." || value.is_empty() {
        return false;
    }
    value
        .chars()
        .all(|r| r.is_ascii_alphanumeric() || r == '_' || r == '-')
}

pub(crate) fn safe_maven_segment(value: &str, allow_dot: bool) -> bool {
    if value.is_empty()
        || value.starts_with('.')
        || value.contains("..")
        || value.contains('/')
        || value.contains('\\')
    {
        return false;
    }
    !value
        .chars()
        .any(|r| r == '\0' || (r as u32) < 0x20 || r as u32 == 0x7f || (!allow_dot && r == '.'))
}

fn render_generated_pom(m: &MavenUploadManifest) -> Vec<u8> {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<project>\n");
    out.push_str("  <modelVersion>4.0.0</modelVersion>\n");
    out.push_str(&format!(
        "  <groupId>{}</groupId>\n",
        escape_xml(&m.group_id)
    ));
    out.push_str(&format!(
        "  <artifactId>{}</artifactId>\n",
        escape_xml(&m.artifact_id)
    ));
    out.push_str(&format!(
        "  <version>{}</version>\n",
        escape_xml(&m.version)
    ));
    if !m.packaging.is_empty() {
        out.push_str(&format!(
            "  <packaging>{}</packaging>\n",
            escape_xml(&m.packaging)
        ));
    }
    out.push_str("</project>\n");
    out.into_bytes()
}

/// Renders a `maven-metadata.xml` document.
///
pub(crate) fn render_maven_metadata(metadata: &MavenMetadata) -> Vec<u8> {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata>\n");
    out.push_str(&format!(
        "  <groupId>{}</groupId>\n",
        escape_xml(&metadata.group_id)
    ));
    out.push_str(&format!(
        "  <artifactId>{}</artifactId>\n",
        escape_xml(&metadata.artifact_id)
    ));
    out.push_str("  <versioning>\n");
    out.push_str(&format!(
        "    <latest>{}</latest>\n",
        escape_xml(&metadata.versioning.latest)
    ));
    out.push_str(&format!(
        "    <release>{}</release>\n",
        escape_xml(&metadata.versioning.release)
    ));
    if !metadata.versioning.versions.is_empty() {
        out.push_str("    <versions>\n");
        for version in &metadata.versioning.versions {
            out.push_str(&format!(
                "      <version>{}</version>\n",
                escape_xml(version)
            ));
        }
        out.push_str("    </versions>\n");
    }
    out.push_str(&format!(
        "    <lastUpdated>{}</lastUpdated>\n",
        escape_xml(&metadata.versioning.last_updated)
    ));
    out.push_str("  </versioning>\n</metadata>\n");
    out.into_bytes()
}

/// Renders a group-level plugin `maven-metadata.xml`.
pub(crate) fn render_maven_plugin_metadata(document: &MavenPluginMetadata) -> Vec<u8> {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata>\n");
    out.push_str(&format!(
        "  <groupId>{}</groupId>\n",
        escape_xml(&document.group_id)
    ));
    if !document.plugins.is_empty() {
        out.push_str("  <plugins>\n");
        for plugin in &document.plugins {
            out.push_str("    <plugin>\n");
            out.push_str(&format!(
                "      <name>{}</name>\n",
                escape_xml(&plugin.name)
            ));
            out.push_str(&format!(
                "      <prefix>{}</prefix>\n",
                escape_xml(&plugin.prefix)
            ));
            out.push_str(&format!(
                "      <artifactId>{}</artifactId>\n",
                escape_xml(&plugin.artifact_id)
            ));
            out.push_str("    </plugin>\n");
        }
        out.push_str("  </plugins>\n");
    }
    out.push_str("</metadata>\n");
    out.into_bytes()
}

fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&#34;"),
            '\t' => out.push_str("&#x9;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            _ => out.push(c),
        }
    }
    out
}

pub(crate) fn contains_xml_directive(value: &[u8]) -> bool {
    let upper = value.to_ascii_uppercase();
    upper.windows(9).any(|w| w == b"<!DOCTYPE") || upper.windows(8).any(|w| w == b"<!ENTITY")
}

/// Walks an XML document, calling `visit` with (element stack, text) for every
/// character-data run and `allow` for every start element, and reports whether
/// the document is well formed and every element was allowed.
fn walk_xml(
    value: &[u8],
    mut visit: impl FnMut(&[String], &str),
    mut allow: impl FnMut(&str, &str, bool) -> bool,
) -> bool {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_reader(value);
    reader.config_mut().trim_text(true);
    let mut stack: Vec<String> = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.local_name().as_ref().to_string();
                let parent = stack.last().cloned().unwrap_or_default();
                let has_attributes = e.attributes().next().is_some();
                if !allow(&parent, &name, has_attributes) {
                    return false;
                }
                stack.push(name);
            }
            // A self-closing element (`<plugins/>`) is one event rather than a Start/End pair.
            Ok(Event::Empty(e)) => {
                let name = e.local_name().as_ref().to_string();
                let parent = stack.last().cloned().unwrap_or_default();
                let has_attributes = e.attributes().next().is_some();
                if !allow(&parent, &name, has_attributes) {
                    return false;
                }
                stack.push(name);
                visit(&stack, "");
                stack.pop();
            }
            Ok(Event::End(_)) => {
                if stack.pop().is_none() {
                    return false;
                }
            }
            Ok(Event::Text(e)) => {
                let text = e.xml_content(quick_xml::XmlVersion::default()).into_owned();
                visit(&stack, &text);
            }
            Ok(Event::Eof) => return stack.is_empty(),
            Ok(_) => {}
            Err(_) => return false,
        }
        buf.clear();
    }
}

/// Parses a `maven-metadata.xml` document, mirroring `xml.Unmarshal` into
/// `mavenMetadata`: unknown elements are ignored and repeated elements keep the
/// last value, except `versioning/versions/version`, which accumulates.
pub(crate) fn parse_maven_metadata(value: &[u8]) -> Result<MavenMetadata, ()> {
    let mut out = MavenMetadata::default();
    let ok = walk_xml(
        value,
        |stack, text| match stack.join("/").as_str() {
            "metadata/groupId" => out.group_id = text.to_string(),
            "metadata/artifactId" => out.artifact_id = text.to_string(),
            "metadata/versioning/latest" => out.versioning.latest = text.to_string(),
            "metadata/versioning/release" => out.versioning.release = text.to_string(),
            "metadata/versioning/lastUpdated" => out.versioning.last_updated = text.to_string(),
            "metadata/versioning/versions/version" => {
                out.versioning.versions.push(text.to_string())
            }
            _ => {}
        },
        |_, _, _| true,
    );
    if ok { Ok(out) } else { Err(()) }
}

/// Parses a group-level plugin `maven-metadata.xml`.
pub(crate) fn parse_maven_plugin_metadata(value: &[u8]) -> Result<MavenPluginMetadata, ()> {
    // Both walker callbacks mutate the document (one opens a `<plugin>`, the
    // other fills its fields), so they share it through a cell.
    let out = std::cell::RefCell::new(MavenPluginMetadata::default());
    let ok = walk_xml(
        value,
        |stack, text| {
            let path = stack.join("/");
            let mut document = out.borrow_mut();
            if path == "metadata/groupId" {
                document.group_id = text.to_string();
                return;
            }
            let Some(field) = path.strip_prefix("metadata/plugins/plugin/") else {
                return;
            };
            let Some(entry) = document.plugins.last_mut() else {
                return;
            };
            match field {
                "name" => entry.name = text.to_string(),
                "prefix" => entry.prefix = text.to_string(),
                "artifactId" => entry.artifact_id = text.to_string(),
                _ => {}
            }
        },
        |parent, name, _| {
            if parent == "plugins" && name == "plugin" {
                out.borrow_mut().plugins.push(MavenPluginEntry::default());
            }
            true
        },
    );
    if ok { Ok(out.into_inner()) } else { Err(()) }
}

/// Parses a `pom.xml` project document.
fn parse_maven_pom(value: &[u8]) -> Result<MavenPOM, ()> {
    // The root-element callback and the text callback both write to the
    // document, so they share it through a cell.
    let out = std::cell::RefCell::new(MavenPOM::default());
    let ok = walk_xml(
        value,
        |stack, text| {
            let mut out = out.borrow_mut();
            match stack.join("/").as_str() {
                "project/modelVersion" => out.model = text.to_string(),
                "project/groupId" => out.group_id = text.to_string(),
                "project/artifactId" => out.artifact_id = text.to_string(),
                "project/version" => out.version = text.to_string(),
                "project/packaging" => out.packaging = text.to_string(),
                "project/parent/groupId" => out.parent_group_id = text.to_string(),
                "project/parent/version" => out.parent_version = text.to_string(),
                _ => {}
            }
        },
        |parent, name, _| {
            let mut out = out.borrow_mut();
            if parent.is_empty() && out.root.is_empty() {
                out.root = name.to_string();
            }
            true
        },
    );
    if ok { Ok(out.into_inner()) } else { Err(()) }
}

/// Parses a `META-INF/maven/plugin.xml` descriptor.
fn parse_plugin_descriptor(value: &[u8]) -> Result<MavenPluginDescriptor, ()> {
    let mut out = MavenPluginDescriptor::default();
    let ok = walk_xml(
        value,
        |stack, text| match stack.join("/").as_str() {
            "plugin/groupId" => out.group_id = text.to_string(),
            "plugin/artifactId" => out.artifact_id = text.to_string(),
            "plugin/version" => out.version = text.to_string(),
            "plugin/goalPrefix" => out.goal_prefix = text.to_string(),
            "plugin/name" => out.name = text.to_string(),
            _ => {}
        },
        |_, _, _| true,
    );
    if ok { Ok(out) } else { Err(()) }
}

pub(crate) fn valid_maven_metadata_xml(value: &[u8]) -> bool {
    walk_xml(
        value,
        |_, _| {},
        |parent, name, has_attributes| {
            let element_ok = match parent {
                "" => name == "metadata",
                "metadata" => matches!(name, "groupId" | "artifactId" | "versioning"),
                "versioning" => matches!(name, "latest" | "release" | "versions" | "lastUpdated"),
                "versions" => name == "version",
                _ => false,
            };
            element_ok && (!has_attributes || parent.is_empty())
        },
    )
}

pub(crate) fn valid_maven_plugin_metadata_xml(value: &[u8]) -> bool {
    walk_xml(
        value,
        |_, _| {},
        |parent, name, _| match parent {
            "" => name == "metadata",
            "metadata" => matches!(name, "groupId" | "plugins"),
            "plugins" => name == "plugin",
            "plugin" => matches!(name, "name" | "prefix" | "artifactId"),
            _ => false,
        },
    )
}

/// Why a plugin JAR could not be inspected.
enum PluginJarError {
    Invalid,
    DescriptorInvalid,
    DescriptorMissing,
}

/// Reads the single bounded `META-INF/maven/plugin.xml` entry out of a plugin
/// JAR. Blocking: the caller runs it on the blocking pool.
fn read_plugin_descriptor(
    file: Box<dyn crate::storage::ReadSeekCloser>,
    max_entries: i64,
    max_meta_bytes: i64,
) -> Result<Vec<u8>, PluginJarError> {
    use std::io::Read as _;

    let mut archive = zip::ZipArchive::new(file).map_err(|_| PluginJarError::Invalid)?;
    if archive.len() as i64 > max_entries {
        return Err(PluginJarError::Invalid);
    }
    let mut descriptor: Option<Vec<u8>> = None;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|_| PluginJarError::Invalid)?;
        if clean_zip_path(entry.name()) != "META-INF/maven/plugin.xml" {
            continue;
        }
        if descriptor.is_some() || entry.is_dir() || entry.size() > max_meta_bytes as u64 {
            return Err(PluginJarError::DescriptorInvalid);
        }
        let mut bytes = Vec::new();
        entry
            .by_ref()
            .take(max_meta_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| PluginJarError::DescriptorInvalid)?;
        if bytes.len() as i64 > max_meta_bytes {
            return Err(PluginJarError::DescriptorInvalid);
        }
        descriptor = Some(bytes);
    }
    descriptor.ok_or(PluginJarError::DescriptorMissing)
}

fn clean_zip_path(name: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for segment in name.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out.join("/")
}
