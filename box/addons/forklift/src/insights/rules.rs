//! The insight rules. Each one reads the snapshot and returns at most one
//! finding, so a rule is tested by building a snapshot, never a database.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{DateTime, TimeDelta, Utc};

use crate::meta::{self, coordinate_key};
use crate::repo;

use super::collect::{DataSnapshot, RepoView};
use super::model::{
    AFFECTED_LIMIT, Affected, Audience, Category, Check, CheckStatus, Finding, Severity,
};

/// Served within this window counts as in use.
const IN_USE: TimeDelta = TimeDelta::days(30);
/// Accounts and tokens idle this long count as dormant.
const DORMANT: TimeDelta = TimeDelta::days(90);
/// Pending approvals older than this are aging.
const APPROVAL_AGING: TimeDelta = TimeDelta::days(7);
/// More global administrators than this is sprawl.
const ADMIN_LIMIT: usize = 3;
/// A client refused at least this often in a day is reported, and ten times
/// as often is high.
const AUTH_FAILURE_THRESHOLD: i64 = 20;
/// A repository needs this many scannable artifacts before its gap counts.
const SCAN_GAP_MIN: i64 = 10;
/// Unscanned share above which a repository has a scan gap.
const SCAN_GAP_RATIO: f64 = 0.2;
/// Unused artifacts a repository needs before retention is suggested.
const STALE_MIN: i64 = 10;

/// Runs every rule and reports each one as a check, passing or not.
pub fn evaluate(s: &DataSnapshot) -> (Vec<Finding>, Vec<Check>) {
    let findings = findings(s);
    let checks = CATALOG
        .iter()
        .map(|r| {
            let found = findings.iter().find(|f| f.id == r.id);
            let status = match found {
                Some(f) => CheckStatus::of(f.severity),
                None if r.id == "coverage-bypass" && s.coverage.is_none() => {
                    CheckStatus::Unavailable
                }
                None => CheckStatus::Ok,
            };
            Check {
                id: r.id.to_string(),
                code: r.code.to_string(),
                category: r.category,
                audiences: r.audiences.to_vec(),
                title: r.title.to_string(),
                rationale: r.rationale.to_string(),
                criteria: r.criteria.to_string(),
                recommendation: r.recommendation.to_string(),
                status,
                flagged: found.map_or(0, |f| f.total),
                acknowledgement: None,
            }
        })
        .collect();
    (findings, checks)
}

/// Runs every rule, keeping the ones that fired.
fn findings(s: &DataSnapshot) -> Vec<Finding> {
    let index = ArtifactIndex::build(s);
    let admins = admin_user_ids(s);
    [
        vuln_in_use(s, &index),
        vuln_dormant(s, &index),
        vuln_moderate(s, &index),
        vuln_widespread(s, &index),
        vuln_accepted(s),
        scan_gap(s, &index),
        copyleft_licenses(s, &index),
        oci_unsigned(s),
        oci_latest_tag(s),
        public_repositories(s),
        plaintext_upstream(s),
        vuln_policy_off(s),
        age_policy_off(s),
        license_policy_off(s),
        retention_off(s),
        admin_sprawl(s, &admins),
        stale_accounts(s),
        locked_accounts(s),
        tokens_without_expiry(s, &admins),
        unscoped_admin_tokens(s, &admins),
        unused_tokens(s),
        auth_failures(s),
        policy_blocks(s),
        aging_approvals(s),
        coverage_bypass(s),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// What every rule checks, why it matters and what to do, independent of
/// whether it fired. Modeled on a Trusted Advisor check: category, description,
/// alert criteria and recommended action.
pub struct Rule {
    pub id: &'static str,
    /// Stable short code, after Security Hub controls: category prefix (SC
    /// supply chain, PO policy, EX exposure, AC access, OP operations) and a
    /// number. Never renumbered or reused once published.
    pub code: &'static str,
    pub category: Category,
    pub audiences: &'static [Audience],
    pub title: &'static str,
    pub rationale: &'static str,
    pub criteria: &'static str,
    pub recommendation: &'static str,
}

pub const CATALOG: &[Rule] = &[
    Rule {
        id: "vuln-in-use",
        code: "SC-01",
        category: Category::SupplyChain,
        audiences: &[Audience::Developer, Audience::Security],
        title: "Vulnerable versions still being downloaded",
        rationale: "Builds that download these versions ship code with known, often exploitable flaws into your services. A version served this month is in active use, so this is the shortest path from a published advisory to an incident.",
        criteria: "Red (critical): a stored package version with a critical advisory was served in the last 30 days. Red (high): the same with a high advisory and no critical one.",
        recommendation: "Upgrade consumers to a fixed version, then block the vulnerable versions with a version deny or a vulnerability policy set to block.",
    },
    Rule {
        id: "vuln-dormant",
        code: "SC-02",
        category: Category::SupplyChain,
        audiences: &[Audience::Devops, Audience::Security],
        title: "Vulnerable versions stored but unused",
        rationale: "Unused vulnerable versions do no harm until a lockfile, a rollback or a forgotten branch pulls them again, and then they arrive without anyone re-checking them. They also occupy storage.",
        criteria: "Yellow: a stored package version with a critical or high advisory has not been served for 30 days.",
        recommendation: "Delete them, or turn on idle retention so unused versions leave on their own before a build pulls them again.",
    },
    Rule {
        id: "vuln-moderate",
        code: "SC-03",
        category: Category::SupplyChain,
        audiences: &[Audience::Developer],
        title: "Versions with medium or low advisories in use",
        rationale: "Medium and low advisories are rarely urgent alone, but they pile up, get chained with others, and are cheapest to fix while the upgrade is small.",
        criteria: "Yellow (low): a stored package version whose worst advisory is medium or low was served in the last 30 days.",
        recommendation: "Pick them up in routine dependency updates, starting with the most downloaded packages.",
    },
    Rule {
        id: "vuln-widespread",
        code: "SC-04",
        category: Category::SupplyChain,
        audiences: &[Audience::Security, Audience::Developer],
        title: "Advisories spread across many versions",
        rationale: "One advisory present in many stored versions means many teams depend on the affected package. Fixing it centrally, with one deny rule or one shared upgrade, removes the exposure everywhere at once.",
        criteria: "Yellow (medium): a high advisory affects 3 or more stored versions. Red (high): the same for a critical advisory.",
        recommendation: "Deny the affected versions in the vulnerability policy or a version deny, and announce the fixed version to the teams that pull it.",
    },
    Rule {
        id: "vuln-accepted",
        code: "PO-04",
        category: Category::Policy,
        audiences: &[Audience::Security],
        title: "Advisories ignored by policy",
        rationale: "An ignored advisory is an accepted risk. Acceptances made once are rarely revisited, even after a fix ships or the advisory is upgraded.",
        criteria: "Info: a repository's vulnerability policy has advisory ids on its ignore list.",
        recommendation: "Review the ignore list on a schedule and remove entries whose reason no longer holds or that now have a fixed version.",
    },
    Rule {
        id: "scan-gap",
        code: "SC-05",
        category: Category::SupplyChain,
        audiences: &[Audience::Devops, Audience::Security],
        title: "Repositories with a vulnerability scan gap",
        rationale: "A version without a scan has no advisory data, so the vulnerability policy cannot judge it and the dashboards report it as clean by omission.",
        criteria: "Yellow: more than 20% of a repository's package versions (at least 10 versions) have no vulnerability scan.",
        recommendation: "Check that the scanner is configured and reachable, and that its backfill is keeping up. Unscanned versions pass the vulnerability policy unless it blocks unscanned versions.",
    },
    Rule {
        id: "copyleft-licenses",
        code: "SC-06",
        category: Category::SupplyChain,
        audiences: &[Audience::Security, Audience::Developer],
        title: "Strong copyleft licenses in stored packages",
        rationale: "Strong copyleft can oblige you to publish your own source when you distribute (GPL, EUPL, OSL) or serve over a network (AGPL, SSPL) software that includes the package.",
        criteria: "Yellow (medium): a stored version is under AGPL or SSPL. Yellow (low): a stored version is under GPL, EUPL or OSL.",
        recommendation: "Confirm each use with legal, and deny the licenses you do not accept in the repository license policy so new versions are stopped at the door.",
    },
    Rule {
        id: "oci-unsigned",
        code: "SC-07",
        category: Category::SupplyChain,
        audiences: &[Audience::Devops, Audience::Developer],
        title: "Images without a signature",
        rationale: "Without a signature nothing proves an image came from your pipeline. An image pushed or swapped under the same tag by someone else runs as if it were yours.",
        criteria: "Yellow: a tag in a hosted OCI repository has no cosign signature tag (sha256-<digest>.sig) for its manifest.",
        recommendation: "Sign images in CI (cosign sign) and verify signatures at admission, so only images your pipeline built can run.",
    },
    Rule {
        id: "oci-latest-tag",
        code: "SC-08",
        category: Category::SupplyChain,
        audiences: &[Audience::Developer],
        title: "Images published under the mutable latest tag",
        rationale: "latest points at whatever was pushed last, so two deployments of the same manifest can run different code and a rollback to latest does not roll back.",
        criteria: "Yellow: a hosted OCI repository has an image tagged latest.",
        recommendation: "Deploy by immutable version tag or digest, and keep latest for local use only.",
    },
    Rule {
        id: "public-repositories",
        code: "EX-01",
        category: Category::Exposure,
        audiences: &[Audience::Security, Audience::Devops],
        title: "Repositories open to anonymous downloads",
        rationale: "Anyone who can reach forklift can download from these repositories without credentials. Hosted repositories hold your internal packages and images.",
        criteria: "Red (high): a hosted repository allows anonymous downloads. Yellow (medium): only proxy or group repositories do.",
        recommendation: "Make them private unless anonymous access is intended, and restrict them by source IP when it is.",
    },
    Rule {
        id: "plaintext-upstream",
        code: "EX-02",
        category: Category::Exposure,
        audiences: &[Audience::Devops],
        title: "Proxies fetching over plain HTTP",
        rationale: "Over plain HTTP anyone on the network path can read or replace what forklift caches, and the poisoned copy is then served to every build.",
        criteria: "Red (high): a proxy repository's upstream URL starts with http://.",
        recommendation: "Point the upstream at its https:// address.",
    },
    Rule {
        id: "vuln-policy-off",
        code: "PO-01",
        category: Category::Policy,
        audiences: &[Audience::Security, Audience::Devops],
        title: "Vulnerability policy not enforcing",
        rationale: "With the policy off or audit-only, a version with a known critical advisory is served like any other, and nobody is told at install time.",
        criteria: "Yellow: a repository whose format has vulnerability scanning has the vulnerability policy off, or on with action audit.",
        recommendation: "Enable the vulnerability policy with action block (or warn first) at threshold high.",
    },
    Rule {
        id: "age-policy-off",
        code: "PO-02",
        category: Category::Policy,
        audiences: &[Audience::Security, Audience::Devops],
        title: "Proxies without a release-age cooldown",
        rationale: "Malicious or hijacked releases are usually found and pulled upstream within days. Without a cooldown, builds fetch them in the first minutes.",
        criteria: "Yellow: a proxy repository has the age policy off.",
        recommendation: "Enable the age policy (for example 3 days) so a hijacked or malicious release is usually pulled upstream before builds can fetch it.",
    },
    Rule {
        id: "license-policy-off",
        code: "PO-03",
        category: Category::Policy,
        audiences: &[Audience::Security],
        title: "No license policy",
        rationale: "Without a policy nothing stops a package under an unacceptable license from entering builds. It is found later, if at all, in a legal review.",
        criteria: "Yellow: a repository whose format has license resolution has the license policy off.",
        recommendation: "Enable the license policy and deny the licenses your organization does not accept.",
    },
    Rule {
        id: "retention-off",
        code: "OP-01",
        category: Category::Operations,
        audiences: &[Audience::Devops],
        title: "Unused artifacts kept forever",
        rationale: "Artifacts nobody downloads cost storage and backups, and old vulnerable versions among them can be pulled back into a build by mistake.",
        criteria: "Yellow: a non-OCI repository without idle retention holds at least 10 artifacts not downloaded for 180 days.",
        recommendation: "Turn on idle retention in the repository settings. The impact preview shows what would go before you save.",
    },
    Rule {
        id: "admin-sprawl",
        code: "AC-01",
        category: Category::Access,
        audiences: &[Audience::Security],
        title: "Too many global administrators",
        rationale: "Every global administrator can change any policy, delete any repository and read every artifact. Each extra account is one more credential that can do all of that.",
        criteria: "Yellow: more than 3 enabled accounts hold the admin action on every repository (pattern *).",
        recommendation: "Move day-to-day work to scoped roles (read, write, approve, security per repository pattern) and keep admin for break-glass accounts.",
    },
    Rule {
        id: "stale-accounts",
        code: "AC-02",
        category: Category::Access,
        audiences: &[Audience::Security, Audience::Devops],
        title: "Dormant accounts with access",
        rationale: "Accounts nobody uses are not watched. If their credentials leak, the access they still hold is used without anyone noticing.",
        criteria: "Yellow: an enabled, non-robot account with at least one role has not signed in for 90 days (or never, and was created over 90 days ago).",
        recommendation: "Disable accounts that are no longer needed, or remove their roles. With OIDC, check that leavers are removed from the mapped groups.",
    },
    Rule {
        id: "locked-accounts",
        code: "AC-03",
        category: Category::Access,
        audiences: &[Audience::Security],
        title: "Accounts locked by failed sign-ins",
        rationale: "Lockouts follow repeated wrong passwords, which is also what password guessing looks like.",
        criteria: "Info: an account is locked after failed sign-ins.",
        recommendation: "Confirm with the owner before unlocking. A lock nobody expected can mean someone is guessing the password.",
    },
    Rule {
        id: "tokens-no-expiry",
        code: "AC-04",
        category: Category::Access,
        audiences: &[Audience::Security],
        title: "Access tokens that never expire",
        rationale: "A token without expiry stays valid forever. Tokens end up in CI variables, scripts and logs, and a leaked one keeps working until someone notices.",
        criteria: "Red (high): a token without expiry belongs to an administrator. Yellow (medium): only other accounts hold such tokens.",
        recommendation: "Reissue them with an expiry and rotate on a schedule. A leaked token without expiry stays valid until someone notices.",
    },
    Rule {
        id: "tokens-unscoped-admin",
        code: "AC-05",
        category: Category::Access,
        audiences: &[Audience::Security, Audience::Devops],
        title: "Administrator tokens without scopes",
        rationale: "An unscoped token carries all of its owner's rights. For an administrator that is full control of forklift, exercised by whatever holds the token.",
        criteria: "Red (high): a token of an account with global admin has no scopes.",
        recommendation: "Scope automation tokens to the repositories and actions they need, or issue them from a robot account with a narrow role.",
    },
    Rule {
        id: "tokens-unused",
        code: "AC-06",
        category: Category::Access,
        audiences: &[Audience::Security, Audience::Devops],
        title: "Valid tokens unused for 90 days",
        rationale: "A credential nobody uses has no reason to exist, but it can still be stolen and used.",
        criteria: "Yellow: an unexpired token has not been used for 90 days (or never, and was created over 90 days ago).",
        recommendation: "Revoke them. An unused credential only adds exposure.",
    },
    Rule {
        id: "auth-failures",
        code: "AC-07",
        category: Category::Access,
        audiences: &[Audience::Security, Audience::Devops],
        title: "Clients repeatedly refused",
        rationale: "A burst of refused requests from one client is either a broken CI credential that fails builds or someone probing for valid credentials.",
        criteria: "Red (high): a client was refused (401 or 403) 200 times or more in the last 24 hours. Yellow (medium): 20 times or more.",
        recommendation: "Check whether these are misconfigured CI credentials or someone probing. Block the address at the edge or with the repository IP allow list if it is not yours.",
    },
    Rule {
        id: "policy-blocks",
        code: "OP-02",
        category: Category::Operations,
        audiences: &[Audience::Developer],
        title: "Builds still requesting blocked packages",
        rationale: "Each block is a build that failed because a dependency is not allowed. Until the dependency is updated the build keeps failing or someone works around the policy.",
        criteria: "Yellow: a vulnerability or license policy blocked a package path in the last 7 days.",
        recommendation: "Update the dependency to an allowed version in the projects named here, so builds stop failing on it.",
    },
    Rule {
        id: "approvals-aging",
        code: "OP-03",
        category: Category::Operations,
        audiences: &[Audience::Security, Audience::Developer],
        title: "Package approvals waiting over a week",
        rationale: "A request waiting for weeks blocks the team that asked, and pushes people towards fetching the package around forklift.",
        criteria: "Yellow: a package approval request has been pending for more than 7 days.",
        recommendation: "Decide them, or enable auto-approval of clean packages so reviewers only see what needs a human.",
    },
    Rule {
        id: "coverage-bypass",
        code: "SC-09",
        category: Category::SupplyChain,
        audiences: &[Audience::Devops, Audience::Developer],
        title: "Projects bypassing forklift",
        rationale: "A project that pulls from public registries directly skips every age, approval, vulnerability and license control in forklift, and its downloads never reach the audit log.",
        criteria: "Yellow (medium): coverage is below 50% and a project in scope is not or only partly wired. Yellow (low): the same with coverage at 50% or more. Needs a completed coverage scan.",
        recommendation: "Point the project's package manager config and CI at the forklift endpoints shown on its Coverage page, most recently active projects first.",
    },
];

/// Looks a rule up by id.
pub fn rule(id: &str) -> &'static Rule {
    CATALOG
        .iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("insight rule {id} is not in the catalog"))
}

struct Spec {
    id: &'static str,
    severity: Severity,
    summary: String,
    params: Vec<(&'static str, String)>,
}

/// Builds a finding from all affected rows: totals and per-repository shares
/// come from the full list, the stored rows are capped.
fn finding(spec: Spec, all: Vec<Affected>) -> Option<Finding> {
    if all.is_empty() {
        return None;
    }
    let mut by_repo = BTreeMap::new();
    for a in &all {
        if !a.repo.is_empty() {
            *by_repo.entry(a.repo.clone()).or_insert(0) += 1;
        }
    }
    let total = all.len() as i64;
    let mut params: BTreeMap<String, String> = spec
        .params
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    params.insert("count".to_string(), total.to_string());
    let meta = rule(spec.id);
    Some(Finding {
        id: spec.id.to_string(),
        code: meta.code.to_string(),
        severity: spec.severity,
        category: meta.category,
        audiences: meta.audiences.to_vec(),
        title: meta.title.to_string(),
        summary: spec.summary,
        rationale: meta.rationale.to_string(),
        criteria: meta.criteria.to_string(),
        recommendation: meta.recommendation.to_string(),
        params,
        total,
        affected: all.into_iter().take(AFFECTED_LIMIT).collect(),
        by_repo,
        everyone: false,
        acknowledged: false,
    })
}

fn repo_affected(view: &RepoView, detail: String) -> Affected {
    Affected {
        kind: "repository".to_string(),
        label: view.repo.name.clone(),
        detail,
        repo: view.repo.name.clone(),
        repo_id: Some(view.repo.id),
        ..Default::default()
    }
}

fn holds_artifacts(view: &RepoView) -> bool {
    view.repo.r#type != meta::TYPE_GROUP
}

fn days_ago(now: DateTime<Utc>, at: DateTime<Utc>) -> i64 {
    (now - at).num_days().max(0)
}

fn human_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes.max(0) as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// One stored package version: its coordinate, the files behind it, and when
/// any of them was last served.
struct Coordinate {
    repo_id: i64,
    ecosystem: String,
    package: String,
    version: String,
    path: String,
    last_served: DateTime<Utc>,
}

/// Groups the versioned artifacts by coordinate once for all supply-chain
/// rules. A jar, its pom and its sources are one version, not three findings.
struct ArtifactIndex {
    coordinates: Vec<Coordinate>,
    license_coordinates: Vec<(i64, String, String)>,
}

impl ArtifactIndex {
    fn build(s: &DataSnapshot) -> Self {
        let mut coordinates: HashMap<(i64, String), Coordinate> = HashMap::new();
        let mut license_coordinates: BTreeSet<(i64, String, String)> = BTreeSet::new();
        for art in &s.artifacts {
            let Some(view) = s.repo(art.repo_id) else {
                continue;
            };
            let format = view.repo.format.as_str();
            let version = repo::version_for_path(format, &art.path);
            if version.is_empty() {
                continue;
            }
            let (system, lic_pkg) = repo::license_coordinate(format, &art.path);
            if !lic_pkg.is_empty() {
                license_coordinates.insert((
                    art.repo_id,
                    coordinate_key(&system, &lic_pkg, &version),
                    art.path.clone(),
                ));
            }
            let (ecosystem, package) = repo::vuln_coordinate(format, &art.path);
            if package.is_empty() {
                continue;
            }
            let key = coordinate_key(&ecosystem, &package, &version);
            let entry = coordinates
                .entry((art.repo_id, key))
                .or_insert_with(|| Coordinate {
                    repo_id: art.repo_id,
                    ecosystem,
                    package,
                    version,
                    path: art.path.clone(),
                    last_served: art.last_accessed_at,
                });
            if art.last_accessed_at > entry.last_served {
                entry.last_served = art.last_accessed_at;
            }
            if art.path.len() < entry.path.len() {
                entry.path.clone_from(&art.path);
            }
        }
        let mut coordinates: Vec<Coordinate> = coordinates.into_values().collect();
        coordinates.sort_by(|a, b| {
            a.repo_id
                .cmp(&b.repo_id)
                .then_with(|| a.package.cmp(&b.package))
                .then_with(|| a.version.cmp(&b.version))
        });
        Self {
            coordinates,
            license_coordinates: license_coordinates.into_iter().collect(),
        }
    }
}

fn severity_rank(label: &str) -> u8 {
    match label {
        "critical" => 4,
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

/// Critical and high versions, split by whether they were served recently.
fn vulnerable_rows(
    s: &DataSnapshot,
    index: &ArtifactIndex,
    ranks: std::ops::RangeInclusive<u8>,
    in_use: bool,
) -> (Vec<Affected>, i64) {
    let mut rows: Vec<(u8, DateTime<Utc>, Affected)> = Vec::new();
    let mut critical = 0;
    for c in &index.coordinates {
        let key = coordinate_key(&c.ecosystem, &c.package, &c.version);
        let Some(scan) = s.vulns.get(&key) else {
            continue;
        };
        let rank = severity_rank(&scan.max_severity);
        if !ranks.contains(&rank) || (c.last_served >= s.now - IN_USE) != in_use {
            continue;
        }
        let Some(view) = s.repo(c.repo_id) else {
            continue;
        };
        if rank == 4 {
            critical += 1;
        }
        let mut ids = scan
            .vuln_ids
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if scan.vuln_ids.len() > 3 {
            ids.push_str(&format!(" +{}", scan.vuln_ids.len() - 3));
        }
        let served = days_ago(s.now, c.last_served);
        rows.push((
            rank,
            c.last_served,
            Affected {
                kind: "artifact".to_string(),
                label: format!("{}@{}", c.package, c.version),
                detail: format!("{} · {ids} · served {served}d ago", scan.max_severity),
                repo: view.repo.name.clone(),
                repo_id: Some(view.repo.id),
                path: c.path.clone(),
                ..Default::default()
            },
        ));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    (rows.into_iter().map(|r| r.2).collect(), critical)
}

fn vuln_in_use(s: &DataSnapshot, index: &ArtifactIndex) -> Option<Finding> {
    let (rows, critical) = vulnerable_rows(s, index, 3..=4, true);
    let count = rows.len();
    finding(
        Spec {
            id: "vuln-in-use",
            severity: if critical > 0 {
                Severity::Critical
            } else {
                Severity::High
            },
            summary: format!(
                "{count} package versions with critical or high advisories ({critical} critical) were served in the last 30 days."
            ),
            params: vec![("critical", critical.to_string())],
        },
        rows,
    )
}

fn vuln_dormant(s: &DataSnapshot, index: &ArtifactIndex) -> Option<Finding> {
    let (rows, critical) = vulnerable_rows(s, index, 3..=4, false);
    let count = rows.len();
    finding(
        Spec {
            id: "vuln-dormant",
            severity: Severity::Medium,
            summary: format!(
                "{count} stored package versions with critical or high advisories ({critical} critical) have not been served for 30 days."
            ),
            params: vec![("critical", critical.to_string())],
        },
        rows,
    )
}

fn vuln_moderate(s: &DataSnapshot, index: &ArtifactIndex) -> Option<Finding> {
    let (rows, _) = vulnerable_rows(s, index, 1..=2, true);
    let count = rows.len();
    finding(
        Spec {
            id: "vuln-moderate",
            severity: Severity::Low,
            summary: format!(
                "{count} package versions with medium or low advisories were served in the last 30 days."
            ),
            params: Vec::new(),
        },
        rows,
    )
}

/// The advisories behind the stored versions, one entry per advisory id: its
/// worst severity and the stored versions it affects.
fn advisory_spread(
    s: &DataSnapshot,
    index: &ArtifactIndex,
) -> BTreeMap<String, (u8, BTreeSet<(String, String)>)> {
    let mut spread: BTreeMap<String, (u8, BTreeSet<(String, String)>)> = BTreeMap::new();
    for c in &index.coordinates {
        let Some(scan) = s
            .vulns
            .get(&coordinate_key(&c.ecosystem, &c.package, &c.version))
        else {
            continue;
        };
        let Some(view) = s.repo(c.repo_id) else {
            continue;
        };
        // Scans stored before per-advisory severities existed carry only ids;
        // those take the version's worst severity.
        let advisories: Vec<(String, u8)> = if scan.advisories.is_empty() {
            let rank = severity_rank(&scan.max_severity);
            scan.vuln_ids.iter().map(|id| (id.clone(), rank)).collect()
        } else {
            scan.advisories
                .iter()
                .map(|a| (a.id.clone(), severity_rank(&a.severity)))
                .collect()
        };
        for (id, rank) in advisories {
            let entry = spread.entry(id).or_default();
            entry.0 = entry.0.max(rank);
            entry.1.insert((
                format!("{}@{}", c.package, c.version),
                view.repo.name.clone(),
            ));
        }
    }
    spread
}

/// One advisory in this many stored versions or more is fixed better once,
/// centrally, than version by version.
const WIDESPREAD_MIN: usize = 3;

fn vuln_widespread(s: &DataSnapshot, index: &ArtifactIndex) -> Option<Finding> {
    let mut critical = 0;
    let mut rows: Vec<(u8, usize, Affected)> = advisory_spread(s, index)
        .into_iter()
        .filter(|(_, (rank, versions))| *rank >= 3 && versions.len() >= WIDESPREAD_MIN)
        .map(|(id, (rank, versions))| {
            if rank == 4 {
                critical += 1;
            }
            let repos: BTreeSet<&str> = versions.iter().map(|(_, r)| r.as_str()).collect();
            let mut listed: Vec<String> = versions
                .iter()
                .take(3)
                .map(|(v, r)| format!("{v} ({r})"))
                .collect();
            if versions.len() > 3 {
                listed.push(format!("+{}", versions.len() - 3));
            }
            let label = if rank == 4 { "critical" } else { "high" };
            (
                rank,
                versions.len(),
                Affected {
                    kind: "advisory".to_string(),
                    detail: format!(
                        "{label} · {} versions in {} repositories: {}",
                        versions.len(),
                        repos.len(),
                        listed.join(", ")
                    ),
                    label: id,
                    ..Default::default()
                },
            )
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let count = rows.len();
    finding(
        Spec {
            id: "vuln-widespread",
            severity: if critical > 0 {
                Severity::High
            } else {
                Severity::Medium
            },
            summary: format!(
                "{count} critical or high advisories ({critical} critical) each affect {WIDESPREAD_MIN} or more stored versions."
            ),
            params: vec![
                ("critical", critical.to_string()),
                ("min", WIDESPREAD_MIN.to_string()),
            ],
        },
        rows.into_iter().map(|r| r.2).collect(),
    )
}

fn vuln_accepted(s: &DataSnapshot) -> Option<Finding> {
    let mut ignored = 0;
    let rows: Vec<Affected> = s
        .repos
        .iter()
        .filter(|v| holds_artifacts(v) && !v.cfg.vuln.ignore.is_empty())
        .map(|v| {
            let list = &v.cfg.vuln.ignore;
            ignored += list.len();
            let mut detail = list.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
            if list.len() > 5 {
                detail.push_str(&format!(" +{}", list.len() - 5));
            }
            repo_affected(v, format!("{} ignored: {detail}", list.len()))
        })
        .collect();
    let count = rows.len();
    finding(
        Spec {
            id: "vuln-accepted",
            severity: Severity::Info,
            summary: format!(
                "{count} repositories ignore {ignored} advisories in their vulnerability policy."
            ),
            params: vec![("ignored", ignored.to_string())],
        },
        rows,
    )
}

fn scan_gap(s: &DataSnapshot, index: &ArtifactIndex) -> Option<Finding> {
    let mut per_repo: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for c in &index.coordinates {
        let entry = per_repo.entry(c.repo_id).or_default();
        entry.0 += 1;
        if s.vulns
            .contains_key(&coordinate_key(&c.ecosystem, &c.package, &c.version))
        {
            entry.1 += 1;
        }
    }
    let mut rows: Vec<(i64, Affected)> = Vec::new();
    for (repo_id, (scannable, scanned)) in per_repo {
        let unscanned = scannable - scanned;
        if scannable < SCAN_GAP_MIN || (unscanned as f64) <= (scannable as f64) * SCAN_GAP_RATIO {
            continue;
        }
        let Some(view) = s.repo(repo_id) else {
            continue;
        };
        rows.push((
            unscanned,
            repo_affected(
                view,
                format!("{unscanned} of {scannable} versions unscanned"),
            ),
        ));
    }
    rows.sort_by_key(|a| std::cmp::Reverse(a.0));
    let count = rows.len();
    finding(
        Spec {
            id: "scan-gap",
            severity: Severity::Medium,
            summary: format!(
                "{count} repositories have more than 20% of their package versions without a vulnerability scan."
            ),
            params: Vec::new(),
        },
        rows.into_iter().map(|r| r.1).collect(),
    )
}

/// Strong copyleft licenses whose obligations reach code that links or serves
/// the package.
fn is_strong_copyleft(id: &str) -> bool {
    let id = id.to_ascii_uppercase();
    ["GPL-", "AGPL-", "SSPL-", "EUPL-", "OSL-"]
        .iter()
        .any(|prefix| id.starts_with(prefix))
}

fn copyleft_licenses(s: &DataSnapshot, index: &ArtifactIndex) -> Option<Finding> {
    let mut rows = Vec::new();
    let mut network = 0;
    let mut seen = HashSet::new();
    for (repo_id, key, path) in &index.license_coordinates {
        if !seen.insert((repo_id, key)) {
            continue;
        }
        let Some(licenses) = s.licenses.get(key) else {
            continue;
        };
        let copyleft: Vec<&String> = licenses.iter().filter(|l| is_strong_copyleft(l)).collect();
        if copyleft.is_empty() {
            continue;
        }
        let Some(view) = s.repo(*repo_id) else {
            continue;
        };
        if copyleft.iter().any(|l| {
            let l = l.to_ascii_uppercase();
            l.starts_with("AGPL-") || l.starts_with("SSPL-")
        }) {
            network += 1;
        }
        let mut parts = key.split('\u{0}').skip(1);
        let package = parts.next().unwrap_or_default();
        let version = parts.next().unwrap_or_default();
        rows.push(Affected {
            kind: "artifact".to_string(),
            label: format!("{package}@{version}"),
            detail: copyleft
                .iter()
                .map(|l| l.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            repo: view.repo.name.clone(),
            repo_id: Some(view.repo.id),
            path: path.clone(),
            ..Default::default()
        });
    }
    let count = rows.len();
    finding(
        Spec {
            id: "copyleft-licenses",
            severity: if network > 0 {
                Severity::Medium
            } else {
                Severity::Low
            },
            summary: format!(
                "{count} stored package versions are under a strong copyleft license ({network} network copyleft such as AGPL or SSPL)."
            ),
            params: vec![("network", network.to_string())],
        },
        rows,
    )
}

fn hosted_oci(s: &DataSnapshot) -> impl Iterator<Item = (&RepoView, &Vec<meta::OCITag>)> {
    s.repos.iter().filter_map(|view| {
        if view.repo.r#type != meta::TYPE_HOSTED {
            return None;
        }
        s.oci_tags.get(&view.repo.id).map(|tags| (view, tags))
    })
}

fn tag_affected(view: &RepoView, tag: &meta::OCITag, detail: String) -> Affected {
    Affected {
        kind: "tag".to_string(),
        label: format!("{}:{}", tag.name, tag.tag),
        detail,
        repo: view.repo.name.clone(),
        repo_id: Some(view.repo.id),
        path: tag.name.clone(),
        tag: tag.tag.clone(),
        ..Default::default()
    }
}

fn oci_unsigned(s: &DataSnapshot) -> Option<Finding> {
    let mut rows = Vec::new();
    for (view, tags) in hosted_oci(s) {
        let names: HashSet<(&str, &str)> = tags
            .iter()
            .map(|t| (t.name.as_str(), t.tag.as_str()))
            .collect();
        for tag in tags {
            if tag.tag.starts_with("sha256-") {
                continue;
            }
            let hex = tag.manifest_digest.trim_start_matches("sha256:");
            let sig = format!("sha256-{hex}.sig");
            if names.contains(&(tag.name.as_str(), sig.as_str())) {
                continue;
            }
            rows.push(tag_affected(
                view,
                tag,
                tag.manifest_digest.chars().take(19).collect(),
            ));
        }
    }
    let count = rows.len();
    finding(
        Spec {
            id: "oci-unsigned",
            severity: Severity::Low,
            summary: format!(
                "{count} image tags in hosted OCI repositories have no cosign signature tag next to them."
            ),
            params: Vec::new(),
        },
        rows,
    )
}

fn oci_latest_tag(s: &DataSnapshot) -> Option<Finding> {
    let mut rows = Vec::new();
    for (view, tags) in hosted_oci(s) {
        for tag in tags.iter().filter(|t| t.tag == "latest") {
            rows.push(tag_affected(
                view,
                tag,
                format!("retagged {}d ago", days_ago(s.now, tag.updated_at)),
            ));
        }
    }
    let count = rows.len();
    finding(
        Spec {
            id: "oci-latest-tag",
            severity: Severity::Low,
            summary: format!(
                "{count} images are pushed as latest, which moves under every consumer on the next push."
            ),
            params: Vec::new(),
        },
        rows,
    )
}

fn public_repositories(s: &DataSnapshot) -> Option<Finding> {
    let mut hosted = 0;
    let mut rows: Vec<(bool, Affected)> = Vec::new();
    for view in s.repos.iter().filter(|v| v.cfg.public) {
        let is_hosted = view.repo.r#type == meta::TYPE_HOSTED;
        if is_hosted {
            hosted += 1;
        }
        rows.push((
            is_hosted,
            repo_affected(
                view,
                format!(
                    "{} {}, anonymous downloads",
                    view.repo.r#type, view.repo.format
                ),
            ),
        ));
    }
    rows.sort_by_key(|a| std::cmp::Reverse(a.0));
    let count = rows.len();
    finding(
        Spec {
            id: "public-repositories",
            severity: if hosted > 0 {
                Severity::High
            } else {
                Severity::Medium
            },
            summary: format!(
                "{count} repositories allow downloads without signing in ({hosted} hosted, holding internal artifacts)."
            ),
            params: vec![("hosted", hosted.to_string())],
        },
        rows.into_iter().map(|r| r.1).collect(),
    )
}

fn plaintext_upstream(s: &DataSnapshot) -> Option<Finding> {
    let rows: Vec<Affected> = s
        .repos
        .iter()
        .filter(|v| {
            v.repo.r#type == meta::TYPE_PROXY
                && v.repo
                    .upstream_url
                    .to_ascii_lowercase()
                    .starts_with("http://")
        })
        .map(|v| repo_affected(v, v.repo.upstream_url.clone()))
        .collect();
    let count = rows.len();
    finding(
        Spec {
            id: "plaintext-upstream",
            severity: Severity::High,
            summary: format!(
                "{count} proxy repositories fetch from an http:// upstream, so a network attacker can swap what gets cached."
            ),
            params: Vec::new(),
        },
        rows,
    )
}

fn vuln_policy_off(s: &DataSnapshot) -> Option<Finding> {
    let rows: Vec<Affected> = s
        .repos
        .iter()
        .filter(|v| holds_artifacts(v) && !repo::osv_ecosystem(&v.repo.format).is_empty())
        .filter_map(|v| {
            if !v.cfg.vuln.enabled {
                return Some(repo_affected(v, "off".to_string()));
            }
            (v.cfg.vuln.effective_action() == repoconfig_audit())
                .then(|| repo_affected(v, "audit only".to_string()))
        })
        .collect();
    let count = rows.len();
    finding(
        Spec {
            id: "vuln-policy-off",
            severity: Severity::Medium,
            summary: format!(
                "{count} repositories serve known-vulnerable versions without warning or blocking."
            ),
            params: Vec::new(),
        },
        rows,
    )
}

const fn repoconfig_audit() -> &'static str {
    crate::repoconfig::VULN_ACTION_AUDIT
}

fn age_policy_off(s: &DataSnapshot) -> Option<Finding> {
    let rows: Vec<Affected> = s
        .repos
        .iter()
        .filter(|v| v.repo.r#type == meta::TYPE_PROXY && !v.cfg.age_policy.enabled)
        .map(|v| repo_affected(v, format!("{} proxy", v.repo.format)))
        .collect();
    let count = rows.len();
    finding(
        Spec {
            id: "age-policy-off",
            severity: Severity::Medium,
            summary: format!(
                "{count} proxy repositories serve a version the moment it is published upstream."
            ),
            params: Vec::new(),
        },
        rows,
    )
}

fn license_policy_off(s: &DataSnapshot) -> Option<Finding> {
    let rows: Vec<Affected> = s
        .repos
        .iter()
        .filter(|v| {
            holds_artifacts(v)
                && !repo::deps_dev_system(&v.repo.format).is_empty()
                && !v.cfg.license.enabled
        })
        .map(|v| repo_affected(v, format!("{} {}", v.repo.r#type, v.repo.format)))
        .collect();
    let count = rows.len();
    finding(
        Spec {
            id: "license-policy-off",
            severity: Severity::Low,
            summary: format!("{count} repositories accept packages under any license."),
            params: Vec::new(),
        },
        rows,
    )
}

fn retention_off(s: &DataSnapshot) -> Option<Finding> {
    let mut rows: Vec<(i64, Affected)> = Vec::new();
    let mut bytes = 0;
    for view in s.repos.iter().filter(|v| holds_artifacts(v)) {
        if !view.cfg.retention.idle_ttl.is_zero() || view.repo.format == meta::FORMAT_OCI {
            continue;
        }
        let Some(&(count, size)) = s.stale.get(&view.repo.id) else {
            continue;
        };
        if count < STALE_MIN {
            continue;
        }
        bytes += size;
        rows.push((
            size,
            repo_affected(
                view,
                format!(
                    "{count} artifacts, {} unused for 180 days",
                    human_bytes(size)
                ),
            ),
        ));
    }
    rows.sort_by_key(|a| std::cmp::Reverse(a.0));
    let count = rows.len();
    let size = human_bytes(bytes);
    finding(
        Spec {
            id: "retention-off",
            severity: Severity::Low,
            summary: format!(
                "{count} repositories without idle retention hold {size} not downloaded for 180 days."
            ),
            params: vec![("size", size)],
        },
        rows.into_iter().map(|r| r.1).collect(),
    )
}

/// Users whose roles grant admin on every repository.
fn admin_user_ids(s: &DataSnapshot) -> HashSet<i64> {
    let admin_roles: HashSet<i64> = s
        .permissions
        .iter()
        .filter(|p| {
            p.repo_pattern == "*"
                && p.actions
                    .split(',')
                    .any(|a| a.trim() == crate::auth::ACTION_ADMIN)
        })
        .map(|p| p.role_id)
        .collect();
    s.roles_by_user
        .iter()
        .filter(|(_, roles)| roles.iter().any(|r| admin_roles.contains(&r.id)))
        .map(|(id, _)| *id)
        .collect()
}

fn user_affected(user: &meta::User, detail: String) -> Affected {
    Affected {
        kind: "user".to_string(),
        label: user.username.clone(),
        detail,
        user_id: Some(user.id),
        ..Default::default()
    }
}

fn last_login(s: &DataSnapshot, user: &meta::User) -> String {
    user.last_login_at.map_or_else(
        || "never signed in".to_string(),
        |at| format!("last sign-in {}d ago", days_ago(s.now, at)),
    )
}

fn admin_sprawl(s: &DataSnapshot, admins: &HashSet<i64>) -> Option<Finding> {
    let mut users: Vec<&meta::User> = s
        .users
        .iter()
        .filter(|u| !u.disabled && admins.contains(&u.id))
        .collect();
    if users.len() <= ADMIN_LIMIT {
        return None;
    }
    users.sort_by_key(|a| a.last_login_at);
    let count = users.len();
    finding(
        Spec {
            id: "admin-sprawl",
            severity: Severity::Medium,
            summary: format!(
                "{count} active accounts hold admin on every repository (guideline: at most {ADMIN_LIMIT})."
            ),
            params: vec![("limit", ADMIN_LIMIT.to_string())],
        },
        users
            .into_iter()
            .map(|u| user_affected(u, last_login(s, u)))
            .collect(),
    )
}

fn stale_accounts(s: &DataSnapshot) -> Option<Finding> {
    let mut users: Vec<&meta::User> = s
        .users
        .iter()
        .filter(|u| {
            !u.disabled
                && !u.robot
                && s.roles_by_user.get(&u.id).is_some_and(|r| !r.is_empty())
                && u.last_login_at
                    .map_or(u.created_at < s.now - DORMANT, |at| at < s.now - DORMANT)
        })
        .collect();
    users.sort_by_key(|a| a.last_login_at);
    let count = users.len();
    finding(
        Spec {
            id: "stale-accounts",
            severity: Severity::Medium,
            summary: format!(
                "{count} enabled accounts with a role have not signed in for 90 days."
            ),
            params: Vec::new(),
        },
        users
            .into_iter()
            .map(|u| user_affected(u, last_login(s, u)))
            .collect(),
    )
}

fn locked_accounts(s: &DataSnapshot) -> Option<Finding> {
    let rows: Vec<Affected> = s
        .users
        .iter()
        .filter_map(|u| {
            u.locked_at.map(|at| {
                user_affected(
                    u,
                    format!(
                        "locked {}d ago after {} failed sign-ins",
                        days_ago(s.now, at),
                        u.failed_login_count
                    ),
                )
            })
        })
        .collect();
    let count = rows.len();
    finding(
        Spec {
            id: "locked-accounts",
            severity: Severity::Info,
            summary: format!("{count} accounts are locked after repeated failed sign-ins."),
            params: Vec::new(),
        },
        rows,
    )
}

fn owner_name(s: &DataSnapshot, user_id: i64) -> String {
    s.users
        .iter()
        .find(|u| u.id == user_id)
        .map_or_else(|| format!("user {user_id}"), |u| u.username.clone())
}

fn token_affected(s: &DataSnapshot, token: &meta::Token, detail: String) -> Affected {
    let owner = owner_name(s, token.user_id);
    Affected {
        kind: "token".to_string(),
        label: format!("{owner} / {}", token.name),
        detail,
        user_id: Some(token.user_id),
        ..Default::default()
    }
}

fn unscoped(token: &meta::Token) -> bool {
    let scopes = token.scopes_json.trim();
    scopes.is_empty() || scopes == "[]" || scopes == "null"
}

fn tokens_without_expiry(s: &DataSnapshot, admins: &HashSet<i64>) -> Option<Finding> {
    let mut admin_owned = 0;
    let mut rows: Vec<(bool, Affected)> = Vec::new();
    for token in s.tokens.iter().filter(|t| t.expires_at.is_none()) {
        let is_admin = admins.contains(&token.user_id);
        if is_admin {
            admin_owned += 1;
        }
        let scope = if unscoped(token) {
            "unscoped"
        } else {
            "scoped"
        };
        rows.push((
            is_admin,
            token_affected(
                s,
                token,
                format!("{scope}{}", if is_admin { ", admin owner" } else { "" }),
            ),
        ));
    }
    rows.sort_by_key(|a| std::cmp::Reverse(a.0));
    let count = rows.len();
    finding(
        Spec {
            id: "tokens-no-expiry",
            severity: if admin_owned > 0 {
                Severity::High
            } else {
                Severity::Medium
            },
            summary: format!(
                "{count} access tokens have no expiry ({admin_owned} owned by administrators)."
            ),
            params: vec![("admin", admin_owned.to_string())],
        },
        rows.into_iter().map(|r| r.1).collect(),
    )
}

fn unscoped_admin_tokens(s: &DataSnapshot, admins: &HashSet<i64>) -> Option<Finding> {
    let rows: Vec<Affected> = s
        .tokens
        .iter()
        .filter(|t| admins.contains(&t.user_id) && unscoped(t))
        .map(|t| {
            token_affected(
                s,
                t,
                t.last_used_at.map_or_else(
                    || "never used".to_string(),
                    |at| format!("used {}d ago", days_ago(s.now, at)),
                ),
            )
        })
        .collect();
    let count = rows.len();
    finding(
        Spec {
            id: "tokens-unscoped-admin",
            severity: Severity::High,
            summary: format!(
                "{count} tokens of administrators carry no scopes, so each one is full admin over the API and every repository."
            ),
            params: Vec::new(),
        },
        rows,
    )
}

fn unused_tokens(s: &DataSnapshot) -> Option<Finding> {
    let rows: Vec<Affected> = s
        .tokens
        .iter()
        .filter(|t| {
            t.expires_at.is_none_or(|at| at > s.now)
                && t.last_used_at
                    .map_or(t.created_at < s.now - DORMANT, |at| at < s.now - DORMANT)
        })
        .map(|t| {
            token_affected(
                s,
                t,
                t.last_used_at.map_or_else(
                    || format!("never used, created {}d ago", days_ago(s.now, t.created_at)),
                    |at| format!("last used {}d ago", days_ago(s.now, at)),
                ),
            )
        })
        .collect();
    let count = rows.len();
    finding(
        Spec {
            id: "tokens-unused",
            severity: Severity::Medium,
            summary: format!("{count} unexpired access tokens have not been used for 90 days."),
            params: Vec::new(),
        },
        rows,
    )
}

fn is_policy_block(event: &str) -> bool {
    event == meta::EVENT_VULN_BLOCK || event == meta::EVENT_LICENSE_BLOCK
}

fn auth_failures(s: &DataSnapshot) -> Option<Finding> {
    let mut per_client: BTreeMap<&str, (i64, BTreeSet<&str>)> = BTreeMap::new();
    for row in s.auth_audit.iter().filter(|r| !is_policy_block(&r.event)) {
        let client = if row.client_ip.is_empty() {
            "unknown"
        } else {
            row.client_ip.as_str()
        };
        let entry = per_client.entry(client).or_default();
        entry.0 += row.count;
        if !row.username.is_empty() {
            entry.1.insert(row.username.as_str());
        }
    }
    let mut rows: Vec<(i64, Affected)> = per_client
        .into_iter()
        .filter(|(_, (n, _))| *n >= AUTH_FAILURE_THRESHOLD)
        .map(|(client, (n, users))| {
            let users = if users.is_empty() {
                "anonymous".to_string()
            } else {
                users.into_iter().take(5).collect::<Vec<_>>().join(", ")
            };
            (
                n,
                Affected {
                    kind: "client".to_string(),
                    label: client.to_string(),
                    detail: format!("{n} refused requests, as {users}"),
                    ..Default::default()
                },
            )
        })
        .collect();
    rows.sort_by_key(|a| std::cmp::Reverse(a.0));
    let heavy = rows.iter().any(|r| r.0 >= AUTH_FAILURE_THRESHOLD * 10);
    let count = rows.len();
    finding(
        Spec {
            id: "auth-failures",
            severity: if heavy {
                Severity::High
            } else {
                Severity::Medium
            },
            summary: format!(
                "{count} clients were refused (401 or 403) at least {AUTH_FAILURE_THRESHOLD} times in the last 24 hours."
            ),
            params: vec![("threshold", AUTH_FAILURE_THRESHOLD.to_string())],
        },
        rows.into_iter().map(|r| r.1).collect(),
    )
}

fn policy_blocks(s: &DataSnapshot) -> Option<Finding> {
    let mut per_path: BTreeMap<(&str, &str), (i64, BTreeSet<&str>)> = BTreeMap::new();
    for row in s.block_audit.iter().filter(|r| is_policy_block(&r.event)) {
        let entry = per_path
            .entry((row.repo_name.as_str(), row.path.as_str()))
            .or_default();
        entry.0 += row.count;
        if !row.username.is_empty() {
            entry.1.insert(row.username.as_str());
        }
    }
    let mut rows: Vec<(i64, Affected)> = per_path
        .into_iter()
        .map(|((repo_name, path), (n, users))| {
            let users = if users.is_empty() {
                "anonymous".to_string()
            } else {
                users.into_iter().take(5).collect::<Vec<_>>().join(", ")
            };
            (
                n,
                Affected {
                    kind: "artifact".to_string(),
                    label: path.to_string(),
                    detail: format!("blocked {n} times, by {users}"),
                    repo: repo_name.to_string(),
                    repo_id: s.repo_by_name(repo_name).map(|v| v.repo.id),
                    path: path.to_string(),
                    ..Default::default()
                },
            )
        })
        .collect();
    rows.sort_by_key(|a| std::cmp::Reverse(a.0));
    let count = rows.len();
    finding(
        Spec {
            id: "policy-blocks",
            severity: Severity::Low,
            summary: format!(
                "{count} package paths were blocked by a vulnerability or license policy in the last 7 days."
            ),
            params: Vec::new(),
        },
        rows.into_iter().map(|r| r.1).collect(),
    )
}

fn aging_approvals(s: &DataSnapshot) -> Option<Finding> {
    let mut rows: Vec<(DateTime<Utc>, Affected)> = s
        .pending
        .iter()
        .filter(|a| a.first_requested_at < s.now - APPROVAL_AGING)
        .map(|a| {
            (
                a.first_requested_at,
                Affected {
                    kind: "approval".to_string(),
                    label: a.package.clone(),
                    detail: format!(
                        "waiting {}d, requested {} times",
                        days_ago(s.now, a.first_requested_at),
                        a.request_count
                    ),
                    repo: a.repo_name.clone(),
                    repo_id: s.repo_by_name(&a.repo_name).map(|v| v.repo.id),
                    ..Default::default()
                },
            )
        })
        .collect();
    rows.sort_by_key(|a| a.0);
    let count = rows.len();
    finding(
        Spec {
            id: "approvals-aging",
            severity: Severity::Low,
            summary: format!(
                "{count} package approval requests have been pending for more than 7 days."
            ),
            params: Vec::new(),
        },
        rows.into_iter().map(|r| r.1).collect(),
    )
}

/// Projects whose builds do not go through forklift fetch dependencies straight
/// from public registries, past every policy and scan configured here.
fn coverage_bypass(s: &DataSnapshot) -> Option<Finding> {
    use crate::coverage::AppliedState;
    let coverage = s.coverage.as_ref()?;
    let mut rows: Vec<(u8, DateTime<Utc>, Affected)> = coverage
        .projects
        .iter()
        .filter(|p| p.exclude_reason.is_empty() && !p.skipped)
        .filter_map(|p| {
            let (rank, detail) = match p.applied {
                AppliedState::Applied => return None,
                AppliedState::Partial => (
                    1,
                    if p.ci_wired {
                        "CI uses forklift, registry not pinned"
                    } else {
                        "registry pinned, CI not wired"
                    },
                ),
                AppliedState::NotApplied | AppliedState::Error => (2, "not using forklift"),
            };
            Some((
                rank,
                p.last_activity_at,
                Affected {
                    kind: "project".to_string(),
                    label: p.path.clone(),
                    detail: format!(
                        "{detail}, active {}d ago",
                        days_ago(s.now, p.last_activity_at)
                    ),
                    ..Default::default()
                },
            ))
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let count = rows.len();
    let percent = coverage.percent;
    let mut found = finding(
        Spec {
            id: "coverage-bypass",
            severity: if percent < 50 {
                Severity::Medium
            } else {
                Severity::Low
            },
            summary: format!(
                "{count} of {} GitLab projects do not fully build through forklift (coverage {percent}%), so their dependencies skip the policies and scans here.",
                coverage.target
            ),
            params: vec![
                ("percent", percent.to_string()),
                ("target", coverage.target.to_string()),
            ],
        },
        rows.into_iter().map(|r| r.2).collect(),
    )?;
    found.everyone = true;
    Some(found)
}

#[cfg(test)]
mod tests {
    use super::super::collect::CoverageView;
    use super::*;
    use crate::meta::{
        AuditCount, InsightArtifact, OCITag, PackageApproval, Permission, Role, Token, User,
        VulnSummary,
    };
    use crate::repoconfig::{self, Duration};

    fn view(id: i64, name: &str, format: &str, typ: &str, cfg: repoconfig::Config) -> RepoView {
        RepoView {
            repo: meta::Repository {
                id,
                name: name.to_string(),
                format: format.to_string(),
                r#type: typ.to_string(),
                upstream_url: if typ == meta::TYPE_PROXY {
                    "http://registry.example.com".to_string()
                } else {
                    String::new()
                },
                ..Default::default()
            },
            cfg,
        }
    }

    fn guarded() -> repoconfig::Config {
        let mut cfg = repoconfig::Config::default();
        cfg.vuln.enabled = true;
        cfg.vuln.action = "block".to_string();
        cfg.license.enabled = true;
        cfg.age_policy.enabled = true;
        cfg.retention.idle_ttl = Duration(86_400 * 1_000_000_000);
        cfg
    }

    fn npm_path(pkg: &str, version: &str) -> String {
        format!("{pkg}/-/{pkg}-{version}.tgz")
    }

    fn vuln_key(path: &str) -> String {
        let (eco, pkg) = repo::vuln_coordinate(meta::FORMAT_NPM, path);
        coordinate_key(&eco, &pkg, &repo::version_for_path(meta::FORMAT_NPM, path))
    }

    fn license_key(path: &str) -> String {
        let (system, pkg) = repo::license_coordinate(meta::FORMAT_NPM, path);
        coordinate_key(
            &system,
            &pkg,
            &repo::version_for_path(meta::FORMAT_NPM, path),
        )
    }

    fn ids(findings: &[Finding]) -> Vec<&str> {
        findings.iter().map(|f| f.id.as_str()).collect()
    }

    fn get<'a>(findings: &'a [Finding], id: &str) -> &'a Finding {
        findings
            .iter()
            .find(|f| f.id == id)
            .unwrap_or_else(|| panic!("missing {id} in {:?}", ids(findings)))
    }

    /// A snapshot that trips every rule.
    fn risky() -> DataSnapshot {
        let now = Utc::now();
        let public = repoconfig::Config {
            public: true,
            ..Default::default()
        };
        let mut audit_only = guarded();
        audit_only.vuln.action = "audit".to_string();
        audit_only.retention.idle_ttl = Duration(0);
        audit_only.age_policy.enabled = false;
        audit_only.vuln.ignore = vec!["CVE-2020-1".to_string()];
        let repos = vec![
            view(1, "npm-hosted", meta::FORMAT_NPM, meta::TYPE_HOSTED, public),
            view(2, "npmjs", meta::FORMAT_NPM, meta::TYPE_PROXY, audit_only),
            view(
                3,
                "oci-hosted",
                meta::FORMAT_OCI,
                meta::TYPE_HOSTED,
                guarded(),
            ),
            view(
                4,
                "npm-public",
                meta::FORMAT_NPM,
                meta::TYPE_GROUP,
                repoconfig::Config::default(),
            ),
        ];
        let used = npm_path("lodash", "4.17.20");
        let dormant = npm_path("minimist", "0.0.8");
        let gpl = npm_path("gpl-lib", "1.0.0");
        let mut artifacts = vec![
            InsightArtifact {
                repo_id: 2,
                path: used.clone(),
                version: "4.17.20".to_string(),
                last_accessed_at: now - TimeDelta::days(1),
                ..Default::default()
            },
            InsightArtifact {
                repo_id: 2,
                path: dormant.clone(),
                version: "0.0.8".to_string(),
                last_accessed_at: now - TimeDelta::days(60),
                ..Default::default()
            },
            InsightArtifact {
                repo_id: 1,
                path: gpl.clone(),
                version: "1.0.0".to_string(),
                last_accessed_at: now,
                ..Default::default()
            },
        ];
        for i in 0..12 {
            artifacts.push(InsightArtifact {
                repo_id: 1,
                path: npm_path(&format!("pkg{i}"), "1.0.0"),
                version: "1.0.0".to_string(),
                last_accessed_at: now,
                ..Default::default()
            });
        }
        let mut vulns = HashMap::new();
        for i in 0..3 {
            vulns.insert(
                vuln_key(&npm_path(&format!("pkg{i}"), "1.0.0")),
                VulnSummary {
                    max_severity: "high".to_string(),
                    vuln_ids: vec!["GHSA-wide".to_string()],
                    advisories: vec![meta::VulnAdvisory {
                        id: "GHSA-wide".to_string(),
                        severity: "high".to_string(),
                        score: String::new(),
                    }],
                },
            );
        }
        vulns.insert(
            vuln_key(&npm_path("pkg3", "1.0.0")),
            VulnSummary {
                max_severity: "medium".to_string(),
                vuln_ids: vec!["GHSA-med".to_string()],
                advisories: Vec::new(),
            },
        );
        vulns.extend([
            (
                vuln_key(&used),
                VulnSummary {
                    max_severity: "critical".to_string(),
                    vuln_ids: (1..=5).map(|i| format!("GHSA-{i}")).collect(),
                    advisories: Vec::new(),
                },
            ),
            (
                vuln_key(&dormant),
                VulnSummary {
                    max_severity: "high".to_string(),
                    vuln_ids: vec!["CVE-1".to_string()],
                    advisories: Vec::new(),
                },
            ),
        ]);
        let digest = format!("sha256:{}", "a".repeat(64));
        let tag = |name: &str, tag: &str, digest: &str| OCITag {
            repo_id: 3,
            name: name.to_string(),
            tag: tag.to_string(),
            manifest_digest: digest.to_string(),
            updated_at: now - TimeDelta::days(2),
        };
        let users = (1..=5)
            .map(|id| User {
                id,
                username: format!("user{id}"),
                created_at: now - TimeDelta::days(400),
                last_login_at: (id != 5).then(|| now - TimeDelta::days(id * 40)),
                locked_at: (id == 2).then_some(now),
                failed_login_count: if id == 2 { 5 } else { 0 },
                ..Default::default()
            })
            .collect();
        DataSnapshot {
            now,
            repos,
            artifacts,
            vulns,
            licenses: HashMap::from([(license_key(&gpl), vec!["AGPL-3.0-only".to_string()])]),
            oci_tags: HashMap::from([(
                3,
                vec![
                    tag("app", "latest", &digest),
                    tag("app", "1.0", &format!("sha256:{}", "b".repeat(64))),
                    tag("app", &format!("sha256-{}.sig", "b".repeat(64)), &digest),
                ],
            )]),
            stale: HashMap::from([(2, (40, 5 << 30))]),
            users,
            roles_by_user: (1..=5)
                .map(|id| {
                    (
                        id,
                        vec![Role {
                            id: 1,
                            name: "admin".to_string(),
                            ..Default::default()
                        }],
                    )
                })
                .collect(),
            permissions: vec![Permission {
                role_id: 1,
                repo_pattern: "*".to_string(),
                actions: "admin".to_string(),
                ..Default::default()
            }],
            tokens: vec![
                Token {
                    id: 1,
                    user_id: 1,
                    name: "ci".to_string(),
                    scopes_json: "[]".to_string(),
                    created_at: now - TimeDelta::days(200),
                    ..Default::default()
                },
                Token {
                    id: 2,
                    user_id: 3,
                    name: "scoped".to_string(),
                    scopes_json: r#"[{"repo_pattern":"npm-*","actions":["read"]}]"#.to_string(),
                    expires_at: Some(now + TimeDelta::days(10)),
                    last_used_at: Some(now),
                    created_at: now,
                    ..Default::default()
                },
            ],
            auth_audit: vec![
                AuditCount {
                    event: "download".to_string(),
                    status: 401,
                    client_ip: "10.0.0.9".to_string(),
                    username: "bot".to_string(),
                    count: 250,
                    ..Default::default()
                },
                AuditCount {
                    event: "download".to_string(),
                    status: 403,
                    client_ip: "10.0.0.8".to_string(),
                    count: 3,
                    ..Default::default()
                },
            ],
            block_audit: vec![AuditCount {
                event: meta::EVENT_VULN_BLOCK.to_string(),
                status: 403,
                repo_name: "npmjs".to_string(),
                path: used,
                username: "dev".to_string(),
                count: 4,
                ..Default::default()
            }],
            pending: vec![PackageApproval {
                repo_name: "npmjs".to_string(),
                package: "left-pad".to_string(),
                first_requested_at: now - TimeDelta::days(10),
                request_count: 3,
                ..Default::default()
            }],
            row_counts: HashMap::new(),
            coverage: Some(CoverageView {
                percent: 40,
                target: 3,
                projects: vec![
                    crate::coverage::Project {
                        path: "team/web".to_string(),
                        applied: crate::coverage::AppliedState::NotApplied,
                        ..Default::default()
                    },
                    crate::coverage::Project {
                        path: "team/api".to_string(),
                        applied: crate::coverage::AppliedState::Applied,
                        ..Default::default()
                    },
                    crate::coverage::Project {
                        path: "team/muted".to_string(),
                        exclude_reason: "muted".to_string(),
                        ..Default::default()
                    },
                ],
            }),
        }
    }

    #[test]
    fn risky_snapshot_trips_every_rule() {
        let (findings, checks) = evaluate(&risky());
        assert_eq!(checks.len(), CATALOG.len());
        assert!(
            checks.iter().all(|c| c.status != CheckStatus::Ok),
            "every rule trips"
        );
        assert_eq!(findings.len(), 25, "{:?}", ids(&findings));

        let used = get(&findings, "vuln-in-use");
        assert_eq!(used.severity, Severity::Critical);
        assert_eq!(used.total, 4, "lodash plus the three widespread versions");
        assert_eq!(used.affected[0].label, "lodash@4.17.20");
        assert!(
            used.affected[0].detail.contains("+2"),
            "{}",
            used.affected[0].detail
        );
        assert_eq!(used.by_repo.get("npmjs"), Some(&1));

        assert_eq!(
            get(&findings, "vuln-dormant").affected[0].label,
            "minimist@0.0.8"
        );
        assert_eq!(get(&findings, "scan-gap").affected[0].label, "npm-hosted");
        assert_eq!(
            get(&findings, "copyleft-licenses").severity,
            Severity::Medium
        );
        let unsigned = get(&findings, "oci-unsigned");
        assert_eq!(
            unsigned.total, 1,
            "the signed tag and the signature are skipped"
        );
        assert_eq!(unsigned.affected[0].tag, "latest");
        assert_eq!(get(&findings, "oci-latest-tag").total, 1);
        assert_eq!(
            get(&findings, "public-repositories").severity,
            Severity::High
        );
        assert_eq!(
            get(&findings, "plaintext-upstream").affected[0].label,
            "npmjs"
        );
        assert_eq!(get(&findings, "vuln-policy-off").affected[0].detail, "off");
        assert_eq!(get(&findings, "age-policy-off").total, 1);
        assert_eq!(get(&findings, "license-policy-off").total, 1);
        assert!(get(&findings, "retention-off").summary.contains("5.0 GB"));
        assert_eq!(get(&findings, "admin-sprawl").total, 5);
        assert_eq!(
            get(&findings, "stale-accounts").total,
            3,
            "users idle 120+ days or never signed in"
        );
        assert_eq!(get(&findings, "locked-accounts").total, 1);
        assert_eq!(get(&findings, "tokens-no-expiry").severity, Severity::High);
        assert_eq!(
            get(&findings, "tokens-unscoped-admin").affected[0].label,
            "user1 / ci"
        );
        assert_eq!(get(&findings, "tokens-unused").total, 1);
        let auth = get(&findings, "auth-failures");
        assert_eq!(auth.severity, Severity::High);
        assert_eq!(auth.total, 1, "below-threshold clients are not reported");
        let blocks = get(&findings, "policy-blocks");
        assert_eq!(blocks.affected[0].repo_id, Some(2));
        assert_eq!(
            get(&findings, "approvals-aging").affected[0].label,
            "left-pad"
        );
        let moderate = get(&findings, "vuln-moderate");
        assert_eq!(moderate.affected[0].label, "pkg3@1.0.0");
        let wide = get(&findings, "vuln-widespread");
        assert_eq!(
            wide.severity,
            Severity::Medium,
            "high advisory, none critical"
        );
        assert_eq!(wide.affected[0].label, "GHSA-wide");
        assert!(
            wide.affected[0]
                .detail
                .contains("3 versions in 1 repositories"),
            "{}",
            wide.affected[0].detail
        );
        let accepted = get(&findings, "vuln-accepted");
        assert_eq!(accepted.affected[0].label, "npmjs");
        assert!(accepted.affected[0].detail.contains("CVE-2020-1"));
        let bypass = get(&findings, "coverage-bypass");
        assert!(bypass.everyone, "coverage is readable by everyone");
        assert_eq!(bypass.severity, Severity::Medium);
        assert_eq!(bypass.total, 1, "applied and excluded projects are skipped");
        assert_eq!(bypass.affected[0].label, "team/web");
    }

    #[test]
    fn rule_codes_are_unique_and_match_their_category() {
        let mut seen = HashSet::new();
        for r in CATALOG {
            assert!(seen.insert(r.code), "duplicate code {}", r.code);
            let prefix = match r.category {
                Category::SupplyChain => "SC-",
                Category::Policy => "PO-",
                Category::Exposure => "EX-",
                Category::Access => "AC-",
                Category::Operations => "OP-",
            };
            assert!(r.code.starts_with(prefix), "{} has code {}", r.id, r.code);
            assert_eq!(r.code.len(), 5, "{}", r.code);
        }
    }

    #[test]
    fn guarded_snapshot_is_clean() {
        let now = Utc::now();
        let s = DataSnapshot {
            now,
            repos: vec![view(
                1,
                "npm-hosted",
                meta::FORMAT_NPM,
                meta::TYPE_HOSTED,
                guarded(),
            )],
            users: vec![User {
                id: 1,
                username: "admin".to_string(),
                last_login_at: Some(now),
                created_at: now,
                ..Default::default()
            }],
            ..Default::default()
        };
        let (findings, checks) = evaluate(&s);
        assert!(findings.is_empty(), "{:?}", ids(&findings));
        let unavailable: Vec<_> = checks
            .iter()
            .filter(|c| c.status == CheckStatus::Unavailable)
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(unavailable, ["coverage-bypass"], "coverage needs a scan");
        assert!(
            checks
                .iter()
                .filter(|c| c.id != "coverage-bypass")
                .all(|c| c.status == CheckStatus::Ok && c.flagged == 0)
        );
    }

    #[test]
    fn affected_rows_are_capped_but_counted() {
        let rows = (0..(AFFECTED_LIMIT + 5))
            .map(|i| Affected {
                repo: format!("r{}", i % 2),
                ..Default::default()
            })
            .collect();
        let f = finding(
            Spec {
                id: "public-repositories",
                severity: Severity::Low,
                summary: String::new(),
                params: Vec::new(),
            },
            rows,
        )
        .expect("finding");
        assert_eq!(f.total, (AFFECTED_LIMIT + 5) as i64);
        assert_eq!(f.affected.len(), AFFECTED_LIMIT);
        assert_eq!(f.by_repo.values().sum::<i64>(), f.total);
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
    }
}
