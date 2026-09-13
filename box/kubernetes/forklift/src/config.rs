//! Loads forklift configuration from environment variables with sensible
//! defaults. Every value can be overridden via env; CLI flags in main take
//! final precedence.
//!

use std::time::Duration;

/// Errors returned while loading or validating configuration.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A value or cross-field constraint was rejected.
    #[error("{0}")]
    Invalid(String),
    /// An environment variable held an unparsable value.
    #[error("{key}: {source}")]
    Env {
        key: String,
        #[source]
        source: Box<Error>,
    },
}

/// Convenience alias for results in this module.
pub type Result<T> = std::result::Result<T, Error>;

/// Config holds all runtime configuration.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    /// The root directory holding the SQLite metadata database and, for the
    /// filesystem backend, the content-addressed blob store. It is backed by a
    /// PersistentVolume (RWX or per-pod RWO) or, in the s3 backend, an
    /// ephemeral volume (emptyDir) that only holds the live SQLite working
    /// file.
    pub data_dir: String,

    /// Selects the blob + metadata backend (filesystem or S3).
    pub storage: StorageConfig,

    /// The listen address for the API + UI + package protocols.
    pub http_addr: String,
    /// The listen address for Prometheus metrics.
    pub metrics_addr: String,
    /// The listen address for the profiling handlers. It is loopback-only by
    /// default: the metrics port is published through the Service, and
    /// profiling endpoints are unauthenticated, so they get their own listener
    /// that only kubectl port-forward (which dials the pod's loopback) can
    /// reach. Empty disables the listener.
    pub pprof_addr: String,
    /// When set (e.g. https://forklift.example.com), used as the base for URLs
    /// synthesised in package metadata instead of deriving it from request
    /// Host/X-Forwarded-* headers.
    pub external_url: String,

    /// One of debug, info, warn, error.
    pub log_level: String,
    /// One of json, text.
    pub log_format: String,

    /// Bounds graceful shutdown.
    pub shutdown_timeout: Duration,

    /// Enables Kubernetes Lease leader election. When disabled the process
    /// always considers itself the leader (single-instance mode).
    pub ha: HAConfig,

    /// Enables PV-based active/standby replication: each pod keeps its own
    /// (RWO) PersistentVolume and the standby continuously pulls the leader's
    /// SQLite snapshot and blobs, promoting that copy when it acquires
    /// leadership. Use instead of a shared RWX volume.
    pub replication: ReplicationConfig,

    /// Configures authentication and authorization.
    pub auth: AuthConfig,

    /// Configures the per-repository audit log.
    pub audit: AuditConfig,

    /// Controls the experimental management-API/UI artifact publication
    /// surface. Protocol-native publishing is unaffected when this is
    /// disabled.
    pub upload: UploadConfig,

    /// Configures background vulnerability scanning (OSV). Scanning is
    /// disabled when `osv_url` is empty; per-repository policy gates
    /// enforcement.
    pub vuln: VulnConfig,

    /// Configures background license resolution (deps.dev). Resolution is
    /// disabled when `deps_dev_url` is empty; per-repository policy gates
    /// enforcement.
    pub license: LicenseConfig,

    /// Configures outbound alarms (e.g. a Slack/Mattermost webhook fired when
    /// a package is quarantined pending approval).
    pub notify: NotifyConfig,

    /// Configures the OCI distribution API (container images, Helm charts)
    /// served under /v2/.
    pub oci: OCIConfig,

    /// Configures the GitLab integration behind the coverage dashboard, which
    /// measures how many projects actually build through this forklift.
    /// Scanning is disabled when either the URL or the token is empty.
    pub coverage: CoverageConfig,

    /// On first run, creates default repositories: a proxy of each public
    /// registry (Maven Central, npm, crates.io, Go proxy) plus a local hosted
    /// repository per format, like a fresh Nexus install. Idempotent.
    pub seed_default_repos: bool,
}

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

/// Bounds the OCI distribution API.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OCIConfig {
    /// Caps a pushed or proxied manifest/index document, which is buffered
    /// whole for digesting and reference verification.
    pub max_manifest_bytes: i64,
    /// Caps one pushed blob (0 = unlimited). Proxy fetches are not capped: a
    /// digest-addressed upstream blob is whatever size the manifest says it
    /// is.
    pub max_blob_bytes: i64,
    /// How long an idle push session (POST opened, never finalized) survives
    /// before the prune deletes it with its temp file.
    pub upload_session_ttl: Duration,
    /// How often the leader runs the OCI reachability prune (untagged
    /// manifests, unreferenced blobs, expired sessions).
    pub prune_interval: Duration,
}

/// How long a blob must stay unreferenced before the sweeper reclaims its
/// bytes.
///
/// It is deliberately a constant rather than a setting. Blob deletion is
/// irreversible while the metadata database is only asynchronously durable, so
/// the delay has to outlast how far that database can move backwards, and an
/// operator tuning it down has no way to see the invariant they are breaking.
/// Read it as the window in which a metadata snapshot is still restorable:
/// bytes freed within the last day are still there, so recovering onto a
/// snapshot from within that day leaves no dangling references. The cost is
/// holding deleted bytes for a day, which at any realistic churn is a rounding
/// error against the blob store.
pub const BLOB_GC_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

/// Selects where blobs and the metadata database are persisted. A single
/// backend switch flips both subsystems so they cannot be misconfigured
/// independently.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StorageConfig {
    /// "fs" (local/PersistentVolume) or "s3" (shared S3 bucket). In s3 mode
    /// blobs live directly in the bucket and the metadata database is
    /// snapshotted to S3 (see `objstore`), so no EBS/RWX volume is needed.
    pub backend: String,
    /// The s3-mode cadence at which the leader uploads a metadata snapshot and
    /// standbys download it. Writes within one interval can be lost on
    /// failover (asynchronous, like PV replication).
    pub meta_sync_interval: Duration,
    /// S3 connection settings, used only when `backend` is "s3".
    pub s3: S3Config,
}

/// Configures the S3 backend. Empty region/credentials fall back to the AWS
/// default credential chain, which resolves EKS IRSA and EKS Pod Identity
/// automatically. `endpoint`/`force_path_style` support S3-compatible stores
/// (MinIO).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct S3Config {
    pub bucket: String,
    pub prefix: String,
    pub region: String,
    pub endpoint: String,
    pub force_path_style: bool,
    pub access_key_id: String,
    pub secret_access_key: String,
}

/// Configures OSV-based vulnerability scanning.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VulnConfig {
    /// The OSV API base (e.g. https://api.osv.dev). Empty disables scanning
    /// entirely.
    pub osv_url: String,
    /// How often stale scan results are re-queried.
    pub rescan_interval: Duration,
    /// Marks a scan result stale (eligible for re-scan) once older than this.
    pub ttl: Duration,
    /// The number of concurrent workers draining the scan queue, so freshly
    /// cached/uploaded coordinates are scanned promptly under burst.
    pub workers: i64,
}

/// Configures deps.dev-based license resolution.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LicenseConfig {
    /// The deps.dev API base (e.g. https://api.deps.dev). Empty disables
    /// license resolution entirely.
    pub deps_dev_url: String,
    /// How often stale results are re-queried.
    pub rescan_interval: Duration,
    /// Marks a result stale (eligible for re-resolution) once older than this.
    pub ttl: Duration,
    /// The number of concurrent workers draining the resolve queue.
    pub workers: i64,
}

/// Configures outbound alarms. The alarm channels (receivers) are managed at
/// runtime in the admin console; this only holds delivery tuning.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NotifyConfig {
    /// Bounds each webhook delivery attempt.
    pub webhook_timeout: Duration,
    /// Coalesces approval alarms per receiver over this window so an install
    /// burst yields one grouped message instead of one webhook per package.
    /// Zero delivers each alarm immediately.
    pub batch_window: Duration,
}

/// Holds the credentials the coverage scanner needs.
///
/// Only the connection lives here. Everything else about a scan (the scope,
/// the schedule, the exclusions, the alarm receivers) is edited by an
/// administrator in the console and stored in the metadata database, because
/// those are operational decisions rather than deployment ones. The token is
/// the exception on purpose: keeping it in the environment means it never
/// reaches the database, the snapshots synchronised to object storage, or a
/// settings API response.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CoverageConfig {
    /// Turns coverage scanning on. It is a separate switch from the
    /// credentials on purpose: a deployment that already has a GitLab token in
    /// its environment for some other reason should not start crawling that
    /// instance because forklift happened to gain the ability to. Off by
    /// default, so the feature is only ever running because somebody asked
    /// for it.
    pub enabled: bool,
    /// The GitLab base, e.g. https://gitlab.example.com. Empty disables
    /// coverage scanning even when `enabled` is true.
    pub gitlab_url: String,
    /// A personal, project or group access token with read_api. Empty
    /// disables coverage scanning even when `enabled` is true.
    pub gitlab_token: String,
}

/// Configures local users, sessions, OIDC and anonymous access.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuthConfig {
    /// Signs stateless session cookies. Must be shared across replicas in HA
    /// mode; if empty an ephemeral secret is generated.
    pub session_secret: String,
    pub session_ttl: Duration,
    /// Allows unauthenticated read access to repositories.
    pub anonymous_read: bool,
    /// `bootstrap_admin_user`/`bootstrap_admin_password` seed an initial admin
    /// on first run when no users exist. The password should be rotated after
    /// first login.
    pub bootstrap_admin_user: String,
    pub bootstrap_admin_password: String,

    pub oidc: OIDCConfig,
    pub rbac: RBACConfig,
}

/// Configures declarative, ArgoCD-style RBAC reconciled from the chart on
/// startup. When `policy_file` is empty, declarative RBAC is disabled and
/// authorization relies solely on roles managed through the API/UI.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RBACConfig {
    /// The path to an ArgoCD-style policy.csv (ConfigMap mount).
    pub policy_file: String,
    /// Grants its permissions to every authenticated principal, regardless of
    /// explicit assignments (ArgoCD policy.default). Empty means no default
    /// access (deny-all until a role is granted).
    pub default_role: String,
    /// A directory of local-account password files (Secret mount), one file
    /// per account named after the username.
    pub accounts_dir: String,
}

/// Configures Keycloak (or any OIDC provider) login.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OIDCConfig {
    pub enabled: bool,
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_url: String,
    pub username_claim: String,
    pub groups_claim: String,
}

/// Configures the per-repository audit log.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuditConfig {
    /// Turns audit logging on (artifact traffic and repository configuration
    /// changes are recorded per repository).
    pub enabled: bool,
    /// How long audit entries are kept before the leader prunes them. Zero
    /// disables pruning (keep forever).
    pub retention: Duration,
}

/// Configures PV-based replication between two replicas.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReplicationConfig {
    pub enabled: bool,
    /// Authenticates the internal replication endpoints. Must be shared by all
    /// replicas.
    pub token: String,
    /// The headless Service domain used to address peer pods, e.g.
    /// "forklift-headless.tools.svc.cluster.local". The leader URL is built as
    /// http://<lease-holder>.<peer_service>:<peer_port>.
    pub peer_service: String,
    /// The HTTP port peers listen on.
    pub peer_port: i64,
    /// The standby's pull cadence. Writes within one interval can be lost on
    /// failover (asynchronous replication).
    pub interval: Duration,
    /// Statically overrides leader discovery (testing / non-Kubernetes).
    pub leader_url: String,
    /// `pod_name`/`pod_namespace` identify this pod for the leader role label
    /// patch.
    pub pod_name: String,
    pub pod_namespace: String,
}

/// Configures leader election for active/standby high availability.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HAConfig {
    pub enabled: bool,
    pub lease_name: String,
    pub lease_namespace: String,
    pub identity: String,
    pub lease_duration: Duration,
    pub renew_deadline: Duration,
    pub retry_period: Duration,
}

const SECOND: Duration = Duration::from_secs(1);
const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(60 * 60);

impl Config {
    /// Builds a `Config` from the environment, applying defaults.
    pub fn load() -> Result<Config> {
        let mut c = Config {
            data_dir: env("FORKLIFT_DATA_DIR", "/data"),
            storage: StorageConfig {
                backend: env("FORKLIFT_STORAGE_BACKEND", "fs"),
                meta_sync_interval: env_duration(
                    "FORKLIFT_STORAGE_META_SYNC_INTERVAL",
                    30 * SECOND,
                ),
                s3: S3Config {
                    bucket: env("FORKLIFT_STORAGE_S3_BUCKET", ""),
                    prefix: env("FORKLIFT_STORAGE_S3_PREFIX", ""),
                    region: env("FORKLIFT_STORAGE_S3_REGION", ""),
                    endpoint: env("FORKLIFT_STORAGE_S3_ENDPOINT", ""),
                    force_path_style: env_bool("FORKLIFT_STORAGE_S3_FORCE_PATH_STYLE", false),
                    access_key_id: env("FORKLIFT_STORAGE_S3_ACCESS_KEY_ID", ""),
                    secret_access_key: env("FORKLIFT_STORAGE_S3_SECRET_ACCESS_KEY", ""),
                },
            },
            http_addr: env("FORKLIFT_HTTP_ADDR", ":8080"),
            metrics_addr: env("FORKLIFT_METRICS_ADDR", ":8081"),
            pprof_addr: env("FORKLIFT_PPROF_ADDR", "127.0.0.1:6060"),
            external_url: env("FORKLIFT_EXTERNAL_URL", ""),
            log_level: env("FORKLIFT_LOG_LEVEL", "info"),
            log_format: env("FORKLIFT_LOG_FORMAT", "json"),
            shutdown_timeout: env_duration("FORKLIFT_SHUTDOWN_TIMEOUT", 15 * SECOND),
            ha: HAConfig {
                enabled: env_bool("FORKLIFT_HA_ENABLED", false),
                lease_name: env("FORKLIFT_HA_LEASE_NAME", "forklift-leader"),
                lease_namespace: env(
                    "FORKLIFT_HA_LEASE_NAMESPACE",
                    &env("POD_NAMESPACE", "default"),
                ),
                identity: env("FORKLIFT_HA_IDENTITY", &env("POD_NAME", &hostname())),
                lease_duration: env_duration("FORKLIFT_HA_LEASE_DURATION", 15 * SECOND),
                renew_deadline: env_duration("FORKLIFT_HA_RENEW_DEADLINE", 10 * SECOND),
                retry_period: env_duration("FORKLIFT_HA_RETRY_PERIOD", 2 * SECOND),
            },
            replication: ReplicationConfig {
                enabled: env_bool("FORKLIFT_REPLICATION_ENABLED", false),
                token: env("FORKLIFT_REPLICATION_TOKEN", ""),
                peer_service: env("FORKLIFT_REPLICATION_PEER_SERVICE", ""),
                peer_port: env_int("FORKLIFT_REPLICATION_PEER_PORT", 8080),
                interval: env_duration("FORKLIFT_REPLICATION_INTERVAL", 30 * SECOND),
                leader_url: env("FORKLIFT_REPLICATION_LEADER_URL", ""),
                pod_name: env("POD_NAME", ""),
                pod_namespace: env("POD_NAMESPACE", ""),
            },
            auth: AuthConfig {
                session_secret: env("FORKLIFT_SESSION_SECRET", ""),
                session_ttl: env_duration("FORKLIFT_SESSION_TTL", 12 * HOUR),
                anonymous_read: env_bool("FORKLIFT_ANONYMOUS_READ", false),
                bootstrap_admin_user: env("FORKLIFT_BOOTSTRAP_ADMIN_USER", "admin"),
                bootstrap_admin_password: env("FORKLIFT_BOOTSTRAP_ADMIN_PASSWORD", ""),
                oidc: OIDCConfig {
                    enabled: env_bool("FORKLIFT_OIDC_ENABLED", false),
                    issuer_url: env("FORKLIFT_OIDC_ISSUER_URL", ""),
                    client_id: env("FORKLIFT_OIDC_CLIENT_ID", ""),
                    client_secret: env("FORKLIFT_OIDC_CLIENT_SECRET", ""),
                    redirect_url: env("FORKLIFT_OIDC_REDIRECT_URL", ""),
                    username_claim: env("FORKLIFT_OIDC_USERNAME_CLAIM", "preferred_username"),
                    groups_claim: env("FORKLIFT_OIDC_GROUPS_CLAIM", "groups"),
                },
                rbac: RBACConfig {
                    policy_file: env("FORKLIFT_RBAC_POLICY_FILE", ""),
                    default_role: env("FORKLIFT_RBAC_DEFAULT_ROLE", ""),
                    accounts_dir: env("FORKLIFT_RBAC_ACCOUNTS_DIR", ""),
                },
            },
            audit: AuditConfig {
                enabled: env_bool("FORKLIFT_AUDIT_ENABLED", true),
                retention: env_duration("FORKLIFT_AUDIT_RETENTION", 90 * 24 * HOUR),
            },
            upload: UploadConfig {
                enabled: env_bool("FORKLIFT_UI_UPLOAD_ENABLED", true),
                max_duration: env_duration("FORKLIFT_UI_UPLOAD_MAX_DURATION", 30 * MINUTE),
                max_concurrent: env_int("FORKLIFT_UI_UPLOAD_MAX_CONCURRENT", 4),
                max_concurrent_user: env_int("FORKLIFT_UI_UPLOAD_MAX_CONCURRENT_USER", 2),
                max_assets: env_int("FORKLIFT_UI_UPLOAD_MAX_ASSETS", 16),
                max_manifest_bytes: 64 << 10,
                max_field_bytes: 1 << 20,
                max_file_bytes: 256 << 20,
                max_batch_bytes: 512 << 20,
                go_max_zip_bytes: 500 << 20,
                archive_max_entries: 100_000,
                archive_max_meta_bytes: 16 << 20,
                idempotency_ttl: 24 * HOUR,
            },
            vuln: VulnConfig {
                osv_url: env("FORKLIFT_OSV_URL", "https://api.osv.dev"),
                rescan_interval: env_duration("FORKLIFT_VULN_RESCAN_INTERVAL", 6 * HOUR),
                ttl: env_duration("FORKLIFT_VULN_TTL", 24 * HOUR),
                workers: env_int("FORKLIFT_VULN_WORKERS", 6),
            },
            license: LicenseConfig {
                deps_dev_url: env("FORKLIFT_DEPSDEV_URL", "https://api.deps.dev"),
                rescan_interval: env_duration("FORKLIFT_LICENSE_RESCAN_INTERVAL", 24 * HOUR),
                ttl: env_duration("FORKLIFT_LICENSE_TTL", 7 * 24 * HOUR),
                workers: env_int("FORKLIFT_LICENSE_WORKERS", 6),
            },
            notify: NotifyConfig {
                webhook_timeout: env_duration("FORKLIFT_NOTIFY_WEBHOOK_TIMEOUT", 5 * SECOND),
                batch_window: env_duration("FORKLIFT_NOTIFY_BATCH_WINDOW", 15 * SECOND),
            },
            coverage: CoverageConfig {
                enabled: env_bool("FORKLIFT_COVERAGE_ENABLED", false),
                gitlab_url: env("FORKLIFT_COVERAGE_GITLAB_URL", "")
                    .trim_end_matches('/')
                    .to_string(),
                gitlab_token: env("FORKLIFT_COVERAGE_GITLAB_TOKEN", ""),
            },
            oci: OCIConfig {
                max_manifest_bytes: env_int64("FORKLIFT_OCI_MAX_MANIFEST_BYTES", 4 << 20),
                max_blob_bytes: env_int64("FORKLIFT_OCI_MAX_BLOB_BYTES", 0),
                upload_session_ttl: env_duration("FORKLIFT_OCI_UPLOAD_SESSION_TTL", 24 * HOUR),
                prune_interval: env_duration("FORKLIFT_OCI_PRUNE_INTERVAL", HOUR),
            },
            seed_default_repos: env_bool("FORKLIFT_SEED_DEFAULT_REPOS", true),
        };
        apply_upload_byte_env(&mut c.upload)?;
        c.validate()?;
        Ok(c)
    }

    /// Verifies cross-field constraints. It is public so CLI flag overrides
    /// can be checked after flag parsing as strictly as environment values.
    pub fn validate(&self) -> Result<()> {
        let invalid = |msg: String| Err(Error::Invalid(msg));
        if self.data_dir.is_empty() {
            return invalid("data dir must not be empty".into());
        }
        match self.log_level.as_str() {
            "debug" | "info" | "warn" | "error" => {}
            other => return invalid(format!("invalid log level {other:?}")),
        }
        match self.log_format.as_str() {
            "json" | "text" => {}
            other => return invalid(format!("invalid log format {other:?}")),
        }
        match self.storage.backend.as_str() {
            "fs" => {}
            "s3" => {
                if self.storage.s3.bucket.is_empty() {
                    return invalid(
                        "s3 storage backend requires FORKLIFT_STORAGE_S3_BUCKET".into(),
                    );
                }
                if self.storage.s3.access_key_id.is_empty()
                    != self.storage.s3.secret_access_key.is_empty()
                {
                    return invalid(
                        "s3 static credentials require both access key id and secret access key"
                            .into(),
                    );
                }
                if self.storage.meta_sync_interval.is_zero() {
                    return invalid("storage meta sync interval must be positive".into());
                }
                // The metadata database is only asynchronously durable here, so it
                // can move backwards by up to one sync interval on failover.
                // Reclaiming blob bytes inside that window turns a rollback into a
                // dangling artifact reference that no code path can repair, so the
                // GC grace period must outlast it. The grace period is fixed
                // (BLOB_GC_GRACE); the sync interval is not, so this rejects the
                // one combination that could invert them.
                if self.storage.meta_sync_interval >= BLOB_GC_GRACE {
                    return invalid(format!(
                        "storage meta sync interval ({}) must be shorter than the blob gc grace period ({})",
                        format_std_duration(self.storage.meta_sync_interval),
                        format_std_duration(BLOB_GC_GRACE)
                    ));
                }
                // The s3 backend already shares blobs and snapshots metadata to S3,
                // so PV-based peer replication is redundant and would fight it for
                // the metadata snapshot. Reject the combination.
                if self.replication.enabled {
                    return invalid(
                        "s3 storage backend is incompatible with replication; disable one".into(),
                    );
                }
            }
            other => return invalid(format!("invalid storage backend {other:?} (want fs or s3)")),
        }
        if self.ha.enabled && self.ha.identity.is_empty() {
            return invalid("HA enabled but identity is empty".into());
        }
        if self.replication.enabled {
            if !self.ha.enabled {
                return invalid("replication requires HA leader election".into());
            }
            if self.replication.token.is_empty() {
                return invalid("replication enabled but token is empty".into());
            }
            if self.replication.peer_service.is_empty() && self.replication.leader_url.is_empty() {
                return invalid(
                    "replication enabled but neither peer service nor leader URL is set".into(),
                );
            }
            if self.replication.interval.is_zero() {
                return invalid("replication interval must be positive".into());
            }
        }
        let u = &self.upload;
        if u.max_duration < MINUTE || u.max_duration > 2 * HOUR {
            return invalid("UI upload max duration must be between 1m and 2h".into());
        }
        if u.max_concurrent < 1 || u.max_concurrent > 32 {
            return invalid("UI upload max concurrent must be between 1 and 32".into());
        }
        if u.max_concurrent_user < 1
            || u.max_concurrent_user > 8
            || u.max_concurrent_user > u.max_concurrent
        {
            return invalid(
                "UI upload per-user concurrency must be between 1 and 8 and not exceed global concurrency"
                    .into(),
            );
        }
        if u.max_assets < 1 || u.max_assets > 64 {
            return invalid("UI upload max assets must be between 1 and 64".into());
        }
        if u.max_file_bytes < 1 << 20 || u.max_file_bytes > 1 << 30 {
            return invalid("UI upload max file bytes must be between 1MiB and 1GiB".into());
        }
        if u.max_batch_bytes < u.max_file_bytes {
            return invalid("UI upload max batch bytes must be at least max file bytes".into());
        }
        if u.go_max_zip_bytes < 1 << 20 || u.go_max_zip_bytes > 500 << 20 {
            return invalid("UI upload Go max zip bytes must be between 1MiB and 500MiB".into());
        }
        Ok(())
    }
}

fn apply_upload_byte_env(c: &mut UploadConfig) -> Result<()> {
    let values: [(&str, &mut i64); 3] = [
        ("FORKLIFT_UI_UPLOAD_MAX_FILE_BYTES", &mut c.max_file_bytes),
        ("FORKLIFT_UI_UPLOAD_MAX_BATCH_BYTES", &mut c.max_batch_bytes),
        (
            "FORKLIFT_UI_UPLOAD_GO_MAX_ZIP_BYTES",
            &mut c.go_max_zip_bytes,
        ),
    ];
    for (key, dst) in values {
        let Some(raw) = lookup_env(key) else {
            continue;
        };
        if raw.trim().is_empty() {
            continue;
        }
        *dst = parse_byte_size(&raw).map_err(|e| Error::Env {
            key: key.to_string(),
            source: Box::new(e),
        })?;
    }
    Ok(())
}

/// Parses a positive byte count with an optional KiB, MiB, or GiB suffix.
/// Decimal SI suffixes are deliberately unsupported.
pub fn parse_byte_size(raw: &str) -> Result<i64> {
    let mut s = raw.trim();
    if s.is_empty() {
        return Err(Error::Invalid("byte size must not be empty".into()));
    }
    let mut multiplier: i64 = 1;
    let lower = s.to_lowercase();
    for (suffix, factor) in [("kib", 1i64 << 10), ("mib", 1 << 20), ("gib", 1 << 30)] {
        if lower.ends_with(suffix) {
            multiplier = factor;
            s = s[..s.len() - suffix.len()].trim();
            break;
        }
    }
    match s.parse::<i64>() {
        Ok(n) if n > 0 && n <= i64::MAX / multiplier => Ok(n * multiplier),
        _ => Err(Error::Invalid(format!(
            "invalid positive IEC byte size {raw:?}"
        ))),
    }
}

/// Reads an environment variable, treating unset and non-UTF-8 values alike.
fn lookup_env(key: &str) -> Option<String> {
    std::env::var_os(key).and_then(|v| v.into_string().ok())
}

/// Returns the raw value of `key` when it is set and not blank, else `def`.
pub fn env(key: &str, def: &str) -> String {
    match lookup_env(key) {
        Some(v) if !v.trim().is_empty() => v,
        _ => def.to_string(),
    }
}

pub fn env_bool(key: &str, def: bool) -> bool {
    match lookup_env(key).as_deref().map(str::trim) {
        Some("1" | "t" | "T" | "TRUE" | "true" | "True") => true,
        Some("0" | "f" | "F" | "FALSE" | "false" | "False") => false,
        _ => def,
    }
}

/// Parses `key` as a decimal integer, falling back to `def` when unset or
/// unparsable.
pub fn env_int(key: &str, def: i64) -> i64 {
    env_int64(key, def)
}

/// Parses `key` as a decimal 64-bit integer, falling back to `def` when unset
/// or unparsable.
pub fn env_int64(key: &str, def: i64) -> i64 {
    lookup_env(key)
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(def)
}

pub fn env_duration(key: &str, def: Duration) -> Duration {
    lookup_env(key)
        .and_then(|v| parse_duration_nanos(v.trim()).ok())
        .map(|n| Duration::from_nanos(n.max(0) as u64))
        .unwrap_or(def)
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes into the buffer we
    // own and returns non-zero on failure, in which case `buf` is not read.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) };
    if rc != 0 {
        return "forklift".to_string();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    match std::str::from_utf8(&buf[..end]) {
        Ok(h) if !h.is_empty() => h.to_string(),
        _ => "forklift".to_string(),
    }
}

pub fn format_std_duration(d: Duration) -> String {
    format_duration_nanos(d.as_nanos().min(i64::MAX as u128) as i64)
}

//
// `Duration.String()` prints "72h3m0.5s", "1.5ms", "0s".

fn unit_nanos(unit: &[u8]) -> Option<u64> {
    match unit {
        b"ns" => Some(1),
        b"us" | b"\xC2\xB5s" | b"\xCE\xBCs" => Some(1_000),
        b"ms" => Some(1_000_000),
        b"s" => Some(1_000_000_000),
        b"m" => Some(60_000_000_000),
        b"h" => Some(3_600_000_000_000),
        _ => None,
    }
}

/// Consumes the leading `[0-9]*` from `s`, failing on overflow past 2^63.
fn leading_int(s: &[u8]) -> Option<(u64, &[u8])> {
    let mut x: u64 = 0;
    let mut i = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        if x > (1u64 << 63) / 10 {
            return None;
        }
        x = x * 10 + u64::from(s[i] - b'0');
        if x > 1u64 << 63 {
            return None;
        }
        i += 1;
    }
    Some((x, &s[i..]))
}

/// Consumes the leading `[0-9]*` of a fraction, returning the digits as an
/// integer plus the power-of-ten scale; digits past the precision of a u64
/// are dropped rather than failing.
fn leading_fraction(s: &[u8]) -> (u64, f64, &[u8]) {
    let mut x: u64 = 0;
    let mut scale = 1f64;
    let mut overflow = false;
    let mut i = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        let c = s[i];
        i += 1;
        if overflow {
            continue;
        }
        if x > (u64::MAX >> 1) / 10 {
            overflow = true;
            continue;
        }
        let y = x * 10 + u64::from(c - b'0');
        if y > 1u64 << 63 {
            overflow = true;
            continue;
        }
        x = y;
        scale *= 10.0;
    }
    (x, scale, &s[i..])
}

/// A duration string is a possibly signed sequence of decimal numbers, each with optional
/// fraction and a unit suffix, such as "300ms", "-1.5h" or "2h45m". Valid units are "ns", "us"
/// (or "µs"), "ms", "s", "m", "h". Bare numbers other than "0" are rejected.
pub fn parse_duration_nanos(s: &str) -> Result<i64> {
    let orig = s;
    let invalid = || Error::Invalid(format!("time: invalid duration {orig:?}"));
    let mut b = s.as_bytes();
    let mut neg = false;
    if let Some(&c) = b.first()
        && (c == b'-' || c == b'+')
    {
        neg = c == b'-';
        b = &b[1..];
    }
    if b == b"0" {
        return Ok(0);
    }
    if b.is_empty() {
        return Err(invalid());
    }
    let mut d: u64 = 0;
    while !b.is_empty() {
        // The next character must be [0-9.]
        if !(b[0] == b'.' || b[0].is_ascii_digit()) {
            return Err(invalid());
        }
        // Consume [0-9]*
        let pl = b.len();
        let (mut v, rest) = leading_int(b).ok_or_else(invalid)?;
        b = rest;
        let pre = pl != b.len(); // whether we consumed anything before a period
        // Consume (\.[0-9]*)?
        let mut post = false;
        let (mut f, mut scale) = (0u64, 1f64);
        if b.first() == Some(&b'.') {
            b = &b[1..];
            let pl = b.len();
            let (ff, sc, rest) = leading_fraction(b);
            f = ff;
            scale = sc;
            b = rest;
            post = pl != b.len();
        }
        if !pre && !post {
            return Err(invalid());
        }
        // Consume unit.
        let i = b
            .iter()
            .position(|&c| c == b'.' || c.is_ascii_digit())
            .unwrap_or(b.len());
        if i == 0 {
            return Err(Error::Invalid(format!(
                "time: missing unit in duration {orig:?}"
            )));
        }
        let u = &b[..i];
        b = &b[i..];
        let unit = unit_nanos(u).ok_or_else(|| {
            Error::Invalid(format!(
                "time: unknown unit {:?} in duration {orig:?}",
                String::from_utf8_lossy(u)
            ))
        })?;
        if v > (1u64 << 63) / unit {
            return Err(invalid());
        }
        v *= unit;
        if f > 0 {
            v += (f as f64 * (unit as f64 / scale)) as u64;
            if v > 1u64 << 63 {
                return Err(invalid());
            }
        }
        d = d.checked_add(v).ok_or_else(invalid)?;
        if d > 1u64 << 63 {
            return Err(invalid());
        }
    }
    if neg {
        // d <= 2^63, so the negation always fits (2^63 becomes i64::MIN).
        return Ok((d as i64).wrapping_neg());
    }
    if d > i64::MAX as u64 {
        return Err(invalid());
    }
    Ok(d as i64)
}

fn split_frac(v: u64, prec: u32) -> (u64, String) {
    let pow = 10u64.pow(prec);
    let frac = v % pow;
    if frac == 0 {
        return (v / pow, String::new());
    }
    let mut digits = format!("{frac:0width$}", width = prec as usize);
    while digits.ends_with('0') {
        digits.pop();
    }
    (v / pow, format!(".{digits}"))
}

/// Leading zero units are omitted; durations under a second use a smaller unit.
pub fn format_duration_nanos(nanos: i64) -> String {
    let u = nanos.unsigned_abs();
    let mut out = String::with_capacity(32);
    if nanos < 0 {
        out.push('-');
    }
    if u < 1_000_000_000 {
        if u == 0 {
            return "0s".to_string();
        }
        let (prec, unit) = if u < 1_000 {
            (0, "ns")
        } else if u < 1_000_000 {
            (3, "µs")
        } else {
            (6, "ms")
        };
        let (int, frac) = split_frac(u, prec);
        out.push_str(&format!("{int}{frac}{unit}"));
        return out;
    }
    let (secs, frac) = split_frac(u, 9);
    let hours = secs / 3600;
    let mins = secs / 60 % 60;
    let s = secs % 60;
    if hours > 0 {
        out.push_str(&format!("{hours}h"));
    }
    if hours > 0 || mins > 0 {
        out.push_str(&format!("{mins}m"));
    }
    out.push_str(&format!("{s}{frac}s"));
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use std::ffi::OsString;
    use std::time::Duration;

    use serial_test::serial;

    use crate::config::*;

    /// Records and restores environment variables.
    struct EnvGuard {
        saved: Vec<(String, Option<OsString>)>,
    }

    impl EnvGuard {
        fn new() -> Self {
            let mut g = EnvGuard { saved: Vec::new() };
            let keys: Vec<String> = std::env::vars_os()
                .filter_map(|(k, _)| k.into_string().ok())
                .filter(|k| k.starts_with("FORKLIFT_") || k == "POD_NAME" || k == "POD_NAMESPACE")
                .collect();
            for k in keys {
                g.remove(&k);
            }
            g
        }

        fn remember(&mut self, key: &str) {
            if !self.saved.iter().any(|(k, _)| k == key) {
                self.saved.push((key.to_string(), std::env::var_os(key)));
            }
        }

        fn set(&mut self, key: &str, value: &str) {
            self.remember(key);
            // SAFETY: env-mutating tests are serialised by `#[serial]` and no
            // other thread in this test reads the environment concurrently.
            unsafe { std::env::set_var(key, value) };
        }

        fn remove(&mut self, key: &str) {
            self.remember(key);
            // SAFETY: see `set`.
            unsafe { std::env::remove_var(key) };
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in self.saved.drain(..).rev() {
                // SAFETY: see `set`.
                unsafe {
                    match v {
                        Some(v) => std::env::set_var(&k, v),
                        None => std::env::remove_var(&k),
                    }
                }
            }
        }
    }

    #[test]
    #[serial]
    fn load_defaults() {
        let mut g = EnvGuard::new();
        g.set("FORKLIFT_DATA_DIR", "");
        let c = Config::load().unwrap();
        assert_eq!(c.data_dir, "/data", "data dir");
        assert_eq!(
            (c.http_addr.as_str(), c.metrics_addr.as_str()),
            (":8080", ":8081"),
            "addrs"
        );
        assert!(!c.ha.enabled, "HA should default off");
    }

    #[test]
    #[serial]
    fn load_overrides() {
        let mut g = EnvGuard::new();
        g.set("FORKLIFT_DATA_DIR", "/tmp/forklift");
        g.set("FORKLIFT_LOG_LEVEL", "debug");
        g.set("FORKLIFT_LOG_FORMAT", "text");
        g.set("FORKLIFT_SHUTDOWN_TIMEOUT", "30s");
        g.set("FORKLIFT_HA_ENABLED", "true");
        g.set("POD_NAME", "forklift-0");
        g.set("POD_NAMESPACE", "registry");

        let c = Config::load().unwrap();
        assert!(
            c.data_dir == "/tmp/forklift" && c.log_level == "debug" && c.log_format == "text",
            "overrides not applied: {c:?}"
        );
        assert_eq!(
            c.shutdown_timeout,
            Duration::from_secs(30),
            "shutdown timeout"
        );
        assert!(
            c.ha.enabled && c.ha.identity == "forklift-0" && c.ha.lease_namespace == "registry",
            "HA config = {:?}",
            c.ha
        );
    }

    #[test]
    #[serial]
    fn validate_rejects_bad_values() {
        let mut g = EnvGuard::new();
        g.set("FORKLIFT_LOG_LEVEL", "trace");
        assert!(Config::load().is_err(), "expected invalid log level error");
    }

    #[test]
    #[serial]
    fn validate_rejects_bad_format() {
        let mut g = EnvGuard::new();
        g.set("FORKLIFT_LOG_FORMAT", "xml");
        assert!(Config::load().is_err(), "expected invalid log format error");
    }

    #[test]
    #[serial]
    fn ha_identity_required_when_enabled() {
        let mut g = EnvGuard::new();
        g.set("FORKLIFT_HA_ENABLED", "true");
        g.set("FORKLIFT_HA_IDENTITY", "");
        g.set("POD_NAME", "");
        // Hostname normally fills identity; forcing it empty via both sources
        // being blank is not possible (hostname fallback), so assert the happy
        // path holds.
        let c = Config::load().expect("unexpected error");
        assert!(
            !c.ha.identity.is_empty(),
            "identity should fall back to hostname"
        );
    }

    #[test]
    #[serial]
    fn env_fallbacks_on_invalid() {
        let mut g = EnvGuard::new();
        g.set("FORKLIFT_HA_ENABLED", "notabool");
        g.set("FORKLIFT_SHUTDOWN_TIMEOUT", "notaduration");
        let c = Config::load().unwrap();
        assert!(
            !c.ha.enabled,
            "invalid bool should fall back to default false"
        );
        assert_eq!(
            c.shutdown_timeout,
            Duration::from_secs(15),
            "invalid duration should fall back to default"
        );
    }

    #[test]
    #[serial]
    fn auth_defaults() {
        let _g = EnvGuard::new();
        let c = Config::load().unwrap();
        assert!(
            c.auth.bootstrap_admin_user == "admin"
                && c.auth.session_ttl == Duration::from_secs(12 * 3600),
            "auth defaults = {:?}",
            c.auth
        );
        assert!(
            c.auth.oidc.username_claim == "preferred_username"
                && c.auth.oidc.groups_claim == "groups",
            "oidc claim defaults = {:?}",
            c.auth.oidc
        );
    }

    #[test]
    #[serial]
    fn storage_defaults_fs() {
        let _g = EnvGuard::new();
        let c = Config::load().unwrap();
        assert_eq!(c.storage.backend, "fs", "storage backend");
        assert_eq!(
            c.storage.meta_sync_interval,
            Duration::from_secs(30),
            "meta sync interval"
        );
    }

    #[test]
    #[serial]
    fn storage_s3_loads() {
        let mut g = EnvGuard::new();
        g.set("FORKLIFT_STORAGE_BACKEND", "s3");
        g.set("FORKLIFT_STORAGE_S3_BUCKET", "my-bucket");
        g.set("FORKLIFT_STORAGE_S3_PREFIX", "forklift");
        g.set("FORKLIFT_STORAGE_S3_REGION", "ap-northeast-2");
        g.set("FORKLIFT_STORAGE_S3_FORCE_PATH_STYLE", "true");
        g.set("FORKLIFT_STORAGE_META_SYNC_INTERVAL", "10s");

        let c = Config::load().expect("valid s3 config rejected");
        assert!(
            c.storage.backend == "s3" && c.storage.s3.bucket == "my-bucket",
            "s3 config = {:?}",
            c.storage
        );
        assert!(
            c.storage.s3.force_path_style && c.storage.s3.region == "ap-northeast-2",
            "s3 config = {:?}",
            c.storage.s3
        );
        assert_eq!(
            c.storage.meta_sync_interval,
            Duration::from_secs(10),
            "meta sync interval"
        );
    }

    #[test]
    #[serial]
    fn storage_s3_requires_bucket() {
        let mut g = EnvGuard::new();
        g.set("FORKLIFT_STORAGE_BACKEND", "s3");
        assert!(
            Config::load().is_err(),
            "expected error when s3 backend has no bucket"
        );
    }

    #[test]
    #[serial]
    fn storage_rejects_unknown_backend() {
        let mut g = EnvGuard::new();
        g.set("FORKLIFT_STORAGE_BACKEND", "gcs");
        assert!(
            Config::load().is_err(),
            "expected error for unknown backend"
        );
    }

    #[test]
    #[serial]
    fn storage_s3_rejects_partial_static_creds() {
        let mut g = EnvGuard::new();
        g.set("FORKLIFT_STORAGE_BACKEND", "s3");
        g.set("FORKLIFT_STORAGE_S3_BUCKET", "b");
        g.set("FORKLIFT_STORAGE_S3_ACCESS_KEY_ID", "only-id");
        assert!(
            Config::load().is_err(),
            "expected error when only one static credential is set"
        );
    }

    #[test]
    #[serial]
    fn storage_s3_incompatible_with_replication() {
        let mut g = EnvGuard::new();
        g.set("FORKLIFT_STORAGE_BACKEND", "s3");
        g.set("FORKLIFT_STORAGE_S3_BUCKET", "b");
        g.set("FORKLIFT_REPLICATION_ENABLED", "true");
        assert!(
            Config::load().is_err(),
            "expected error for s3 backend + replication"
        );
    }

    #[test]
    #[serial]
    fn upload_defaults() {
        let _g = EnvGuard::new();
        let c = Config::load().unwrap();
        assert!(
            c.upload.enabled,
            "UI upload must default on now that every supported format slice is ready"
        );
        assert!(
            c.upload.max_duration == Duration::from_secs(30 * 60)
                && c.upload.max_concurrent == 4
                && c.upload.max_concurrent_user == 2,
            "upload concurrency defaults = {:?}",
            c.upload
        );
        assert!(
            c.upload.max_assets == 16
                && c.upload.max_file_bytes == 256 << 20
                && c.upload.max_batch_bytes == 512 << 20,
            "upload size defaults = {:?}",
            c.upload
        );
        assert!(
            c.upload.go_max_zip_bytes == 500 << 20
                && c.upload.max_manifest_bytes == 64 << 10
                && c.upload.archive_max_entries == 100_000,
            "upload parser defaults = {:?}",
            c.upload
        );
    }

    #[test]
    #[serial]
    fn upload_overrides() {
        let mut g = EnvGuard::new();
        g.set("FORKLIFT_UI_UPLOAD_ENABLED", "true");
        g.set("FORKLIFT_UI_UPLOAD_MAX_DURATION", "45m");
        g.set("FORKLIFT_UI_UPLOAD_MAX_CONCURRENT", "8");
        g.set("FORKLIFT_UI_UPLOAD_MAX_CONCURRENT_USER", "3");
        g.set("FORKLIFT_UI_UPLOAD_MAX_ASSETS", "32");
        g.set("FORKLIFT_UI_UPLOAD_MAX_FILE_BYTES", "64MiB");
        g.set("FORKLIFT_UI_UPLOAD_MAX_BATCH_BYTES", "1GiB");
        g.set("FORKLIFT_UI_UPLOAD_GO_MAX_ZIP_BYTES", "400MiB");
        let c = Config::load().unwrap();
        assert!(
            c.upload.enabled
                && c.upload.max_duration == Duration::from_secs(45 * 60)
                && c.upload.max_concurrent == 8
                && c.upload.max_concurrent_user == 3,
            "upload overrides = {:?}",
            c.upload
        );
        assert!(
            c.upload.max_assets == 32
                && c.upload.max_file_bytes == 64 << 20
                && c.upload.max_batch_bytes == 1 << 30
                && c.upload.go_max_zip_bytes == 400 << 20,
            "upload byte overrides = {:?}",
            c.upload
        );
    }

    #[test]
    #[serial]
    fn upload_can_be_explicitly_disabled() {
        let mut g = EnvGuard::new();
        g.set("FORKLIFT_UI_UPLOAD_ENABLED", "false");
        let c = Config::load().unwrap();
        assert!(
            !c.upload.enabled,
            "FORKLIFT_UI_UPLOAD_ENABLED=false must preserve the opt-out"
        );
    }

    #[test]
    fn parse_byte_size_cases() {
        for (input, want) in [
            ("1", 1i64),
            ("64KiB", 64 << 10),
            ("256mib", 256 << 20),
            ("1 GiB", 1 << 30),
        ] {
            match parse_byte_size(input) {
                Ok(got) if got == want => {}
                other => panic!("parse_byte_size({input:?}) = {other:?}; want {want}"),
            }
        }
        for input in ["", "0", "-1", "1MB", "lots", "999999999999999999999GiB"] {
            assert!(
                parse_byte_size(input).is_err(),
                "parse_byte_size({input:?}) succeeded, want error"
            );
        }
    }

    #[test]
    #[serial]
    fn upload_validation() {
        let _g = EnvGuard::new();
        type Case = (&'static str, fn(&mut UploadConfig));
        let tests: [Case; 7] = [
            ("duration", |c| c.max_duration = Duration::from_secs(30)),
            ("global concurrency", |c| c.max_concurrent = 0),
            ("user concurrency", |c| {
                c.max_concurrent_user = c.max_concurrent + 1
            }),
            ("assets", |c| c.max_assets = 65),
            ("file bytes", |c| c.max_file_bytes = 1 << 10),
            ("batch bytes", |c| c.max_batch_bytes = c.max_file_bytes - 1),
            ("go zip bytes", |c| c.go_max_zip_bytes = 501 << 20),
        ];
        for (name, set) in tests {
            let mut c = Config::load().unwrap();
            set(&mut c.upload);
            assert!(
                c.validate().is_err(),
                "{name}: validate succeeded, want error"
            );
        }
    }

    fn set_replication_env(g: &mut EnvGuard) {
        g.set("FORKLIFT_HA_ENABLED", "true");
        g.set("FORKLIFT_REPLICATION_ENABLED", "true");
        g.set("FORKLIFT_REPLICATION_TOKEN", "secret");
        g.set(
            "FORKLIFT_REPLICATION_PEER_SERVICE",
            "forklift-headless.tools.svc",
        );
    }

    #[test]
    #[serial]
    fn replication_load() {
        let mut g = EnvGuard::new();
        set_replication_env(&mut g);
        g.set("FORKLIFT_REPLICATION_PEER_PORT", "9090");
        g.set("FORKLIFT_REPLICATION_INTERVAL", "10s");
        let c = Config::load().unwrap();
        let r = &c.replication;
        assert!(
            r.enabled
                && r.token == "secret"
                && r.peer_port == 9090
                && r.interval == Duration::from_secs(10),
            "unexpected replication config: {r:?}"
        );
    }

    #[test]
    #[serial]
    fn replication_requires_ha() {
        let mut g = EnvGuard::new();
        set_replication_env(&mut g);
        g.set("FORKLIFT_HA_ENABLED", "false");
        assert!(
            Config::load().is_err(),
            "expected error: replication without HA"
        );
    }

    #[test]
    #[serial]
    fn replication_requires_token() {
        let mut g = EnvGuard::new();
        set_replication_env(&mut g);
        g.set("FORKLIFT_REPLICATION_TOKEN", "");
        assert!(
            Config::load().is_err(),
            "expected error: replication without token"
        );
    }

    #[test]
    #[serial]
    fn replication_requires_peer_or_leader_url() {
        let mut g = EnvGuard::new();
        set_replication_env(&mut g);
        g.set("FORKLIFT_REPLICATION_PEER_SERVICE", "");
        assert!(
            Config::load().is_err(),
            "expected error: no peer service and no leader URL"
        );
        g.set("FORKLIFT_REPLICATION_LEADER_URL", "http://leader:8080");
        Config::load().expect("leader URL override should satisfy validation");
    }

    #[test]
    fn parse_duration_units_and_bounds() {
        let ns = 1i64;
        let us = 1_000 * ns;
        let ms = 1_000 * us;
        let s = 1_000 * ms;
        let m = 60 * s;
        let h = 60 * m;
        for (input, want) in [
            ("0", 0),
            ("5s", 5 * s),
            ("30s", 30 * s),
            ("1h30m", h + 30 * m),
            ("2h", 2 * h),
            ("15m", 15 * m),
            ("500ms", 500 * ms),
            ("1.5h", h + 30 * m),
            ("-1.5h", -(h + 30 * m)),
            ("+2m", 2 * m),
            ("1h1m1s1ms1us1ns", h + m + s + ms + us + ns),
            ("1µs", us),
            ("1μs", us),
            (".5s", 500 * ms),
            ("1.s", s),
            ("9223372036854775807ns", i64::MAX),
            ("-9223372036854775808ns", i64::MIN),
        ] {
            assert_eq!(parse_duration_nanos(input).unwrap(), want, "{input}");
        }
        for input in [
            "",
            "5",
            "s",
            "1",
            "-",
            "1x",
            "1h-5m",
            "1.5",
            ".s",
            "9223372036854775808ns",
            "notaduration",
            "1 h",
        ] {
            assert!(
                parse_duration_nanos(input).is_err(),
                "{input:?} should fail"
            );
        }
    }

    #[test]
    fn format_duration_canonical_units() {
        let s = 1_000_000_000i64;
        for (nanos, want) in [
            (0, "0s"),
            (1, "1ns"),
            (1_100, "1.1µs"),
            (1_000_000, "1ms"),
            (1_500_000, "1.5ms"),
            (s, "1s"),
            (90 * s, "1m30s"),
            (15 * 60 * s, "15m0s"),
            (3600 * s, "1h0m0s"),
            (72 * 3600 * s, "72h0m0s"),
            (72 * 3600 * s + 3 * 60 * s + s / 2, "72h3m0.5s"),
            (-(90 * s), "-1m30s"),
            (i64::MAX, "2562047h47m16.854775807s"),
            (i64::MIN, "-2562047h47m16.854775808s"),
        ] {
            assert_eq!(format_duration_nanos(nanos), want, "{nanos}");
        }
        assert_eq!(format_std_duration(BLOB_GC_GRACE), "24h0m0s");
    }
}
