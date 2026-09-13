//! Users, roles and permissions, OIDC group mappings and personal access tokens: the tables
//! behind authentication and authorization.

use std::collections::HashMap;

use rusqlite::{Row, params, params_from_iter};

use super::time::{format_time_opt, now_rfc3339, parse_time, parse_time_opt};
use super::{Error, GroupMapping, Permission, Result, Role, SOURCE_LOCAL, Store, Token, User};

fn ensure_affected(n: usize) -> Result<()> {
    if n == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
}

// --- Users ---

impl Store {
    /// Inserts a user.
    pub async fn create_user(&self, mut u: User) -> Result<User> {
        let now = now_rfc3339();
        if u.source.is_empty() {
            u.source = SOURCE_LOCAL.to_string();
        }
        let id = self
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO users(username, password_hash, source, email, disabled, robot, created_at, updated_at)
         VALUES(?, ?, ?, ?, ?, ?, ?, ?)",
                    params![
                        u.username,
                        u.password_hash,
                        u.source,
                        u.email,
                        u.disabled,
                        u.robot,
                        now,
                        now
                    ],
                )
                .map_err(|e| Error::sqlite("create user", e))?;
                Ok(conn.last_insert_rowid())
            })
            .await?;
        self.get_user(id).await
    }

    /// Upserts an OIDC user by username, returning the stored row. It is used at
    /// login to keep a local record of external identities.
    pub async fn ensure_user(&self, username: &str, email: &str, source: &str) -> Result<User> {
        match self.get_user_by_username(username).await {
            Ok(u) => return Ok(u),
            Err(Error::NotFound) => {}
            Err(e) => return Err(e),
        }
        self.create_user(User {
            username: username.to_string(),
            email: email.to_string(),
            source: source.to_string(),
            ..User::default()
        })
        .await
    }

    /// Returns a user by ID.
    pub async fn get_user(&self, id: i64) -> Result<User> {
        self.read(move |conn| {
            conn.query_row(
                "SELECT id, username, password_hash, source, email, disabled, created_at, updated_at, last_login_at, lockout_enabled, failed_login_count, locked_at, robot FROM users WHERE id = ?",
                params![id],
                scan_user,
            )
            .map_err(|e| Error::sqlite("get user", e))
        })
        .await
    }

    /// Returns a user by username.
    pub async fn get_user_by_username(&self, username: &str) -> Result<User> {
        let username = username.to_string();
        self.read(move |conn| {
            conn.query_row(
                "SELECT id, username, password_hash, source, email, disabled, created_at, updated_at, last_login_at, lockout_enabled, failed_login_count, locked_at, robot FROM users WHERE username = ?",
                params![username],
                scan_user,
            )
            .map_err(|e| Error::sqlite("get user by username", e))
        })
        .await
    }

    /// Returns all users ordered by username.
    pub async fn list_users(&self) -> Result<Vec<User>> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare("SELECT id, username, password_hash, source, email, disabled, created_at, updated_at, last_login_at, lockout_enabled, failed_login_count, locked_at, robot FROM users ORDER BY username")
                .map_err(|e| Error::sqlite("list users", e))?;
            let out = stmt
                .query_map([], scan_user)
                .map_err(|e| Error::sqlite("list users", e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::sqlite("list users", e))?;
            Ok(out)
        })
        .await
    }

    /// Updates a user's password hash.
    pub async fn set_password(&self, id: i64, hash: &str) -> Result<()> {
        let hash = hash.to_string();
        self.write(move |conn| {
            let n = conn
                .execute(
                    "UPDATE users SET password_hash = ?, updated_at = ? WHERE id = ?",
                    params![hash, now_rfc3339(), id],
                )
                .map_err(|e| Error::sqlite("set password", e))?;
            ensure_affected(n)
        })
        .await
    }

    /// Records a successful interactive login. It deliberately does not bump
    /// updated_at, which tracks profile changes.
    pub async fn touch_last_login(&self, id: i64) -> Result<()> {
        self.write(move |conn| {
            let n = conn
                .execute(
                    "UPDATE users SET last_login_at = ? WHERE id = ?",
                    params![now_rfc3339(), id],
                )
                .map_err(|e| Error::sqlite("touch last login", e))?;
            ensure_affected(n)
        })
        .await
    }

    /// Toggles a user's disabled flag.
    pub async fn set_user_disabled(&self, id: i64, disabled: bool) -> Result<()> {
        self.write(move |conn| {
            let n = conn
                .execute(
                    "UPDATE users SET disabled = ?, updated_at = ? WHERE id = ?",
                    params![disabled, now_rfc3339(), id],
                )
                .map_err(|e| Error::sqlite("set user disabled", e))?;
            ensure_affected(n)
        })
        .await
    }

    /// Toggles a user's failed-password lockout. Disabling it also clears any
    /// accumulated failure count and unlocks the account, so turning the
    /// feature off can never leave someone stuck locked out.
    pub async fn set_lockout_enabled(&self, id: i64, enabled: bool) -> Result<()> {
        self.write(move |conn| {
            let n = if enabled {
                conn.execute(
                    "UPDATE users SET lockout_enabled = 1, updated_at = ? WHERE id = ?",
                    params![now_rfc3339(), id],
                )
            } else {
                conn.execute(
                    "UPDATE users SET lockout_enabled = 0, failed_login_count = 0, locked_at = '', updated_at = ? WHERE id = ?",
                    params![now_rfc3339(), id],
                )
            }
            .map_err(|e| Error::sqlite("set lockout enabled", e))?;
            ensure_affected(n)
        })
        .await
    }

    /// Records one failed local-password attempt. When the account opted into
    /// lockout and the running count reaches threshold, it sets locked_at
    /// (idempotent: an already-locked account keeps its original time).
    pub async fn register_failed_login(&self, id: i64, threshold: i64) -> Result<()> {
        self.write(move |conn| {
            let n = conn
                .execute(
                    "UPDATE users
		    SET failed_login_count = failed_login_count + 1,
		        locked_at = CASE
		            WHEN lockout_enabled = 1 AND failed_login_count + 1 >= ? AND locked_at = ''
		                THEN ? ELSE locked_at END,
		        updated_at = ?
		  WHERE id = ?",
                    params![threshold, now_rfc3339(), now_rfc3339(), id],
                )
                .map_err(|e| Error::sqlite("register failed login", e))?;
            ensure_affected(n)
        })
        .await
    }

    /// Clears the failure count and unlocks the account. Called on a successful
    /// login and by an admin unlock action.
    pub async fn reset_failed_login(&self, id: i64) -> Result<()> {
        self.write(move |conn| {
            let n = conn
                .execute(
                    "UPDATE users SET failed_login_count = 0, locked_at = '', updated_at = ? WHERE id = ?",
                    params![now_rfc3339(), id],
                )
                .map_err(|e| Error::sqlite("reset failed login", e))?;
            ensure_affected(n)
        })
        .await
    }

    /// Removes a user (cascading to roles and tokens).
    pub async fn delete_user(&self, id: i64) -> Result<()> {
        self.write(move |conn| {
            let n = conn
                .execute("DELETE FROM users WHERE id = ?", params![id])
                .map_err(|e| Error::sqlite("delete user", e))?;
            ensure_affected(n)
        })
        .await
    }
}

// --- Roles & permissions ---

impl Store {
    /// Inserts a role.
    pub async fn create_role(&self, mut r: Role) -> Result<Role> {
        let (name, description) = (r.name.clone(), r.description.clone());
        let id = self
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO roles(name, description, created_at) VALUES(?, ?, ?)",
                    params![name, description, now_rfc3339()],
                )
                .map_err(|e| Error::sqlite("create role", e))?;
                Ok(conn.last_insert_rowid())
            })
            .await?;
        r.id = id;
        Ok(r)
    }

    /// Returns a role by name.
    pub async fn get_role_by_name(&self, name: &str) -> Result<Role> {
        let name = name.to_string();
        self.read(move |conn| {
            conn.query_row(
                "SELECT id, name, description, created_at, managed FROM roles WHERE name = ?",
                params![name],
                scan_role,
            )
            .map_err(|e| Error::sqlite("get role by name", e))
        })
        .await
    }

    /// Returns all roles.
    pub async fn list_roles(&self) -> Result<Vec<Role>> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, name, description, created_at, managed FROM roles ORDER BY name",
                )
                .map_err(|e| Error::sqlite("list roles", e))?;
            let out = stmt
                .query_map([], scan_role)
                .map_err(|e| Error::sqlite("list roles", e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::sqlite("list roles", e))?;
            Ok(out)
        })
        .await
    }

    /// Removes a role.
    pub async fn delete_role(&self, id: i64) -> Result<()> {
        self.write(move |conn| {
            let n = conn
                .execute("DELETE FROM roles WHERE id = ?", params![id])
                .map_err(|e| Error::sqlite("delete role", e))?;
            ensure_affected(n)
        })
        .await
    }

    /// Grants actions on a repo pattern to a role.
    pub async fn add_permission(&self, mut p: Permission) -> Result<Permission> {
        let (role_id, pattern, actions) = (p.role_id, p.repo_pattern.clone(), p.actions.clone());
        let id = self
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO role_permissions(role_id, repo_pattern, actions) VALUES(?, ?, ?)",
                    params![role_id, pattern, actions],
                )
                .map_err(|e| Error::sqlite("add permission", e))?;
                Ok(conn.last_insert_rowid())
            })
            .await?;
        p.id = id;
        Ok(p)
    }

    /// Returns every role permission (grouped by role in the API).
    pub async fn list_permissions(&self) -> Result<Vec<Permission>> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare("SELECT id, role_id, repo_pattern, actions, managed FROM role_permissions ORDER BY id")
                .map_err(|e| Error::sqlite("list permissions", e))?;
            let rows = stmt
                .query_map([], scan_permission)
                .map_err(|e| Error::sqlite("list permissions", e))?;
            collect_permissions(rows, "list permissions")
        })
        .await
    }

    /// Removes one permission from a role.
    pub async fn delete_permission(&self, role_id: i64, id: i64) -> Result<()> {
        self.write(move |conn| {
            let n = conn
                .execute(
                    "DELETE FROM role_permissions WHERE id = ? AND role_id = ?",
                    params![id, role_id],
                )
                .map_err(|e| Error::sqlite("delete permission", e))?;
            ensure_affected(n)
        })
        .await
    }

    /// Returns every user's assigned roles in one query, keyed by user ID. Used
    /// by the admin user list.
    pub async fn roles_by_user(&self) -> Result<HashMap<i64, Vec<Role>>> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT ur.user_id, r.id, r.name, r.description, r.created_at
         FROM user_roles ur JOIN roles r ON r.id = ur.role_id ORDER BY r.name",
                )
                .map_err(|e| Error::sqlite("roles by user", e))?;
            let rows = stmt
                .query_map([], |r| {
                    let user_id: i64 = r.get(0)?;
                    let created: String = r.get(4)?;
                    Ok((
                        user_id,
                        Role {
                            id: r.get(1)?,
                            name: r.get(2)?,
                            description: r.get(3)?,
                            created_at: parse_time(&created),
                            managed: false,
                        },
                    ))
                })
                .map_err(|e| Error::sqlite("roles by user", e))?;
            let mut out: HashMap<i64, Vec<Role>> = HashMap::new();
            for row in rows {
                let (user_id, role) = row.map_err(|e| Error::sqlite("roles by user", e))?;
                out.entry(user_id).or_default().push(role);
            }
            Ok(out)
        })
        .await
    }

    /// Grants a role to a user.
    pub async fn assign_role(&self, user_id: i64, role_id: i64) -> Result<()> {
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO user_roles(user_id, role_id) VALUES(?, ?) ON CONFLICT DO NOTHING",
                params![user_id, role_id],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("assign role", e))
        })
        .await
    }

    /// Revokes a role from a user.
    pub async fn remove_role(&self, user_id: i64, role_id: i64) -> Result<()> {
        self.write(move |conn| {
            conn.execute(
                "DELETE FROM user_roles WHERE user_id = ? AND role_id = ?",
                params![user_id, role_id],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("remove role", e))
        })
        .await
    }

    /// Returns the permissions granted to a user via their roles.
    pub async fn permissions_for_user(&self, user_id: i64) -> Result<Vec<Permission>> {
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT rp.id, rp.role_id, rp.repo_pattern, rp.actions, rp.managed
         FROM role_permissions rp
         JOIN user_roles ur ON ur.role_id = rp.role_id
         WHERE ur.user_id = ?",
                )
                .map_err(|e| Error::sqlite("permissions for user", e))?;
            let rows = stmt
                .query_map(params![user_id], scan_permission)
                .map_err(|e| Error::sqlite("permissions for user", e))?;
            collect_permissions(rows, "permissions for user")
        })
        .await
    }

    /// Returns the permissions for a set of role names. It is used to resolve
    /// OIDC group-derived roles into permissions. An empty set of names yields
    /// an empty list without touching the database.
    pub async fn permissions_for_role_names(&self, names: &[String]) -> Result<Vec<Permission>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let names = names.to_vec();
        self.read(move |conn| {
            let mut query = String::from(
                "SELECT rp.id, rp.role_id, rp.repo_pattern, rp.actions, rp.managed
              FROM role_permissions rp JOIN roles r ON r.id = rp.role_id WHERE r.name IN (",
            );
            for i in 0..names.len() {
                if i > 0 {
                    query.push(',');
                }
                query.push('?');
            }
            query.push(')');
            let mut stmt = conn
                .prepare(&query)
                .map_err(|e| Error::sqlite("permissions for role names", e))?;
            let rows = stmt
                .query_map(params_from_iter(names.iter()), scan_permission)
                .map_err(|e| Error::sqlite("permissions for role names", e))?;
            collect_permissions(rows, "permissions for role names")
        })
        .await
    }
}

// --- OIDC group mappings ---

impl Store {
    /// Maps a group name to a role.
    pub async fn create_group_mapping(&self, group_name: &str, role_id: i64) -> Result<()> {
        let group_name = group_name.to_string();
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO oidc_group_mappings(group_name, role_id) VALUES(?, ?)
         ON CONFLICT(group_name) DO UPDATE SET role_id = excluded.role_id",
                params![group_name, role_id],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("create group mapping", e))
        })
        .await
    }

    /// Returns all group-to-role mappings.
    pub async fn list_group_mappings(&self) -> Result<Vec<GroupMapping>> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare("SELECT id, group_name, role_id, managed FROM oidc_group_mappings ORDER BY group_name")
                .map_err(|e| Error::sqlite("list group mappings", e))?;
            let out = stmt
                .query_map([], |r| {
                    let managed: i64 = r.get(3)?;
                    Ok(GroupMapping {
                        id: r.get(0)?,
                        group_name: r.get(1)?,
                        role_id: r.get(2)?,
                        managed: managed != 0,
                    })
                })
                .map_err(|e| Error::sqlite("list group mappings", e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::sqlite("list group mappings", e))?;
            Ok(out)
        })
        .await
    }

    /// Removes a mapping.
    pub async fn delete_group_mapping(&self, id: i64) -> Result<()> {
        self.write(move |conn| {
            let n = conn
                .execute("DELETE FROM oidc_group_mappings WHERE id = ?", params![id])
                .map_err(|e| Error::sqlite("delete group mapping", e))?;
            ensure_affected(n)
        })
        .await
    }

    /// Returns the role names mapped from the given group names. An empty set of
    /// groups yields an empty list without touching the database.
    pub async fn role_names_for_groups(&self, groups: &[String]) -> Result<Vec<String>> {
        if groups.is_empty() {
            return Ok(Vec::new());
        }
        let groups = groups.to_vec();
        self.read(move |conn| {
            let mut query = String::from(
                "SELECT r.name FROM oidc_group_mappings m JOIN roles r ON r.id = m.role_id WHERE m.group_name IN (",
            );
            for i in 0..groups.len() {
                if i > 0 {
                    query.push(',');
                }
                query.push('?');
            }
            query.push(')');
            let mut stmt = conn
                .prepare(&query)
                .map_err(|e| Error::sqlite("role names for groups", e))?;
            let out = stmt
                .query_map(params_from_iter(groups.iter()), |r| r.get::<_, String>(0))
                .map_err(|e| Error::sqlite("role names for groups", e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::sqlite("role names for groups", e))?;
            Ok(out)
        })
        .await
    }
}

// --- Tokens (PAT) ---

impl Store {
    /// Stores a personal access token (hash only).
    pub async fn create_token(&self, mut t: Token) -> Result<Token> {
        let row = t.clone();
        let id = self
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO tokens(user_id, name, description, hash, scopes_json, expires_at, created_at)
         VALUES(?, ?, ?, ?, ?, ?, ?)",
                    params![
                        row.user_id,
                        row.name,
                        row.description,
                        row.hash,
                        row.scopes_json,
                        format_time_opt(row.expires_at),
                        now_rfc3339()
                    ],
                )
                .map_err(|e| Error::sqlite("create token", e))?;
                Ok(conn.last_insert_rowid())
            })
            .await?;
        t.id = id;
        Ok(t)
    }

    /// Returns a token by its hash.
    pub async fn get_token_by_hash(&self, hash: &str) -> Result<Token> {
        let hash = hash.to_string();
        self.read(move |conn| {
            conn.query_row(
                "SELECT id, user_id, name, hash, scopes_json, expires_at, last_used_at, created_at FROM tokens WHERE hash = ?",
                params![hash],
                |r| {
                    let expires: Option<String> = r.get(5)?;
                    let last_used: Option<String> = r.get(6)?;
                    let created: String = r.get(7)?;
                    Ok(Token {
                        id: r.get(0)?,
                        user_id: r.get(1)?,
                        name: r.get(2)?,
                        description: String::new(),
                        hash: r.get(3)?,
                        scopes_json: r.get(4)?,
                        expires_at: parse_time_opt(expires.as_deref()),
                        last_used_at: parse_time_opt(last_used.as_deref()),
                        created_at: parse_time(&created),
                    })
                },
            )
            .map_err(|e| Error::sqlite("get token by hash", e))
        })
        .await
    }

    /// Returns a user's tokens (without the hash).
    pub async fn list_tokens(&self, user_id: i64) -> Result<Vec<Token>> {
        self.read(move |conn| {
            let mut stmt = conn
                .prepare("SELECT id, user_id, name, description, '' , scopes_json, expires_at, last_used_at, created_at FROM tokens WHERE user_id = ? ORDER BY created_at DESC")
                .map_err(|e| Error::sqlite("list tokens", e))?;
            let out = stmt
                .query_map(params![user_id], scan_token_listing)
                .map_err(|e| Error::sqlite("list tokens", e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::sqlite("list tokens", e))?;
            Ok(out)
        })
        .await
    }

    /// Returns how many access tokens a user currently owns. Revoking a token
    /// deletes its row, so this reflects the live count used to enforce the
    /// per-user token cap.
    pub async fn count_tokens(&self, user_id: i64) -> Result<i64> {
        self.read(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM tokens WHERE user_id = ?",
                params![user_id],
                |r| r.get::<_, i64>(0),
            )
            .map_err(|e| Error::sqlite("count tokens", e))
        })
        .await
    }

    /// Returns the token count per user id, for list views that would otherwise
    /// need one `count_tokens` query per row. Users with no tokens are absent
    /// from the map, so a missing key means zero.
    pub async fn token_counts_by_user(&self) -> Result<HashMap<i64, i64>> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare("SELECT user_id, COUNT(*) FROM tokens GROUP BY user_id")
                .map_err(|e| Error::sqlite("token counts by user", e))?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
                .map_err(|e| Error::sqlite("token counts by user", e))?;
            let mut out = HashMap::new();
            for row in rows {
                let (user_id, n) = row.map_err(|e| Error::sqlite("token counts by user", e))?;
                out.insert(user_id, n);
            }
            Ok(out)
        })
        .await
    }

    /// Returns every token across users (admin views), with scopes, owner id and
    /// expiry, so the API can surface which tokens reach a repository.
    pub async fn list_all_tokens(&self) -> Result<Vec<Token>> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare("SELECT id, user_id, name, description, '', scopes_json, expires_at, last_used_at, created_at FROM tokens ORDER BY created_at DESC")
                .map_err(|e| Error::sqlite("list all tokens", e))?;
            let out = stmt
                .query_map([], scan_token_listing)
                .map_err(|e| Error::sqlite("list all tokens", e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::sqlite("list all tokens", e))?;
            Ok(out)
        })
        .await
    }

    /// Records the last-used time of a token.
    pub async fn touch_token(&self, id: i64) -> Result<()> {
        self.write(move |conn| {
            conn.execute(
                "UPDATE tokens SET last_used_at = ? WHERE id = ?",
                params![now_rfc3339(), id],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("touch token", e))
        })
        .await
    }

    /// Replaces the scopes of a token owned by `user_id`. Name, expiry and the
    /// secret itself never change: scopes are the only mutable part, so
    /// tightening or extending access does not force a re-issue.
    pub async fn update_token_scopes(
        &self,
        user_id: i64,
        id: i64,
        scopes_json: &str,
    ) -> Result<()> {
        let scopes_json = scopes_json.to_string();
        self.write(move |conn| {
            let n = conn
                .execute(
                    "UPDATE tokens SET scopes_json = ? WHERE id = ? AND user_id = ?",
                    params![scopes_json, id, user_id],
                )
                .map_err(|e| Error::sqlite("update token scopes", e))?;
            ensure_affected(n)
        })
        .await
    }

    /// Removes a token owned by `user_id`.
    pub async fn delete_token(&self, user_id: i64, id: i64) -> Result<()> {
        self.write(move |conn| {
            let n = conn
                .execute(
                    "DELETE FROM tokens WHERE id = ? AND user_id = ?",
                    params![id, user_id],
                )
                .map_err(|e| Error::sqlite("delete token", e))?;
            ensure_affected(n)
        })
        .await
    }

    /// Returns the number of users (used for first-run bootstrap).
    pub async fn count_users(&self) -> Result<i64> {
        self.read(|conn| {
            conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, i64>(0))
                .map_err(|e| Error::sqlite("count users", e))
        })
        .await
    }
}

// --- scan helpers ---

/// Reads one row of the 13-column users projection.
fn scan_user(r: &Row<'_>) -> rusqlite::Result<User> {
    let disabled: i64 = r.get(5)?;
    let created: String = r.get(6)?;
    let updated: String = r.get(7)?;
    let last_login: String = r.get(8)?;
    let lockout_enabled: i64 = r.get(9)?;
    let locked_at: String = r.get(11)?;
    let robot: i64 = r.get(12)?;
    Ok(User {
        id: r.get(0)?,
        username: r.get(1)?,
        password_hash: r.get(2)?,
        source: r.get(3)?,
        email: r.get(4)?,
        disabled: disabled != 0,
        robot: robot != 0,
        created_at: parse_time(&created),
        updated_at: parse_time(&updated),
        last_login_at: parse_time_opt(Some(&last_login)),
        lockout_enabled: lockout_enabled != 0,
        failed_login_count: r.get(10)?,
        locked_at: parse_time_opt(Some(&locked_at)),
    })
}

fn scan_role(r: &Row<'_>) -> rusqlite::Result<Role> {
    let created: String = r.get(3)?;
    let managed: i64 = r.get(4)?;
    Ok(Role {
        id: r.get(0)?,
        name: r.get(1)?,
        description: r.get(2)?,
        created_at: parse_time(&created),
        managed: managed != 0,
    })
}

fn scan_permission(r: &Row<'_>) -> rusqlite::Result<Permission> {
    let managed: i64 = r.get(4)?;
    Ok(Permission {
        id: r.get(0)?,
        role_id: r.get(1)?,
        repo_pattern: r.get(2)?,
        actions: r.get(3)?,
        managed: managed != 0,
    })
}

fn collect_permissions<'a>(
    rows: impl Iterator<Item = rusqlite::Result<Permission>> + 'a,
    op: &'static str,
) -> Result<Vec<Permission>> {
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| Error::sqlite(op, e))
}

/// Reads the 9-column listing projection whose fifth column is the literal `''`
/// standing in for the hash, so listings never carry the secret's digest.
fn scan_token_listing(r: &Row<'_>) -> rusqlite::Result<Token> {
    let expires: Option<String> = r.get(6)?;
    let last_used: Option<String> = r.get(7)?;
    let created: String = r.get(8)?;
    Ok(Token {
        id: r.get(0)?,
        user_id: r.get(1)?,
        name: r.get(2)?,
        description: r.get(3)?,
        hash: r.get(4)?,
        scopes_json: r.get(5)?,
        expires_at: parse_time_opt(expires.as_deref()),
        last_used_at: parse_time_opt(last_used.as_deref()),
        created_at: parse_time(&created),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use chrono::{Duration, Utc};

    use crate::meta::*;

    #[tokio::test]
    async fn user_crud() {
        let (s, _dir) = test_store().await;

        let u = s
            .create_user(User {
                username: "alice".into(),
                password_hash: "h".into(),
                email: "a@x.io".into(),
                ..User::default()
            })
            .await
            .unwrap();
        assert_eq!(u.source, SOURCE_LOCAL, "default source");
        assert_eq!(
            s.get_user_by_username("alice").await.unwrap().id,
            u.id,
            "get by username mismatch"
        );
        assert_eq!(s.count_users().await.unwrap(), 1, "count");

        s.set_password(u.id, "h2").await.unwrap();
        assert_eq!(
            s.get_user(u.id).await.unwrap().password_hash,
            "h2",
            "password not updated"
        );
        s.set_user_disabled(u.id, true).await.unwrap();
        assert!(s.get_user(u.id).await.unwrap().disabled, "not disabled");

        // ensure_user is idempotent and provisions OIDC users.
        let e1 = s.ensure_user("bob", "b@x.io", SOURCE_OIDC).await.unwrap();
        let e2 = s.ensure_user("bob", "b@x.io", SOURCE_OIDC).await.unwrap();
        assert_eq!(e1.id, e2.id, "ensure_user not idempotent");

        let list = s.list_users().await.unwrap();
        assert_eq!(list.len(), 2, "list len");
        s.delete_user(u.id).await.unwrap();
        let err = s.get_user(u.id).await.unwrap_err();
        assert!(err.is_not_found(), "want not found, got {err}");
    }

    #[tokio::test]
    async fn roles_permissions_assignment() {
        let (s, _dir) = test_store().await;

        let u = s
            .create_user(User {
                username: "dev".into(),
                ..User::default()
            })
            .await
            .unwrap();
        let role = s
            .create_role(Role {
                name: "writers".into(),
                ..Role::default()
            })
            .await
            .unwrap();
        assert_eq!(
            s.get_role_by_name("writers").await.unwrap().id,
            role.id,
            "role get mismatch"
        );
        s.add_permission(Permission {
            role_id: role.id,
            repo_pattern: "maven-*".into(),
            actions: "read,write".into(),
            ..Permission::default()
        })
        .await
        .unwrap();
        s.assign_role(u.id, role.id).await.unwrap();
        // Idempotent assign.
        s.assign_role(u.id, role.id).await.unwrap();

        let perms = s.permissions_for_user(u.id).await.unwrap();
        assert_eq!(perms.len(), 1, "perms = {perms:?}");
        assert_eq!(perms[0].repo_pattern, "maven-*", "perms = {perms:?}");
        let by_name = s
            .permissions_for_role_names(&["writers".to_string()])
            .await
            .unwrap();
        assert_eq!(by_name.len(), 1, "perms by name = {by_name:?}");
        assert!(
            s.permissions_for_role_names(&[]).await.unwrap().is_empty(),
            "empty names should return nothing"
        );

        s.remove_role(u.id, role.id).await.unwrap();
        assert!(
            s.permissions_for_user(u.id).await.unwrap().is_empty(),
            "role not removed"
        );

        let roles = s.list_roles().await.unwrap();
        assert_eq!(roles.len(), 1, "roles len");
        s.delete_role(role.id).await.unwrap();
    }

    #[tokio::test]
    async fn group_mappings() {
        let (s, _dir) = test_store().await;
        let role = s
            .create_role(Role {
                name: "platform".into(),
                ..Role::default()
            })
            .await
            .unwrap();

        s.create_group_mapping("team-platform", role.id)
            .await
            .unwrap();
        // Upsert on conflict.
        s.create_group_mapping("team-platform", role.id)
            .await
            .unwrap();
        let names = s
            .role_names_for_groups(&["team-platform".to_string(), "unknown".to_string()])
            .await
            .unwrap();
        assert_eq!(names, vec!["platform".to_string()], "role names");
        assert!(
            s.role_names_for_groups(&[]).await.unwrap().is_empty(),
            "empty groups should return nothing"
        );

        let list = s.list_group_mappings().await.unwrap();
        assert_eq!(list.len(), 1, "mappings");
        s.delete_group_mapping(list[0].id).await.unwrap();
    }

    #[tokio::test]
    async fn tokens() {
        let (s, _dir) = test_store().await;
        let u = s
            .create_user(User {
                username: "svc".into(),
                ..User::default()
            })
            .await
            .unwrap();

        let exp = Utc::now() + Duration::hours(1);
        let tok = s
            .create_token(Token {
                user_id: u.id,
                name: "ci".into(),
                hash: "abc123".into(),
                scopes_json: "[]".into(),
                expires_at: Some(exp),
                ..Token::default()
            })
            .await
            .unwrap();
        let got = s.get_token_by_hash("abc123").await.unwrap();
        assert_eq!(got.id, tok.id, "get by hash");
        assert!(got.expires_at.is_some(), "expiry not persisted");
        s.touch_token(tok.id).await.unwrap();

        let list = s.list_tokens(u.id).await.unwrap();
        assert_eq!(list.len(), 1, "list should omit hash: {list:?}");
        assert_eq!(list[0].hash, "", "list should omit hash: {list:?}");
        s.delete_token(u.id, tok.id).await.unwrap();
        let err = s.get_token_by_hash("abc123").await.unwrap_err();
        assert!(err.is_not_found(), "want not found, got {err}");
    }

    #[tokio::test]
    async fn list_all_tokens_store() {
        let (s, _dir) = test_store().await;
        let u = s
            .create_user(User {
                username: "svc".into(),
                ..User::default()
            })
            .await
            .unwrap();
        let exp = Utc::now() + Duration::hours(1);
        s.create_token(Token {
            user_id: u.id,
            name: "ci".into(),
            hash: "h1".into(),
            scopes_json: "[]".into(),
            expires_at: Some(exp),
            ..Token::default()
        })
        .await
        .unwrap();
        let toks = s.list_all_tokens().await.unwrap();
        assert_eq!(toks.len(), 1, "list all tokens = {toks:?}");
        assert_eq!(toks[0].name, "ci", "list all tokens = {toks:?}");
    }

    #[tokio::test]
    async fn token_counts_by_user() {
        let (s, _dir) = test_store().await;

        let counts = s.token_counts_by_user().await.unwrap();
        assert!(counts.is_empty(), "empty counts = {counts:?}");

        let mut ids = Vec::new();
        for name in ["alice", "bob", "carol"] {
            ids.push(
                s.create_user(User {
                    username: name.into(),
                    password_hash: "x".into(),
                    source: SOURCE_LOCAL.into(),
                    ..User::default()
                })
                .await
                .unwrap()
                .id,
            );
        }
        let (alice, bob) = (ids[0], ids[1]);
        for (user_id, name, hash) in [
            (alice, "ci", "hash-1"),
            (alice, "laptop", "hash-2"),
            (bob, "ci", "hash-3"),
        ] {
            s.create_token(Token {
                user_id,
                name: name.into(),
                hash: hash.into(),
                scopes_json: "[]".into(),
                ..Token::default()
            })
            .await
            .unwrap_or_else(|e| panic!("create token {name}: {e}"));
        }

        let counts = s.token_counts_by_user().await.expect("counts");
        assert_eq!(counts.get(&alice), Some(&2), "counts = {counts:?}");
        assert_eq!(counts.get(&bob), Some(&1), "counts = {counts:?}");
        // carol owns none, so she is absent rather than present with a zero.
        assert_eq!(counts.len(), 2, "counts = {counts:?}, want only the owners");
        // The per-user count used to enforce the cap agrees with the grouped one.
        let n = s.count_tokens(alice).await.unwrap();
        assert_eq!(n, counts[&alice], "count_tokens = {n}");
    }

    #[tokio::test]
    async fn role_permission_queries() {
        let (s, _dir) = test_store().await;

        let u = s
            .create_user(User {
                username: "dev".into(),
                ..User::default()
            })
            .await
            .unwrap();
        let r1 = s
            .create_role(Role {
                name: "readers".into(),
                ..Role::default()
            })
            .await
            .unwrap();
        let r2 = s
            .create_role(Role {
                name: "writers".into(),
                ..Role::default()
            })
            .await
            .unwrap();
        let p1 = s
            .add_permission(Permission {
                role_id: r1.id,
                repo_pattern: "*".into(),
                actions: "read".into(),
                ..Permission::default()
            })
            .await
            .unwrap();
        s.add_permission(Permission {
            role_id: r2.id,
            repo_pattern: "maven-*".into(),
            actions: "write".into(),
            ..Permission::default()
        })
        .await
        .unwrap();
        s.assign_role(u.id, r1.id).await.unwrap();
        s.assign_role(u.id, r2.id).await.unwrap();

        // list_permissions returns every permission across roles.
        let perms = s.list_permissions().await.unwrap();
        assert_eq!(perms.len(), 2, "list permissions, want 2");

        // roles_by_user maps the user to both roles, ordered by name.
        let by_user = s.roles_by_user().await.unwrap();
        let roles = &by_user[&u.id];
        assert_eq!(roles.len(), 2, "roles for user = {roles:?}");
        assert_eq!(roles[0].name, "readers", "roles for user = {roles:?}");
        assert_eq!(roles[1].name, "writers", "roles for user = {roles:?}");

        // delete_permission is scoped to its role: a wrong role ID must not delete.
        let err = s.delete_permission(r2.id, p1.id).await.unwrap_err();
        assert!(err.is_not_found(), "cross-role delete err = {err}");
        s.delete_permission(r1.id, p1.id).await.unwrap();
        let perms = s.list_permissions().await.unwrap();
        assert_eq!(perms.len(), 1, "after delete, want 1");
    }
}
