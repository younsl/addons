//! Repository-scoped RBAC authorization: the resolved [`Principal`], the action
//! vocabulary and the glob matcher shared by every permission check.

use crate::meta;

/// Action constants. "admin" implies all other actions. "approve" grants
/// package approval decisions (quarantine) on matching repositories without
/// repository management rights, e.g. for security engineers. "audit" grants
/// read-only access to the administrative surfaces (users, roles, group
/// mappings, audit logs, repository permissions) without any mutation rights,
/// e.g. for a security auditor. It also grants read access to the package
/// approval surface (the queue, request detail and version-deny list) but not
/// the right to decide: approving, rejecting and bulk-clearing require the
/// approve action (see the API handler's `can_approve`). "security" grants
/// edits to the security-policy subset of a repository's configuration (age
/// policy, approval policy, vulnerability policy, license policy, IP ACL,
/// notification receivers) through `PUT /repositories/{id}/security`, without
/// the repository management rights that `PUT /repositories/{id}` carries:
/// upstream URL and upstream credentials stay admin-only, so the action cannot
/// be escalated into full control of where packages come from.
pub const ACTION_READ: &str = "read";
/// See [`ACTION_READ`] for the action vocabulary.
pub const ACTION_WRITE: &str = "write";
/// See [`ACTION_READ`] for the action vocabulary.
pub const ACTION_DELETE: &str = "delete";
/// See [`ACTION_READ`] for the action vocabulary.
pub const ACTION_APPROVE: &str = "approve";
/// See [`ACTION_READ`] for the action vocabulary.
pub const ACTION_AUDIT: &str = "audit";
/// See [`ACTION_READ`] for the action vocabulary.
pub const ACTION_SECURITY: &str = "security";
/// See [`ACTION_READ`] for the action vocabulary.
pub const ACTION_ADMIN: &str = "admin";

/// One entry of a personal access token's fine-grained scope: a set of actions
/// on repositories matching a glob pattern.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Scope {
    #[serde(rename = "repo_pattern", default)]
    pub repo_pattern: String,
    #[serde(default)]
    pub actions: Vec<String>,
}

/// An authenticated identity with resolved effective permissions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Principal {
    pub username: String,
    pub source: String,
    /// The administrator acting as this principal, empty for an ordinary
    /// session. The principal's permissions are always the impersonated user's
    /// own, never the administrator's, so impersonation never widens access;
    /// the field exists for attribution and for stopping the session.
    pub impersonator: String,
    /// The union of permissions granted via the principal's roles.
    pub(crate) perms: Vec<meta::Permission>,
    /// When true, [`Principal::token_scopes`] further restricts `perms`
    /// (intersection).
    pub(crate) via_token: bool,
    pub(crate) token_scopes: Vec<Scope>,
}

impl Principal {
    /// Reports whether the principal has admin on all repositories.
    pub fn is_admin(&self) -> bool {
        self.can("", ACTION_ADMIN)
    }

    /// Reports whether the principal may perform `action` on `repo`. An empty
    /// repo matches a wildcard-only check (used for global admin). A
    /// token-authenticated principal must satisfy both its role permissions and
    /// its token scopes.
    pub fn can(&self, repo: &str, action: &str) -> bool {
        if !self.roles_allow(repo, action) {
            return false;
        }
        // A token with no scopes inherits the user's full (role-limited)
        // access; a scoped token further narrows it to the listed repo/action
        // pairs.
        if self.via_token
            && !self.token_scopes.is_empty()
            && !scopes_allow(&self.token_scopes, repo, action)
        {
            return false;
        }
        true
    }

    /// Reports whether the principal may decide package approvals on at least
    /// one repository pattern (admin qualifies via admin-implies-all). Gates the
    /// approvals API and the UI nav; per-repository enforcement happens with
    /// `can(repo, ACTION_APPROVE)` on each decision. Token-scoped principals
    /// never qualify: token scopes cannot carry the approve action.
    pub fn can_approve_any(&self) -> bool {
        if !self
            .perms
            .iter()
            .any(|p| actions_contain(&p.actions, ACTION_APPROVE))
        {
            return false;
        }
        if self.via_token && !self.token_scopes.is_empty() {
            return self
                .token_scopes
                .iter()
                .any(|s| action_list_contains(&s.actions, ACTION_APPROVE));
        }
        true
    }

    /// Reports whether the principal may read the administrative surfaces on at
    /// least one repository pattern (admin qualifies via admin-implies-all).
    /// Unlike approve, audit is the one management-plane action a token scope
    /// may carry: it is read-only viewing, so a scoped token qualifies when one
    /// of its scopes lists the audit action.
    pub fn can_audit_any(&self) -> bool {
        if !self
            .perms
            .iter()
            .any(|p| actions_contain(&p.actions, ACTION_AUDIT))
        {
            return false;
        }
        if self.via_token && !self.token_scopes.is_empty() {
            return self
                .token_scopes
                .iter()
                .any(|s| action_list_contains(&s.actions, ACTION_AUDIT));
        }
        true
    }

    /// Reports whether the principal may edit the security-policy subset of a
    /// repository's config on at least one repository pattern (admin qualifies
    /// via admin-implies-all). Gates the UI's write affordance on the Security
    /// tab; per-repository enforcement happens with `can(repo,
    /// ACTION_SECURITY)` inside the handler. Like approve and audit, the
    /// security action cannot be carried by a token scope: changing policy is a
    /// human decision, so a scoped token never qualifies.
    pub fn can_security_any(&self) -> bool {
        if !self
            .perms
            .iter()
            .any(|p| actions_contain(&p.actions, ACTION_SECURITY))
        {
            return false;
        }
        if self.via_token && !self.token_scopes.is_empty() {
            return false;
        }
        true
    }

    fn roles_allow(&self, repo: &str, action: &str) -> bool {
        self.perms
            .iter()
            .any(|p| match_glob(&p.repo_pattern, repo) && actions_contain(&p.actions, action))
    }
}

fn scopes_allow(scopes: &[Scope], repo: &str, action: &str) -> bool {
    scopes
        .iter()
        .any(|s| match_glob(&s.repo_pattern, repo) && action_list_contains(&s.actions, action))
}

/// Checks a CSV action list, honouring admin-implies-all.
fn actions_contain(csv: &str, action: &str) -> bool {
    csv.split(',')
        .map(str::trim)
        .any(|a| a == ACTION_ADMIN || a == action)
}

fn action_list_contains(list: &[String], action: &str) -> bool {
    list.iter().any(|a| a == ACTION_ADMIN || a == action)
}

/// Reports whether a permission's repo pattern matches a repository name, using
/// the same glob semantics as authorization. Exported so the API can list which
/// roles apply to a given repository.
pub fn match_repo_pattern(pattern: &str, name: &str) -> bool {
    match_glob(pattern, name)
}

/// Matches a repository name against a pattern that may contain `*` wildcards.
/// Repository names contain no slashes, so `*` matches any run of characters.
/// An empty repo only matches a `*` pattern (global checks).
pub(crate) fn match_glob(pattern: &str, name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if name.is_empty() {
        return false;
    }
    glob_match(pattern, name)
}

/// A small `*`-only glob matcher using the canonical two-pointer algorithm with star
/// backtracking.
fn glob_match(pattern: &str, s: &str) -> bool {
    let (pattern, s) = (pattern.as_bytes(), s.as_bytes());
    let (mut sx, mut px) = (0usize, 0usize);
    let (mut star_idx, mut s_tmp) = (usize::MAX, 0usize);
    while sx < s.len() {
        if px < pattern.len() && pattern[px] == s[sx] {
            sx += 1;
            px += 1;
        } else if px < pattern.len() && pattern[px] == b'*' {
            star_idx = px;
            s_tmp = sx;
            px += 1;
        } else if star_idx != usize::MAX {
            px = star_idx + 1;
            s_tmp += 1;
            sx = s_tmp;
        } else {
            return false;
        }
    }
    while px < pattern.len() && pattern[px] == b'*' {
        px += 1;
    }
    px == pattern.len()
}
