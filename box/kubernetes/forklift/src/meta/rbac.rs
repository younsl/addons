//! Declarative RBAC reconciliation and login-time OIDC group sync.

use std::collections::HashMap;

use rusqlite::{Transaction, params, params_from_iter};

use super::time::now_rfc3339;
use super::{Error, Permission, Result, SOURCE_LOCAL, SOURCE_OIDC, Store};

/// The desired declarative RBAC state parsed from the chart policy.
/// [`Store::apply_managed_rbac`] reconciles the database to match it, owning
/// every row it writes via the managed flag and leaving interactively-created
/// (unmanaged) rows untouched.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ManagedRBAC {
    pub roles: Vec<ManagedRole>,
    /// Keycloak group name -> role name
    pub group_roles: Vec<ManagedGrant>,
    /// username -> role name
    pub user_roles: Vec<ManagedGrant>,
    /// local accounts to provision
    pub local_users: Vec<ManagedLocalUser>,
}

/// A declaratively-defined role and its permissions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ManagedRole {
    pub name: String,
    pub description: String,
    /// `repo_pattern` + `actions`; `role_id`/`id`/`managed` ignored
    pub permissions: Vec<Permission>,
}

/// Assigns a subject (group or username) to a role by name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ManagedGrant {
    pub subject: String,
    pub role: String,
}

/// A local (password) account to provision. `password_hash` is applied only
/// when the user is first created; an existing account's password is never
/// overwritten by reconciliation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ManagedLocalUser {
    pub username: String,
    pub password_hash: String,
    pub email: String,
}

/// Marks a user_roles row as login-synced: materialized from a user's OIDC
/// group membership on login, rather than assigned interactively (managed = 0)
/// or by the declarative policy (managed = 1). Treating it as managed keeps it
/// read-only via the API (it would only reappear on next login) while letting
/// the declarative reconciler, which owns managed = 1, leave it be.
const MANAGED_LOGIN_SYNC: i64 = 2;

impl Store {
    /// Reconciles the database to the desired declarative state in a single
    /// transaction. It is authoritative for managed rows: roles, grants and
    /// group mappings present in the database with managed=1 but absent from
    /// the desired state are removed. Unmanaged rows are never touched, except
    /// that a grant or mapping duplicating a desired one is adopted (managed=1).
    /// Users are never deleted; removing a user from the policy only strips its
    /// managed roles.
    pub async fn apply_managed_rbac(&self, d: ManagedRBAC) -> Result<()> {
        self.write(move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| Error::sqlite("begin managed rbac", e))?;

            let now = now_rfc3339();

            // 1. Ensure users exist. Local accounts get a password on creation;
            //    subjects referenced only by a user grant are provisioned as OIDC
            //    placeholders so the assignment resolves before the user's first
            //    login.
            ensure_managed_users(&tx, &d, &now)?;

            // 2. Upsert desired roles, then drop managed roles no longer desired
            //    (cascade removes their permissions, grants and group mappings).
            let mut role_id: HashMap<String, i64> = HashMap::new();
            let mut names: Vec<String> = Vec::with_capacity(d.roles.len());
            for r in &d.roles {
                tx.execute(
                    "INSERT INTO roles(name, description, created_at, managed) VALUES(?, ?, ?, 1)
             ON CONFLICT(name) DO UPDATE SET description = excluded.description, managed = 1",
                    params![r.name, r.description, now],
                )
                .map_err(|e| Error::Other(format!("upsert role {:?}: {e}", r.name)))?;
                let id: i64 = tx
                    .query_row("SELECT id FROM roles WHERE name = ?", params![r.name], |row| {
                        row.get(0)
                    })
                    .map_err(|e| Error::sqlite("lookup upserted role", e))?;
                role_id.insert(r.name.clone(), id);
                names.push(r.name.clone());
            }
            delete_managed_roles_except(&tx, &names)?;

            // 3. Rebuild managed permissions.
            tx.execute("DELETE FROM role_permissions WHERE managed = 1", [])
                .map_err(|e| Error::sqlite("clear managed permissions", e))?;
            for r in &d.roles {
                for p in &r.permissions {
                    tx.execute(
                        "INSERT INTO role_permissions(role_id, repo_pattern, actions, managed) VALUES(?, ?, ?, 1)",
                        params![role_id[&r.name], p.repo_pattern, p.actions],
                    )
                    .map_err(|e| {
                        Error::Other(format!("add permission for role {:?}: {e}", r.name))
                    })?;
                }
            }

            // 4. Rebuild managed user-role assignments.
            tx.execute("DELETE FROM user_roles WHERE managed = 1", [])
                .map_err(|e| Error::sqlite("clear managed user roles", e))?;
            for g in &d.user_roles {
                let uid = lookup_user_id(&tx, &g.subject)?;
                let rid = lookup_role_id(&tx, &mut role_id, &g.role)?;
                tx.execute(
                    "INSERT INTO user_roles(user_id, role_id, managed) VALUES(?, ?, 1)
             ON CONFLICT(user_id, role_id) DO UPDATE SET managed = 1",
                    params![uid, rid],
                )
                .map_err(|e| {
                    Error::Other(format!("assign role {:?} to {:?}: {e}", g.role, g.subject))
                })?;
            }

            // 5. Rebuild managed group mappings.
            tx.execute("DELETE FROM oidc_group_mappings WHERE managed = 1", [])
                .map_err(|e| Error::sqlite("clear managed group mappings", e))?;
            for g in &d.group_roles {
                let rid = lookup_role_id(&tx, &mut role_id, &g.role)?;
                tx.execute(
                    "INSERT INTO oidc_group_mappings(group_name, role_id, managed) VALUES(?, ?, 1)
             ON CONFLICT(group_name) DO UPDATE SET role_id = excluded.role_id, managed = 1",
                    params![g.subject, rid],
                )
                .map_err(|e| {
                    Error::Other(format!("map group {:?} to role {:?}: {e}", g.subject, g.role))
                })?;
            }

            tx.commit().map_err(|e| Error::sqlite("commit managed rbac", e))
        })
        .await
    }

    /// Reconciles a user's login-synced (managed = 2) role assignments to
    /// exactly the roles currently mapped from the given Keycloak groups. It
    /// runs on every OIDC login so a user's stored roles track the identity
    /// provider's group claims: roles for groups the user has left are revoked,
    /// roles for newly-joined groups are granted. Interactive (managed = 0) and
    /// declarative (managed = 1) assignments are never touched; an existing
    /// assignment of a group-mapped role under those flags is left as-is.
    pub async fn sync_oidc_group_roles(&self, user_id: i64, groups: &[String]) -> Result<()> {
        let groups = groups.to_vec();
        self.write(move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| Error::sqlite("begin oidc group sync", e))?;

            let role_ids = group_role_ids(&tx, &groups)?;

            // Revoke login-synced roles the user no longer qualifies for.
            if role_ids.is_empty() {
                tx.execute(
                    "DELETE FROM user_roles WHERE user_id = ? AND managed = ?",
                    params![user_id, MANAGED_LOGIN_SYNC],
                )
                .map_err(|e| Error::sqlite("revoke synced roles", e))?;
                return tx
                    .commit()
                    .map_err(|e| Error::sqlite("commit oidc group sync", e));
            }
            let del = format!(
                "DELETE FROM user_roles WHERE user_id = ? AND managed = ? AND role_id NOT IN ({})",
                placeholders(role_ids.len())
            );
            let mut args: Vec<i64> = Vec::with_capacity(role_ids.len() + 2);
            args.push(user_id);
            args.push(MANAGED_LOGIN_SYNC);
            args.extend(role_ids.iter().copied());
            tx.execute(&del, params_from_iter(args.iter()))
                .map_err(|e| Error::sqlite("revoke synced roles", e))?;

            // Grant currently-qualifying group roles. DO NOTHING preserves an
            // existing interactive or declarative assignment of the same role.
            for id in &role_ids {
                tx.execute(
                    "INSERT INTO user_roles(user_id, role_id, managed) VALUES(?, ?, ?)
             ON CONFLICT(user_id, role_id) DO NOTHING",
                    params![user_id, id, MANAGED_LOGIN_SYNC],
                )
                .map_err(|e| Error::sqlite("grant synced role", e))?;
            }
            tx.commit()
                .map_err(|e| Error::sqlite("commit oidc group sync", e))
        })
        .await
    }

    /// Reports whether a role is managed by the declarative policy.
    pub async fn role_managed(&self, id: i64) -> Result<bool> {
        self.managed_flag("SELECT managed FROM roles WHERE id = ?", id)
            .await
    }

    /// Reports whether a permission is managed.
    pub async fn permission_managed(&self, id: i64) -> Result<bool> {
        self.managed_flag("SELECT managed FROM role_permissions WHERE id = ?", id)
            .await
    }

    /// Reports whether a group mapping is managed.
    pub async fn group_mapping_managed(&self, id: i64) -> Result<bool> {
        self.managed_flag("SELECT managed FROM oidc_group_mappings WHERE id = ?", id)
            .await
    }

    /// Runs a one-column `managed` lookup by id; a missing row reads as
    /// unmanaged rather than as an error.
    async fn managed_flag(&self, query: &'static str, id: i64) -> Result<bool> {
        self.read(
            move |conn| match conn.query_row(query, params![id], |r| r.get::<_, i64>(0)) {
                Ok(managed) => Ok(managed != 0),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
                Err(e) => Err(Error::sqlite("managed flag", e)),
            },
        )
        .await
    }

    /// Reports whether a user's role assignment is managed by the declarative
    /// policy (and therefore read-only via the API).
    pub async fn is_managed_user_role(&self, user_id: i64, role_id: i64) -> Result<bool> {
        self.read(move |conn| {
            match conn.query_row(
                "SELECT managed FROM user_roles WHERE user_id = ? AND role_id = ?",
                params![user_id, role_id],
                |r| r.get::<_, i64>(0),
            ) {
                Ok(managed) => Ok(managed != 0),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
                Err(e) => Err(Error::sqlite("is managed user role", e)),
            }
        })
        .await
    }
}

fn ensure_managed_users(tx: &Transaction<'_>, d: &ManagedRBAC, now: &str) -> Result<()> {
    // Local accounts first so a username appearing in both lists is created as
    // a local (password) user rather than an OIDC placeholder.
    for u in &d.local_users {
        ensure_user(
            tx,
            &u.username,
            &u.password_hash,
            &u.email,
            SOURCE_LOCAL,
            now,
        )?;
    }
    for g in &d.user_roles {
        ensure_user(tx, &g.subject, "", "", SOURCE_OIDC, now)?;
    }
    Ok(())
}

/// Inserts a managed user if absent. An existing user (managed or not) is left
/// as-is: its password, source and email are never overwritten.
fn ensure_user(
    tx: &Transaction<'_>,
    username: &str,
    password_hash: &str,
    email: &str,
    source: &str,
    now: &str,
) -> Result<()> {
    match tx.query_row(
        "SELECT id FROM users WHERE username = ?",
        params![username],
        |r| r.get::<_, i64>(0),
    ) {
        Ok(_) => return Ok(()),
        Err(rusqlite::Error::QueryReturnedNoRows) => {}
        Err(e) => return Err(Error::sqlite("lookup managed user", e)),
    }
    tx.execute(
        "INSERT INTO users(username, password_hash, source, email, disabled, created_at, updated_at, managed)
         VALUES(?, ?, ?, ?, 0, ?, ?, 1)",
        params![username, password_hash, source, email, now, now],
    )
    .map(|_| ())
    .map_err(|e| Error::sqlite("insert managed user", e))
}

fn lookup_user_id(tx: &Transaction<'_>, username: &str) -> Result<i64> {
    tx.query_row(
        "SELECT id FROM users WHERE username = ?",
        params![username],
        |r| r.get::<_, i64>(0),
    )
    .map_err(|e| Error::Other(format!("resolve user {username:?}: {e}")))
}

fn lookup_role_id(
    tx: &Transaction<'_>,
    cache: &mut HashMap<String, i64>,
    name: &str,
) -> Result<i64> {
    if let Some(id) = cache.get(name) {
        return Ok(*id);
    }
    let id: i64 = tx
        .query_row("SELECT id FROM roles WHERE name = ?", params![name], |r| {
            r.get(0)
        })
        .map_err(|e| Error::Other(format!("grant references unknown role {name:?}: {e}")))?;
    cache.insert(name.to_string(), id);
    Ok(id)
}

fn delete_managed_roles_except(tx: &Transaction<'_>, keep: &[String]) -> Result<()> {
    if keep.is_empty() {
        return tx
            .execute("DELETE FROM roles WHERE managed = 1", [])
            .map(|_| ())
            .map_err(|e| Error::sqlite("delete managed roles", e));
    }
    let q = format!(
        "DELETE FROM roles WHERE managed = 1 AND name NOT IN ({})",
        placeholders(keep.len())
    );
    tx.execute(&q, params_from_iter(keep.iter()))
        .map(|_| ())
        .map_err(|e| Error::sqlite("delete managed roles", e))
}

/// `n` comma-separated `?` placeholders for an IN list.
fn placeholders(n: usize) -> String {
    let mut s = "?,".repeat(n);
    s.pop();
    s
}

/// Resolves the distinct role IDs mapped from the given group names.
fn group_role_ids(tx: &Transaction<'_>, groups: &[String]) -> Result<Vec<i64>> {
    if groups.is_empty() {
        return Ok(Vec::new());
    }
    let q = format!(
        "SELECT DISTINCT role_id FROM oidc_group_mappings WHERE group_name IN ({})",
        placeholders(groups.len())
    );
    let mut stmt = tx
        .prepare(&q)
        .map_err(|e| Error::sqlite("group role ids", e))?;
    let ids = stmt
        .query_map(params_from_iter(groups.iter()), |r| r.get::<_, i64>(0))
        .map_err(|e| Error::sqlite("group role ids", e))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| Error::sqlite("group role ids", e))?;
    Ok(ids)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;

    use crate::meta::*;

    fn dev_policy() -> ManagedRBAC {
        ManagedRBAC {
            roles: vec![
                ManagedRole {
                    name: "readonly".into(),
                    permissions: vec![Permission {
                        repo_pattern: "*".into(),
                        actions: "read".into(),
                        ..Permission::default()
                    }],
                    ..ManagedRole::default()
                },
                ManagedRole {
                    name: "dev".into(),
                    permissions: vec![Permission {
                        repo_pattern: "team-*".into(),
                        actions: "read,write".into(),
                        ..Permission::default()
                    }],
                    ..ManagedRole::default()
                },
            ],
            group_roles: vec![ManagedGrant {
                subject: "/platform".into(),
                role: "readonly".into(),
            }],
            user_roles: vec![ManagedGrant {
                subject: "alice".into(),
                role: "dev".into(),
            }],
            local_users: Vec::new(),
        }
    }

    #[tokio::test]
    async fn apply_managed_rbac() {
        let (s, _dir) = test_store().await;

        s.apply_managed_rbac(dev_policy()).await.expect("apply");

        let roles = s.list_roles().await.unwrap();
        assert_eq!(roles.len(), 2, "roles = {}, want 2", roles.len());
        for r in &roles {
            assert!(r.managed, "role {:?} should be managed", r.name);
        }

        // alice was provisioned as an OIDC placeholder and granted dev.
        let alice = s
            .get_user_by_username("alice")
            .await
            .expect("alice not created");
        assert_eq!(alice.source, SOURCE_OIDC, "alice source, want oidc");
        let perms = s.permissions_for_user(alice.id).await.unwrap();
        assert_eq!(perms.len(), 1, "alice perms = {perms:?}");
        assert_eq!(perms[0].actions, "read,write", "alice perms = {perms:?}");

        // Group mapping resolves to readonly.
        let names = s
            .role_names_for_groups(&["/platform".to_string()])
            .await
            .unwrap();
        assert_eq!(names, vec!["readonly".to_string()], "group roles");
    }

    #[tokio::test]
    async fn apply_managed_rbac_authoritative() {
        let (s, _dir) = test_store().await;

        // Seed an unmanaged role + mapping via the regular API path.
        let unmanaged = s
            .create_role(Role {
                name: "unmanaged".into(),
                ..Role::default()
            })
            .await
            .unwrap();
        s.add_permission(Permission {
            role_id: unmanaged.id,
            repo_pattern: "*".into(),
            actions: "read".into(),
            ..Permission::default()
        })
        .await
        .unwrap();
        s.create_group_mapping("/manual", unmanaged.id)
            .await
            .unwrap();

        s.apply_managed_rbac(dev_policy()).await.expect("apply 1");

        // Re-apply a policy that drops the dev role entirely.
        let mut shrunk = dev_policy();
        shrunk.roles.truncate(1); // keep only readonly
        shrunk.user_roles.clear();
        s.apply_managed_rbac(shrunk).await.expect("apply 2");

        let roles = s.list_roles().await.unwrap();
        let got: Vec<&str> = roles.iter().map(|r| r.name.as_str()).collect();
        assert!(
            !got.contains(&"dev"),
            "managed role 'dev' should be removed after policy shrink"
        );
        assert!(
            got.contains(&"readonly"),
            "managed role 'readonly' should remain"
        );
        // The unmanaged role and its mapping survive reconciliation untouched.
        assert!(
            got.contains(&"unmanaged"),
            "unmanaged role must be preserved"
        );
        let mappings = s.list_group_mappings().await.unwrap();
        let manual = mappings.iter().find(|m| m.group_name == "/manual");
        let manual = manual.expect("unmanaged group mapping must be preserved");
        assert!(!manual.managed, "/manual mapping must stay unmanaged");
    }

    /// Pins the decoupling contract: removing a role from the chart policy removes
    /// it from the database along with every assignment of it, whether the policy
    /// made that assignment or an operator did it by hand in the UI. No user_roles
    /// row may outlive its role, or a later role reusing the id would silently
    /// inherit those members.
    #[tokio::test]
    async fn apply_managed_rbac_detaches_users_from_dropped_role() {
        let (s, _dir) = test_store().await;

        s.apply_managed_rbac(dev_policy()).await.expect("apply 1");
        // alice holds `dev` through the policy; bob was given the same role by hand.
        let bob = s
            .create_user(User {
                username: "bob".into(),
                password_hash: "x".into(),
                source: SOURCE_LOCAL.into(),
                ..User::default()
            })
            .await
            .unwrap();
        let roles = s.list_roles().await.unwrap();
        let dev_id = roles
            .iter()
            .find(|r| r.name == "dev")
            .map(|r| r.id)
            .expect("dev role not created");
        s.assign_role(bob.id, dev_id).await.unwrap();

        // The chart drops `dev`.
        let mut shrunk = dev_policy();
        shrunk.roles.truncate(1);
        shrunk.user_roles.clear();
        s.apply_managed_rbac(shrunk).await.expect("apply 2");

        let by_user = s.roles_by_user().await.unwrap();
        for (uid, rs) in &by_user {
            for r in rs {
                assert!(
                    r.name != "dev" && r.id != dev_id,
                    "user {uid} still holds the dropped role: {r:?}"
                );
            }
        }
        // bob keeps existing; only the grant goes away.
        s.get_user(bob.id)
            .await
            .expect("bob should survive the role removal");
        // And the permissions the role carried are gone with it.
        let perms = s.permissions_for_user(bob.id).await.unwrap();
        assert!(
            perms.is_empty(),
            "bob should hold no permissions: {perms:?}"
        );
    }

    #[tokio::test]
    async fn apply_managed_rbac_clears_all_managed() {
        let (s, _dir) = test_store().await;
        s.apply_managed_rbac(dev_policy()).await.unwrap();
        // An empty policy removes every managed role (and cascades grants/mappings).
        s.apply_managed_rbac(ManagedRBAC::default())
            .await
            .expect("apply empty");
        let roles = s.list_roles().await.unwrap();
        assert!(roles.is_empty(), "managed roles should be wiped: {roles:?}");
        let mappings = s.list_group_mappings().await.unwrap();
        assert!(
            mappings.is_empty(),
            "managed mappings should be wiped: {mappings:?}"
        );
    }

    #[tokio::test]
    async fn apply_managed_rbac_unknown_role() {
        let (s, _dir) = test_store().await;
        // A grant referencing a role neither defined in the policy nor existing in
        // the database is rejected.
        let err = s
            .apply_managed_rbac(ManagedRBAC {
                group_roles: vec![ManagedGrant {
                    subject: "/x".into(),
                    role: "ghost".into(),
                }],
                ..ManagedRBAC::default()
            })
            .await;
        assert!(err.is_err(), "expected error for grant to unknown role");
    }

    #[tokio::test]
    async fn apply_managed_rbac_local_user() {
        let (s, _dir) = test_store().await;

        // Pre-existing local user must not have its password overwritten.
        let existing = s
            .create_user(User {
                username: "bootstrap".into(),
                password_hash: "original".into(),
                source: SOURCE_LOCAL.into(),
                ..User::default()
            })
            .await
            .unwrap();

        let d = ManagedRBAC {
            roles: vec![ManagedRole {
                name: "dev".into(),
                permissions: vec![Permission {
                    repo_pattern: "*".into(),
                    actions: "read".into(),
                    ..Permission::default()
                }],
                ..ManagedRole::default()
            }],
            local_users: vec![
                ManagedLocalUser {
                    username: "ci-bot".into(),
                    password_hash: "hashed-pw".into(),
                    ..ManagedLocalUser::default()
                },
                ManagedLocalUser {
                    username: "bootstrap".into(),
                    password_hash: "should-not-apply".into(),
                    ..ManagedLocalUser::default()
                },
            ],
            user_roles: vec![ManagedGrant {
                subject: "ci-bot".into(),
                role: "dev".into(),
            }],
            ..ManagedRBAC::default()
        };
        s.apply_managed_rbac(d).await.expect("apply");

        let bot = s
            .get_user_by_username("ci-bot")
            .await
            .expect("ci-bot not created");
        assert_eq!(bot.source, SOURCE_LOCAL, "ci-bot = {bot:?}");
        assert_eq!(bot.password_hash, "hashed-pw", "ci-bot = {bot:?}");

        let again = s.get_user(existing.id).await.unwrap();
        assert_eq!(
            again.password_hash, "original",
            "existing password overwritten"
        );
    }

    #[tokio::test]
    async fn managed_flag_helpers() {
        let (s, _dir) = test_store().await;
        s.apply_managed_rbac(dev_policy()).await.unwrap();

        let roles = s.list_roles().await.unwrap();
        let dev_id = roles.iter().find(|r| r.name == "dev").unwrap().id;
        assert!(s.role_managed(dev_id).await.unwrap(), "role_managed(dev)");
        assert!(
            !s.role_managed(99999).await.unwrap(),
            "missing role should report unmanaged"
        );

        let perms = s.list_permissions().await.unwrap();
        assert!(
            s.permission_managed(perms[0].id).await.unwrap(),
            "permission_managed"
        );

        let mappings = s.list_group_mappings().await.unwrap();
        assert!(
            s.group_mapping_managed(mappings[0].id).await.unwrap(),
            "group_mapping_managed"
        );

        let alice = s.get_user_by_username("alice").await.unwrap();
        let dev = s.get_role_by_name("dev").await.unwrap();
        assert!(
            s.is_managed_user_role(alice.id, dev.id).await.unwrap(),
            "is_managed_user_role"
        );
        assert!(
            !s.is_managed_user_role(alice.id, 99999).await.unwrap(),
            "missing assignment should report unmanaged"
        );
    }

    #[tokio::test]
    async fn sync_oidc_group_roles() {
        let (s, _dir) = test_store().await;

        let readonly = s
            .create_role(Role {
                name: "readonly".into(),
                ..Role::default()
            })
            .await
            .unwrap();
        let security = s
            .create_role(Role {
                name: "security".into(),
                ..Role::default()
            })
            .await
            .unwrap();
        let admins = s
            .create_role(Role {
                name: "admins".into(),
                ..Role::default()
            })
            .await
            .unwrap();
        s.create_group_mapping("/security", security.id)
            .await
            .expect("map security");
        s.create_group_mapping("/administrator", admins.id)
            .await
            .expect("map admin");

        let u = s
            .create_user(User {
                username: "bob".into(),
                source: SOURCE_OIDC.into(),
                ..User::default()
            })
            .await
            .unwrap();

        // An admin manually grants readonly; this interactive (managed=0) row must
        // survive every sync.
        s.assign_role(u.id, readonly.id)
            .await
            .expect("manual assign");

        // First login: member of /security only.
        s.sync_oidc_group_roles(u.id, &["/security".to_string()])
            .await
            .expect("sync 1");
        let got = role_names(&s, u.id).await;
        assert!(
            same_set(&got, &["readonly", "security"]),
            "after sync 1 roles = {got:?}"
        );
        // The synced role is treated as managed (read-only via API).
        assert!(
            s.is_managed_user_role(u.id, security.id).await.unwrap(),
            "synced security role should report managed"
        );
        // The manual grant stays unmanaged.
        assert!(
            !s.is_managed_user_role(u.id, readonly.id).await.unwrap(),
            "manual readonly grant should stay unmanaged"
        );

        // Second login: moved from /security to /administrator. security is revoked,
        // admins granted, manual readonly untouched.
        s.sync_oidc_group_roles(u.id, &["/administrator".to_string()])
            .await
            .expect("sync 2");
        let got = role_names(&s, u.id).await;
        assert!(
            same_set(&got, &["readonly", "admins"]),
            "after sync 2 roles = {got:?}"
        );

        // Third login: no group claims. All login-synced roles revoked, manual stays.
        s.sync_oidc_group_roles(u.id, &[]).await.expect("sync 3");
        let got = role_names(&s, u.id).await;
        assert!(
            same_set(&got, &["readonly"]),
            "after sync 3 roles = {got:?}"
        );
    }

    async fn role_names(s: &Store, user_id: i64) -> Vec<String> {
        let by_user = s.roles_by_user().await.expect("roles_by_user");
        by_user
            .get(&user_id)
            .map(|rs| rs.iter().map(|r| r.name.clone()).collect())
            .unwrap_or_default()
    }

    fn same_set(a: &[String], b: &[&str]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let mut m: HashMap<&str, i64> = HashMap::new();
        for x in a {
            *m.entry(x.as_str()).or_default() += 1;
        }
        for x in b {
            *m.entry(x).or_default() -= 1;
        }
        m.values().all(|v| *v == 0)
    }
}
