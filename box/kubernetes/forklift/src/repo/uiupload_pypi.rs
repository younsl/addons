//! The PyPI publisher: wheel/sdist inspection against PEP 440/427 and the
//! atomic commit, plus the twine upload adapter.

use std::collections::BTreeMap;
use std::sync::Arc;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::meta::{
    self, Artifact, ArtifactBatch, ArtifactPublication, ArtifactUploadRequest, Repository,
    UPLOAD_RECEIVING, UploadRequestKey,
};
use crate::repoconfig;

use super::pypi::normalize_pypi;
use super::uiupload::{
    ArtifactUploadManifest, ArtifactUploadResult, PublishResult, StagedUploadAsset, UploadProblem,
    UploadedArtifact, Uploader, upload_problem,
};

/// PEP 440's version grammar, as the canonicaliser applies it.
static PYPI_VERSION_GRAMMAR: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)^v?(?:(\d+)!)?(\d+(?:\.\d+)*)(?:[-_.]?(a|b|c|rc|alpha|beta|pre|preview)(?:[-_.]?(\d+))?)?(?:(?:-(\d+))|(?:[-_.]?(post|rev|r)(?:[-_.]?(\d+))?))?(?:[-_.]?(dev)(?:[-_.]?(\d+))?)?(?:\+([a-z0-9]+(?:[-_.][a-z0-9]+)*))?$",
    )
    .expect("valid regex")
});

static LOCAL_SEGMENT_SPLIT: Lazy<Regex> = Lazy::new(|| Regex::new(r"[-_.]").expect("valid regex"));

/// One inspected distribution file.
#[derive(Debug, Clone, Default)]
struct PyPIDistribution {
    asset: StagedUploadAsset,
    project: String,
    display_version: String,
    version_id: String,
    kind: String,
    requires_python: String,
    metadata_version: String,
    wheel_tags: Vec<String>,
    content_type: String,
}

#[derive(Debug, Clone, Default)]
struct MailHeader(BTreeMap<String, Vec<String>>);

impl MailHeader {
    fn get(&self, key: &str) -> &str {
        self.0
            .get(&canonical_header_key(key))
            .and_then(|values| values.first())
            .map(String::as_str)
            .unwrap_or("")
    }

    fn values(&self, key: &str) -> Vec<String> {
        self.0
            .get(&canonical_header_key(key))
            .cloned()
            .unwrap_or_default()
    }
}

impl Uploader {
    pub(crate) async fn publish_pypi(
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
                "Invalid PyPI manifest",
                "Exactly the PyPI format object is required",
            )
        };
        if manifest.pypi.is_none()
            || manifest.maven.is_some()
            || manifest.npm.is_some()
            || manifest.cargo.is_some()
            || manifest.go_.is_some()
        {
            return Err(format_fields());
        }
        if manifest.overwrite {
            return Err(upload_problem(
                422,
                "redeploy_not_supported",
                "PyPI files are immutable",
                "A distribution filename cannot be replaced",
            ));
        }
        let Ok(repository_config) = repoconfig::parse(&repository.config_json) else {
            return Err(upload_problem(
                503,
                "repository_config_invalid",
                "Repository unavailable",
                "The repository upload configuration is invalid",
            ));
        };
        let mut distributions: Vec<PyPIDistribution> = Vec::with_capacity(staged.len());
        let mut seen_names = std::collections::HashSet::new();
        for asset in staged {
            if !seen_names.insert(asset.filename.clone()) {
                return Err(upload_problem(
                    422,
                    "duplicate_asset",
                    "Duplicate PyPI filename",
                    "Every distribution filename must be unique",
                ));
            }
            distributions.push(
                self.inspect_pypi_distribution(
                    asset,
                    repository_config.upload.pypi_allow_legacy_zip,
                )
                .await?,
            );
        }
        let Some(first) = distributions.first().cloned() else {
            return Err(upload_problem(
                422,
                "pypi_asset_count",
                "PyPI distributions required",
                "Upload at least one wheel or source distribution",
            ));
        };
        let (project, version_id) = (first.project.clone(), first.version_id.clone());
        for distribution in &distributions[1..] {
            if distribution.project != project || distribution.version_id != version_id {
                return Err(upload_problem(
                    422,
                    "pypi_release_mismatch",
                    "PyPI release mismatch",
                    "All files in one request must belong to the same normalized project and version",
                ));
            }
        }

        let publication_time = self.now();
        let publication = match self
            .store
            .get_artifact_publication_by_identity(
                repository.id,
                meta::FORMAT_PYPI,
                &project,
                &version_id,
            )
            .await
        {
            Ok(mut publication) => {
                publication.upload_id = request.upload_id.clone();
                publication.updated_at = publication_time;
                publication
            }
            Err(meta::Error::NotFound) => {
                let Some(publication_id) = (self.new_id.read())() else {
                    return Err(upload_problem(
                        503,
                        "storage_unavailable",
                        "Upload unavailable",
                        "Could not allocate a publication identifier",
                    ));
                };
                ArtifactPublication {
                    id: publication_id,
                    repo_id: repository.id,
                    format: meta::FORMAT_PYPI.to_string(),
                    package_name: project.clone(),
                    version: version_id.clone(),
                    coordinate: format!("{project}=={}", first.display_version),
                    upload_id: request.upload_id.clone(),
                    created_by: request.principal_name.clone(),
                    created_by_source: request.principal_source.clone(),
                    created_at: publication_time,
                    updated_at: publication_time,
                    ..Default::default()
                }
            }
            Err(_) => {
                return Err(upload_problem(
                    503,
                    "storage_unavailable",
                    "Upload unavailable",
                    "Existing PyPI publication state could not be read",
                ));
            }
        };

        let mut create: Vec<Artifact> = Vec::with_capacity(distributions.len());
        let mut created: Vec<UploadedArtifact> = Vec::with_capacity(distributions.len());
        for distribution in &distributions {
            let Ok(tombstoned) = self
                .store
                .has_publication_tombstone(
                    repository.id,
                    meta::FORMAT_PYPI,
                    &project,
                    &version_id,
                    &distribution.asset.filename,
                )
                .await
            else {
                return Err(upload_problem(
                    503,
                    "storage_unavailable",
                    "Upload unavailable",
                    "The PyPI filename tombstone could not be checked",
                ));
            };
            if tombstoned {
                return Err(upload_problem(
                    409,
                    "distribution_filename_reused",
                    "PyPI filename cannot be reused",
                    "A removed distribution filename is permanently reserved",
                ));
            }
            let artifact_path = format!("packages/{project}/{}", distribution.asset.filename);
            let metadata_json = serde_json::to_string(&serde_json::json!({
                "format": "pypi",
                "format_metadata": {
                    "display_version": distribution.display_version,
                    "distribution_kind": distribution.kind,
                    "metadata_version": distribution.metadata_version,
                    "requires_python": distribution.requires_python,
                    "wheel_tags": distribution.wheel_tags,
                },
                "managed_by": "ui_upload",
                "package": project,
                "role": "primary",
                "schema_version": 1,
                "source_filename": distribution.asset.filename,
                "upload_id": request.upload_id,
                "version_identity": version_id,
            }))
            .unwrap_or_default();
            create.push(Artifact {
                repo_id: repository.id,
                path: artifact_path.clone(),
                version: distribution.display_version.clone(),
                blob_sha256: distribution.asset.digest.clone(),
                size: distribution.asset.size,
                content_type: distribution.content_type.clone(),
                metadata_json,
                published_at: Some(publication_time),
                cached_by: request.principal_name.clone(),
                publication_id: publication.id.clone(),
                artifact_role: "primary".to_string(),
                ..Default::default()
            });
            created.push(UploadedArtifact {
                path: artifact_path,
                role: "primary".to_string(),
                size: distribution.asset.size,
                sha256: distribution.asset.digest.clone(),
            });
        }
        let result = ArtifactUploadResult {
            upload_id: request.upload_id.clone(),
            repository: repository.name.clone(),
            format: meta::FORMAT_PYPI.to_string(),
            coordinate: publication.coordinate.clone(),
            created,
            replaced: Vec::new(),
            derived: Vec::new(),
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
        match self
            .store
            .apply_artifact_batch(
                key,
                ArtifactBatch {
                    expected_upload_state: UPLOAD_RECEIVING.to_string(),
                    publication: publication.clone(),
                    create: create.clone(),
                    upload_result_json: result_json,
                    ..Default::default()
                },
            )
            .await
        {
            Err(meta::Error::ArtifactConflict) => Err(upload_problem(
                409,
                "distribution_filename_reused",
                "PyPI distribution already exists",
                "One or more distribution filenames already exist",
            )),
            Err(_) => Err(upload_problem(
                503,
                "storage_unavailable",
                "Upload commit failed",
                "The PyPI publication could not be committed",
            )),
            Ok(()) => {
                self.record_committed(repository, &create);
                self.notify_published(repository, &publication, &request.principal_name)
                    .await;
                Ok(result)
            }
        }
    }

    async fn inspect_pypi_distribution(
        &self,
        asset: &StagedUploadAsset,
        allow_legacy_zip: bool,
    ) -> Result<PyPIDistribution, Box<UploadProblem>> {
        let lower = asset.filename.to_lowercase();
        if lower.ends_with(".whl") {
            return self.inspect_pypi_wheel(asset).await;
        }
        if lower.ends_with(".tar.gz") {
            return self.inspect_pypi_sdist(asset).await;
        }
        if lower.ends_with(".zip") {
            if !allow_legacy_zip {
                return Err(upload_problem(
                    415,
                    "pypi_legacy_zip_disabled",
                    "Legacy ZIP sdist disabled",
                    "Enable the repository compatibility option before uploading legacy ZIP sdists",
                ));
            }
            return self.inspect_pypi_legacy_zip(asset).await;
        }
        Err(upload_problem(
            415,
            "pypi_distribution_unsupported",
            "Unsupported PyPI distribution",
            "Upload a .whl or .tar.gz distribution",
        ))
    }

    async fn inspect_pypi_legacy_zip(
        &self,
        asset: &StagedUploadAsset,
    ) -> Result<PyPIDistribution, Box<UploadProblem>> {
        let Some((name, version)) = parse_legacy_zip_filename(&asset.filename) else {
            return Err(upload_problem(
                422,
                "sdist_filename_invalid",
                "Invalid ZIP sdist filename",
                "The filename must identify a project and PEP 440 version",
            ));
        };
        let Some(seekable) = self.engine.blobs.as_seekable() else {
            return Err(upload_problem(
                503,
                "seekable_staging_unavailable",
                "Archive staging unavailable",
                "The blob store cannot validate a ZIP sdist",
            ));
        };
        let (file, _size) = seekable.open_seekable(&asset.digest).await.map_err(|_| {
            upload_problem(
                503,
                "storage_unavailable",
                "Staged sdist unavailable",
                "The ZIP sdist could not be reopened",
            )
        })?;
        let (max_entries, max_meta) = (
            self.cfg.archive_max_entries,
            self.cfg.archive_max_meta_bytes,
        );
        let expected_root = format!("{name}-{version}");
        let outcome = tokio::task::spawn_blocking(move || {
            read_legacy_zip_metadata(file, max_entries, max_meta)
        })
        .await
        .unwrap_or(Err(ZipSdistError::ArchiveInvalid));
        let (root, metadata_bytes) = outcome.map_err(|err| match err {
            ZipSdistError::ArchiveInvalid => upload_problem(
                422,
                "sdist_archive_invalid",
                "Invalid ZIP sdist",
                "The archive is malformed or exceeds the entry limit",
            ),
            ZipSdistError::Unsafe => upload_problem(
                422,
                "sdist_archive_invalid",
                "Unsafe ZIP sdist",
                "Archive paths and links must remain inside one root directory",
            ),
            ZipSdistError::RootMismatch => upload_problem(
                422,
                "sdist_root_invalid",
                "Invalid ZIP sdist root",
                "Every entry must share one project-version root",
            ),
            ZipSdistError::MetadataInvalid => upload_problem(
                422,
                "sdist_metadata_invalid",
                "Invalid PKG-INFO",
                "Exactly one bounded root PKG-INFO file is required",
            ),
        })?;
        let identity_mismatch = || {
            upload_problem(
                422,
                "sdist_identity_mismatch",
                "ZIP sdist identity mismatch",
                "Archive root, filename and PKG-INFO must identify the same release",
            )
        };
        if root != expected_root {
            return Err(identity_mismatch());
        }
        let Some(metadata_bytes) = metadata_bytes else {
            return Err(identity_mismatch());
        };
        let metadata = parse_core_metadata(&metadata_bytes).map_err(|_| {
            upload_problem(
                422,
                "sdist_metadata_invalid",
                "Invalid PKG-INFO",
                "PKG-INFO is not valid core metadata",
            )
        })?;
        let project = normalize_pypi(metadata.get("Name"));
        let version_id = canonical_pypi_version(metadata.get("Version"));
        if project != normalize_pypi(&name)
            || version_id.is_empty()
            || version_id != canonical_pypi_version(&version)
        {
            return Err(upload_problem(
                422,
                "sdist_identity_mismatch",
                "ZIP sdist identity mismatch",
                "Filename and PKG-INFO must identify the same release",
            ));
        }
        Ok(PyPIDistribution {
            asset: asset.clone(),
            project,
            display_version: metadata.get("Version").to_string(),
            version_id,
            kind: "sdist".to_string(),
            requires_python: metadata.get("Requires-Python").to_string(),
            metadata_version: metadata.get("Metadata-Version").to_string(),
            wheel_tags: Vec::new(),
            content_type: "application/zip".to_string(),
        })
    }

    async fn inspect_pypi_wheel(
        &self,
        asset: &StagedUploadAsset,
    ) -> Result<PyPIDistribution, Box<UploadProblem>> {
        let Some((filename_name, filename_version, filename_tags)) =
            parse_wheel_filename(&asset.filename)
        else {
            return Err(upload_problem(
                422,
                "wheel_filename_invalid",
                "Invalid wheel filename",
                "The wheel filename does not follow the binary distribution format",
            ));
        };
        let Some(seekable) = self.engine.blobs.as_seekable() else {
            return Err(upload_problem(
                503,
                "seekable_staging_unavailable",
                "Archive staging unavailable",
                "The blob store cannot provide bounded random access",
            ));
        };
        let (file, _size) = seekable.open_seekable(&asset.digest).await.map_err(|_| {
            upload_problem(
                503,
                "storage_unavailable",
                "Staged wheel unavailable",
                "The wheel could not be reopened",
            )
        })?;
        let (max_entries, max_meta) = (
            self.cfg.archive_max_entries,
            self.cfg.archive_max_meta_bytes,
        );
        let wheel_prefix = format!(
            "{}-{}.dist-info/",
            filename_name.replace('-', "_"),
            filename_version.replace('-', "_")
        );
        let required = tokio::task::spawn_blocking(move || {
            read_wheel_metadata(file, &wheel_prefix, max_entries, max_meta)
        })
        .await
        .unwrap_or(Err(WheelError::ArchiveInvalid))
        .map_err(|err| match err {
            WheelError::ArchiveInvalid => upload_problem(
                422,
                "wheel_invalid",
                "Invalid wheel archive",
                "The wheel ZIP is malformed or exceeds the entry limit",
            ),
            WheelError::PathInvalid => upload_problem(
                422,
                "wheel_path_invalid",
                "Unsafe wheel paths",
                "The wheel contains a duplicate or unsafe path",
            ),
            WheelError::MetadataInvalid => upload_problem(
                422,
                "wheel_metadata_invalid",
                "Invalid wheel metadata",
                "Required wheel metadata is duplicated or too large",
            ),
        })?;
        if required.len() != 3 {
            return Err(upload_problem(
                422,
                "wheel_metadata_missing",
                "Wheel metadata missing",
                "METADATA, WHEEL, and RECORD are required in the matching dist-info directory",
            ));
        }
        let metadata = parse_core_metadata(&required["METADATA"]).map_err(|_| {
            upload_problem(
                422,
                "wheel_metadata_invalid",
                "Invalid wheel metadata",
                "METADATA does not contain valid Name and Version headers",
            )
        })?;
        let project = normalize_pypi(metadata.get("Name"));
        let version_id = canonical_pypi_version(metadata.get("Version"));
        if project.is_empty()
            || version_id.is_empty()
            || project != normalize_pypi(&filename_name)
            || version_id != canonical_pypi_version(&filename_version)
        {
            return Err(upload_problem(
                422,
                "pypi_identity_mismatch",
                "Wheel identity mismatch",
                "Wheel filename, dist-info directory, and core metadata must identify the same release",
            ));
        }
        let wheel_headers = parse_metadata_headers(&required["WHEEL"]).map_err(|_| {
            upload_problem(
                422,
                "wheel_metadata_invalid",
                "Invalid WHEEL metadata",
                "WHEEL requires Wheel-Version and Root-Is-Purelib",
            )
        })?;
        if wheel_headers.get("Wheel-Version").is_empty()
            || wheel_headers.get("Root-Is-Purelib").is_empty()
        {
            return Err(upload_problem(
                422,
                "wheel_metadata_invalid",
                "Invalid WHEEL metadata",
                "WHEEL requires Wheel-Version and Root-Is-Purelib",
            ));
        }
        let mut header_tags = wheel_headers.values("Tag");
        header_tags.sort();
        if header_tags.is_empty() || header_tags != filename_tags {
            return Err(upload_problem(
                422,
                "wheel_tag_mismatch",
                "Wheel tags do not match",
                "Expanded WHEEL Tag headers must equal the filename tags",
            ));
        }
        Ok(PyPIDistribution {
            asset: asset.clone(),
            project,
            display_version: metadata.get("Version").to_string(),
            version_id,
            kind: "wheel".to_string(),
            requires_python: metadata.get("Requires-Python").to_string(),
            metadata_version: metadata.get("Metadata-Version").to_string(),
            wheel_tags: header_tags,
            content_type: "application/zip".to_string(),
        })
    }

    async fn inspect_pypi_sdist(
        &self,
        asset: &StagedUploadAsset,
    ) -> Result<PyPIDistribution, Box<UploadProblem>> {
        let Some((name, version)) = parse_sdist_filename(&asset.filename) else {
            return Err(upload_problem(
                422,
                "sdist_filename_invalid",
                "Invalid source distribution filename",
                "The .tar.gz filename must end in a valid name-version pair",
            ));
        };
        let (reader, _) = self.engine.blobs.open(&asset.digest).await.map_err(|_| {
            upload_problem(
                503,
                "storage_unavailable",
                "Staged sdist unavailable",
                "The source distribution could not be reopened",
            )
        })?;
        let (max_entries, max_meta) = (
            self.cfg.archive_max_entries,
            self.cfg.archive_max_meta_bytes,
        );
        let root = asset
            .filename
            .strip_suffix(".tar.gz")
            .unwrap_or(&asset.filename)
            .to_string();
        let bridged = tokio_util::io::SyncIoBridge::new(reader);
        let metadata_bytes = tokio::task::spawn_blocking(move || {
            read_sdist_metadata(bridged, &root, max_entries, max_meta)
        })
        .await
        .unwrap_or(Err(SdistError::Invalid))
        .map_err(|err| match err {
            SdistError::Invalid => upload_problem(
                422,
                "sdist_invalid",
                "Invalid source distribution",
                "The source distribution is not a gzip tar archive",
            ),
            SdistError::PathInvalid => upload_problem(
                422,
                "sdist_path_invalid",
                "Unsafe source distribution paths",
                "The source archive contains an unsafe, duplicate, or mismatched top-level path",
            ),
            SdistError::MetadataInvalid => upload_problem(
                422,
                "sdist_metadata_invalid",
                "Invalid PKG-INFO",
                "Exactly one bounded regular PKG-INFO is required",
            ),
        })?;
        let metadata = parse_core_metadata(&metadata_bytes.unwrap_or_default()).map_err(|_| {
            upload_problem(
                422,
                "sdist_metadata_missing",
                "PKG-INFO missing",
                "The source distribution must include valid core metadata",
            )
        })?;
        let project = normalize_pypi(metadata.get("Name"));
        let version_id = canonical_pypi_version(metadata.get("Version"));
        if project != normalize_pypi(&name)
            || version_id.is_empty()
            || version_id != canonical_pypi_version(&version)
        {
            return Err(upload_problem(
                422,
                "pypi_identity_mismatch",
                "Source distribution identity mismatch",
                "Filename and PKG-INFO must identify the same release",
            ));
        }
        Ok(PyPIDistribution {
            asset: asset.clone(),
            project,
            display_version: metadata.get("Version").to_string(),
            version_id,
            kind: "sdist".to_string(),
            requires_python: metadata.get("Requires-Python").to_string(),
            metadata_version: metadata.get("Metadata-Version").to_string(),
            wheel_tags: Vec::new(),
            content_type: "application/gzip".to_string(),
        })
    }
}

fn parse_core_metadata(value: &[u8]) -> Result<MailHeader, ()> {
    let header = parse_metadata_headers(value)?;
    if header.get("Name").is_empty() || header.get("Version").is_empty() {
        return Err(());
    }
    Ok(header)
}

fn parse_metadata_headers(value: &[u8]) -> Result<MailHeader, ()> {
    if value.is_empty() {
        return Err(());
    }
    let text = String::from_utf8_lossy(value);
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current: Option<(String, String)> = None;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            let Some((_, value)) = current.as_mut() else {
                return Err(());
            };
            value.push(' ');
            value.push_str(line.trim());
            continue;
        }
        if let Some((key, value)) = current.take() {
            out.entry(key).or_default().push(value);
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(());
        };
        if key.is_empty() || key.contains(' ') {
            return Err(());
        }
        current = Some((canonical_header_key(key), value.trim().to_string()));
    }
    if let Some((key, value)) = current.take() {
        out.entry(key).or_default().push(value);
    }
    if out.is_empty() {
        return Err(());
    }
    Ok(MailHeader(out))
}

fn canonical_header_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut upper = true;
    for c in key.chars() {
        if upper {
            out.extend(c.to_uppercase());
        } else {
            out.extend(c.to_lowercase());
        }
        upper = c == '-';
    }
    out
}

fn parse_legacy_zip_filename(filename: &str) -> Option<(String, String)> {
    if !filename.to_lowercase().ends_with(".zip") {
        return None;
    }
    split_name_version(&filename[..filename.len() - 4])
}

fn parse_sdist_filename(filename: &str) -> Option<(String, String)> {
    if !filename.to_lowercase().ends_with(".tar.gz") {
        return None;
    }
    split_name_version(&filename[..filename.len() - 7])
}

/// Splits a `name-version` stem on the right-most dash that yields a valid
/// project name and PEP 440 version.
fn split_name_version(stem: &str) -> Option<(String, String)> {
    let mut cut = stem.len();
    while let Some(index) = stem[..cut].rfind('-') {
        if index == 0 {
            return None;
        }
        let (name, version) = (&stem[..index], &stem[index + 1..]);
        if !normalize_pypi(name).is_empty() && !canonical_pypi_version(version).is_empty() {
            return Some((name.to_string(), version.to_string()));
        }
        cut = index;
    }
    None
}

fn parse_wheel_filename(filename: &str) -> Option<(String, String, Vec<String>)> {
    if !filename.to_lowercase().ends_with(".whl") {
        return None;
    }
    let parts: Vec<&str> = filename[..filename.len() - 4].split('-').collect();
    if parts.len() != 5 && parts.len() != 6 {
        return None;
    }
    let (name, version) = (parts[0], parts[1]);
    let tag_start = parts.len() - 3;
    let mut tags = Vec::new();
    for py in parts[tag_start].split('.') {
        for abi in parts[tag_start + 1].split('.') {
            for platform in parts[tag_start + 2].split('.') {
                tags.push(format!("{py}-{abi}-{platform}"));
            }
        }
    }
    tags.sort();
    if name.is_empty() || canonical_pypi_version(version).is_empty() {
        return None;
    }
    Some((name.replace('_', "-"), version.replace('_', "-"), tags))
}

/// Applies PEP 440 version canonicalisation.
pub(crate) fn canonical_pypi_version(version: &str) -> String {
    let value = version.trim();
    let Some(match_) = PYPI_VERSION_GRAMMAR.captures(value) else {
        return String::new();
    };
    let group = |i: usize| match_.get(i).map(|m| m.as_str()).unwrap_or("");
    let normal_number = |raw: &str| -> String {
        if raw.is_empty() {
            return "0".to_string();
        }
        raw.parse::<u64>().unwrap_or(0).to_string()
    };
    let mut release: Vec<String> = group(2).split('.').map(&normal_number).collect();
    while release.len() > 1 && release[release.len() - 1] == "0" {
        release.pop();
    }
    let mut canonical = release.join(".");
    if !group(1).is_empty() && normal_number(group(1)) != "0" {
        canonical = format!("{}!{canonical}", normal_number(group(1)));
    }
    if !group(3).is_empty() {
        let label = match group(3).to_lowercase().as_str() {
            "alpha" => "a".to_string(),
            "beta" => "b".to_string(),
            "c" | "pre" | "preview" => "rc".to_string(),
            other => other.to_string(),
        };
        canonical.push_str(&label);
        canonical.push_str(&normal_number(group(4)));
    }
    let post = if group(5).is_empty() && !group(6).is_empty() {
        group(7)
    } else {
        group(5)
    };
    if !group(5).is_empty() || !group(6).is_empty() {
        canonical.push_str(".post");
        canonical.push_str(&normal_number(post));
    }
    if !group(8).is_empty() {
        canonical.push_str(".dev");
        canonical.push_str(&normal_number(group(9)));
    }
    if !group(10).is_empty() {
        let lowered = group(10).to_lowercase();
        let parts: Vec<String> = LOCAL_SEGMENT_SPLIT
            .split(&lowered)
            .map(|part| {
                if part.parse::<u64>().is_ok() {
                    normal_number(part)
                } else {
                    part.to_string()
                }
            })
            .collect();
        canonical.push('+');
        canonical.push_str(&parts.join("."));
    }
    canonical
}

/// Why a legacy ZIP sdist could not be inspected.
enum ZipSdistError {
    ArchiveInvalid,
    Unsafe,
    RootMismatch,
    MetadataInvalid,
}

/// Reads the archive root and its `PKG-INFO` out of a legacy ZIP sdist.
/// Blocking: the caller runs it on the blocking pool.
fn read_legacy_zip_metadata(
    file: Box<dyn crate::storage::ReadSeekCloser>,
    max_entries: i64,
    max_meta_bytes: i64,
) -> Result<(String, Option<Vec<u8>>), ZipSdistError> {
    use std::io::Read as _;

    let mut archive = zip::ZipArchive::new(file).map_err(|_| ZipSdistError::ArchiveInvalid)?;
    if archive.len() as i64 > max_entries {
        return Err(ZipSdistError::ArchiveInvalid);
    }
    let mut root = String::new();
    let mut metadata: Option<Vec<u8>> = None;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|_| ZipSdistError::ArchiveInvalid)?;
        let name = entry.name().to_string();
        let clean = clean_archive_path(name.strip_prefix("./").unwrap_or(&name));
        if clean.is_empty() || clean.starts_with("../") || name.starts_with('/') {
            return Err(ZipSdistError::Unsafe);
        }
        let entry_root = clean.split('/').next().unwrap_or("").to_string();
        if root.is_empty() {
            root = entry_root;
        } else if root != entry_root {
            return Err(ZipSdistError::RootMismatch);
        }
        if clean != format!("{root}/PKG-INFO") {
            continue;
        }
        if metadata.is_some() || entry.is_dir() || entry.size() > max_meta_bytes as u64 {
            return Err(ZipSdistError::MetadataInvalid);
        }
        let mut bytes = Vec::new();
        entry
            .by_ref()
            .take(max_meta_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ZipSdistError::MetadataInvalid)?;
        if bytes.len() as i64 > max_meta_bytes {
            return Err(ZipSdistError::MetadataInvalid);
        }
        metadata = Some(bytes);
    }
    Ok((root, metadata))
}

/// Why a wheel could not be inspected.
#[allow(clippy::enum_variant_names)] // Names match the public problem codes.
enum WheelError {
    ArchiveInvalid,
    PathInvalid,
    MetadataInvalid,
}

/// Reads the three required `dist-info` documents out of a wheel. Blocking.
fn read_wheel_metadata(
    file: Box<dyn crate::storage::ReadSeekCloser>,
    wheel_prefix: &str,
    max_entries: i64,
    max_meta_bytes: i64,
) -> Result<BTreeMap<String, Vec<u8>>, WheelError> {
    use std::io::Read as _;

    let mut archive = zip::ZipArchive::new(file).map_err(|_| WheelError::ArchiveInvalid)?;
    if archive.len() as i64 > max_entries {
        return Err(WheelError::ArchiveInvalid);
    }
    let mut required: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut seen = std::collections::HashSet::new();
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|_| WheelError::ArchiveInvalid)?;
        let name = entry.name().to_string();
        let clean = clean_archive_path(&name);
        let folded = clean.to_lowercase();
        if clean.is_empty()
            || clean.starts_with("../")
            || clean.starts_with('/')
            || !seen.insert(folded)
        {
            return Err(WheelError::PathInvalid);
        }
        for required_name in ["METADATA", "WHEEL", "RECORD"] {
            if clean != format!("{wheel_prefix}{required_name}") {
                continue;
            }
            if entry.is_dir()
                || entry.size() > max_meta_bytes as u64
                || required.contains_key(required_name)
            {
                return Err(WheelError::MetadataInvalid);
            }
            let mut value = Vec::new();
            entry
                .by_ref()
                .take(max_meta_bytes as u64 + 1)
                .read_to_end(&mut value)
                .map_err(|_| WheelError::MetadataInvalid)?;
            if value.len() as i64 > max_meta_bytes {
                return Err(WheelError::MetadataInvalid);
            }
            required.insert(required_name.to_string(), value);
        }
    }
    Ok(required)
}

/// Why a source distribution could not be inspected.
#[allow(clippy::enum_variant_names)] // Names match the public problem codes.
enum SdistError {
    Invalid,
    PathInvalid,
    MetadataInvalid,
}

/// Reads the root `PKG-INFO` out of a gzipped source distribution. Blocking.
fn read_sdist_metadata<R: std::io::Read>(
    reader: R,
    root: &str,
    max_entries: i64,
    max_meta_bytes: i64,
) -> Result<Option<Vec<u8>>, SdistError> {
    use std::io::Read as _;

    let gz = flate2::read::GzDecoder::new(reader);
    let mut archive = tar::Archive::new(gz);
    let entries = archive.entries().map_err(|_| SdistError::Invalid)?;
    let mut metadata: Option<Vec<u8>> = None;
    let mut seen = std::collections::HashSet::new();
    let mut count = 0i64;
    for entry in entries {
        let mut entry = entry.map_err(|_| SdistError::Invalid)?;
        count += 1;
        let Ok(path) = entry.path() else {
            return Err(SdistError::PathInvalid);
        };
        let name = path.to_string_lossy().to_string();
        let clean = clean_archive_path(name.strip_prefix("./").unwrap_or(&name));
        let folded = clean.to_lowercase();
        if count > max_entries
            || clean.is_empty()
            || clean.starts_with("../")
            || !format!("{clean}/").starts_with(&format!("{root}/"))
            || !seen.insert(folded)
        {
            return Err(SdistError::PathInvalid);
        }
        if clean != format!("{root}/PKG-INFO") {
            continue;
        }
        let size = entry.header().size().unwrap_or(u64::MAX);
        if !entry.header().entry_type().is_file()
            || size > max_meta_bytes as u64
            || metadata.is_some()
        {
            return Err(SdistError::MetadataInvalid);
        }
        let mut bytes = Vec::new();
        entry
            .by_ref()
            .take(max_meta_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| SdistError::MetadataInvalid)?;
        if bytes.len() as i64 > max_meta_bytes {
            return Err(SdistError::MetadataInvalid);
        }
        metadata = Some(bytes);
    }
    Ok(metadata)
}

/// An entry that cleans to "." yields an empty string, which every caller treats as unsafe.
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
