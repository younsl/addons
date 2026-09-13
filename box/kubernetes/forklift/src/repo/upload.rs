//! Browser/API upload preflight and storage for hosted repositories.

use std::sync::Arc;

use http::StatusCode;
use http::request::Parts;
use serde::Serialize;
use tokio::io::AsyncRead;

use crate::meta::{self, Repository};
use crate::repoconfig::{self, Config, MODE_AUDIT};

use super::{Kind, Manager, username_from_context};

/// Bounds a single browser upload. Keeping the limit aligned with the streaming
/// PyPI endpoint makes behavior predictable across upload surfaces and prevents
/// an accidental multi-gigabyte browser request.
pub const MAX_UI_UPLOAD_BYTES: i64 = 256 << 20;

/// The server-authoritative result of browser-upload preflight.
/// [`Manager::upload_hosted`] repeats the same validation so preflight cannot be
/// bypassed by a modified client or a repository configuration change between
/// requests.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct UploadPlan {
    pub path: String,
    pub package: String,
    pub version: String,
    pub content_type: String,
    pub size: i64,
    pub approval_required: bool,
    pub exists: bool,
}

/// Reports where the artifact landed and whether it entered the package
/// approval quarantine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct UploadResult {
    #[serde(flatten)]
    pub plan: UploadPlan,
    pub approval_status: String,
}

/// Errors from the upload path. [`UploadError::Validation`] is safe to return to
/// a client as a 4xx response.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("{0}")]
    Validation(#[from] UploadValidationError),
    #[error("{0}")]
    Meta(#[from] meta::Error),
    #[error("{0}")]
    Store(String),
}

/// A client-safe upload rejection with the status it maps to.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct UploadValidationError {
    pub status: StatusCode,
    pub message: String,
}

fn upload_error(status: StatusCode, message: impl Into<String>) -> UploadError {
    UploadError::Validation(UploadValidationError {
        status,
        message: message.into(),
    })
}

impl Manager {
    /// Performs the metadata-only preflight used by the UI.
    pub async fn validate_hosted_upload(
        &self,
        repo_id: i64,
        raw_path: &str,
        supplied_type: &str,
        size: i64,
    ) -> Result<UploadPlan, UploadError> {
        let repository = self.store.get_repository(repo_id).await?;
        if repository.r#type != meta::TYPE_HOSTED {
            return Err(upload_error(
                StatusCode::BAD_REQUEST,
                "uploads are only allowed on hosted repositories",
            ));
        }
        if repository.disabled {
            return Err(upload_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "repository is disabled",
            ));
        }
        if size <= 0 {
            return Err(upload_error(
                StatusCode::BAD_REQUEST,
                "file must not be empty",
            ));
        }
        if size > MAX_UI_UPLOAD_BYTES {
            return Err(upload_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "file exceeds the 256 MiB upload limit",
            ));
        }

        let mut p = raw_path.trim().trim_matches('/').to_string();
        if p.is_empty() || p.len() > 1024 || p.contains(['\\', '\0', '\r', '\n']) {
            return Err(upload_error(
                StatusCode::BAD_REQUEST,
                "invalid artifact path",
            ));
        }
        for segment in p.split('/') {
            if segment.is_empty() || segment == "." || segment == ".." {
                return Err(upload_error(
                    StatusCode::BAD_REQUEST,
                    "artifact path contains an invalid segment",
                ));
            }
        }

        let (pkg, version, mut content_type) = match repository.format.as_str() {
            meta::FORMAT_MAVEN => {
                let (pkg, version, ct) = (
                    super::maven::maven_package(&p),
                    super::maven::maven_version(&p),
                    super::maven::maven_content_type(&p),
                );
                if pkg.is_empty()
                    || version.is_empty()
                    || super::maven::maven_kind(&p) != Kind::Artifact
                {
                    return Err(upload_error(
                        StatusCode::BAD_REQUEST,
                        "Maven path must be group/artifact/version/filename",
                    ));
                }
                (pkg, version, ct)
            }
            meta::FORMAT_NPM => {
                p = super::npm::decode_npm_path(&p);
                let (pkg, version) = (super::npm::npm_package(&p), super::npm::npm_version(&p));
                if pkg.is_empty()
                    || version.is_empty()
                    || !p.contains("/-/")
                    || !p.to_lowercase().ends_with(".tgz")
                {
                    return Err(upload_error(
                        StatusCode::BAD_REQUEST,
                        "npm path must be package/-/package-version.tgz",
                    ));
                }
                (pkg, version, "application/gzip".to_string())
            }
            meta::FORMAT_CARGO => {
                let (pkg, version, ct) = (
                    super::cargo::cargo_package(&p),
                    super::cargo::cargo_version(&p),
                    super::cargo::cargo_content_type(&p),
                );
                if pkg.is_empty()
                    || version.is_empty()
                    || super::cargo::cargo_kind(&p) != Kind::Artifact
                {
                    return Err(upload_error(
                        StatusCode::BAD_REQUEST,
                        "Cargo path must be api/v1/crates/name/version/download",
                    ));
                }
                (pkg, version, ct)
            }
            meta::FORMAT_GO => {
                let (pkg, version, ct) = (
                    super::gomod::go_package(&p),
                    super::gomod::go_version(&p),
                    super::gomod::go_content_type(&p),
                );
                if pkg.is_empty()
                    || version.is_empty()
                    || super::gomod::go_kind(&p) != Kind::Artifact
                {
                    return Err(upload_error(
                        StatusCode::BAD_REQUEST,
                        "Go path must be module/@v/version.zip or module/@v/version.mod",
                    ));
                }
                (pkg, version, ct)
            }
            meta::FORMAT_PYPI => {
                let filename = p.rsplit('/').next().unwrap_or("").to_string();
                let (pkg, version) = (
                    super::pypi::pypi_package_from_filename(&filename),
                    super::pypi::pypi_version(&filename),
                );
                if pkg.is_empty() || version.is_empty() || p != format!("packages/{pkg}/{filename}")
                {
                    return Err(upload_error(
                        StatusCode::BAD_REQUEST,
                        "PyPI path must be packages/normalized-project/distribution-file",
                    ));
                }
                (pkg, version, "application/octet-stream".to_string())
            }
            // Raw has no coordinate convention: the validated path is the
            // identity and there is no version. The generic path check above is
            // the only structural requirement.
            meta::FORMAT_RAW => (
                super::raw::raw_package(&p),
                super::raw::raw_version(&p),
                super::raw::raw_content_type(&p),
            ),
            _ => {
                return Err(upload_error(
                    StatusCode::BAD_REQUEST,
                    "unsupported repository format",
                ));
            }
        };
        if !supplied_type.is_empty()
            && supplied_type != "application/octet-stream"
            && let Ok(parsed) = supplied_type.parse::<mime::Mime>()
        {
            content_type = parsed.essence_str().to_string();
        }

        let cfg = repoconfig::parse(&repository.config_json)
            .map_err(|e| UploadError::Store(e.to_string()))?;
        let exists = match self.store.get_artifact(repository.id, &p).await {
            Ok(_) => true,
            Err(e) if e.is_not_found() => false,
            Err(e) => return Err(e.into()),
        };
        Ok(UploadPlan {
            path: p,
            package: pkg.clone(),
            version,
            content_type,
            size,
            approval_required: cfg.approval.enabled && !approval_bypassed(&cfg, &pkg),
            exists,
        })
    }

    /// Stores a browser upload through the same engine used by package clients,
    /// schedules both security scanners, then enters package approval when
    /// configured. Existing paths are rejected to prevent an accidental UI
    /// overwrite.
    pub async fn upload_hosted(
        self: &Arc<Self>,
        parts: &Parts,
        repo_id: i64,
        raw_path: &str,
        supplied_type: &str,
        size: i64,
        body: impl AsyncRead + Send + Unpin + 'static,
    ) -> Result<UploadResult, UploadError> {
        let plan = self
            .validate_hosted_upload(repo_id, raw_path, supplied_type, size)
            .await?;
        if plan.exists {
            return Err(upload_error(
                StatusCode::CONFLICT,
                "an artifact already exists at this path",
            ));
        }
        let repository = self.store.get_repository(repo_id).await?;
        let limited = Box::pin(tokio::io::AsyncReadExt::take(body, (size + 1) as u64));
        let username = username_from_context(parts);
        self.engine
            .put(
                &repository,
                &plan.path,
                &plan.version,
                &plan.content_type,
                None,
                limited,
                &username,
            )
            .await
            .map_err(|e| UploadError::Store(e.to_string()))?;
        let stored = self.store.get_artifact(repository.id, &plan.path).await?;
        if stored.size != size {
            let _ = self.store.delete_artifact(repository.id, &plan.path).await;
            return Err(upload_error(
                StatusCode::BAD_REQUEST,
                "uploaded byte count does not match the validated file size",
            ));
        }
        self.scan_stored(&repository, &plan.path);
        self.resolve_stored(&repository, &plan.path);

        let mut result = UploadResult {
            plan: plan.clone(),
            approval_status: "not_required".to_string(),
        };
        if plan.approval_required {
            let cfg = repoconfig::parse(&repository.config_json)
                .map_err(|e| UploadError::Store(e.to_string()))?;
            result.approval_status = self
                .queue_hosted_approval(parts, &repository, &cfg, &plan.package, &plan.version)
                .await?;
        }
        Ok(result)
    }

    async fn queue_hosted_approval(
        self: &Arc<Self>,
        parts: &Parts,
        repository: &Repository,
        cfg: &Config,
        pkg: &str,
        version: &str,
    ) -> Result<String, UploadError> {
        if cfg.approval.effective_mode() == MODE_AUDIT {
            self.record_approval_request(parts, &repository.name, pkg);
            return Ok("audit".to_string());
        }
        let username = username_from_context(parts);
        let created = self
            .store
            .upsert_pending_approval(&repository.name, pkg, &username, version)
            .await?;
        let status = self
            .store
            .get_approval_status(&repository.name, pkg)
            .await?;
        if created {
            self.record_approval_request(parts, &repository.name, pkg);
            let hook = self.on_approval.read().clone();
            if let Some(hook) = hook {
                hook(
                    repository.name.clone(),
                    repository.id,
                    repository.format.clone(),
                    pkg.to_string(),
                    version.to_string(),
                    username,
                    cfg.notify.receivers.clone(),
                );
            }
        }
        Ok(status)
    }
}

/// Reports whether `pkg` matches one of the repository's auto-approve globs.
pub(crate) fn approval_bypassed(cfg: &Config, pkg: &str) -> bool {
    cfg.approval
        .auto_approve
        .iter()
        .any(|pattern| glob_match::glob_match(pattern, pkg))
}

#[cfg(test)]
pub(crate) mod tests {
    use http::{Method, StatusCode};

    use crate::meta;
    use crate::repoconfig;

    use crate::repo::upload::MAX_UI_UPLOAD_BYTES;
    use crate::testing::repo::{call, mk_format_repo, mux, new_test_manager};

    /// The request parts a browser upload is attributed to.
    fn upload_parts(uri: &str) -> http::request::Parts {
        let (parts, _) = http::Request::builder()
            .method(Method::PUT)
            .uri(uri)
            .body(())
            .expect("request")
            .into_parts();
        parts
    }

    #[tokio::test]
    async fn hosted_browser_upload_preflight_and_approval_quarantine() {
        let tm = new_test_manager().await;
        let mut cfg = repoconfig::default();
        cfg.approval.enabled = true;
        mk_format_repo(
            &tm.store,
            "releases",
            meta::FORMAT_MAVEN,
            meta::TYPE_HOSTED,
            "",
            cfg,
        )
        .await;
        let repository = tm
            .store
            .get_repository_by_name("releases")
            .await
            .expect("repository");

        let artifact_path = "com/acme/widget/1.2.0/widget-1.2.0.jar";
        let body = "JAR-BYTES";
        let plan = tm
            .manager
            .validate_hosted_upload(
                repository.id,
                artifact_path,
                "application/java-archive",
                body.len() as i64,
            )
            .await
            .expect("preflight");
        assert_eq!(plan.package, "com.acme:widget", "{plan:?}");
        assert_eq!(plan.version, "1.2.0", "{plan:?}");
        assert!(plan.approval_required, "{plan:?}");
        assert!(!plan.exists, "{plan:?}");

        let result = tm
            .manager
            .upload_hosted(
                &upload_parts("/api/v1/repositories/1/artifacts/upload"),
                repository.id,
                artifact_path,
                "application/java-archive",
                body.len() as i64,
                std::io::Cursor::new(body.as_bytes().to_vec()),
            )
            .await
            .expect("upload");
        assert_eq!(result.approval_status, meta::APPROVAL_PENDING);
        tm.store
            .get_artifact(repository.id, artifact_path)
            .await
            .expect("artifact was not stored");

        // The bytes exist but the normal package endpoint must quarantine them until
        // a reviewer approves the package.
        let app = mux(&tm.manager);
        let resp = call(
            &app,
            Method::GET,
            &format!("/maven/releases/{artifact_path}"),
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "before approval");
        assert!(
            resp.text().contains("pending approval"),
            "before approval: {}",
            resp.text()
        );
        tm.store
            .upsert_approval_decision(
                "releases",
                &plan.package,
                meta::APPROVAL_APPROVED,
                "reviewer",
                "looks good",
            )
            .await
            .expect("approve");
        let resp = call(
            &app,
            Method::GET,
            &format!("/maven/releases/{artifact_path}"),
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "after approval");
        assert_eq!(resp.text(), body);

        let plan = tm
            .manager
            .validate_hosted_upload(repository.id, artifact_path, "", body.len() as i64)
            .await
            .expect("collision preflight");
        assert!(plan.exists, "collision preflight: {plan:?}");
        assert!(
            tm.manager
                .upload_hosted(
                    &upload_parts("/upload"),
                    repository.id,
                    artifact_path,
                    "",
                    body.len() as i64,
                    std::io::Cursor::new(body.as_bytes().to_vec()),
                )
                .await
                .is_err(),
            "expected existing-path upload to fail"
        );
    }

    #[tokio::test]
    async fn hosted_browser_upload_raw_stores_and_serves_literal_path() {
        let tm = new_test_manager().await;
        mk_format_repo(
            &tm.store,
            "files",
            meta::FORMAT_RAW,
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        let repository = tm
            .store
            .get_repository_by_name("files")
            .await
            .expect("repository");

        // A raw artifact has no coordinate or version: the path is the identity, and
        // the content type is guessed from the extension.
        let artifact_path = "release/notes/v1.2.0.txt";
        let body = "hello raw";
        let plan = tm
            .manager
            .validate_hosted_upload(repository.id, artifact_path, "", body.len() as i64)
            .await
            .expect("preflight");
        assert_eq!(plan.package, artifact_path, "{plan:?}");
        assert_eq!(plan.version, "", "{plan:?}");
        assert!(!plan.approval_required, "{plan:?}");
        assert!(!plan.exists, "{plan:?}");
        // The guess itself is asserted below with an extension the builtin table does carry.
        assert_eq!(plan.content_type, "application/octet-stream");
        let guessed = tm
            .manager
            .validate_hosted_upload(repository.id, "release/notes/v1.2.0.json", "", 4)
            .await
            .expect("preflight");
        assert_eq!(guessed.content_type, "application/json");

        tm.manager
            .upload_hosted(
                &upload_parts("/upload"),
                repository.id,
                artifact_path,
                "",
                body.len() as i64,
                std::io::Cursor::new(body.as_bytes().to_vec()),
            )
            .await
            .expect("upload");

        let resp = call(
            &mux(&tm.manager),
            Method::GET,
            &format!("/raw/files/{artifact_path}"),
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "serve raw");
        assert_eq!(resp.text(), body);
    }

    #[tokio::test]
    async fn hosted_browser_upload_rejects_invalid_metadata_before_storage() {
        let tm = new_test_manager().await;
        mk_format_repo(
            &tm.store,
            "npm-hosted",
            meta::FORMAT_NPM,
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        let repository = tm
            .store
            .get_repository_by_name("npm-hosted")
            .await
            .expect("repository");

        for (path, size) in [
            ("../escape.tgz", 10),
            ("lodash/lodash-4.17.21.tgz", 10),
            ("lodash/-/lodash-4.17.21.tgz", 0),
            ("lodash/-/lodash-4.17.21.tgz", MAX_UI_UPLOAD_BYTES + 1),
        ] {
            assert!(
                tm.manager
                    .validate_hosted_upload(repository.id, path, "application/gzip", size)
                    .await
                    .is_err(),
                "preflight({path:?}, {size}) unexpectedly passed"
            );
        }
        let count = tm
            .store
            .count_artifacts(repository.id)
            .await
            .expect("count artifacts");
        assert_eq!(count, 0, "invalid preflight stored artifacts");
    }
}
