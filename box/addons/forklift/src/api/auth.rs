use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Request, State};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use http::StatusCode;
use http::request::Parts;
use serde::{Deserialize, Serialize};

use crate::audit;
use crate::auth::{self, Scope};
use crate::meta::{self, Permission, Role, Token, User};

use super::path_id;
use super::{Handler, NAME_RULE_MSG, map_error, valid_name, write_error, write_json};

// --- session ---

#[derive(Debug, Clone, Default, Deserialize)]
struct LoginReq {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

pub(super) async fn login(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let Some(authz) = h.authz.clone() else {
        return write_error(StatusCode::NOT_FOUND, "auth disabled");
    };
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let Ok(req) = serde_json::from_slice::<LoginReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let u = match authz.authenticate_local(&req.username, &req.password).await {
        Ok(u) => u,
        Err(auth::Error::AccountLocked) => {
            return write_error(
                StatusCode::FORBIDDEN,
                "account locked: too many failed attempts, contact an administrator",
            );
        }
        Err(_) => return write_error(StatusCode::UNAUTHORIZED, "invalid credentials"),
    };
    // Best-effort: a bookkeeping failure must not block the login.
    if let Err(err) = h.store.touch_last_login(u.id).await {
        tracing::warn!(user = %u.username, err = %err, "record last login");
    }
    let value = match authz.issue_session(&u.username, &u.source, &[]) {
        Ok(value) => value,
        Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    let mut response = write_json(
        StatusCode::OK,
        SessionIdentityDTO {
            username: u.username,
            source: u.source,
        },
    );
    auth::set_session_cookie(&mut response, &value, is_secure(&parts));
    response
}

pub(super) async fn logout() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    auth::clear_session_cookie(&mut response);
    response
}

pub(super) async fn me(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let Some(p) = auth::from_request_parts(&parts) else {
        return write_json(StatusCode::OK, serde_json::json!({"authenticated": false}));
    };
    let csrf_token = h
        .authz
        .as_ref()
        .and_then(|authz| authz.csrf_token(&parts))
        .unwrap_or_default();
    let mut out: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
    out.insert("authenticated", true.into());
    out.insert("username", p.username.clone().into());
    out.insert("source", p.source.clone().into());
    out.insert("admin", p.is_admin().into());
    out.insert("approver", (p.is_admin() || p.can_approve_any()).into());
    out.insert("auditor", (p.is_admin() || p.can_audit_any()).into());
    out.insert("security", (p.is_admin() || p.can_security_any()).into());
    out.insert("csrf_token", csrf_token.into());
    // Present only while impersonating, so the UI can raise its banner and offer
    // the way back to the administrator's own account.
    if !p.impersonator.is_empty() {
        out.insert("impersonator", p.impersonator.clone().into());
    }
    write_json(StatusCode::OK, out)
}

// --- impersonation ---

/// The shortest accepted justification. Impersonation takes over another
/// identity, so the operator must state why in a way that is still meaningful
/// when read back out of the logs months later.
const MIN_IMPERSONATE_REASON: usize = 10;

/// Caps the justification so a log line stays readable.
const MAX_IMPERSONATE_REASON: usize = 500;

#[derive(Debug, Clone, Default, Deserialize)]
struct ImpersonateReq {
    #[serde(default)]
    reason: String,
}

/// Swaps the caller's session for one that acts as the target user. Admin-only
/// (enforced by the route group) and session-only: a personal access token or
/// Basic credentials cannot start one, because the result is a cookie the API
/// client would never use and the audit trail would be weaker.
///
/// The issued session carries the target's own permissions, so impersonating
/// cannot grant the administrator anything the target does not already have; it
/// is a support tool for reproducing what a user sees.
pub(super) async fn impersonate_user(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let Some(authz) = h.authz.clone() else {
        return write_error(StatusCode::NOT_FOUND, "auth disabled");
    };
    let (parts, body) = request.into_parts();
    let Some(p) = auth::from_request_parts(&parts) else {
        return auth::unauthorized();
    };
    // A valid CSRF claim exists only inside a signed session cookie, so this
    // doubles as the "authenticated by session" test.
    if authz.csrf_token(&parts).is_none() {
        return write_error(
            StatusCode::FORBIDDEN,
            "impersonation requires an interactive session; sign in to the web console first",
        );
    }
    // No chaining: an administrator impersonating another administrator must
    // return to their own account before starting a new impersonation, so every
    // session has exactly one accountable operator behind it.
    if !p.impersonator.is_empty() {
        return write_error(
            StatusCode::CONFLICT,
            "already impersonating: stop the current impersonation first",
        );
    }
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let Ok(req) = serde_json::from_slice::<ImpersonateReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let reason = req.reason.trim();
    let reason_len = reason.chars().count();
    if reason_len < MIN_IMPERSONATE_REASON {
        return write_error(
            StatusCode::BAD_REQUEST,
            &format!(
                "reason required: describe why you are impersonating this user (at least {MIN_IMPERSONATE_REASON} characters)"
            ),
        );
    }
    if reason_len > MAX_IMPERSONATE_REASON {
        return write_error(
            StatusCode::BAD_REQUEST,
            &format!("reason too long: at most {MAX_IMPERSONATE_REASON} characters"),
        );
    }
    let target = match h.store.get_user(id).await {
        Ok(target) => target,
        Err(err) => return map_error(err),
    };
    if target.username == p.username {
        return write_error(
            StatusCode::BAD_REQUEST,
            "cannot impersonate your own account",
        );
    }
    if target.robot {
        return write_error(
            StatusCode::BAD_REQUEST,
            "a robot account cannot be impersonated: it has no interactive session",
        );
    }
    if target.disabled {
        return write_error(
            StatusCode::BAD_REQUEST,
            "a disabled account cannot be impersonated",
        );
    }
    let value = match authz.issue_impersonation(&target.username, &target.source, &p.username) {
        Ok(value) => value,
        Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    // Logged at warn so it stands out in the operational log: this is the record
    // of who acted as whom, and why.
    tracing::warn!(
        admin = %p.username, target = %target.username, reason = %reason,
        client_ip = %audit::client_ip_parts(&parts),
        expires_in = %format_std_duration(auth::IMPERSONATION_TTL),
        "impersonation started"
    );
    let mut response = write_json(
        StatusCode::OK,
        ImpersonationStartDTO {
            username: target.username,
            source: target.source,
            impersonator: p.username.clone(),
            expires_in: format_std_duration(auth::IMPERSONATION_TTL),
        },
    );
    auth::set_session_cookie(&mut response, &value, is_secure(&parts));
    response
}

/// Returns the caller to their own account. Open to any authenticated principal,
/// because the impersonated session usually holds no admin rights and must still
/// be able to hand itself back.
pub(super) async fn stop_impersonation(
    State(h): State<Arc<Handler>>,
    request: Request,
) -> Response {
    let Some(authz) = h.authz.clone() else {
        return write_error(StatusCode::NOT_FOUND, "auth disabled");
    };
    let (parts, _) = request.into_parts();
    let Some(p) = auth::from_request_parts(&parts) else {
        return auth::unauthorized();
    };
    if p.impersonator.is_empty() {
        return write_error(StatusCode::BAD_REQUEST, "not impersonating");
    }
    // Re-check the administrator's account: it may have been disabled, deleted or
    // demoted while the impersonated session was open. Anything unusable ends in
    // a cleared cookie rather than a restored session.
    let u = match h.store.get_user_by_username(&p.impersonator).await {
        Ok(u) if !u.disabled && !u.robot => u,
        _ => {
            tracing::warn!(
                admin = %p.impersonator, target = %p.username,
                client_ip = %audit::client_ip_parts(&parts),
                "impersonation stopped but original account is unusable"
            );
            let mut response = write_error(
                StatusCode::UNAUTHORIZED,
                "your original account is no longer available; sign in again",
            );
            auth::clear_session_cookie(&mut response);
            return response;
        }
    };
    let value = match authz.issue_session(&u.username, &u.source, &[]) {
        Ok(value) => value,
        Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    tracing::warn!(
        admin = %u.username, target = %p.username,
        client_ip = %audit::client_ip_parts(&parts),
        "impersonation stopped"
    );
    let mut response = write_json(
        StatusCode::OK,
        SessionIdentityDTO {
            username: u.username,
            source: u.source,
        },
    );
    auth::set_session_cookie(&mut response, &value, is_secure(&parts));
    response
}

// --- tokens (PAT, self-service) ---

#[derive(Debug, Clone, Default, Deserialize)]
struct CreateTokenReq {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    scopes: Vec<Scope>,
    /// e.g. "720h", required, at most one year.
    #[serde(default)]
    expires_in: String,
}

/// Caps personal access token lifetime at one year.
const MAX_TOKEN_TTL: i64 = 365 * 24 * 60 * 60;

/// Caps how many access tokens one user (including a robot account) may hold at
/// once. Enforced on every issue path; revoke one to free a slot.
pub(super) const MAX_TOKENS_PER_USER: i64 = 3;

pub(super) async fn list_tokens(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let u = match h.current_user(&parts).await {
        Ok(u) => u,
        Err(response) => return *response,
    };
    match h.store.list_tokens(u.id).await {
        Ok(tokens) => write_json(
            StatusCode::OK,
            tokens.into_iter().map(token_summary).collect::<Vec<_>>(),
        ),
        Err(err) => map_error(err),
    }
}

pub(super) async fn create_token(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let u = match h.current_user(&parts).await {
        Ok(u) => u,
        Err(response) => return *response,
    };
    issue_token(&h, body, u.id).await
}

/// Checks a create-token request and returns the parsed expiry and serialized
/// scopes. An `Err` message means the request is invalid (the caller responds
/// 400 with it).
fn validate_token_req(req: &CreateTokenReq) -> Result<(DateTime<Utc>, String), String> {
    if !valid_name(req.name.trim()) {
        return Err(format!("invalid token name: {NAME_RULE_MSG}"));
    }
    if req.description.trim().is_empty() {
        return Err("token description required".to_string());
    }
    if let Some(msg) = validate_scopes(&req.scopes) {
        return Err(msg);
    }
    if req.expires_in.is_empty() {
        return Err("expires_in required".to_string());
    }
    let Some(seconds) = parse_duration_secs(&req.expires_in) else {
        return Err("invalid expires_in".to_string());
    };
    if seconds <= 0 {
        return Err("invalid expires_in".to_string());
    }
    if seconds > MAX_TOKEN_TTL {
        return Err("expires_in exceeds the one year maximum".to_string());
    }
    let scopes_json = serde_json::to_string(&req.scopes).unwrap_or_default();
    Ok((Utc::now() + ChronoDuration::seconds(seconds), scopes_json))
}

/// Checks a token scope list. `Some(msg)` means it is invalid (the caller
/// responds 400 with it).
pub(super) fn validate_scopes(scopes: &[Scope]) -> Option<String> {
    if scopes.is_empty() {
        return Some("at least one scope required".to_string());
    }
    for s in scopes {
        if s.repo_pattern.trim().is_empty() {
            return Some("scope repo_pattern required".to_string());
        }
        if s.actions.is_empty() {
            return Some("scope actions required".to_string());
        }
        for a in &s.actions {
            // Data-plane actions plus audit, the one management-plane action a
            // token may carry: audit is read-only viewing of the admin surfaces
            // (approval queue, audit logs, users, roles), so a leaked token still
            // cannot decide approvals, edit policy, or manage accounts. approve,
            // security and admin stay session-only by design.
            if ![
                auth::ACTION_READ,
                auth::ACTION_WRITE,
                auth::ACTION_DELETE,
                auth::ACTION_AUDIT,
            ]
            .contains(&a.as_str())
            {
                return Some(format!("invalid scope action: {a}"));
            }
        }
    }
    None
}

/// Validates the request body and creates a personal access token owned by
/// `user_id`, returning the plaintext exactly once. Shared by the self-service
/// create (current user) and the admin create (a target user). Token scopes only
/// ever narrow the owner's role permissions (enforced at auth time via
/// `Principal::can`), so an admin issuing a token cannot escalate the target
/// user's effective access.
async fn issue_token(h: &Arc<Handler>, body: axum::body::Body, user_id: i64) -> Response {
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let Ok(req) = serde_json::from_slice::<CreateTokenReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let (expires_at, scopes_json) = match validate_token_req(&req) {
        Ok(parsed) => parsed,
        Err(msg) => return write_error(StatusCode::BAD_REQUEST, &msg),
    };
    // Cap the number of tokens per user. Guards both the self-service and the
    // admin-for-user issue paths, since both funnel through here.
    let count = match h.store.count_tokens(user_id).await {
        Ok(count) => count,
        Err(err) => return map_error(err),
    };
    if count >= MAX_TOKENS_PER_USER {
        return write_error(
            StatusCode::CONFLICT,
            &format!(
                "token limit reached: a user may hold at most {MAX_TOKENS_PER_USER} access tokens; revoke one first"
            ),
        );
    }
    let (plaintext, hash) = match auth::generate_token() {
        Ok(pair) => pair,
        Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    let t = match h
        .store
        .create_token(Token {
            user_id,
            name: req.name.clone(),
            description: req.description.clone(),
            hash,
            scopes_json,
            expires_at: Some(expires_at),
            ..Default::default()
        })
        .await
    {
        Ok(t) => t,
        Err(err) => return map_error(err),
    };
    // The plaintext is returned exactly once.
    write_json(
        StatusCode::CREATED,
        IssuedTokenDTO {
            id: t.id,
            name: t.name,
            description: t.description,
            token: plaintext,
            expires_at: t.expires_at,
        },
    )
}

/// Who the session now belongs to, returned by the two operations that change
/// that: logging in, and stepping back out of an impersonated session.
#[derive(Debug, Clone, Serialize)]
struct SessionIdentityDTO {
    username: String,
    source: String,
}

/// [`SessionIdentityDTO`] plus who is behind the session and how long it lasts.
/// Impersonation is time-boxed, so the caller is told when it lapses rather than
/// having to discover it.
#[derive(Debug, Clone, Serialize)]
struct ImpersonationStartDTO {
    username: String,
    source: String,
    impersonator: String,
    expires_in: String,
}

/// The slim acknowledgement `POST /users` answers with. The full user is a
/// separate GET; creation only reports what the caller needs to address the new
/// row.
#[derive(Debug, Clone, Serialize)]
struct CreatedUserDTO {
    id: i64,
    username: String,
}

/// A freshly created token. The only response carrying the plaintext secret,
/// which the server cannot show again.
#[derive(Debug, Clone, Serialize)]
struct IssuedTokenDTO {
    id: i64,
    name: String,
    description: String,
    token: String,
    /// Null when the token does not expire.
    expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct UpdateTokenReq {
    #[serde(default)]
    scopes: Vec<Scope>,
}

/// Decodes and validates an update-token body and replaces the scopes of the
/// (`user_id`, `token_id`) token. Scopes still only narrow the owner's role
/// permissions (enforced at auth time via `Principal::can`), so widening a
/// token's scope list never escalates beyond the owner's roles.
async fn update_token_scopes(
    h: &Arc<Handler>,
    body: axum::body::Body,
    user_id: i64,
    token_id: i64,
) -> Response {
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let Ok(req) = serde_json::from_slice::<UpdateTokenReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    if let Some(msg) = validate_scopes(&req.scopes) {
        return write_error(StatusCode::BAD_REQUEST, &msg);
    }
    let encoded = serde_json::to_string(&req.scopes).unwrap_or_default();
    match h
        .store
        .update_token_scopes(user_id, token_id, &encoded)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => map_error(err),
    }
}

pub(super) async fn update_token(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let u = match h.current_user(&parts).await {
        Ok(u) => u,
        Err(response) => return *response,
    };
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    update_token_scopes(&h, body, u.id, id).await
}

pub(super) async fn delete_token(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let u = match h.current_user(&parts).await {
        Ok(u) => u,
        Err(response) => return *response,
    };
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    match h.store.delete_token(u.id, id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => map_error(err),
    }
}

// --- user tokens (admin/auditor) ---
//
// The per-user token endpoints let an administrator manage another user's
// personal access tokens from that user's detail page; an auditor may list them
// read-only. They reuse the user-id-keyed store methods, so a token is always
// scoped to (and deletable only within) the target user.

/// The list shape for a token, hash and plaintext excluded.
pub(super) fn token_summary(t: Token) -> BTreeMap<&'static str, serde_json::Value> {
    let mut out: BTreeMap<&'static str, serde_json::Value> = BTreeMap::new();
    out.insert("id", t.id.into());
    out.insert("name", t.name.into());
    out.insert("description", t.description.into());
    out.insert("scopes_json", t.scopes_json.into());
    out.insert(
        "expires_at",
        serde_json::to_value(t.expires_at).unwrap_or(serde_json::Value::Null),
    );
    out.insert(
        "last_used_at",
        serde_json::to_value(t.last_used_at).unwrap_or(serde_json::Value::Null),
    );
    out.insert(
        "created_at",
        serde_json::to_value(t.created_at).unwrap_or(serde_json::Value::Null),
    );
    out
}

pub(super) async fn list_user_tokens(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    if let Err(err) = h.store.get_user(id).await {
        return map_error(err);
    }
    match h.store.list_tokens(id).await {
        Ok(tokens) => write_json(
            StatusCode::OK,
            tokens.into_iter().map(token_summary).collect::<Vec<_>>(),
        ),
        Err(err) => map_error(err),
    }
}

pub(super) async fn create_user_token(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (_, body) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    if let Err(err) = h.store.get_user(id).await {
        return map_error(err);
    }
    issue_token(&h, body, id).await
}

pub(super) async fn update_user_token(
    State(h): State<Arc<Handler>>,
    UrlPath((id, token_id)): UrlPath<(String, String)>,
    request: Request,
) -> Response {
    let (_, body) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let Ok(token_id) = token_id.parse::<i64>() else {
        return write_error(StatusCode::BAD_REQUEST, "invalid token id");
    };
    update_token_scopes(&h, body, id, token_id).await
}

pub(super) async fn delete_user_token(
    State(h): State<Arc<Handler>>,
    UrlPath((id, token_id)): UrlPath<(String, String)>,
) -> Response {
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let Ok(token_id) = token_id.parse::<i64>() else {
        return write_error(StatusCode::BAD_REQUEST, "invalid token id");
    };
    match h.store.delete_token(id, token_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => map_error(err),
    }
}

// --- users (admin) ---

#[derive(Debug, Clone, Default, Deserialize)]
struct CreateUserReq {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    role_ids: Vec<i64>,
    /// Creates a token-only service account: no password is required and
    /// interactive login is refused, but the account can own personal access
    /// tokens for automation. Ignored (treated as false) for the password path.
    #[serde(default)]
    robot: bool,
}

#[derive(Debug, Clone, Serialize)]
struct RoleRefDTO {
    id: i64,
    name: String,
}

#[derive(Debug, Clone, Serialize)]
struct UserDTO {
    id: i64,
    username: String,
    source: String,
    email: String,
    disabled: bool,
    /// Marks a token-only service account (no interactive login).
    robot: bool,
    created_at: DateTime<Utc>,
    /// Null when the user has never logged in.
    last_login_at: Option<DateTime<Utc>>,
    roles: Vec<RoleRefDTO>,
    // Account lockout fields.
    lockout_enabled: bool,
    locked: bool,
    /// The consecutive failed-password count that feeds the lockout threshold;
    /// reset to 0 on success or unlock.
    failed_login_count: i64,
    /// True for the bootstrap admin, which cannot be locked out and whose
    /// lockout toggle is disabled in the UI.
    protected: bool,
    /// How many personal access tokens the user owns, so list views can show it
    /// without fetching each user's tokens.
    token_count: i64,
}

impl Handler {
    fn to_user_dto(&self, u: User, roles: &[Role], token_count: i64) -> UserDTO {
        let locked = u.locked();
        let protected = self
            .authz
            .as_ref()
            .is_some_and(|authz| authz.is_protected_admin(&u.username));
        UserDTO {
            id: u.id,
            username: u.username,
            source: u.source,
            email: u.email,
            disabled: u.disabled,
            robot: u.robot,
            created_at: u.created_at,
            last_login_at: u.last_login_at,
            roles: roles
                .iter()
                .map(|r| RoleRefDTO {
                    id: r.id,
                    name: r.name.clone(),
                })
                .collect(),
            lockout_enabled: u.lockout_enabled,
            locked,
            failed_login_count: u.failed_login_count,
            protected,
            token_count,
        }
    }
}

pub(super) async fn list_users(State(h): State<Arc<Handler>>) -> Response {
    let users = match h.store.list_users().await {
        Ok(users) => users,
        Err(err) => return map_error(err),
    };
    let roles_by = match h.store.roles_by_user().await {
        Ok(roles_by) => roles_by,
        Err(err) => return map_error(err),
    };
    let tokens_by = match h.store.token_counts_by_user().await {
        Ok(tokens_by) => tokens_by,
        Err(err) => return map_error(err),
    };
    let empty: Vec<Role> = Vec::new();
    let out: Vec<UserDTO> = users
        .into_iter()
        .map(|u| {
            let roles = roles_by.get(&u.id).unwrap_or(&empty).clone();
            let tokens = tokens_by.get(&u.id).copied().unwrap_or_default();
            h.to_user_dto(u, &roles, tokens)
        })
        .collect();
    write_json(StatusCode::OK, out)
}

pub(super) async fn create_user(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (_, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let Ok(req) = serde_json::from_slice::<CreateUserReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    // A robot account is token-only, so it carries no password; a normal user
    // requires one. A robot with a password supplied is a client error, not a
    // silent drop, so the intent is never ambiguous.
    if req.robot {
        if !req.password.is_empty() {
            return write_error(
                StatusCode::BAD_REQUEST,
                "a robot account cannot have a password",
            );
        }
    } else if req.password.is_empty() {
        return write_error(StatusCode::BAD_REQUEST, "username and password required");
    }
    if !valid_name(req.username.trim()) {
        return write_error(
            StatusCode::BAD_REQUEST,
            &format!("invalid username: {NAME_RULE_MSG}"),
        );
    }
    // Validate any requested roles before creating the user so a bad role id
    // fails cleanly instead of leaving a roleless user behind.
    if !req.role_ids.is_empty() {
        let roles = match h.store.list_roles().await {
            Ok(roles) => roles,
            Err(err) => return map_error(err),
        };
        let valid: std::collections::HashSet<i64> = roles.iter().map(|role| role.id).collect();
        if req.role_ids.iter().any(|rid| !valid.contains(rid)) {
            return write_error(StatusCode::BAD_REQUEST, "unknown role id");
        }
    }
    // A robot has no password hash to store; a normal user's password is hashed.
    let mut hash = String::new();
    if !req.robot {
        match auth::hash_password(&req.password) {
            Ok(hashed) => hash = hashed,
            Err(err) => {
                return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
            }
        }
    }
    let u = match h
        .store
        .create_user(User {
            username: req.username.clone(),
            password_hash: hash,
            source: meta::SOURCE_LOCAL.to_string(),
            email: req.email.clone(),
            robot: req.robot,
            ..Default::default()
        })
        .await
    {
        Ok(u) => u,
        Err(err) => {
            if err.to_string().contains("UNIQUE") || matches!(err, meta::Error::Conflict) {
                return write_error(StatusCode::CONFLICT, "username already exists");
            }
            return map_error(err);
        }
    };
    // New local accounts get failed-password lockout on by default; an admin can
    // turn it off from the user's detail page. Robots never log in with a
    // password (nothing to lock out) and the protected admin is exempt.
    let protected = h
        .authz
        .as_ref()
        .is_some_and(|authz| authz.is_protected_admin(&u.username));
    if !req.robot
        && !protected
        && let Err(err) = h.store.set_lockout_enabled(u.id, true).await
    {
        return map_error(err);
    }
    for rid in req.role_ids {
        if let Err(err) = h.store.assign_role(u.id, rid).await {
            return map_error(err);
        }
    }
    write_json(
        StatusCode::CREATED,
        CreatedUserDTO {
            id: u.id,
            username: u.username,
        },
    )
}

/// Admin edits; omitted fields are left unchanged.
#[derive(Debug, Clone, Default, Deserialize)]
struct UpdateUserReq {
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    disabled: Option<bool>,
    #[serde(default)]
    lockout_enabled: Option<bool>,
    #[serde(default)]
    unlock: Option<bool>,
}

pub(super) async fn update_user(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let Ok(req) = serde_json::from_slice::<UpdateUserReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let target = match h.store.get_user(id).await {
        Ok(target) => target,
        Err(err) => return map_error(err),
    };
    let protected = h
        .authz
        .as_ref()
        .is_some_and(|authz| authz.is_protected_admin(&target.username));

    if let Some(disabled) = req.disabled {
        if disabled && protected {
            return write_error(
                StatusCode::BAD_REQUEST,
                "cannot disable the default admin account",
            );
        }
        if disabled && h.is_self(&parts, id).await {
            return write_error(StatusCode::BAD_REQUEST, "cannot disable your own account");
        }
        if let Err(err) = h.store.set_user_disabled(id, disabled).await {
            return map_error(err);
        }
    }
    if let Some(password) = &req.password {
        if target.robot {
            return write_error(
                StatusCode::BAD_REQUEST,
                "a robot account cannot have a password",
            );
        }
        if target.source != meta::SOURCE_LOCAL {
            return write_error(
                StatusCode::BAD_REQUEST,
                "cannot set a password for an OIDC user",
            );
        }
        if password.is_empty() {
            return write_error(StatusCode::BAD_REQUEST, "password must not be empty");
        }
        let hash = match auth::hash_password(password) {
            Ok(hash) => hash,
            Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
        };
        if let Err(err) = h.store.set_password(id, &hash).await {
            return map_error(err);
        }
    }
    if let Some(lockout_enabled) = req.lockout_enabled {
        if lockout_enabled && (target.robot || target.source != meta::SOURCE_LOCAL) {
            return write_error(
                StatusCode::BAD_REQUEST,
                "lockout applies only to local-password accounts",
            );
        }
        if lockout_enabled && protected {
            return write_error(
                StatusCode::BAD_REQUEST,
                "cannot enable lockout for the default admin account",
            );
        }
        if let Err(err) = h.store.set_lockout_enabled(id, lockout_enabled).await {
            return map_error(err);
        }
    }
    if req.unlock == Some(true)
        && let Err(err) = h.store.reset_failed_login(id).await
    {
        return map_error(err);
    }

    let target = match h.store.get_user(id).await {
        Ok(target) => target,
        Err(err) => return map_error(err),
    };
    let roles_by = match h.store.roles_by_user().await {
        Ok(roles_by) => roles_by,
        Err(err) => return map_error(err),
    };
    let token_count = match h.store.count_tokens(id).await {
        Ok(count) => count,
        Err(err) => return map_error(err),
    };
    let roles = roles_by.get(&id).cloned().unwrap_or_default();
    write_json(StatusCode::OK, h.to_user_dto(target, &roles, token_count))
}

pub(super) async fn delete_user(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    if h.is_self(&parts, id).await {
        return write_error(StatusCode::BAD_REQUEST, "cannot delete your own account");
    }
    match h.store.delete_user(id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => map_error(err),
    }
}

impl Handler {
    /// Reports whether the request principal is the user with the given ID.
    async fn is_self(&self, parts: &Parts, id: i64) -> bool {
        let Some(p) = auth::from_request_parts(parts) else {
            return false;
        };
        self.store
            .get_user_by_username(&p.username)
            .await
            .is_ok_and(|u| u.id == id)
    }

    /// Loads the [`User`] for the request principal.
    async fn current_user(&self, parts: &Parts) -> Result<User, Box<Response>> {
        let Some(p) = auth::from_request_parts(parts) else {
            return Err(Box::new(auth::unauthorized()));
        };
        self.store
            .get_user_by_username(&p.username)
            .await
            .map_err(|_| Box::new(write_error(StatusCode::UNAUTHORIZED, "unknown user")))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AssignRoleReq {
    #[serde(default)]
    role_id: i64,
}

pub(super) async fn assign_role(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (_, body) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let Ok(req) = serde_json::from_slice::<AssignRoleReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    match h.store.assign_role(id, req.role_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => map_error(err),
    }
}

pub(super) async fn remove_role(
    State(h): State<Arc<Handler>>,
    UrlPath((id, role_id)): UrlPath<(String, String)>,
) -> Response {
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let Ok(role_id) = role_id.parse::<i64>() else {
        return write_error(StatusCode::BAD_REQUEST, "invalid role id");
    };
    match h.store.is_managed_user_role(id, role_id).await {
        Ok(true) => return write_error(StatusCode::CONFLICT, MANAGED_ROLE_MSG),
        Ok(false) => {}
        Err(err) => return map_error(err),
    }
    match h.store.remove_role(id, role_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => map_error(err),
    }
}

// --- roles & permissions (admin) ---

#[derive(Debug, Clone, Default, Deserialize)]
struct CreateRoleReq {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    permissions: Vec<AddPermissionReq>,
}

/// Reports whether `a` is a grantable permission action.
fn valid_role_action(a: &str) -> bool {
    [
        auth::ACTION_READ,
        auth::ACTION_WRITE,
        auth::ACTION_DELETE,
        auth::ACTION_APPROVE,
        auth::ACTION_AUDIT,
        auth::ACTION_SECURITY,
        auth::ACTION_ADMIN,
    ]
    .contains(&a)
}

/// Returned when a client tries to mutate an entity owned by the declarative
/// RBAC policy. Such entities are reconciled from the chart and are read-only
/// via the API; change them in the chart values instead.
const MANAGED_ROLE_MSG: &str = "managed by declarative RBAC policy; edit the chart values instead";

#[derive(Debug, Clone, Serialize)]
struct PermissionDTO {
    id: i64,
    repo_pattern: String,
    actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RoleDTO {
    id: i64,
    name: String,
    description: String,
    created_at: DateTime<Utc>,
    managed: bool,
    permissions: Vec<PermissionDTO>,
    /// How many users are assigned this role.
    user_count: i64,
}

fn to_role_dto(r: Role, perms: &[Permission], user_count: i64) -> RoleDTO {
    RoleDTO {
        id: r.id,
        name: r.name,
        description: r.description,
        created_at: r.created_at,
        managed: r.managed,
        permissions: perms
            .iter()
            .map(|p| PermissionDTO {
                id: p.id,
                repo_pattern: p.repo_pattern.clone(),
                actions: p.actions.split(',').map(str::to_string).collect(),
            })
            .collect(),
        user_count,
    }
}

pub(super) async fn list_roles(State(h): State<Arc<Handler>>) -> Response {
    let roles = match h.store.list_roles().await {
        Ok(roles) => roles,
        Err(err) => return map_error(err),
    };
    let perms = match h.store.list_permissions().await {
        Ok(perms) => perms,
        Err(err) => return map_error(err),
    };
    let mut by_role: HashMap<i64, Vec<Permission>> = HashMap::new();
    for p in perms {
        by_role.entry(p.role_id).or_default().push(p);
    }
    let roles_by = match h.store.roles_by_user().await {
        Ok(roles_by) => roles_by,
        Err(err) => return map_error(err),
    };
    let mut user_count: HashMap<i64, i64> = HashMap::new();
    for user_roles in roles_by.values() {
        for role in user_roles {
            *user_count.entry(role.id).or_default() += 1;
        }
    }
    let out: Vec<RoleDTO> = roles
        .into_iter()
        .map(|role| {
            let id = role.id;
            to_role_dto(
                role,
                by_role.get(&id).map_or(&[][..], Vec::as_slice),
                user_count.get(&id).copied().unwrap_or_default(),
            )
        })
        .collect();
    write_json(StatusCode::OK, out)
}

pub(super) async fn create_role(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (_, body) = request.into_parts();
    let invalid_name = || {
        write_error(
            StatusCode::BAD_REQUEST,
            &format!("invalid role name: {NAME_RULE_MSG}"),
        )
    };
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return invalid_name();
    };
    let req = match serde_json::from_slice::<CreateRoleReq>(&bytes) {
        Ok(req) if valid_name(req.name.trim()) => req,
        _ => return invalid_name(),
    };
    // Validate any inline permissions before creating the role so a bad grant
    // fails cleanly instead of leaving a permissionless role behind.
    for p in &req.permissions {
        if p.repo_pattern.trim().is_empty() || p.actions.is_empty() {
            return write_error(StatusCode::BAD_REQUEST, "repo_pattern and actions required");
        }
        for a in &p.actions {
            if !valid_role_action(a) {
                return write_error(StatusCode::BAD_REQUEST, &format!("invalid action: {a}"));
            }
        }
    }
    let role = match h
        .store
        .create_role(Role {
            name: req.name.clone(),
            description: req.description.clone(),
            ..Default::default()
        })
        .await
    {
        Ok(role) => role,
        Err(err) => {
            if err.to_string().contains("UNIQUE") || matches!(err, meta::Error::Conflict) {
                return write_error(StatusCode::CONFLICT, "role already exists");
            }
            return map_error(err);
        }
    };
    let mut perms = Vec::with_capacity(req.permissions.len());
    for p in &req.permissions {
        match h
            .store
            .add_permission(Permission {
                role_id: role.id,
                repo_pattern: p.repo_pattern.trim().to_string(),
                actions: p.actions.join(","),
                ..Default::default()
            })
            .await
        {
            Ok(added) => perms.push(added),
            Err(err) => return map_error(err),
        }
    }
    // A freshly created role has no users assigned yet.
    write_json(StatusCode::CREATED, to_role_dto(role, &perms, 0))
}

pub(super) async fn delete_role(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    match h.store.role_managed(id).await {
        Ok(true) => return write_error(StatusCode::CONFLICT, MANAGED_ROLE_MSG),
        Ok(false) => {}
        Err(err) => return map_error(err),
    }
    match h.store.delete_role(id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => map_error(err),
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AddPermissionReq {
    #[serde(default)]
    repo_pattern: String,
    #[serde(default)]
    actions: Vec<String>,
}

pub(super) async fn add_permission(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (_, body) = request.into_parts();
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    match h.store.role_managed(id).await {
        Ok(true) => return write_error(StatusCode::CONFLICT, MANAGED_ROLE_MSG),
        Ok(false) => {}
        Err(err) => return map_error(err),
    }
    let required = || write_error(StatusCode::BAD_REQUEST, "repo_pattern and actions required");
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return required();
    };
    let req = match serde_json::from_slice::<AddPermissionReq>(&bytes) {
        Ok(req) if !req.repo_pattern.is_empty() && !req.actions.is_empty() => req,
        _ => return required(),
    };
    for a in &req.actions {
        if !valid_role_action(a) {
            return write_error(StatusCode::BAD_REQUEST, &format!("invalid action: {a}"));
        }
    }
    match h
        .store
        .add_permission(Permission {
            role_id: id,
            repo_pattern: req.repo_pattern.clone(),
            actions: req.actions.join(","),
            ..Default::default()
        })
        .await
    {
        Ok(p) => write_json(
            StatusCode::CREATED,
            PermissionDTO {
                id: p.id,
                repo_pattern: p.repo_pattern,
                actions: req.actions,
            },
        ),
        Err(err) => map_error(err),
    }
}

pub(super) async fn delete_permission(
    State(h): State<Arc<Handler>>,
    UrlPath((id, perm_id)): UrlPath<(String, String)>,
) -> Response {
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let Ok(perm_id) = perm_id.parse::<i64>() else {
        return write_error(StatusCode::BAD_REQUEST, "invalid permission id");
    };
    match h.store.permission_managed(perm_id).await {
        Ok(true) => return write_error(StatusCode::CONFLICT, MANAGED_ROLE_MSG),
        Ok(false) => {}
        Err(err) => return map_error(err),
    }
    match h.store.delete_permission(id, perm_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => map_error(err),
    }
}

// --- group mappings (admin) ---

#[derive(Debug, Clone, Default, Deserialize)]
struct CreateGroupMappingReq {
    #[serde(default)]
    group_name: String,
    #[serde(default)]
    role_id: i64,
}

/// One identity-provider group bound to a role.
#[derive(Debug, Clone, Serialize)]
struct GroupMappingDTO {
    id: i64,
    group_name: String,
    role_id: i64,
    /// Marks a mapping reconciled from chart configuration, which is therefore
    /// read-only through this API.
    managed: bool,
}

pub(super) async fn list_group_mappings(State(h): State<Arc<Handler>>) -> Response {
    match h.store.list_group_mappings().await {
        Ok(mappings) => write_json(
            StatusCode::OK,
            mappings
                .into_iter()
                .map(|g| GroupMappingDTO {
                    id: g.id,
                    group_name: g.group_name,
                    role_id: g.role_id,
                    managed: g.managed,
                })
                .collect::<Vec<_>>(),
        ),
        Err(err) => map_error(err),
    }
}

pub(super) async fn create_group_mapping(
    State(h): State<Arc<Handler>>,
    request: Request,
) -> Response {
    let (_, body) = request.into_parts();
    let required = || write_error(StatusCode::BAD_REQUEST, "group_name and role_id required");
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return required();
    };
    let req = match serde_json::from_slice::<CreateGroupMappingReq>(&bytes) {
        Ok(req) if !req.group_name.trim().is_empty() => req,
        _ => return required(),
    };
    match h
        .store
        .create_group_mapping(&req.group_name, req.role_id)
        .await
    {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(err) => map_error(err),
    }
}

pub(super) async fn delete_group_mapping(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let id = match path_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    match h.store.group_mapping_managed(id).await {
        Ok(true) => return write_error(StatusCode::CONFLICT, MANAGED_ROLE_MSG),
        Ok(false) => {}
        Err(err) => return map_error(err),
    }
    match h.store.delete_group_mapping(id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => map_error(err),
    }
}

/// Whether the request arrived over TLS, which decides the `Secure` attribute on
/// the session cookie.
fn is_secure(parts: &Parts) -> bool {
    parts.uri.scheme_str() == Some("https")
        || super::header_str(parts, http::HeaderName::from_static("x-forwarded-proto")) == "https"
}

fn parse_duration_secs(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let (negative, rest) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    let mut total = 0f64;
    let mut number = String::new();
    let mut saw_unit = false;
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            number.push(c);
            continue;
        }
        let unit_secs = match c {
            's' => 1.0,
            'm' => {
                if chars.peek() == Some(&'s') {
                    chars.next();
                    0.001
                } else {
                    60.0
                }
            }
            'h' => 3600.0,
            _ => return None,
        };
        let magnitude: f64 = number.parse().ok()?;
        number.clear();
        saw_unit = true;
        total = magnitude.mul_add(unit_secs, total);
    }
    if !saw_unit || !number.is_empty() {
        return None;
    }
    let seconds = total as i64;
    Some(if negative { -seconds } else { seconds })
}

fn format_std_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs.is_multiple_of(3600) {
        return format!("{}h0m0s", secs / 3600);
    }
    if secs.is_multiple_of(60) {
        return format!("{}m0s", secs / 60);
    }
    format!("{secs}s")
}

#[cfg(test)]
pub(crate) mod tests {
    use axum::body::Body;
    use http::{Method, Request, StatusCode};

    use crate::testing::api::{TestResponse, TestServer, new_test_server};

    /// Sends a request carrying a personal access token as a Bearer credential.
    async fn bearer_do(srv: &TestServer, token: &str, method: Method, uri: &str) -> TestResponse {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .expect("build request");
        srv.send(request).await
    }

    #[tokio::test]
    async fn login_and_me() {
        let srv = new_test_server().await;

        // Wrong password.
        let resp = srv
            .anon_do(
                Method::POST,
                "/login",
                r#"{"username":"admin","password":"nope"}"#,
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::UNAUTHORIZED,
            "bad login = {}",
            resp.status
        );

        // Correct password sets a session cookie.
        let resp = srv
            .anon_do(
                Method::POST,
                "/login",
                r#"{"username":"admin","password":"adminpw"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::OK, "login = {}", resp.status);
        let cookie = resp
            .headers
            .get(http::header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .expect("expected session cookie")
            .to_string();

        // /me with the session cookie reports admin.
        let request = Request::builder()
            .method(Method::GET)
            .uri("/me")
            .header(http::header::COOKIE, &cookie)
            .body(Body::empty())
            .expect("build request");
        let me = srv.send(request).await.json();
        assert!(
            me["authenticated"] == true && me["admin"] == true,
            "me = {me}"
        );

        // /me anonymous.
        let anon = srv.anon_do(Method::GET, "/me", "").await.json();
        assert_eq!(anon["authenticated"], false, "anon me = {anon}");
    }

    #[tokio::test]
    async fn token_lifecycle() {
        let srv = new_test_server().await;

        // Create a PAT as admin.
        let resp = srv
        .admin_do(
            Method::POST,
            "/tokens",
            r#"{"name":"ci","description":"ci pipeline","scopes":[{"repo_pattern":"*","actions":["read"]}],"expires_in":"720h"}"#,
        )
        .await;
        assert_eq!(
            resp.status,
            StatusCode::CREATED,
            "create token = {}",
            resp.status
        );
        let created = resp.json();
        let token = created["token"].as_str().unwrap_or_default().to_string();
        assert!(!token.is_empty(), "no token returned");

        // Use the PAT as a Bearer credential against /me.
        let me = bearer_do(&srv, &token, Method::GET, "/me").await.json();
        assert!(
            me["authenticated"] == true && me["username"] == "admin",
            "token me = {me}"
        );

        // List and delete.
        let list = srv.admin_do(Method::GET, "/tokens", "").await.json();
        let list = list.as_array().expect("token list");
        assert_eq!(list.len(), 1, "token list len = {}", list.len());
        let id = list[0]["id"].as_i64().expect("token id");
        let resp = srv
            .admin_do(Method::DELETE, &format!("/tokens/{id}"), "")
            .await;
        assert_eq!(
            resp.status,
            StatusCode::NO_CONTENT,
            "delete token = {}",
            resp.status
        );
    }

    #[tokio::test]
    async fn token_requires_auth() {
        let srv = new_test_server().await;
        let resp = srv.anon_do(Method::GET, "/tokens", "").await;
        assert_eq!(
            resp.status,
            StatusCode::UNAUTHORIZED,
            "anon tokens = {}",
            resp.status
        );
    }

    #[tokio::test]
    async fn user_role_and_group_mapping_flow() {
        let srv = new_test_server().await;

        // Create a user.
        let resp = srv
            .admin_do(
                Method::POST,
                "/users",
                r#"{"username":"dev","password":"devpw","email":"dev@example.com"}"#,
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::CREATED,
            "create user = {}",
            resp.status
        );
        let user_id = resp.json()["id"].as_i64().expect("user id");

        // Duplicate user -> conflict.
        let resp = srv
            .admin_do(
                Method::POST,
                "/users",
                r#"{"username":"dev","password":"x"}"#,
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::CONFLICT,
            "dup user = {}",
            resp.status
        );

        // Create a role with a permission.
        let resp = srv
            .admin_do(
                Method::POST,
                "/roles",
                r#"{"name":"maven-rw","description":"maven read/write"}"#,
            )
            .await;
        let role_id = resp.json()["id"].as_i64().expect("role id");

        let resp = srv
            .admin_do(
                Method::POST,
                &format!("/roles/{role_id}/permissions"),
                r#"{"repo_pattern":"maven-*","actions":["read","write"]}"#,
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::CREATED,
            "add permission = {}",
            resp.status
        );

        // Invalid action rejected.
        let resp = srv
            .admin_do(
                Method::POST,
                &format!("/roles/{role_id}/permissions"),
                r#"{"repo_pattern":"*","actions":["destroy"]}"#,
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::BAD_REQUEST,
            "invalid action = {}",
            resp.status
        );

        // Assign the role to the user.
        let resp = srv
            .admin_do(
                Method::POST,
                &format!("/users/{user_id}/roles"),
                &format!(r#"{{"role_id":{role_id}}}"#),
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::NO_CONTENT,
            "assign role = {}",
            resp.status
        );

        // Group mapping.
        let resp = srv
            .admin_do(
                Method::POST,
                "/group-mappings",
                &format!(r#"{{"group_name":"team-x","role_id":{role_id}}}"#),
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::CREATED,
            "create mapping = {}",
            resp.status
        );
        let mappings = srv
            .admin_do(Method::GET, "/group-mappings", "")
            .await
            .json();
        assert_eq!(
            mappings.as_array().map(Vec::len),
            Some(1),
            "mappings len = {mappings}"
        );

        // Lists.
        let users = srv.admin_do(Method::GET, "/users", "").await.json();
        // admin + dev
        assert_eq!(
            users.as_array().map(Vec::len),
            Some(2),
            "users len = {users}"
        );
        let roles = srv.admin_do(Method::GET, "/roles", "").await.json();
        // admin (bootstrap) + maven-rw
        assert!(
            roles.as_array().map(Vec::len).unwrap_or_default() >= 2,
            "roles len = {roles}"
        );
    }

    #[tokio::test]
    async fn token_update_scopes() {
        let srv = new_test_server().await;

        let resp = srv
        .admin_do(
            Method::POST,
            "/tokens",
            r#"{"name":"ci","description":"ci pipeline","scopes":[{"repo_pattern":"*","actions":["read"]}],"expires_in":"720h"}"#,
        )
        .await;
        assert_eq!(
            resp.status,
            StatusCode::CREATED,
            "create token = {}",
            resp.status
        );

        let list = srv.admin_do(Method::GET, "/tokens", "").await.json();
        let id = list[0]["id"].as_i64().expect("token id");

        // Self PATCH broadens the scopes.
        let resp = srv
            .admin_do(
                Method::PATCH,
                &format!("/tokens/{id}"),
                r#"{"scopes":[{"repo_pattern":"*","actions":["read","write"]}]}"#,
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::NO_CONTENT,
            "update token = {}",
            resp.status
        );

        // Invalid scopes are rejected.
        let resp = srv
            .admin_do(
                Method::PATCH,
                &format!("/tokens/{id}"),
                r#"{"scopes":[{"repo_pattern":"","actions":[]}]}"#,
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::BAD_REQUEST,
            "invalid update = {}",
            resp.status
        );

        // Admin PATCH of the same user's token via the user-scoped route (admin id
        // is 1 in a freshly bootstrapped store).
        let resp = srv
            .admin_do(
                Method::PATCH,
                &format!("/users/1/tokens/{id}"),
                r#"{"scopes":[{"repo_pattern":"lib-*","actions":["read"]}]}"#,
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::NO_CONTENT,
            "admin update user token = {}",
            resp.status
        );
    }

    /// Covers the one management-plane action a token may carry: an audit-scoped PAT
    /// can read the admin surfaces (approvals queue, users) but a read-only PAT
    /// cannot, and the decide/mutate actions can still never be carried by a scope.
    #[tokio::test]
    async fn token_audit_scope() {
        let srv = new_test_server().await;

        let issue = async |body: String| -> (String, StatusCode) {
            let resp = srv.admin_do(Method::POST, "/tokens", &body).await;
            if resp.status != StatusCode::CREATED {
                println!("create failed: {}", resp.text());
                return (String::new(), resp.status);
            }
            let status = resp.status;
            let token = resp.json()["token"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            (token, status)
        };
        let get = async |token: &str, path: &str| -> StatusCode {
            bearer_do(&srv, token, Method::GET, path).await.status
        };

        // approve, security and admin stay session-only.
        for action in ["approve", "security", "admin"] {
            let (_, code) = issue(format!(
            r#"{{"name":"bad-{action}","description":"t","scopes":[{{"repo_pattern":"*","actions":["{action}"]}}],"expires_in":"720h"}}"#
        ))
        .await;
            assert_eq!(
                code,
                StatusCode::BAD_REQUEST,
                "scope action {action} accepted with {code}, want 400"
            );
        }

        let (read_token, code) = issue(
        r#"{"name":"read-only","description":"t","scopes":[{"repo_pattern":"*","actions":["read"]}],"expires_in":"720h"}"#
            .to_string(),
    )
    .await;
        assert_eq!(code, StatusCode::CREATED, "create read token = {code}");
        let (audit_token, code) = issue(
        r#"{"name":"read-audit","description":"t","scopes":[{"repo_pattern":"*","actions":["read","audit"]}],"expires_in":"720h"}"#
            .to_string(),
    )
    .await;
        assert_eq!(code, StatusCode::CREATED, "create audit token = {code}");

        for path in ["/approvals", "/users", "/roles"] {
            let got = get(&read_token, path).await;
            assert_eq!(
                got,
                StatusCode::FORBIDDEN,
                "read token GET {path} = {got}, want 403"
            );
            let got = get(&audit_token, path).await;
            assert_eq!(
                got,
                StatusCode::OK,
                "audit token GET {path} = {got}, want 200"
            );
        }
    }

    mod token_limit {

        use super::super::MAX_TOKENS_PER_USER;
        use crate::testing::api::{TestResponse, new_test_server};
        use http::{Method, StatusCode};

        #[tokio::test]
        async fn token_limit_per_user() {
            let srv = new_test_server().await;
            let resp = srv
                .admin_do(
                    Method::POST,
                    "/users",
                    r#"{"username":"capbot","robot":true}"#,
                )
                .await;
            assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
            let uid = resp.json()["id"].as_i64().expect("user id");

            let issue = async || -> TestResponse {
                srv.admin_do(
                    Method::POST,
                    &format!("/users/{uid}/tokens"),
                    r#"{"name":"t","description":"d","scopes":[{"repo_pattern":"*","actions":["read"]}],"expires_in":"720h"}"#,
                )
                .await
            };

            let mut first_id = 0;
            for i in 0..MAX_TOKENS_PER_USER {
                let resp = issue().await;
                assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
                if i == 0 {
                    first_id = resp.json()["id"].as_i64().expect("token id");
                }
            }

            // The next issue is over the cap.
            let resp = issue().await;
            assert_eq!(resp.status, StatusCode::CONFLICT, "{}", resp.text());

            // Revoking one frees a slot.
            let resp = srv
                .admin_do(
                    Method::DELETE,
                    &format!("/users/{uid}/tokens/{first_id}"),
                    "",
                )
                .await;
            assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
            let resp = issue().await;
            assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
        }
    }
}
