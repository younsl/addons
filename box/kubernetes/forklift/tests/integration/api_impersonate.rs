use http::{Method, StatusCode};

use forklift::testing::api::{ADMIN_PASS, ADMIN_USER, TestResponse, TestServer, new_test_server};

struct SessionClient {
    cookie: String,
}

impl SessionClient {
    /// Signs in over `/login` and keeps the resulting session cookie, which is
    /// what impersonation requires (Basic auth and personal access tokens cannot
    /// start one).
    async fn new(srv: &TestServer, username: &str, password: &str) -> SessionClient {
        let resp = srv
            .anon_do(
                Method::POST,
                "/login",
                &format!(r#"{{"username":"{username}","password":"{password}"}}"#),
            )
            .await;
        assert_eq!(resp.status, StatusCode::OK, "login: {}", resp.text());
        let mut client = SessionClient {
            cookie: String::new(),
        };
        client.absorb(&resp);
        assert!(!client.cookie.is_empty(), "login set no session cookie");
        client
    }

    /// Records the session cookie from a response, the jar's only job here.
    fn absorb(&mut self, resp: &TestResponse) {
        for value in resp.headers.get_all(http::header::SET_COOKIE) {
            let Ok(value) = value.to_str() else { continue };
            let pair = value.split(';').next().unwrap_or_default();
            if let Some(rest) = pair.strip_prefix("forklift_session=")
                && !rest.is_empty()
            {
                self.cookie = pair.to_string();
            }
        }
    }

    async fn do_req(
        &mut self,
        srv: &TestServer,
        method: Method,
        uri: &str,
        body: &str,
    ) -> TestResponse {
        let request = http::Request::builder()
            .method(method)
            .uri(uri)
            .header(http::header::COOKIE, &self.cookie)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .expect("build request");
        let resp = srv.send(request).await;
        self.absorb(&resp);
        resp
    }
}

/// Creates a password user and returns its id.
async fn create_local_user(srv: &TestServer, username: &str, password: &str) -> i64 {
    let resp = srv
        .admin_do(
            Method::POST,
            "/users",
            &format!(r#"{{"username":"{username}","password":"{password}"}}"#),
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    resp.json()["id"].as_i64().expect("user id")
}

/// Covers the happy path: an admin session becomes the target user (with the
/// target's own, non-admin permissions), /me reports who is behind it, and
/// stopping restores the admin's own session.
#[tokio::test]
async fn impersonate_round_trip() {
    let srv = new_test_server().await;
    let id = create_local_user(&srv, "alice", "alicepw").await;

    let mut admin = SessionClient::new(&srv, ADMIN_USER, ADMIN_PASS).await;
    let resp = admin
        .do_req(
            &srv,
            Method::POST,
            &format!("/users/{id}/impersonate"),
            r#"{"reason":"reproducing a reported 403 on the npm proxy"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let started = resp.json();
    assert!(
        started["username"] == "alice" && started["impersonator"] == ADMIN_USER,
        "unexpected impersonate response: {started}"
    );

    let resp = admin.do_req(&srv, Method::GET, "/me", "").await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let me = resp.json();
    assert!(
        me["username"] == "alice" && me["impersonator"] == ADMIN_USER,
        "me during impersonation = {me}"
    );
    // The impersonated session carries alice's permissions, not the admin's.
    assert_ne!(
        me["admin"], true,
        "impersonated session kept admin rights: {me}"
    );
    let resp = admin.do_req(&srv, Method::GET, "/users", "").await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());

    // No chaining while one is active. alice is not an admin, so the route
    // rejects first.
    let resp = admin
        .do_req(
            &srv,
            Method::POST,
            &format!("/users/{id}/impersonate"),
            r#"{"reason":"a second impersonation attempt"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());

    let resp = admin
        .do_req(&srv, Method::POST, "/impersonate/stop", "")
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());

    let resp = admin.do_req(&srv, Method::GET, "/me", "").await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let me = resp.json();
    assert!(
        me["username"] == ADMIN_USER && me["admin"] == true,
        "me after stop = {me}"
    );
    assert!(
        me.get("impersonator").is_none(),
        "impersonator still reported after stop: {me}"
    );

    // Stopping when not impersonating is a client error.
    let resp = admin
        .do_req(&srv, Method::POST, "/impersonate/stop", "")
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
}

/// Verifies an admin impersonating another admin cannot start a second
/// impersonation from inside the first.
#[tokio::test]
async fn impersonate_no_chaining() {
    let srv = new_test_server().await;
    let other_admin = create_local_user(&srv, "admin2", "admin2pw").await;
    let target = create_local_user(&srv, "bob", "bobpw123").await;

    // Give admin2 the administrator role so the impersonated session still
    // passes the admin route guard.
    let resp = srv.admin_do(Method::GET, "/roles", "").await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let roles = resp.json();
    let admin_role_id = roles
        .as_array()
        .expect("roles")
        .iter()
        .find(|role| role["name"] == "administrator")
        .and_then(|role| role["id"].as_i64())
        .expect("administrator role not found");
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/users/{other_admin}/roles"),
            &format!(r#"{{"role_id":{admin_role_id}}}"#),
        )
        .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());

    let mut admin = SessionClient::new(&srv, ADMIN_USER, ADMIN_PASS).await;
    let resp = admin
        .do_req(
            &srv,
            Method::POST,
            &format!("/users/{other_admin}/impersonate"),
            r#"{"reason":"checking what the other administrator sees"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());

    let resp = admin
        .do_req(
            &srv,
            Method::POST,
            &format!("/users/{target}/impersonate"),
            r#"{"reason":"chaining into a third identity"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CONFLICT, "{}", resp.text());
}

/// Covers every refusal: a missing or too-short reason, self, robot and disabled
/// targets, a non-session credential, and a non-admin caller.
#[tokio::test]
async fn impersonate_rejections() {
    let srv = new_test_server().await;
    let alice = create_local_user(&srv, "alice", "alicepw").await;

    let resp = srv
        .admin_do(
            Method::POST,
            "/users",
            r#"{"username":"ci-bot","robot":true}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let robot = resp.json()["id"].as_i64().expect("robot id");

    let disabled = create_local_user(&srv, "dave", "davepw12").await;
    let resp = srv
        .admin_do(
            Method::PUT,
            &format!("/users/{disabled}"),
            r#"{"disabled":true}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());

    let resp = srv.admin_do(Method::GET, "/users", "").await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let users = resp.json();
    let admin_id = users
        .as_array()
        .expect("users")
        .iter()
        .find(|u| u["username"] == ADMIN_USER)
        .and_then(|u| u["id"].as_i64())
        .expect("admin id");

    let mut admin = SessionClient::new(&srv, ADMIN_USER, ADMIN_PASS).await;
    let impersonate = format!("/users/{alice}/impersonate");

    for (name, uri, body, want) in [
        (
            "no reason",
            impersonate.clone(),
            "{}",
            StatusCode::BAD_REQUEST,
        ),
        (
            "short reason",
            impersonate.clone(),
            r#"{"reason":"support"}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            "self",
            format!("/users/{admin_id}/impersonate"),
            r#"{"reason":"impersonating myself"}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            "robot",
            format!("/users/{robot}/impersonate"),
            r#"{"reason":"looking at the ci robot"}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            "disabled",
            format!("/users/{disabled}/impersonate"),
            r#"{"reason":"looking at a disabled account"}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            "unknown user",
            "/users/99999/impersonate".to_string(),
            r#"{"reason":"a user that does not exist"}"#,
            StatusCode::NOT_FOUND,
        ),
    ] {
        let resp = admin.do_req(&srv, Method::POST, &uri, body).await;
        assert_eq!(resp.status, want, "{name}: {}", resp.text());
    }

    // Basic auth is admin but not an interactive session.
    let resp = srv
        .admin_do(
            Method::POST,
            &impersonate,
            r#"{"reason":"starting one from a script"}"#,
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::FORBIDDEN,
        "basic auth: {}",
        resp.text()
    );

    // A signed-in non-admin never reaches the handler.
    let mut user = SessionClient::new(&srv, "alice", "alicepw").await;
    let resp = user
        .do_req(
            &srv,
            Method::POST,
            &impersonate,
            r#"{"reason":"a non-admin trying it"}"#,
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::FORBIDDEN,
        "non-admin: {}",
        resp.text()
    );
}
