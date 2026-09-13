//! The managed upload pipeline: bounded receive, format planning and the atomic
//! store batch shared by the browser/API upload route and the ecosystem-native
//! publishers.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{FromRequest, Multipart, Request};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha1::Digest as _;

use crate::config::UploadConfig;
use crate::meta::{
    self, Artifact, ArtifactPublication, ArtifactUploadRequest, Repository, Store,
    UPLOAD_COMMITTED, UPLOAD_CONFLICT, UPLOAD_FAILED, UPLOAD_RECEIVING, UploadRequestKey,
};

use super::{Engine, Manager};

/// An injectable clock, so planner tests can pin publication timestamps.
pub(crate) type UploadClock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;
/// An injectable upload-id generator.
pub(crate) type UploadIdFn = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// The versioned tagged-union request carried as the first multipart part.
/// Format-specific validators reject unused objects.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactUploadManifest {
    #[serde(default)]
    pub schema_version: i64,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub overwrite: bool,
    #[serde(default)]
    pub assets: Vec<ArtifactUploadAsset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maven: Option<MavenUploadManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pypi: Option<PyPIUploadManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm: Option<NPMUploadManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cargo: Option<CargoUploadManifest>,
    #[serde(default, rename = "go", skip_serializing_if = "Option::is_none")]
    pub go_: Option<GoUploadManifest>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactUploadAsset {
    #[serde(default)]
    pub part: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub extension: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub classifier: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MavenUploadManifest {
    #[serde(default)]
    pub group_id: String,
    #[serde(default)]
    pub artifact_id: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub generate_pom: bool,
    #[serde(default)]
    pub packaging: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PyPIUploadManifest {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NPMUploadManifest {
    #[serde(default)]
    pub dist_tag: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CargoUploadManifest {
    #[serde(default)]
    pub yanked: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoUploadManifest {
    #[serde(default)]
    pub module: String,
    #[serde(default)]
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub time: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UploadedArtifact {
    pub path: String,
    pub role: String,
    pub size: i64,
    pub sha256: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArtifactUploadResult {
    pub upload_id: String,
    pub repository: String,
    pub format: String,
    pub coordinate: String,
    // Result rows written by earlier releases store an empty list as `null`.
    #[serde(default, deserialize_with = "crate::repoconfig::null_default")]
    pub created: Vec<UploadedArtifact>,
    #[serde(default, deserialize_with = "crate::repoconfig::null_default")]
    pub replaced: Vec<UploadedArtifact>,
    #[serde(default, deserialize_with = "crate::repoconfig::null_default")]
    pub derived: Vec<UploadedArtifact>,
    pub scan_status: String,
    pub durability: String,
    #[serde(default, deserialize_with = "crate::repoconfig::null_default")]
    pub warnings: Vec<UploadWarning>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UploadWarning {
    pub code: String,
    pub detail: String,
}

/// Independent of HTTP so parser/planner tests do not need a router. The API
/// adapter writes it as `application/problem+json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UploadProblem {
    #[serde(rename = "type")]
    pub type_: String,
    pub title: String,
    pub status: i64,
    pub code: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub upload_id: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub field_errors: HashMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub conflict_action: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub retryable: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

pub(crate) fn upload_problem(
    status: i64,
    code: &str,
    title: &str,
    detail: &str,
) -> Box<UploadProblem> {
    Box::new(UploadProblem {
        type_: format!("https://forklift.dev/problems/{}", code.replace('_', "-")),
        title: title.to_string(),
        status,
        code: code.to_string(),
        detail: detail.to_string(),
        ..Default::default()
    })
}

/// What a format publisher returns.
pub(crate) type PublishResult = Result<ArtifactUploadResult, Box<UploadProblem>>;

/// One received asset, already streamed into the blob store.
#[derive(Debug, Clone, Default)]
pub(crate) struct StagedUploadAsset {
    pub(crate) spec: ArtifactUploadAsset,
    pub(crate) filename: String,
    pub(crate) digest: String,
    pub(crate) sha1: String,
    pub(crate) sha512: String,
    pub(crate) size: i64,
}

/// Coordinates bounded receive, format planning and the atomic store batch. The
/// global feature gate is enforced by the API; direct tests may call
/// [`Uploader::receive`] while disabled to exercise the complete upload pipeline
/// in isolation.
pub struct Uploader {
    pub(crate) engine: Arc<Engine>,
    pub(crate) store: Arc<Store>,
    pub(crate) cfg: UploadConfig,
    pub(crate) durability: parking_lot::RwLock<String>,
    pub(crate) now: parking_lot::RwLock<UploadClock>,
    pub(crate) new_id: parking_lot::RwLock<UploadIdFn>,
    /// Bounds concurrent uploads instance-wide; `users` bounds them per principal.
    gate: Arc<tokio::sync::Semaphore>,
    users: parking_lot::Mutex<HashMap<String, i64>>,
    scan_enabled: parking_lot::RwLock<bool>,
    /// When set, invoked once per committed publication so the manager can apply
    /// the same package-approval quarantine that registry publishes and browser
    /// uploads receive.
    ///
    on_publish: parking_lot::RwLock<Option<Arc<Manager>>>,
}

impl Uploader {
    pub fn new(engine: Arc<Engine>, cfg: UploadConfig) -> Arc<Uploader> {
        let store = Arc::clone(&engine.store);
        let permits = cfg.max_concurrent.max(1) as usize;
        Arc::new(Uploader {
            engine,
            store,
            cfg,
            durability: parking_lot::RwLock::new("local".to_string()),
            now: parking_lot::RwLock::new(Arc::new(Utc::now)),
            new_id: parking_lot::RwLock::new(Arc::new(random_upload_id)),
            gate: Arc::new(tokio::sync::Semaphore::new(permits)),
            users: parking_lot::Mutex::new(HashMap::new()),
            scan_enabled: parking_lot::RwLock::new(false),
            on_publish: parking_lot::RwLock::new(None),
        })
    }

    /// Marks successful uploads as subject to the configured asynchronous
    /// metadata replication RPO. Blob persistence and metadata durability are
    /// deliberately reported separately from HTTP success.
    pub fn set_async_durability(&self, async_: bool) {
        *self.durability.write() = if async_ { "async" } else { "local" }.to_string();
    }

    /// Keeps upload responses honest about whether a vulnerability job can
    /// actually be queued after the publication commits.
    pub fn set_scan_enabled(&self, enabled: bool) {
        *self.scan_enabled.write() = enabled;
    }

    /// Registers the manager whose `quarantine_uploaded_publication` runs after
    /// each publication commits, subjecting uploaded packages to the approval
    /// workflow.
    pub(crate) fn set_publish_hook(&self, manager: Arc<Manager>) {
        *self.on_publish.write() = Some(manager);
    }

    /// Signals a freshly committed publication to the publish hook.
    pub(crate) async fn notify_published(
        &self,
        repository: &Repository,
        publication: &ArtifactPublication,
        username: &str,
    ) {
        let hook = self.on_publish.read().clone();
        if let Some(manager) = hook
            && !publication.package_name.is_empty()
        {
            manager
                .quarantine_uploaded_publication(
                    repository,
                    &publication.package_name,
                    &publication.version,
                    username,
                )
                .await;
        }
    }

    pub(crate) fn scan_status(&self) -> String {
        if *self.scan_enabled.read() {
            "queued".to_string()
        } else {
            "disabled".to_string()
        }
    }

    pub(crate) fn durability_value(&self) -> String {
        self.durability.read().clone()
    }

    pub(crate) fn now(&self) -> DateTime<Utc> {
        (self.now.read())()
    }

    pub(crate) fn record_committed(&self, repository: &Repository, artifacts: &[Artifact]) {
        for artifact in artifacts {
            if artifact.artifact_role != "primary" && artifact.artifact_role != "metadata" {
                continue;
            }
            self.engine
                .bytes
                .with_label_values(&["ingress", &repository.format])
                .inc_by(artifact.size as f64);
            let hook = self.engine.on_store.read().clone();
            if let Some(on_store) = hook {
                on_store(repository.clone(), artifact.path.clone());
            }
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    pub fn supports(&self, format: &str) -> bool {
        matches!(
            format,
            meta::FORMAT_MAVEN
                | meta::FORMAT_NPM
                | meta::FORMAT_PYPI
                | meta::FORMAT_CARGO
                | meta::FORMAT_GO
        )
    }

    pub fn max_request_bytes(&self) -> i64 {
        self.cfg.max_batch_bytes + self.cfg.max_manifest_bytes + self.cfg.max_field_bytes
    }

    /// Streams and publishes one request. The `replay` flag is true only for a
    /// durable committed idempotency replay, in which case `body` is not
    /// consumed.
    #[allow(clippy::too_many_arguments)] // Domain operation parameters.
    pub async fn receive(
        self: &Arc<Self>,
        repository: &Repository,
        principal_name: &str,
        principal_source: &str,
        idempotency_key: &str,
        content_type: &str,
        body: Body,
    ) -> ReceiveOutcome {
        self.receive_inner(
            repository,
            principal_name,
            principal_source,
            idempotency_key,
            content_type,
            body,
            false,
        )
        .await
    }

    /// Permits Maven replacement only when the API adapter has already verified
    /// repository-scoped delete permission for this principal.
    #[allow(clippy::too_many_arguments)] // Domain operation parameters.
    pub async fn receive_authorized(
        self: &Arc<Self>,
        repository: &Repository,
        principal_name: &str,
        principal_source: &str,
        idempotency_key: &str,
        content_type: &str,
        body: Body,
        allow_maven_replace: bool,
    ) -> ReceiveOutcome {
        self.receive_inner(
            repository,
            principal_name,
            principal_source,
            idempotency_key,
            content_type,
            body,
            allow_maven_replace,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)] // Domain operation parameters.
    async fn receive_inner(
        self: &Arc<Self>,
        repository: &Repository,
        principal_name: &str,
        principal_source: &str,
        idempotency_key: &str,
        content_type: &str,
        body: Body,
        allow_maven_replace: bool,
    ) -> ReceiveOutcome {
        match tokio::time::timeout(
            self.cfg.max_duration,
            self.receive_bounded(
                repository,
                principal_name,
                principal_source,
                idempotency_key,
                content_type,
                body,
                allow_maven_replace,
            ),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => ReceiveOutcome::problem(upload_problem(
                503,
                "storage_unavailable",
                "Upload unavailable",
                "The upload exceeded the configured time budget",
            )),
        }
    }

    #[allow(clippy::too_many_arguments)] // Domain operation parameters.
    async fn receive_bounded(
        self: &Arc<Self>,
        repository: &Repository,
        principal_name: &str,
        principal_source: &str,
        idempotency_key: &str,
        content_type: &str,
        body: Body,
        allow_maven_replace: bool,
    ) -> ReceiveOutcome {
        if repository.r#type != meta::TYPE_HOSTED {
            return ReceiveOutcome::problem(upload_problem(
                400,
                "repository_not_hosted",
                "Hosted repository required",
                "Artifacts can be uploaded only to hosted repositories",
            ));
        }
        if repository.disabled {
            return ReceiveOutcome::problem(upload_problem(
                503,
                "repository_disabled",
                "Repository disabled",
                "The repository is currently disabled",
            ));
        }
        if let Some(problem) = validate_idempotency_key(idempotency_key) {
            return ReceiveOutcome::problem(problem);
        }
        let Some(_slot) = self
            .acquire(&format!("{principal_source}:{principal_name}"))
            .await
        else {
            return ReceiveOutcome::problem(upload_problem(
                429,
                "upload_busy",
                "Upload capacity exhausted",
                "Retry after an upload slot becomes available",
            ));
        };

        let Some(upload_id) = (self.new_id.read())() else {
            return ReceiveOutcome::problem(upload_problem(
                503,
                "storage_unavailable",
                "Upload unavailable",
                "Could not allocate an upload identifier",
            ));
        };
        let key = UploadRequestKey {
            repo_id: repository.id,
            principal_source: principal_source.to_string(),
            principal_name: principal_name.to_string(),
            idempotency_key: idempotency_key.to_string(),
        };
        let request = ArtifactUploadRequest {
            idempotency_key: idempotency_key.to_string(),
            repo_id: repository.id,
            principal_name: principal_name.to_string(),
            principal_source: principal_source.to_string(),
            upload_id: upload_id.clone(),
            state: UPLOAD_RECEIVING.to_string(),
            expires_at: self.now() + self.idempotency_ttl(),
            ..Default::default()
        };
        if let Err(err) = self.store.create_upload_request(request.clone()).await {
            if !matches!(err, meta::Error::Conflict) {
                return ReceiveOutcome::problem(upload_problem(
                    503,
                    "storage_unavailable",
                    "Upload unavailable",
                    "Could not reserve the idempotency key",
                ));
            }
            let Ok(existing) = self.store.get_upload_request(key.clone()).await else {
                return ReceiveOutcome::problem(upload_problem(
                    503,
                    "storage_unavailable",
                    "Upload unavailable",
                    "Could not recover the idempotent upload",
                ));
            };
            return self.replay_outcome(existing);
        }

        let outcome = self
            .publish(
                repository,
                request,
                key,
                &upload_id,
                content_type,
                body,
                allow_maven_replace,
            )
            .await;
        if outcome.problem.is_some() {
            let _ = self
                .store
                .set_upload_state(
                    &upload_id,
                    UPLOAD_RECEIVING,
                    UPLOAD_FAILED,
                    "",
                    self.now() + self.idempotency_ttl(),
                )
                .await;
        }
        outcome
    }

    /// Renders the outcome for an idempotency key that is already taken.
    fn replay_outcome(&self, existing: ArtifactUploadRequest) -> ReceiveOutcome {
        match existing.state.as_str() {
            UPLOAD_COMMITTED => {
                match serde_json::from_str::<ArtifactUploadResult>(&existing.result_json) {
                    Ok(result) => ReceiveOutcome {
                        result,
                        replay: true,
                        problem: None,
                    },
                    Err(_) => ReceiveOutcome::problem(upload_problem(
                        503,
                        "stored_result_invalid",
                        "Stored result unavailable",
                        "The committed upload result could not be decoded",
                    )),
                }
            }
            UPLOAD_CONFLICT | UPLOAD_RECEIVING => {
                // A stored conflict plan carries the original problem verbatim.
                if existing.state == UPLOAD_CONFLICT
                    && let Ok(stored) = serde_json::from_str::<
                        super::uiupload_maven::MavenConflictPlan,
                    >(&existing.plan_json)
                    && !stored.problem.code.is_empty()
                {
                    return ReceiveOutcome::problem(Box::new(stored.problem));
                }
                let mut p = upload_problem(
                    409,
                    "upload_in_progress",
                    "Upload already in progress",
                    "The idempotency key is already active",
                );
                p.upload_id = existing.upload_id;
                ReceiveOutcome::problem(p)
            }
            _ => {
                let mut p = upload_problem(
                    409,
                    "idempotency_key_consumed",
                    "Idempotency key already used",
                    "Use a new idempotency key for a new attempt",
                );
                p.upload_id = existing.upload_id;
                ReceiveOutcome::problem(p)
            }
        }
    }

    /// Receives the multipart body and hands it to the format publisher.
    #[allow(clippy::too_many_arguments)] // Domain operation parameters.
    async fn publish(
        self: &Arc<Self>,
        repository: &Repository,
        request: ArtifactUploadRequest,
        key: UploadRequestKey,
        upload_id: &str,
        content_type: &str,
        body: Body,
        allow_maven_replace: bool,
    ) -> ReceiveOutcome {
        // Held from the staging writes through the batch commit so the sweeper
        // cannot reclaim a staged blob before its artifact row exists.
        let _gc = self.engine.gc_mu.read().await;
        let (manifest, staged) = match self.receive_multipart(content_type, body).await {
            Ok(v) => v,
            Err(mut problem) => {
                problem.upload_id = upload_id.to_string();
                return ReceiveOutcome::problem(problem);
            }
        };
        if manifest.format != repository.format {
            let mut p = upload_problem(
                422,
                "format_mismatch",
                "Repository format mismatch",
                "Manifest format does not match the target repository",
            );
            p.upload_id = upload_id.to_string();
            return ReceiveOutcome::problem(p);
        }

        let outcome = match manifest.format.as_str() {
            meta::FORMAT_MAVEN => {
                self.publish_maven(
                    repository,
                    &request,
                    key,
                    &manifest,
                    &staged,
                    allow_maven_replace,
                )
                .await
            }
            meta::FORMAT_NPM => {
                self.publish_npm(repository, &request, key, &manifest, &staged)
                    .await
            }
            meta::FORMAT_PYPI => {
                self.publish_pypi(repository, &request, key, &manifest, &staged)
                    .await
            }
            meta::FORMAT_CARGO => {
                self.publish_cargo(repository, &request, key, &manifest, &staged)
                    .await
            }
            meta::FORMAT_GO => {
                self.publish_go(repository, &request, key, &manifest, &staged)
                    .await
            }
            _ => Err(upload_problem(
                422,
                "unsupported_format",
                "Unsupported upload format",
                "This repository format does not support managed artifact uploads",
            )),
        };
        match outcome {
            Ok(result) => ReceiveOutcome {
                result,
                replay: false,
                problem: None,
            },
            Err(mut problem) => {
                problem.upload_id = upload_id.to_string();
                ReceiveOutcome::problem(problem)
            }
        }
    }

    pub(crate) fn idempotency_ttl(&self) -> chrono::TimeDelta {
        chrono::TimeDelta::from_std(self.cfg.idempotency_ttl).unwrap_or(chrono::TimeDelta::MAX)
    }

    async fn acquire(self: &Arc<Self>, user: &str) -> Option<UploadSlot> {
        let permit = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            Arc::clone(&self.gate).acquire_owned(),
        )
        .await
        .ok()?
        .ok()?;
        {
            let mut users = self.users.lock();
            let count = users.entry(user.to_string()).or_insert(0);
            if *count >= self.cfg.max_concurrent_user {
                users.remove(user);
                return None;
            }
            *count += 1;
        }
        Some(UploadSlot {
            uploader: Arc::clone(self),
            user: user.to_string(),
            _permit: permit,
        })
    }

    async fn receive_multipart(
        &self,
        content_type: &str,
        body: Body,
    ) -> Result<(ArtifactUploadManifest, Vec<StagedUploadAsset>), Box<UploadProblem>> {
        let multipart_invalid = || {
            upload_problem(
                400,
                "multipart_invalid",
                "Invalid multipart upload",
                "Content-Type must be multipart/form-data with a boundary",
            )
        };
        let media_type: mime::Mime = content_type.parse().map_err(|_| multipart_invalid())?;
        if media_type.essence_str() != "multipart/form-data"
            || media_type.get_param(mime::BOUNDARY).is_none()
        {
            return Err(multipart_invalid());
        }
        let request = Request::builder()
            .method(http::Method::POST)
            .uri("/")
            .header(http::header::CONTENT_TYPE, content_type)
            .body(body)
            .map_err(|_| multipart_invalid())?;
        let mut multipart = Multipart::from_request(request, &())
            .await
            .map_err(|_| multipart_invalid())?;

        let manifest_first = || {
            upload_problem(
                400,
                "manifest_first",
                "Manifest must be first",
                "The first multipart part must be the manifest field",
            )
        };
        let Ok(Some(mut part)) = multipart.next_field().await else {
            return Err(manifest_first());
        };
        if part.name() != Some("manifest") || part.file_name().is_some() {
            return Err(manifest_first());
        }
        let manifest_bytes = read_at_most(&mut part, self.cfg.max_manifest_bytes)
            .await
            .map_err(|_| {
                upload_problem(
                    413,
                    "manifest_too_large",
                    "Manifest too large",
                    "The upload manifest exceeds 64 KiB",
                )
            })?;
        // `Multipart::next_field` refuses to advance while the previous field is still alive,
        // so the manifest part is released here.
        drop(part);
        if std::str::from_utf8(&manifest_bytes).is_err() {
            return Err(upload_problem(
                400,
                "manifest_invalid",
                "Invalid manifest",
                "The upload manifest must be UTF-8 JSON",
            ));
        }
        let manifest_invalid =
            |detail: &str| upload_problem(400, "manifest_invalid", "Invalid manifest", detail);
        let mut de = serde_json::Deserializer::from_slice(&manifest_bytes);
        let manifest = ArtifactUploadManifest::deserialize(&mut de)
            .map_err(|_| manifest_invalid("The upload manifest does not match schema version 1"))?;
        if de.end().is_err() || manifest.schema_version != 1 {
            return Err(manifest_invalid(
                "Exactly one schema version 1 manifest is required",
            ));
        }
        if manifest.assets.is_empty() || manifest.assets.len() as i64 > self.cfg.max_assets {
            return Err(upload_problem(
                413,
                "asset_count_exceeded",
                "Invalid asset count",
                "The upload contains an unsupported number of assets",
            ));
        }

        let mut staged = Vec::with_capacity(manifest.assets.len());
        let mut seen = std::collections::HashSet::new();
        let mut total = 0i64;
        for asset in &manifest.assets {
            if asset.part.is_empty() || !seen.insert(asset.part.clone()) {
                return Err(upload_problem(
                    400,
                    "asset_part_invalid",
                    "Invalid asset parts",
                    "Every declared asset part must be unique and non-empty",
                ));
            }
            let asset_order_invalid = || {
                upload_problem(
                    400,
                    "asset_order_invalid",
                    "Invalid asset order",
                    "File parts must exactly follow manifest.assets order",
                )
            };
            let Ok(Some(mut next)) = multipart.next_field().await else {
                return Err(asset_order_invalid());
            };
            let filename = next.file_name().unwrap_or("").to_string();
            if next.name() != Some(asset.part.as_str()) || filename.is_empty() {
                return Err(asset_order_invalid());
            }
            if next.content_type() != Some("application/octet-stream")
                || !safe_upload_filename(&filename)
            {
                return Err(upload_problem(
                    400,
                    "asset_header_invalid",
                    "Invalid asset",
                    "Every asset needs a safe filename and application/octet-stream content type",
                ));
            }
            let limit = if manifest.format == meta::FORMAT_GO && asset.spec_role() == "zip" {
                self.cfg.go_max_zip_bytes
            } else {
                self.cfg.max_file_bytes
            };
            let storage_unavailable = |detail: &str| {
                upload_problem(503, "storage_unavailable", "Upload storage failed", detail)
            };
            let (digest, size, sha1, sha512) = self
                .stage_asset(&mut next, limit)
                .await
                .map_err(|_| storage_unavailable("The artifact bytes could not be staged"))?;
            self.store
                .ensure_blob(&digest, size)
                .await
                .map_err(|_| storage_unavailable("The staged artifact could not be recorded"))?;
            if size > limit {
                return Err(upload_problem(
                    413,
                    "file_too_large",
                    "Artifact too large",
                    "An artifact exceeds the configured per-file limit",
                ));
            }
            total += size;
            if total > self.cfg.max_batch_bytes {
                return Err(upload_problem(
                    413,
                    "batch_too_large",
                    "Upload too large",
                    "The upload exceeds the configured aggregate limit",
                ));
            }
            staged.push(StagedUploadAsset {
                spec: asset.clone(),
                filename,
                digest,
                sha1,
                sha512,
                size,
            });
        }
        if multipart.next_field().await.ok().flatten().is_some() {
            return Err(upload_problem(
                400,
                "multipart_trailing_part",
                "Unexpected multipart field",
                "The request must end after the last declared asset",
            ));
        }
        Ok((manifest, staged))
    }

    /// Streams one asset part into the blob store while hashing it, returning
    /// the digest, size and the SHA-1/SHA-512 checksums Maven and PyPI publish
    /// alongside the artifact.
    ///
    /// A multipart field borrows the request, and the blob store takes an owned `'static`
    /// reader, so the bytes are pumped through an in-memory pipe while the hashers observe
    /// them.
    async fn stage_asset(
        &self,
        field: &mut axum::extract::multipart::Field<'_>,
        limit: i64,
    ) -> Result<(String, i64, String, String), std::io::Error> {
        let (mut sink, source) = tokio::io::duplex(64 * 1024);
        let limited = tokio::io::AsyncReadExt::take(source, (limit + 1) as u64);
        let store_blob = self.engine.blobs.put(Box::pin(limited));
        let mut sha1_hash = sha1::Sha1::new();
        let mut sha512_hash = sha2::Sha512::new();
        let pump = async {
            use tokio::io::AsyncWriteExt as _;
            let mut written = 0i64;
            while let Ok(Some(chunk)) = field.chunk().await {
                if written > limit {
                    break;
                }
                sha1_hash.update(&chunk);
                sha512_hash.update(&chunk);
                written += chunk.len() as i64;
                if sink.write_all(&chunk).await.is_err() {
                    break;
                }
            }
            let _ = sink.shutdown().await;
        };
        let (stored, ()) = tokio::join!(store_blob, pump);
        let (digest, size) = stored.map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok((
            digest,
            size,
            hex::encode(sha1_hash.finalize()),
            base64::engine::general_purpose::STANDARD.encode(sha512_hash.finalize()),
        ))
    }
}

impl ArtifactUploadAsset {
    fn spec_role(&self) -> &str {
        &self.role
    }
}

#[derive(Debug, Default)]
pub struct ReceiveOutcome {
    pub result: ArtifactUploadResult,
    pub replay: bool,
    pub problem: Option<UploadProblem>,
}

impl ReceiveOutcome {
    fn problem(problem: Box<UploadProblem>) -> ReceiveOutcome {
        ReceiveOutcome {
            result: ArtifactUploadResult::default(),
            replay: false,
            problem: Some(*problem),
        }
    }
}

/// Releases the instance-wide and per-principal upload slots on drop.
struct UploadSlot {
    uploader: Arc<Uploader>,
    user: String,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for UploadSlot {
    fn drop(&mut self) {
        let mut users = self.uploader.users.lock();
        if let Some(count) = users.get_mut(&self.user) {
            *count -= 1;
            if *count <= 0 {
                users.remove(&self.user);
            }
        }
    }
}

pub(crate) fn validate_idempotency_key(key: &str) -> Option<Box<UploadProblem>> {
    let invalid = || {
        Some(upload_problem(
            400,
            "idempotency_key_invalid",
            "Invalid idempotency key",
            "Idempotency-Key must contain 16 to 128 printable ASCII characters",
        ))
    };
    if key.len() < 16 || key.len() > 128 {
        return invalid();
    }
    if key.bytes().any(|c| !(0x21..=0x7e).contains(&c)) {
        return invalid();
    }
    None
}

pub(crate) fn safe_upload_filename(filename: &str) -> bool {
    if filename.is_empty() || filename.contains('/') || filename.contains('\\') {
        return false;
    }
    !filename
        .chars()
        .any(|r| r == '\0' || (r as u32) < 0x20 || r as u32 == 0x7f)
}

/// Reads a multipart field whole, refusing anything over `limit`.
async fn read_at_most(
    field: &mut axum::extract::multipart::Field<'_>,
    limit: i64,
) -> Result<Vec<u8>, ()> {
    let mut out = Vec::new();
    while let Ok(Some(chunk)) = field.chunk().await {
        out.extend_from_slice(&chunk);
        if out.len() as i64 > limit {
            return Err(());
        }
    }
    Ok(out)
}

pub(crate) fn random_upload_id() -> Option<String> {
    let mut value = [0u8; 16];
    rand::fill(&mut value);
    Some(hex::encode(value))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::body::Body;
    use chrono::{TimeZone as _, Utc};
    use tokio::io::AsyncReadExt as _;

    use crate::config::UploadConfig;
    use crate::meta::{self, Repository, Store};
    use crate::storage::FsStore;

    use crate::repo::uiupload::{
        ArtifactUploadAsset, ArtifactUploadManifest, CargoUploadManifest, GoUploadManifest,
        MavenUploadManifest, NPMUploadManifest, PyPIUploadManifest, ReceiveOutcome, Uploader,
    };
    use base64::Engine as _;

    use crate::repo::router::Resolved;
    use crate::repo::uiupload_cargo::cargo_sparse_path;
    use crate::repo::uiupload_maven::valid_maven_metadata_xml;
    use crate::repo::uiupload_pypi::canonical_pypi_version;
    use crate::repo::{Engine, Manager, uiupload_go::GoInfo};

    /// The uploader under test with the store behind it. The temporary directories
    /// are held so the database and blob files outlive the test body.
    pub(crate) struct UploadHarness {
        pub(crate) uploader: Arc<Uploader>,
        pub(crate) store: Arc<Store>,
        pub(crate) repository: Repository,
        _db_dir: tempfile::TempDir,
        _blob_dir: tempfile::TempDir,
    }

    pub(crate) async fn new_upload_test_harness() -> UploadHarness {
        // `Engine::new` builds reqwest clients, which need a crypto provider.
        crate::server::install_crypto_provider();
        let db_dir = tempfile::tempdir().expect("temp dir");
        let blob_dir = tempfile::tempdir().expect("temp dir");
        let store = Arc::new(
            Store::open(db_dir.path().join("upload.db"))
                .await
                .expect("open store"),
        );
        let blobs = Arc::new(FsStore::new(blob_dir.path()).expect("blob store"));
        let engine = Engine::new(Arc::clone(&store), blobs, &prometheus::Registry::new());
        let uploader = Uploader::new(
            engine,
            UploadConfig {
                enabled: true,
                max_duration: std::time::Duration::from_secs(30 * 60),
                max_concurrent: 4,
                max_concurrent_user: 2,
                max_assets: 16,
                max_manifest_bytes: 64 << 10,
                max_field_bytes: 1 << 20,
                max_file_bytes: 256 << 20,
                max_batch_bytes: 512 << 20,
                go_max_zip_bytes: 500 << 20,
                archive_max_entries: 100_000,
                archive_max_meta_bytes: 16 << 20,
                idempotency_ttl: std::time::Duration::from_secs(24 * 60 * 60),
            },
        );
        let fixed = Utc.with_ymd_and_hms(2026, 7, 22, 12, 34, 56).unwrap();
        *uploader.now.write() = Arc::new(move || fixed);
        let next_id = Arc::new(AtomicUsize::new(0));
        *uploader.new_id.write() = Arc::new(move || {
            let n = next_id.fetch_add(1, Ordering::SeqCst) + 1;
            Some(format!("test-id-{n:024}"))
        });
        let repository = hosted_repo(&store, "maven-local", meta::FORMAT_MAVEN).await;
        UploadHarness {
            uploader,
            store,
            repository,
            _db_dir: db_dir,
            _blob_dir: blob_dir,
        }
    }

    pub(crate) async fn hosted_repo(store: &Arc<Store>, name: &str, format: &str) -> Repository {
        store
            .create_repository(Repository {
                name: name.to_string(),
                format: format.to_string(),
                r#type: meta::TYPE_HOSTED.to_string(),
                ..Default::default()
            })
            .await
            .expect("create repository")
    }

    /// One multipart part: a form field or a file.
    pub(crate) enum Part {
        Field(&'static str, String),
        File(&'static str, String, Vec<u8>),
    }

    pub(crate) fn multipart_body(parts: Vec<Part>) -> (String, Vec<u8>) {
        let boundary = "forklifttestboundary";
        let mut body: Vec<u8> = Vec::new();
        for part in parts {
            let (name, filename, value) = match part {
                Part::Field(name, value) => (name, None, value.into_bytes()),
                Part::File(name, filename, value) => (name, Some(filename), value),
            };
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match filename {
            Some(filename) => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
                )
                .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            ),
        }
            body.extend_from_slice(&value);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        (format!("multipart/form-data; boundary={boundary}"), body)
    }

    /// Builds a minimal but valid Go module ZIP, for publication tests.
    pub(crate) fn go_module_zip(module_path: &str, version: &str) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, value) in [
            (
                format!("{module_path}@{version}/go.mod"),
                format!("module {module_path}\n\ngo 1.23\n"),
            ),
            (
                format!("{module_path}@{version}/widget.go"),
                "package widget\n".to_string(),
            ),
        ] {
            writer.start_file(name, options).expect("zip entry");
            writer.write_all(value.as_bytes()).expect("zip write");
        }
        writer.finish().expect("zip finish").into_inner()
    }

    pub(crate) fn go_upload_body(module_path: &str, version: &str) -> (String, Vec<u8>) {
        let manifest = ArtifactUploadManifest {
            schema_version: 1,
            format: meta::FORMAT_GO.to_string(),
            assets: vec![ArtifactUploadAsset {
                part: "asset0".to_string(),
                role: "zip".to_string(),
                ..Default::default()
            }],
            go_: Some(GoUploadManifest {
                module: module_path.to_string(),
                version: version.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        multipart_body(vec![
            Part::Field(
                "manifest",
                serde_json::to_string(&manifest).expect("manifest json"),
            ),
            Part::File(
                "asset0",
                "module.zip".to_string(),
                go_module_zip(module_path, version),
            ),
        ])
    }

    /// Reads a stored blob whole.
    pub(crate) async fn blob_bytes(uploader: &Arc<Uploader>, digest: &str) -> Vec<u8> {
        let (mut reader, _) = uploader.engine.blobs.open(digest).await.expect("open blob");
        let mut value = Vec::new();
        reader.read_to_end(&mut value).await.expect("read blob");
        value
    }

    pub(crate) async fn receive(
        uploader: &Arc<Uploader>,
        repository: &Repository,
        key: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> ReceiveOutcome {
        uploader
            .receive(
                repository,
                "alice",
                meta::SOURCE_LOCAL,
                key,
                content_type,
                Body::from(body),
            )
            .await
    }

    #[tokio::test]
    async fn go_ui_upload_builds_proxy_triplet_and_indexes() {
        let h = new_upload_test_harness().await;
        let repository = hosted_repo(&h.store, "go-local", meta::FORMAT_GO).await;
        let (content_type, body) = go_upload_body("example.com/acme/widget", "v1.2.3");
        let outcome = receive(
            &h.uploader,
            &repository,
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            &content_type,
            body,
        )
        .await;
        assert!(outcome.problem.is_none(), "problem={:?}", outcome.problem);
        assert_eq!(outcome.result.coordinate, "example.com/acme/widget@v1.2.3");
        assert_eq!(outcome.result.created.len(), 3);

        let base = "example.com/acme/widget/@v/v1.2.3";
        for suffix in [".zip", ".mod", ".info"] {
            h.store
                .get_artifact(repository.id, &format!("{base}{suffix}"))
                .await
                .unwrap_or_else(|e| panic!("missing {suffix}: {e}"));
        }
        let list_artifact = h
            .store
            .get_artifact(repository.id, "example.com/acme/widget/@v/list")
            .await
            .expect("list artifact");
        let list = blob_bytes(&h.uploader, &list_artifact.blob_sha256).await;
        assert_eq!(String::from_utf8_lossy(&list), "v1.2.3\n");

        let latest_artifact = h
            .store
            .get_artifact(repository.id, "example.com/acme/widget/@latest")
            .await
            .expect("latest artifact");
        let latest: GoInfo =
            serde_json::from_slice(&blob_bytes(&h.uploader, &latest_artifact.blob_sha256).await)
                .expect("decode @latest");
        assert_eq!(latest.version, "v1.2.3");
    }

    #[tokio::test]
    async fn go_two_versions_updates_list_and_latest() {
        let h = new_upload_test_harness().await;
        let repository = hosted_repo(&h.store, "go-multi", meta::FORMAT_GO).await;
        for (index, version) in ["v1.0.0", "v1.1.0"].iter().enumerate() {
            let (content_type, body) = go_upload_body("example.com/acme/widget", version);
            let key = format!("d0000000-0000-4000-8000-00000000000{}", index + 1);
            let outcome = receive(&h.uploader, &repository, &key, &content_type, body).await;
            assert!(
                outcome.problem.is_none(),
                "publish {version}: {:?}",
                outcome.problem
            );
        }
        // The @v/list now carries both versions and @latest resolves to the newest.
        let list_artifact = h
            .store
            .get_artifact(repository.id, "example.com/acme/widget/@v/list")
            .await
            .expect("list artifact");
        let list = blob_bytes(&h.uploader, &list_artifact.blob_sha256).await;
        assert_eq!(String::from_utf8_lossy(&list), "v1.0.0\nv1.1.0\n");

        let latest_artifact = h
            .store
            .get_artifact(repository.id, "example.com/acme/widget/@latest")
            .await
            .expect("latest artifact");
        let latest: GoInfo =
            serde_json::from_slice(&blob_bytes(&h.uploader, &latest_artifact.blob_sha256).await)
                .expect("decode @latest");
        assert_eq!(latest.version, "v1.1.0");
    }

    pub(crate) fn maven_upload_body(version: &str, jar: &[u8]) -> (String, Vec<u8>) {
        let manifest = ArtifactUploadManifest {
            schema_version: 1,
            format: meta::FORMAT_MAVEN.to_string(),
            assets: vec![ArtifactUploadAsset {
                part: "asset0".to_string(),
                extension: "jar".to_string(),
                ..Default::default()
            }],
            maven: Some(MavenUploadManifest {
                group_id: "com.acme".to_string(),
                artifact_id: "widget".to_string(),
                version: version.to_string(),
                generate_pom: true,
                packaging: "jar".to_string(),
            }),
            ..Default::default()
        };
        multipart_body(vec![
            Part::Field(
                "manifest",
                serde_json::to_string(&manifest).expect("manifest json"),
            ),
            Part::File("asset0", "local-build.jar".to_string(), jar.to_vec()),
        ])
    }

    fn maven_plugin_upload_body(
        artifact_id: &str,
        version: &str,
        prefix: &str,
    ) -> (String, Vec<u8>) {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer
            .start_file("META-INF/maven/plugin.xml", options)
            .expect("zip entry");
        writer
        .write_all(
            format!(
                "<plugin><name>Test Plugin</name><groupId>com.acme</groupId><artifactId>{artifact_id}</artifactId><version>{version}</version><goalPrefix>{prefix}</goalPrefix></plugin>"
            )
            .as_bytes(),
        )
        .expect("zip write");
        let jar = writer.finish().expect("zip finish").into_inner();
        let manifest = ArtifactUploadManifest {
            schema_version: 1,
            format: meta::FORMAT_MAVEN.to_string(),
            assets: vec![ArtifactUploadAsset {
                part: "asset0".to_string(),
                extension: "jar".to_string(),
                ..Default::default()
            }],
            maven: Some(MavenUploadManifest {
                group_id: "com.acme".to_string(),
                artifact_id: artifact_id.to_string(),
                version: version.to_string(),
                generate_pom: true,
                packaging: "maven-plugin".to_string(),
            }),
            ..Default::default()
        };
        multipart_body(vec![
            Part::Field(
                "manifest",
                serde_json::to_string(&manifest).expect("manifest json"),
            ),
            Part::File("asset0", "plugin.jar".to_string(), jar),
        ])
    }

    pub(crate) fn gzipped_tar(name: &str, value: &[u8]) -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_path(name).expect("tar path");
        header.set_size(value.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder.append(&header, value).expect("tar entry");
        builder
            .into_inner()
            .expect("tar finish")
            .finish()
            .expect("gzip finish")
    }

    pub(crate) fn npm_upload_body(package_json: &str, tag: &str) -> (String, Vec<u8>) {
        let manifest = ArtifactUploadManifest {
            schema_version: 1,
            format: meta::FORMAT_NPM.to_string(),
            assets: vec![ArtifactUploadAsset {
                part: "asset0".to_string(),
                ..Default::default()
            }],
            npm: Some(NPMUploadManifest {
                dist_tag: tag.to_string(),
            }),
            ..Default::default()
        };
        multipart_body(vec![
            Part::Field(
                "manifest",
                serde_json::to_string(&manifest).expect("manifest json"),
            ),
            Part::File(
                "asset0",
                "package.tgz".to_string(),
                gzipped_tar("package/package.json", package_json.as_bytes()),
            ),
        ])
    }

    pub(crate) fn cargo_upload_body(name: &str, version: &str) -> (String, Vec<u8>) {
        let manifest = ArtifactUploadManifest {
            schema_version: 1,
            format: meta::FORMAT_CARGO.to_string(),
            assets: vec![ArtifactUploadAsset {
                part: "asset0".to_string(),
                ..Default::default()
            }],
            cargo: Some(CargoUploadManifest { yanked: false }),
            ..Default::default()
        };
        let cargo_toml = format!(
            "[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2021\"\nrust-version = \"1.75\"\n\n[dependencies]\nserde = \"1.0\"\n"
        );
        multipart_body(vec![
            Part::Field(
                "manifest",
                serde_json::to_string(&manifest).expect("manifest json"),
            ),
            Part::File(
                "asset0",
                "package.crate".to_string(),
                gzipped_tar(
                    &format!("{name}-{version}/Cargo.toml"),
                    cargo_toml.as_bytes(),
                ),
            ),
        ])
    }

    #[tokio::test]
    async fn maven_publication_delete_removes_last_version_metadata() {
        let h = new_upload_test_harness().await;
        let (content_type, body) = maven_upload_body("4.0.0", b"jar");
        let outcome = receive(
            &h.uploader,
            &h.repository,
            "maven-delete-key-00000000001",
            &content_type,
            body,
        )
        .await;
        assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
        let publication = h
            .store
            .get_artifact_publication_by_identity(
                h.repository.id,
                meta::FORMAT_MAVEN,
                "com.acme:widget",
                "4.0.0",
            )
            .await
            .expect("publication");
        let result = h
            .uploader
            .delete_publication(&h.repository, &publication.id, "alice")
            .await
            .expect("delete");
        assert_eq!(result.derived_deleted.len(), 3, "{result:?}");
        for path in [
            "com/acme/widget/4.0.0/widget-4.0.0.jar",
            "com/acme/widget/maven-metadata.xml",
        ] {
            assert!(
                matches!(
                    h.store.get_artifact(h.repository.id, path).await,
                    Err(meta::Error::NotFound)
                ),
                "path {path} still present"
            );
        }
    }

    #[tokio::test]
    async fn maven_plugin_upload_builds_group_metadata_and_rejects_prefix_collision() {
        let h = new_upload_test_harness().await;
        let (content_type, body) = maven_plugin_upload_body("widget-plugin", "1.0.0", "widget");
        let outcome = receive(
            &h.uploader,
            &h.repository,
            "maven-plugin-key-00000000001",
            &content_type,
            body,
        )
        .await;
        assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
        assert_eq!(outcome.result.derived.len(), 6);
        let metadata = h
            .store
            .get_artifact(h.repository.id, "com/acme/maven-metadata.xml")
            .await
            .expect("group metadata");
        let value = blob_bytes(&h.uploader, &metadata.blob_sha256).await;
        let text = String::from_utf8_lossy(&value);
        assert!(text.contains("<prefix>widget</prefix>"), "{text}");
        assert!(
            text.contains("<artifactId>widget-plugin</artifactId>"),
            "{text}"
        );

        // A second plugin claiming the same goal prefix is refused.
        let (content_type, body) = maven_plugin_upload_body("other-plugin", "1.0.0", "widget");
        let outcome = receive(
            &h.uploader,
            &h.repository,
            "maven-plugin-key-00000000002",
            &content_type,
            body,
        )
        .await;
        assert_eq!(
            outcome.problem.as_ref().map(|p| p.code.as_str()),
            Some("plugin_prefix_conflict"),
            "{:?}",
            outcome.problem
        );

        // Deleting the only plugin of the group removes the group metadata.
        let publication = h
            .store
            .get_artifact_publication_by_identity(
                h.repository.id,
                meta::FORMAT_MAVEN,
                "com.acme:widget-plugin",
                "1.0.0",
            )
            .await
            .expect("publication");
        h.uploader
            .delete_publication(&h.repository, &publication.id, "alice")
            .await
            .expect("delete");
        assert!(matches!(
            h.store
                .get_artifact(h.repository.id, "com/acme/maven-metadata.xml")
                .await,
            Err(meta::Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn npm_publication_delete_rebuilds_packument_and_tombstones_version() {
        let h = new_upload_test_harness().await;
        let repository = hosted_repo(&h.store, "npm-local", meta::FORMAT_NPM).await;
        for (index, version) in ["1.0.0", "2.0.0"].iter().enumerate() {
            let (content_type, body) = npm_upload_body(
                &format!(r#"{{"name":"widget","version":"{version}"}}"#),
                "latest",
            );
            let key = format!("npm-delete-key-{index:020}");
            let outcome = receive(&h.uploader, &repository, &key, &content_type, body).await;
            assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
        }
        let publication = h
            .store
            .get_artifact_publication_by_identity(
                repository.id,
                meta::FORMAT_NPM,
                "widget",
                "1.0.0",
            )
            .await
            .expect("publication");
        h.uploader
            .delete_publication(&repository, &publication.id, "alice")
            .await
            .expect("delete");
        assert!(matches!(
            h.store
                .get_artifact(repository.id, "widget/-/widget-1.0.0.tgz")
                .await,
            Err(meta::Error::NotFound)
        ));
        let packument_artifact = h
            .store
            .get_artifact(repository.id, "widget")
            .await
            .expect("packument");
        let packument: serde_json::Value =
            serde_json::from_slice(&blob_bytes(&h.uploader, &packument_artifact.blob_sha256).await)
                .expect("decode packument");
        let versions = packument["versions"].as_object().expect("versions");
        assert!(!versions.contains_key("1.0.0"), "{packument}");
        assert!(versions.contains_key("2.0.0"), "{packument}");
        assert_eq!(packument["dist-tags"]["latest"], "2.0.0");
        // The deleted version is tombstoned, so republishing it is refused.
        assert!(
            h.store
                .has_publication_tombstone(repository.id, meta::FORMAT_NPM, "widget", "1.0.0", "*")
                .await
                .expect("tombstone lookup")
        );
    }

    #[tokio::test]
    async fn npm_delete_publication_with_two_versions() {
        let h = new_upload_test_harness().await;
        let repository = hosted_repo(&h.store, "npm-multi", meta::FORMAT_NPM).await;
        for (index, version) in ["1.0.0", "1.1.0"].iter().enumerate() {
            let (content_type, body) = npm_upload_body(
                &format!(r#"{{"name":"widget","version":"{version}"}}"#),
                "latest",
            );
            let key = format!("e0000000-0000-4000-8000-00000000000{}", index + 1);
            let outcome = receive(&h.uploader, &repository, &key, &content_type, body).await;
            assert!(
                outcome.problem.is_none(),
                "publish {version}: {:?}",
                outcome.problem
            );
        }
        // Delete the newer version; the packument is rebuilt around the survivor.
        let publication = h
            .store
            .get_artifact_publication_by_identity(
                repository.id,
                meta::FORMAT_NPM,
                "widget",
                "1.1.0",
            )
            .await
            .expect("publication");
        h.uploader
            .delete_publication(&repository, &publication.id, "alice")
            .await
            .expect("delete");
        h.store
            .get_artifact_publication_by_identity(
                repository.id,
                meta::FORMAT_NPM,
                "widget",
                "1.0.0",
            )
            .await
            .expect("survivor missing");
    }

    #[tokio::test]
    async fn cargo_yank_toggle() {
        let h = new_upload_test_harness().await;
        let repository = hosted_repo(&h.store, "cargo-yank", meta::FORMAT_CARGO).await;
        let (content_type, body) = cargo_upload_body("widget", "1.0.0");
        let outcome = receive(
            &h.uploader,
            &repository,
            "f0000000-0000-4000-8000-000000000001",
            &content_type,
            body,
        )
        .await;
        assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
        let publication = h
            .store
            .get_artifact_publication_by_identity(
                repository.id,
                meta::FORMAT_CARGO,
                "widget",
                "1.0.0",
            )
            .await
            .expect("publication");
        for yanked in [true, false] {
            let result = h
                .uploader
                .set_cargo_yanked(&repository, &publication.id, "alice", yanked)
                .await
                .unwrap_or_else(|p| panic!("yank {yanked}: {p:?}"));
            assert_eq!(result.yanked, Some(yanked));
            let index = h
                .store
                .get_artifact(repository.id, "wi/dg/widget")
                .await
                .expect("sparse index");
            let value = blob_bytes(&h.uploader, &index.blob_sha256).await;
            let line: serde_json::Value =
                serde_json::from_str(String::from_utf8_lossy(&value).trim()).expect("index line");
            assert_eq!(line["yanked"], serde_json::Value::Bool(yanked));
        }
    }

    /// Builds the request parts a protocol handler is called with.
    fn native_request_parts(
        method: http::Method,
        uri: &str,
        content_type: &str,
    ) -> Arc<http::request::Parts> {
        let mut builder = http::Request::builder().method(method).uri(uri);
        if !content_type.is_empty() {
            builder = builder.header(http::header::CONTENT_TYPE, content_type);
        }
        let (parts, _) = builder.body(Body::empty()).expect("request").into_parts();
        Arc::new(parts)
    }

    fn native_manager(h: &UploadHarness) -> Arc<Manager> {
        let manager = Manager::new(
            Arc::clone(&h.uploader.engine),
            Arc::clone(&h.store),
            None,
            None,
            None,
        );
        manager.set_uploader(Some(Arc::clone(&h.uploader)));
        manager
    }

    pub(crate) fn pypi_wheel(name: &str, version: &str, tag: &str) -> (String, Vec<u8>) {
        let filename_name = name.replace('-', "_");
        let filename = format!("{filename_name}-{version}-{tag}.whl");
        let prefix = format!("{filename_name}-{version}.dist-info/");
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (entry, value) in [
            (
                format!("{prefix}METADATA"),
                format!(
                    "Metadata-Version: 2.3\nName: {name}\nVersion: {version}\nRequires-Python: >=3.10\n\n"
                ),
            ),
            (
                format!("{prefix}WHEEL"),
                format!("Wheel-Version: 1.0\nRoot-Is-Purelib: true\nTag: {tag}\n\n"),
            ),
            (format!("{prefix}RECORD"), String::new()),
        ] {
            writer.start_file(entry, options).expect("zip entry");
            writer.write_all(value.as_bytes()).expect("zip write");
        }
        (filename, writer.finish().expect("zip finish").into_inner())
    }

    #[tokio::test]
    async fn npm_native_publish_uses_atomic_uploader() {
        let h = new_upload_test_harness().await;
        let repository = hosted_repo(&h.store, "npm-local", meta::FORMAT_NPM).await;
        let tarball = gzipped_tar(
            "package/package.json",
            br#"{"name":"native-widget","version":"2.0.0"}"#,
        );
        let doc = serde_json::json!({
            "name": "native-widget",
            "dist-tags": {"next": "2.0.0"},
            "_attachments": {
                "native-widget-2.0.0.tgz": {
                    "data": base64::engine::general_purpose::STANDARD.encode(&tarball),
                }
            }
        });
        let manager = native_manager(&h);
        let response = manager
            .npm_publish(
                native_request_parts(http::Method::PUT, "/npm/npm-local/native-widget", ""),
                Resolved {
                    repo: repository.clone(),
                    cfg: crate::repoconfig::Config::default(),
                    path: "native-widget".to_string(),
                },
                Body::from(serde_json::to_vec(&doc).expect("publish document")),
            )
            .await;
        assert_eq!(response.status(), http::StatusCode::CREATED);
        let artifact = h
            .store
            .get_artifact(repository.id, "native-widget/-/native-widget-2.0.0.tgz")
            .await
            .expect("tarball");
        assert!(!artifact.publication_id.is_empty());
        let packument = h
            .store
            .get_artifact(repository.id, "native-widget")
            .await
            .expect("packument");
        assert!(meta::is_ui_managed_aggregate_metadata(
            &packument.metadata_json
        ));
        // The publish targeted the only tag the document carried.
        let value = blob_bytes(&h.uploader, &packument.blob_sha256).await;
        let document: serde_json::Value = serde_json::from_slice(&value).expect("decode packument");
        assert_eq!(document["dist-tags"]["next"], "2.0.0");
    }

    #[tokio::test]
    async fn npm_native_publish_accepts_scoped_attachment_name() {
        let h = new_upload_test_harness().await;
        let repository = hosted_repo(&h.store, "npm-local", meta::FORMAT_NPM).await;
        let tarball = gzipped_tar(
            "package/package.json",
            br#"{"name":"@acme/widget","version":"1.2.3"}"#,
        );
        let doc = serde_json::json!({
            "name": "@acme/widget",
            "dist-tags": {"latest": "1.2.3"},
            "_attachments": {
                "@acme/widget-1.2.3.tgz": {
                    "data": base64::engine::general_purpose::STANDARD.encode(&tarball),
                }
            }
        });
        let manager = native_manager(&h);
        let response = manager
            .npm_publish(
                native_request_parts(http::Method::PUT, "/npm/npm-local/@acme/widget", ""),
                Resolved {
                    repo: repository.clone(),
                    cfg: crate::repoconfig::Config::default(),
                    path: "@acme/widget".to_string(),
                },
                Body::from(serde_json::to_vec(&doc).expect("publish document")),
            )
            .await;
        assert_eq!(response.status(), http::StatusCode::CREATED);
        let artifact = h
            .store
            .get_artifact(repository.id, "@acme/widget/-/widget-1.2.3.tgz")
            .await
            .expect("scoped tarball");
        assert!(!artifact.publication_id.is_empty());
    }

    #[tokio::test]
    async fn twine_upload_uses_atomic_uploader() {
        let h = new_upload_test_harness().await;
        let repository = hosted_repo(&h.store, "pypi-local", meta::FORMAT_PYPI).await;
        let (wheel_name, wheel) = pypi_wheel("twine-project", "3.1.4", "py3-none-any");
        // Twine sends untrusted name/version fields alongside the distribution; the
        // publisher derives identity from the wheel metadata instead.
        let (content_type, body) = multipart_body(vec![
            Part::Field("name", "untrusted-name".to_string()),
            Part::Field("version", "0.0.0".to_string()),
            Part::File("content", wheel_name.clone(), wheel),
        ]);
        let manager = native_manager(&h);
        let response = manager
            .pypi_upload(
                native_request_parts(http::Method::POST, "/pypi/pypi-local/", &content_type),
                Resolved {
                    repo: repository.clone(),
                    cfg: crate::repoconfig::Config::default(),
                    path: String::new(),
                },
                Body::from(body),
            )
            .await;
        assert_eq!(response.status(), http::StatusCode::CREATED);
        let artifact = h
            .store
            .get_artifact(
                repository.id,
                &format!("packages/twine-project/{wheel_name}"),
            )
            .await
            .expect("distribution");
        assert!(!artifact.publication_id.is_empty());
        assert_eq!(artifact.version, "3.1.4");
    }

    pub(crate) fn pypi_sdist(name: &str, version: &str) -> (String, Vec<u8>) {
        let filename = format!("{name}-{version}.tar.gz");
        let value = format!("Metadata-Version: 2.3\nName: {name}\nVersion: {version}\n\n");
        (
            filename,
            gzipped_tar(&format!("{name}-{version}/PKG-INFO"), value.as_bytes()),
        )
    }

    pub(crate) fn pypi_upload_body(files: &[(String, Vec<u8>)]) -> (String, Vec<u8>) {
        let mut files: Vec<(String, Vec<u8>)> = files.to_vec();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        let manifest = ArtifactUploadManifest {
            schema_version: 1,
            format: meta::FORMAT_PYPI.to_string(),
            assets: (0..files.len())
                .map(|index| ArtifactUploadAsset {
                    part: format!("asset{index}"),
                    ..Default::default()
                })
                .collect(),
            pypi: Some(PyPIUploadManifest {}),
            ..Default::default()
        };
        let mut parts = vec![Part::Field(
            "manifest",
            serde_json::to_string(&manifest).expect("manifest json"),
        )];
        for (index, (name, value)) in files.into_iter().enumerate() {
            parts.push(Part::File(ASSET_PART_NAMES[index], name, value));
        }
        multipart_body(parts)
    }

    /// The multipart writer takes `&'static str` part names, so the fixed asset
    /// names are spelled out rather than formatted per call.
    const ASSET_PART_NAMES: [&str; 8] = [
        "asset0", "asset1", "asset2", "asset3", "asset4", "asset5", "asset6", "asset7",
    ];

    fn pypi_legacy_zip(name: &str, version: &str) -> (String, Vec<u8>) {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer
            .start_file(format!("{name}-{version}/PKG-INFO"), options)
            .expect("zip entry");
        writer
            .write_all(
                format!("Metadata-Version: 2.3\nName: {name}\nVersion: {version}\n\n").as_bytes(),
            )
            .expect("zip write");
        (
            format!("{name}-{version}.zip"),
            writer.finish().expect("zip finish").into_inner(),
        )
    }

    fn cargo_upload_body_with_manifest(
        name: &str,
        version: &str,
        cargo_toml: &str,
    ) -> (String, Vec<u8>) {
        let manifest = ArtifactUploadManifest {
            schema_version: 1,
            format: meta::FORMAT_CARGO.to_string(),
            assets: vec![ArtifactUploadAsset {
                part: "asset0".to_string(),
                ..Default::default()
            }],
            cargo: Some(CargoUploadManifest { yanked: false }),
            ..Default::default()
        };
        multipart_body(vec![
            Part::Field(
                "manifest",
                serde_json::to_string(&manifest).expect("manifest json"),
            ),
            Part::File(
                "asset0",
                format!("{name}-{version}.crate"),
                gzipped_tar(
                    &format!("{name}-{version}/Cargo.toml"),
                    cargo_toml.as_bytes(),
                ),
            ),
        ])
    }

    #[tokio::test]
    async fn maven_ui_upload_generated_pom_and_replay() {
        let h = new_upload_test_harness().await;
        let (content_type, body) = maven_upload_body("1.0.0", b"valid-enough-jar-bytes");
        let key = "11111111-1111-4111-8111-111111111111";
        let outcome = receive(&h.uploader, &h.repository, key, &content_type, body).await;
        assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
        assert!(!outcome.replay, "first upload reported a replay");
        assert_eq!(outcome.result.coordinate, "com.acme:widget:1.0.0");
        assert_eq!(outcome.result.created.len(), 6, "{:?}", outcome.result);
        assert_eq!(outcome.result.derived.len(), 3, "{:?}", outcome.result);
        for path in [
            "com/acme/widget/1.0.0/widget-1.0.0.jar",
            "com/acme/widget/1.0.0/widget-1.0.0.pom",
            "com/acme/widget/maven-metadata.xml",
        ] {
            let artifact = h
                .store
                .get_artifact(h.repository.id, path)
                .await
                .unwrap_or_else(|e| panic!("artifact {path}: {e}"));
            assert!(
                !artifact.blob_sha256.is_empty() && !artifact.artifact_role.is_empty(),
                "artifact {path} is not managed: {artifact:?}"
            );
        }
        let publications = h
            .store
            .list_artifact_publications(h.repository.id)
            .await
            .expect("publications");
        assert_eq!(publications.len(), 1, "{publications:?}");
        assert_eq!(publications[0].asset_count, 6, "{publications:?}");

        // The same idempotency key replays the committed result without reading the
        // body again.
        let replayed = receive(
            &h.uploader,
            &h.repository,
            key,
            "not/multipart",
            b"ignored".to_vec(),
        )
        .await;
        assert!(replayed.problem.is_none(), "{:?}", replayed.problem);
        assert!(replayed.replay, "second upload was not a replay");
        assert_eq!(replayed.result.upload_id, outcome.result.upload_id);
    }

    #[tokio::test]
    async fn maven_ui_upload_reports_async_durability() {
        let h = new_upload_test_harness().await;
        h.uploader.set_async_durability(true);
        let (content_type, body) = maven_upload_body("2.0.0", b"jar");
        let outcome = receive(
            &h.uploader,
            &h.repository,
            "44444444-4444-4444-8444-444444444444",
            &content_type,
            body,
        )
        .await;
        assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
        assert_eq!(outcome.result.durability, "async");
    }

    #[tokio::test]
    async fn maven_ui_upload_conflict_leaves_original_publication() {
        let h = new_upload_test_harness().await;
        let (content_type, body) = maven_upload_body("1.0.0", b"first");
        let outcome = receive(
            &h.uploader,
            &h.repository,
            "11111111-1111-4111-8111-111111111111",
            &content_type,
            body,
        )
        .await;
        assert!(outcome.problem.is_none(), "{:?}", outcome.problem);

        let (content_type, body) = maven_upload_body("1.0.0", b"different");
        let outcome = receive(
            &h.uploader,
            &h.repository,
            "22222222-2222-4222-8222-222222222222",
            &content_type,
            body,
        )
        .await;
        let problem = outcome.problem.expect("expected a conflict");
        assert_eq!(problem.code, "artifact_conflict", "{problem:?}");
        assert!(!problem.conflicts.is_empty(), "{problem:?}");

        let publications = h
            .store
            .list_artifact_publications(h.repository.id)
            .await
            .expect("publications");
        assert_eq!(publications.len(), 1, "publications after conflict");
        let artifact = h
            .store
            .get_artifact(h.repository.id, "com/acme/widget/1.0.0/widget-1.0.0.jar")
            .await
            .expect("artifact");
        assert_eq!(
            String::from_utf8_lossy(&blob_bytes(&h.uploader, &artifact.blob_sha256).await),
            "first",
            "conflict replaced the original bytes"
        );

        let result = h
            .uploader
            .commit_maven_conflict(
                &h.repository,
                &problem.upload_id,
                "alice",
                meta::SOURCE_LOCAL,
            )
            .await
            .expect("commit conflict");
        assert!(!result.replaced.is_empty(), "{result:?}");
        let artifact = h
            .store
            .get_artifact(h.repository.id, "com/acme/widget/1.0.0/widget-1.0.0.jar")
            .await
            .expect("artifact");
        assert_eq!(
            String::from_utf8_lossy(&blob_bytes(&h.uploader, &artifact.blob_sha256).await),
            "different",
            "confirmed replacement did not land"
        );
    }

    #[tokio::test]
    async fn maven_ui_upload_refuses_unmanaged_metadata() {
        let h = new_upload_test_harness().await;
        h.store
            .put_artifact(meta::Artifact {
                repo_id: h.repository.id,
                path: "com/acme/widget/maven-metadata.xml".to_string(),
                blob_sha256: "legacy-metadata".to_string(),
                size: 1,
                metadata_json: "{}".to_string(),
                ..Default::default()
            })
            .await
            .expect("seed legacy metadata");
        let (content_type, body) = maven_upload_body("1.0.0", b"jar");
        let outcome = receive(
            &h.uploader,
            &h.repository,
            "55555555-5555-4555-8555-555555555555",
            &content_type,
            body,
        )
        .await;
        assert_eq!(
            outcome.problem.as_ref().map(|p| p.code.as_str()),
            Some("derived_metadata_not_managed"),
            "{:?}",
            outcome.problem
        );
    }

    #[test]
    fn maven_metadata_xml_rejects_unknown_elements() {
        assert!(
        valid_maven_metadata_xml(
            br#"<metadata><groupId>com.acme</groupId><artifactId>widget</artifactId><versioning><versions><version>1.0.0</version></versions></versioning></metadata>"#
        ),
        "standard Maven metadata was rejected"
    );
        assert!(
            !valid_maven_metadata_xml(
                br#"<metadata><groupId>com.acme</groupId><plugins/></metadata>"#
            ),
            "unknown Maven metadata element was accepted"
        );
    }

    #[tokio::test]
    async fn npm_ui_upload_builds_immutable_version_and_packument() {
        let h = new_upload_test_harness().await;
        let repository = hosted_repo(&h.store, "npm-local", meta::FORMAT_NPM).await;
        let (content_type, body) = npm_upload_body(
            r#"{"name":"@acme/widget","version":"1.2.3","description":"test"}"#,
            "next",
        );
        let outcome = receive(
            &h.uploader,
            &repository,
            "66666666-6666-4666-8666-666666666666",
            &content_type,
            body,
        )
        .await;
        assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
        assert!(!outcome.replay);
        assert_eq!(outcome.result.coordinate, "@acme/widget@1.2.3");
        assert_eq!(outcome.result.scan_status, "disabled");

        let artifact = h
            .store
            .get_artifact(repository.id, "@acme/widget/-/widget-1.2.3.tgz")
            .await
            .expect("tarball artifact");
        assert!(!artifact.publication_id.is_empty(), "{artifact:?}");
        let packument_artifact = h
            .store
            .get_artifact(repository.id, "@acme/widget")
            .await
            .expect("packument artifact");
        assert!(
            meta::is_ui_managed_aggregate_metadata(&packument_artifact.metadata_json),
            "{packument_artifact:?}"
        );
        let packument: serde_json::Value =
            serde_json::from_slice(&blob_bytes(&h.uploader, &packument_artifact.blob_sha256).await)
                .expect("packument json");
        assert_eq!(packument["name"], "@acme/widget", "{packument}");
        assert_eq!(packument["dist-tags"]["next"], "1.2.3", "{packument}");
        let dist = &packument["versions"]["1.2.3"]["dist"];
        assert!(
            dist["shasum"].as_str().is_some_and(|v| !v.is_empty()),
            "{packument}"
        );
        assert!(
            dist["integrity"]
                .as_str()
                .is_some_and(|v| v.starts_with("sha512-")),
            "{packument}"
        );

        // A second version merges into the same packument.
        let (content_type, body) =
            npm_upload_body(r#"{"name":"@acme/widget","version":"1.3.0"}"#, "latest");
        let outcome = receive(
            &h.uploader,
            &repository,
            "88888888-8888-4888-8888-888888888888",
            &content_type,
            body,
        )
        .await;
        assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
        let packument_artifact = h
            .store
            .get_artifact(repository.id, "@acme/widget")
            .await
            .expect("packument artifact");
        let packument: serde_json::Value =
            serde_json::from_slice(&blob_bytes(&h.uploader, &packument_artifact.blob_sha256).await)
                .expect("packument json");
        let versions = packument["versions"].as_object().expect("versions");
        assert_eq!(versions.len(), 2, "{packument}");
        assert!(
            versions.contains_key("1.2.3") && versions.contains_key("1.3.0"),
            "{packument}"
        );
        assert_eq!(packument["dist-tags"]["next"], "1.2.3", "{packument}");
        assert_eq!(packument["dist-tags"]["latest"], "1.3.0", "{packument}");
    }

    #[tokio::test]
    async fn npm_ui_upload_rejects_private_and_non_canonical_version() {
        for (name, package_json, want) in [
            (
                "private",
                r#"{"name":"widget","version":"1.0.0","private":true}"#,
                "npm_private_package",
            ),
            (
                "version",
                r#"{"name":"widget","version":"v1.0.0"}"#,
                "npm_identity_invalid",
            ),
        ] {
            let h = new_upload_test_harness().await;
            let repository = hosted_repo(&h.store, "npm-local", meta::FORMAT_NPM).await;
            let (content_type, body) = npm_upload_body(package_json, "latest");
            let outcome = receive(
                &h.uploader,
                &repository,
                "77777777-7777-4777-8777-777777777777",
                &content_type,
                body,
            )
            .await;
            assert_eq!(
                outcome.problem.as_ref().map(|p| p.code.as_str()),
                Some(want),
                "{name}: {:?}",
                outcome.problem
            );
        }
    }

    #[tokio::test]
    async fn pypi_ui_upload_publishes_wheel_and_sdist_atomically() {
        let h = new_upload_test_harness().await;
        let repository = hosted_repo(&h.store, "pypi-local", meta::FORMAT_PYPI).await;
        let (wheel_name, wheel) = pypi_wheel("sample-project", "1.0.0", "py3-none-any");
        let (sdist_name, sdist) = pypi_sdist("sample-project", "1.0.0");
        let (content_type, body) =
            pypi_upload_body(&[(wheel_name.clone(), wheel), (sdist_name.clone(), sdist)]);
        let outcome = receive(
            &h.uploader,
            &repository,
            "99999999-9999-4999-8999-999999999999",
            &content_type,
            body,
        )
        .await;
        assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
        assert_eq!(outcome.result.coordinate, "sample-project==1.0.0");
        assert_eq!(outcome.result.created.len(), 2, "{:?}", outcome.result);

        let publication = h
            .store
            .get_artifact_publication_by_identity(
                repository.id,
                meta::FORMAT_PYPI,
                "sample-project",
                "1",
            )
            .await
            .expect("publication");
        assert_eq!(publication.asset_count, 2, "{publication:?}");
        for name in [&wheel_name, &sdist_name] {
            let artifact = h
                .store
                .get_artifact(repository.id, &format!("packages/sample-project/{name}"))
                .await
                .unwrap_or_else(|e| panic!("artifact {name}: {e}"));
            assert_eq!(artifact.publication_id, publication.id, "artifact {name}");
            assert_eq!(artifact.version, "1.0.0", "artifact {name}");
        }
    }

    #[test]
    fn canonical_pypi_version_pep440_aliases() {
        for (input, want) in [
            ("v1.0.0", "1"),
            ("1.0-alpha_01", "1a1"),
            ("1.0c1", "1rc1"),
            ("1.0-2", "1.post2"),
            ("1.0dev", "1.dev0"),
            ("2!01.0+LINUX_001", "2!1+linux.1"),
        ] {
            assert_eq!(
                canonical_pypi_version(input),
                want,
                "canonical_pypi_version({input:?})"
            );
        }
        for invalid in ["", "1..0", "1.0+", "banana"] {
            assert_eq!(
                canonical_pypi_version(invalid),
                "",
                "invalid {invalid:?} canonicalized"
            );
        }
    }

    #[tokio::test]
    async fn pypi_publication_delete_creates_filename_tombstone() {
        let h = new_upload_test_harness().await;
        let repository = hosted_repo(&h.store, "pypi-local", meta::FORMAT_PYPI).await;
        let (wheel_name, wheel) = pypi_wheel("deleted-project", "1.0.0", "py3-none-any");
        let (content_type, body) = pypi_upload_body(&[(wheel_name.clone(), wheel.clone())]);
        let outcome = receive(
            &h.uploader,
            &repository,
            "pypi-delete-key-000000000001",
            &content_type,
            body,
        )
        .await;
        assert!(outcome.problem.is_none(), "{:?}", outcome.problem);

        let publication = h
            .store
            .get_artifact_publication_by_identity(
                repository.id,
                meta::FORMAT_PYPI,
                "deleted-project",
                "1",
            )
            .await
            .expect("publication");
        h.uploader
            .delete_publication(&repository, &publication.id, "alice")
            .await
            .expect("delete");

        // The filename is tombstoned, so republishing the same distribution fails.
        let (content_type, body) = pypi_upload_body(&[(wheel_name, wheel)]);
        let outcome = receive(
            &h.uploader,
            &repository,
            "pypi-reuse-key-0000000000001",
            &content_type,
            body,
        )
        .await;
        assert_eq!(
            outcome.problem.as_ref().map(|p| p.code.as_str()),
            Some("distribution_filename_reused"),
            "{:?}",
            outcome.problem
        );
    }

    #[tokio::test]
    async fn pypi_legacy_zip_requires_compatibility_option() {
        let h = new_upload_test_harness().await;
        let (filename, archive) = pypi_legacy_zip("legacy-project", "1.0.0");
        let (content_type, body) = pypi_upload_body(&[(filename.clone(), archive.clone())]);
        let disabled = hosted_repo(&h.store, "pypi-disabled", meta::FORMAT_PYPI).await;
        let outcome = receive(
            &h.uploader,
            &disabled,
            "pypi-zip-disabled-0000000001",
            &content_type,
            body,
        )
        .await;
        assert_eq!(
            outcome.problem.as_ref().map(|p| p.code.as_str()),
            Some("pypi_legacy_zip_disabled"),
            "{:?}",
            outcome.problem
        );

        let mut enabled_config = crate::repoconfig::default();
        enabled_config.upload.pypi_allow_legacy_zip = true;
        let enabled = h
            .store
            .create_repository(Repository {
                name: "pypi-enabled".to_string(),
                format: meta::FORMAT_PYPI.to_string(),
                r#type: meta::TYPE_HOSTED.to_string(),
                config_json: enabled_config.json().expect("config json"),
                ..Default::default()
            })
            .await
            .expect("create repository");
        let (content_type, body) = pypi_upload_body(&[(filename, archive)]);
        let outcome = receive(
            &h.uploader,
            &enabled,
            "pypi-zip-enabled-00000000001",
            &content_type,
            body,
        )
        .await;
        assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
        assert_eq!(outcome.result.coordinate, "legacy-project==1.0.0");
    }

    #[tokio::test]
    async fn cargo_ui_upload_builds_sparse_index() {
        let h = new_upload_test_harness().await;
        let repository = hosted_repo(&h.store, "cargo-local", meta::FORMAT_CARGO).await;
        let (content_type, body) = cargo_upload_body("Widget", "1.2.3");
        let outcome = receive(
            &h.uploader,
            &repository,
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            &content_type,
            body,
        )
        .await;
        assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
        assert_eq!(outcome.result.coordinate, "Widget@1.2.3");
        h.store
            .get_artifact(repository.id, "api/v1/crates/Widget/1.2.3/download")
            .await
            .expect("crate artifact");
        let index_artifact = h
            .store
            .get_artifact(repository.id, "wi/dg/widget")
            .await
            .expect("index artifact");
        let index_bytes = blob_bytes(&h.uploader, &index_artifact.blob_sha256).await;
        let entry: serde_json::Value =
            serde_json::from_str(String::from_utf8_lossy(&index_bytes).trim())
                .expect("index entry");
        assert_eq!(entry["name"], "Widget", "{entry}");
        assert_eq!(entry["vers"], "1.2.3", "{entry}");
        assert!(
            entry["cksum"].as_str().is_some_and(|v| !v.is_empty()),
            "{entry}"
        );
    }

    #[tokio::test]
    async fn cargo_publication_yank_updates_only_index() {
        let h = new_upload_test_harness().await;
        let repository = hosted_repo(&h.store, "cargo-local", meta::FORMAT_CARGO).await;
        let (content_type, body) = cargo_upload_body("demo-crate", "1.2.3");
        let outcome = receive(
            &h.uploader,
            &repository,
            "cargo-yank-key-0000000000001",
            &content_type,
            body,
        )
        .await;
        assert!(outcome.problem.is_none(), "{:?}", outcome.problem);

        let publication = h
            .store
            .get_artifact_publication_by_identity(
                repository.id,
                meta::FORMAT_CARGO,
                "demo-crate",
                "1.2.3",
            )
            .await
            .expect("publication");
        let result = h
            .uploader
            .set_cargo_yanked(&repository, &publication.id, "alice", true)
            .await
            .expect("yank");
        assert_eq!(result.yanked, Some(true), "{result:?}");
        let index = h
            .store
            .get_artifact(repository.id, &cargo_sparse_path("demo-crate"))
            .await
            .expect("index artifact");
        let value = blob_bytes(&h.uploader, &index.blob_sha256).await;
        assert!(
            String::from_utf8_lossy(&value).contains(r#""yanked":true"#),
            "index={}",
            String::from_utf8_lossy(&value)
        );
        let publication = h
            .store
            .get_artifact_publication(&publication.id)
            .await
            .expect("publication");
        assert!(
            publication.yanked,
            "publication yank state was not persisted: {publication:?}"
        );

        h.uploader
            .set_cargo_yanked(&repository, &publication.id, "alice", false)
            .await
            .expect("unyank");
        let publication = h
            .store
            .get_artifact_publication(&publication.id)
            .await
            .expect("publication");
        assert!(
            !publication.yanked,
            "publication unyank state was not persisted: {publication:?}"
        );
    }

    #[tokio::test]
    async fn cargo_upload_preserves_target_dependencies_and_features2() {
        let h = new_upload_test_harness().await;
        let repository = hosted_repo(&h.store, "cargo-local", meta::FORMAT_CARGO).await;
        let cargo_toml = r#"[package]
name = "advanced-crate"
version = "1.0.0"
[dependencies]
serde = { version = "1", optional = true }
[target.'cfg(unix)'.dependencies]
libc = "0.2"
[features]
unix-extra = ["serde?/derive", "dep:serde"]
"#;
        let (content_type, body) =
            cargo_upload_body_with_manifest("advanced-crate", "1.0.0", cargo_toml);
        let outcome = receive(
            &h.uploader,
            &repository,
            "cargo-advanced-key-0000000001",
            &content_type,
            body,
        )
        .await;
        assert!(outcome.problem.is_none(), "{:?}", outcome.problem);

        let index = h
            .store
            .get_artifact(repository.id, &cargo_sparse_path("advanced-crate"))
            .await
            .expect("index artifact");
        let entry: serde_json::Value = serde_json::from_str(
            String::from_utf8_lossy(&blob_bytes(&h.uploader, &index.blob_sha256).await).trim(),
        )
        .expect("index entry");
        assert_eq!(entry["v"], 2, "{entry}");
        assert!(!entry["features2"].is_null(), "{entry}");
        let found_target = entry["deps"]
            .as_array()
            .expect("deps")
            .iter()
            .any(|dep| dep["name"] == "libc" && dep["target"] == "cfg(unix)");
        assert!(found_target, "target dependency missing: {}", entry["deps"]);
    }

    #[tokio::test]
    async fn ui_upload_rejects_manifest_not_first() {
        let h = new_upload_test_harness().await;
        let (content_type, body) = multipart_body(vec![Part::File(
            "asset0",
            "a.jar".to_string(),
            b"x".to_vec(),
        )]);
        let outcome = receive(
            &h.uploader,
            &h.repository,
            "11111111-1111-4111-8111-111111111111",
            &content_type,
            body,
        )
        .await;
        assert_eq!(
            outcome.problem.as_ref().map(|p| p.code.as_str()),
            Some("manifest_first"),
            "{:?}",
            outcome.problem
        );
    }

    /// The publish hook wired by `set_uploader` quarantines an uploaded package on
    /// an approval-enabled repo, so managed uploads enter the same approval workflow
    /// as registry publishes.
    #[tokio::test]
    async fn managed_upload_queues_approval() {
        let h = new_upload_test_harness().await;
        let manager = native_manager(&h); // wires the approval publish hook
        let mut cfg = crate::repoconfig::default();
        cfg.approval.enabled = true;
        let repository = h
            .store
            .create_repository(Repository {
                name: "maven-appr".to_string(),
                format: meta::FORMAT_MAVEN.to_string(),
                r#type: meta::TYPE_HOSTED.to_string(),
                config_json: cfg.json().expect("config json"),
                ..Default::default()
            })
            .await
            .expect("create repository");
        let _ = &manager;

        let (content_type, body) = maven_upload_body("1.0.0", b"valid-enough-jar-bytes");
        let outcome = receive(
            &h.uploader,
            &repository,
            "33333333-3333-4333-8333-333333333333",
            &content_type,
            body,
        )
        .await;
        assert!(outcome.problem.is_none(), "{:?}", outcome.problem);

        let status = h
            .store
            .get_approval_status("maven-appr", "com.acme:widget")
            .await
            .expect("approval status");
        assert_eq!(status, meta::APPROVAL_PENDING, "expected pending approval");
    }

    #[tokio::test]
    async fn maven_cancel_conflict_keeps_original() {
        let h = new_upload_test_harness().await;
        let (content_type, body) = maven_upload_body("1.0.0", b"first");
        let outcome = receive(
            &h.uploader,
            &h.repository,
            "11111111-1111-4111-8111-111111111111",
            &content_type,
            body,
        )
        .await;
        assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
        let (content_type, body) = maven_upload_body("1.0.0", b"second");
        let outcome = receive(
            &h.uploader,
            &h.repository,
            "22222222-2222-4222-8222-222222222222",
            &content_type,
            body,
        )
        .await;
        let problem = outcome.problem.expect("expected a conflict");
        assert!(!problem.upload_id.is_empty(), "{problem:?}");
        assert!(
            h.uploader
                .cancel_maven_conflict(
                    &h.repository,
                    &problem.upload_id,
                    "alice",
                    meta::SOURCE_LOCAL
                )
                .await
                .is_none(),
            "cancel reported a problem"
        );
        // The original bytes survive the cancelled replacement.
        let artifact = h
            .store
            .get_artifact(h.repository.id, "com/acme/widget/1.0.0/widget-1.0.0.jar")
            .await
            .expect("artifact");
        assert_eq!(
            String::from_utf8_lossy(&blob_bytes(&h.uploader, &artifact.blob_sha256).await),
            "first",
            "cancel altered the original bytes"
        );
        // Cancelling an unknown upload is a problem, not a panic.
        assert!(
            h.uploader
                .cancel_maven_conflict(&h.repository, "no-such-upload", "alice", meta::SOURCE_LOCAL)
                .await
                .is_some(),
            "expected a problem cancelling an unknown upload"
        );
    }

    #[tokio::test]
    async fn maven_invalid_coordinates_rejected() {
        let h = new_upload_test_harness().await;
        let (content_type, body) = multipart_body(vec![
        Part::Field(
            "manifest",
            r#"{"schema_version":1,"format":"maven","overwrite":false,"assets":[{"part":"asset0","extension":"jar"}],"maven":{"group_id":"","artifact_id":"widget","version":"1.0.0","generate_pom":true,"packaging":"jar"}}"#
                .to_string(),
        ),
        Part::File("asset0", "widget.jar".to_string(), b"jar".to_vec()),
    ]);
        let outcome = receive(
            &h.uploader,
            &h.repository,
            "33333333-3333-4333-8333-333333333333",
            &content_type,
            body,
        )
        .await;
        assert!(
            outcome.problem.is_some(),
            "expected a validation problem for an empty group_id"
        );
    }

    /// A two-asset Maven upload (jar plus supplied POM) with `generate_pom` off, so
    /// the uploaded POM is validated.
    fn maven_jar_plus_pom_body(pom: &str) -> (String, Vec<u8>) {
        multipart_body(vec![
        Part::Field(
            "manifest",
            r#"{"schema_version":1,"format":"maven","overwrite":false,"assets":[{"part":"asset0","extension":"jar"},{"part":"asset1","extension":"pom"}],"maven":{"group_id":"com.acme","artifact_id":"widget","version":"1.0.0","generate_pom":false,"packaging":"jar"}}"#
                .to_string(),
        ),
        Part::File("asset0", "widget.jar".to_string(), b"jar-bytes".to_vec()),
        Part::File("asset1", "widget.pom".to_string(), pom.as_bytes().to_vec()),
    ])
    }

    #[tokio::test]
    async fn maven_supplied_pom_publishes() {
        let h = new_upload_test_harness().await;
        let pom = r#"<project><modelVersion>4.0.0</modelVersion><groupId>com.acme</groupId><artifactId>widget</artifactId><version>1.0.0</version><packaging>jar</packaging></project>"#;
        let (content_type, body) = maven_jar_plus_pom_body(pom);
        let outcome = receive(
            &h.uploader,
            &h.repository,
            "aa111111-1111-4111-8111-111111111111",
            &content_type,
            body,
        )
        .await;
        assert!(
            outcome.problem.is_none(),
            "supplied POM publish: {:?}",
            outcome.problem
        );
        h.store
            .get_artifact(h.repository.id, "com/acme/widget/1.0.0/widget-1.0.0.pom")
            .await
            .expect("supplied POM not stored");
    }

    #[tokio::test]
    async fn maven_supplied_pom_mismatch_rejected() {
        let h = new_upload_test_harness().await;
        // The version differs from the manifest -> pom_coordinate_mismatch.
        let pom = r#"<project><modelVersion>4.0.0</modelVersion><groupId>com.acme</groupId><artifactId>widget</artifactId><version>9.9.9</version></project>"#;
        let (content_type, body) = maven_jar_plus_pom_body(pom);
        let outcome = receive(
            &h.uploader,
            &h.repository,
            "aa222222-2222-4222-8222-222222222222",
            &content_type,
            body,
        )
        .await;
        assert_eq!(
            outcome.problem.as_ref().map(|p| p.code.as_str()),
            Some("pom_coordinate_mismatch"),
            "{:?}",
            outcome.problem
        );
    }

    /// Result rows written by earlier releases store empty lists as `null`, so an
    /// idempotent replay of such an upload must still decode the stored result.
    #[test]
    fn upload_result_decodes_null_lists() {
        let raw = r#"{"upload_id":"u1","repository":"maven-hosted","format":"maven","coordinate":"g:a:1","created":null,"replaced":[{"path":"g/a/1/a-1.jar","role":"primary","size":1,"sha256":"x"}],"derived":null,"scan_status":"clean","durability":"durable","warnings":null}"#;
        let result: crate::repo::uiupload::ArtifactUploadResult =
            serde_json::from_str(raw).expect("legacy result decodes");
        assert!(result.created.is_empty());
        assert_eq!(result.replaced.len(), 1);
        assert!(result.derived.is_empty());
        assert!(result.warnings.is_empty());
    }

    mod serve_uploaded {
        use std::sync::Arc;

        use axum::Router;
        use http::{Method, StatusCode};

        use crate::meta::{self, Repository, Store};

        use crate::repo::uiupload::Uploader;
        use crate::repo::uiupload::tests::{
            cargo_upload_body, go_upload_body, maven_upload_body, new_upload_test_harness,
            npm_upload_body, pypi_sdist, pypi_upload_body, pypi_wheel, receive,
        };
        use crate::testing::repo::{call, mux};

        /// Publishes through the managed uploader and fails the test on a problem.
        async fn must_publish(
            uploader: &Arc<Uploader>,
            repo: &Repository,
            key: &str,
            content_type: &str,
            body: Vec<u8>,
        ) {
            let outcome = receive(uploader, repo, key, content_type, body).await;
            assert!(
                outcome.problem.is_none(),
                "publish {}: {:?}",
                repo.format,
                outcome.problem
            );
        }

        async fn get_ok(app: &Router, path: &str) {
            let resp = call(app, Method::GET, path, "").await;
            assert_eq!(resp.status, StatusCode::OK, "GET {path} = {}", resp.text());
        }

        async fn mk(store: &Arc<Store>, name: &str, format: &str) -> Repository {
            crate::repo::uiupload::tests::hosted_repo(store, name, format).await
        }

        #[tokio::test]
        async fn serve_uploaded_artifacts() {
            let h = new_upload_test_harness().await;
            // The harness owns the engine the uploader publishes into, so the manager
            // that serves the protocol routes is built over that same engine.
            let manager = crate::repo::Manager::new(
                Arc::clone(&h.uploader.engine),
                Arc::clone(&h.store),
                None,
                None,
                None,
            );
            let app = mux(&manager);

            // Maven
            let mvn = mk(&h.store, "mvn-hosted", meta::FORMAT_MAVEN).await;
            let (ct, body) = maven_upload_body("1.0.0", b"valid-enough-jar-bytes");
            must_publish(
                &h.uploader,
                &mvn,
                "10000000-0000-4000-8000-000000000001",
                &ct,
                body,
            )
            .await;
            get_ok(
                &app,
                "/maven/mvn-hosted/com/acme/widget/1.0.0/widget-1.0.0.jar",
            )
            .await;
            get_ok(
                &app,
                "/maven/mvn-hosted/com/acme/widget/1.0.0/widget-1.0.0.pom",
            )
            .await;
            get_ok(&app, "/maven/mvn-hosted/com/acme/widget/maven-metadata.xml").await;

            // npm
            let npm = mk(&h.store, "npm-hosted", meta::FORMAT_NPM).await;
            let (ct, body) = npm_upload_body(r#"{"name":"widget","version":"1.0.0"}"#, "latest");
            must_publish(
                &h.uploader,
                &npm,
                "20000000-0000-4000-8000-000000000002",
                &ct,
                body,
            )
            .await;
            get_ok(&app, "/npm/npm-hosted/widget").await;
            get_ok(&app, "/npm/npm-hosted/widget/-/widget-1.0.0.tgz").await;

            let gomod = mk(&h.store, "go-hosted", meta::FORMAT_GO).await;
            let (ct, body) = go_upload_body("example.com/acme/widget", "v1.2.3");
            must_publish(
                &h.uploader,
                &gomod,
                "30000000-0000-4000-8000-000000000003",
                &ct,
                body,
            )
            .await;
            get_ok(&app, "/go/go-hosted/example.com/acme/widget/@v/list").await;
            get_ok(&app, "/go/go-hosted/example.com/acme/widget/@latest").await;
            get_ok(&app, "/go/go-hosted/example.com/acme/widget/@v/v1.2.3.zip").await;
            get_ok(&app, "/go/go-hosted/example.com/acme/widget/@v/v1.2.3.info").await;

            // Cargo
            let crg = mk(&h.store, "cargo-hosted", meta::FORMAT_CARGO).await;
            let (ct, body) = cargo_upload_body("Widget", "1.2.3");
            must_publish(
                &h.uploader,
                &crg,
                "40000000-0000-4000-8000-000000000004",
                &ct,
                body,
            )
            .await;
            get_ok(
                &app,
                "/cargo/cargo-hosted/api/v1/crates/Widget/1.2.3/download",
            )
            .await;

            // PyPI
            let pypi = mk(&h.store, "pypi-hosted", meta::FORMAT_PYPI).await;
            let wheel = pypi_wheel("sample-project", "1.0.0", "py3-none-any");
            let sdist = pypi_sdist("sample-project", "1.0.0");
            let (ct, body) = pypi_upload_body(&[wheel, sdist]);
            must_publish(
                &h.uploader,
                &pypi,
                "50000000-0000-4000-8000-000000000005",
                &ct,
                body,
            )
            .await;
            get_ok(&app, "/pypi/pypi-hosted/simple/sample-project/").await;
        }
    }
}
