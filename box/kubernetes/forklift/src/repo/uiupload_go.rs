//! The Go publisher: module ZIP validation, the GOPROXY `.mod`/`.info`/`.zip`
//! triplet and the `@v/list` / `@latest` indexes.
//!
//! Validates module identity, escaping, semantic versions and ZIP layout
//! according to the module proxy specification. Shared version primitives live
//! in `group_metadata.rs`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::io::AsyncReadExt as _;

use crate::meta::{
    self, Artifact, ArtifactBatch, ArtifactCAS, ArtifactPublication, ArtifactUploadRequest,
    Repository, UPLOAD_RECEIVING, UploadRequestKey,
};

use super::group_metadata::{
    go_semver_build, go_semver_canonical, go_semver_compare, go_semver_major, go_semver_prerelease,
    go_semver_valid,
};
use super::uiupload::{
    ArtifactUploadManifest, ArtifactUploadResult, PublishResult, StagedUploadAsset, UploadProblem,
    UploadWarning, UploadedArtifact, Uploader, upload_problem,
};
use super::uiupload_maven::uploaded_result;

/// A GOPROXY `<version>.info` / `@latest` document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GoInfo {
    #[serde(rename = "Version", default)]
    pub(crate) version: String,
    #[serde(
        rename = "Time",
        default = "zero_time",
        serialize_with = "serialize_go_time",
        deserialize_with = "deserialize_go_time"
    )]
    pub(crate) time: DateTime<Utc>,
}

impl Default for GoInfo {
    fn default() -> Self {
        GoInfo {
            version: String::new(),
            time: zero_time(),
        }
    }
}

fn zero_time() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("0001-01-01T00:00:00Z")
        .expect("valid instant")
        .with_timezone(&Utc)
}

fn serialize_go_time<S: Serializer>(t: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&meta::time::format_time(*t))
}

fn deserialize_go_time<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
    let value = String::deserialize(d)?;
    DateTime::parse_from_rfc3339(&value)
        .map(|t| t.with_timezone(&Utc))
        .map_err(serde::de::Error::custom)
}

impl Uploader {
    /// Publishes one Go module version: the uploaded ZIP plus the generated
    /// `.mod` and `.info` documents, with the `@v/list` and `@latest` indexes
    /// re-planned under compare-and-set.
    pub(crate) async fn publish_go(
        self: &Arc<Self>,
        repository: &Repository,
        request: &ArtifactUploadRequest,
        key: UploadRequestKey,
        manifest: &ArtifactUploadManifest,
        staged: &[StagedUploadAsset],
    ) -> PublishResult {
        let format_fields = || {
            upload_problem(
                422,
                "manifest_format_fields",
                "Invalid Go manifest",
                "Exactly the Go format object is required",
            )
        };
        let Some(go) = &manifest.go_ else {
            return Err(format_fields());
        };
        if manifest.maven.is_some()
            || manifest.npm.is_some()
            || manifest.pypi.is_some()
            || manifest.cargo.is_some()
        {
            return Err(format_fields());
        }
        if manifest.overwrite {
            return Err(upload_problem(
                422,
                "redeploy_not_supported",
                "Go module versions are immutable",
                "A GOPROXY version cannot be replaced",
            ));
        }
        let (module_path, version) = (go.module.trim(), go.version.trim());
        if check_path(module_path).is_err() || go_semver_canonical(version) != version {
            return Err(upload_problem(
                422,
                "go_identity_invalid",
                "Invalid Go module identity",
                "Module path and version must be canonical",
            ));
        }
        let (_, path_major, ok) = split_path_version(module_path);
        if !ok || check_path_major(version, path_major).is_err() {
            return Err(upload_problem(
                422,
                "go_major_mismatch",
                "Go module major version mismatch",
                "The version major must match the module path suffix",
            ));
        }
        if !go.time.is_empty() {
            return Err(upload_problem(
                403,
                "go_time_admin_required",
                "Release time is server controlled",
                "Only an administrative import API may provide a release time",
            ));
        }
        let (mut zip_asset, mut mod_asset) = (None, None);
        for asset in staged {
            match asset.spec.role.as_str() {
                "zip" => {
                    if zip_asset.is_some() {
                        return Err(upload_problem(
                            422,
                            "go_asset_duplicate",
                            "Duplicate Go ZIP",
                            "Exactly one module ZIP is required",
                        ));
                    }
                    zip_asset = Some(asset);
                }
                "mod" => {
                    if mod_asset.is_some() {
                        return Err(upload_problem(
                            422,
                            "go_asset_duplicate",
                            "Duplicate go.mod",
                            "At most one separate go.mod is allowed",
                        ));
                    }
                    mod_asset = Some(asset);
                }
                _ => {
                    return Err(upload_problem(
                        422,
                        "go_asset_role_invalid",
                        "Invalid Go asset role",
                        "Go assets must use role zip or mod",
                    ));
                }
            }
        }
        let Some(zip_asset) = zip_asset.filter(|a| a.filename.to_lowercase().ends_with(".zip"))
        else {
            return Err(upload_problem(
                415,
                "go_zip_required",
                "Go module ZIP required",
                "Upload exactly one .zip with role zip",
            ));
        };
        if let Some(asset) = mod_asset
            && !asset.filename.to_lowercase().ends_with(".mod")
        {
            return Err(upload_problem(
                415,
                "go_mod_invalid",
                "Invalid go.mod asset",
                "The optional role mod file must use a .mod filename",
            ));
        }
        let zip_mod = self
            .inspect_go_module_zip(zip_asset, module_path, version)
            .await?;
        let mut mod_bytes = zip_mod.clone();
        if let Some(asset) = mod_asset {
            let separate = self
                .read_bounded_blob(&asset.digest, self.cfg.archive_max_meta_bytes)
                .await?;
            if !zip_mod.is_empty() && normalize_go_mod(&zip_mod) != normalize_go_mod(&separate) {
                return Err(upload_problem(
                    422,
                    "go_mod_mismatch",
                    "go.mod mismatch",
                    "The ZIP go.mod and separate .mod bytes differ",
                ));
            }
            mod_bytes = separate;
        }
        if mod_bytes.is_empty() {
            mod_bytes = format!("module {module_path}\n").into_bytes();
        }
        if modfile_module_path(&mod_bytes) != module_path {
            return Err(upload_problem(
                422,
                "go_mod_path_mismatch",
                "go.mod module mismatch",
                "The module directive must match the requested module path",
            ));
        }
        mod_bytes = normalize_go_mod(&mod_bytes);

        let escaped_path = escape_path(module_path).unwrap_or_default();
        let escaped_version = escape_version(version).unwrap_or_default();
        let base = format!("{escaped_path}/@v/{escaped_version}");
        let publication_time = self.now();
        let mut info_time = publication_time;
        if is_pseudo_version(version) {
            let Some(parsed) = pseudo_version_time(version) else {
                return Err(upload_problem(
                    422,
                    "go_pseudo_version_invalid",
                    "Invalid pseudo-version",
                    "The pseudo-version timestamp cannot be parsed",
                ));
            };
            info_time = parsed;
        }
        let mut info_bytes = serde_json::to_vec(&GoInfo {
            version: version.to_string(),
            time: info_time,
        })
        .unwrap_or_default();
        info_bytes.push(b'\n');
        let stage_failed = |detail: &str| {
            upload_problem(503, "storage_unavailable", "Upload storage failed", detail)
        };
        let generated_mod = self
            .stage_generated(
                &format!("{base}.mod"),
                "metadata",
                "text/plain; charset=utf-8",
                &mod_bytes,
            )
            .await
            .map_err(|_| stage_failed("The Go .mod file could not be staged"))?;
        let generated_info = self
            .stage_generated(
                &format!("{base}.info"),
                "metadata",
                "application/json",
                &info_bytes,
            )
            .await
            .map_err(|_| stage_failed("The Go .info file could not be staged"))?;
        let (list_cas, latest_cas, derived) = self
            .plan_go_indexes(repository.id, &escaped_path, version, &info_bytes)
            .await?;
        let Some(publication_id) = (self.new_id.read())() else {
            return Err(upload_problem(
                503,
                "storage_unavailable",
                "Upload unavailable",
                "Could not allocate a publication identifier",
            ));
        };
        let publication = ArtifactPublication {
            id: publication_id,
            repo_id: repository.id,
            format: meta::FORMAT_GO.to_string(),
            package_name: module_path.to_string(),
            version: version.to_string(),
            coordinate: format!("{module_path}@{version}"),
            upload_id: request.upload_id.clone(),
            created_by: request.principal_name.clone(),
            created_by_source: request.principal_source.clone(),
            created_at: publication_time,
            updated_at: publication_time,
            ..Default::default()
        };
        let owned_metadata = serde_json::to_string(&serde_json::json!({
            "format": "go",
            "format_metadata": {"module_time": meta::time::format_time(info_time)},
            "managed_by": "ui_upload",
            "package": module_path,
            "role": "primary",
            "schema_version": 1,
            "source_filename": zip_asset.filename,
            "upload_id": request.upload_id,
            "version_identity": version,
        }))
        .unwrap_or_default();
        let generated = |path: &str, digest: &str, size: i64, content_type: &str| Artifact {
            repo_id: repository.id,
            path: path.to_string(),
            version: version.to_string(),
            blob_sha256: digest.to_string(),
            size,
            content_type: content_type.to_string(),
            metadata_json: owned_metadata.clone(),
            published_at: Some(publication_time),
            cached_by: request.principal_name.clone(),
            publication_id: publication.id.clone(),
            artifact_role: "metadata".to_string(),
            ..Default::default()
        };
        let create = vec![
            Artifact {
                artifact_role: "primary".to_string(),
                ..generated(
                    &format!("{base}.zip"),
                    &zip_asset.digest,
                    zip_asset.size,
                    "application/zip",
                )
            },
            generated(
                &generated_mod.path,
                &generated_mod.digest,
                generated_mod.size,
                &generated_mod.content_type,
            ),
            generated(
                &generated_info.path,
                &generated_info.digest,
                generated_info.size,
                &generated_info.content_type,
            ),
        ];
        let mut mutable = vec![latest_cas];
        if let Some(list) = list_cas {
            mutable.push(list);
        }
        let mut result = ArtifactUploadResult {
            upload_id: request.upload_id.clone(),
            repository: repository.name.clone(),
            format: meta::FORMAT_GO.to_string(),
            coordinate: publication.coordinate.clone(),
            created: vec![
                UploadedArtifact {
                    path: format!("{base}.zip"),
                    role: "primary".to_string(),
                    size: zip_asset.size,
                    sha256: zip_asset.digest.clone(),
                },
                uploaded_result(&generated_mod),
                uploaded_result(&generated_info),
            ],
            replaced: Vec::new(),
            derived,
            scan_status: self.scan_status(),
            durability: self.durability_value(),
            warnings: vec![UploadWarning {
                code: "private_module_checksum".to_string(),
                detail: "Configure GOPRIVATE or GONOSUMDB for private module paths.".to_string(),
            }],
        };
        let mut batch = ArtifactBatch {
            expected_upload_state: UPLOAD_RECEIVING.to_string(),
            publication: publication.clone(),
            create,
            mutable_cas: mutable,
            upload_result_json: serde_json::to_string(&result).unwrap_or_default(),
            ..Default::default()
        };
        let mut commit_err = Ok(());
        for _ in 0..3 {
            commit_err = self
                .store
                .apply_artifact_batch(key.clone(), batch.clone())
                .await;
            if !matches!(commit_err, Err(meta::Error::DerivedMetadataChanged)) {
                break;
            }
            let (list_cas, latest_cas, replanned) = self
                .plan_go_indexes(repository.id, &escaped_path, version, &info_bytes)
                .await?;
            batch.mutable_cas = vec![latest_cas];
            if let Some(list) = list_cas {
                batch.mutable_cas.push(list);
            }
            result.derived = replanned;
            batch.upload_result_json = serde_json::to_string(&result).unwrap_or_default();
        }
        match commit_err {
            Err(meta::Error::ArtifactConflict) => Err(upload_problem(
                409,
                "immutable_version_exists",
                "Go module version already exists",
                "The module and version are immutable",
            )),
            Err(meta::Error::DerivedMetadataChanged) => Err(upload_problem(
                409,
                "concurrent_metadata_update",
                "Go module index changed concurrently",
                "Retry with a new idempotency key",
            )),
            Err(_) => Err(upload_problem(
                503,
                "storage_unavailable",
                "Upload commit failed",
                "The Go module publication could not be committed",
            )),
            Ok(()) => {
                self.record_committed(repository, &batch.create);
                self.notify_published(repository, &publication, &request.principal_name)
                    .await;
                Ok(result)
            }
        }
    }

    /// Validates the staged module ZIP against the GOPROXY archive rules and
    /// returns its root `go.mod`, empty when the archive carries none.
    async fn inspect_go_module_zip(
        &self,
        asset: &StagedUploadAsset,
        module_path: &str,
        version: &str,
    ) -> Result<Vec<u8>, Box<UploadProblem>> {
        let Some(seekable) = self.engine.blobs.as_seekable() else {
            return Err(upload_problem(
                503,
                "seekable_staging_unavailable",
                "Archive staging unavailable",
                "The blob store cannot provide bounded random access",
            ));
        };
        let (file, size) = seekable.open_seekable(&asset.digest).await.map_err(|_| {
            upload_problem(
                503,
                "storage_unavailable",
                "Staged module unavailable",
                "The Go module ZIP could not be reopened",
            )
        })?;
        let (max_entries, max_meta) = (
            self.cfg.archive_max_entries,
            self.cfg.archive_max_meta_bytes,
        );
        let (module_path, version) = (module_path.to_string(), version.to_string());
        let outcome = tokio::task::spawn_blocking(move || {
            read_go_module_zip(file, size, &module_path, &version, max_entries, max_meta)
        })
        .await
        .unwrap_or(Err(GoZipError::ZipInvalid));
        outcome.map_err(|err| match err {
            GoZipError::ZipInvalid => upload_problem(
                422,
                "go_zip_invalid",
                "Invalid Go module ZIP",
                "The ZIP violates GOPROXY module archive rules",
            ),
            GoZipError::EntryLimit => upload_problem(
                422,
                "go_zip_invalid",
                "Invalid Go module ZIP",
                "The ZIP is malformed or exceeds the entry limit",
            ),
            GoZipError::ModInvalid => upload_problem(
                422,
                "go_mod_invalid",
                "Invalid ZIP go.mod",
                "The top-level go.mod is duplicated or too large",
            ),
        })
    }

    /// Reads a staged blob whole, refusing anything over `limit`.
    pub(crate) async fn read_bounded_blob(
        &self,
        digest: &str,
        limit: i64,
    ) -> Result<Vec<u8>, Box<UploadProblem>> {
        let (reader, _) = self.engine.blobs.open(digest).await.map_err(|_| {
            upload_problem(
                503,
                "storage_unavailable",
                "Staged metadata unavailable",
                "The staged metadata could not be reopened",
            )
        })?;
        let too_large = || {
            upload_problem(
                422,
                "metadata_too_large",
                "Metadata too large",
                "The staged metadata exceeds its parsing limit",
            )
        };
        let mut value = Vec::new();
        tokio::io::AsyncReadExt::take(reader, (limit + 1) as u64)
            .read_to_end(&mut value)
            .await
            .map_err(|_| too_large())?;
        if value.len() as i64 > limit {
            return Err(too_large());
        }
        Ok(value)
    }

    /// Plans the two module-level indexes: the `@v/list` (absent while every
    /// published version is a pseudo-version) and `@latest`, each guarded by the
    /// digest it is replacing.
    async fn plan_go_indexes(
        &self,
        repo_id: i64,
        escaped_path: &str,
        version: &str,
        current_info: &[u8],
    ) -> Result<(Option<ArtifactCAS>, ArtifactCAS, Vec<UploadedArtifact>), Box<UploadProblem>> {
        let (list_path, latest_path) = (
            format!("{escaped_path}/@v/list"),
            format!("{escaped_path}/@latest"),
        );
        let (mut versions, expected_list) = self.read_go_list(repo_id, &list_path).await?;
        if !is_pseudo_version(version) {
            versions.push(version.to_string());
        }
        let versions = unique_go_versions(versions);
        let mut list_cas = None;
        let mut derived = Vec::new();
        let aggregate_metadata = serde_json::to_string(&serde_json::json!({
            "aggregate_schema_version": 1,
            "format": "go",
            "managed_by": "ui_upload_aggregate",
            "package": escaped_path,
            "schema_version": 1,
        }))
        .unwrap_or_default();
        let stage_failed = |detail: &str| {
            upload_problem(503, "storage_unavailable", "Upload storage failed", detail)
        };
        if !versions.is_empty() {
            let list_bytes = format!("{}\n", versions.join("\n")).into_bytes();
            let list_asset = self
                .stage_generated(
                    &list_path,
                    "index",
                    "text/plain; charset=utf-8",
                    &list_bytes,
                )
                .await
                .map_err(|_| stage_failed("The Go version list could not be staged"))?;
            list_cas = Some(ArtifactCAS {
                artifact: Artifact {
                    repo_id,
                    path: list_path.clone(),
                    blob_sha256: list_asset.digest.clone(),
                    size: list_asset.size,
                    content_type: list_asset.content_type.clone(),
                    metadata_json: aggregate_metadata.clone(),
                    artifact_role: "index".to_string(),
                    ..Default::default()
                },
                expected_sha256: expected_list,
            });
            derived.push(uploaded_result(&list_asset));
        }
        let latest_info = self
            .select_go_latest(repo_id, escaped_path, version, current_info)
            .await?;
        let latest_asset = self
            .stage_generated(&latest_path, "index", "application/json", &latest_info)
            .await
            .map_err(|_| stage_failed("The Go latest metadata could not be staged"))?;
        let expected_latest = self
            .expected_managed_aggregate_digest(repo_id, &latest_path, "Go")
            .await?;
        let latest_cas = ArtifactCAS {
            artifact: Artifact {
                repo_id,
                path: latest_path,
                blob_sha256: latest_asset.digest.clone(),
                size: latest_asset.size,
                content_type: latest_asset.content_type.clone(),
                metadata_json: aggregate_metadata,
                artifact_role: "index".to_string(),
                ..Default::default()
            },
            expected_sha256: expected_latest,
        };
        derived.push(uploaded_result(&latest_asset));
        Ok((list_cas, latest_cas, derived))
    }

    /// Reads the existing `@v/list`, refusing to take over a list that was not
    /// written by managed upload (non-canonical or pseudo-version lines).
    async fn read_go_list(
        &self,
        repo_id: i64,
        list_path: &str,
    ) -> Result<(Vec<String>, String), Box<UploadProblem>> {
        let artifact = match self.store.get_artifact(repo_id, list_path).await {
            Ok(artifact) => artifact,
            Err(meta::Error::NotFound) => return Ok((Vec::new(), String::new())),
            Err(_) => {
                return Err(upload_problem(
                    503,
                    "storage_unavailable",
                    "Go list unavailable",
                    "The existing version list could not be read",
                ));
            }
        };
        let value = self
            .read_bounded_blob(&artifact.blob_sha256, self.cfg.archive_max_meta_bytes)
            .await?;
        let text = String::from_utf8_lossy(&value);
        let mut versions = Vec::new();
        for line in text.trim().split('\n') {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if go_semver_canonical(line) != line || is_pseudo_version(line) {
                return Err(upload_problem(
                    409,
                    "derived_metadata_not_managed",
                    "Existing Go list is unsupported",
                    "The version list contains a non-canonical or pseudo-version line",
                ));
            }
            versions.push(line.to_string());
        }
        Ok((versions, artifact.blob_sha256))
    }

    /// Picks the `@latest` document: releases beat pre-releases, which beat
    /// pseudo-versions, and the highest semver wins within a rank.
    async fn select_go_latest(
        &self,
        repo_id: i64,
        escaped_path: &str,
        current_version: &str,
        current_info: &[u8],
    ) -> Result<Vec<u8>, Box<UploadProblem>> {
        let mut infos: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        infos.insert(current_version.to_string(), current_info.to_vec());
        let Ok(artifacts) = self
            .store
            .list_artifacts(repo_id, &format!("{escaped_path}/@v/"))
            .await
        else {
            return Err(upload_problem(
                503,
                "storage_unavailable",
                "Go metadata unavailable",
                "Existing module versions could not be listed",
            ));
        };
        let unsupported = |detail: &str| {
            upload_problem(
                409,
                "derived_metadata_not_managed",
                "Existing Go metadata is unsupported",
                detail,
            )
        };
        let paths: std::collections::HashSet<&str> =
            artifacts.iter().map(|a| a.path.as_str()).collect();
        for artifact in &artifacts {
            let Some(base) = artifact.path.strip_suffix(".info") else {
                continue;
            };
            let escaped_version = go_path_base(&artifact.path);
            let escaped_version = escaped_version
                .strip_suffix(".info")
                .unwrap_or(&escaped_version);
            let Some(version) = unescape_version(escaped_version) else {
                return Err(unsupported("A legacy .info path has invalid escaping"));
            };
            if version == current_version {
                continue;
            }
            if !paths.contains(format!("{base}.mod").as_str())
                || !paths.contains(format!("{base}.zip").as_str())
            {
                return Err(unsupported(
                    "Every legacy .info requires matching .mod and .zip artifacts",
                ));
            }
            let value = self
                .read_bounded_blob(&artifact.blob_sha256, self.cfg.archive_max_meta_bytes)
                .await?;
            match serde_json::from_slice::<GoInfo>(&value) {
                Ok(info) if info.version == version && go_semver_canonical(&version) == version => {
                }
                _ => return Err(unsupported("A legacy .info file is malformed")),
            }
            infos.insert(version, value);
        }
        let mut versions: Vec<&String> = infos.keys().collect();
        versions.sort_by(|a, b| {
            go_latest_rank(b)
                .cmp(&go_latest_rank(a))
                .then_with(|| go_semver_compare(b, a))
        });
        let best = versions.first().map(|v| (*v).clone()).unwrap_or_default();
        Ok(infos.remove(&best).unwrap_or_default())
    }

    /// The digest an aggregate document is expected to replace, or `""` when it
    /// does not exist yet.
    async fn expected_managed_aggregate_digest(
        &self,
        repo_id: i64,
        artifact_path: &str,
        format_name: &str,
    ) -> Result<String, Box<UploadProblem>> {
        match self.store.get_artifact(repo_id, artifact_path).await {
            Ok(artifact) => Ok(artifact.blob_sha256),
            Err(meta::Error::NotFound) => Ok(String::new()),
            Err(_) => Err(upload_problem(
                503,
                "storage_unavailable",
                &format!("{format_name} metadata unavailable"),
                "Existing derived metadata could not be read",
            )),
        }
    }
}

/// How `@latest` ranks a version: a release outranks a pre-release, which
/// outranks a pseudo-version.
fn go_latest_rank(version: &str) -> u8 {
    if is_pseudo_version(version) {
        0
    } else if !go_semver_prerelease(version).is_empty() {
        1
    } else {
        2
    }
}

/// Trims trailing newlines and re-terminates with exactly one, so a `go.mod`
/// compares equal however it was line-ended.
fn normalize_go_mod(value: &[u8]) -> Vec<u8> {
    let end = value
        .iter()
        .rposition(|b| *b != b'\n' && *b != b'\r')
        .map_or(0, |i| i + 1);
    let mut out = value[..end].to_vec();
    out.push(b'\n');
    out
}

fn unique_go_versions(values: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = values
        .into_iter()
        .filter(|v| seen.insert(v.clone()))
        .collect();
    out.sort_by(|a, b| go_semver_compare(a, b));
    out
}

/// Why a module ZIP was rejected.
#[cfg_attr(test, derive(Debug))]
enum GoZipError {
    /// The archive violates `modzip.CheckZip`.
    ZipInvalid,
    /// The archive is unreadable or carries more entries than configured.
    EntryLimit,
    /// The root `go.mod` is duplicated, oversized or unreadable.
    ModInvalid,
}

/// The limits `golang.org/x/mod/zip` enforces on a module archive.
const MAX_ZIP_FILE: i64 = 500 << 20;
const MAX_GO_MOD: i64 = 16 << 20;
const MAX_LICENSE: i64 = 16 << 20;

/// Runs `modzip.CheckZip` over the staged archive and reads its root `go.mod`.
/// Blocking: the caller runs it on the blocking pool.
fn read_go_module_zip(
    file: Box<dyn crate::storage::ReadSeekCloser>,
    size: i64,
    module_path: &str,
    version: &str,
    max_entries: i64,
    max_meta_bytes: i64,
) -> Result<Vec<u8>, GoZipError> {
    use std::io::Read as _;

    let mut archive = zip::ZipArchive::new(file).map_err(|_| GoZipError::ZipInvalid)?;
    check_module_zip(&mut archive, size, module_path, version)?;
    if archive.len() as i64 > max_entries {
        return Err(GoZipError::EntryLimit);
    }
    let root = format!("{module_path}@{version}/go.mod");
    let mut go_mod: Option<Vec<u8>> = None;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|_| GoZipError::EntryLimit)?;
        if entry.name() != root {
            continue;
        }
        if go_mod.is_some() || entry.size() > max_meta_bytes as u64 {
            return Err(GoZipError::ModInvalid);
        }
        let mut value = Vec::new();
        entry
            .by_ref()
            .take(max_meta_bytes as u64 + 1)
            .read_to_end(&mut value)
            .map_err(|_| GoZipError::ModInvalid)?;
        if value.len() as i64 > max_meta_bytes {
            return Err(GoZipError::ModInvalid);
        }
        go_mod = Some(value);
    }
    Ok(go_mod.unwrap_or_default())
}

/// `golang.org/x/mod/zip.CheckZip`: every entry lives under `path@version/`,
/// has a clean relative name, collides with nothing case-insensitively, and the
/// archive stays inside the size limits.
///
fn check_module_zip<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    size: i64,
    module_path: &str,
    version: &str,
) -> Result<(), GoZipError> {
    // `module.CanonicalVersion`: the canonical form, with `+incompatible` kept.
    let mut canonical = go_semver_canonical(version);
    if go_semver_build(version) == "+incompatible" {
        canonical.push_str("+incompatible");
    }
    if canonical != version {
        return Err(GoZipError::ZipInvalid);
    }
    if check_path(module_path).is_err() || !go_semver_valid(version) {
        return Err(GoZipError::ZipInvalid);
    }
    let (_, path_major, _) = split_path_version(module_path);
    if check_path_major(version, path_major).is_err() {
        return Err(GoZipError::ZipInvalid);
    }
    if size > MAX_ZIP_FILE {
        return Err(GoZipError::ZipInvalid);
    }
    let prefix = format!("{module_path}@{version}/");
    let mut collisions: HashMap<String, (String, bool)> = HashMap::new();
    let mut total: i64 = 0;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|_| GoZipError::ZipInvalid)?;
        let full = entry.name().to_string();
        let Some(name) = full.strip_prefix(&prefix) else {
            return Err(GoZipError::ZipInvalid);
        };
        if name.is_empty() {
            continue;
        }
        let is_dir = name.ends_with('/');
        let name = name.strip_suffix('/').unwrap_or(name);
        if go_path_clean(name) != name || check_file_path(name).is_err() {
            return Err(GoZipError::ZipInvalid);
        }
        check_collision(&mut collisions, name, is_dir)?;
        if is_dir {
            continue;
        }
        let base = go_path_base(name);
        if base.eq_ignore_ascii_case("go.mod") && (base != name || name != "go.mod") {
            return Err(GoZipError::ZipInvalid);
        }
        let entry_size = entry.size() as i64;
        if MAX_ZIP_FILE - total >= entry_size {
            total += entry_size;
        } else {
            return Err(GoZipError::ZipInvalid);
        }
        if (name == "go.mod" && entry_size > MAX_GO_MOD)
            || (name == "LICENSE" && entry_size > MAX_LICENSE)
        {
            return Err(GoZipError::ZipInvalid);
        }
    }
    Ok(())
}

/// `zip.collisionChecker.check`: no two entries may fold to the same name, and
/// a name is either a file or a directory throughout the archive.
fn check_collision(
    seen: &mut HashMap<String, (String, bool)>,
    path: &str,
    is_dir: bool,
) -> Result<(), GoZipError> {
    let fold = str_to_fold(path);
    match seen.get(&fold) {
        Some((other, other_is_dir)) => {
            if other != path || is_dir != *other_is_dir || !is_dir {
                return Err(GoZipError::ZipInvalid);
            }
        }
        None => {
            seen.insert(fold, (path.to_string(), is_dir));
        }
    }
    let parent = go_path_dir(path);
    if parent != "." {
        return check_collision(seen, &parent, true);
    }
    Ok(())
}

/// `char::to_lowercase` stands in for `unicode.SimpleFold`'s minimum-of-the-orbit walk; the two
/// agree for every case pair that reaches a module archive.
fn str_to_fold(s: &str) -> String {
    if s.is_ascii() && !s.bytes().any(|b| b.is_ascii_uppercase()) {
        return s.to_string();
    }
    s.to_lowercase()
}

fn go_path_clean(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let rooted = path.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    let mut dotdot = 0;
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if out.len() > dotdot {
                    out.pop();
                } else if !rooted {
                    out.push("..");
                    dotdot += 1;
                }
            }
            other => out.push(other),
        }
    }
    let joined = out.join("/");
    if rooted {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

pub(crate) fn go_path_base(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        Some(i) => trimmed[i + 1..].to_string(),
        None => trimmed.to_string(),
    }
}

fn go_path_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => go_path_clean(&path[..i + 1]),
        None => go_path_clean(""),
    }
}

/// What [`check_path_element`] is validating, mirroring `module.pathKind`.
#[derive(Clone, Copy, PartialEq)]
enum PathKind {
    Module,
    File,
}

/// The Windows device names no path element may be built on.
const BAD_WINDOWS_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// `module.CheckPath`: a module path is a valid path whose first element is a
/// lowercase domain-shaped name carrying a dot, with a well-formed major-version
/// suffix.
fn check_path(path: &str) -> Result<(), ()> {
    check_path_elements(path, PathKind::Module)?;
    let first = path.find('/').unwrap_or(path.len());
    if first == 0 {
        return Err(());
    }
    let head = &path[..first];
    if !head.contains('.') || path.starts_with('-') || !head.chars().all(first_path_ok) {
        return Err(());
    }
    let (_, _, ok) = split_path_version(path);
    if !ok { Err(()) } else { Ok(()) }
}

/// `module.CheckFilePath`: the same element rules, relaxed for file names.
fn check_file_path(path: &str) -> Result<(), ()> {
    check_path_elements(path, PathKind::File)
}

/// `module.checkPath`: no empty, doubled or trailing separators, and every
/// element valid for `kind`.
fn check_path_elements(path: &str, kind: PathKind) -> Result<(), ()> {
    if path.is_empty()
        || (path.starts_with('-') && kind != PathKind::File)
        || path.contains("//")
        || path.ends_with('/')
    {
        return Err(());
    }
    for element in path.split('/') {
        check_path_element(element, kind)?;
    }
    Ok(())
}

/// `module.checkElem`.
fn check_path_element(elem: &str, kind: PathKind) -> Result<(), ()> {
    if elem.is_empty()
        || elem.chars().all(|c| c == '.')
        || (elem.starts_with('.') && kind == PathKind::Module)
        || elem.ends_with('.')
    {
        return Err(());
    }
    let element_ok = |c: char| match kind {
        PathKind::Module => mod_path_ok(c),
        PathKind::File => file_name_ok(c),
    };
    if !elem.chars().all(element_ok) {
        return Err(());
    }
    // Windows disallows a set of device names, with or without an extension.
    let short = match elem.find('.') {
        Some(i) => &elem[..i],
        None => elem,
    };
    if BAD_WINDOWS_NAMES
        .iter()
        .any(|bad| bad.eq_ignore_ascii_case(short))
    {
        return Err(());
    }
    if kind == PathKind::File {
        // Windows short-names only matter for import paths.
        return Ok(());
    }
    // Reject elements shaped like a Windows short-name: a tilde followed by
    // one or more ASCII digits.
    if let Some(tilde) = short.rfind('~')
        && tilde < short.len() - 1
    {
        let suffix = &short[tilde + 1..];
        if suffix.bytes().all(|b| b.is_ascii_digit()) {
            return Err(());
        }
    }
    Ok(())
}

/// `module.firstPathOK`: the restricted set the leading (domain) element allows.
fn first_path_ok(r: char) -> bool {
    r == '-' || r == '.' || r.is_ascii_digit() || r.is_ascii_lowercase()
}

/// `module.modPathOK`.
fn mod_path_ok(r: char) -> bool {
    r == '-' || r == '.' || r == '_' || r == '~' || r.is_ascii_alphanumeric()
}

/// `module.fileNameOK`: ASCII alphanumerics, a restricted punctuation set (no
/// shell metacharacters or path separators), spaces, and any Unicode letter.
fn file_name_ok(r: char) -> bool {
    if r.is_ascii() {
        return r.is_ascii_alphanumeric() || "!#$%&()+,-.=@[]^_{}~ ".contains(r);
    }
    r.is_alphabetic()
}

/// `module.SplitPathVersion`: splits a module path into its prefix and its
/// major-version suffix (`/v2`, `.v2` for gopkg.in), reporting whether the
/// suffix is well formed.
fn split_path_version(path: &str) -> (&str, &str, bool) {
    if path.starts_with("gopkg.in/") {
        return split_gopkg_in(path);
    }
    let bytes = path.as_bytes();
    let mut i = bytes.len();
    let mut dot = false;
    while i > 0 && (bytes[i - 1].is_ascii_digit() || bytes[i - 1] == b'.') {
        if bytes[i - 1] == b'.' {
            dot = true;
        }
        i -= 1;
    }
    if i <= 1 || i == bytes.len() || bytes[i - 1] != b'v' || bytes[i - 2] != b'/' {
        return (path, "", true);
    }
    let (prefix, path_major) = (&path[..i - 2], &path[i - 2..]);
    if dot || path_major.len() <= 2 || path_major.as_bytes()[2] == b'0' || path_major == "/v1" {
        return (path, "", false);
    }
    (prefix, path_major, true)
}

/// `module.splitGopkgIn`: every gopkg.in path ends in `.vN`.
fn split_gopkg_in(path: &str) -> (&str, &str, bool) {
    if !path.starts_with("gopkg.in/") {
        return (path, "", false);
    }
    let bytes = path.as_bytes();
    let mut i = bytes.len();
    if path.ends_with("-unstable") {
        i -= "-unstable".len();
    }
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i <= 1 || bytes[i - 1] != b'v' || bytes[i - 2] != b'.' {
        return (path, "", false);
    }
    let (prefix, path_major) = (&path[..i - 2], &path[i - 2..]);
    if path_major.len() <= 2 || (path_major.as_bytes()[2] == b'0' && path_major != ".v0") {
        return (path, "", false);
    }
    (prefix, path_major, true)
}

/// `module.CheckPathMajor`: the version's major must match the path suffix.
fn check_path_major(v: &str, path_major: &str) -> Result<(), ()> {
    let mut path_major = path_major;
    if path_major.starts_with(".v") && path_major.ends_with("-unstable") {
        path_major = path_major.strip_suffix("-unstable").unwrap_or(path_major);
    }
    if v.starts_with("v0.0.0-") && path_major == ".v1" {
        // An old pseudo-version bug produced v0.0.0- for gopkg.in .v1 modules.
        return Ok(());
    }
    let major = go_semver_major(v);
    if path_major.is_empty() {
        if major == "v0" || major == "v1" || go_semver_build(v) == "+incompatible" {
            return Ok(());
        }
    } else if (path_major.starts_with('/') || path_major.starts_with('.'))
        && major == path_major[1..]
    {
        return Ok(());
    }
    Err(())
}

/// `module.EscapePath`: upper-case letters become `!` plus their lower-case
/// form, so a case-insensitive filesystem cannot merge two module paths.
fn escape_path(path: &str) -> Option<String> {
    check_path(path).ok()?;
    escape_string(path)
}

/// `module.EscapeVersion`.
fn escape_version(v: &str) -> Option<String> {
    if check_path_element(v, PathKind::File).is_err() || v.contains('!') {
        return None;
    }
    escape_string(v)
}

fn escape_string(s: &str) -> Option<String> {
    if s.chars().any(|c| c == '!' || !c.is_ascii()) {
        return None;
    }
    if !s.chars().any(|c| c.is_ascii_uppercase()) {
        return Some(s.to_string());
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            out.push('!');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// `module.UnescapeVersion`: reverses the `!x` case escaping.
fn unescape_version(escaped: &str) -> Option<String> {
    let v = unescape_string(escaped)?;
    check_path_element(&v, PathKind::File).ok()?;
    Some(v)
}

fn unescape_string(escaped: &str) -> Option<String> {
    let mut out = String::with_capacity(escaped.len());
    let mut bang = false;
    for c in escaped.chars() {
        if !c.is_ascii() {
            return None;
        }
        if bang {
            bang = false;
            if !c.is_ascii_lowercase() {
                return None;
            }
            out.push(c.to_ascii_uppercase());
            continue;
        }
        if c == '!' {
            bang = true;
            continue;
        }
        if c.is_ascii_uppercase() {
            return None;
        }
        out.push(c);
    }
    if bang { None } else { Some(out) }
}

/// `module.IsPseudoVersion`: a valid semver shaped
/// `vX.0.0-yyyymmddhhmmss-abcdef` (or the pre-release variants).
fn is_pseudo_version(v: &str) -> bool {
    v.matches('-').count() >= 2 && go_semver_valid(v) && matches_pseudo_shape(v)
}

/// Matches `^v[0-9]+\.(0\.0-|\d+\.\d+-([^+]*\.)?0\.)\d{14}-[A-Za-z0-9]+(\+build)?$`,
/// the regexp `module.pseudoVersionRE` compiles.
fn matches_pseudo_shape(v: &str) -> bool {
    let Some(rest) = v.strip_prefix('v') else {
        return false;
    };
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return false;
    }
    let Some(rest) = rest[digits..].strip_prefix('.') else {
        return false;
    };
    // Everything after the `+` is build metadata the pattern only shape-checks;
    // `go_semver_valid` has already validated it.
    let (rest, build) = match rest.split_once('+') {
        Some((head, build)) => (head, Some(build)),
        None => (rest, None),
    };
    if let Some(build) = build
        && (build.is_empty()
            || build
                .split('.')
                .any(|part| part.is_empty() || !part.bytes().all(is_build_char)))
    {
        return false;
    }
    // `0\.0-` or `\d+\.\d+-([^+]*\.)?0\.`, then the timestamp and revision. The
    // first alternative is tried first but, like the regexp, failing inside it
    // falls through to the second rather than rejecting the version.
    if let Some(tail) = rest.strip_prefix("0.0-")
        && pseudo_stamp_ok(tail)
    {
        return true;
    }
    let Some((minor, rest)) = rest.split_once('.') else {
        return false;
    };
    if minor.is_empty() || !minor.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Some((patch, rest)) = rest.split_once('-') else {
        return false;
    };
    if patch.is_empty() || !patch.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    // `([^+]*\.)?` matches any prefix ending in a dot (or nothing at all), so
    // every `0.` that starts an identifier is a candidate the regexp would try.
    rest.match_indices("0.")
        .any(|(i, _)| (i == 0 || rest.as_bytes()[i - 1] == b'.') && pseudo_stamp_ok(&rest[i + 2..]))
}

/// The `\d{14}-[A-Za-z0-9]+` tail every pseudo-version ends with.
fn pseudo_stamp_ok(tail: &str) -> bool {
    let Some((timestamp, revision)) = tail.split_once('-') else {
        return false;
    };
    timestamp.len() == 14
        && timestamp.bytes().all(|b| b.is_ascii_digit())
        && !revision.is_empty()
        && revision.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn is_build_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-'
}

/// `module.PseudoVersionTime`: the `yyyymmddhhmmss` stamp a pseudo-version
/// carries, in UTC.
fn pseudo_version_time(v: &str) -> Option<DateTime<Utc>> {
    let timestamp = parse_pseudo_version_timestamp(v)?;
    NaiveDateTime::parse_from_str(&timestamp, "%Y%m%d%H%M%S")
        .ok()
        .map(|t| t.and_utc())
}

/// `module.parsePseudoVersion`, reduced to the timestamp field this publisher
/// needs.
fn parse_pseudo_version_timestamp(v: &str) -> Option<String> {
    if !is_pseudo_version(v) {
        return None;
    }
    let build = go_semver_build(v);
    let v = &v[..v.len() - build.len()];
    let j = v.rfind('-')?;
    let v = &v[..j];
    let i = v.rfind('-')?;
    match v.rfind('.') {
        Some(dot) if dot > i => Some(v[dot + 1..].to_string()),
        _ => Some(v[i + 1..].to_string()),
    }
}

/// `modfile.ModulePath`: the path from the first `module` directive, or `""`
/// when the file declares none.
fn modfile_module_path(mod_file: &[u8]) -> String {
    for line in mod_file.split(|b| *b == b'\n') {
        let line = match line.windows(2).position(|w| w == b"//") {
            Some(i) => &line[..i],
            None => line,
        };
        let line = line.trim_ascii();
        let Some(rest) = line.strip_prefix(b"module".as_slice()) else {
            continue;
        };
        let trimmed = rest.trim_ascii();
        // The directive needs whitespace after `module`, then a path.
        if trimmed.len() == rest.len() || trimmed.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(trimmed) else {
            return String::new();
        };
        if text.starts_with('"') || text.starts_with('`') {
            return go_unquote(text).unwrap_or_default();
        }
        return text.to_string();
    }
    String::new()
}

/// `strconv.Unquote` for the two literal forms a `module` directive may use.
fn go_unquote(text: &str) -> Option<String> {
    if let Some(inner) = text.strip_prefix('`') {
        let inner = inner.strip_suffix('`')?;
        if inner.contains('`') {
            return None;
        }
        // Carriage returns are discarded from a raw string literal.
        return Some(inner.replace('\r', ""));
    }
    let inner = text.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\n' | '"' => return None,
            '\\' => {
                let escape = chars.next()?;
                match escape {
                    'a' => out.push('\u{7}'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'v' => out.push('\u{b}'),
                    '\\' => out.push('\\'),
                    '\'' => out.push('\''),
                    '"' => out.push('"'),
                    'x' => out.push(hex_escape(&mut chars, 2)? as u8 as char),
                    'u' => out.push(char::from_u32(hex_escape(&mut chars, 4)?)?),
                    'U' => out.push(char::from_u32(hex_escape(&mut chars, 8)?)?),
                    '0'..='7' => {
                        let mut value = escape as u32 - '0' as u32;
                        for _ in 0..2 {
                            let digit = chars.next()?.to_digit(8)?;
                            value = value * 8 + digit;
                        }
                        if value > 255 {
                            return None;
                        }
                        out.push(value as u8 as char);
                    }
                    _ => return None,
                }
            }
            other => out.push(other),
        }
    }
    Some(out)
}

fn hex_escape(chars: &mut std::str::Chars<'_>, width: usize) -> Option<u32> {
    let mut value = 0u32;
    for _ in 0..width {
        value = value * 16 + chars.next()?.to_digit(16)?;
    }
    Some(value)
}

/// Module protocol conformance cases.
#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    #[test]
    fn module_check_path() {
        for path in [
            "example.com/acme/widget",
            "example.com/acme/widget/v2",
            "gopkg.in/yaml.v2",
            "example.com/a_b/c~d",
        ] {
            assert!(check_path(path).is_ok(), "expected {path} to be valid");
        }
        for path in [
            "",
            "example",              // no dot in the first element
            "-example.com/a",       // leading dash
            "Example.com/a",        // upper case in the first element
            "example.com//a",       // double slash
            "example.com/a/",       // trailing slash
            "example.com/.a",       // leading dot in an element
            "example.com/a.",       // trailing dot in an element
            "example.com/CON",      // Windows device name
            "example.com/con.go",   // Windows device name with an extension
            "example.com/widget~1", // Windows short-name shape
            "example.com/a/v1",     // /v1 is never a path suffix
            "example.com/a/v02",    // leading zero in the major suffix
        ] {
            assert!(check_path(path).is_err(), "expected {path} to be invalid");
        }
    }

    #[test]
    fn module_split_path_version() {
        assert_eq!(
            split_path_version("example.com/m/v2"),
            ("example.com/m", "/v2", true)
        );
        assert_eq!(
            split_path_version("example.com/m"),
            ("example.com/m", "", true)
        );
        assert_eq!(
            split_path_version("example.com/m/v1"),
            ("example.com/m/v1", "", false)
        );
        assert_eq!(
            split_path_version("gopkg.in/yaml.v2"),
            ("gopkg.in/yaml", ".v2", true)
        );
        assert_eq!(
            split_path_version("gopkg.in/check.v1-unstable"),
            ("gopkg.in/check", ".v1-unstable", true)
        );
    }

    #[test]
    fn module_check_path_major() {
        for (version, path_major) in [
            ("v2.0.0", "/v2"),
            ("v1.0.0", ""),
            ("v0.5.0", ""),
            ("v2.0.0+incompatible", ""),
            ("v2.0.0", ".v2"),
            // The historical gopkg.in .v1 pseudo-version bug stays allowed.
            ("v0.0.0-20161208181325-20d25e280405", ".v1"),
        ] {
            assert!(
                check_path_major(version, path_major).is_ok(),
                "expected {version} to fit {path_major}"
            );
        }
        for (version, path_major) in [("v1.0.0", "/v2"), ("v2.0.0", ""), ("v3.0.0", "/v2")] {
            assert!(
                check_path_major(version, path_major).is_err(),
                "expected {version} to clash with {path_major}"
            );
        }
    }

    #[test]
    fn module_escaping_round_trips() {
        assert_eq!(
            escape_path("github.com/BurntSushi/toml").as_deref(),
            Some("github.com/!burnt!sushi/toml")
        );
        assert_eq!(
            escape_path("example.com/acme/widget").as_deref(),
            Some("example.com/acme/widget")
        );
        assert_eq!(
            escape_version("v1.0.0-Beta").as_deref(),
            Some("v1.0.0-!beta")
        );
        assert_eq!(
            unescape_version("v1.0.0-!beta").as_deref(),
            Some("v1.0.0-Beta")
        );
        assert_eq!(unescape_version("v1.0.0").as_deref(), Some("v1.0.0"));
        // An unescaped upper-case letter, a dangling bang and a bang followed by
        // a non-letter are all rejected.
        assert_eq!(unescape_version("v1.0.0-Beta"), None);
        assert_eq!(unescape_version("v1.0.0-beta!"), None);
        assert_eq!(unescape_version("v1.0.0-!1"), None);
        // A version already carrying a bang cannot be escaped.
        assert_eq!(escape_version("v1.0.0-!beta"), None);
    }

    #[test]
    fn module_pseudo_versions() {
        for version in [
            "v0.0.0-20240102030405-abcdef123456",
            "v1.2.4-0.20240102030405-abcdef123456",
            "v1.2.4-pre.0.20240102030405-abcdef123456",
            "v0.0.0-20240102030405-abcdef123456+incompatible",
        ] {
            assert!(
                is_pseudo_version(version),
                "expected {version} to be a pseudo-version"
            );
        }
        for version in [
            "v1.2.3",
            "v0.0.0-20240102030405",                    // no revision
            "v0.0.0-2024010203040-abcdef1234",          // 13-digit stamp
            "v1.2.4-pre.1.20240102030405-abcdef123456", // the counter must be 0
        ] {
            assert!(
                !is_pseudo_version(version),
                "expected {version} not to be a pseudo-version"
            );
        }
        assert_eq!(
            pseudo_version_time("v0.0.0-20240102030405-abcdef123456"),
            Some(Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap())
        );
        assert_eq!(
            pseudo_version_time("v1.2.4-pre.0.20240102030405-abcdef123456"),
            Some(Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap())
        );
        // A syntactically valid stamp that is not a real instant.
        assert_eq!(
            pseudo_version_time("v0.0.0-20241302030405-abcdef123456"),
            None
        );
    }

    #[test]
    fn semver_canonical_and_ordering() {
        assert_eq!(go_semver_canonical("v1"), "v1.0.0");
        assert_eq!(go_semver_canonical("v1.2"), "v1.2.0");
        assert_eq!(go_semver_canonical("v1.2.3"), "v1.2.3");
        assert_eq!(go_semver_canonical("v1.2.3+meta"), "v1.2.3");
        assert_eq!(go_semver_canonical("v1.2.3-rc.1"), "v1.2.3-rc.1");
        for invalid in ["1.2.3", "v01.2.3", "v1.2.3-", "v1.2.3.4", ""] {
            assert_eq!(go_semver_canonical(invalid), "", "{invalid}");
        }
        assert_eq!(
            unique_go_versions(vec![
                "v1.10.0".to_string(),
                "v1.2.0".to_string(),
                "v1.10.0".to_string(),
                "v1.2.0-rc.1".to_string(),
            ]),
            vec!["v1.2.0-rc.1", "v1.2.0", "v1.10.0"]
        );
    }

    #[test]
    fn latest_ranks_release_over_prerelease_over_pseudo() {
        assert_eq!(go_latest_rank("v1.2.3"), 2);
        assert_eq!(go_latest_rank("v1.2.3-rc.1"), 1);
        assert_eq!(go_latest_rank("v0.0.0-20240102030405-abcdef123456"), 0);
    }

    #[test]
    fn modfile_reads_the_module_directive() {
        for (source, want) in [
            ("module example.com/m\n\ngo 1.23\n", "example.com/m"),
            (
                "// header\nmodule example.com/m // trailing\n",
                "example.com/m",
            ),
            ("module \"example.com/m\"\n", "example.com/m"),
            ("module `example.com/m`\n", "example.com/m"),
            ("  module   example.com/m  \n", "example.com/m"),
            ("go 1.23\n", ""),
            ("modulex example.com/m\n", ""),
            ("module\n", ""),
        ] {
            assert_eq!(modfile_module_path(source.as_bytes()), want, "{source:?}");
        }
    }

    #[test]
    fn go_mod_normalisation_and_paths() {
        assert_eq!(normalize_go_mod(b"module m\r\n\r\n"), b"module m\n");
        assert_eq!(normalize_go_mod(b"module m"), b"module m\n");
        assert_eq!(go_path_clean("a/./b"), "a/b");
        assert_eq!(go_path_clean("a//b"), "a/b");
        assert_eq!(go_path_clean("a/b/"), "a/b");
        assert_eq!(go_path_clean("a/../b"), "b");
        assert_eq!(go_path_clean("../a"), "../a");
        assert_eq!(go_path_clean(""), ".");
        assert_eq!(go_path_base("a/b/c.info"), "c.info");
        assert_eq!(go_path_dir("a/b/c"), "a/b");
        assert_eq!(go_path_dir("a"), ".");
    }

    /// Builds a ZIP in a temporary file and hands back the seekable view the
    /// publisher would get from the blob store.
    fn staged_zip(entries: &[(&str, &str)]) -> (Box<dyn crate::storage::ReadSeekCloser>, i64) {
        use std::io::Write as _;
        let dir = tempfile::tempdir().expect("temp dir");
        // The directory is leaked so the file outlives the handle; the test
        // process is short-lived and the OS reclaims it.
        let path = dir.keep().join("module.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&path).expect("create"));
        let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, value) in entries {
            writer.start_file(*name, options).expect("entry");
            writer.write_all(value.as_bytes()).expect("write");
        }
        writer.finish().expect("finish");
        let size = std::fs::metadata(&path).expect("stat").len() as i64;
        let file = std::fs::File::open(&path).expect("open");
        (Box::new(crate::storage::StagedFile::new(file, path)), size)
    }

    #[test]
    fn module_zip_check_accepts_a_well_formed_archive() {
        let (file, size) = staged_zip(&[
            ("example.com/m@v1.0.0/go.mod", "module example.com/m\n"),
            ("example.com/m@v1.0.0/widget.go", "package widget\n"),
        ]);
        let go_mod = read_go_module_zip(file, size, "example.com/m", "v1.0.0", 100, 1 << 20)
            .expect("valid archive");
        assert_eq!(go_mod, b"module example.com/m\n");
    }

    #[test]
    fn module_zip_check_rejects_bad_archives() {
        // An entry outside the `path@version/` prefix.
        let (file, size) = staged_zip(&[("other/widget.go", "package widget\n")]);
        assert!(matches!(
            read_go_module_zip(file, size, "example.com/m", "v1.0.0", 100, 1 << 20),
            Err(GoZipError::ZipInvalid)
        ));
        // A nested go.mod, which the module root alone may carry.
        let (file, size) = staged_zip(&[("example.com/m@v1.0.0/sub/go.mod", "module x\n")]);
        assert!(matches!(
            read_go_module_zip(file, size, "example.com/m", "v1.0.0", 100, 1 << 20),
            Err(GoZipError::ZipInvalid)
        ));
        // A case-insensitive collision between two entries.
        let (file, size) = staged_zip(&[
            ("example.com/m@v1.0.0/Widget.go", "package widget\n"),
            ("example.com/m@v1.0.0/widget.go", "package widget\n"),
        ]);
        assert!(matches!(
            read_go_module_zip(file, size, "example.com/m", "v1.0.0", 100, 1 << 20),
            Err(GoZipError::ZipInvalid)
        ));
        // More entries than the configured archive limit.
        let (file, size) = staged_zip(&[
            ("example.com/m@v1.0.0/go.mod", "module example.com/m\n"),
            ("example.com/m@v1.0.0/widget.go", "package widget\n"),
        ]);
        assert!(matches!(
            read_go_module_zip(file, size, "example.com/m", "v1.0.0", 1, 1 << 20),
            Err(GoZipError::EntryLimit)
        ));
        // A root go.mod above the metadata limit.
        let (file, size) = staged_zip(&[("example.com/m@v1.0.0/go.mod", "module example.com/m\n")]);
        assert!(matches!(
            read_go_module_zip(file, size, "example.com/m", "v1.0.0", 100, 4),
            Err(GoZipError::ModInvalid)
        ));
    }
}
