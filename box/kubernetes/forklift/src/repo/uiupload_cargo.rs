//! The Cargo publisher: `.crate` inspection, sparse-index planning and the
//! atomic commit.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::meta::{
    self, Artifact, ArtifactBatch, ArtifactCAS, ArtifactPublication, ArtifactUploadRequest,
    Repository, UPLOAD_RECEIVING, UploadRequestKey,
};

use super::uiupload::{
    ArtifactUploadManifest, ArtifactUploadResult, PublishResult, StagedUploadAsset, UploadProblem,
    UploadedArtifact, Uploader, upload_problem,
};
use super::uiupload_maven::uploaded_result;

/// The identity and index inputs read out of an uploaded `.crate`.
#[derive(Debug, Clone, Default)]
struct CargoPackageData {
    name: String,
    version: String,
    version_id: String,
    dependencies: Vec<serde_json::Value>,
    features: serde_json::Map<String, serde_json::Value>,
    features2: serde_json::Map<String, serde_json::Value>,
    index_version: i64,
    links: String,
    rust_version: String,
}

impl Uploader {
    pub(crate) async fn publish_cargo(
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
                "Invalid Cargo manifest",
                "Exactly the Cargo format object is required",
            )
        };
        let Some(cargo) = &manifest.cargo else {
            return Err(format_fields());
        };
        if manifest.maven.is_some()
            || manifest.npm.is_some()
            || manifest.pypi.is_some()
            || manifest.go_.is_some()
        {
            return Err(format_fields());
        }
        if manifest.overwrite || cargo.yanked {
            return Err(upload_problem(
                422,
                "redeploy_not_supported",
                "Cargo upload is immutable",
                "Upload publishes one non-yanked crate version",
            ));
        }
        if staged.len() != 1 || !staged[0].filename.to_lowercase().ends_with(".crate") {
            return Err(upload_problem(
                415,
                "cargo_crate_required",
                "Cargo crate required",
                "Upload exactly one .crate archive",
            ));
        }
        let asset = &staged[0];
        let pkg = self.inspect_cargo_crate(asset).await?;
        let Ok(publications) = self.store.list_artifact_publications(repository.id).await else {
            return Err(upload_problem(
                503,
                "storage_unavailable",
                "Upload unavailable",
                "Existing Cargo crate names could not be checked",
            ));
        };
        let requested_name = pkg.name.to_lowercase();
        for existing in &publications {
            if existing.format == meta::FORMAT_CARGO
                && cargo_canonical_name(&existing.package_name)
                    == cargo_canonical_name(&requested_name)
                && existing.package_name != requested_name
            {
                return Err(upload_problem(
                    409,
                    "crate_name_conflict",
                    "Cargo crate name conflicts",
                    "Crate names are unique ignoring case and hyphen/underscore differences",
                ));
            }
        }
        let crate_path = format!("api/v1/crates/{}/{}/download", pkg.name, pkg.version);
        let index_path = cargo_sparse_path(&pkg.name);
        let publication_time = self.now().with_nanosecond(0).unwrap_or_else(|| self.now());
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
            format: meta::FORMAT_CARGO.to_string(),
            package_name: requested_name.clone(),
            version: pkg.version_id.clone(),
            coordinate: format!("{}@{}", pkg.name, pkg.version),
            upload_id: request.upload_id.clone(),
            created_by: request.principal_name.clone(),
            created_by_source: request.principal_source.clone(),
            created_at: publication_time,
            updated_at: publication_time,
            ..Default::default()
        };
        let Ok(tombstoned) = self
            .store
            .has_publication_tombstone(
                repository.id,
                meta::FORMAT_CARGO,
                &requested_name,
                &pkg.version_id,
                "*",
            )
            .await
        else {
            return Err(upload_problem(
                503,
                "storage_unavailable",
                "Upload unavailable",
                "The Cargo tombstone could not be checked",
            ));
        };
        if tombstoned {
            return Err(upload_problem(
                409,
                "immutable_version_exists",
                "Cargo version cannot be reused",
                "This crate version was previously removed",
            ));
        }

        let (index_bytes, expected_index) = self
            .plan_cargo_index(
                repository.id,
                &index_path,
                &pkg,
                &asset.digest,
                publication_time,
            )
            .await?;
        let stage_failed = |detail: &str| {
            upload_problem(503, "storage_unavailable", "Upload storage failed", detail)
        };
        let index_asset = self
            .stage_generated(
                &index_path,
                "index",
                "text/plain; charset=utf-8",
                &index_bytes,
            )
            .await
            .map_err(|_| stage_failed("The Cargo sparse index could not be staged"))?;

        let owned_metadata = serde_json::to_string(&serde_json::json!({
            "format": "cargo",
            "format_metadata": {
                "checksum": asset.digest,
                "index_schema_version": 1,
            },
            "managed_by": "ui_upload",
            "package": pkg.name,
            "role": "primary",
            "schema_version": 1,
            "source_filename": asset.filename,
            "upload_id": request.upload_id,
            "version_identity": pkg.version_id,
        }))
        .unwrap_or_default();
        let aggregate_metadata = serde_json::to_string(&serde_json::json!({
            "aggregate_schema_version": 1,
            "format": "cargo",
            "managed_by": "ui_upload_aggregate",
            "package": requested_name,
            "schema_version": 1,
        }))
        .unwrap_or_default();
        let primary = Artifact {
            repo_id: repository.id,
            path: crate_path.clone(),
            version: pkg.version.clone(),
            blob_sha256: asset.digest.clone(),
            size: asset.size,
            content_type: "application/gzip".to_string(),
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
                path: index_path.clone(),
                blob_sha256: index_asset.digest.clone(),
                size: index_asset.size,
                content_type: "text/plain; charset=utf-8".to_string(),
                metadata_json: aggregate_metadata,
                cached_by: request.principal_name.clone(),
                artifact_role: "index".to_string(),
                ..Default::default()
            },
            expected_sha256: expected_index,
        };
        let mut result = ArtifactUploadResult {
            upload_id: request.upload_id.clone(),
            repository: repository.name.clone(),
            format: meta::FORMAT_CARGO.to_string(),
            coordinate: publication.coordinate.clone(),
            created: vec![UploadedArtifact {
                path: crate_path,
                role: "primary".to_string(),
                size: asset.size,
                sha256: asset.digest.clone(),
            }],
            replaced: Vec::new(),
            derived: vec![uploaded_result(&index_asset)],
            scan_status: self.scan_status(),
            durability: self.durability_value(),
            warnings: Vec::new(),
        };
        let mut batch = ArtifactBatch {
            expected_upload_state: UPLOAD_RECEIVING.to_string(),
            publication: publication.clone(),
            create: vec![primary],
            mutable_cas: vec![index],
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
            let (value, expected) = self
                .plan_cargo_index(
                    repository.id,
                    &index_path,
                    &pkg,
                    &asset.digest,
                    publication_time,
                )
                .await?;
            let staged_index = self
                .stage_generated(&index_path, "index", "text/plain; charset=utf-8", &value)
                .await
                .map_err(|_| stage_failed("The Cargo index could not be staged"))?;
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
                "Cargo version already exists",
                "The crate version already exists",
            )),
            Err(meta::Error::DerivedMetadataChanged) => Err(upload_problem(
                409,
                "concurrent_metadata_update",
                "Cargo index changed concurrently",
                "Retry with a new idempotency key",
            )),
            Err(_) => Err(upload_problem(
                503,
                "storage_unavailable",
                "Upload commit failed",
                "The Cargo publication could not be committed",
            )),
            Ok(()) => {
                self.record_committed(repository, &batch.create);
                self.notify_published(repository, &publication, &request.principal_name)
                    .await;
                Ok(result)
            }
        }
    }

    /// Reads `Cargo.toml` out of the staged `.crate` and derives the sparse
    /// index inputs from it.
    async fn inspect_cargo_crate(
        &self,
        asset: &StagedUploadAsset,
    ) -> Result<CargoPackageData, Box<UploadProblem>> {
        let (reader, _) = self.engine.blobs.open(&asset.digest).await.map_err(|_| {
            upload_problem(
                503,
                "storage_unavailable",
                "Staged crate unavailable",
                "The .crate archive could not be reopened",
            )
        })?;
        let (max_entries, max_meta) = (
            self.cfg.archive_max_entries,
            self.cfg.archive_max_meta_bytes,
        );
        let bridged = tokio_util::io::SyncIoBridge::new(reader);
        let (root, manifest_bytes) = tokio::task::spawn_blocking(move || {
            read_crate_manifest(bridged, max_entries, max_meta)
        })
        .await
        .unwrap_or(Err(CrateError::ArchiveInvalid))
        .map_err(|err| match err {
            CrateError::ArchiveInvalid => upload_problem(
                422,
                "cargo_archive_invalid",
                "Invalid Cargo archive",
                "The .crate file is not a gzip tar archive",
            ),
            CrateError::PathInvalid => upload_problem(
                422,
                "cargo_path_invalid",
                "Unsafe Cargo paths",
                "Every crate entry must be under one top-level package directory",
            ),
            CrateError::ManifestInvalid => upload_problem(
                422,
                "cargo_manifest_invalid",
                "Invalid Cargo.toml",
                "Exactly one bounded regular Cargo.toml is required",
            ),
        })?;
        let Some(manifest_bytes) = manifest_bytes else {
            return Err(upload_problem(
                422,
                "cargo_manifest_missing",
                "Cargo.toml missing",
                "The .crate archive must contain Cargo.toml",
            ));
        };
        let manifest_invalid = || {
            upload_problem(
                422,
                "cargo_manifest_invalid",
                "Invalid Cargo.toml",
                "The normalized manifest is not valid TOML",
            )
        };
        let text = String::from_utf8(manifest_bytes).map_err(|_| manifest_invalid())?;
        let document: toml::Table = toml::from_str(&text).map_err(|_| manifest_invalid())?;
        let Some(package_table) = document.get("package").and_then(toml::Value::as_table) else {
            return Err(upload_problem(
                422,
                "cargo_identity_invalid",
                "Cargo package missing",
                "Cargo.toml requires a package table",
            ));
        };
        let name = package_table
            .get("name")
            .and_then(toml::Value::as_str)
            .unwrap_or("")
            .to_string();
        let version = package_table
            .get("version")
            .and_then(toml::Value::as_str)
            .unwrap_or("")
            .to_string();
        let parsed_version = semver::Version::parse(&version);
        if !valid_cargo_name(&name)
            || parsed_version.is_err()
            || root != format!("{name}-{version}")
        {
            return Err(upload_problem(
                422,
                "cargo_identity_mismatch",
                "Cargo identity mismatch",
                "Archive root and Cargo.toml must identify the same valid crate version",
            ));
        }
        let dependencies = cargo_index_dependencies(&document)?;
        let mut features = serde_json::Map::new();
        let mut features2 = serde_json::Map::new();
        if let Some(raw) = document.get("features").and_then(toml::Value::as_table) {
            for (feature_name, value) in raw {
                let modern = value.as_array().is_some_and(|members| {
                    members.iter().any(|member| {
                        let text = member.as_str().unwrap_or("");
                        text.starts_with("dep:") || text.contains("?/")
                    })
                });
                let converted = toml_to_json(value);
                if modern {
                    features2.insert(feature_name.clone(), converted);
                } else {
                    features.insert(feature_name.clone(), converted);
                }
            }
        }
        let links = package_table
            .get("links")
            .and_then(toml::Value::as_str)
            .unwrap_or("")
            .to_string();
        let rust_version = package_table
            .get("rust-version")
            .and_then(toml::Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut version_id = parsed_version.expect("valid version").to_string();
        if let Some(index) = version_id.find('+') {
            version_id.truncate(index);
        }
        let index_version = if features2.is_empty() { 1 } else { 2 };
        Ok(CargoPackageData {
            name,
            version,
            version_id,
            dependencies,
            features,
            features2,
            index_version,
            links,
            rust_version,
        })
    }

    /// Renders the new sparse-index line and merges it into the existing index,
    /// reporting the digest the commit must find unchanged.
    async fn plan_cargo_index(
        &self,
        repo_id: i64,
        index_path: &str,
        pkg: &CargoPackageData,
        checksum: &str,
        publication_time: chrono::DateTime<chrono::Utc>,
    ) -> Result<(Vec<u8>, String), Box<UploadProblem>> {
        let mut entry = serde_json::Map::new();
        entry.insert("name".to_string(), serde_json::json!(pkg.name));
        entry.insert("vers".to_string(), serde_json::json!(pkg.version));
        entry.insert(
            "deps".to_string(),
            serde_json::Value::Array(pkg.dependencies.clone()),
        );
        entry.insert("cksum".to_string(), serde_json::json!(checksum));
        entry.insert(
            "features".to_string(),
            serde_json::Value::Object(pkg.features.clone()),
        );
        entry.insert("yanked".to_string(), serde_json::json!(false));
        entry.insert(
            "pubtime".to_string(),
            serde_json::json!(publication_time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        );
        if !pkg.features2.is_empty() {
            entry.insert(
                "features2".to_string(),
                serde_json::Value::Object(pkg.features2.clone()),
            );
            entry.insert("v".to_string(), serde_json::json!(pkg.index_version));
        }
        if !pkg.links.is_empty() {
            entry.insert("links".to_string(), serde_json::json!(pkg.links));
        }
        if !pkg.rust_version.is_empty() {
            entry.insert(
                "rust_version".to_string(),
                serde_json::json!(pkg.rust_version),
            );
        }
        let Ok(encoded) = serde_json::to_string(&sorted_json(serde_json::Value::Object(entry)))
        else {
            return Err(upload_problem(
                503,
                "result_encoding_failed",
                "Upload unavailable",
                "The Cargo index entry could not be encoded",
            ));
        };

        let mut expected = String::new();
        let mut lines: Vec<String> = Vec::new();
        match self.store.get_artifact(repo_id, index_path).await {
            Ok(artifact) => {
                expected = artifact.blob_sha256.clone();
                let value = self
                    .read_bounded_blob(&artifact.blob_sha256, self.cfg.archive_max_meta_bytes)
                    .await
                    .map_err(|_| {
                        upload_problem(
                            409,
                            "derived_metadata_not_managed",
                            "Existing Cargo index is unsupported",
                            "The sparse index exceeds parsing limits",
                        )
                    })?;
                let unsupported = |detail: &str| {
                    upload_problem(
                        409,
                        "derived_metadata_not_managed",
                        "Existing Cargo index is unsupported",
                        detail,
                    )
                };
                let text = String::from_utf8_lossy(&value).to_string();
                for line in text.lines() {
                    if line.is_empty() {
                        continue;
                    }
                    if line.len() as i64 > self.cfg.max_field_bytes {
                        return Err(unsupported("The sparse index exceeds parsing limits"));
                    }
                    #[derive(serde::Deserialize)]
                    struct Existing {
                        #[serde(default)]
                        vers: String,
                    }
                    let existing: Existing = serde_json::from_str(line)
                        .map_err(|_| unsupported("A sparse-index line is malformed"))?;
                    let parsed = semver::Version::parse(&existing.vers)
                        .map_err(|_| unsupported("A sparse-index version is invalid"))?;
                    let mut identity = parsed.to_string();
                    if let Some(index) = identity.find('+') {
                        identity.truncate(index);
                    }
                    if identity == pkg.version_id {
                        return Err(upload_problem(
                            409,
                            "immutable_version_exists",
                            "Cargo version already exists",
                            "The sparse index already contains this SemVer identity",
                        ));
                    }
                    lines.push(line.to_string());
                }
            }
            Err(meta::Error::NotFound) => {}
            Err(_) => {
                return Err(upload_problem(
                    503,
                    "storage_unavailable",
                    "Cargo index unavailable",
                    "The sparse index could not be read",
                ));
            }
        }
        lines.push(encoded);
        Ok((format!("{}\n", lines.join("\n")).into_bytes(), expected))
    }
}

/// Builds the sparse index's `deps` array from the manifest's dependency
/// tables.
///
fn cargo_index_dependencies(
    document: &toml::Table,
) -> Result<Vec<serde_json::Value>, Box<UploadProblem>> {
    let mut result = Vec::new();
    append_dependency_tables(document, None, &mut result)?;
    if let Some(targets) = document.get("target") {
        let Some(targets) = targets.as_table() else {
            return Err(upload_problem(
                422,
                "cargo_target_invalid",
                "Invalid target dependencies",
                "Target dependency tables must be TOML tables",
            ));
        };
        for (target_name, table) in targets {
            let Some(table) = table.as_table() else {
                return Err(upload_problem(
                    422,
                    "cargo_target_invalid",
                    "Invalid target dependencies",
                    "Target dependency tables must be TOML tables",
                ));
            };
            append_dependency_tables(table, Some(target_name.as_str()), &mut result)?;
        }
    }
    Ok(result)
}

fn append_dependency_tables(
    container: &toml::Table,
    target: Option<&str>,
    result: &mut Vec<serde_json::Value>,
) -> Result<(), Box<UploadProblem>> {
    for (table_name, kind) in [
        ("dependencies", "normal"),
        ("dev-dependencies", "dev"),
        ("build-dependencies", "build"),
    ] {
        let Some(table) = container.get(table_name).and_then(toml::Value::as_table) else {
            continue;
        };
        for (alias, value) in table {
            let mut dependency = serde_json::Map::new();
            dependency.insert("name".to_string(), serde_json::json!(alias));
            dependency.insert("features".to_string(), serde_json::json!([] as [&str; 0]));
            dependency.insert("optional".to_string(), serde_json::json!(false));
            dependency.insert("default_features".to_string(), serde_json::json!(true));
            dependency.insert(
                "target".to_string(),
                match target {
                    Some(target) => serde_json::json!(target),
                    None => serde_json::Value::Null,
                },
            );
            dependency.insert("kind".to_string(), serde_json::json!(kind));
            dependency.insert("registry".to_string(), serde_json::Value::Null);
            match value {
                toml::Value::String(requirement) => {
                    dependency.insert("req".to_string(), serde_json::json!(requirement));
                }
                toml::Value::Table(typed) => {
                    let requirement = typed
                        .get("version")
                        .and_then(toml::Value::as_str)
                        .unwrap_or("");
                    if requirement.is_empty() {
                        if typed.contains_key("path") {
                            return Err(upload_problem(
                                422,
                                "cargo_path_dependency",
                                "Unversioned path dependency",
                                "Published path dependencies require a version",
                            ));
                        }
                        return Err(upload_problem(
                            422,
                            "cargo_dependency_invalid",
                            "Invalid Cargo dependency",
                            "Every registry dependency requires a version",
                        ));
                    }
                    dependency.insert("req".to_string(), serde_json::json!(requirement));
                    if let Some(package_name) = typed.get("package").and_then(toml::Value::as_str)
                        && !package_name.is_empty()
                    {
                        dependency.insert("package".to_string(), serde_json::json!(package_name));
                    }
                    if let Some(optional) = typed.get("optional").and_then(toml::Value::as_bool) {
                        dependency.insert("optional".to_string(), serde_json::json!(optional));
                    }
                    if let Some(default_features) =
                        typed.get("default-features").and_then(toml::Value::as_bool)
                    {
                        dependency.insert(
                            "default_features".to_string(),
                            serde_json::json!(default_features),
                        );
                    }
                    if let Some(values) = typed.get("features").and_then(toml::Value::as_array) {
                        let feature_names: Vec<&str> =
                            values.iter().filter_map(toml::Value::as_str).collect();
                        dependency.insert("features".to_string(), serde_json::json!(feature_names));
                    }
                    if let Some(registry) = typed.get("registry").and_then(toml::Value::as_str)
                        && !registry.is_empty()
                    {
                        dependency.insert("registry".to_string(), serde_json::json!(registry));
                    }
                }
                _ => {
                    return Err(upload_problem(
                        422,
                        "cargo_dependency_invalid",
                        "Invalid Cargo dependency",
                        "Dependency entries must be strings or tables",
                    ));
                }
            }
            result.push(sorted_json(serde_json::Value::Object(dependency)));
        }
    }
    Ok(())
}

/// Converts a TOML value into the JSON the sparse index carries.
fn toml_to_json(value: &toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(s) => serde_json::json!(s),
        toml::Value::Integer(i) => serde_json::json!(i),
        toml::Value::Float(f) => serde_json::json!(f),
        toml::Value::Boolean(b) => serde_json::json!(b),
        toml::Value::Datetime(d) => serde_json::json!(d.to_string()),
        toml::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(toml_to_json).collect())
        }
        toml::Value::Table(table) => {
            let mut out = serde_json::Map::new();
            for (key, value) in table {
                out.insert(key.clone(), toml_to_json(value));
            }
            serde_json::Value::Object(out)
        }
    }
}

fn sorted_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => {
            let sorted: BTreeMap<String, serde_json::Value> = object
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

/// Why a `.crate` archive could not be inspected.
#[allow(clippy::enum_variant_names)] // Names match the public problem codes.
enum CrateError {
    ArchiveInvalid,
    PathInvalid,
    ManifestInvalid,
}

/// Reads the archive root and its `Cargo.toml` out of a `.crate`. Blocking: the
/// caller runs it on the blocking pool.
fn read_crate_manifest<R: std::io::Read>(
    reader: R,
    max_entries: i64,
    max_meta_bytes: i64,
) -> Result<(String, Option<Vec<u8>>), CrateError> {
    use std::io::Read as _;

    let gz = flate2::read::GzDecoder::new(reader);
    let mut archive = tar::Archive::new(gz);
    let entries = archive.entries().map_err(|_| CrateError::ArchiveInvalid)?;
    let mut manifest: Option<Vec<u8>> = None;
    let mut root = String::new();
    let mut seen = std::collections::HashSet::new();
    let mut count = 0i64;
    for entry in entries {
        let mut entry = entry.map_err(|_| CrateError::ArchiveInvalid)?;
        count += 1;
        let Ok(path) = entry.path() else {
            return Err(CrateError::PathInvalid);
        };
        let name = path.to_string_lossy().to_string();
        let clean = clean_archive_path(name.strip_prefix("./").unwrap_or(&name));
        let parts: Vec<&str> = clean.split('/').collect();
        if count > max_entries || parts.len() < 2 || clean.is_empty() || clean.starts_with("../") {
            return Err(CrateError::PathInvalid);
        }
        if root.is_empty() {
            root = parts[0].to_string();
        }
        let folded = clean.to_lowercase();
        if parts[0] != root || !seen.insert(folded) {
            return Err(CrateError::PathInvalid);
        }
        if clean != format!("{root}/Cargo.toml") {
            continue;
        }
        let size = entry.header().size().unwrap_or(u64::MAX);
        if !entry.header().entry_type().is_file()
            || size > max_meta_bytes as u64
            || manifest.is_some()
        {
            return Err(CrateError::ManifestInvalid);
        }
        let mut bytes = Vec::new();
        entry
            .by_ref()
            .take(max_meta_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| CrateError::ManifestInvalid)?;
        if bytes.len() as i64 > max_meta_bytes {
            return Err(CrateError::ManifestInvalid);
        }
        manifest = Some(bytes);
    }
    Ok((root, manifest))
}

fn clean_archive_path(name: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for segment in name.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if out.pop().is_none() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out.join("/")
}

fn valid_cargo_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 || !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub(crate) fn cargo_canonical_name(name: &str) -> String {
    name.to_lowercase().replace('_', "-")
}

pub(crate) fn cargo_sparse_path(name: &str) -> String {
    let value = name.to_lowercase();
    match value.len() {
        1 => format!("1/{value}"),
        2 => format!("2/{value}"),
        3 => format!("3/{}/{value}", &value[..1]),
        _ => format!("{}/{}/{value}", &value[..2], &value[2..4]),
    }
}

use chrono::Timelike as _;
