//! Reads everything the rules weigh into one in-memory snapshot, so each rule
//! is a pure function and a run costs a fixed number of queries.

use std::collections::HashMap;

use chrono::{DateTime, TimeDelta, Utc};

use crate::coverage;
use crate::meta::{self, Store};
use crate::repoconfig;
use crate::storage;

/// Refused requests are judged over the last day, policy blocks over a week.
pub const AUTH_WINDOW: TimeDelta = TimeDelta::hours(24);
pub const BLOCK_WINDOW: TimeDelta = TimeDelta::days(7);
/// Artifacts unserved this long count as unused.
pub const STALE_AFTER: TimeDelta = TimeDelta::days(180);
/// Bounds how many pending approvals one run reads.
const PENDING_LIMIT: i64 = 2_000;

/// A repository with its parsed configuration. A repository whose config does
/// not parse is analysed with defaults rather than skipped.
#[derive(Debug, Clone)]
pub struct RepoView {
    pub repo: meta::Repository,
    pub cfg: repoconfig::Config,
}

/// Everything one analyzer run reads.
#[derive(Debug, Clone, Default)]
pub struct DataSnapshot {
    pub now: DateTime<Utc>,
    pub repos: Vec<RepoView>,
    pub row_counts: HashMap<i64, i64>,
    pub artifacts: Vec<meta::InsightArtifact>,
    pub vulns: HashMap<String, meta::VulnSummary>,
    pub licenses: HashMap<String, Vec<String>>,
    pub oci_tags: HashMap<i64, Vec<meta::OCITag>>,
    pub stale: HashMap<i64, (i64, i64)>,
    pub users: Vec<meta::User>,
    pub roles_by_user: HashMap<i64, Vec<meta::Role>>,
    pub roles: Vec<meta::Role>,
    pub group_mappings: Vec<meta::GroupMapping>,
    /// The role every authenticated user holds (`FORKLIFT_RBAC_DEFAULT_ROLE`),
    /// empty when there is none.
    pub default_role: String,
    pub permissions: Vec<meta::Permission>,
    pub tokens: Vec<meta::Token>,
    pub auth_audit: Vec<meta::AuditCount>,
    pub block_audit: Vec<meta::AuditCount>,
    pub pending: Vec<meta::PackageApproval>,
    pub coverage: Option<CoverageView>,
    /// The filesystem holding the data directory, `None` when it cannot be
    /// measured.
    pub disk: Option<storage::Disk>,
}

/// The last completed Forklift coverage scan, when coverage is enabled.
#[derive(Debug, Clone, Default)]
pub struct CoverageView {
    pub percent: i64,
    pub target: i64,
    pub projects: Vec<coverage::Project>,
}

impl DataSnapshot {
    pub fn repo(&self, id: i64) -> Option<&RepoView> {
        self.repos.iter().find(|r| r.repo.id == id)
    }

    pub fn repo_by_name(&self, name: &str) -> Option<&RepoView> {
        self.repos.iter().find(|r| r.repo.name == name)
    }
}

/// Reads one snapshot at `now`.
pub async fn collect(
    store: &Store,
    coverage: Option<&coverage::Scanner>,
    default_role: &str,
    now: DateTime<Utc>,
) -> Result<DataSnapshot, meta::Error> {
    let repos = store
        .list_repositories()
        .await?
        .into_iter()
        .map(|repo| {
            let cfg = repoconfig::parse(&repo.config_json).unwrap_or_default();
            RepoView { repo, cfg }
        })
        .collect::<Vec<_>>();
    let mut oci_tags = HashMap::new();
    for view in &repos {
        if view.repo.format == meta::FORMAT_OCI && view.repo.r#type != meta::TYPE_GROUP {
            oci_tags.insert(view.repo.id, store.list_oci_tag_rows(view.repo.id).await?);
        }
    }
    Ok(DataSnapshot {
        now,
        row_counts: store
            .all_repo_stats()
            .await?
            .into_iter()
            .map(|(id, st)| (id, st.artifact_count))
            .collect(),
        artifacts: store.insight_artifacts().await?,
        vulns: store.vuln_scan_index().await?,
        licenses: store.license_index().await?,
        oci_tags,
        stale: store.stale_artifact_totals(now - STALE_AFTER).await?,
        users: store.list_users().await?,
        roles_by_user: store.roles_by_user().await?,
        roles: store.list_roles().await?,
        group_mappings: store.list_group_mappings().await?,
        default_role: default_role.to_string(),
        permissions: store.list_permissions().await?,
        tokens: store.list_all_tokens().await?,
        auth_audit: store.audit_security_counts(now - AUTH_WINDOW).await?,
        block_audit: store.audit_security_counts(now - BLOCK_WINDOW).await?,
        pending: store
            .list_approvals("", meta::APPROVAL_PENDING, PENDING_LIMIT, 0)
            .await?,
        coverage: coverage.and_then(|scanner| {
            if !scanner.enabled() {
                return None;
            }
            let overview = scanner.overview();
            overview.last_scanned_at?;
            Some(CoverageView {
                percent: overview.summary.percent,
                target: overview.summary.target,
                projects: overview.projects,
            })
        }),
        disk: store
            .path()
            .parent()
            .map(|dir| {
                if dir.as_os_str().is_empty() {
                    std::path::Path::new(".")
                } else {
                    dir
                }
            })
            .and_then(|dir| storage::disk_usage(dir).ok()),
        repos,
    })
}
