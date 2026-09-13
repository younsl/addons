//! Authentication (local password, Keycloak OIDC, personal access tokens) and
//! repository-scoped RBAC authorization.
//!
//! [`Service`] is the entry point: it resolves a request's [`Principal`] from
//! whichever credential the caller presented, and the middleware in
//! [`middleware`] publishes that principal in the request extensions for the API
//! and repository routers to enforce against.

use std::sync::Arc;
use std::time::Duration;

use crate::meta;

pub(crate) mod authz;
mod credentials;
pub mod middleware;
mod oidc;
mod policy;
mod reconcile;
mod session;

pub use authz::{
    ACTION_ADMIN, ACTION_APPROVE, ACTION_AUDIT, ACTION_DELETE, ACTION_READ, ACTION_SECURITY,
    ACTION_WRITE, Principal, Scope, match_repo_pattern,
};
pub use credentials::{
    MAX_FAILED_LOGINS, generate_token, hash_password, hash_token, is_pat, random_password,
    set_test_hash_cost, verify_password,
};
pub use middleware::{
    OptionalPrincipal, RequirePrincipal, clear_session_cookie, from_request, from_request_parts,
    middleware, require_admin, require_approver, require_approver_or_auditor, require_auditor,
    require_auth, set_session_cookie, unauthorized, unauthorized_basic,
};
pub use oidc::{OidcParams, OidcProvider, handle_callback, handle_login, handle_logout};
pub use policy::parse_policy;
pub use reconcile::reconcile_rbac;
pub use session::{SessionCodec, SessionData};

/// Authentication, session and authorization errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Authentication failed.
    #[error("invalid credentials")]
    InvalidCredential,
    /// A valid account is locked out after too many failed password attempts and
    /// must be unlocked by an administrator.
    #[error("account locked")]
    AccountLocked,
    /// A session cookie was malformed, forged or expired.
    #[error("{0}")]
    Session(String),
    /// A declarative RBAC policy failed to parse.
    #[error("{0}")]
    Policy(String),
    /// Discovery, verification or the token exchange failed.
    #[error("{0}")]
    Oidc(String),
    /// The metadata store failed.
    #[error("{0}")]
    Meta(#[from] meta::Error),
    /// A JSON payload (session cookie, token scopes) failed to encode or decode.
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    /// Free-form failure with a message.
    #[error("{0}")]
    Other(String),
}

/// Result alias used throughout the module.
pub type Result<T> = std::result::Result<T, Error>;

/// The default session lifetime when [`Options::session_ttl`] is zero.
const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// Bounds an impersonated session. It is deliberately far shorter than a normal
/// session so a forgotten impersonation expires on its own instead of lingering
/// for the full session lifetime.
pub const IMPERSONATION_TTL: Duration = Duration::from_secs(60 * 60);

/// Configures the auth [`Service`].
#[derive(Default)]
pub struct Options {
    pub session_secret: Vec<u8>,
    pub session_ttl: Duration,
    pub anonymous_read: bool,
    /// Optional OIDC provider; `None` disables the login flow.
    pub oidc: Option<Arc<OidcProvider>>,
    /// When set, grants its permissions to every authenticated principal
    /// regardless of explicit role assignments (ArgoCD `policy.default`).
    pub default_role: String,
    /// Names the seeded admin account, which is exempt from failed-password
    /// lockout so an operator can never lock themselves out of the only
    /// guaranteed admin.
    pub bootstrap_admin_user: String,
}

/// Authenticates requests and resolves effective permissions.
pub struct Service {
    store: Arc<meta::Store>,
    codec: SessionCodec,
    oidc: Option<Arc<OidcProvider>>,
    anonymous_read: bool,
    default_role: String,
    bootstrap_admin: String,
}

impl Service {
    /// Builds a Service. If `session_secret` is empty, an ephemeral random
    /// secret is generated (suitable for single-instance only).
    pub fn new(store: Arc<meta::Store>, opts: Options) -> Arc<Service> {
        let mut secret = opts.session_secret;
        if secret.is_empty() {
            secret = vec![0u8; 32];
            rand::fill(&mut secret[..]);
            tracing::warn!(
                "FORKLIFT_SESSION_SECRET not set; using ephemeral secret (sessions will not survive restart or work across replicas)"
            );
        }
        let ttl = if opts.session_ttl.is_zero() {
            DEFAULT_SESSION_TTL
        } else {
            opts.session_ttl
        };
        Arc::new(Service {
            store,
            codec: SessionCodec::new(secret, ttl),
            oidc: opts.oidc,
            anonymous_read: opts.anonymous_read,
            default_role: opts.default_role,
            bootstrap_admin: opts.bootstrap_admin_user,
        })
    }

    /// The metadata store the service authenticates against.
    pub fn store(&self) -> &Arc<meta::Store> {
        &self.store
    }

    /// The configured OIDC provider, if any.
    pub fn oidc(&self) -> Option<Arc<OidcProvider>> {
        self.oidc.clone()
    }

    /// Reports whether `username` is the seeded bootstrap admin, which is exempt
    /// from failed-password lockout.
    pub fn is_protected_admin(&self, username: &str) -> bool {
        !self.bootstrap_admin.is_empty() && username == self.bootstrap_admin
    }

    /// Reports whether unauthenticated reads are allowed.
    pub fn anonymous_read(&self) -> bool {
        self.anonymous_read
    }

    /// Reports whether OIDC login is available.
    pub fn oidc_enabled(&self) -> bool {
        self.oidc.is_some()
    }

    /// Verifies a username/password against a local user. When the account opted
    /// into lockout, consecutive password failures are counted and the account is
    /// refused once locked (even with the right password) until an admin unlocks
    /// it; a success clears the count. The bootstrap admin is never locked.
    pub async fn authenticate_local(&self, username: &str, password: &str) -> Result<meta::User> {
        let u = self
            .store
            .get_user_by_username(username)
            .await
            .map_err(|_| Error::InvalidCredential)?;
        if u.disabled || u.robot || u.source != meta::SOURCE_LOCAL || u.password_hash.is_empty() {
            return Err(Error::InvalidCredential);
        }
        let protected = self.is_protected_admin(&u.username);
        if u.locked() && !protected {
            return Err(Error::AccountLocked);
        }
        let hash = u.password_hash.clone();
        let plain = password.to_string();
        let ok = tokio::task::spawn_blocking(move || verify_password(&hash, &plain))
            .await
            .unwrap_or(false);
        if !ok {
            if !protected
                && let Err(e) = self
                    .store
                    .register_failed_login(u.id, MAX_FAILED_LOGINS)
                    .await
            {
                tracing::warn!(user = %u.username, err = %e, "record failed login");
            }
            return Err(Error::InvalidCredential);
        }
        // Success: clear any accumulated failures (write only when there is state
        // to clear, so steady-state Basic-auth requests stay read-only).
        if (u.failed_login_count > 0 || u.locked())
            && let Err(e) = self.store.reset_failed_login(u.id).await
        {
            tracing::warn!(user = %u.username, err = %e, "reset failed login");
        }
        Ok(u)
    }

    /// Encodes a signed session cookie value for a user.
    pub fn issue_session(&self, username: &str, source: &str, groups: &[String]) -> Result<String> {
        self.codec.encode(username, source, groups)
    }

    /// Encodes a session cookie value that acts as the target user while
    /// recording the administrator who started it. The resulting principal
    /// carries only the target's permissions.
    pub fn issue_impersonation(
        &self,
        target: &str,
        source: &str,
        impersonator: &str,
    ) -> Result<String> {
        self.codec
            .encode_impersonated(target, source, impersonator, IMPERSONATION_TTL)
    }

    /// Identifies the principal for a request, or `None` for anonymous.
    /// Authorization wins over a simultaneously supplied session cookie so audit
    /// attribution and CSRF behavior cannot be confused by mixed credentials.
    pub async fn resolve(&self, parts: &http::request::Parts) -> Result<Option<Arc<Principal>>> {
        let authz = parts
            .headers
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if let Some(tok) = authz.strip_prefix("Bearer ") {
            if is_pat(tok) {
                return self.principal_from_token(tok).await;
            }
            if self.oidc.is_some() {
                return self.principal_from_bearer_jwt(tok).await;
            }
        } else if authz.starts_with("Basic ")
            && let Some((user, pass)) = middleware::basic_auth(&authz)
        {
            return if is_pat(&pass) {
                self.principal_from_token(&pass).await
            } else if is_pat(&user) {
                self.principal_from_token(&user).await
            } else {
                self.principal_from_password(&user, &pass).await
            };
        }
        if !authz.is_empty() {
            return Ok(None);
        }
        if let Some(value) = middleware::cookie(parts, middleware::SESSION_COOKIE)
            && let Ok(data) = self.codec.decode(&value)
        {
            return self.principal_from_session(data).await;
        }
        Ok(None)
    }

    /// Returns the claim bound into a valid signed session cookie. It is exposed
    /// by `/me` so JavaScript never needs access to the HttpOnly cookie.
    pub fn csrf_token(&self, parts: &http::request::Parts) -> Option<String> {
        let value = middleware::cookie(parts, middleware::SESSION_COOKIE)?;
        let data = self.codec.decode(&value).ok()?;
        if data.csrf.is_empty() {
            return None;
        }
        Some(data.csrf)
    }

    /// Accepts Authorization-authenticated requests and otherwise requires a
    /// constant-time match against the signed session claim.
    pub fn validate_csrf(&self, parts: &http::request::Parts) -> bool {
        if parts.headers.contains_key(http::header::AUTHORIZATION) {
            return true;
        }
        let Some(value) = middleware::cookie(parts, middleware::SESSION_COOKIE) else {
            return false;
        };
        let Ok(data) = self.codec.decode(&value) else {
            return false;
        };
        let header = parts
            .headers
            .get("X-CSRF-Token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        session::valid_csrf(&data, header)
    }

    async fn principal_from_session(&self, d: SessionData) -> Result<Option<Arc<Principal>>> {
        let Ok(u) = self.store.get_user_by_username(&d.username).await else {
            return Ok(None);
        };
        // Robot accounts never hold a session (login is refused), but reject any
        // stale/forged session cookie for one defensively.
        if u.disabled || u.robot {
            return Ok(None);
        }
        let mut p = self
            .build_principal(&u, &d.groups, false, Vec::new())
            .await?;
        // Permissions above are the impersonated user's own; this only records
        // who is behind the session so it can be audited and stopped.
        p.impersonator = d.impersonator;
        Ok(Some(Arc::new(p)))
    }

    pub(crate) async fn principal_from_password(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<Arc<Principal>>> {
        let Ok(u) = self.authenticate_local(username, password).await else {
            return Ok(None);
        };
        Ok(Some(Arc::new(
            self.build_principal(&u, &[], false, Vec::new()).await?,
        )))
    }

    async fn principal_from_token(&self, plaintext: &str) -> Result<Option<Arc<Principal>>> {
        let Ok(t) = self.store.get_token_by_hash(&hash_token(plaintext)).await else {
            return Ok(None);
        };
        if t.expires_at.is_some_and(|e| chrono::Utc::now() > e) {
            return Ok(None);
        }
        let u = match self.store.get_user(t.user_id).await {
            Ok(u) if !u.disabled => u,
            _ => return Ok(None),
        };
        let mut scopes: Vec<Scope> = Vec::new();
        if !t.scopes_json.is_empty() {
            scopes = serde_json::from_str(&t.scopes_json).unwrap_or_default();
        }
        let _ = self.store.touch_token(t.id).await;
        Ok(Some(Arc::new(
            self.build_principal(&u, &[], true, scopes).await?,
        )))
    }

    async fn principal_from_bearer_jwt(&self, raw: &str) -> Result<Option<Arc<Principal>>> {
        let Some(oidc) = self.oidc.as_ref() else {
            return Ok(None);
        };
        let Ok((username, _email, groups)) = oidc.verify(raw) else {
            return Ok(None);
        };
        // Unknown identity presenting a valid token: treat as anonymous until a
        // login flow records the user.
        let Ok(u) = self.store.get_user_by_username(&username).await else {
            return Ok(None);
        };
        // OIDC is an interactive login path; a disabled or robot account must not
        // authenticate through it (robots use personal access tokens only).
        if u.disabled || u.robot {
            return Ok(None);
        }
        Ok(Some(Arc::new(
            self.build_principal(&u, &groups, false, Vec::new()).await?,
        )))
    }

    /// Resolves the effective permissions for a user, combining
    /// directly-assigned roles with OIDC group-mapped roles.
    pub(crate) async fn build_principal(
        &self,
        u: &meta::User,
        groups: &[String],
        via_token: bool,
        scopes: Vec<Scope>,
    ) -> Result<Principal> {
        let mut perms = self.store.permissions_for_user(u.id).await?;
        if !groups.is_empty() {
            let names = self.store.role_names_for_groups(groups).await?;
            let group_perms = self.store.permissions_for_role_names(&names).await?;
            perms.extend(group_perms);
        }
        // Every authenticated principal inherits the default role's permissions,
        // if one is configured (e.g. read-only access for all signed-in users).
        if !self.default_role.is_empty() {
            let def_perms = self
                .store
                .permissions_for_role_names(std::slice::from_ref(&self.default_role))
                .await?;
            perms.extend(def_perms);
        }
        Ok(Principal {
            username: u.username.clone(),
            source: u.source.clone(),
            impersonator: String::new(),
            perms,
            via_token,
            token_scopes: scopes,
        })
    }

    /// Lists the usernames of enabled users who may approve packages on `repo`,
    /// computed from each user's persisted roles (directly assigned plus any
    /// synced from OIDC groups) and the default role, using the same
    /// [`Principal::can`] check the API enforces. OIDC group approvers who have
    /// never signed in are not persisted and so are not enumerable here.
    pub async fn approvers_for(&self, repo: &str) -> Result<Vec<String>> {
        let users = self.store.list_users().await?;
        let mut out = Vec::new();
        for u in users {
            if u.disabled {
                continue;
            }
            let p = self.build_principal(&u, &[], false, Vec::new()).await?;
            if p.can(repo, ACTION_APPROVE) {
                out.push(u.username);
            }
        }
        Ok(out)
    }

    /// Seeds an initial admin user and role on first run when no users exist yet.
    /// It is idempotent and a no-op once any user is present. When no password is
    /// supplied, a random one is generated and logged once so the operator can
    /// sign in and rotate it.
    pub async fn bootstrap_admin(&self, username: &str, password: &str) -> Result<()> {
        let username = if username.is_empty() {
            "admin"
        } else {
            username
        };
        if self.store.count_users().await? > 0 {
            return Ok(());
        }
        let generated = password.is_empty();
        let password = if generated {
            random_password()?
        } else {
            password.to_string()
        };
        let hash = hash_password(&password)?;
        let u = match self
            .store
            .create_user(meta::User {
                username: username.to_string(),
                password_hash: hash,
                source: meta::SOURCE_LOCAL.to_string(),
                ..Default::default()
            })
            .await
        {
            Ok(u) => u,
            // Another replica won the bootstrap race; treat as already done.
            Err(meta::Error::Conflict) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let role = match self.store.get_role_by_name("administrator").await {
            Ok(r) => r,
            Err(meta::Error::NotFound) => {
                let role = self
                    .store
                    .create_role(meta::Role {
                        name: "administrator".into(),
                        description: "Full administrative access".into(),
                        ..Default::default()
                    })
                    .await?;
                self.store
                    .add_permission(meta::Permission {
                        role_id: role.id,
                        repo_pattern: "*".into(),
                        actions: ACTION_ADMIN.into(),
                        ..Default::default()
                    })
                    .await?;
                role
            }
            Err(e) => return Err(e.into()),
        };
        self.store.assign_role(u.id, role.id).await?;
        if generated {
            tracing::warn!(
                username = %username,
                password = %password,
                "generated initial admin password; sign in and rotate it"
            );
        } else {
            tracing::info!(username = %username, "bootstrapped admin user");
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    mod service {
        use crate::auth::authz::match_glob;
        use crate::auth::*;
        use crate::meta;
        use crate::testing::auth::*;
        use axum::Router;
        use axum::body::Body;
        use axum::routing::get;
        use chrono::{TimeZone, Utc};
        use http::{Request, StatusCode};
        use std::sync::Arc;
        use std::time::Duration;
        use tower::ServiceExt as _;
        #[test]
        fn glob_match_cases() {
            let cases: &[(&str, &str, bool)] = &[
                ("*", "anything", true),
                ("maven-*", "maven-central", true),
                ("maven-*", "npm-proxy", false),
                ("*-proxy", "npm-proxy", true),
                ("exact", "exact", true),
                ("exact", "other", false),
                ("a*c", "abc", true),
                ("a*c", "ac", true),
                ("a*c", "abd", false),
            ];
            for (pattern, name, want) in cases {
                assert_eq!(
                    match_glob(pattern, name),
                    *want,
                    "match_glob({pattern:?},{name:?})"
                );
            }
            // Empty name only matches "*".
            assert!(
                !match_glob("foo", ""),
                "empty name should not match non-wildcard pattern"
            );
        }

        #[test]
        fn principal_can() {
            let p = Principal {
                perms: vec![
                    meta::Permission {
                        repo_pattern: "maven-*".into(),
                        actions: "read,write".into(),
                        ..Default::default()
                    },
                    meta::Permission {
                        repo_pattern: "shared".into(),
                        actions: "read".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            };
            assert!(
                p.can("maven-central", ACTION_READ) && p.can("maven-central", ACTION_WRITE),
                "expected read/write on maven-central"
            );
            assert!(
                !p.can("maven-central", ACTION_DELETE),
                "delete should be denied"
            );
            assert!(
                p.can("shared", ACTION_READ) && !p.can("shared", ACTION_WRITE),
                "shared should be read-only"
            );
            assert!(!p.can("npm-proxy", ACTION_READ), "npm-proxy not granted");
        }

        #[test]
        fn principal_admin_implies_all() {
            let p = Principal {
                perms: vec![meta::Permission {
                    repo_pattern: "*".into(),
                    actions: "admin".into(),
                    ..Default::default()
                }],
                ..Default::default()
            };
            assert!(
                p.is_admin() && p.can("anything", ACTION_DELETE),
                "admin should imply all actions"
            );
        }

        #[test]
        fn principal_token_scope_narrows() {
            let base = vec![meta::Permission {
                repo_pattern: "*".into(),
                actions: "admin".into(),
                ..Default::default()
            }];
            // Unscoped token inherits full access.
            let full = Principal {
                perms: base.clone(),
                via_token: true,
                ..Default::default()
            };
            assert!(
                full.can("any", ACTION_WRITE),
                "unscoped token should inherit role perms"
            );
            // Scoped token narrows to a single repo/action.
            let scoped = Principal {
                perms: base,
                via_token: true,
                token_scopes: vec![Scope {
                    repo_pattern: "maven-*".into(),
                    actions: vec![ACTION_READ.into()],
                }],
                ..Default::default()
            };
            assert!(
                scoped.can("maven-central", ACTION_READ),
                "scoped read should be allowed"
            );
            assert!(
                !scoped.can("maven-central", ACTION_WRITE) && !scoped.can("npm", ACTION_READ),
                "scope should narrow access"
            );
        }

        #[test]
        fn password_and_token() {
            crate::testing::auth::init();
            let hash = hash_password("s3cret").expect("hash");
            assert!(
                verify_password(&hash, "s3cret") && !verify_password(&hash, "wrong"),
                "password verification broken"
            );

            let (plain, h) = generate_token().expect("generate token");
            assert!(is_pat(&plain), "generated token has no PAT prefix: {plain}");
            assert_eq!(hash_token(&plain), h, "token hash mismatch");
            assert!(
                !is_pat("not-a-token"),
                "arbitrary string should not be a PAT"
            );
        }

        #[test]
        fn session_codec() {
            let mut c = SessionCodec::new(b"secret".to_vec(), Duration::from_secs(3600));
            let base = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
            c.now = Arc::new(move || base);

            let val = c
                .encode("alice", "local", &["devs".to_string()])
                .expect("encode");
            let d = c.decode(&val).expect("decode");
            assert_eq!(d.username, "alice");
            assert_eq!(d.groups.len(), 1);

            // Tampering breaks the signature.
            assert!(
                c.decode(&format!("{val}x")).is_err(),
                "tampered session should fail"
            );
            // Expired session is rejected.
            let later = base + chrono::Duration::hours(2);
            c.now = Arc::new(move || later);
            assert!(c.decode(&val).is_err(), "expired session should fail");
        }

        #[tokio::test]
        async fn bootstrap_and_local_auth() {
            let t = new_test_service().await;

            t.svc
                .bootstrap_admin("admin", "pw")
                .await
                .expect("bootstrap");
            // Idempotent: second call is a no-op.
            t.svc
                .bootstrap_admin("admin", "pw")
                .await
                .expect("re-bootstrap");
            assert_eq!(t.store.count_users().await.unwrap(), 1, "user count");

            t.svc
                .authenticate_local("admin", "pw")
                .await
                .expect("admin login failed");
            assert!(
                t.svc.authenticate_local("admin", "bad").await.is_err(),
                "bad password should fail"
            );
        }

        #[tokio::test]
        async fn account_lockout() {
            let t = new_test_service_with(Options {
                session_secret: b"test-secret-test-secret-test-secret".to_vec(),
                bootstrap_admin_user: "admin".into(),
                ..Default::default()
            })
            .await;

            let hash = hash_password("pw").unwrap();
            let u = t
                .store
                .create_user(meta::User {
                    username: "alice".into(),
                    password_hash: hash,
                    source: meta::SOURCE_LOCAL.into(),
                    ..Default::default()
                })
                .await
                .expect("create user");
            t.store.set_lockout_enabled(u.id, true).await.unwrap();

            // MAX_FAILED_LOGINS-1 failures must not lock yet.
            for i in 0..MAX_FAILED_LOGINS - 1 {
                let err = t.svc.authenticate_local("alice", "bad").await.unwrap_err();
                assert!(
                    matches!(err, Error::InvalidCredential),
                    "attempt {i}: err = {err}, want invalid credential"
                );
            }
            // A correct password before the threshold still works and resets the count.
            t.svc
                .authenticate_local("alice", "pw")
                .await
                .expect("login before lock should succeed");

            // Now fail the full threshold to lock the account.
            for _ in 0..MAX_FAILED_LOGINS {
                let _ = t.svc.authenticate_local("alice", "bad").await;
            }
            let err = t.svc.authenticate_local("alice", "pw").await.unwrap_err();
            assert!(
                matches!(err, Error::AccountLocked),
                "locked account: err = {err}, want AccountLocked"
            );

            // Admin unlock restores access.
            t.store.reset_failed_login(u.id).await.unwrap();
            t.svc
                .authenticate_local("alice", "pw")
                .await
                .expect("after unlock");

            // The protected bootstrap admin never locks, even with lockout enabled.
            let admin_hash = hash_password("pw").unwrap();
            let admin = t
                .store
                .create_user(meta::User {
                    username: "admin".into(),
                    password_hash: admin_hash,
                    source: meta::SOURCE_LOCAL.into(),
                    ..Default::default()
                })
                .await
                .unwrap();
            t.store.set_lockout_enabled(admin.id, true).await.unwrap();
            for _ in 0..MAX_FAILED_LOGINS + 3 {
                let _ = t.svc.authenticate_local("admin", "bad").await;
            }
            t.svc
                .authenticate_local("admin", "pw")
                .await
                .expect("protected admin must never lock");
        }

        #[tokio::test]
        async fn bootstrap_generates_random_password() {
            let t = new_test_service().await;

            // Empty password on an empty DB seeds an admin with a generated password.
            t.svc.bootstrap_admin("", "").await.expect("bootstrap");
            assert_eq!(
                t.store.count_users().await.unwrap(),
                1,
                "user count (admin auto-created)"
            );
            let u = t
                .store
                .get_user_by_username("admin")
                .await
                .expect("admin not created");
            assert!(
                !u.password_hash.is_empty(),
                "generated admin should have a password hash"
            );
            // Idempotent: a second call does not create another user.
            t.svc.bootstrap_admin("", "").await.expect("re-bootstrap");
            assert_eq!(
                t.store.count_users().await.unwrap(),
                1,
                "count after re-bootstrap"
            );
        }

        #[test]
        fn random_password_unique() {
            let a = random_password().expect("random password");
            let b = random_password().expect("random password");
            assert!(a != b && a.len() >= 16, "weak random password: {a:?} {b:?}");
        }

        /// Verifies the Robot Account invariant: interactive login is refused, but the
        /// robot's personal access token still authenticates.
        #[tokio::test]
        async fn robot_account_login_vs_token() {
            let t = new_test_service().await;

            // A robot has no password and is flagged robot.
            let robot = t
                .store
                .create_user(meta::User {
                    username: "ci-bot".into(),
                    source: meta::SOURCE_LOCAL.into(),
                    robot: true,
                    ..Default::default()
                })
                .await
                .expect("create robot");
            assert!(robot.robot, "created robot did not round-trip robot=true");

            // Interactive login (even if a password were somehow set) is refused.
            assert!(
                t.svc
                    .authenticate_local("ci-bot", "anything")
                    .await
                    .is_err(),
                "robot account must not authenticate interactively"
            );

            // Its token authenticates for package operations.
            let (plain, hash) = generate_token().unwrap();
            t.store
                .create_token(meta::Token {
                    user_id: robot.id,
                    name: "ci".into(),
                    hash,
                    scopes_json: r#"[{"repo_pattern":"*","actions":["read","write"]}]"#.into(),
                    ..Default::default()
                })
                .await
                .expect("create token");
            let parts = Request::builder()
                .uri("/")
                .header(http::header::AUTHORIZATION, format!("Bearer {plain}"))
                .body(Body::empty())
                .unwrap()
                .into_parts()
                .0;
            let p = t.svc.resolve(&parts).await.expect("resolve");
            assert!(
                p.is_some(),
                "robot token must resolve (authentication succeeds even though login is refused)"
            );

            // Disabling the robot also disables its token (disabled gates all auth).
            t.store.set_user_disabled(robot.id, true).await.unwrap();
            assert!(
                t.svc.resolve(&parts).await.unwrap().is_none(),
                "disabled robot's token must not resolve"
            );
        }

        #[tokio::test]
        async fn resolve_basic_and_token() {
            let t = new_test_service().await;
            t.svc
                .bootstrap_admin("admin", "pw")
                .await
                .expect("bootstrap");

            // Basic auth resolves an admin principal.
            let parts = basic_auth_parts("admin", "pw");
            let p = t.svc.resolve(&parts).await.expect("basic resolve");
            assert!(p.is_some_and(|p| p.is_admin()), "basic resolve");

            // Create a scoped PAT for the admin and resolve via Bearer.
            let admin = t.store.get_user_by_username("admin").await.unwrap();
            let (plain, hash) = generate_token().unwrap();
            t.store
                .create_token(meta::Token {
                    user_id: admin.id,
                    name: "ci".into(),
                    hash,
                    scopes_json: r#"[{"repo_pattern":"maven-*","actions":["read"]}]"#.into(),
                    ..Default::default()
                })
                .await
                .expect("create token");
            let parts = Request::builder()
                .uri("/")
                .header(http::header::AUTHORIZATION, format!("Bearer {plain}"))
                .body(Body::empty())
                .unwrap()
                .into_parts()
                .0;
            let p = t.svc.resolve(&parts).await.expect("token resolve");
            let p = p.expect("token resolve");
            assert!(
                p.can("maven-central", ACTION_READ) && !p.can("maven-central", ACTION_WRITE),
                "token scope not enforced"
            );

            // Anonymous request resolves to nil.
            assert!(
                t.svc.resolve(&request_parts()).await.unwrap().is_none(),
                "expected anonymous principal"
            );
        }

        #[tokio::test]
        async fn rbac_group_mapping() {
            let t = new_test_service().await;

            let role = t
                .store
                .create_role(meta::Role {
                    name: "maven-writers".into(),
                    ..Default::default()
                })
                .await
                .unwrap();
            t.store
                .add_permission(meta::Permission {
                    role_id: role.id,
                    repo_pattern: "maven-*".into(),
                    actions: "read,write".into(),
                    ..Default::default()
                })
                .await
                .unwrap();
            t.store
                .create_group_mapping("team-platform", role.id)
                .await
                .unwrap();
            let u = t
                .store
                .create_user(meta::User {
                    username: "bob".into(),
                    source: meta::SOURCE_OIDC.into(),
                    ..Default::default()
                })
                .await
                .unwrap();

            let p = t
                .svc
                .build_principal(&u, &["team-platform".to_string()], false, Vec::new())
                .await
                .expect("build principal");
            assert!(
                p.can("maven-snapshots", ACTION_WRITE),
                "group-mapped role should grant write"
            );
            assert!(
                !p.can("npm-proxy", ACTION_READ),
                "unmapped repo should be denied"
            );
        }

        /// A router that records the principal the middleware injected.
        fn principal_probe(
            svc: Arc<Service>,
            sink: Arc<parking_lot::Mutex<Option<Arc<Principal>>>>,
        ) -> Router {
            Router::new()
                .route(
                    "/",
                    get(move |req: Request<Body>| {
                        let sink = Arc::clone(&sink);
                        async move {
                            *sink.lock() = from_request(&req);
                            StatusCode::OK
                        }
                    }),
                )
                .layer(axum::middleware::from_fn_with_state(
                    svc,
                    crate::auth::middleware,
                ))
        }

        #[tokio::test]
        async fn middleware_injects_principal() {
            let t = new_test_service().await;
            t.svc.bootstrap_admin("admin", "pw").await.unwrap();

            let sink = Arc::new(parking_lot::Mutex::new(None));
            let app = principal_probe(Arc::clone(&t.svc), Arc::clone(&sink));
            let resp = app
                .oneshot(to_request(basic_auth_parts("admin", "pw")))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let got = sink.lock().clone();
            assert_eq!(
                got.map(|p| p.username.clone()).as_deref(),
                Some("admin"),
                "principal not injected"
            );
        }

        #[tokio::test]
        async fn require_admin_guard() {
            let t = new_test_service().await;
            t.svc.bootstrap_admin("admin", "pw").await.unwrap();
            // Non-admin user.
            let hash = hash_password("pw").unwrap();
            t.store
                .create_user(meta::User {
                    username: "plain".into(),
                    password_hash: hash,
                    source: meta::SOURCE_LOCAL.into(),
                    ..Default::default()
                })
                .await
                .unwrap();

            let guard = || {
                Router::new()
                    .route("/", get(|| async { StatusCode::OK }))
                    .layer(axum::middleware::from_fn(require_admin))
                    .layer(axum::middleware::from_fn_with_state(
                        Arc::clone(&t.svc),
                        crate::auth::middleware,
                    ))
            };

            // Admin allowed.
            let resp = guard()
                .oneshot(to_request(basic_auth_parts("admin", "pw")))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "admin");

            // Non-admin forbidden.
            let resp = guard()
                .oneshot(to_request(basic_auth_parts("plain", "pw")))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "non-admin");

            // Anonymous unauthorized.
            let resp = guard().oneshot(to_request(request_parts())).await.unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "anon");
        }

        #[test]
        fn unauthorized_challenge_headers() {
            // UI API 401s must not carry a Basic challenge: browsers would pop the
            // native credential dialog and cache credentials, bypassing logout.
            let resp = unauthorized();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
            assert!(
                resp.headers().get(http::header::WWW_AUTHENTICATE).is_none(),
                "unauthorized WWW-Authenticate should be empty"
            );

            // Package-manager 401s keep the challenge so clients send credentials.
            let resp = unauthorized_basic();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                resp.headers()
                    .get(http::header::WWW_AUTHENTICATE)
                    .and_then(|v| v.to_str().ok()),
                Some("Basic realm=\"forklift\"")
            );
        }
    }

    mod authz_any {
        use std::sync::Arc;
        use std::time::Duration;

        use axum::Router;
        use axum::routing::get;
        use http::StatusCode;
        use tower::ServiceExt as _;

        use crate::auth::authz::match_glob;
        use crate::auth::*;
        use crate::meta;
        use crate::testing::auth::{
            basic_auth_parts, cookie_parts, new_test_service, request_parts, to_request,
        };

        fn perms(actions: &str) -> Vec<meta::Permission> {
            vec![meta::Permission {
                repo_pattern: "maven-*".into(),
                actions: actions.into(),
                ..Default::default()
            }]
        }

        fn scope(actions: &[&str]) -> Vec<Scope> {
            vec![Scope {
                repo_pattern: "maven-*".into(),
                actions: actions.iter().map(|a| a.to_string()).collect(),
            }]
        }

        /// The three "any" predicates gate whole surfaces (the approvals queue, the
        /// read-only admin views, the Security tab's write affordance), so what a token
        /// may carry is the whole point: approve and security are human decisions and can
        /// never come from a token scope, while audit is read-only and can. Getting this
        /// wrong hands a leaked token a management-plane action, which is why each case
        /// is pinned rather than left to the two callers to imply.
        #[test]
        fn principal_any_predicates() {
            let cases: Vec<(&str, Principal, bool, bool, bool)> = vec![
                ("no permissions", Principal::default(), false, false, false),
                (
                    "read only",
                    Principal {
                        perms: perms("read,write"),
                        ..Default::default()
                    },
                    false,
                    false,
                    false,
                ),
                (
                    "approver",
                    Principal {
                        perms: perms("read,approve"),
                        ..Default::default()
                    },
                    true,
                    false,
                    false,
                ),
                (
                    "auditor",
                    Principal {
                        perms: perms("read,audit"),
                        ..Default::default()
                    },
                    false,
                    true,
                    false,
                ),
                (
                    "security engineer",
                    Principal {
                        perms: perms("read,security"),
                        ..Default::default()
                    },
                    false,
                    false,
                    true,
                ),
                // admin in the CSV implies every action, including these three.
                (
                    "admin",
                    Principal {
                        perms: perms("admin"),
                        ..Default::default()
                    },
                    true,
                    true,
                    true,
                ),
                // An unscoped token inherits the user's full role-limited access.
                (
                    "token without scopes",
                    Principal {
                        perms: perms("approve,audit,security"),
                        via_token: true,
                        ..Default::default()
                    },
                    true,
                    true,
                    true,
                ),
                // A scoped token narrows: audit may be carried, approve and security not.
                (
                    "token scoped to audit",
                    Principal {
                        perms: perms("approve,audit,security"),
                        via_token: true,
                        token_scopes: scope(&["audit"]),
                        ..Default::default()
                    },
                    false,
                    true,
                    false,
                ),
                (
                    "token scoped to approve",
                    Principal {
                        perms: perms("approve,audit,security"),
                        via_token: true,
                        token_scopes: scope(&["approve"]),
                        ..Default::default()
                    },
                    true,
                    false,
                    false,
                ),
                (
                    "token scoped to read",
                    Principal {
                        perms: perms("approve,audit,security"),
                        via_token: true,
                        token_scopes: scope(&["read"]),
                        ..Default::default()
                    },
                    false,
                    false,
                    false,
                ),
                // A token scope alone grants nothing: the role must allow it too.
                (
                    "token scope without the role",
                    Principal {
                        perms: perms("read"),
                        via_token: true,
                        token_scopes: scope(&["approve", "audit"]),
                        ..Default::default()
                    },
                    false,
                    false,
                    false,
                ),
            ];
            for (name, p, approve, audit, security) in cases {
                assert_eq!(p.can_approve_any(), approve, "{name}: can_approve_any");
                assert_eq!(p.can_audit_any(), audit, "{name}: can_audit_any");
                assert_eq!(p.can_security_any(), security, "{name}: can_security_any");
            }
        }

        /// `match_repo_pattern` is the exported spelling the API uses to list which roles
        /// apply to a repository, and it must stay the same glob the authorization path
        /// runs; a divergence would show permissions that do not apply (or hide ones
        /// that do).
        #[test]
        fn match_repo_pattern_matches_authorization() {
            let cases: &[(&str, &str, bool)] = &[
                ("*", "maven-central", true),
                ("maven-*", "maven-central", true),
                ("maven-*", "npm-proxy", false),
                ("exact", "exact", true),
                ("maven-*", "", false),
                ("*", "", true),
            ];
            for (pattern, name, want) in cases {
                assert_eq!(
                    match_repo_pattern(pattern, name),
                    *want,
                    "match_repo_pattern({pattern:?},{name:?})"
                );
                assert_eq!(
                    match_repo_pattern(pattern, name),
                    match_glob(pattern, name),
                    "match_repo_pattern({pattern:?},{name:?}) disagrees with match_glob"
                );
            }
        }

        /// Creates an enabled local user holding one role with one permission, which is
        /// the shape every middleware case below needs.
        async fn mk_user_with_role(
            store: &Arc<meta::Store>,
            username: &str,
            role_name: &str,
            actions: &str,
        ) {
            let hash = hash_password("pw123456").expect("hash");
            let u = store
                .create_user(meta::User {
                    username: username.into(),
                    password_hash: hash,
                    source: meta::SOURCE_LOCAL.into(),
                    ..Default::default()
                })
                .await
                .unwrap_or_else(|e| panic!("create user {username}: {e}"));
            let role = store
                .create_role(meta::Role {
                    name: role_name.into(),
                    ..Default::default()
                })
                .await
                .unwrap_or_else(|e| panic!("create role {role_name}: {e}"));
            store
                .add_permission(meta::Permission {
                    role_id: role.id,
                    repo_pattern: "*".into(),
                    actions: actions.into(),
                    ..Default::default()
                })
                .await
                .expect("add permission");
            store.assign_role(u.id, role.id).await.expect("assign role");
        }

        /// Each management-plane middleware admits a different set, and the difference is
        /// the security boundary between a security engineer, an auditor and an
        /// administrator. Anonymous must be 401 (a challenge the caller can answer) and a
        /// signed-in principal without the action 403 (an answer that will not change).
        #[tokio::test]
        async fn management_plane_middlewares() {
            let t = new_test_service().await;
            t.svc
                .bootstrap_admin("admin", "pw123456")
                .await
                .expect("bootstrap");
            mk_user_with_role(&t.store, "plain", "reader-role", "read").await;
            mk_user_with_role(&t.store, "approver", "approver-role", "read,approve").await;
            mk_user_with_role(&t.store, "auditor", "auditor-role", "read,audit").await;

            let guard = |which: &str| {
                let inner = Router::new().route("/", get(|| async { StatusCode::OK }));
                let inner = match which {
                    "approver" => inner.layer(axum::middleware::from_fn(require_approver)),
                    "approverOrAuditor" => {
                        inner.layer(axum::middleware::from_fn(require_approver_or_auditor))
                    }
                    "auditor" => inner.layer(axum::middleware::from_fn(require_auditor)),
                    other => panic!("unknown guard {other}"),
                };
                inner.layer(axum::middleware::from_fn_with_state(
                    Arc::clone(&t.svc),
                    crate::auth::middleware,
                ))
            };

            let cases: &[(&str, &str, StatusCode)] = &[
                ("approver", "", StatusCode::UNAUTHORIZED),
                ("approver", "plain", StatusCode::FORBIDDEN),
                ("approver", "auditor", StatusCode::FORBIDDEN),
                ("approver", "approver", StatusCode::OK),
                ("approver", "admin", StatusCode::OK),
                ("approverOrAuditor", "", StatusCode::UNAUTHORIZED),
                ("approverOrAuditor", "plain", StatusCode::FORBIDDEN),
                ("approverOrAuditor", "approver", StatusCode::OK),
                ("approverOrAuditor", "auditor", StatusCode::OK),
                ("approverOrAuditor", "admin", StatusCode::OK),
                ("auditor", "", StatusCode::UNAUTHORIZED),
                ("auditor", "plain", StatusCode::FORBIDDEN),
                ("auditor", "approver", StatusCode::FORBIDDEN),
                ("auditor", "auditor", StatusCode::OK),
                ("auditor", "admin", StatusCode::OK),
            ];
            for (which, user, want) in cases {
                let parts = if user.is_empty() {
                    request_parts()
                } else {
                    basic_auth_parts(user, "pw123456")
                };
                let resp = guard(which).oneshot(to_request(parts)).await.unwrap();
                let who = if user.is_empty() { "anonymous" } else { user };
                assert_eq!(resp.status(), *want, "{which} guard, {who}");
            }
        }

        /// An impersonated session must act as the target and never widen access: the
        /// resolved principal carries the target's own permissions with the
        /// administrator recorded only for attribution. It is also short-lived, so the
        /// TTL clamp is pinned here rather than trusted to the caller.
        #[tokio::test]
        async fn impersonation_resolves_as_target() {
            let t = new_test_service().await;
            t.svc
                .bootstrap_admin("admin", "pw123456")
                .await
                .expect("bootstrap");
            mk_user_with_role(&t.store, "plain", "reader-role", "read").await;

            let cookie = t
                .svc
                .issue_impersonation("plain", meta::SOURCE_LOCAL, "admin")
                .expect("issue impersonation");
            let parts = cookie_parts("forklift_session", &cookie);
            let p = t.svc.resolve(&parts).await.expect("resolve");
            let p = p.expect("principal, want the impersonated user");
            assert_eq!(p.username, "plain", "principal, want the impersonated user");
            assert_eq!(p.impersonator, "admin", "impersonator");
            // The target's own access, not the administrator's.
            assert!(
                !p.is_admin(),
                "impersonating a non-admin produced an admin principal"
            );
            assert!(
                p.can("maven-central", ACTION_READ),
                "impersonated principal lost the target's own read access"
            );

            // The codec clamps a longer-than-session TTL, so an impersonation cannot
            // outlive an ordinary session by asking for more.
            let codec = SessionCodec::new(
                b"test-secret-test-secret-test-secret".to_vec(),
                Duration::from_secs(60),
            );
            let value = codec
                .encode_impersonated(
                    "plain",
                    meta::SOURCE_LOCAL,
                    "admin",
                    Duration::from_secs(24 * 3600),
                )
                .expect("encode impersonated");
            let data = codec.decode(&value).expect("decode");
            assert_eq!(data.impersonator, "admin");
            assert_eq!(data.username, "plain");
            let until = data.expires - chrono::Utc::now().timestamp();
            assert!(
                until <= 61,
                "expiry {until}s exceeds the codec's session lifetime"
            );
            // Groups are deliberately not carried into an impersonated session: the
            // administrator cannot know the target's live OIDC claims.
            assert!(
                data.groups.is_empty(),
                "groups = {:?}, want none",
                data.groups
            );
        }

        /// `approvers_for` is what the notifier addresses an approval request to, so it
        /// must follow the same `can` check the API enforces: disabled accounts are out,
        /// and a user whose role does not cover the repository is not notified about it.
        #[tokio::test]
        async fn approvers_for() {
            let t = new_test_service().await;

            mk_user_with_role(&t.store, "reader", "reader-role", "read").await;
            mk_user_with_role(&t.store, "approver", "approver-role", "read,approve").await;
            mk_user_with_role(&t.store, "gone", "gone-role", "read,approve").await;
            // A scoped approver on maven only, to prove the repository argument is used.
            let hash = hash_password("pw123456").unwrap();
            let scoped = t
                .store
                .create_user(meta::User {
                    username: "maven-approver".into(),
                    password_hash: hash,
                    source: meta::SOURCE_LOCAL.into(),
                    ..Default::default()
                })
                .await
                .unwrap();
            let role = t
                .store
                .create_role(meta::Role {
                    name: "maven-approver-role".into(),
                    ..Default::default()
                })
                .await
                .unwrap();
            t.store
                .add_permission(meta::Permission {
                    role_id: role.id,
                    repo_pattern: "maven-*".into(),
                    actions: "approve".into(),
                    ..Default::default()
                })
                .await
                .unwrap();
            t.store.assign_role(scoped.id, role.id).await.unwrap();
            // Disabling an account removes it from the notification list.
            let gone = t
                .store
                .get_user_by_username("gone")
                .await
                .expect("get user gone");
            t.store
                .set_user_disabled(gone.id, true)
                .await
                .expect("disable user");

            let got = t
                .svc
                .approvers_for("npm-proxy")
                .await
                .expect("approvers for npm-proxy");
            assert_eq!(got, vec!["approver".to_string()], "npm-proxy approvers");
            let got = t
                .svc
                .approvers_for("maven-central")
                .await
                .expect("approvers for maven-central");
            assert_eq!(
                got,
                vec!["approver".to_string(), "maven-approver".to_string()],
                "maven-central approvers"
            );
        }
    }
}
