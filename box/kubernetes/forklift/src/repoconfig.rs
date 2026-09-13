//! Per-repository configuration: caching policy, age policy, and package
//! approval policy. It is stored as JSON in `repositories.config_json` and
//! shared by the admin API and the proxy cache core.
//!

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::config::{format_duration_nanos, parse_duration_nanos};

/// Errors returned while parsing or validating a repository config.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The stored or submitted document is not valid JSON for this shape.
    #[error("invalid config json: {0}")]
    Json(#[from] serde_json::Error),
    /// An invariant was violated.
    #[error("{0}")]
    Invalid(String),
}

/// Convenience alias for results in this module.
pub type Result<T> = std::result::Result<T, Error>;

/// Deserializes a missing or `null` value as the type's default. Shared with
/// the persisted JSON payloads that Go serialized empty slices as `null`.
pub(crate) fn null_default<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

fn is_false(b: &bool) -> bool {
    !b
}

/// The per-repository config payload.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(deserialize_with = "null_default")]
    pub cache: CacheConfig,
    #[serde(deserialize_with = "null_default")]
    pub age_policy: AgePolicyConfig,
    #[serde(deserialize_with = "null_default")]
    pub approval: ApprovalConfig,
    #[serde(deserialize_with = "null_default")]
    pub retention: RetentionConfig,
    #[serde(deserialize_with = "null_default")]
    pub vuln: VulnPolicyConfig,
    #[serde(deserialize_with = "null_default")]
    pub license: LicensePolicyConfig,
    #[serde(deserialize_with = "null_default")]
    pub policy_pipeline: PolicyPipelineConfig,
    #[serde(deserialize_with = "null_default")]
    pub group: GroupConfig,
    #[serde(deserialize_with = "null_default")]
    pub ip_acl: IPACLConfig,
    /// Allows anonymous (unauthenticated) reads on this repository even when
    /// the instance-wide FORKLIFT_ANONYMOUS_READ is off, like a Harbor public
    /// project. Writes and deletes always require authentication.
    #[serde(skip_serializing_if = "is_false", deserialize_with = "null_default")]
    pub public: bool,
    #[serde(deserialize_with = "null_default")]
    pub notify: NotifyConfig,
    #[serde(deserialize_with = "null_default")]
    pub upload: UploadConfig,
    /// Carries the credentials a proxy repository presents to its upstream.
    /// Secrets are stored in the config document and masked by the API layer
    /// on every read.
    #[serde(
        skip_serializing_if = "UpstreamAuthConfig::is_zero",
        deserialize_with = "null_default"
    )]
    pub upstream_auth: UpstreamAuthConfig,
}

/// Upstream auth type: HTTP basic (username/password or token as password).
pub const UPSTREAM_AUTH_BASIC: &str = "basic";
/// Upstream auth type: a bearer token.
pub const UPSTREAM_AUTH_BEARER: &str = "bearer";
/// Upstream auth type: an arbitrary header name/value pair.
pub const UPSTREAM_AUTH_HEADER: &str = "header";

/// Authenticates proxy fetches against a private upstream: HTTP basic
/// (username/password or token as password), a bearer token, or an arbitrary
/// header (e.g. a vendor API key). An empty `type_` sends no credentials,
/// matching prior behavior. `type_` is an open discriminator: flow based
/// schemes (e.g. an "oci" type for the Harbor/Docker Hub registry token
/// challenge) extend it later without changing this section's shape, and the
/// request-building seam lives in the repo engine's upstream request builder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpstreamAuthConfig {
    /// "" | basic | bearer | header
    #[serde(
        rename = "type",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub type_: String,
    /// basic
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub username: String,
    /// basic
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub password: String,
    /// bearer
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub token: String,
    /// header: header name
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub header: String,
    /// header: header value
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub value: String,
}

impl UpstreamAuthConfig {
    /// Reports whether credentials are configured.
    pub fn enabled(&self) -> bool {
        !self.type_.is_empty()
    }

    pub fn is_zero(&self) -> bool {
        *self == Self::default()
    }

    /// Checks the auth shape against its type.
    pub fn validate(&self) -> Result<()> {
        match self.type_.as_str() {
            "" => {}
            UPSTREAM_AUTH_BASIC => {
                if self.username.is_empty() {
                    return Err(Error::Invalid(
                        "upstream_auth basic requires username".into(),
                    ));
                }
            }
            UPSTREAM_AUTH_BEARER => {
                if self.token.is_empty() {
                    return Err(Error::Invalid("upstream_auth bearer requires token".into()));
                }
            }
            UPSTREAM_AUTH_HEADER => {
                if self.header.is_empty() || self.value.is_empty() {
                    return Err(Error::Invalid(
                        "upstream_auth header requires header and value".into(),
                    ));
                }
                if self.header.contains([' ', '\t', '\r', '\n', ':']) {
                    return Err(Error::Invalid(format!(
                        "invalid upstream_auth header name {:?}",
                        self.header
                    )));
                }
            }
            other => {
                return Err(Error::Invalid(format!(
                    "unsupported upstream_auth type {other:?}"
                )));
            }
        }
        Ok(())
    }

    /// Returns a copy with secret fields replaced by [`SECRET_MASK`] when set.
    /// The username and header name stay readable; only secrets are hidden.
    pub fn masked(&self) -> UpstreamAuthConfig {
        let mut u = self.clone();
        if !u.password.is_empty() {
            u.password = SECRET_MASK.to_string();
        }
        if !u.token.is_empty() {
            u.token = SECRET_MASK.to_string();
        }
        if !u.value.is_empty() {
            u.value = SECRET_MASK.to_string();
        }
        u
    }

    /// Restores secrets that came back as [`SECRET_MASK`] from `prev`, so a
    /// client can round-trip a masked config without knowing the stored
    /// values.
    pub fn unmask_from(&self, prev: &UpstreamAuthConfig) -> UpstreamAuthConfig {
        let mut u = self.clone();
        if u.password == SECRET_MASK {
            u.password = prev.password.clone();
        }
        if u.token == SECRET_MASK {
            u.token = prev.token.clone();
        }
        if u.value == SECRET_MASK {
            u.value = prev.value.clone();
        }
        u
    }

    /// Sets the configured credentials on an outbound upstream request's headers.
    pub fn apply_headers(&self, headers: &mut http::HeaderMap) {
        use base64::Engine as _;
        let (name, value) = match self.type_.as_str() {
            UPSTREAM_AUTH_BASIC => {
                let raw = format!("{}:{}", self.username, self.password);
                let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
                (
                    http::header::AUTHORIZATION,
                    http::HeaderValue::from_str(&format!("Basic {encoded}")),
                )
            }
            UPSTREAM_AUTH_BEARER => (
                http::header::AUTHORIZATION,
                http::HeaderValue::from_str(&format!("Bearer {}", self.token)),
            ),
            UPSTREAM_AUTH_HEADER => {
                let Ok(name) = http::HeaderName::from_str(&self.header) else {
                    tracing::warn!(header = %self.header, "skipping upstream auth header with invalid name");
                    return;
                };
                (name, http::HeaderValue::from_str(&self.value))
            }
            _ => return,
        };
        match value {
            Ok(mut v) => {
                v.set_sensitive(true);
                headers.insert(name, v);
            }
            Err(_) => {
                tracing::warn!(header = %name, "skipping upstream auth header with invalid value");
            }
        }
    }

    /// Sets the configured credentials on an outbound upstream request (the
    /// `reqwest` form of [`UpstreamAuthConfig::apply_headers`]).
    pub fn apply(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut headers = http::HeaderMap::new();
        self.apply_headers(&mut headers);
        if headers.is_empty() {
            return req;
        }
        req.headers(headers)
    }
}

/// Replaces upstream credential secrets in every API response. A masked value
/// sent back on update means "keep the stored secret".
pub const SECRET_MASK: &str = "********";

/// Format-specific knobs for browser/API uploads into a hosted repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UploadConfig {
    #[serde(skip_serializing_if = "is_false", deserialize_with = "null_default")]
    pub pypi_allow_legacy_zip: bool,
}

/// Policy filter name: the fixed version-deny artifact guard (legacy v1
/// pipelines listed it explicitly).
pub const POLICY_VERSION_DENY: &str = "version_deny";
/// Policy filter name: known-vulnerability assessment.
pub const POLICY_VULNERABILITY: &str = "vulnerability";
/// Policy filter name: license assessment.
pub const POLICY_LICENSE: &str = "license";
/// Policy filter name: release-age assessment.
pub const POLICY_AGE: &str = "age";

/// The current policy pipeline schema. Version deny is a fixed artifact
/// guard; human approval is the fixed final boundary. Only the assessment
/// filters are configurable.
pub const POLICY_PIPELINE_SCHEMA_VERSION: i64 = 2;
const LEGACY_POLICY_PIPELINE_VERSION: i64 = 1;

const DEFAULT_POLICY_ORDER: [&str; 3] = [POLICY_VULNERABILITY, POLICY_LICENSE, POLICY_AGE];

fn default_policy_order() -> Vec<String> {
    DEFAULT_POLICY_ORDER.iter().map(|s| s.to_string()).collect()
}

/// Controls policy evaluation order. Human approval is not included because
/// it always runs last, immediately before serving.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyPipelineConfig {
    #[serde(deserialize_with = "null_default")]
    pub schema_version: i64,
    #[serde(deserialize_with = "null_default")]
    pub order: Vec<String>,
}

impl PolicyPipelineConfig {
    /// Returns the configured order or the legacy order when it was omitted.
    /// The returned vector is the caller's to modify.
    pub fn effective_order(&self) -> Vec<String> {
        if self.order.is_empty() {
            return default_policy_order();
        }
        self.order.clone()
    }

    /// Checks the current assessment-chain schema and requires each
    /// reorderable filter exactly once. Fixed phase filters are invalid
    /// entries.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 0 && self.schema_version != POLICY_PIPELINE_SCHEMA_VERSION {
            return Err(Error::Invalid(format!(
                "unsupported policy pipeline schema_version {}",
                self.schema_version
            )));
        }
        if self.order.is_empty() {
            return Ok(());
        }
        let mut seen: Vec<&str> = Vec::with_capacity(self.order.len());
        for name in &self.order {
            match name.as_str() {
                POLICY_VULNERABILITY | POLICY_LICENSE | POLICY_AGE => {}
                other => {
                    return Err(Error::Invalid(format!(
                        "unsupported policy pipeline step {other:?}"
                    )));
                }
            }
            if seen.contains(&name.as_str()) {
                return Err(Error::Invalid(format!(
                    "duplicate policy pipeline step {name:?}"
                )));
            }
            seen.push(name);
        }
        for name in DEFAULT_POLICY_ORDER {
            if !seen.contains(&name) {
                return Err(Error::Invalid(format!(
                    "missing policy pipeline step {name:?}"
                )));
            }
        }
        Ok(())
    }

    fn validate_legacy(&self) -> Result<()> {
        if self.order.is_empty() {
            return Ok(());
        }
        let legacy = [
            POLICY_VERSION_DENY,
            POLICY_VULNERABILITY,
            POLICY_LICENSE,
            POLICY_AGE,
        ];
        let mut seen: Vec<&str> = Vec::with_capacity(self.order.len());
        for name in &self.order {
            if seen.contains(&name.as_str()) {
                return Err(Error::Invalid(format!(
                    "duplicate policy pipeline step {name:?}"
                )));
            }
            seen.push(name);
        }
        for name in legacy {
            if !seen.contains(&name) {
                return Err(Error::Invalid(format!(
                    "missing policy pipeline step {name:?}"
                )));
            }
        }
        if seen.len() != legacy.len() {
            return Err(Error::Invalid(
                "unsupported legacy policy pipeline step".into(),
            ));
        }
        Ok(())
    }
}

/// Selects which notification receivers (named alarm channels managed in the
/// admin console) are alerted for this repository's events, currently a
/// package entering the approval queue. `receivers` lists receiver names; an
/// empty list means no notification. Unknown/disabled names are skipped at
/// dispatch time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_default"
    )]
    pub receivers: Vec<String>,
}

/// Restricts which source IPs may reach a repository. When `enabled`, a
/// request whose client IP is not covered by `allow` (a list of IPv4/IPv6
/// addresses or CIDR blocks) is refused with 403. Enabled with an empty
/// `allow` denies every request. The client IP is the first X-Forwarded-For
/// hop set by the ingress, falling back to the TCP peer address (see
/// `audit::client_ip`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IPACLConfig {
    #[serde(deserialize_with = "null_default")]
    pub enabled: bool,
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_default"
    )]
    pub allow: Vec<String>,
}

fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_canonical(),
        v4 => v4,
    }
}

impl IPACLConfig {
    /// Reports whether `ip` (a bare address string) matches any `allow`
    /// entry. A client IP that does not parse is denied; a malformed stored
    /// entry is skipped. Callers should only invoke this when the ACL is
    /// `enabled`.
    pub fn allowed(&self, ip: &str) -> bool {
        let Ok(addr) = ip.parse::<IpAddr>() else {
            return false;
        };
        let addr = unmap(addr);
        for entry in &self.allow {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            if entry.contains('/') {
                if let Ok(pfx) = entry.parse::<ipnet::IpNet>()
                    && pfx.contains(&addr)
                {
                    return true;
                }
                continue;
            }
            if let Ok(got) = entry.parse::<IpAddr>()
                && unmap(got) == addr
            {
                return true;
            }
        }
        false
    }
}

/// Vulnerability policy action: deny the request.
pub const VULN_ACTION_BLOCK: &str = "block";
/// Vulnerability policy action: serve but log/metric.
pub const VULN_ACTION_WARN: &str = "warn";
/// Vulnerability policy action: serve and only record what would be blocked.
pub const VULN_ACTION_AUDIT: &str = "audit";

/// Severity label: critical.
pub const SEVERITY_CRITICAL: &str = "critical";
/// Severity label: high.
pub const SEVERITY_HIGH: &str = "high";
/// Severity label: medium.
pub const SEVERITY_MEDIUM: &str = "medium";
/// Severity label: low.
pub const SEVERITY_LOW: &str = "low";

/// Gates proxy packages by known-vulnerability scan results. When enabled, a
/// requested version whose highest advisory severity meets `threshold` is
/// blocked, warned, or audited per `action`. `ignore` lists advisory ids
/// (CVE/GHSA/OSV) accepted as false-positive or risk-accepted.
/// `block_unscanned` blocks a not-yet-scanned version (enforce posture);
/// otherwise the request is served and the coordinate is scanned
/// asynchronously.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VulnPolicyConfig {
    #[serde(deserialize_with = "null_default")]
    pub enabled: bool,
    /// critical|high|medium|low (default high)
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub threshold: String,
    /// block|warn|audit (default audit)
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub action: String,
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_default"
    )]
    pub ignore: Vec<String>,
    #[serde(skip_serializing_if = "is_false", deserialize_with = "null_default")]
    pub block_unscanned: bool,
}

impl VulnPolicyConfig {
    /// Returns `action` with the audit default applied.
    pub fn effective_action(&self) -> &str {
        if self.action.is_empty() {
            return VULN_ACTION_AUDIT;
        }
        &self.action
    }

    /// Returns `threshold` with the high default applied.
    pub fn effective_threshold(&self) -> &str {
        if self.threshold.is_empty() {
            return SEVERITY_HIGH;
        }
        &self.threshold
    }
}

/// Gates proxy packages by their resolved SPDX license(s). When enabled, a
/// requested version is evaluated against `deny` and `allow`: a version
/// carrying any license in `deny` is blocked/warned/audited per `action`; if
/// `allow` is non-empty, a version carrying any license outside `allow` is
/// also gated (allow-list mode). `block_unresolved` gates a not-yet-resolved
/// version (enforce posture); otherwise the request is served and the
/// coordinate is resolved asynchronously. License identifiers are matched
/// case-insensitively.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LicensePolicyConfig {
    #[serde(deserialize_with = "null_default")]
    pub enabled: bool,
    /// block|warn|audit (default audit)
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub action: String,
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_default"
    )]
    pub deny: Vec<String>,
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_default"
    )]
    pub allow: Vec<String>,
    #[serde(skip_serializing_if = "is_false", deserialize_with = "null_default")]
    pub block_unresolved: bool,
}

impl LicensePolicyConfig {
    /// Returns `action` with the audit default applied.
    pub fn effective_action(&self) -> &str {
        if self.action.is_empty() {
            return VULN_ACTION_AUDIT;
        }
        &self.action
    }
}

/// Auto-deletes artifacts that have been idle (not served) for `idle_ttl`,
/// keyed on last_accessed_at (the last-served time). Zero disables it.
/// Applies to proxy (cached) and hosted (uploaded) repositories alike; group
/// repositories hold no artifacts of their own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionConfig {
    #[serde(
        skip_serializing_if = "Duration::is_zero",
        deserialize_with = "null_default"
    )]
    pub idle_ttl: Duration,
}

/// Lists the member repositories of a group repository, in lookup order
/// (first hit wins). Only meaningful when the repository type is "group";
/// membership invariants that need store access (members exist, same format,
/// not themselves groups) are enforced by the API layer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupConfig {
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_default"
    )]
    pub members: Vec<String>,
}

/// Controls proxy caching for a repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    /// Turns caching on. When false a proxy repo passes through to upstream
    /// on every request without persisting blobs.
    #[serde(deserialize_with = "null_default")]
    pub enabled: bool,
    /// How long cached immutable artifacts stay fresh before revalidation.
    /// Zero means never revalidate (immutable).
    #[serde(deserialize_with = "null_default")]
    pub artifact_ttl: Duration,
    #[serde(deserialize_with = "null_default")]
    pub metadata_ttl: Duration,
    /// How long 404 results are cached.
    #[serde(deserialize_with = "null_default")]
    pub negative_ttl: Duration,
    /// Caps the repository cache size; 0 means unbounded.
    #[serde(deserialize_with = "null_default")]
    pub max_size_bytes: i64,
    /// The policy applied when `max_size_bytes` is exceeded. Only "lru".
    #[serde(deserialize_with = "null_default")]
    pub eviction: String,
}

/// Gates artifact versions by their upstream release age. The primary use is
/// a cooldown window: block versions newer than `min_age` to mitigate freshly
/// published malicious packages (supply-chain protection).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgePolicyConfig {
    #[serde(deserialize_with = "null_default")]
    pub enabled: bool,
    /// Requires an upstream release to be at least this old to be served.
    #[serde(deserialize_with = "null_default")]
    pub min_age: Duration,
    /// Optionally blocks releases older than this. Zero disables it.
    #[serde(deserialize_with = "null_default")]
    pub max_age: Duration,
    /// "block" (deny) or "warn" (allow but log/metric).
    #[serde(deserialize_with = "null_default")]
    pub action: String,
}

/// Gates proxy and hosted packages behind an explicit approval decision
/// (quarantine). Unapproved packages are blocked with 403 and queued as
/// pending approval requests. The decision unit is the whole package; version
/// freshness is the age policy's job. Browser uploads to hosted repositories
/// enter this same quarantine immediately after storage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApprovalConfig {
    #[serde(deserialize_with = "null_default")]
    pub enabled: bool,
    /// "enforce" (block unapproved, default) or "audit" (serve but log/count
    /// what enforce would have blocked).
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub mode: String,
    /// Lists `path.Match` glob patterns of package names that bypass approval,
    /// e.g. "@company/*" for an npm scope. Matching is per path segment:
    /// "@company/*" matches "@company/lib" but not "@company/a/b".
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_default"
    )]
    pub auto_approve: Vec<String>,
    /// Admits a package automatically once the exact requested version has a
    /// stored vulnerability scan with no advisories (max severity "none"),
    /// instead of queuing it for manual review. Applies in both enforce and
    /// audit modes and requires a scanner to be configured. Until a clean
    /// verdict exists the request is still blocked/would-blocked and the scan
    /// is queued, so a later request auto-approves once the async scan lands
    /// clean. The per-version vulnerability gate still runs after approval,
    /// so a vulnerable sibling version, or a newly disclosed advisory on this
    /// one, stays gated.
    #[serde(skip_serializing_if = "is_false", deserialize_with = "null_default")]
    pub auto_approve_clean: bool,
}

/// Approval mode: block unapproved packages (default).
pub const MODE_ENFORCE: &str = "enforce";
/// Approval mode: serve but log/count what enforce would have blocked.
pub const MODE_AUDIT: &str = "audit";

impl ApprovalConfig {
    /// Returns the mode with the empty-string default applied.
    pub fn effective_mode(&self) -> &str {
        if self.mode.is_empty() {
            return MODE_ENFORCE;
        }
        &self.mode
    }
}

/// Age policy action: deny.
pub const ACTION_BLOCK: &str = "block";
/// Age policy action: allow but log/metric.
pub const ACTION_WARN: &str = "warn";

/// The only supported cache eviction policy.
pub const EVICTION_LRU: &str = "lru";

/// Returns a sensible default config for a new repository.
pub fn default() -> Config {
    Config {
        cache: CacheConfig {
            enabled: true,
            metadata_ttl: Duration::from_std(std::time::Duration::from_secs(15 * 60)),
            negative_ttl: Duration::from_std(std::time::Duration::from_secs(5 * 60)),
            eviction: EVICTION_LRU.to_string(),
            ..CacheConfig::default()
        },
        age_policy: AgePolicyConfig {
            enabled: false,
            action: ACTION_BLOCK.to_string(),
            ..AgePolicyConfig::default()
        },
        policy_pipeline: PolicyPipelineConfig {
            schema_version: POLICY_PIPELINE_SCHEMA_VERSION,
            order: default_policy_order(),
        },
        ..Config::default()
    }
}

fn merge(base: &mut serde_json::Value, incoming: serde_json::Value) {
    match (base, incoming) {
        (serde_json::Value::Object(b), serde_json::Value::Object(i)) => {
            for (k, v) in i {
                match b.get_mut(&k) {
                    Some(slot) => merge(slot, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (_, serde_json::Value::Null) => {}
        (b, i) => *b = i,
    }
}

/// Decodes and validates a config JSON document, applying defaults for
/// omitted sections.
pub fn parse(raw: &str) -> Result<Config> {
    let mut c = default();
    if !raw.trim().is_empty() && raw != "{}" {
        let incoming: serde_json::Value = serde_json::from_str(raw)?;
        let mut doc = serde_json::to_value(&c)?;
        merge(&mut doc, incoming);
        c = serde_json::from_value(doc)?;
    }
    if c.policy_pipeline.schema_version == LEGACY_POLICY_PIPELINE_VERSION {
        c.policy_pipeline.validate_legacy()?;
        let order = c
            .policy_pipeline
            .order
            .iter()
            .filter(|name| name.as_str() != POLICY_VERSION_DENY)
            .cloned()
            .collect();
        c.policy_pipeline = PolicyPipelineConfig {
            schema_version: POLICY_PIPELINE_SCHEMA_VERSION,
            order,
        };
    }
    c.validate()?;
    Ok(c)
}

impl Config {
    /// Checks invariants.
    pub fn validate(&self) -> Result<()> {
        if !self.cache.eviction.is_empty() && self.cache.eviction != EVICTION_LRU {
            return Err(Error::Invalid(format!(
                "unsupported eviction {:?}",
                self.cache.eviction
            )));
        }
        if self.cache.max_size_bytes < 0 {
            return Err(Error::Invalid("max_size_bytes must be >= 0".into()));
        }
        if self.retention.idle_ttl.nanos() < 0 {
            return Err(Error::Invalid("retention idle_ttl must be >= 0".into()));
        }
        match self.age_policy.action.as_str() {
            "" | ACTION_BLOCK | ACTION_WARN => {}
            other => {
                return Err(Error::Invalid(format!(
                    "unsupported age policy action {other:?}"
                )));
            }
        }
        match self.approval.mode.as_str() {
            "" | MODE_ENFORCE | MODE_AUDIT => {}
            other => {
                return Err(Error::Invalid(format!(
                    "unsupported approval mode {other:?}"
                )));
            }
        }
        for pat in &self.approval.auto_approve {
            if let Err(e) = path_match(pat, "probe") {
                return Err(Error::Invalid(format!(
                    "invalid auto_approve pattern {pat:?}: {e}"
                )));
            }
        }
        match self.vuln.action.as_str() {
            "" | VULN_ACTION_BLOCK | VULN_ACTION_WARN | VULN_ACTION_AUDIT => {}
            other => {
                return Err(Error::Invalid(format!("unsupported vuln action {other:?}")));
            }
        }
        match self.vuln.threshold.as_str() {
            "" | SEVERITY_CRITICAL | SEVERITY_HIGH | SEVERITY_MEDIUM | SEVERITY_LOW => {}
            other => {
                return Err(Error::Invalid(format!(
                    "unsupported vuln threshold {other:?}"
                )));
            }
        }
        match self.license.action.as_str() {
            "" | VULN_ACTION_BLOCK | VULN_ACTION_WARN | VULN_ACTION_AUDIT => {}
            other => {
                return Err(Error::Invalid(format!(
                    "unsupported license action {other:?}"
                )));
            }
        }
        self.policy_pipeline.validate()?;
        self.upstream_auth.validate()?;
        for entry in &self.ip_acl.allow {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            if entry.contains('/') {
                if let Err(e) = entry.parse::<ipnet::IpNet>() {
                    return Err(Error::Invalid(format!(
                        "invalid ip_acl allow entry {entry:?}: {e}"
                    )));
                }
                continue;
            }
            if let Err(e) = entry.parse::<IpAddr>() {
                return Err(Error::Invalid(format!(
                    "invalid ip_acl allow entry {entry:?}: {e}"
                )));
            }
        }
        Ok(())
    }

    /// Serialises the config.
    pub fn json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }
}

// ---------------------------------------------------------------------------
// Duration
// ---------------------------------------------------------------------------

/// A signed nanosecond duration that (un)marshals from human strings, additionally supporting
/// day ("d") and week ("w") suffixes (e.g. "3d", "2w", "72h", "30m").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Duration(pub i64);

impl Duration {
    /// Wraps a nanosecond count.
    pub const fn from_nanos(nanos: i64) -> Self {
        Duration(nanos)
    }

    /// Converts from a `std::time::Duration`, saturating at `i64::MAX`
    /// nanoseconds (about 292 years).
    pub fn from_std(d: std::time::Duration) -> Self {
        Duration(d.as_nanos().min(i64::MAX as u128) as i64)
    }

    /// The signed nanosecond count.
    pub const fn nanos(self) -> i64 {
        self.0
    }

    /// True when zero (the JSON `omitempty` condition).
    pub const fn is_zero(&self) -> bool {
        self.0 == 0
    }

    /// Returns the value as a `std::time::Duration`; a negative value maps to
    /// zero since the standard type is unsigned.
    pub fn d(self) -> std::time::Duration {
        std::time::Duration::from_nanos(self.0.max(0) as u64)
    }
}

impl From<std::time::Duration> for Duration {
    fn from(d: std::time::Duration) -> Self {
        Duration::from_std(d)
    }
}

impl From<Duration> for std::time::Duration {
    fn from(d: Duration) -> Self {
        d.d()
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_duration_nanos(self.0))
    }
}

/// Renders the duration as a string.
impl Serialize for Duration {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&format_duration_nanos(self.0))
    }
}

/// Accepts a string ("3d") or a number (nanoseconds).
impl<'de> Deserialize<'de> for Duration {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl de::Visitor<'_> for V {
            type Value = Duration;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a duration string or a nanosecond count")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Duration, E> {
                parse_duration(v).map_err(E::custom)
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Duration, E> {
                Ok(Duration(v))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Duration, E> {
                i64::try_from(v)
                    .map(Duration)
                    .map_err(|_| E::custom(format!("duration {v} overflows int64")))
            }
        }
        d.deserialize_any(V)
    }
}

/// The empty string and "0" are zero.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() || s == "0" {
        return Ok(Duration(0));
    }
    let n = s.len();
    if n >= 2 {
        let scale = match s.as_bytes()[n - 1] {
            b'd' => Some(24.0 * 3_600e9),
            b'w' => Some(7.0 * 24.0 * 3_600e9),
            _ => None,
        };
        if let Some(scale) = scale {
            let v = s[..n - 1]
                .parse::<f64>()
                .map_err(|e| Error::Invalid(format!("invalid duration {s:?}: {e}")))?;
            return Ok(Duration((v * scale) as i64));
        }
    }
    parse_duration_nanos(s)
        .map(Duration)
        .map_err(|e| Error::Invalid(e.to_string()))
}

// ---------------------------------------------------------------------------
// path.Match
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("syntax error in pattern")]
pub struct BadPattern;

/// The match must span the whole name. The only error is a malformed pattern.
pub fn path_match(pattern: &str, name: &str) -> std::result::Result<bool, BadPattern> {
    let mut pattern = pattern.as_bytes();
    let mut name = name.as_bytes();
    'pattern: while !pattern.is_empty() {
        let (star, chunk, rest) = scan_chunk(pattern);
        pattern = rest;
        if star && chunk.is_empty() {
            // Trailing * matches rest of string unless it has a /.
            return Ok(!name.contains(&b'/'));
        }
        // Look for match at current position.
        let (t, ok) = match_chunk(chunk, name)?;
        // If we're the last chunk, make sure we've exhausted the name;
        // otherwise we'll give a false result even if we could still match
        // using the star.
        if ok && (t.is_empty() || !pattern.is_empty()) {
            name = t;
            continue;
        }
        if star {
            // Look for match skipping i+1 bytes. Cannot skip /.
            let mut i = 0;
            while i < name.len() && name[i] != b'/' {
                let (t, ok) = match_chunk(chunk, &name[i + 1..])?;
                if ok {
                    // If we're the last chunk, make sure we exhausted the name.
                    if pattern.is_empty() && !t.is_empty() {
                        i += 1;
                        continue;
                    }
                    name = t;
                    continue 'pattern;
                }
                i += 1;
            }
        }
        // Before returning false with no error, check that the remainder of
        // the pattern is syntactically valid.
        while !pattern.is_empty() {
            let (_, chunk, rest) = scan_chunk(pattern);
            pattern = rest;
            match_chunk(chunk, b"")?;
        }
        return Ok(false);
    }
    Ok(name.is_empty())
}

/// Gets the next segment of pattern, which is a non-star string possibly
/// preceded by a star.
fn scan_chunk(mut pattern: &[u8]) -> (bool, &[u8], &[u8]) {
    let mut star = false;
    while let Some(b'*') = pattern.first() {
        pattern = &pattern[1..];
        star = true;
    }
    let mut inrange = false;
    let mut i = 0;
    while i < pattern.len() {
        match pattern[i] {
            b'\\' => {
                // Error check handled in match_chunk: bad pattern.
                if i + 1 < pattern.len() {
                    i += 1;
                }
            }
            b'[' => inrange = true,
            b']' => inrange = false,
            b'*' if !inrange => break,
            _ => {}
        }
        i += 1;
    }
    (star, &pattern[..i], &pattern[i..])
}

fn decode_rune(s: &[u8]) -> (u32, usize) {
    const RUNE_ERROR: (u32, usize) = (0xFFFD, 1);
    let Some(&b0) = s.first() else {
        return (0xFFFD, 0);
    };
    let (len, init) = match b0 {
        0x00..=0x7F => return (u32::from(b0), 1),
        0xC2..=0xDF => (2, u32::from(b0 & 0x1F)),
        0xE0..=0xEF => (3, u32::from(b0 & 0x0F)),
        0xF0..=0xF4 => (4, u32::from(b0 & 0x07)),
        _ => return RUNE_ERROR,
    };
    if s.len() < len {
        return RUNE_ERROR;
    }
    let mut r = init;
    for &b in &s[1..len] {
        if b & 0xC0 != 0x80 {
            return RUNE_ERROR;
        }
        r = (r << 6) | u32::from(b & 0x3F);
    }
    match char::from_u32(r) {
        Some(c) if c.len_utf8() == len => (r, len),
        _ => RUNE_ERROR,
    }
}

/// Checks whether chunk matches the beginning of s. If so, it returns the
/// remainder of s (after the match). Chunk is all single-character
/// operators: literals, char classes, and ?.
fn match_chunk<'a>(
    mut chunk: &[u8],
    mut s: &'a [u8],
) -> std::result::Result<(&'a [u8], bool), BadPattern> {
    // `failed` records whether the match has failed. After the match fails,
    // the loop continues on processing chunk, checking that the pattern is
    // well-formed but no longer reading s.
    let mut failed = false;
    while !chunk.is_empty() {
        if !failed && s.is_empty() {
            failed = true;
        }
        match chunk[0] {
            b'[' => {
                // Character class.
                let mut r = 0u32;
                if !failed {
                    let (rr, n) = decode_rune(s);
                    r = rr;
                    s = &s[n..];
                }
                chunk = &chunk[1..];
                // Possibly negated.
                let mut negated = false;
                if let Some(b'^') = chunk.first() {
                    negated = true;
                    chunk = &chunk[1..];
                }
                // Parse all ranges.
                let mut matched = false;
                let mut nrange = 0;
                loop {
                    if chunk.first() == Some(&b']') && nrange > 0 {
                        chunk = &chunk[1..];
                        break;
                    }
                    let (lo, rest) = get_esc(chunk)?;
                    chunk = rest;
                    let mut hi = lo;
                    if chunk[0] == b'-' {
                        let (h, rest) = get_esc(&chunk[1..])?;
                        hi = h;
                        chunk = rest;
                    }
                    if lo <= r && r <= hi {
                        matched = true;
                    }
                    nrange += 1;
                }
                if matched == negated {
                    failed = true;
                }
            }
            b'?' => {
                if !failed {
                    if s[0] == b'/' {
                        failed = true;
                    }
                    let (_, n) = decode_rune(s);
                    s = &s[n..];
                }
                chunk = &chunk[1..];
            }
            c => {
                if c == b'\\' {
                    chunk = &chunk[1..];
                    if chunk.is_empty() {
                        return Err(BadPattern);
                    }
                }
                if !failed {
                    if chunk[0] != s[0] {
                        failed = true;
                    }
                    s = &s[1..];
                }
                chunk = &chunk[1..];
            }
        }
    }
    if failed {
        return Ok((b"", false));
    }
    Ok((s, true))
}

/// Gets a possibly-escaped character from chunk, for a character class.
fn get_esc(mut chunk: &[u8]) -> std::result::Result<(u32, &[u8]), BadPattern> {
    if chunk.is_empty() || chunk[0] == b'-' || chunk[0] == b']' {
        return Err(BadPattern);
    }
    if chunk[0] == b'\\' {
        chunk = &chunk[1..];
        if chunk.is_empty() {
            return Err(BadPattern);
        }
    }
    let (r, n) = decode_rune(chunk);
    if r == 0xFFFD && n == 1 {
        return Err(BadPattern);
    }
    let rest = &chunk[n..];
    if rest.is_empty() {
        return Err(BadPattern);
    }
    Ok((r, rest))
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::repoconfig::*;

    const MINUTE: i64 = 60 * 1_000_000_000;
    const HOUR: i64 = 60 * MINUTE;

    #[test]
    fn parse_duration_cases() {
        let cases = [
            ("", 0),
            ("0", 0),
            ("30m", 30 * MINUTE),
            ("72h", 72 * HOUR),
            ("3d", 3 * 24 * HOUR),
            ("2w", 2 * 7 * 24 * HOUR),
            ("1.5d", 36 * HOUR),
        ];
        for (input, want) in cases {
            let got =
                parse_duration(input).unwrap_or_else(|e| panic!("parse_duration({input:?}): {e}"));
            assert_eq!(got.nanos(), want, "parse_duration({input:?})");
        }
        assert!(
            parse_duration("nonsense").is_err(),
            "expected error for invalid duration"
        );
    }

    #[test]
    fn parse_applies_defaults() {
        let c = parse("").unwrap();
        assert!(c.cache.enabled, "cache should default enabled");
        assert_eq!(
            c.cache.metadata_ttl.d(),
            std::time::Duration::from_secs(15 * 60),
            "metadata ttl default"
        );
        assert_eq!(
            c.age_policy.action, ACTION_BLOCK,
            "age policy action default"
        );
        let want_order = [POLICY_VULNERABILITY, POLICY_LICENSE, POLICY_AGE];
        assert_eq!(
            c.policy_pipeline.effective_order(),
            want_order,
            "policy order"
        );
    }

    #[test]
    fn parse_round_trip() {
        let input = r#"{"cache":{"enabled":true,"artifact_ttl":"1h","max_size_bytes":1048576,"eviction":"lru"},"age_policy":{"enabled":true,"min_age":"3d","action":"block"}}"#;
        let c = parse(input).expect("parse");
        assert_eq!(c.cache.max_size_bytes, 1048576, "max size");
        assert_eq!(c.age_policy.min_age.nanos(), 3 * 24 * HOUR, "min age");
        let out = c.json().unwrap();
        let c2 = parse(&out).expect("reparse");
        assert_eq!(
            c2.age_policy.min_age, c.age_policy.min_age,
            "round trip lost min_age"
        );
    }

    #[test]
    fn validate_rejects_bad_values() {
        let mut bad = Config {
            cache: CacheConfig {
                eviction: "fifo".into(),
                ..CacheConfig::default()
            },
            ..Config::default()
        };
        assert!(
            bad.validate().is_err(),
            "expected eviction validation error"
        );

        bad = Config {
            cache: CacheConfig {
                max_size_bytes: -1,
                ..CacheConfig::default()
            },
            ..Config::default()
        };
        assert!(bad.validate().is_err(), "expected negative size error");

        bad = Config {
            age_policy: AgePolicyConfig {
                action: "drop".into(),
                ..AgePolicyConfig::default()
            },
            ..Config::default()
        };
        assert!(bad.validate().is_err(), "expected action validation error");

        bad = Config {
            approval: ApprovalConfig {
                mode: "quarantine".into(),
                ..ApprovalConfig::default()
            },
            ..Config::default()
        };
        assert!(
            bad.validate().is_err(),
            "expected approval mode validation error"
        );

        bad = Config {
            approval: ApprovalConfig {
                auto_approve: vec!["[invalid".into()],
                ..ApprovalConfig::default()
            },
            ..Config::default()
        };
        assert!(
            bad.validate().is_err(),
            "expected auto_approve pattern validation error"
        );

        bad = Config {
            retention: RetentionConfig {
                idle_ttl: Duration::from_nanos(-1),
            },
            ..Config::default()
        };
        assert!(bad.validate().is_err(), "expected negative idle_ttl error");
    }

    #[test]
    fn policy_pipeline_config() {
        let c = parse(
            r#"{"policy_pipeline":{"schema_version":2,"order":["age","license","vulnerability"]}}"#,
        )
        .unwrap();
        let got = c.policy_pipeline.effective_order();
        assert_eq!(
            got,
            [POLICY_AGE, POLICY_LICENSE, POLICY_VULNERABILITY],
            "effective order"
        );
        let raw = c.json().unwrap();
        let again = parse(&raw).unwrap();
        assert_eq!(
            again.policy_pipeline.effective_order()[0],
            POLICY_AGE,
            "pipeline round trip: {:?}",
            again.policy_pipeline
        );

        let bad_orders = [
            PolicyPipelineConfig {
                schema_version: 3,
                order: vec![
                    POLICY_VULNERABILITY.into(),
                    POLICY_LICENSE.into(),
                    POLICY_AGE.into(),
                ],
            },
            PolicyPipelineConfig {
                schema_version: 2,
                order: vec![
                    POLICY_LICENSE.into(),
                    POLICY_LICENSE.into(),
                    POLICY_AGE.into(),
                ],
            },
            PolicyPipelineConfig {
                schema_version: 2,
                order: vec![POLICY_LICENSE.into(), POLICY_AGE.into()],
            },
            PolicyPipelineConfig {
                schema_version: 2,
                order: vec!["approval".into(), POLICY_LICENSE.into(), POLICY_AGE.into()],
            },
            PolicyPipelineConfig {
                schema_version: 2,
                order: vec![
                    POLICY_VERSION_DENY.into(),
                    POLICY_LICENSE.into(),
                    POLICY_AGE.into(),
                ],
            },
        ];
        for pipeline in bad_orders {
            let mut cfg = default();
            cfg.policy_pipeline = pipeline.clone();
            assert!(
                cfg.validate().is_err(),
                "invalid pipeline accepted: {pipeline:?}"
            );
        }
    }

    #[test]
    fn policy_pipeline_legacy_v1_normalizes_to_v2() {
        let c = parse(
        r#"{"policy_pipeline":{"schema_version":1,"order":["age","version_deny","license","vulnerability"]}}"#,
    )
    .unwrap();
        assert_eq!(
            c.policy_pipeline.schema_version, POLICY_PIPELINE_SCHEMA_VERSION,
            "schema version"
        );
        assert_eq!(
            c.policy_pipeline.effective_order(),
            [POLICY_AGE, POLICY_LICENSE, POLICY_VULNERABILITY],
            "normalized order"
        );
    }

    #[test]
    fn retention_config_round_trip() {
        let c = parse(r#"{"retention":{"idle_ttl":"7d"}}"#).unwrap();
        assert_eq!(
            c.retention.idle_ttl.nanos(),
            7 * 24 * HOUR,
            "idle_ttl, want 168h"
        );
        let raw = c.json().unwrap();
        let again = parse(&raw).unwrap();
        assert_eq!(
            again.retention.idle_ttl, c.retention.idle_ttl,
            "round-trip idle_ttl"
        );
    }

    #[test]
    fn approval_config() {
        let input = r#"{"approval":{"enabled":true,"mode":"audit","auto_approve":["@company/*","left-*"]}}"#;
        let c = parse(input).expect("parse");
        assert!(
            c.approval.enabled && c.approval.effective_mode() == MODE_AUDIT,
            "approval = {:?}",
            c.approval
        );
        assert_eq!(c.approval.auto_approve.len(), 2, "auto_approve");
        let out = c.json().unwrap();
        let c2 = parse(&out).expect("reparse");
        assert!(
            c2.approval.mode == MODE_AUDIT && c2.approval.auto_approve.len() == 2,
            "round trip lost approval config"
        );
        // Defaults: disabled, effective mode enforce.
        let d = parse("").unwrap();
        assert!(
            !d.approval.enabled && d.approval.effective_mode() == MODE_ENFORCE,
            "approval default = {:?}",
            d.approval
        );
    }

    #[test]
    fn ipacl_allowed() {
        let acl = IPACLConfig {
            enabled: true,
            allow: vec![
                "203.0.113.5".into(),
                "10.0.0.0/16".into(),
                "2001:db8::/32".into(),
                "  ".into(),
                "bogus".into(),
            ],
        };
        let cases = [
            ("203.0.113.5", true),        // exact IPv4
            ("203.0.113.6", false),       // outside
            ("10.0.5.7", true),           // inside CIDR
            ("10.1.0.1", false),          // outside CIDR
            ("2001:db8::1", true),        // inside IPv6 CIDR
            ("2001:dead::1", false),      // outside IPv6 CIDR
            ("::ffff:203.0.113.5", true), // IPv4-mapped form of an allowed v4
            ("not-an-ip", false),         // unparseable client ip denied
        ];
        for (ip, want) in cases {
            assert_eq!(acl.allowed(ip), want, "allowed({ip:?})");
        }
    }

    #[test]
    fn ipacl_validate() {
        let mut good = default();
        good.ip_acl = IPACLConfig {
            enabled: true,
            allow: vec!["10.0.0.0/8".into(), "203.0.113.1".into(), " ".into()],
        };
        good.validate().expect("valid ip_acl rejected");

        let mut bad = default();
        bad.ip_acl = IPACLConfig {
            enabled: true,
            allow: vec!["10.0.0.0/99".into()],
        };
        assert!(bad.validate().is_err(), "invalid CIDR accepted");

        let mut bad2 = default();
        bad2.ip_acl = IPACLConfig {
            enabled: false,
            allow: vec!["not-an-ip".into()],
        };
        assert!(bad2.validate().is_err(), "invalid address accepted");
    }

    #[test]
    fn upstream_auth_validate() {
        let valid = [
            UpstreamAuthConfig::default(),
            UpstreamAuthConfig {
                type_: UPSTREAM_AUTH_BASIC.into(),
                username: "u".into(),
                password: "p".into(),
                ..UpstreamAuthConfig::default()
            },
            UpstreamAuthConfig {
                type_: UPSTREAM_AUTH_BASIC.into(),
                username: "token-only".into(),
                ..UpstreamAuthConfig::default()
            },
            UpstreamAuthConfig {
                type_: UPSTREAM_AUTH_BEARER.into(),
                token: "t".into(),
                ..UpstreamAuthConfig::default()
            },
            UpstreamAuthConfig {
                type_: UPSTREAM_AUTH_HEADER.into(),
                header: "X-Api-Key".into(),
                value: "k".into(),
                ..UpstreamAuthConfig::default()
            },
        ];
        for (i, u) in valid.iter().enumerate() {
            u.validate().unwrap_or_else(|e| panic!("valid[{i}]: {e}"));
        }
        let invalid = [
            UpstreamAuthConfig {
                type_: "ntlm".into(),
                ..UpstreamAuthConfig::default()
            },
            UpstreamAuthConfig {
                type_: UPSTREAM_AUTH_BASIC.into(),
                ..UpstreamAuthConfig::default()
            },
            UpstreamAuthConfig {
                type_: UPSTREAM_AUTH_BEARER.into(),
                ..UpstreamAuthConfig::default()
            },
            UpstreamAuthConfig {
                type_: UPSTREAM_AUTH_HEADER.into(),
                header: "X-Api-Key".into(),
                ..UpstreamAuthConfig::default()
            },
            UpstreamAuthConfig {
                type_: UPSTREAM_AUTH_HEADER.into(),
                value: "k".into(),
                ..UpstreamAuthConfig::default()
            },
            UpstreamAuthConfig {
                type_: UPSTREAM_AUTH_HEADER.into(),
                header: "bad name".into(),
                value: "k".into(),
                ..UpstreamAuthConfig::default()
            },
            UpstreamAuthConfig {
                type_: UPSTREAM_AUTH_HEADER.into(),
                header: "evil:inject".into(),
                value: "k".into(),
                ..UpstreamAuthConfig::default()
            },
        ];
        for (i, u) in invalid.iter().enumerate() {
            assert!(u.validate().is_err(), "invalid[{i}]: expected error");
        }
    }

    #[test]
    fn upstream_auth_apply() {
        use base64::Engine as _;

        let mut headers = http::HeaderMap::new();
        UpstreamAuthConfig::default().apply_headers(&mut headers);
        assert!(
            headers.get(http::header::AUTHORIZATION).is_none(),
            "empty auth must not set headers"
        );

        UpstreamAuthConfig {
            type_: UPSTREAM_AUTH_BASIC.into(),
            username: "u".into(),
            password: "p".into(),
            ..UpstreamAuthConfig::default()
        }
        .apply_headers(&mut headers);
        let got = headers
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let want = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("u:p")
        );
        assert_eq!(got, want, "basic auth");

        let mut headers = http::HeaderMap::new();
        UpstreamAuthConfig {
            type_: UPSTREAM_AUTH_BEARER.into(),
            token: "tok".into(),
            ..UpstreamAuthConfig::default()
        }
        .apply_headers(&mut headers);
        assert_eq!(
            headers
                .get(http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer tok"),
            "bearer"
        );

        let mut headers = http::HeaderMap::new();
        UpstreamAuthConfig {
            type_: UPSTREAM_AUTH_HEADER.into(),
            header: "X-Api-Key".into(),
            value: "k".into(),
            ..UpstreamAuthConfig::default()
        }
        .apply_headers(&mut headers);
        assert_eq!(
            headers.get("X-Api-Key").and_then(|v| v.to_str().ok()),
            Some("k"),
            "header"
        );
    }

    /// The reqwest form of the same behaviour: the builder carries the credentials
    /// onto the built request.
    #[test]
    fn upstream_auth_apply_request_builder() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();
        let client = reqwest::Client::builder().build().unwrap();
        let build = |auth: &UpstreamAuthConfig| {
            auth.apply(client.get("https://upstream.example.com/x"))
                .build()
                .unwrap()
        };

        let req = build(&UpstreamAuthConfig::default());
        assert!(
            req.headers().get(http::header::AUTHORIZATION).is_none(),
            "empty auth must not set headers"
        );

        let req = build(&UpstreamAuthConfig {
            type_: UPSTREAM_AUTH_BEARER.into(),
            token: "tok".into(),
            ..UpstreamAuthConfig::default()
        });
        assert_eq!(
            req.headers()
                .get(http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer tok")
        );

        let req = build(&UpstreamAuthConfig {
            type_: UPSTREAM_AUTH_HEADER.into(),
            header: "X-Api-Key".into(),
            value: "k".into(),
            ..UpstreamAuthConfig::default()
        });
        assert_eq!(
            req.headers().get("X-Api-Key").and_then(|v| v.to_str().ok()),
            Some("k")
        );
    }

    #[test]
    fn upstream_auth_mask_round_trip() {
        let stored = UpstreamAuthConfig {
            type_: UPSTREAM_AUTH_BASIC.into(),
            username: "u".into(),
            password: "s3cret".into(),
            ..UpstreamAuthConfig::default()
        };
        let mut masked = stored.masked();
        assert!(
            masked.password == SECRET_MASK && masked.username == "u",
            "masked = {masked:?}"
        );
        // A client edits the username but returns the masked password untouched.
        masked.username = "u2".into();
        let restored = masked.unmask_from(&stored);
        assert!(
            restored.password == "s3cret" && restored.username == "u2",
            "restored = {restored:?}"
        );
        // A replaced secret passes through as-is.
        masked.password = "new-secret".into();
        let got = masked.unmask_from(&stored);
        assert_eq!(got.password, "new-secret", "replaced = {got:?}");
        // Empty auth masks to empty and survives config round-trip omitted.
        assert_eq!(
            UpstreamAuthConfig::default().masked(),
            UpstreamAuthConfig::default(),
            "empty auth should mask to empty"
        );
        let out = default().json().unwrap();
        assert!(
            !out.contains(r#""upstream_auth""#),
            "empty upstream_auth should be omitted: {out}"
        );
    }

    // --- path.Match (pattern-matching coverage) ---------------

    #[test]
    fn path_match_matches_go() {
        for (pattern, name, want) in [
            ("@company/*", "@company/lib", true),
            ("@company/*", "@company/a/b", false),
            ("left-*", "left-pad", true),
            ("left-*", "right-pad", false),
            ("*", "anything", true),
            ("*", "with/slash", false),
            ("a?c", "abc", true),
            ("a?c", "a/c", false),
            ("[a-c]x", "bx", true),
            ("[a-c]x", "dx", false),
            ("[^a-c]x", "dx", true),
            ("probe", "probe", true),
            (r"\*", "*", true),
        ] {
            assert_eq!(
                path_match(pattern, name).unwrap(),
                want,
                "path_match({pattern:?}, {name:?})"
            );
        }
        for pattern in ["[invalid", "[", "[]", "a[b", r"x\"] {
            assert!(
                path_match(pattern, "probe").is_err(),
                "path_match({pattern:?}) should be a bad pattern"
            );
        }
    }
}
