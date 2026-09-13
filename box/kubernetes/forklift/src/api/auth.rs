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

use super::repositories::path_id;
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
                by_role.get(&id).map(Vec::as_slice).unwrap_or(&[]),
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
        total += magnitude * unit_secs;
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

    mod openapi_schema {
        //!
        //! Rust has no runtime reflection and `Serialize` exposes nothing about the fields a value
        //! *can* carry -- a `skip_serializing_if` field simply vanishes from a serialised value -- so
        //! the shapes are read out of the source instead, with `syn`.

        use std::collections::BTreeMap;
        use std::path::Path;
        use std::sync::OnceLock;

        use crate::meta::Token;

        /// The document served at `/openapi.yaml`, named for the failure messages.
        const SPEC_PATH: &str = "src/openapi/openapi.yaml";
        const SPEC: &str = include_str!("../openapi/openapi.yaml");

        /// The value a schema is pinned to: a DTO named by its Rust path, or -- for the handlers that
        /// answer with a map rather than a struct -- the JSON that map serialises to.
        enum Shape {
            Ty(&'static str),
            Json(serde_json::Value),
        }

        impl Shape {
            fn json_fields(&self) -> Vec<JsonField> {
                match self {
                    Shape::Ty(query) => struct_json_fields(query),
                    Shape::Json(value) => map_json_fields(value),
                }
            }
        }

        /// Pins each response schema to the Rust value the handler actually serialises.
        /// A schema is only useful if it is true: the console's client is generated from
        /// it, so a field the document invents does not exist at runtime, and a field it
        /// omits is invisible to the UI even though the server sends it.
        ///
        /// This compares the property set and which properties are required, not types.
        /// It catches the failure that actually happens -- a field added to a DTO and
        /// forgotten in the document -- without turning the document into a second copy
        /// of the Rust structs.
        ///
        /// Requiredness is not decoration. A field without `skip_serializing_if` is on
        /// the wire on every response, but a schema that does not say so generates an
        /// optional field, and every call site then carries a guard for a case that
        /// cannot occur.
        #[test]
        fn spec_schemas_match_response_shapes() {
            let schemas = spec_schemas();
            let cases: Vec<(&str, Shape)> = vec![
                ("User", Shape::Ty("UserDTO")),
                ("RoleRef", Shape::Ty("RoleRefDTO")),
                ("Permission", Shape::Ty("PermissionDTO")),
                ("Role", Shape::Ty("RoleDTO")),
                (
                    "Token",
                    Shape::Json(
                        serde_json::to_value(crate::api::auth::token_summary(Token::default()))
                            .expect("marshal token summary"),
                    ),
                ),
                ("IssuedToken", Shape::Ty("IssuedTokenDTO")),
                ("HAStatus", Shape::Ty("HAStatus")),
                ("GroupMapping", Shape::Ty("GroupMappingDTO")),
                ("Repository", Shape::Ty("RepositoryDTO")),
                ("RepositoryListItem", Shape::Ty("RepositoryListItemDTO")),
                (
                    "RepositoryCapabilities",
                    Shape::Ty("RepositoryCapabilitiesDTO"),
                ),
                ("RepositoryName", Shape::Ty("RepositoryNameDTO")),
                ("RepoPermission", Shape::Ty("RepoPermissionDTO")),
                ("RepoToken", Shape::Ty("RepoTokenDTO")),
                ("PackageApprovalList", Shape::Ty("ApprovalListDTO")),
                ("ApprovalCount", Shape::Ty("ApprovalCountDTO")),
                ("PendingApprovalRepoList", Shape::Ty("PendingRepoListDTO")),
                ("VersionDenyList", Shape::Ty("VersionDenyListDTO")),
                ("PackageApproval", Shape::Ty("ApprovalDTO")),
                ("VersionDeny", Shape::Ty("VersionDenyDTO")),
                ("AuditLogList", Shape::Ty("AuditLogListDTO")),
                ("AuditLog", Shape::Ty("AuditLogDTO")),
                (
                    "NotificationSamplePreview",
                    Shape::Ty("NotificationSamplePreviewDTO"),
                ),
                (
                    "NotificationSampleReceiver",
                    Shape::Ty("SampleReceiverInfo"),
                ),
                (
                    "NotificationSampleReport",
                    Shape::Ty("NotificationSampleReportDTO"),
                ),
                (
                    "NotificationSampleResult",
                    Shape::Ty("NotificationSampleResultDTO"),
                ),
                ("ApprovalAlarmPayload", Shape::Ty("ApprovalPayload")),
                ("Receiver", Shape::Ty("ReceiverDTO")),
                ("Announcement", Shape::Ty("AnnouncementDTO")),
                ("SessionIdentity", Shape::Ty("SessionIdentityDTO")),
                ("ImpersonationStart", Shape::Ty("ImpersonationStartDTO")),
                ("CreatedUser", Shape::Ty("CreatedUserDTO")),
                ("StatusMessage", Shape::Ty("StatusMessageDTO")),
                ("UploadSession", Shape::Ty("UploadSessionDTO")),
                (
                    "PublicationLifecycleResult",
                    Shape::Ty("PublicationLifecycleResult"),
                ),
                ("ArtifactPurgeResult", Shape::Ty("ArtifactPurgeResultDTO")),
                (
                    "ArtifactForceDeleteResult",
                    Shape::Ty("ArtifactForceDeleteResultDTO"),
                ),
                ("BulkApproveResult", Shape::Ty("BulkApproveResultDTO")),
                ("ArtifactList", Shape::Ty("ArtifactListDTO")),
                ("Artifact", Shape::Ty("ArtifactDTO")),
                ("ArtifactPublication", Shape::Ty("PublicationDTO")),
                ("StorageStats", Shape::Ty("StorageStats")),
                ("DiskUsage", Shape::Ty("Disk")),
                ("DanglingRef", Shape::Ty("DanglingRefDTO")),
                ("StatusCount", Shape::Ty("StatusCountDTO")),
                ("VulnAdvisory", Shape::Ty("VulnAdvisory")),
                ("PendingApprovalRepo", Shape::Ty("PendingRepoDTO")),
                ("MinioStats", Shape::Ty("MinIOStats")),
                // Forklift coverage.
                ("CoverageProject", Shape::Ty("coverage::types::Project")),
                ("CoverageExcludedProject", Shape::Ty("ExcludedProject")),
                ("CoverageProgress", Shape::Ty("Progress")),
                ("CoverageOverview", Shape::Ty("Overview")),
                ("CoverageGroup", Shape::Ty("GroupCoverage")),
                ("CoverageSnapshot", Shape::Ty("Snapshot")),
                ("CoverageLastCommit", Shape::Ty("LastCommit")),
                ("CoveragePipeline", Shape::Ty("Pipeline")),
                ("CoveragePipelineFile", Shape::Ty("PipelineFile")),
                ("CoverageMute", Shape::Ty("CoverageMuteDTO")),
                ("CoverageHostCheck", Shape::Ty("HostCheck")),
                ("CoverageGitlabCheck", Shape::Ty("GitLabCheck")),
                ("CoverageSettings", Shape::Ty("CoverageSettingsDTO")),
                ("CoverageScanStarted", Shape::Ty("CoverageScanStartedDTO")),
                ("CoverageAlarmPayload", Shape::Ty("CoveragePayload")),
                ("CoverageAlarmPreview", Shape::Ty("CoverageAlarmPreviewDTO")),
                // The policy document and each of its sections. Every section is a value
                // field, so it is always on the wire; a client that had to guard each one
                // would be guarding a case the server never produces.
                ("RepoConfig", Shape::Ty("repoconfig::Config")),
                ("CacheConfig", Shape::Ty("CacheConfig")),
                ("AgePolicyConfig", Shape::Ty("AgePolicyConfig")),
                ("ApprovalConfig", Shape::Ty("ApprovalConfig")),
                ("PolicyPipelineConfig", Shape::Ty("PolicyPipelineConfig")),
                ("GroupConfig", Shape::Ty("GroupConfig")),
                ("IPACLConfig", Shape::Ty("IPACLConfig")),
                ("NotifyConfig", Shape::Ty("repoconfig::NotifyConfig")),
                ("RetentionConfig", Shape::Ty("RetentionConfig")),
                ("VulnPolicyConfig", Shape::Ty("VulnPolicyConfig")),
                ("LicensePolicyConfig", Shape::Ty("LicensePolicyConfig")),
                ("UploadConfig", Shape::Ty("repoconfig::UploadConfig")),
                ("UpstreamAuthConfig", Shape::Ty("UpstreamAuthConfig")),
            ];

            let mut failures = Vec::new();
            for (schema, shape) in &cases {
                let declared = schemas
                    .get(*schema)
                    .unwrap_or_else(|| panic!("{schema} is not defined in {SPEC_PATH}"));
                let fields = shape.json_fields();
                let serialised = names(&fields);

                for k in &serialised {
                    if !declared.properties.contains(k) {
                        failures.push(format!(
                            "handlers send {k:?} but {schema} does not declare it; the generated client cannot see it"
                        ));
                    }
                }
                for k in &declared.properties {
                    if !serialised.contains(k) {
                        failures.push(format!(
                            "{schema} declares {k:?} but handlers never send it; the generated client exposes a field that is always undefined"
                        ));
                    }
                }

                for f in &fields {
                    let name = &f.name;
                    if !f.omitempty && !declared.required.contains(name) {
                        failures.push(format!(
                            "{schema}.{name} is sent on every response but the schema does not require it; the generated client makes callers guard a field that is always there"
                        ));
                    } else if f.omitempty && declared.required.contains(name) {
                        failures.push(format!(
                            "{schema}.{name} is omitempty, so responses can leave it out, but the schema requires it"
                        ));
                    }
                }
            }
            assert!(failures.is_empty(), "{}", failures.join("\n"));
        }

        /// One schema flattened: everything it declares plus everything it composes in
        /// through `allOf`.
        pub(crate) struct SchemaShape {
            pub(crate) properties: Vec<String>,
            pub(crate) required: Vec<String>,
        }

        /// Pins each named request-body schema to the Rust value the handler decodes
        /// into. A field the document invents is one a client can send and the server
        /// will silently drop; a field it omits is one the server accepts but no
        /// generated client can offer.
        ///
        /// Unlike responses, requiredness is not read off the struct: whether a field
        /// may be left out is a validation decision the handler makes, not something an
        /// attribute records. Only the property set is compared.
        #[test]
        fn spec_schemas_match_request_shapes() {
            let schemas = spec_schemas();
            let cases = [
                ("RoleCreate", "CreateRoleReq"),
                ("PermissionCreate", "AddPermissionReq"),
                ("AnnouncementInput", "AnnouncementInput"),
                ("LoginInput", "LoginReq"),
                ("RepositoryCreate", "CreateRepositoryReq"),
                ("RepositoryInput", "UpdateRepositoryReq"),
                ("RepositorySecurityInput", "UpdateRepositorySecurityReq"),
                ("RepositoryDisabledInput", "SetDisabledReq"),
                ("UpstreamCheckInput", "CheckUpstreamReq"),
                ("UploadValidationInput", "ValidateUploadReq"),
                ("PublicationYankInput", "YankPublicationReq"),
                ("TokenCreate", "CreateTokenReq"),
                ("TokenScopesInput", "UpdateTokenReq"),
                ("UserCreate", "CreateUserReq"),
                ("UserInput", "UpdateUserReq"),
                ("ImpersonationInput", "ImpersonateReq"),
                ("RoleAssignmentCreate", "AssignRoleReq"),
                ("PackageApprovalCreate", "CreateApprovalReq"),
                ("BulkApproveInput", "ApproveAllReq"),
                ("ApprovalDecisionInput", "DecideApprovalReq"),
                ("VersionDenyCreate", "CreateVersionDenyReq"),
                ("GroupMappingCreate", "CreateGroupMappingReq"),
                ("ReceiverInput", "ReceiverReq"),
                ("WebhookTestInput", "WebhookTestReq"),
            ];

            let mut failures = Vec::new();
            for (schema, ty) in cases {
                let declared = schemas
                    .get(schema)
                    .unwrap_or_else(|| panic!("{schema} is not defined in {SPEC_PATH}"));
                let accepted = names(&struct_json_fields(ty));

                for k in &accepted {
                    if !declared.properties.contains(k) {
                        failures.push(format!(
                            "handlers accept {k:?} but {schema} does not declare it; no generated client can send it"
                        ));
                    }
                }
                for k in &declared.properties {
                    if !accepted.contains(k) {
                        failures.push(format!(
                            "{schema} declares {k:?} but handlers ignore it; clients send a field that does nothing"
                        ));
                    }
                }
            }
            assert!(failures.is_empty(), "{}", failures.join("\n"));
        }

        /// Pins the token-scope action enum to the validator that actually rejects
        /// requests.
        ///
        /// The property-set tests above compare names, not values, so an enum can fall
        /// behind without anything failing: the action stayed grantable, the document
        /// kept listing three, and the generated client had no type for the fourth.
        #[test]
        fn spec_token_scope_actions_match_validator() {
            use crate::auth::{
                ACTION_ADMIN, ACTION_APPROVE, ACTION_AUDIT, ACTION_DELETE, ACTION_READ,
                ACTION_SECURITY, ACTION_WRITE, Scope,
            };

            let documented = spec_enum("TokenScope", "actions");

            // Every action the authorisation model defines, so the test sees an action
            // that becomes grantable as well as one that stops being.
            for action in [
                ACTION_READ,
                ACTION_WRITE,
                ACTION_DELETE,
                ACTION_APPROVE,
                ACTION_AUDIT,
                ACTION_SECURITY,
                ACTION_ADMIN,
            ] {
                let accepted = crate::api::auth::validate_scopes(&[Scope {
                    repo_pattern: "*".to_string(),
                    actions: vec![action.to_string()],
                }])
                .is_none();
                let listed = documented.iter().any(|v| v == action);
                assert!(
                    !(accepted && !listed),
                    "token scopes accept {action:?} but TokenScope.actions does not list it; no generated client can request it"
                );
                assert!(
                    !(!accepted && listed),
                    "TokenScope.actions lists {action:?} but token scopes reject it; the client offers a value the server 400s on"
                );
            }
        }

        // ---------------------------------------------------------------------------
        // The document
        // ---------------------------------------------------------------------------

        /// One schema as the document declares it, before `allOf` is flattened.
        #[derive(Clone, Default, serde::Deserialize)]
        struct SchemaNode {
            #[serde(rename = "$ref", default)]
            reference: String,
            #[serde(default)]
            properties: BTreeMap<String, serde_yaml_ng::Value>,
            #[serde(default)]
            required: Vec<String>,
            #[serde(rename = "allOf", default)]
            all_of: Vec<SchemaNode>,
        }

        #[derive(serde::Deserialize)]
        struct SpecComponents {
            #[serde(default)]
            schemas: BTreeMap<String, SchemaNode>,
        }

        #[derive(serde::Deserialize)]
        struct SpecDoc {
            components: SpecComponents,
        }

        /// The declared and required property names of every object schema in the
        /// document.
        pub(crate) fn spec_schemas() -> BTreeMap<String, SchemaShape> {
            let doc: SpecDoc =
                serde_yaml_ng::from_str(SPEC).unwrap_or_else(|e| panic!("parse spec: {e}"));
            assert!(
                !doc.components.schemas.is_empty(),
                "no schemas found in {SPEC_PATH}"
            );

            let mut out = BTreeMap::new();
            for name in doc.components.schemas.keys() {
                let mut s = resolve_shape(&doc.components.schemas, name, &[]);
                s.properties.sort();
                s.properties.dedup();
                s.required.sort();
                s.required.dedup();
                out.insert(name.clone(), s);
            }
            out
        }

        /// Flattens a schema's own properties together with those of every schema it
        /// composes through `allOf`, which is how a list shape is expressed as "the
        /// detail shape plus these extras".
        fn resolve_shape(
            all: &BTreeMap<String, SchemaNode>,
            name: &str,
            seen: &[String],
        ) -> SchemaShape {
            assert!(
                !seen.iter().any(|s| s == name),
                "schema {name:?} composes itself"
            );
            let mut next = seen.to_vec();
            next.push(name.to_string());
            let node = all.get(name).cloned().unwrap_or_default();
            collect(all, &node, &next)
        }

        fn collect(
            all: &BTreeMap<String, SchemaNode>,
            n: &SchemaNode,
            seen: &[String],
        ) -> SchemaShape {
            let mut s = SchemaShape {
                properties: n.properties.keys().cloned().collect(),
                required: n.required.clone(),
            };
            for part in &n.all_of {
                let nested = match part.reference.strip_prefix("#/components/schemas/") {
                    Some(reference) => resolve_shape(all, reference, seen),
                    None => collect(all, part, seen),
                };
                s.properties.extend(nested.properties);
                s.required.extend(nested.required);
            }
            s
        }

        /// The enum values declared for one array property's items.
        fn spec_enum(schema: &str, property: &str) -> Vec<String> {
            #[derive(serde::Deserialize)]
            struct Doc {
                components: Components,
            }
            #[derive(serde::Deserialize)]
            struct Components {
                schemas: BTreeMap<String, Schema>,
            }
            #[derive(serde::Deserialize)]
            struct Schema {
                #[serde(default)]
                properties: BTreeMap<String, Property>,
            }
            #[derive(serde::Deserialize)]
            struct Property {
                #[serde(default)]
                items: Items,
            }
            #[derive(Default, serde::Deserialize)]
            struct Items {
                #[serde(default)]
                r#enum: Vec<String>,
            }

            let doc: Doc =
                serde_yaml_ng::from_str(SPEC).unwrap_or_else(|e| panic!("parse spec: {e}"));
            let values = doc
                .components
                .schemas
                .get(schema)
                .and_then(|s| s.properties.get(property))
                .map(|p| p.items.r#enum.clone())
                .unwrap_or_default();
            assert!(
                !values.is_empty(),
                "{schema}.{property} declares no enum in {SPEC_PATH}"
            );
            values
        }

        // ---------------------------------------------------------------------------
        // The Rust shapes
        // ---------------------------------------------------------------------------

        /// One key a value serialises to, and whether responses may leave it out.
        #[derive(Clone, PartialEq, Eq)]
        struct JsonField {
            name: String,
            omitempty: bool,
        }

        fn names(fs: &[JsonField]) -> Vec<String> {
            let mut out: Vec<String> = fs.iter().map(|f| f.name.clone()).collect();
            out.sort();
            out.dedup();
            out
        }

        /// Every top-level key the named struct can serialise to, read from its
        /// declaration.
        fn struct_json_fields(query: &str) -> Vec<JsonField> {
            let mut fields = collect_json_fields(query, &[]);
            fields.sort_by(|a, b| a.name.cmp(&b.name));
            fields.dedup();
            fields
        }

        /// The keys a handler-built map serialises to. A map has no declaration to
        /// read, so it is marshalled; a key the marshalled value carries is one the
        /// handler always sets, since the only maps here are built from literals.
        fn map_json_fields(value: &serde_json::Value) -> Vec<JsonField> {
            let obj = value
                .as_object()
                .unwrap_or_else(|| panic!("not a JSON object: {value}"));
            let mut fields: Vec<JsonField> = obj
                .keys()
                .map(|k| JsonField {
                    name: k.clone(),
                    omitempty: false,
                })
                .collect();
            fields.sort_by(|a, b| a.name.cmp(&b.name));
            fields
        }

        fn collect_json_fields(query: &str, seen: &[String]) -> Vec<JsonField> {
            let def = find_struct(query);
            assert!(
                !seen.contains(&def.path),
                "struct {} flattens itself",
                def.path
            );
            let mut next = seen.to_vec();
            next.push(def.path.clone());

            let mut out = Vec::new();
            for f in &def.fields {
                if f.skip {
                    continue;
                }
                if f.flatten {
                    out.extend(collect_json_fields(&f.ty, &next));
                    continue;
                }
                let name = match &f.rename {
                    Some(name) => name.clone(),
                    None => apply_rename_all(def.rename_all.as_deref(), &f.ident),
                };
                out.push(JsonField {
                    name,
                    omitempty: f.omitempty,
                });
            }
            out
        }

        /// One struct declaration, reduced to what decides its JSON shape.
        struct StructDef {
            /// Module path plus name, e.g. `api::auth::UserDTO`.
            path: String,
            rename_all: Option<String>,
            fields: Vec<RawField>,
        }

        struct RawField {
            ident: String,
            /// Last segment of the declared type, for resolving a flattened field.
            ty: String,
            rename: Option<String>,
            skip: bool,
            omitempty: bool,
            flatten: bool,
        }

        /// Looks a struct up by name, or by as much of its module path as it takes to
        /// be unambiguous (`repoconfig::Config`).
        fn find_struct(query: &str) -> &'static StructDef {
            let matches: Vec<&StructDef> = structs()
                .iter()
                .filter(|s| s.path == query || s.path.ends_with(&format!("::{query}")))
                .collect();
            match matches.len() {
                1 => matches[0],
                0 => panic!("no struct named {query} in src/"),
                _ => panic!(
                    "{query} is ambiguous: {}",
                    matches
                        .iter()
                        .map(|s| s.path.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }

        fn structs() -> &'static Vec<StructDef> {
            static INDEX: OnceLock<Vec<StructDef>> = OnceLock::new();
            INDEX.get_or_init(build_index)
        }

        /// Parses every non-test source file and indexes the structs it declares.
        fn build_index() -> Vec<StructDef> {
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
            let mut out = Vec::new();
            let mut stack = vec![root.clone()];
            while let Some(dir) = stack.pop() {
                let entries = std::fs::read_dir(&dir)
                    .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
                for entry in entries {
                    let path = entry.expect("dir entry").path();
                    if path.is_dir() {
                        stack.push(path);
                        continue;
                    }
                    if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                        continue;
                    }
                    // Test files declare fixtures whose names would collide with the
                    // types under test.
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default();
                    if name.ends_with("_test.rs") {
                        continue;
                    }
                    let src = std::fs::read_to_string(&path)
                        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
                    let file = syn::parse_file(&src)
                        .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
                    collect_structs(&file.items, &module_path(&root, &path), &mut out);
                }
            }
            assert!(
                !out.is_empty(),
                "no structs found under src/; the extraction is broken, not the source"
            );
            out
        }

        /// The module path a file's items live in: `src/api/auth.rs` is `api::auth`.
        fn module_path(root: &Path, file: &Path) -> Vec<String> {
            let relative = file.strip_prefix(root).unwrap_or(file).with_extension("");
            relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .filter(|s| s != "lib")
                .collect()
        }

        fn collect_structs(items: &[syn::Item], module: &[String], out: &mut Vec<StructDef>) {
            for item in items {
                match item {
                    syn::Item::Struct(s) => {
                        if let Some(def) = struct_def(s, module) {
                            out.push(def);
                        }
                    }
                    syn::Item::Mod(m) => {
                        // An inline `#[cfg(test)] mod` holds fixtures, not the shapes
                        // the handlers answer with.
                        if is_cfg_test(&m.attrs) {
                            continue;
                        }
                        if let Some((_, inner)) = &m.content {
                            let mut nested = module.to_vec();
                            nested.push(m.ident.to_string());
                            collect_structs(inner, &nested, out);
                        }
                    }
                    _ => {}
                }
            }
        }

        fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
            attrs.iter().any(|a| {
                a.path().is_ident("cfg")
                    && matches!(&a.meta, syn::Meta::List(l) if l.tokens.to_string() == "test")
            })
        }

        fn struct_def(s: &syn::ItemStruct, module: &[String]) -> Option<StructDef> {
            let syn::Fields::Named(named) = &s.fields else {
                return None;
            };
            let mut path = module.to_vec();
            path.push(s.ident.to_string());

            let mut rename_all = None;
            for option in serde_options(&s.attrs) {
                if let SerdeOption::NameValue(key, value) = option
                    && key == "rename_all"
                {
                    rename_all = Some(value);
                }
            }

            let fields = named
                .named
                .iter()
                .map(|f| {
                    let mut raw = RawField {
                        ident: ident_name(f.ident.as_ref().expect("named field")),
                        ty: type_name(&f.ty),
                        rename: None,
                        skip: false,
                        omitempty: false,
                        flatten: false,
                    };
                    for option in serde_options(&f.attrs) {
                        match option {
                            SerdeOption::Path(key) => match key.as_str() {
                                "skip" | "skip_serializing" => raw.skip = true,
                                "flatten" => raw.flatten = true,
                                _ => {}
                            },
                            SerdeOption::NameValue(key, value) => match key.as_str() {
                                "rename" => raw.rename = Some(value),
                                "skip_serializing_if" => raw.omitempty = true,
                                _ => {}
                            },
                        }
                    }
                    raw
                })
                .collect();

            Some(StructDef {
                path: path.join("::"),
                rename_all,
                fields,
            })
        }

        /// One entry inside `#[serde(...)]`: a bare flag or a `key = "value"` pair.
        enum SerdeOption {
            Path(String),
            NameValue(String, String),
        }

        fn serde_options(attrs: &[syn::Attribute]) -> Vec<SerdeOption> {
            let mut out = Vec::new();
            for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
                let parsed = attr
                    .parse_args_with(
                        syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                    )
                    .unwrap_or_default();
                for meta in parsed {
                    match meta {
                        syn::Meta::Path(p) => {
                            if let Some(ident) = p.get_ident() {
                                out.push(SerdeOption::Path(ident.to_string()));
                            }
                        }
                        syn::Meta::NameValue(nv) => {
                            let Some(ident) = nv.path.get_ident() else {
                                continue;
                            };
                            let value = match &nv.value {
                                syn::Expr::Lit(syn::ExprLit {
                                    lit: syn::Lit::Str(s),
                                    ..
                                }) => s.value(),
                                _ => String::new(),
                            };
                            out.push(SerdeOption::NameValue(ident.to_string(), value));
                        }
                        // `rename(serialize = "x")` and friends: none of the DTOs use
                        // the split form, so it carries no shape information here.
                        syn::Meta::List(_) => {}
                    }
                }
            }
            out
        }

        /// A raw identifier (`r#type`) serialises under the name without the escape.
        fn ident_name(ident: &syn::Ident) -> String {
            let s = ident.to_string();
            s.strip_prefix("r#").unwrap_or(&s).to_string()
        }

        fn type_name(ty: &syn::Type) -> String {
            match ty {
                syn::Type::Path(p) => p
                    .path
                    .segments
                    .last()
                    .map(|s| s.ident.to_string())
                    .unwrap_or_default(),
                _ => String::new(),
            }
        }

        fn apply_rename_all(rule: Option<&str>, ident: &str) -> String {
            match rule {
                None | Some("snake_case") => ident.to_string(),
                Some("camelCase") => {
                    let pascal = apply_rename_all(Some("PascalCase"), ident);
                    let mut chars = pascal.chars();
                    match chars.next() {
                        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
                        None => pascal,
                    }
                }
                Some("PascalCase") => ident
                    .split('_')
                    .map(|word| {
                        let mut chars = word.chars();
                        match chars.next() {
                            Some(first) => {
                                first.to_uppercase().collect::<String>() + chars.as_str()
                            }
                            None => String::new(),
                        }
                    })
                    .collect(),
                Some("kebab-case") => ident.replace('_', "-"),
                Some("SCREAMING_SNAKE_CASE") => ident.to_uppercase(),
                Some("lowercase") => ident.to_lowercase(),
                Some("UPPERCASE") => ident.to_uppercase(),
                Some(other) => panic!("unhandled serde rename_all rule {other:?}"),
            }
        }
    }

    mod openapi_response {
        //!
        //! Rust's harness has no subtests, so the mismatches are collected and reported together at the
        //! end of each test.

        use std::collections::BTreeMap;

        use http::{Method, StatusCode};
        use serde_json::Value;

        use super::openapi_schema::{SchemaShape, spec_schemas};
        use crate::testing::api::{TestServer, new_test_server};

        /// The document served at `/openapi.yaml`, named for the failure messages.
        const SPEC_PATH: &str = "src/openapi/openapi.yaml";
        const SPEC: &str = include_str!("../openapi/openapi.yaml");

        /// One documented operation and the request that exercises it.
        struct Case {
            name: &'static str,
            /// Operation as it appears in the document.
            method: &'static str,
            path: &'static str,
            /// Path parameter values, in the order they appear in path.
            params: &'static [&'static str],
            /// Query string appended to the request, without the leading "?".
            query: &'static str,
            /// Request body, for the mutations.
            body: &'static str,
        }

        /// Shorthand for the rows below, which set two or three fields out of six.
        const fn case(name: &'static str, method: &'static str, path: &'static str) -> Case {
            Case {
                name,
                method,
                path,
                params: &[],
                query: "",
                body: "",
            }
        }

        /// Pins each operation to the schema its handler actually answers with, by
        /// calling the endpoint and comparing the keys on the wire to the schema the
        /// document attaches to that operation.
        ///
        /// The schema tests next door compare a schema against a Rust DTO, so a schema
        /// that is right in isolation still passes when the document hangs it off the
        /// wrong operation. That is not hypothetical: GET /repositories/{id} was
        /// documented as Repository while the handler answers with the list item shape,
        /// and six fields -- including the one the console branches its upload UI on --
        /// were missing from the generated client with every test green.
        ///
        /// Mutations are here too, and they are the ones that could not be checked any
        /// other way: five of them had their schema attached on the strength of a manual
        /// measurement, which proved the link was right that day and nothing more.
        #[tokio::test]
        async fn spec_operations_match_live_responses() {
            let srv = new_test_server().await;
            seed_for_response_test(&srv).await;

            let cases = [
                // Reads.
                Case {
                    params: &["1"],
                    ..case("repository detail", "get", "/api/v1/repositories/{id}")
                },
                Case {
                    params: &["1"],
                    ..case(
                        "repository artifacts",
                        "get",
                        "/api/v1/repositories/{id}/artifacts",
                    )
                },
                Case {
                    params: &["1"],
                    ..case(
                        "repository audit log",
                        "get",
                        "/api/v1/repositories/{id}/audit-logs",
                    )
                },
                Case {
                    params: &["1"],
                    ..case(
                        "repository permissions",
                        "get",
                        "/api/v1/repositories/{id}/permissions",
                    )
                },
                Case {
                    params: &["1"],
                    ..case(
                        "repository tokens",
                        "get",
                        "/api/v1/repositories/{id}/tokens",
                    )
                },
                Case {
                    params: &["1"],
                    ..case("user tokens", "get", "/api/v1/users/{id}/tokens")
                },
                case("session principal", "get", "/api/v1/me"),
                case("storage overview", "get", "/api/v1/storage"),
                case("ha status", "get", "/api/v1/ha"),
                Case {
                    query: "q=probe",
                    ..case("global search", "get", "/api/v1/search")
                },
                // Mutations. Ordered so each one's precondition is met by the seed or by
                // a mutation above it.
                Case {
                    body: r#"{"name":"pinned-hosted","format":"maven","type":"hosted"}"#,
                    ..case("create repository", "post", "/api/v1/repositories")
                },
                Case {
                    params: &["1"],
                    body: r#"{"upstream_url":"","config":{}}"#,
                    ..case("update repository", "put", "/api/v1/repositories/{id}")
                },
                Case {
                    params: &["1"],
                    body: r#"{"config":{"approval":{"enabled":false}}}"#,
                    ..case(
                        "update repository security",
                        "put",
                        "/api/v1/repositories/{id}/security",
                    )
                },
                Case {
                    params: &["1"],
                    body: r#"{"disabled":false}"#,
                    ..case(
                        "toggle repository disabled",
                        "post",
                        "/api/v1/repositories/{id}/disabled",
                    )
                },
                Case {
                    params: &["1"],
                    body: r#"{"disabled":false}"#,
                    ..case("update user", "put", "/api/v1/users/{id}")
                },
                Case {
                    body: r#"{"name":"pinned-role","description":"created by the pinning test"}"#,
                    ..case("create role", "post", "/api/v1/roles")
                },
                Case {
                    params: &["1"],
                    body: r#"{"repo_pattern":"pinned-*","actions":["read"]}"#,
                    ..case(
                        "add role permission",
                        "post",
                        "/api/v1/roles/{id}/permissions",
                    )
                },
                Case {
                    body: r#"{"name":"pinned-token","description":"created by the pinning test","expires_in":"720h","scopes":[{"repo_pattern":"maven-*","actions":["read"]}]}"#,
                    ..case("create token", "post", "/api/v1/tokens")
                },
                Case {
                    params: &["1"],
                    body: r#"{"name":"pinned-user-token","description":"created by the pinning test","expires_in":"720h","scopes":[{"repo_pattern":"maven-*","actions":["read"]}]}"#,
                    ..case("create user token", "post", "/api/v1/users/{id}/tokens")
                },
                Case {
                    body: r#"{"repo":"pinned-proxy","package":"com.example:pinned","version":"1.0.0","reason":"pinning test"}"#,
                    ..case("create version deny", "post", "/api/v1/version-denies")
                },
                Case {
                    body: r#"{"name":"pinned-recv","description":"created by the pinning test","webhook_url":"https://example.invalid/hook","enabled":true}"#,
                    ..case("create receiver", "post", "/api/v1/notification/receivers")
                },
                Case {
                    params: &["1"],
                    body: r#"{"name":"seeded-recv","description":"updated by the pinning test","webhook_url":"https://example.invalid/hook","enabled":true}"#,
                    ..case(
                        "update receiver",
                        "put",
                        "/api/v1/notification/receivers/{id}",
                    )
                },
            ];

            let schemas = spec_schemas();
            let mut failures = Vec::new();
            for tc in &cases {
                let schema_name = operation_response_schema(tc.method, tc.path);
                // Routes are mounted without the /api/v1 prefix the document carries;
                // the gateway adds it in production.
                let mut url = tc
                    .path
                    .strip_prefix("/api/v1")
                    .unwrap_or(tc.path)
                    .to_string();
                for value in tc.params {
                    url = replace_route_params(&url, value);
                }
                if !tc.query.is_empty() {
                    url = format!("{url}?{}", tc.query);
                }
                let declared = schemas.get(&schema_name).unwrap_or_else(|| {
                    panic!(
                        "{} {} references schema {schema_name:?}, which is not defined",
                        tc.method, tc.path
                    )
                });

                let method =
                    Method::from_bytes(tc.method.to_uppercase().as_bytes()).expect("method");
                let resp = srv.admin_do(method.clone(), &url, tc.body).await;
                assert!(
                    resp.status == StatusCode::OK || resp.status == StatusCode::CREATED,
                    "{method} {url}: status {} ({})",
                    resp.status.as_u16(),
                    resp.text().trim()
                );
                let body = first_object(&resp.body).unwrap_or_else(|| {
                    panic!(
                        "{method} {url} answered with an empty list, so there is nothing to compare; seed a row for it ({})",
                        tc.name
                    )
                });
                compare_to_schema(
                    &format!("{method} {url}"),
                    &schema_name,
                    declared,
                    &body,
                    &mut failures,
                );
            }
            assert!(failures.is_empty(), "{}", failures.join("\n"));
        }

        /// Reduces a response to the object to compare: the body itself, or a list's
        /// first element. A list is reported as absent rather than as an empty object,
        /// so an unseeded endpoint fails loudly instead of passing vacuously.
        fn first_object(raw: &[u8]) -> Option<serde_json::Map<String, Value>> {
            let trimmed = std::str::from_utf8(raw).unwrap_or_default().trim();
            if trimmed.starts_with('{') {
                let body: serde_json::Map<String, Value> =
                    serde_json::from_str(trimmed).unwrap_or_else(|e| panic!("decode object: {e}"));
                return Some(body);
            }
            if trimmed.starts_with('[') {
                let items: Vec<Value> =
                    serde_json::from_str(trimmed).unwrap_or_else(|e| panic!("decode list: {e}"));
                let first = items.first()?;
                let body = first
                    .as_object()
                    .unwrap_or_else(|| panic!("decode list element: {first}"));
                return Some(body.clone());
            }
            None
        }

        /// Checks the keys on the wire against the schema both ways: an undeclared key
        /// is invisible to the generated client, and a required key that never arrives
        /// makes the client trust something absent.
        fn compare_to_schema(
            label: &str,
            schema_name: &str,
            declared: &SchemaShape,
            body: &serde_json::Map<String, Value>,
            failures: &mut Vec<String>,
        ) {
            for key in body.keys() {
                if !declared.properties.contains(key) {
                    failures.push(format!(
                        "{label} sends {key:?} but {schema_name} does not declare it; the generated client cannot see it"
                    ));
                }
            }
            // Only requiredness is checked in the other direction: an optional field may
            // legitimately be absent from this particular response.
            for key in &declared.required {
                if !body.contains_key(key) {
                    failures.push(format!(
                        "{schema_name} requires {key:?} but {label} did not send it"
                    ));
                }
            }
        }

        /// Creates the rows the operations are called against, so the responses are of
        /// real records rather than of 404s and empty lists.
        ///
        /// Repository 1 is hosted and repository 2 is a proxy: the version-deny and
        /// approval surfaces only accept a proxy, while the upload capability only
        /// applies to a hosted one.
        async fn seed_for_response_test(srv: &TestServer) {
            async fn seed(srv: &TestServer, what: &str, path: &str, body: &str) {
                let resp = srv.admin_do(Method::POST, path, body).await;
                assert!(
                    resp.status == StatusCode::CREATED || resp.status == StatusCode::OK,
                    "seed {what}: status {} ({})",
                    resp.status.as_u16(),
                    resp.text().trim()
                );
            }
            seed(
                srv,
                "hosted repository",
                "/repositories",
                r#"{"name":"maven-hosted","format":"maven","type":"hosted"}"#,
            )
            .await;
            seed(
                srv,
                "proxy repository",
                "/repositories",
                r#"{"name":"pinned-proxy","format":"maven","type":"proxy","upstream_url":"https://example.invalid/maven2"}"#,
            )
            .await;
            // A token scoped to the seeded repository, so both /repositories/{id}/tokens
            // and /users/{id}/tokens answer with a row instead of an empty list.
            seed(
                srv,
                "scoped token",
                "/users/1/tokens",
                r#"{"name":"seeded-token","description":"seeded by the pinning test","expires_in":"720h","scopes":[{"repo_pattern":"maven-*","actions":["read"]}]}"#,
            )
            .await;
            seed(
                srv,
                "receiver",
                "/notification/receivers",
                r#"{"name":"seeded-recv","description":"seeded by the pinning test","webhook_url":"https://example.invalid/hook","enabled":true}"#,
            )
            .await;
            seed(
                srv,
                "group mapping",
                "/group-mappings",
                r#"{"group_name":"seeded-group","role_id":1}"#,
            )
            .await;
        }

        /// No documented operation the table drives carries more than one parameter.
        fn replace_route_params(path: &str, value: &str) -> String {
            let mut out = String::new();
            let mut rest = path;
            while let Some(open) = rest.find('{') {
                out.push_str(&rest[..open]);
                match rest[open..].find('}') {
                    Some(close) => {
                        out.push_str(value);
                        rest = &rest[open + close + 1..];
                    }
                    None => {
                        rest = &rest[open..];
                        break;
                    }
                }
            }
            out.push_str(rest);
            out
        }

        /// A path item mixes methods with a "parameters" sequence, so each key is
        /// decoded on demand rather than through one struct that would have to model
        /// both shapes.
        #[derive(serde::Deserialize)]
        struct PathsDoc {
            paths: BTreeMap<String, BTreeMap<String, serde_yaml_ng::Value>>,
        }

        #[derive(serde::Deserialize)]
        struct Operation {
            #[serde(default)]
            responses: BTreeMap<String, ResponseNode>,
        }

        #[derive(Default, serde::Deserialize)]
        struct ResponseNode {
            #[serde(default)]
            content: BTreeMap<String, MediaType>,
        }

        #[derive(Default, serde::Deserialize)]
        struct MediaType {
            #[serde(default)]
            schema: SchemaRef,
        }

        #[derive(Default, serde::Deserialize)]
        struct SchemaRef {
            #[serde(rename = "$ref", default)]
            reference: String,
            #[serde(default)]
            items: ItemsRef,
        }

        #[derive(Default, serde::Deserialize)]
        struct ItemsRef {
            #[serde(rename = "$ref", default)]
            reference: String,
        }

        fn paths_doc() -> PathsDoc {
            serde_yaml_ng::from_str(SPEC).unwrap_or_else(|e| panic!("parse spec: {e}"))
        }

        /// The component schema an operation's success response resolves to, following
        /// an array's items when the response is a list.
        fn operation_response_schema(method: &str, path: &str) -> String {
            let doc = paths_doc();
            let node = doc
                .paths
                .get(path)
                .and_then(|item| item.get(method))
                .unwrap_or_else(|| panic!("{method} {path} is not in {SPEC_PATH}"));
            let operation: Operation = serde_yaml_ng::from_value(node.clone())
                .unwrap_or_else(|e| panic!("decode {method} {path}: {e}"));

            for status in ["200", "201"] {
                let Some(content) = operation
                    .responses
                    .get(status)
                    .and_then(|r| r.content.get("application/json"))
                else {
                    continue;
                };
                let reference = if content.schema.reference.is_empty() {
                    &content.schema.items.reference
                } else {
                    &content.schema.reference
                };
                if let Some(name) = reference.strip_prefix("#/components/schemas/") {
                    return name.to_string();
                }
            }
            panic!(
                "{method} {path} has no named success response schema; an inline shape cannot be pinned"
            )
        }

        /// The table above names the operations worth seeding data for; this catches the rest without
        /// anyone having to remember to list them.
        ///
        /// Operations whose success response has no named schema are reported rather
        /// than skipped: an inline shape is exactly what leaves a generated client with
        /// an anonymous per-operation type and a screen with nothing to import.
        #[tokio::test]
        async fn spec_covers_every_parameterless_get() {
            let srv = new_test_server().await;
            seed_for_response_test(&srv).await;
            let schemas = spec_schemas();

            // GETs that need a query parameter to answer at all. Without one they 400,
            // which the sweep would otherwise skip as "nothing to compare".
            let required = BTreeMap::from([("/api/v1/search", "q=maven")]);

            let mut skipped = Vec::new();
            let mut failures = Vec::new();
            for path in parameterless_get_paths() {
                let mut url = path.strip_prefix("/api/v1").unwrap_or(&path).to_string();
                if let Some(q) = required.get(path.as_str()) {
                    url = format!("{url}?{q}");
                }
                let resp = srv.admin_do(Method::GET, &url, "").await;
                if resp.status != StatusCode::OK {
                    skipped.push(format!("{path} (status {})", resp.status.as_u16()));
                    continue;
                }

                // A list is compared through its first element, which is what the item
                // schema describes. An empty one is recorded rather than silently
                // passed: it means nothing was checked.
                let Some(body) = first_object(&resp.body) else {
                    skipped.push(format!("{path} (empty list or non-object body)"));
                    continue;
                };

                let schema_name = operation_response_schema("get", &path);
                let declared = schemas.get(&schema_name).unwrap_or_else(|| {
                    panic!("GET {path} references schema {schema_name:?}, which is not defined")
                });
                compare_to_schema(
                    &format!("GET {url}"),
                    &schema_name,
                    declared,
                    &body,
                    &mut failures,
                );
            }
            // Skips are reported at the end rather than next to the comparisons, where a
            // green run reads as full coverage of every path swept.
            if !skipped.is_empty() {
                println!("not compared ({}): {}", skipped.len(), skipped.join(", "));
            }
            assert!(failures.is_empty(), "{}", failures.join("\n"));
        }

        /// The documented paths with a GET and no path parameter, which are the ones
        /// callable without seeding anything first.
        fn parameterless_get_paths() -> Vec<String> {
            let doc = paths_doc();
            let mut paths: Vec<String> = doc
                .paths
                .iter()
                .filter(|(path, item)| {
                    !path.contains('{') && path.starts_with("/api/v1") && item.contains_key("get")
                })
                .map(|(path, _)| path.clone())
                .collect();
            paths.sort();
            paths
        }
    }
}
