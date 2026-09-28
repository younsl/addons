//! Upload-specific configuration loading, validation, and derived limits.

use std::time::Duration;

use super::{Error, HOUR, MINUTE, Result, lookup_env, parse_byte_size, parse_duration_nanos};

/// The `uiUpload.maxFileBytes` default. Upload paths that run without an
/// [`crate::repo::Uploader`] (only unit tests wire a manager that way) fall back
/// to it so every per-file limit still derives from the same setting.
pub const DEFAULT_UI_UPLOAD_MAX_FILE_BYTES: i64 = 256 << 20;

/// Bounds browser/API artifact publication. Parser limits that are
/// intentionally fixed in v1 are still carried here so the uploader has one
/// immutable configuration value and tests can assert every boundary.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UploadConfig {
    pub enabled: bool,
    pub max_duration: Duration,
    pub max_concurrent: i64,
    pub max_concurrent_user: i64,
    pub max_assets: i64,
    pub max_manifest_bytes: i64,
    pub max_field_bytes: i64,
    pub max_file_bytes: i64,
    pub max_batch_bytes: i64,
    pub go_max_zip_bytes: i64,
    pub archive_max_entries: i64,
    pub archive_max_meta_bytes: i64,
    pub idempotency_ttl: Duration,
}

impl UploadConfig {
    /// Loads upload settings from the environment. Unlike the legacy generic
    /// helpers, upload settings reject malformed values instead of silently
    /// starting with a default the operator did not request.
    pub(super) fn from_env() -> Result<Self> {
        Ok(Self {
            enabled: env_value("FORKLIFT_UI_UPLOAD_ENABLED", true, parse_bool)?,
            max_duration: env_value(
                "FORKLIFT_UI_UPLOAD_MAX_DURATION",
                30 * MINUTE,
                parse_duration,
            )?,
            max_concurrent: env_value("FORKLIFT_UI_UPLOAD_MAX_CONCURRENT", 4, parse_i64)?,
            max_concurrent_user: env_value("FORKLIFT_UI_UPLOAD_MAX_CONCURRENT_USER", 2, parse_i64)?,
            max_assets: env_value("FORKLIFT_UI_UPLOAD_MAX_ASSETS", 16, parse_i64)?,
            max_manifest_bytes: 64 << 10,
            max_field_bytes: 1 << 20,
            max_file_bytes: env_value(
                "FORKLIFT_UI_UPLOAD_MAX_FILE_BYTES",
                DEFAULT_UI_UPLOAD_MAX_FILE_BYTES,
                parse_byte_size,
            )?,
            max_batch_bytes: env_value(
                "FORKLIFT_UI_UPLOAD_MAX_BATCH_BYTES",
                512 << 20,
                parse_byte_size,
            )?,
            go_max_zip_bytes: env_value(
                "FORKLIFT_UI_UPLOAD_GO_MAX_ZIP_BYTES",
                500 << 20,
                parse_byte_size,
            )?,
            archive_max_entries: 100_000,
            archive_max_meta_bytes: 16 << 20,
            idempotency_ttl: 24 * HOUR,
        })
    }

    /// Verifies upload-local and cross-field constraints after environment and
    /// CLI overrides have both had a chance to update the settings.
    pub(crate) fn validate(&self) -> Result<()> {
        let invalid = |msg: &str| Err(Error::Invalid(msg.to_string()));
        if self.max_duration < MINUTE || self.max_duration > 2 * HOUR {
            return invalid("UI upload max duration must be between 1m and 2h");
        }
        if !(1..=32).contains(&self.max_concurrent) {
            return invalid("UI upload max concurrent must be between 1 and 32");
        }
        if !(1..=8).contains(&self.max_concurrent_user)
            || self.max_concurrent_user > self.max_concurrent
        {
            return invalid(
                "UI upload per-user concurrency must be between 1 and 8 and not exceed global concurrency",
            );
        }
        if !(1..=64).contains(&self.max_assets) {
            return invalid("UI upload max assets must be between 1 and 64");
        }
        if !((1 << 20)..=(1 << 30)).contains(&self.max_file_bytes) {
            return invalid("UI upload max file bytes must be between 1MiB and 1GiB");
        }
        if self.max_batch_bytes < self.max_file_bytes {
            return invalid("UI upload max batch bytes must be at least max file bytes");
        }
        if !((1 << 20)..=(500 << 20)).contains(&self.go_max_zip_bytes) {
            return invalid("UI upload Go max zip bytes must be between 1MiB and 500MiB");
        }
        if self.max_batch_bytes < self.go_max_zip_bytes {
            return invalid("UI upload max batch bytes must be at least Go max zip bytes");
        }
        if self.checked_max_request_bytes().is_none() {
            return invalid("UI upload request byte limit overflows int64");
        }
        Ok(())
    }

    /// The whole multipart request bound derived from the aggregate payload and
    /// fixed parser allowances. Callers receive only validated configurations.
    pub(crate) fn max_request_bytes(&self) -> i64 {
        self.checked_max_request_bytes()
            .expect("validated upload request byte limit")
    }

    fn checked_max_request_bytes(&self) -> Option<i64> {
        self.max_batch_bytes
            .checked_add(self.max_manifest_bytes)?
            .checked_add(self.max_field_bytes)
    }
}

/// Formats a byte count the way [`parse_byte_size`] reads it back (`256MiB`,
/// `64KiB`, `1234`), so a limit in an error message matches the setting that
/// configures it.
pub fn format_byte_size(n: i64) -> String {
    for (suffix, factor) in [("GiB", 1i64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)] {
        if n > 0 && n % factor == 0 {
            return format!("{}{suffix}", n / factor);
        }
    }
    n.to_string()
}

/// Reads a nonblank environment value with a strict parser. Unset and blank
/// values use the default; malformed values retain the key in the error.
fn env_value<T>(key: &str, default: T, parse: impl FnOnce(&str) -> Result<T>) -> Result<T> {
    let Some(raw) = lookup_env(key).filter(|value| !value.trim().is_empty()) else {
        return Ok(default);
    };
    parse(raw.trim()).map_err(|source| Error::Env {
        key: key.to_string(),
        source: Box::new(source),
    })
}

fn parse_bool(raw: &str) -> Result<bool> {
    match raw {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
        _ => Err(Error::Invalid(format!("invalid boolean {raw:?}"))),
    }
}

fn parse_i64(raw: &str) -> Result<i64> {
    raw.parse()
        .map_err(|_| Error::Invalid(format!("invalid integer {raw:?}")))
}

fn parse_duration(raw: &str) -> Result<Duration> {
    let nanos = parse_duration_nanos(raw)?;
    Ok(Duration::from_nanos(nanos.max(0) as u64))
}
