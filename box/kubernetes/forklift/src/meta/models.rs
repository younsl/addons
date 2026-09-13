//! Row types shared across the store.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A Hosted, Proxy (cached upstream) or Group repository for one package
/// format.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Repository {
    pub id: i64,
    pub name: String,
    pub format: String,
    /// hosted | proxy | group
    pub r#type: String,
    pub upstream_url: String,
    pub config_json: String,
    /// Optional operator-facing free text shown in the console.
    pub description: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Takes the repository offline: it stops serving the package protocols
    /// while keeping its config and stored artifacts.
    pub disabled: bool,
}

/// A stored path within a repository pointing at a content-addressed blob,
/// plus caching/age-policy metadata.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Artifact {
    pub id: i64,
    pub repo_id: i64,
    pub path: String,
    pub version: String,
    pub blob_sha256: String,
    pub size: i64,
    pub content_type: String,
    pub metadata_json: String,
    /// Upstream original release time, when known.
    pub published_at: Option<DateTime<Utc>>,
    pub cached_at: DateTime<Utc>,
    pub last_accessed_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// The principal who first caused this artifact to be cached or uploaded
    /// ("" for anonymous / pre-migration rows).
    pub cached_by: String,
    /// Who last downloaded the artifact (throttled with `last_accessed_at`);
    /// empty for anonymous pulls.
    pub last_accessed_by: String,
    /// Empty for legacy/raw artifacts and shared mutable indexes.
    pub publication_id: String,
    /// primary, metadata, checksum, or index for managed paths.
    pub artifact_role: String,
}

/// Groups the immutable paths forming one package version.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtifactPublication {
    pub id: String,
    pub repo_id: i64,
    pub format: String,
    pub package_name: String,
    pub version: String,
    pub coordinate: String,
    pub upload_id: String,
    pub created_by: String,
    pub created_by_source: String,
    pub yanked: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub asset_count: i64,
    pub total_size: i64,
}

/// Prevents reuse of a deleted coordinate where an ecosystem requires it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtifactPublicationTombstone {
    pub repo_id: i64,
    pub format: String,
    pub package_name: String,
    pub version: String,
    pub asset_key: String,
    pub deleted_at: DateTime<Utc>,
    pub deleted_by: String,
}

/// An internal CAS-backed aggregate representation of a group index.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupMetadataCache {
    pub group_repo_id: i64,
    pub path: String,
    pub representation: String,
    pub blob_sha256: String,
    pub size: i64,
    pub sources_json: String,
    pub config_revision: String,
    pub expires_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The durable idempotency state for one upload.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ArtifactUploadRequest {
    pub idempotency_key: String,
    pub repo_id: i64,
    pub principal_name: String,
    pub principal_source: String,
    pub upload_id: String,
    pub state: String,
    pub plan_json: String,
    pub result_json: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

pub const UPLOAD_RECEIVING: &str = "receiving";
pub const UPLOAD_CONFLICT: &str = "conflict";
pub const UPLOAD_COMMITTED: &str = "committed";
pub const UPLOAD_FAILED: &str = "failed";

/// The reference-counted record for a content-addressed blob.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Blob {
    pub sha256: String,
    pub size: i64,
    pub ref_count: i64,
    pub created_at: DateTime<Utc>,
}

/// A local (password) or OIDC-sourced principal.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    /// local | oidc
    pub source: String,
    pub email: String,
    pub disabled: bool,
    /// Marks a token-only service account: interactive login (password and OIDC
    /// session) is refused, but the account's personal access tokens still
    /// authenticate for package operations. Set at creation and immutable.
    pub robot: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// `None` when the user has never logged in.
    pub last_login_at: Option<DateTime<Utc>>,
    /// Opts the account into failed-password lockout. When on,
    /// `failed_login_count` consecutive local-password failures (reset on
    /// success) crossing the threshold set `locked_at`, after which the account
    /// cannot authenticate until an admin unlocks it.
    pub lockout_enabled: bool,
    pub failed_login_count: i64,
    /// `None` when not locked.
    pub locked_at: Option<DateTime<Utc>>,
}

impl User {
    /// Reports whether the account is currently locked out.
    pub fn locked(&self) -> bool {
        self.locked_at.is_some()
    }
}

/// A named bundle of repository permissions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Role {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    /// Marks roles owned by the declarative RBAC policy. Managed roles are
    /// reconciled from the chart on startup and are read-only via the API.
    pub managed: bool,
}

/// Grants a set of actions on repositories matching a glob pattern.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Permission {
    pub id: i64,
    pub role_id: i64,
    /// glob: `*` or `maven-*`
    pub repo_pattern: String,
    /// csv: read,write,delete,admin
    pub actions: String,
    pub managed: bool,
}

/// Maps a Keycloak group name to a role.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupMapping {
    pub id: i64,
    pub group_name: String,
    pub role_id: i64,
    pub managed: bool,
}

/// A personal access token (PAT). Only the SHA-256 hash is stored.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Token {
    pub id: i64,
    pub user_id: i64,
    pub name: String,
    pub description: String,
    pub hash: String,
    pub scopes_json: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Source constants for users.
pub const SOURCE_LOCAL: &str = "local";
pub const SOURCE_OIDC: &str = "oidc";

/// Repository format constants.
pub const FORMAT_MAVEN: &str = "maven";
pub const FORMAT_NPM: &str = "npm";
pub const FORMAT_CARGO: &str = "cargo";
pub const FORMAT_GO: &str = "go";
pub const FORMAT_PYPI: &str = "pypi";
/// Stores arbitrary files at their literal path with no package coordinate,
/// version convention, or ecosystem scanning. The path is the artifact
/// identity.
pub const FORMAT_RAW: &str = "raw";
/// Speaks the OCI Distribution Specification under `/v2/{repo}/…`: container
/// images, Helm charts and other OCI artifacts. Blobs and manifests are
/// immutable digest-addressed artifact rows; tags live in `oci_tags`.
pub const FORMAT_OCI: &str = "oci";

/// Repository type constants.
pub const TYPE_HOSTED: &str = "hosted";
pub const TYPE_PROXY: &str = "proxy";
pub const TYPE_GROUP: &str = "group";
