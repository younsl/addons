//! Stateless signed session cookies.
//!

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use hmac::{Mac as _, digest::KeyInit};
use subtle::ConstantTimeEq as _;

use super::Error;

type HmacSha256 = hmac::Hmac<sha2::Sha256>;

/// The payload stored in a signed session cookie. Sessions are stateless (no
/// server-side store), which keeps HA failover trivial as long as all replicas
/// share the same signing secret.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionData {
    #[serde(rename = "u", default)]
    pub username: String,
    #[serde(rename = "s", default)]
    pub source: String,
    #[serde(rename = "g", default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    #[serde(rename = "c", default)]
    pub csrf: String,
    #[serde(rename = "e", default)]
    pub expires: i64,
    /// Names the administrator who started an impersonated session. Empty for an
    /// ordinary sign-in. It is what lets the UI show the banner and what the
    /// stop endpoint restores the session to.
    #[serde(rename = "i", default, skip_serializing_if = "String::is_empty")]
    pub impersonator: String,
}

/// Signs and verifies session cookies with HMAC-SHA256.
pub struct SessionCodec {
    secret: Vec<u8>,
    ttl: Duration,
    pub(crate) now: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

impl SessionCodec {
    /// Creates a codec. The secret must be shared across replicas.
    pub fn new(secret: Vec<u8>, ttl: Duration) -> SessionCodec {
        SessionCodec {
            secret,
            ttl,
            now: Arc::new(Utc::now),
        }
    }

    /// Produces a signed cookie value for a principal.
    pub fn encode(&self, username: &str, source: &str, groups: &[String]) -> Result<String, Error> {
        self.encode_inner(username, source, groups, "", self.ttl)
    }

    /// Produces a signed cookie value that acts as `username` while recording
    /// the administrator who started it. Groups are deliberately not carried
    /// over: the administrator cannot know the target's live OIDC group claims,
    /// so an impersonated session resolves only the roles persisted for that
    /// user. `ttl` is clamped to the codec's normal session lifetime.
    pub fn encode_impersonated(
        &self,
        username: &str,
        source: &str,
        impersonator: &str,
        ttl: Duration,
    ) -> Result<String, Error> {
        let ttl = if ttl.is_zero() || ttl > self.ttl {
            self.ttl
        } else {
            ttl
        };
        self.encode_inner(username, source, &[], impersonator, ttl)
    }

    fn encode_inner(
        &self,
        username: &str,
        source: &str,
        groups: &[String],
        impersonator: &str,
        ttl: Duration,
    ) -> Result<String, Error> {
        let mut csrf_bytes = [0u8; 32];
        rand::fill(&mut csrf_bytes[..]);
        let expires = ((self.now)()
            + chrono::Duration::from_std(ttl).map_err(|e| Error::Other(e.to_string()))?)
        .timestamp();
        let payload = serde_json::to_vec(&SessionData {
            username: username.to_string(),
            source: source.to_string(),
            groups: groups.to_vec(),
            csrf: URL_SAFE_NO_PAD.encode(csrf_bytes),
            expires,
            impersonator: impersonator.to_string(),
        })?;
        let body = URL_SAFE_NO_PAD.encode(payload);
        let sig = self.sign(&body);
        Ok(format!("{body}.{sig}"))
    }

    /// Verifies a cookie value and returns its payload.
    pub fn decode(&self, value: &str) -> Result<SessionData, Error> {
        let (body, sig) = value
            .split_once('.')
            .ok_or_else(|| Error::Session("malformed session".into()))?;
        if !bool::from(sig.as_bytes().ct_eq(self.sign(body).as_bytes())) {
            return Err(Error::Session("bad session signature".into()));
        }
        let raw = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|e| Error::Session(e.to_string()))?;
        let d: SessionData = serde_json::from_slice(&raw)?;
        if (self.now)().timestamp() > d.expires {
            return Err(Error::Session("session expired".into()));
        }
        Ok(d)
    }

    fn sign(&self, body: &str) -> String {
        let mut mac = <HmacSha256 as KeyInit>::new_from_slice(&self.secret)
            .expect("HMAC accepts a key of any length");
        mac.update(body.as_bytes());
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    }
}

/// Constant-time comparison of the session's CSRF claim against the header a
/// mutating request presented.
pub(crate) fn valid_csrf(data: &SessionData, token: &str) -> bool {
    !data.csrf.is_empty() && bool::from(data.csrf.as_bytes().ct_eq(token.as_bytes()))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::routing::get;
    use http::{Request, StatusCode};
    use tower::ServiceExt as _;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::auth::*;
    use crate::meta;
    use crate::testing::auth::{
        TestService, cookie_parts, new_test_service, request_parts, to_request,
    };

    /// Creates an enabled local user with the given password.
    async fn mk_local_user(store: &Arc<meta::Store>, username: &str, password: &str) -> meta::User {
        let hash = hash_password(password).expect("hash");
        store
            .create_user(meta::User {
                username: username.into(),
                password_hash: hash,
                source: meta::SOURCE_LOCAL.into(),
                ..Default::default()
            })
            .await
            .expect("create user")
    }

    /// Builds a request carrying a session cookie issued by `svc`, exercising
    /// [`set_session_cookie`] on the way.
    fn session_request(
        svc: &Service,
        username: &str,
        source: &str,
        groups: &[String],
    ) -> http::request::Parts {
        let val = svc
            .issue_session(username, source, groups)
            .expect("issue session");
        let mut resp = axum::response::Response::new(Body::empty());
        set_session_cookie(&mut resp, &val, true);
        let set_cookie = resp
            .headers()
            .get(http::header::SET_COOKIE)
            .expect("Set-Cookie")
            .to_str()
            .unwrap()
            .to_string();
        let pair = set_cookie.split(';').next().unwrap().to_string();
        Request::builder()
            .uri("/")
            .header(http::header::COOKIE, pair)
            .body(Body::empty())
            .unwrap()
            .into_parts()
            .0
    }

    #[tokio::test]
    async fn session_cookie_resolve() {
        let t = new_test_service().await;
        let u = mk_local_user(&t.store, "alice", "pw123456").await;

        let parts = session_request(&t.svc, "alice", meta::SOURCE_LOCAL, &[]);
        let p = t
            .svc
            .resolve(&parts)
            .await
            .expect("resolve")
            .expect("resolve");
        assert_eq!(p.username, "alice");
        assert_eq!(p.source, meta::SOURCE_LOCAL);

        // A disabled user's session no longer resolves.
        t.store.set_user_disabled(u.id, true).await.unwrap();
        let parts = session_request(&t.svc, "alice", meta::SOURCE_LOCAL, &[]);
        assert!(
            t.svc.resolve(&parts).await.unwrap().is_none(),
            "disabled user resolved"
        );

        // A session naming an unknown user resolves to anonymous.
        let parts = session_request(&t.svc, "ghost", meta::SOURCE_LOCAL, &[]);
        assert!(
            t.svc.resolve(&parts).await.unwrap().is_none(),
            "unknown user resolved"
        );

        // A tampered cookie value is ignored.
        let parts = cookie_parts("forklift_session", "garbage");
        assert!(
            t.svc.resolve(&parts).await.unwrap().is_none(),
            "tampered cookie resolved"
        );
    }

    #[tokio::test]
    async fn session_csrf_token() {
        let t = new_test_service().await;
        mk_local_user(&t.store, "csrf-user", "pw123456").await;
        let mut parts = session_request(&t.svc, "csrf-user", meta::SOURCE_LOCAL, &[]);
        let token = t
            .svc
            .csrf_token(&parts)
            .expect("signed session did not expose a CSRF token");
        assert!(!token.is_empty());
        assert!(
            !t.svc.validate_csrf(&parts),
            "missing CSRF header was accepted"
        );
        parts.headers.insert("X-CSRF-Token", token.parse().unwrap());
        assert!(
            t.svc.validate_csrf(&parts),
            "matching signed CSRF token was rejected"
        );
        parts
            .headers
            .insert("X-CSRF-Token", format!("{token}x").parse().unwrap());
        assert!(
            !t.svc.validate_csrf(&parts),
            "mismatched CSRF token was accepted"
        );
        parts.headers.insert(
            http::header::AUTHORIZATION,
            "Bearer forklift_pat_invalid".parse().unwrap(),
        );
        assert!(
            t.svc.validate_csrf(&parts),
            "Authorization-authenticated mutation should not require CSRF"
        );
    }

    #[tokio::test]
    async fn session_groups_grant_mapped_roles() {
        let t = new_test_service().await;
        mk_local_user(&t.store, "dev", "pw123456").await;

        let role = t
            .store
            .create_role(meta::Role {
                name: "readers".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        t.store
            .add_permission(meta::Permission {
                role_id: role.id,
                repo_pattern: "*".into(),
                actions: "read".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        t.store
            .create_group_mapping("team-x", role.id)
            .await
            .unwrap();

        // Without groups the user has no permissions; with the mapped group the
        // session grants read.
        let parts = session_request(&t.svc, "dev", meta::SOURCE_OIDC, &[]);
        let p = t.svc.resolve(&parts).await.unwrap();
        assert!(
            p.is_some_and(|p| !p.can("any-repo", ACTION_READ)),
            "groupless session should have no perms"
        );
        let parts = session_request(&t.svc, "dev", meta::SOURCE_OIDC, &["team-x".to_string()]);
        let p = t.svc.resolve(&parts).await.unwrap();
        assert!(
            p.is_some_and(|p| p.can("any-repo", ACTION_READ)),
            "group-mapped session should read"
        );
    }

    #[tokio::test]
    async fn require_auth_middleware() {
        let t = new_test_service().await;
        mk_local_user(&t.store, "alice", "pw123456").await;

        let app = || {
            Router::new()
                .route("/", get(|| async { StatusCode::OK }))
                .layer(axum::middleware::from_fn(require_auth))
                .layer(axum::middleware::from_fn_with_state(
                    Arc::clone(&t.svc),
                    crate::auth::middleware,
                ))
        };

        let resp = app().oneshot(to_request(request_parts())).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "anonymous");

        let parts = session_request(&t.svc, "alice", meta::SOURCE_LOCAL, &[]);
        let resp = app().oneshot(to_request(parts)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "session");
    }

    #[tokio::test]
    async fn handle_logout_clears_cookie() {
        let resp = handle_logout().await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT, "logout");
        let cookies: Vec<&str> = resp
            .headers()
            .get_all(http::header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(cookies.len(), 1, "cookie not cleared: {cookies:?}");
        assert!(
            cookies[0].starts_with("forklift_session=") && cookies[0].contains("Max-Age=0"),
            "cookie not cleared: {cookies:?}"
        );
    }

    #[tokio::test]
    async fn oidc_handlers_not_configured() {
        let t = new_test_service().await;
        let resp = handle_login(
            State(Arc::clone(&t.svc)),
            Request::builder()
                .uri("/auth/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "login without oidc");
        let resp = handle_callback(
            State(Arc::clone(&t.svc)),
            Request::builder()
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

    /// Builds a Service with a hand-constructed provider whose token endpoint points
    /// at `token_url`, bypassing issuer discovery.
    async fn oidc_test_service(token_url: &str) -> TestService {
        crate::testing::auth::new_test_service_with(Options {
            session_secret: b"test-secret-test-secret-test-secret".to_vec(),
            oidc: Some(OidcProvider::new_for_test(
                "forklift",
                "https://forklift.example.com/auth/callback",
                "https://idp.example.com/auth",
                token_url,
            )),
            ..Default::default()
        })
        .await
    }

    #[tokio::test]
    async fn handle_login_redirects_to_idp() {
        let t = oidc_test_service("https://idp.example.com/token").await;

        let resp = handle_login(
            State(Arc::clone(&t.svc)),
            Request::builder()
                .uri("/auth/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FOUND, "login");
        let loc = resp
            .headers()
            .get(http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            loc.starts_with("https://idp.example.com/auth") && loc.contains("state="),
            "redirect = {loc:?}"
        );
        let cookies: Vec<String> = resp
            .headers()
            .get_all(http::header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        let state = cookies
            .iter()
            .find(|c| c.starts_with("forklift_oidc_state="))
            .unwrap_or_else(|| panic!("state cookie missing: {cookies:?}"));
        assert!(
            !state
                .trim_start_matches("forklift_oidc_state=")
                .starts_with(';'),
            "state cookie is empty: {state}"
        );
    }

    #[tokio::test]
    async fn handle_callback_state_and_exchange() {
        // Token endpoint that returns a token without an id_token.
        let idp = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at",
                "token_type": "Bearer",
            })))
            .mount(&idp)
            .await;

        let t = oidc_test_service(&format!("{}/token", idp.uri())).await;

        let callback = |uri: &str, state_cookie: Option<&str>| {
            let mut b = Request::builder().uri(uri);
            if let Some(c) = state_cookie {
                b = b.header(http::header::COOKIE, format!("forklift_oidc_state={c}"));
            }
            b.body(Body::empty()).unwrap()
        };

        // Missing state cookie -> 400.
        let resp = handle_callback(
            State(Arc::clone(&t.svc)),
            callback("/auth/callback?state=x&code=c", None),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "missing state cookie"
        );

        // State mismatch -> 400.
        let resp = handle_callback(
            State(Arc::clone(&t.svc)),
            callback("/auth/callback?state=other&code=c", Some("expected")),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "state mismatch");

        // Valid state but the exchange yields no id_token -> 502.
        let resp = handle_callback(
            State(Arc::clone(&t.svc)),
            callback("/auth/callback?state=s1&code=c", Some("s1")),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY, "no id_token");

        // Exchange failure (token endpoint errors) -> 502.
        let idp_down = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom\n"))
            .mount(&idp_down)
            .await;
        let t = oidc_test_service(&format!("{}/token", idp_down.uri())).await;
        let resp = handle_callback(
            State(Arc::clone(&t.svc)),
            callback("/auth/callback?state=s1&code=c", Some("s1")),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY, "exchange failure");
    }

    #[tokio::test]
    async fn new_oidc_discovery() {
        // Minimal OIDC discovery document; the issuer must match the server URL.
        let idp = MockServer::start().await;
        let issuer = idp.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/auth"),
                "token_endpoint": format!("{issuer}/token"),
                "jwks_uri": format!("{issuer}/keys"),
                "response_types_supported": ["code"],
                "subject_types_supported": ["public"],
                "id_token_signing_alg_values_supported": ["RS256"],
            })))
            .mount(&idp)
            .await;
        Mock::given(method("GET"))
            .and(path("/keys"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "keys": [] })),
            )
            .mount(&idp)
            .await;

        let p = OidcProvider::new(OidcParams {
            issuer_url: issuer.clone(),
            client_id: "forklift".into(),
            ..Default::default()
        })
        .await
        .expect("new oidc");
        assert_eq!(p.username_claim, "preferred_username", "claim defaults");
        assert_eq!(p.groups_claim, "groups", "claim defaults");

        // A malformed token fails verification (error path of verify).
        assert!(p.verify("not-a-jwt").is_err(), "garbage token verified");

        // Unreachable issuer fails discovery.
        assert!(
            OidcProvider::new(OidcParams {
                issuer_url: "http://127.0.0.1:1/nope".into(),
                ..Default::default()
            })
            .await
            .is_err(),
            "expected discovery error"
        );
    }
}
