//! Domain types shared by the rules, the engine, and both HTTP surfaces.

use serde::Serialize;

/// Status values Argo CD publishes on an Application.
pub const SYNC_SYNCED: &str = "Synced";
pub const HEALTH_HEALTHY: &str = "Healthy";
pub const STATUS_UNKNOWN: &str = "Unknown";

/// One Argo CD Application reduced to the fields the gate reasons about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppSnapshot {
    /// `metadata.name`, for example `prd-payment-api`.
    pub name: String,
    /// `spec.project`, which doubles as the environment name.
    pub project: String,
    /// The environment-independent app identity, `payment-api`.
    pub identity: String,
    /// `status.sync.status`, empty when Argo CD has not reconciled.
    pub sync_status: String,
    /// `status.health.status`, empty when not reconciled.
    pub health_status: String,
    /// `status.summary.images`, the images currently running.
    pub live_images: Vec<ImageRef>,
    /// True when the app carries the skip annotation.
    pub skip_requested: bool,
    /// `operation.sync.revision` on a pending sync, empty when the object
    /// carries no operation.
    pub pending_revision: String,
    /// Revisions from `status.history`, which is what makes a rollback
    /// recognisable without asking Argo CD anything.
    pub deployed_revisions: Vec<String>,
    /// `status.sync.revision`, the revision already live here.
    pub current_revision: String,
}

impl AppSnapshot {
    /// Reports whether the pending sync goes back to a revision this
    /// application has already deployed.
    ///
    /// Two conditions, and the second one matters. The target must appear in
    /// this application's own history, and it must not be the revision already
    /// live here. Without that second test an application whose revision is a
    /// chart version rather than a git commit would look like a rollback on
    /// every sync, because its revision never changes even when the desired
    /// state does.
    #[must_use]
    pub fn is_rollback(&self) -> bool {
        if self.pending_revision.is_empty() || self.pending_revision == self.current_revision {
            return false;
        }
        self.deployed_revisions.contains(&self.pending_revision)
    }

    /// Reports whether Argo CD considers the app in sync with git.
    #[must_use]
    pub fn is_synced(&self) -> bool {
        self.sync_status == SYNC_SYNCED
    }

    /// Reports whether Argo CD considers the app's resources healthy.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.health_status == HEALTH_HEALTHY
    }

    /// `sync_status` with a printable fallback.
    #[must_use]
    pub fn sync_or_unknown(&self) -> &str {
        if self.sync_status.is_empty() {
            STATUS_UNKNOWN
        } else {
            &self.sync_status
        }
    }

    /// `health_status` with a printable fallback.
    #[must_use]
    pub fn health_or_unknown(&self) -> &str {
        if self.health_status.is_empty() {
            STATUS_UNKNOWN
        } else {
            &self.health_status
        }
    }
}

/// A parsed container image reference.
///
/// Environments in one estate routinely pull the same application image from
/// different registries, one account per environment, so the comparable part
/// of a reference is the repository's last path segment rather than the fully
/// qualified repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageRef {
    /// The reference exactly as it appeared in the manifest.
    pub raw: String,
    /// The fully qualified repository without tag or digest.
    pub repository: String,
    /// The last path segment of `repository`, `payment-api`.
    pub basename: String,
    /// Empty when the reference is digest-pinned.
    pub tag: String,
    /// Empty when the reference is tag-based.
    pub digest: String,
}

impl ImageRef {
    /// The comparable identifier: the tag when present, else the digest.
    #[must_use]
    pub fn reference(&self) -> &str {
        if self.tag.is_empty() {
            &self.digest
        } else {
            &self.tag
        }
    }
}

/// The outcome of comparing one repository basename across two environments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageComparison {
    pub repository: String,
    pub desired_tag: String,
    pub upstream_tag: String,
    pub matched: bool,
}

/// The upstream environment summary reported to callers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamStatus {
    pub app: String,
    pub env: String,
    pub exists: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub sync_status: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub health_status: String,
}

/// The machine-readable reason for a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum Code {
    /// The environment is outside the chain, or is its head.
    NotGated,
    /// The principal or the Application opted out.
    Exempt,
    /// The sync targets a revision this environment has already deployed.
    Rollback,
    /// No upstream Application exists for this identity.
    UpstreamMissing,
    /// The upstream exists but is not Synced.
    UpstreamOutOfSync,
    /// The upstream is Synced but not Healthy.
    UpstreamUnhealthy,
    /// The upstream runs a different image tag than this sync would deploy.
    ImageTagMismatch,
    /// A fact the verdict needs could not be read.
    LookupFailed,
    /// Every configured check passed.
    Passed,
}

impl Code {
    /// The label value, identical to the JSON representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotGated => "NotGated",
            Self::Exempt => "Exempt",
            Self::Rollback => "Rollback",
            Self::UpstreamMissing => "UpstreamMissing",
            Self::UpstreamOutOfSync => "UpstreamOutOfSync",
            Self::UpstreamUnhealthy => "UpstreamUnhealthy",
            Self::ImageTagMismatch => "ImageTagMismatch",
            Self::LookupFailed => "LookupFailed",
            Self::Passed => "Passed",
        }
    }
}

impl std::fmt::Display for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The gate verdict, shared by the admission webhook and the UI extension API
/// so both always agree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub app: String,
    pub env: String,
    pub identity: String,
    /// False when the environment is outside the configured chain.
    pub gated: bool,
    pub allowed: bool,
    pub code: Code,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<UpstreamStatus>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageComparison>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl Decision {
    /// The verdict for an environment the gate does not cover.
    #[must_use]
    pub fn not_gated(app: &str, env: &str, identity: &str) -> Self {
        Self {
            app: app.to_string(),
            env: env.to_string(),
            identity: identity.to_string(),
            gated: false,
            allowed: true,
            code: Code::NotGated,
            message: format!(
                "Sync of {app} is allowed. The environment {env} is not gated by the promotion gate either because it is absent from the configured promotion chain or because it sits at the head of that chain and so has no upstream environment to wait for. No upstream state was read and no image tag was compared."
            ),
            upstream: None,
            images: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// A gated verdict skeleton with the outcome still to be filled in.
    pub(crate) fn gated(app: &AppSnapshot) -> Self {
        Self {
            app: app.name.clone(),
            env: app.project.clone(),
            identity: app.identity.clone(),
            gated: true,
            allowed: false,
            code: Code::LookupFailed,
            message: String::new(),
            upstream: None,
            images: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_needs_history_and_a_different_current_revision() {
        let mut app = AppSnapshot {
            pending_revision: "abc".into(),
            current_revision: "def".into(),
            deployed_revisions: vec!["abc".into(), "def".into()],
            ..AppSnapshot::default()
        };
        assert!(app.is_rollback());

        app.current_revision = "abc".into();
        assert!(
            !app.is_rollback(),
            "same revision as live is not a rollback"
        );

        app.current_revision = "def".into();
        app.deployed_revisions.clear();
        assert!(
            !app.is_rollback(),
            "revision outside history is not a rollback"
        );

        app.pending_revision.clear();
        assert!(!app.is_rollback(), "no pending revision is not a rollback");
    }

    #[test]
    fn status_fallbacks_print_unknown() {
        let app = AppSnapshot::default();
        assert_eq!(app.sync_or_unknown(), STATUS_UNKNOWN);
        assert_eq!(app.health_or_unknown(), STATUS_UNKNOWN);
        assert!(!app.is_synced());
        assert!(!app.is_healthy());

        let app = AppSnapshot {
            sync_status: SYNC_SYNCED.into(),
            health_status: HEALTH_HEALTHY.into(),
            ..AppSnapshot::default()
        };
        assert_eq!(app.sync_or_unknown(), SYNC_SYNCED);
        assert_eq!(app.health_or_unknown(), HEALTH_HEALTHY);
        assert!(app.is_synced());
        assert!(app.is_healthy());
    }

    #[test]
    fn reference_prefers_tag_over_digest() {
        let tagged = ImageRef {
            tag: "1.0".into(),
            digest: "sha256:abc".into(),
            ..ImageRef::default()
        };
        assert_eq!(tagged.reference(), "1.0");
        let pinned = ImageRef {
            digest: "sha256:abc".into(),
            ..ImageRef::default()
        };
        assert_eq!(pinned.reference(), "sha256:abc");
    }

    #[test]
    fn decision_json_omits_empty_optionals() {
        let verdict = Decision::not_gated("dev-app", "dev", "app");
        let json = serde_json::to_value(&verdict).unwrap();
        assert_eq!(json["code"], "NotGated");
        assert_eq!(json["gated"], false);
        assert_eq!(json["allowed"], true);
        assert!(json.get("upstream").is_none());
        assert!(json.get("images").is_none());
        assert!(json.get("warnings").is_none());
        assert_eq!(Code::ImageTagMismatch.to_string(), "ImageTagMismatch");
    }
}
