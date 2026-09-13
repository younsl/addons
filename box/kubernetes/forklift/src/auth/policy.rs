//! ArgoCD-style declarative RBAC policy parsing.

use std::collections::{BTreeMap, BTreeSet};

use super::Error;
use super::authz::{
    ACTION_ADMIN, ACTION_APPROVE, ACTION_AUDIT, ACTION_DELETE, ACTION_READ, ACTION_SECURITY,
    ACTION_WRITE,
};
use crate::meta;

/// Subject prefix for grant (`g`) lines naming a user. A bare subject is treated
/// as a user too.
const SUBJECT_USER_PREFIX: &str = "user:";
/// Subject prefix for grant (`g`) lines naming an identity-provider group.
const SUBJECT_GROUP_PREFIX: &str = "group:";

/// Parses an ArgoCD-style RBAC policy into the desired managed state. The
/// grammar is line-oriented; blank lines and lines beginning with `#` are
/// ignored. Two statement kinds are supported:
///
/// ```text
/// p, <role>, <resource>, <action>, <object>, <effect>
/// g, <subject>, <role>
/// ```
///
/// For permission (`p`) lines, `<resource>` is `repo` (or `*`), `<action>` is
/// one of read|write|delete|approve|admin (or `*` meaning admin), `<object>` is
/// a repository glob pattern, and `<effect>` is `allow` (the only effect
/// forklift enforces; `deny` is rejected). For grant (`g`) lines, `<subject>` is
/// `user:<name>`, `group:<name>`, or a bare name (treated as a user); groups map
/// to Keycloak group claims.
///
/// Local users are not expressed in the policy text; they are supplied
/// separately (see [`super::reconcile::load_accounts`]) because passwords must
/// come from a Secret, not a ConfigMap.
pub fn parse_policy(text: &str) -> Result<meta::ManagedRBAC, Error> {
    // role name -> repo pattern -> ordered, de-duplicated action set.
    let mut role_actions: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    let mut role_seen: BTreeSet<String> = BTreeSet::new();
    let mut group_roles: Vec<meta::ManagedGrant> = Vec::new();
    let mut user_roles: Vec<meta::ManagedGrant> = Vec::new();

    for (i, raw) in text.split('\n').enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields = split_csv(line);
        match fields[0].as_str() {
            "p" => {
                let (role, pattern, action) = parse_permission_line(&fields)
                    .map_err(|e| Error::Policy(format!("policy line {}: {e}", i + 1)))?;
                role_seen.insert(role.clone());
                let by_pattern = role_actions.entry(role).or_default();
                let actions = by_pattern.entry(pattern).or_default();
                if !actions.iter().any(|a| a == &action) {
                    actions.push(action);
                }
            }
            "g" => {
                let (subject, role) = parse_grant_line(&fields)
                    .map_err(|e| Error::Policy(format!("policy line {}: {e}", i + 1)))?;
                if let Some(name) = subject.strip_prefix(SUBJECT_GROUP_PREFIX) {
                    group_roles.push(meta::ManagedGrant {
                        subject: name.to_string(),
                        role,
                    });
                } else {
                    let name = subject
                        .strip_prefix(SUBJECT_USER_PREFIX)
                        .unwrap_or(&subject)
                        .to_string();
                    user_roles.push(meta::ManagedGrant {
                        subject: name,
                        role,
                    });
                }
            }
            other => {
                return Err(Error::Policy(format!(
                    "policy line {}: unknown statement {:?} (want 'p' or 'g')",
                    i + 1,
                    other
                )));
            }
        }
    }

    Ok(meta::ManagedRBAC {
        roles: build_roles(&role_seen, &role_actions),
        group_roles,
        user_roles,
        local_users: Vec::new(),
    })
}

/// Returns `(role, pattern, action)` for a permission line.
fn parse_permission_line(fields: &[String]) -> Result<(String, String, String), String> {
    if fields.len() < 5 || fields.len() > 6 {
        return Err("permission needs 'p, <role>, <resource>, <action>, <object>[, allow]'".into());
    }
    let role = fields[1].clone();
    let resource = fields[2].as_str();
    let mut action = fields[3].clone();
    let pattern = fields[4].clone();
    let effect = if fields.len() == 6 {
        fields[5].as_str()
    } else {
        "allow"
    };
    if role.is_empty() || pattern.is_empty() {
        return Err("role and object must not be empty".into());
    }
    if resource != "repo" && resource != "*" {
        return Err(format!("unsupported resource {resource:?} (want 'repo')"));
    }
    if effect != "allow" {
        return Err(format!(
            "unsupported effect {effect:?} (forklift enforces allow only)"
        ));
    }
    if action == "*" {
        action = ACTION_ADMIN.to_string();
    }
    if !valid_role_action(&action) {
        return Err(format!("invalid action {action:?}"));
    }
    Ok((role, pattern, action))
}

/// Returns `(subject, role)` for a grant line.
fn parse_grant_line(fields: &[String]) -> Result<(String, String), String> {
    if fields.len() != 3 {
        return Err("grant needs 'g, <subject>, <role>'".into());
    }
    let (subject, role) = (fields[1].clone(), fields[2].clone());
    if subject.is_empty() || role.is_empty() {
        return Err("subject and role must not be empty".into());
    }
    if subject == SUBJECT_USER_PREFIX || subject == SUBJECT_GROUP_PREFIX {
        return Err("subject must not be empty after prefix".into());
    }
    Ok((subject, role))
}

fn build_roles(
    seen: &BTreeSet<String>,
    actions: &BTreeMap<String, BTreeMap<String, Vec<String>>>,
) -> Vec<meta::ManagedRole> {
    seen.iter()
        .map(|name| meta::ManagedRole {
            name: name.clone(),
            description: "Managed by declarative RBAC policy".into(),
            permissions: actions
                .get(name)
                .map(|by_pattern| {
                    by_pattern
                        .iter()
                        .map(|(pattern, acts)| meta::Permission {
                            repo_pattern: pattern.clone(),
                            actions: acts.join(","),
                            ..Default::default()
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect()
}

/// Reports whether `action` is a grantable RBAC action.
fn valid_role_action(action: &str) -> bool {
    matches!(
        action,
        ACTION_READ
            | ACTION_WRITE
            | ACTION_DELETE
            | ACTION_APPROVE
            | ACTION_AUDIT
            | ACTION_SECURITY
            | ACTION_ADMIN
    )
}

fn split_csv(line: &str) -> Vec<String> {
    line.split(',').map(|p| p.trim().to_string()).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::auth::*;

    #[test]
    fn parse_policy_cases() {
        let policy = r#"
# comment line
p, readonly, repo, read, *, allow
p, dev, repo, read, team-a-*, allow
p, dev, repo, write, team-a-*, allow
p, dev, repo, read, team-a-*, allow
p, super, *, *, *
g, group:/platform, readonly
g, user:alice, dev
g, bob, dev
"#;
        let got = parse_policy(policy).expect("parse_policy");

        // Roles are sorted by name: dev, readonly, super.
        assert_eq!(got.roles.len(), 3, "roles = {:?}", got.roles);
        assert_eq!(
            [
                got.roles[0].name.as_str(),
                got.roles[1].name.as_str(),
                got.roles[2].name.as_str()
            ],
            ["dev", "readonly", "super"],
            "role order"
        );

        // dev has one merged permission on team-a-* with read,write (deduped).
        let dev = &got.roles[0];
        assert_eq!(
            dev.permissions.len(),
            1,
            "dev permissions = {:?}",
            dev.permissions
        );
        assert_eq!(dev.permissions[0].repo_pattern, "team-a-*");
        assert_eq!(dev.permissions[0].actions, "read,write");

        // '*' action resolves to admin.
        assert_eq!(
            got.roles[2].permissions[0].actions, ACTION_ADMIN,
            "super action"
        );

        // Group vs user subjects.
        assert_eq!(
            got.group_roles.len(),
            1,
            "group roles = {:?}",
            got.group_roles
        );
        assert_eq!(got.group_roles[0].subject, "/platform");
        assert_eq!(got.group_roles[0].role, "readonly");
        // alice (user: prefix) and bob (bare) both become user grants.
        assert_eq!(got.user_roles.len(), 2, "user roles = {:?}", got.user_roles);
        assert_eq!(got.user_roles[0].subject, "alice");
        assert_eq!(got.user_roles[1].subject, "bob");
    }

    #[test]
    fn parse_policy_empty() {
        let got = parse_policy("\n  \n# only comments\n").expect("parse_policy empty");
        assert!(
            got.roles.is_empty() && got.group_roles.is_empty() && got.user_roles.is_empty(),
            "empty policy should be empty: {got:?}"
        );
    }

    #[test]
    fn parse_policy_errors() {
        let cases: &[(&str, &str)] = &[
            ("bad action", "p, r, repo, frobnicate, *, allow"),
            ("deny effect", "p, r, repo, read, *, deny"),
            ("bad resource", "p, r, secret, read, *, allow"),
            ("short perm", "p, r, repo, read"),
            ("empty role", "p, , repo, read, *, allow"),
            ("grant arity", "g, user:alice"),
            ("empty subject", "g, , dev"),
            ("empty role name", "g, user:alice, "),
            ("prefix only", "g, group:, dev"),
            ("unknown stmt", "x, foo, bar"),
        ];
        for (name, policy) in cases {
            assert!(
                parse_policy(policy).is_err(),
                "{name}: expected error for {policy:?}"
            );
        }
    }

    #[test]
    fn parse_policy_line_number_in_error() {
        let err = parse_policy("p, ok, repo, read, *, allow\np, bad, repo, nope, *, allow")
            .expect_err("expected an error");
        assert!(
            err.to_string().contains("line 2"),
            "error should point at line 2: {err}"
        );
    }
}
