//! The npm publisher: tarball inspection, packument planning and the atomic
//! commit, plus the npm-native publish adapter.

use std::sync::Arc;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::meta::{
    self, Artifact, ArtifactBatch, ArtifactCAS, ArtifactPublication, ArtifactUploadRequest,
    Repository, UPLOAD_RECEIVING, UploadRequestKey,
};

use super::uiupload::{
    ArtifactUploadManifest, ArtifactUploadResult, PublishResult, StagedUploadAsset,
    UploadedArtifact, Uploader, upload_problem,
};
use super::uiupload_maven::uploaded_result;

const NPM_PACKAGE_JSON_PATH: &str = "package/package.json";

static NPM_IDENTIFIER: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[a-z0-9][a-z0-9._~-]*$").expect("valid regex"));

/// The identity and manifest read out of an uploaded `.tgz`.
#[derive(Debug, Clone, Default)]
struct NpmPackageData {
    name: String,
    version: String,
    manifest: serde_json::Map<String, serde_json::Value>,
}

impl Uploader {
    pub(crate) async fn publish_npm(
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
                "Invalid npm manifest",
                "Exactly the npm format object is required",
            )
        };
        let Some(npm) = &manifest.npm else {
            return Err(format_fields());
        };
        if manifest.maven.is_some()
            || manifest.pypi.is_some()
            || manifest.cargo.is_some()
            || manifest.go_.is_some()
        {
            return Err(format_fields());
        }
        if manifest.overwrite {
            return Err(upload_problem(
                422,
                "redeploy_not_supported",
                "npm versions are immutable",
                "An npm version cannot be replaced",
            ));
        }
        if staged.len() != 1 || manifest.assets.len() != 1 {
            return Err(upload_problem(
                422,
                "npm_asset_count",
                "One npm tarball required",
                "Upload exactly one .tgz package",
            ));
        }
        let asset = &staged[0];
        if !asset.filename.to_lowercase().ends_with(".tgz") {
            return Err(upload_problem(
                415,
                "npm_tarball_required",
                "npm tarball required",
                "The uploaded file must use the .tgz extension",
            ));
        }
        let pkg = self.inspect_npm_package(asset).await?;
        let mut tag = npm.dist_tag.trim().to_string();
        if tag.is_empty() {
            tag = "latest".to_string();
        }
        if !valid_npm_dist_tag(&tag) {
            return Err(upload_problem(
                422,
                "npm_dist_tag_invalid",
                "Invalid npm dist-tag",
                "The dist-tag must be a non-semver URL-safe label",
            ));
        }

        let basename = super::path_base(&pkg.name).to_string();
        let tarball_path = format!("{}/-/{basename}-{}.tgz", pkg.name, pkg.version);
        let Ok(tombstoned) = self
            .store
            .has_publication_tombstone(
                repository.id,
                meta::FORMAT_NPM,
                &pkg.name,
                &pkg.version,
                "*",
            )
            .await
        else {
            return Err(upload_problem(
                503,
                "storage_unavailable",
                "Upload unavailable",
                "The npm tombstone could not be checked",
            ));
        };
        if tombstoned {
            return Err(upload_problem(
                409,
                "immutable_version_exists",
                "npm version cannot be reused",
                "This npm package version was previously unpublished",
            ));
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
        let publication = ArtifactPublication {
            id: publication_id,
            repo_id: repository.id,
            format: meta::FORMAT_NPM.to_string(),
            package_name: pkg.name.clone(),
            version: pkg.version.clone(),
            coordinate: format!("{}@{}", pkg.name, pkg.version),
            upload_id: request.upload_id.clone(),
            created_by: request.principal_name.clone(),
            created_by_source: request.principal_source.clone(),
            created_at: publication_time,
            updated_at: publication_time,
            ..Default::default()
        };

        let (packument, expected_packument) = self
            .plan_npm_packument(repository.id, &pkg, &tag, asset, publication_time)
            .await?;
        let encode_failed = || {
            upload_problem(
                503,
                "result_encoding_failed",
                "Upload unavailable",
                "The npm packument could not be encoded",
            )
        };
        let packument_bytes = serde_json::to_vec(&packument).map_err(|_| encode_failed())?;
        let stage_failed = || {
            upload_problem(
                503,
                "storage_unavailable",
                "Upload storage failed",
                "The npm packument could not be staged",
            )
        };
        let packument_asset = self
            .stage_generated(&pkg.name, "index", "application/json", &packument_bytes)
            .await
            .map_err(|_| stage_failed())?;

        let owned_metadata = json_object(&[
            ("format", serde_json::json!("npm")),
            (
                "format_metadata",
                serde_json::json!({ "dist_tag": tag.clone() }),
            ),
            ("managed_by", serde_json::json!("ui_upload")),
            ("package", serde_json::json!(pkg.name)),
            ("role", serde_json::json!("primary")),
            ("schema_version", serde_json::json!(1)),
            ("source_filename", serde_json::json!(asset.filename)),
            ("upload_id", serde_json::json!(request.upload_id)),
            ("version_identity", serde_json::json!(pkg.version)),
        ]);
        let aggregate_metadata = json_object(&[
            ("aggregate_schema_version", serde_json::json!(1)),
            ("format", serde_json::json!("npm")),
            ("managed_by", serde_json::json!("ui_upload_aggregate")),
            ("package", serde_json::json!(pkg.name)),
            ("schema_version", serde_json::json!(1)),
        ]);
        let primary = Artifact {
            repo_id: repository.id,
            path: tarball_path.clone(),
            version: pkg.version.clone(),
            blob_sha256: asset.digest.clone(),
            size: asset.size,
            content_type: "application/octet-stream".to_string(),
            metadata_json: owned_metadata,
            published_at: Some(publication_time),
            cached_by: request.principal_name.clone(),
            publication_id: publication.id.clone(),
            artifact_role: "primary".to_string(),
            ..Default::default()
        };
        let index = ArtifactCAS {
            artifact: Artifact {
                repo_id: repository.id,
                path: pkg.name.clone(),
                blob_sha256: packument_asset.digest.clone(),
                size: packument_asset.size,
                content_type: "application/json".to_string(),
                metadata_json: aggregate_metadata,
                cached_by: request.principal_name.clone(),
                artifact_role: "index".to_string(),
                ..Default::default()
            },
            expected_sha256: expected_packument,
        };
        let mut result = ArtifactUploadResult {
            upload_id: request.upload_id.clone(),
            repository: repository.name.clone(),
            format: meta::FORMAT_NPM.to_string(),
            coordinate: publication.coordinate.clone(),
            created: vec![UploadedArtifact {
                path: tarball_path,
                role: "primary".to_string(),
                size: asset.size,
                sha256: asset.digest.clone(),
            }],
            replaced: Vec::new(),
            derived: vec![uploaded_result(&packument_asset)],
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
            create: vec![primary],
            mutable_cas: vec![index],
            upload_result_json: result_json,
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
            let (replanned, expected) = self
                .plan_npm_packument(repository.id, &pkg, &tag, asset, publication_time)
                .await?;
            let value = serde_json::to_vec(&replanned).map_err(|_| encode_failed())?;
            let staged_index = self
                .stage_generated(&pkg.name, "index", "application/json", &value)
                .await
                .map_err(|_| stage_failed())?;
            batch.mutable_cas[0].artifact.blob_sha256 = staged_index.digest.clone();
            batch.mutable_cas[0].artifact.size = staged_index.size;
            batch.mutable_cas[0].expected_sha256 = expected;
            result.derived = vec![uploaded_result(&staged_index)];
            batch.upload_result_json = serde_json::to_string(&result).unwrap_or_default();
        }
        match commit_err {
            Err(meta::Error::ArtifactConflict) => Err(upload_problem(
                409,
                "immutable_version_exists",
                "npm version already exists",
                "The package and version already exist",
            )),
            Err(meta::Error::DerivedMetadataChanged) => Err(upload_problem(
                409,
                "concurrent_metadata_update",
                "npm package changed concurrently",
                "Retry with a new idempotency key",
            )),
            Err(_) => Err(upload_problem(
                503,
                "storage_unavailable",
                "Upload commit failed",
                "The npm publication could not be committed",
            )),
            Ok(()) => {
                self.record_committed(repository, &batch.create);
                self.notify_published(repository, &publication, &request.principal_name)
                    .await;
                Ok(result)
            }
        }
    }

    /// Reads `package/package.json` out of the staged tarball and validates the
    /// package identity it declares.
    async fn inspect_npm_package(
        &self,
        asset: &StagedUploadAsset,
    ) -> Result<NpmPackageData, Box<super::uiupload::UploadProblem>> {
        let (reader, _) = self.engine.blobs.open(&asset.digest).await.map_err(|_| {
            upload_problem(
                503,
                "storage_unavailable",
                "Staged tarball unavailable",
                "The npm tarball could not be reopened",
            )
        })?;
        let max_entries = self.cfg.archive_max_entries;
        let max_field = self.cfg.max_field_bytes;
        // gzip + tar parsing is blocking; the bytes stream in over a bridge.
        let bridged = tokio_util::io::SyncIoBridge::new(reader);
        let package_json = tokio::task::spawn_blocking(move || {
            read_npm_package_json(bridged, max_entries, max_field)
        })
        .await
        .unwrap_or(Err(NpmTarballError::Invalid))
        .map_err(|err| match err {
            NpmTarballError::Invalid => upload_problem(
                422,
                "npm_tarball_invalid",
                "Invalid npm tarball",
                "The package is not a gzip tar archive",
            ),
            NpmTarballError::EntryLimit => upload_problem(
                422,
                "archive_entry_limit",
                "npm tarball has too many entries",
                "The package archive exceeds the entry limit",
            ),
            NpmTarballError::PackageJsonInvalid => upload_problem(
                422,
                "npm_package_json_invalid",
                "Invalid package.json",
                "Exactly one bounded regular package/package.json is required",
            ),
            NpmTarballError::PackageJsonMissing => upload_problem(
                422,
                "npm_package_json_missing",
                "package.json missing",
                "The tarball must contain package/package.json",
            ),
        })?;

        let invalid_json = || {
            upload_problem(
                422,
                "npm_package_json_invalid",
                "Invalid package.json",
                "package/package.json must contain one JSON object",
            )
        };
        let mut de = serde_json::Deserializer::from_slice(&package_json);
        let manifest: serde_json::Map<String, serde_json::Value> =
            serde::Deserialize::deserialize(&mut de).map_err(|_| invalid_json())?;
        de.end().map_err(|_| invalid_json())?;

        let name = manifest.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let version = manifest
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if name.is_empty()
            || version.is_empty()
            || !valid_npm_name(name)
            || !valid_strict_semver(version)
        {
            return Err(upload_problem(
                422,
                "npm_identity_invalid",
                "Invalid npm package identity",
                "package.json must contain a valid lowercase name and canonical SemVer version",
            ));
        }
        if manifest
            .get("private")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return Err(upload_problem(
                422,
                "npm_private_package",
                "Private npm package",
                "package.json declares private=true and cannot be published",
            ));
        }
        Ok(NpmPackageData {
            name: name.to_string(),
            version: version.to_string(),
            manifest: manifest.clone(),
        })
    }

    /// Merges the new version into the package's packument and reports the
    /// digest the commit must find unchanged.
    async fn plan_npm_packument(
        &self,
        repo_id: i64,
        pkg: &NpmPackageData,
        tag: &str,
        asset: &StagedUploadAsset,
        publication_time: chrono::DateTime<chrono::Utc>,
    ) -> Result<(serde_json::Value, String), Box<super::uiupload::UploadProblem>> {
        let now = crate::meta::time::format_time(publication_time);
        let mut packument = serde_json::Map::new();
        packument.insert("_id".to_string(), serde_json::json!(pkg.name));
        packument.insert("name".to_string(), serde_json::json!(pkg.name));
        packument.insert(
            "versions".to_string(),
            serde_json::Value::Object(serde_json::Map::new()),
        );
        packument.insert(
            "dist-tags".to_string(),
            serde_json::Value::Object(serde_json::Map::new()),
        );
        packument.insert(
            "time".to_string(),
            serde_json::json!({ "created": now, "modified": now }),
        );
        let mut expected = String::new();
        match self.store.get_artifact(repo_id, &pkg.name).await {
            Ok(artifact) => {
                expected = artifact.blob_sha256.clone();
                let (reader, _) = self
                    .engine
                    .blobs
                    .open(&artifact.blob_sha256)
                    .await
                    .map_err(|err| {
                        self.engine.note_blob_missing(
                            repo_id,
                            "",
                            &pkg.name,
                            &artifact.blob_sha256,
                            "index",
                            503,
                            &err.to_string(),
                        );
                        upload_problem(
                            503,
                            "storage_unavailable",
                            "npm packument unavailable",
                            "Existing package metadata could not be opened",
                        )
                    })?;
                let decoded = super::npm::decode_json_stream::<
                    serde_json::Map<String, serde_json::Value>,
                    _,
                >(
                    reader, (self.cfg.archive_max_meta_bytes + 1) as u64
                )
                .await;
                let Some(decoded) = decoded else {
                    return Err(upload_problem(
                        409,
                        "derived_metadata_not_managed",
                        "Existing npm metadata is unsupported",
                        "The existing packument is not valid JSON",
                    ));
                };
                packument = decoded;
                if packument.get("name").and_then(|v| v.as_str()).unwrap_or("") != pkg.name {
                    return Err(upload_problem(
                        409,
                        "derived_metadata_not_managed",
                        "Existing npm metadata is inconsistent",
                        "The packument name does not match its repository path",
                    ));
                }
            }
            Err(meta::Error::NotFound) => {}
            Err(_) => {
                return Err(upload_problem(
                    503,
                    "storage_unavailable",
                    "npm packument unavailable",
                    "Existing package metadata could not be read",
                ));
            }
        }
        let unsupported = || {
            upload_problem(
                409,
                "derived_metadata_not_managed",
                "Existing npm metadata is unsupported",
                "versions, dist-tags, and time must be JSON objects",
            )
        };
        for field in ["versions", "dist-tags", "time"] {
            if !packument
                .get(field)
                .is_some_and(serde_json::Value::is_object)
            {
                return Err(unsupported());
            }
        }
        if packument["versions"]
            .as_object()
            .expect("object")
            .contains_key(&pkg.version)
        {
            return Err(upload_problem(
                409,
                "immutable_version_exists",
                "npm version already exists",
                "The package and version already exist",
            ));
        }

        let mut version_manifest = pkg.manifest.clone();
        for field in [
            "private",
            "publishConfig",
            "_id",
            "_from",
            "_resolved",
            "dist",
        ] {
            version_manifest.remove(field);
        }
        version_manifest.insert("name".to_string(), serde_json::json!(pkg.name));
        version_manifest.insert("version".to_string(), serde_json::json!(pkg.version));
        version_manifest.insert(
            "_id".to_string(),
            serde_json::json!(format!("{}@{}", pkg.name, pkg.version)),
        );
        version_manifest.insert(
            "dist".to_string(),
            serde_json::json!({
                "tarball": format!(
                    "{}/-/{}-{}.tgz",
                    pkg.name,
                    super::path_base(&pkg.name),
                    pkg.version
                ),
                "shasum": asset.sha1,
                "integrity": format!("sha512-{}", asset.sha512),
            }),
        );
        packument["versions"]
            .as_object_mut()
            .expect("object")
            .insert(
                pkg.version.clone(),
                serde_json::Value::Object(version_manifest),
            );
        packument["dist-tags"]
            .as_object_mut()
            .expect("object")
            .insert(tag.to_string(), serde_json::json!(pkg.version));
        let times = packument["time"].as_object_mut().expect("object");
        times.insert(pkg.version.clone(), serde_json::json!(now));
        times.insert("modified".to_string(), serde_json::json!(now));
        packument.insert("_id".to_string(), serde_json::json!(pkg.name));
        packument.insert("name".to_string(), serde_json::json!(pkg.name));
        Ok((sorted_json(serde_json::Value::Object(packument)), expected))
    }
}

fn json_object(pairs: &[(&str, serde_json::Value)]) -> String {
    let mut object = serde_json::Map::new();
    for (key, value) in pairs {
        object.insert((*key).to_string(), value.clone());
    }
    serde_json::to_string(&serde_json::Value::Object(object)).unwrap_or_default()
}

/// Recursively re-inserts object keys in sorted order.
///
pub(crate) fn sorted_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => {
            let sorted: std::collections::BTreeMap<String, serde_json::Value> = object
                .into_iter()
                .map(|(k, v)| (k, sorted_json(v)))
                .collect();
            let mut out = serde_json::Map::new();
            for (k, v) in sorted {
                out.insert(k, v);
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sorted_json).collect())
        }
        other => other,
    }
}

/// Why an npm tarball could not be inspected.
enum NpmTarballError {
    Invalid,
    EntryLimit,
    PackageJsonInvalid,
    PackageJsonMissing,
}

/// Reads the single bounded `package/package.json` out of a gzipped tarball.
/// Blocking: the caller runs it on the blocking pool.
fn read_npm_package_json<R: std::io::Read>(
    reader: R,
    max_entries: i64,
    max_field_bytes: i64,
) -> Result<Vec<u8>, NpmTarballError> {
    use std::io::Read as _;

    let gz = flate2::read::GzDecoder::new(reader);
    let mut archive = tar::Archive::new(gz);
    let entries = archive.entries().map_err(|_| NpmTarballError::Invalid)?;
    let mut package_json: Option<Vec<u8>> = None;
    let mut count = 0i64;
    for entry in entries {
        let mut entry = entry.map_err(|_| NpmTarballError::Invalid)?;
        count += 1;
        if count > max_entries {
            return Err(NpmTarballError::EntryLimit);
        }
        let Ok(path) = entry.path() else { continue };
        let name = path.to_string_lossy().to_string();
        let clean = clean_tar_path(name.strip_prefix("./").unwrap_or(&name));
        if clean != NPM_PACKAGE_JSON_PATH {
            continue;
        }
        let size = entry.header().size().unwrap_or(u64::MAX);
        let regular = entry.header().entry_type().is_file();
        if package_json.is_some() || !regular || size > max_field_bytes as u64 {
            return Err(NpmTarballError::PackageJsonInvalid);
        }
        let mut bytes = Vec::new();
        entry
            .by_ref()
            .take(max_field_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| NpmTarballError::PackageJsonInvalid)?;
        if bytes.len() as i64 > max_field_bytes {
            return Err(NpmTarballError::PackageJsonInvalid);
        }
        package_json = Some(bytes);
    }
    package_json.ok_or(NpmTarballError::PackageJsonMissing)
}

fn clean_tar_path(name: &str) -> String {
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

pub(crate) fn valid_npm_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() > 214
        || name != name.to_lowercase()
        || name
            .chars()
            .any(|c| matches!(c, '\\' | '\0' | '\r' | '\n' | '\t' | ' '))
    {
        return false;
    }
    if let Some(scoped) = name.strip_prefix('@') {
        let parts: Vec<&str> = scoped.split('/').collect();
        return parts.len() == 2
            && NPM_IDENTIFIER.is_match(parts[0])
            && NPM_IDENTIFIER.is_match(parts[1]);
    }
    !name.contains('/') && NPM_IDENTIFIER.is_match(name)
}

pub(crate) fn valid_strict_semver(version: &str) -> bool {
    if version.is_empty() || version.starts_with('v') || version.trim() != version {
        return false;
    }
    let (core_and_pre, build) = match version.split_once('+') {
        Some((core, build)) => (core, Some(build)),
        None => (version, None),
    };
    if let Some(build) = build
        && !valid_semver_identifiers(build, false)
    {
        return false;
    }
    let (core, prerelease) = match core_and_pre.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (core_and_pre, None),
    };
    let segments: Vec<&str> = core.split('.').collect();
    if segments.len() != 3 {
        return false;
    }
    if !segments.iter().all(|s| numeric_semver_identifier(s)) {
        return false;
    }
    match prerelease {
        None => true,
        Some(pre) => valid_semver_identifiers(pre, true),
    }
}

fn numeric_semver_identifier(value: &str) -> bool {
    if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
        return false;
    }
    value.bytes().all(|c| c.is_ascii_digit())
}

/// Reports whether every dot-separated identifier in a semver pre-release or
/// build segment is well-formed.
pub(crate) fn valid_semver_identifiers(value: &str, reject_numeric_leading_zero: bool) -> bool {
    for identifier in value.split('.') {
        if identifier.is_empty() {
            return false;
        }
        let mut numeric = true;
        for char in identifier.chars() {
            if !(char.is_ascii_alphanumeric() || char == '-') {
                return false;
            }
            if !char.is_ascii_digit() {
                numeric = false;
            }
        }
        if reject_numeric_leading_zero
            && numeric
            && identifier.len() > 1
            && identifier.starts_with('0')
        {
            return false;
        }
    }
    true
}

fn valid_npm_dist_tag(tag: &str) -> bool {
    if tag.is_empty()
        || tag.len() > 128
        || tag.chars().any(|c| {
            matches!(
                c,
                '/' | '\\'
                    | '@'
                    | '%'
                    | ':'
                    | '~'
                    | '^'
                    | '*'
                    | '<'
                    | '>'
                    | '='
                    | '|'
                    | ' '
                    | '\t'
                    | '\r'
                    | '\n'
            )
        })
    {
        return false;
    }
    if valid_strict_semver(tag)
        || (tag.starts_with('v') && valid_strict_semver(tag.strip_prefix('v').unwrap_or(tag)))
    {
        return false;
    }
    tag.bytes().all(|c| (0x21..=0x7e).contains(&c))
}
