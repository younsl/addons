//! OIDC discovery, the Authorization Code flow and ID-token verification.
//!
//! The observable behaviour (routes, statuses, cookie names, claim handling) is unchanged.

use std::future::Future;
use std::pin::Pin;
use std::str::FromStr as _;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::StatusCode;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreIdToken, CoreProviderMetadata};
use openidconnect::{
    AsyncHttpClient, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet,
    EndpointNotSet, EndpointSet, HttpRequest, HttpResponse, IssuerUrl, Nonce, RedirectUrl,
    Scope as OidcScope, TokenResponse as _,
};

use super::middleware::{append_cookie, cookie, text_error};
use super::{Error, Service};

/// The provider's client after discovery: the authorization endpoint is always
/// set, the token and userinfo endpoints only if the issuer advertised them.
type DiscoveredClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// Configures the OIDC provider (decoupled from the config module).
#[derive(Debug, Clone, Default)]
pub struct OidcParams {
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_url: String,
    pub username_claim: String,
    pub groups_claim: String,
}

/// Verifies OIDC tokens and drives the Authorization Code flow against Keycloak
/// (or any compliant provider).
pub struct OidcProvider {
    client: DiscoveredClient,
    http: HttpClient,
    pub(super) username_claim: String,
    pub(super) groups_claim: String,
}

impl OidcProvider {
    /// Discovers the provider and builds the verifier and OAuth2 config. It
    /// requires network access to the issuer at startup.
    pub async fn new(p: OidcParams) -> Result<Arc<OidcProvider>, Error> {
        let http = HttpClient::new()?;
        let issuer = IssuerUrl::new(p.issuer_url.clone())
            .map_err(|e| Error::Oidc(format!("oidc discovery: {e}")))?;
        let metadata = CoreProviderMetadata::discover_async(issuer, &http)
            .await
            .map_err(|e| Error::Oidc(format!("oidc discovery: {e}")))?;
        Ok(Arc::new(OidcProvider {
            client: build_client(metadata, &p)?,
            http,
            username_claim: if p.username_claim.is_empty() {
                "preferred_username".to_string()
            } else {
                p.username_claim
            },
            groups_claim: if p.groups_claim.is_empty() {
                "groups".to_string()
            } else {
                p.groups_claim
            },
        }))
    }

    /// Validates a raw ID token and extracts identity claims, returning
    /// `(username, email, groups)`.
    pub fn verify(&self, raw_id_token: &str) -> Result<(String, String, Vec<String>), Error> {
        self.verify_with_nonce(raw_id_token, None)
    }

    /// [`OidcProvider::verify`] with an optional nonce to match against the token's `nonce`
    /// claim.
    pub fn verify_with_nonce(
        &self,
        raw_id_token: &str,
        nonce: Option<&Nonce>,
    ) -> Result<(String, String, Vec<String>), Error> {
        let token = CoreIdToken::from_str(raw_id_token)
            .map_err(|e| Error::Oidc(format!("parse id token: {e}")))?;
        let verifier = self.client.id_token_verifier();
        match nonce {
            Some(n) => token.claims(&verifier, n),
            None => token.claims(&verifier, |_: Option<&Nonce>| Ok(())),
        }
        .map_err(|e| Error::Oidc(format!("verify id token: {e}")))?;
        self.extract_claims(&raw_claims(raw_id_token)?)
    }

    /// Pulls the username, email and group claims out of a verified claim set.
    pub(super) fn extract_claims(
        &self,
        claims: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(String, String, Vec<String>), Error> {
        let username = claims
            .get(&self.username_claim)
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if username.is_empty() {
            return Err(Error::Oidc(format!(
                "missing username claim {:?}",
                self.username_claim
            )));
        }
        let email = claims
            .get("email")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let groups = claims
            .get(&self.groups_claim)
            .and_then(|v| v.as_array())
            .map(|raw| {
                raw.iter()
                    .filter_map(|g| g.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Ok((username.to_string(), email.to_string(), groups))
    }

    /// Builds a provider whose endpoints are set directly, bypassing issuer discovery.
    #[cfg(test)]
    pub(crate) fn new_for_test(
        client_id: &str,
        redirect_url: &str,
        auth_url: &str,
        token_url: &str,
    ) -> Arc<OidcProvider> {
        use openidconnect::core::{
            CoreJsonWebKeySet, CoreJwsSigningAlgorithm, CoreResponseType, CoreSubjectIdentifierType,
        };
        use openidconnect::{
            AuthUrl, EmptyAdditionalProviderMetadata, JsonWebKeySetUrl, ResponseTypes, TokenUrl,
        };

        let metadata = CoreProviderMetadata::new(
            IssuerUrl::new("https://idp.example.com".into()).expect("issuer"),
            AuthUrl::new(auth_url.to_string()).expect("auth url"),
            JsonWebKeySetUrl::new("https://idp.example.com/keys".into()).expect("jwks url"),
            vec![ResponseTypes::new(vec![CoreResponseType::Code])],
            vec![CoreSubjectIdentifierType::Public],
            vec![CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256],
            EmptyAdditionalProviderMetadata {},
        )
        .set_token_endpoint(Some(
            TokenUrl::new(token_url.to_string()).expect("token url"),
        ))
        .set_jwks(CoreJsonWebKeySet::new(Vec::new()));
        let params = OidcParams {
            client_id: client_id.to_string(),
            redirect_url: redirect_url.to_string(),
            ..Default::default()
        };
        Arc::new(OidcProvider {
            client: build_client(metadata, &params).expect("build test client"),
            http: HttpClient::new().expect("http client"),
            username_claim: "preferred_username".into(),
            groups_claim: "groups".into(),
        })
    }
}

/// Wires the discovered metadata into a client with the configured credentials
/// and redirect URI.
fn build_client(metadata: CoreProviderMetadata, p: &OidcParams) -> Result<DiscoveredClient, Error> {
    let secret = if p.client_secret.is_empty() {
        None
    } else {
        Some(ClientSecret::new(p.client_secret.clone()))
    };
    let mut client =
        CoreClient::from_provider_metadata(metadata, ClientId::new(p.client_id.clone()), secret);
    if !p.redirect_url.is_empty() {
        client = client.set_redirect_uri(
            RedirectUrl::new(p.redirect_url.clone())
                .map_err(|e| Error::Oidc(format!("oidc redirect url: {e}")))?,
        );
    }
    Ok(client)
}

/// Splits a JWT and decodes its payload into a raw claim map. The token has
/// already been verified when this runs.
fn raw_claims(raw: &str) -> Result<serde_json::Map<String, serde_json::Value>, Error> {
    let payload = raw
        .split('.')
        .nth(1)
        .ok_or_else(|| Error::Oidc("malformed id token".into()))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|e| Error::Oidc(format!("decode id token claims: {e}")))?;
    serde_json::from_slice(&bytes).map_err(|e| Error::Oidc(format!("parse id token claims: {e}")))
}

/// The cookie carrying the CSRF state across the Authorization Code round trip.
const OIDC_STATE_COOKIE: &str = "forklift_oidc_state";
/// The cookie carrying the ID-token nonce across the round trip.
const OIDC_NONCE_COOKIE: &str = "forklift_oidc_nonce";

/// The `users.source` value for identities minted by the OIDC login flow.
pub(crate) const SOURCE_OIDC: &str = "oidc";

/// Starts the Authorization Code flow.
pub async fn handle_login(State(svc): State<Arc<Service>>, req: Request) -> Response {
    let Some(oidc) = svc.oidc() else {
        return text_error(StatusCode::NOT_FOUND, "oidc not configured");
    };
    let (parts, _) = req.into_parts();
    let state = random_state();
    let state_for_url = state.clone();
    let (url, _csrf, nonce) = oidc
        .client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            move || CsrfToken::new(state_for_url),
            Nonce::new_random,
        )
        .add_scope(OidcScope::new("profile".to_string()))
        .add_scope(OidcScope::new("email".to_string()))
        .url();

    let secure = is_secure(&parts);
    let mut resp = redirect(url.as_str());
    append_cookie(&mut resp, &flow_cookie(OIDC_STATE_COOKIE, &state, secure));
    append_cookie(
        &mut resp,
        &flow_cookie(OIDC_NONCE_COOKIE, nonce.secret(), secure),
    );
    resp
}

/// Completes the flow: it verifies state, exchanges the code, upserts the OIDC
/// user, and issues a session cookie.
pub async fn handle_callback(State(svc): State<Arc<Service>>, req: Request) -> Response {
    let Some(oidc) = svc.oidc() else {
        return text_error(StatusCode::NOT_FOUND, "oidc not configured");
    };
    let (parts, _) = req.into_parts();
    let query: std::collections::HashMap<String, String> = parts
        .uri
        .query()
        .map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect())
        .unwrap_or_default();

    let state_cookie = cookie(&parts, OIDC_STATE_COOKIE).unwrap_or_default();
    if state_cookie.is_empty() || Some(&state_cookie) != query.get("state") {
        return text_error(StatusCode::BAD_REQUEST, "invalid oauth state");
    }
    let code = query.get("code").cloned().unwrap_or_default();
    let token = match oidc.client.exchange_code(AuthorizationCode::new(code)) {
        Ok(request) => match request.request_async(&oidc.http).await {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(err = %e, "oidc token exchange failed");
                return text_error(StatusCode::BAD_GATEWAY, "token exchange failed");
            }
        },
        Err(e) => {
            tracing::debug!(err = %e, "oidc token endpoint not configured");
            return text_error(StatusCode::BAD_GATEWAY, "token exchange failed");
        }
    };
    let Some(raw_id) = token.id_token() else {
        return text_error(StatusCode::BAD_GATEWAY, "no id_token in response");
    };
    let nonce = cookie(&parts, OIDC_NONCE_COOKIE).map(Nonce::new);
    let (username, email, groups) =
        match oidc.verify_with_nonce(&raw_id.to_string(), nonce.as_ref()) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(err = %e, "oidc id token verification failed");
                return text_error(StatusCode::UNAUTHORIZED, "id token verification failed");
            }
        };
    let u = match svc
        .store()
        .ensure_user(&username, &email, SOURCE_OIDC)
        .await
    {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(err = %e, "oidc user provisioning failed");
            return text_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "user provisioning failed",
            );
        }
    };
    // A robot account is token-only and a disabled account is off: neither may
    // obtain an interactive session, even if the IdP asserts a matching username.
    if u.robot || u.disabled {
        return text_error(StatusCode::FORBIDDEN, "account may not log in");
    }
    // Materialize the user's group-mapped forklift roles so the assignment is
    // durable and visible in the UI, syncing on every login to track the
    // identity provider's current group membership. Best-effort: effective
    // permissions are also resolved dynamically from the session groups, so a
    // sync failure must not block the login.
    if let Err(e) = svc.store().sync_oidc_group_roles(u.id, &groups).await {
        tracing::warn!(user = %u.username, err = %e, "sync oidc group roles");
    }
    // Best-effort: a bookkeeping failure must not block the login.
    if let Err(e) = svc.store().touch_last_login(u.id).await {
        tracing::warn!(user = %u.username, err = %e, "record last login");
    }
    let value = match svc.issue_session(&username, SOURCE_OIDC, &groups) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(err = %e, "session issue failed");
            return text_error(StatusCode::INTERNAL_SERVER_ERROR, "session issue failed");
        }
    };
    let mut resp = redirect("/");
    super::middleware::set_session_cookie(&mut resp, &value, is_secure(&parts));
    resp
}

/// Clears the session cookie.
pub async fn handle_logout() -> Response {
    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = StatusCode::NO_CONTENT;
    super::middleware::clear_session_cookie(&mut resp);
    resp
}

fn redirect(location: &str) -> Response {
    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = StatusCode::FOUND;
    if let Ok(v) = http::HeaderValue::from_str(location) {
        resp.headers_mut().insert(http::header::LOCATION, v);
    }
    resp
}

/// The short-lived cookie shape both flow cookies use (`Max-Age=300`).
fn flow_cookie(name: &str, value: &str, secure: bool) -> String {
    let mut c = format!("{name}={value}; Path=/; Max-Age=300; HttpOnly");
    if secure {
        c.push_str("; Secure");
    }
    c.push_str("; SameSite=Lax");
    c
}

/// 16 random bytes, hex encoded: the OAuth2 `state` parameter.
pub(crate) fn random_state() -> String {
    let mut b = [0u8; 16];
    rand::fill(&mut b[..]);
    hex::encode(b)
}

/// Reports whether the request reached forklift over TLS, directly or through a
/// terminating proxy.
pub(crate) fn is_secure(parts: &http::request::Parts) -> bool {
    parts.uri.scheme_str() == Some("https")
        || parts
            .headers
            .get("X-Forwarded-Proto")
            .and_then(|v| v.to_str().ok())
            == Some("https")
}

/// A reqwest-backed [`AsyncHttpClient`] for `openidconnect`.
pub struct HttpClient {
    inner: reqwest::Client,
}

impl HttpClient {
    /// Builds the client used for discovery, JWKS fetches and token exchange.
    pub fn new() -> Result<HttpClient, Error> {
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();
        let inner = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| Error::Oidc(format!("build oidc http client: {e}")))?;
        Ok(HttpClient { inner })
    }
}

/// Errors the [`HttpClient`] adapter can surface to `openidconnect`.
#[derive(Debug, thiserror::Error)]
pub enum HttpClientError {
    /// The request could not be sent or the body could not be read.
    #[error("{0}")]
    Reqwest(#[from] reqwest::Error),
    /// The response could not be rebuilt as an `http::Response`.
    #[error("{0}")]
    Http(#[from] http::Error),
}

impl<'c> AsyncHttpClient<'c> for HttpClient {
    type Error = HttpClientError;
    type Future = Pin<Box<dyn Future<Output = Result<HttpResponse, Self::Error>> + Send + 'c>>;

    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(async move {
            let req = reqwest::Request::try_from(request)?;
            let resp = self.inner.execute(req).await?;
            let status = resp.status();
            let headers = resp.headers().clone();
            let body = resp.bytes().await?.to_vec();
            let mut out = HttpResponse::new(body);
            *out.status_mut() = status;
            *out.headers_mut() = headers;
            Ok(out)
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use axum::body::Body;
    use axum::extract::State;
    use http::StatusCode;

    use crate::auth::oidc::{is_secure, random_state};
    use crate::auth::*;
    use crate::testing::auth::{new_test_service, request_parts};

    fn claim_provider() -> std::sync::Arc<OidcProvider> {
        OidcProvider::new_for_test(
            "forklift",
            "https://forklift.example.com/auth/callback",
            "https://idp.example.com/auth",
            "https://idp.example.com/token",
        )
    }

    #[test]
    fn extract_claims() {
        let o = claim_provider();
        let claims = serde_json::json!({
            "preferred_username": "alice",
            "email": "alice@example.com",
            "groups": ["devs", "platform", 42],
        });
        let (user, email, groups) = o
            .extract_claims(claims.as_object().unwrap())
            .expect("extract claims");
        assert_eq!(user, "alice");
        assert_eq!(email, "alice@example.com");
        assert_eq!(groups.len(), 2, "groups = {groups:?}");
        assert_eq!(groups[0], "devs");

        let missing = serde_json::json!({ "email": "x" });
        assert!(
            o.extract_claims(missing.as_object().unwrap()).is_err(),
            "missing username should error"
        );
    }

    #[tokio::test]
    async fn oidc_handlers_disabled() {
        let t = new_test_service().await; // no OIDC provider
        let resp = handle_login(
            State(std::sync::Arc::clone(&t.svc)),
            http::Request::builder()
                .uri("/auth/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "login without oidc");

        let resp = handle_callback(
            State(std::sync::Arc::clone(&t.svc)),
            http::Request::builder()
                .uri("/auth/callback")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "callback without oidc"
        );
    }

    #[test]
    fn random_state_and_secure() {
        let (a, b) = (random_state(), random_state());
        assert!(a != b && a.len() == 32, "random_state weak: {a:?} {b:?}");

        let parts = request_parts();
        assert!(!is_secure(&parts), "plain http should not be secure");
        let parts = http::Request::builder()
            .uri("/")
            .header("X-Forwarded-Proto", "https")
            .body(Body::empty())
            .unwrap()
            .into_parts()
            .0;
        assert!(is_secure(&parts), "forwarded https should be secure");
    }

    #[tokio::test]
    async fn service_flags() {
        let t = new_test_service().await;
        assert!(!t.svc.oidc_enabled(), "OIDC should be disabled");
        assert!(!t.svc.anonymous_read(), "anonymous read should default off");
    }
}
