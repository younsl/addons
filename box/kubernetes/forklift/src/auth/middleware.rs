//! Request-principal resolution middleware, the management-plane guards and the
//! cookie/401 helpers the API and repository routers share.

use std::sync::Arc;

use axum::extract::{FromRequestParts, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http::{HeaderValue, StatusCode, header};

use super::{Principal, Service};

/// The name of the signed session cookie.
pub(crate) const SESSION_COOKIE: &str = "forklift_session";

/// Resolves the request principal (if any) and stores it in the request
/// extensions. It never rejects; enforcement is done by handlers via
/// [`from_request_parts`] and [`Principal::can`], or by [`require_auth`].
///
/// The principal is inserted twice: as `Arc<Principal>` (present only when
/// authenticated, for `req.extensions().get::<Arc<Principal>>()`) and as
/// `Option<Arc<Principal>>` (always present, for the
/// `Extension<Option<Arc<Principal>>>` extractor).
pub async fn middleware(State(svc): State<Arc<Service>>, req: Request, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();
    let principal = match svc.resolve(&parts).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(err = %e, "principal resolution failed");
            None
        }
    };
    if let Some(p) = principal.clone() {
        parts.extensions.insert(p);
    }
    parts.extensions.insert(principal);
    next.run(Request::from_parts(parts, body)).await
}

/// Returns the principal stored by [`middleware`], or `None` for anonymous.
pub fn from_request_parts(parts: &http::request::Parts) -> Option<Arc<Principal>> {
    parts.extensions.get::<Arc<Principal>>().cloned()
}

/// Returns the principal carried by a whole request, the borrow-checker-friendly
/// spelling for middleware that has not split the request into parts yet.
pub fn from_request(req: &Request) -> Option<Arc<Principal>> {
    req.extensions().get::<Arc<Principal>>().cloned()
}

/// Extractor for handlers that require an authenticated principal: it rejects
/// with the same plain 401 [`unauthorized`] writes, so a handler can take it
/// instead of repeating the nil check.
pub struct RequirePrincipal(pub Arc<Principal>);

impl<S: Send + Sync> FromRequestParts<S> for RequirePrincipal {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        from_request_parts(parts)
            .map(RequirePrincipal)
            .ok_or_else(unauthorized)
    }
}

/// Extractor for handlers that treat anonymous as a valid caller.
pub struct OptionalPrincipal(pub Option<Arc<Principal>>);

impl<S: Send + Sync> FromRequestParts<S> for OptionalPrincipal {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(OptionalPrincipal(from_request_parts(parts)))
    }
}

/// Middleware that allows only global administrators.
pub async fn require_admin(req: Request, next: Next) -> Response {
    guard(req, next, |p| p.is_admin()).await
}

/// Middleware that allows administrators and principals holding the approve
/// action on at least one repository pattern. Handlers behind it still enforce
/// per-repository checks via `can(repo, ACTION_APPROVE)`.
pub async fn require_approver(req: Request, next: Next) -> Response {
    guard(req, next, |p| p.is_admin() || p.can_approve_any()).await
}

/// Middleware that allows administrators plus principals holding either the
/// approve or the audit action on at least one repository pattern. It gates the
/// package-approval surface (the approvals queue and version-deny list) so a
/// security auditor can read it. Auditors get read-only access: the mutating
/// handlers behind it still enforce the approve action per repository, which the
/// audit action does not grant.
pub async fn require_approver_or_auditor(req: Request, next: Next) -> Response {
    guard(req, next, |p| {
        p.is_admin() || p.can_approve_any() || p.can_audit_any()
    })
    .await
}

/// Middleware that allows administrators and principals holding the audit action
/// on at least one repository pattern. It gates the read-only administrative
/// endpoints (users, roles, group mappings, audit logs, repository permissions);
/// mutating endpoints stay behind [`require_admin`].
pub async fn require_auditor(req: Request, next: Next) -> Response {
    guard(req, next, |p| p.is_admin() || p.can_audit_any()).await
}

/// Middleware that requires any authenticated principal.
pub async fn require_auth(req: Request, next: Next) -> Response {
    guard(req, next, |_| true).await
}

/// Shared shape of every guard: anonymous is 401 (a challenge the caller can
/// answer), an authenticated principal without the action is 403 (an answer that
/// will not change).
async fn guard(req: Request, next: Next, allow: impl Fn(&Principal) -> bool) -> Response {
    match from_request(&req) {
        None => unauthorized(),
        Some(p) if !allow(&p) => text_error(StatusCode::FORBIDDEN, "forbidden"),
        Some(_) => next.run(req).await,
    }
}

/// Writes the signed session cookie onto a response.
pub fn set_session_cookie(resp: &mut Response, value: &str, secure: bool) {
    let mut cookie = format!("{SESSION_COOKIE}={value}; Path=/; HttpOnly");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie.push_str("; SameSite=Lax");
    append_cookie(resp, &cookie);
}

/// Removes the session cookie.
pub fn clear_session_cookie(resp: &mut Response) {
    append_cookie(
        resp,
        &format!("{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly"),
    );
}

pub(crate) fn append_cookie(resp: &mut Response, cookie: &str) {
    if let Ok(v) = HeaderValue::from_str(cookie) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
}

/// Writes a plain 401 without a Basic challenge. Used by the UI API so browsers
/// never show the native credential dialog (cached Basic credentials would
/// otherwise bypass logout and the login page).
pub fn unauthorized() -> Response {
    text_error(StatusCode::UNAUTHORIZED, "unauthorized")
}

/// Writes a 401 with a Basic challenge so package-manager clients (Maven, npm,
/// cargo, go) know to send credentials.
pub fn unauthorized_basic() -> Response {
    let mut resp = unauthorized();
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"forklift\""),
    );
    resp
}

/// Mirrors `crate::server::http_error`, which the server module owns; auth carries its own copy
/// so it does not depend on the server module's initialisation order.
pub(crate) fn text_error(status: StatusCode, msg: &str) -> Response {
    (
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )],
        format!("{msg}\n"),
    )
        .into_response()
}

/// Returns the value of cookie `name` from the request's `Cookie` headers.
pub(crate) fn cookie(parts: &http::request::Parts, name: &str) -> Option<String> {
    for value in parts.headers.get_all(header::COOKIE) {
        let Ok(text) = value.to_str() else { continue };
        for pair in text.split(';') {
            let pair = pair.trim();
            if let Some(v) = pair.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
                return Some(v.trim_matches('"').to_string());
            }
        }
    }
    None
}

/// Decodes an `Authorization: Basic` header into `(username, password)`.
pub(crate) fn basic_auth(value: &str) -> Option<(String, String)> {
    use base64::Engine as _;
    let raw = value.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(raw.trim())
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (user, pass) = decoded.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}
