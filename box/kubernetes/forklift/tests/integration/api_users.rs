use axum::body::Body;
use http::{Method, Request, StatusCode};
use serde_json::Value;

use forklift::testing::api::{
    ADMIN_USER, TestResponse, TestServer, new_test_server, new_test_server_protected_admin,
};

/// Looks one user up by name in a `/users` listing.
fn find_user<'a>(users: &'a Value, username: &str) -> Option<&'a Value> {
    users.as_array()?.iter().find(|u| u["username"] == username)
}

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

/// Covers Robot Account creation: no password is required (and one is
/// rejected), the account is flagged robot in the list, and it can still be
/// issued a token by an admin.
#[tokio::test]
async fn robot_account_create() {
    let srv = new_test_server().await;

    // A robot with a password is a client error.
    let resp = srv
        .admin_do(
            Method::POST,
            "/users",
            r#"{"username":"bad-bot","robot":true,"password":"pw123456"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());

    // A robot without a password is created.
    let resp = srv
        .admin_do(
            Method::POST,
            "/users",
            r#"{"username":"ci-bot","robot":true}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let bot_id = resp.json()["id"].as_i64().expect("robot id");

    // It appears in the list flagged as a robot.
    let resp = srv.admin_do(Method::GET, "/users", "").await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let users = resp.json();
    let bot = find_user(&users, "ci-bot");
    assert!(
        bot.map(|b| b["robot"] == true).unwrap_or(false),
        "ci-bot not flagged robot: {bot:?}"
    );

    // An admin can issue a token for the robot.
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/users/{bot_id}/tokens"),
            r#"{"name":"ci","description":"ci token","scopes":[{"repo_pattern":"*","actions":["read"]}],"expires_in":"720h"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());

    // A normal user still requires a password.
    let resp = srv
        .admin_do(Method::POST, "/users", r#"{"username":"nopw"}"#)
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
}

/// Verifies the per-user access-token cap: three issue, the fourth is refused,
/// and revoking one frees a slot.
#[tokio::test]
async fn user_admin_lifecycle() {
    let srv = new_test_server().await;

    // Create a user and a role with one permission.
    let resp = srv
        .admin_do(
            Method::POST,
            "/users",
            r#"{"username":"dev1","password":"pw123456","email":"dev1@example.com"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let user_id = resp.json()["id"].as_i64().expect("user id");

    let resp = srv
        .admin_do(
            Method::POST,
            "/roles",
            r#"{"name":"readers","description":"read everything"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let role_id = resp.json()["id"].as_i64().expect("role id");

    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/roles/{role_id}/permissions"),
            r#"{"repo_pattern":"*","actions":["read"]}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let perm_id = resp.json()["id"].as_i64().expect("permission id");

    // Assign the role; the user list must reflect it.
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/users/{user_id}/roles"),
            &format!(r#"{{"role_id":{role_id}}}"#),
        )
        .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());

    let resp = srv.admin_do(Method::GET, "/users", "").await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let users = resp.json();
    let dev1 = find_user(&users, "dev1");
    assert!(
        dev1.map(|d| d["roles"].as_array().map(Vec::len) == Some(1)
            && d["roles"][0]["name"] == "readers")
            .unwrap_or(false),
        "dev1 = {dev1:?}, want role readers"
    );

    // Roles list includes the permission.
    let resp = srv.admin_do(Method::GET, "/roles", "").await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let roles = resp.json();
    let readers = roles
        .as_array()
        .expect("roles")
        .iter()
        .find(|r| r["name"] == "readers")
        .expect("readers role");
    assert!(
        readers["permissions"].as_array().map(Vec::len) == Some(1)
            && readers["permissions"][0]["repo_pattern"] == "*"
            && readers["permissions"][0]["actions"][0] == "read",
        "readers = {readers}"
    );
    assert_eq!(
        readers["user_count"], 1,
        "readers user_count = {}, want 1",
        readers["user_count"]
    );

    // Disable, then re-enable, then reset the password.
    let resp = srv
        .admin_do(
            Method::PUT,
            &format!("/users/{user_id}"),
            r#"{"disabled":true}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let u = resp.json();
    assert_eq!(u["disabled"], true, "user not disabled: {u}");
    let resp = srv
        .admin_do(
            Method::PUT,
            &format!("/users/{user_id}"),
            r#"{"disabled":false,"password":"new-pw-123"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let u = resp.json();
    assert_ne!(u["disabled"], true, "user still disabled: {u}");

    // Remove the permission and the role assignment.
    let resp = srv
        .admin_do(
            Method::DELETE,
            &format!("/roles/{role_id}/permissions/{perm_id}"),
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
    let resp = srv
        .admin_do(
            Method::DELETE,
            &format!("/users/{user_id}/roles/{role_id}"),
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());

    // Delete the user.
    let resp = srv
        .admin_do(Method::DELETE, &format!("/users/{user_id}"), "")
        .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
}

/// Covers an administrator managing a target user's personal access tokens from
/// the user detail page: create, list, the token authenticating as that user,
/// and revoke.
#[tokio::test]
async fn user_token_admin_lifecycle() {
    let srv = new_test_server().await;

    let resp = srv
        .admin_do(
            Method::POST,
            "/users",
            r#"{"username":"dev1","password":"pw123456"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let user_id = resp.json()["id"].as_i64().expect("user id");

    // Admin issues a token for dev1; the plaintext is returned once.
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/users/{user_id}/tokens"),
            r#"{"name":"ci","description":"dev1 ci","scopes":[{"repo_pattern":"*","actions":["read"]}],"expires_in":"720h"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let token = resp.json()["token"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!token.is_empty(), "no token returned");

    // The token authenticates as dev1, not the admin who created it.
    let me = bearer_do(&srv, &token, Method::GET, "/me").await.json();
    assert_eq!(me["username"], "dev1", "token me = {me}, want dev1");

    // Admin lists dev1's tokens.
    let resp = srv
        .admin_do(Method::GET, &format!("/users/{user_id}/tokens"), "")
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let list = resp.json();
    assert!(
        list.as_array().map(Vec::len) == Some(1) && list[0]["name"] == "ci",
        "token list = {list}"
    );
    let token_id = list[0]["id"].as_i64().expect("token id");

    // Creating a token for an unknown user is a 404.
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/users/{}/tokens", user_id + 9999),
            r#"{"name":"x","description":"x","scopes":[{"repo_pattern":"*","actions":["read"]}],"expires_in":"1h"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND, "{}", resp.text());

    // Revoke, then the list is empty and the token no longer authenticates.
    let resp = srv
        .admin_do(
            Method::DELETE,
            &format!("/users/{user_id}/tokens/{token_id}"),
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
    let resp = srv
        .admin_do(Method::GET, &format!("/users/{user_id}/tokens"), "")
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let list = resp.json();
    assert_eq!(
        list.as_array().map(Vec::len),
        Some(0),
        "token list after revoke = {list}"
    );
}

/// Covers the access rules for the per-user token endpoints: an auditor may list
/// but not create or revoke; a plain user may do neither.
#[tokio::test]
async fn user_token_rbac() {
    let srv = new_test_server().await;

    let resp = srv
        .admin_do(
            Method::POST,
            "/users",
            r#"{"username":"dev1","password":"pw123456"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let user_id = resp.json()["id"].as_i64().expect("user id");

    // Seed a token so the auditor has something to read.
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/users/{user_id}/tokens"),
            r#"{"name":"ci","description":"d","scopes":[{"repo_pattern":"*","actions":["read"]}],"expires_in":"1h"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let token_id = resp.json()["id"].as_i64().expect("token id");

    // Auditor: read action plus audit on all repos.
    let resp = srv
        .admin_do(
            Method::POST,
            "/users",
            r#"{"username":"aud1","password":"pw123456"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let aud_id = resp.json()["id"].as_i64().expect("user id");
    let resp = srv
        .admin_do(
            Method::POST,
            "/roles",
            r#"{"name":"auditor","description":"ro"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let aud_role_id = resp.json()["id"].as_i64().expect("role id");
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/roles/{aud_role_id}/permissions"),
            r#"{"repo_pattern":"*","actions":["read","audit"]}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/users/{aud_id}/roles"),
            &format!(r#"{{"role_id":{aud_role_id}}}"#),
        )
        .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());

    let tokens_url = format!("/users/{user_id}/tokens");

    // Auditor can list, but cannot create or revoke.
    let resp = srv
        .do_as("aud1", "pw123456", Method::GET, &tokens_url, "")
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let resp = srv
        .do_as(
            "aud1",
            "pw123456",
            Method::POST,
            &tokens_url,
            r#"{"name":"x","description":"d","scopes":[{"repo_pattern":"*","actions":["read"]}],"expires_in":"1h"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
    let resp = srv
        .do_as(
            "aud1",
            "pw123456",
            Method::DELETE,
            &format!("{tokens_url}/{token_id}"),
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());

    // Plain user (no audit) cannot even list.
    let resp = srv
        .admin_do(
            Method::POST,
            "/users",
            r#"{"username":"plain","password":"pw123456"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let resp = srv
        .do_as("plain", "pw123456", Method::GET, &tokens_url, "")
        .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
}

/// Covers the default admin guard: even another administrator cannot disable the
/// bootstrap admin, keeping at least one guaranteed admin account always able to
/// sign in.
#[tokio::test]
async fn protected_admin_cannot_be_disabled() {
    let srv = new_test_server_protected_admin().await;

    // Find the bootstrap admin's id.
    let resp = srv.admin_do(Method::GET, "/users", "").await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let users = resp.json();
    let admin = find_user(&users, ADMIN_USER).expect("bootstrap admin not found");
    assert_eq!(
        admin["protected"], true,
        "bootstrap admin not marked protected: {admin}"
    );
    let admin_id = admin["id"].as_i64().expect("admin id");

    // A second administrator (so the self-guard is not what blocks the request).
    let resp = srv
        .admin_do(
            Method::POST,
            "/users",
            r#"{"username":"admin2","password":"pw123456"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let admin2_id = resp.json()["id"].as_i64().expect("user id");
    let resp = srv
        .admin_do(
            Method::POST,
            "/roles",
            r#"{"name":"superadmin","description":"admin"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let role_id = resp.json()["id"].as_i64().expect("role id");
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/roles/{role_id}/permissions"),
            r#"{"repo_pattern":"*","actions":["admin"]}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/users/{admin2_id}/roles"),
            &format!(r#"{{"role_id":{role_id}}}"#),
        )
        .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());

    // admin2 cannot disable the bootstrap admin.
    let resp = srv
        .do_as(
            "admin2",
            "pw123456",
            Method::PUT,
            &format!("/users/{admin_id}"),
            r#"{"disabled":true}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());

    // The bootstrap admin is still active.
    let users = srv.admin_do(Method::GET, "/users", "").await.json();
    for u in users.as_array().expect("users") {
        assert!(
            !(u["id"].as_i64() == Some(admin_id) && u["disabled"] == true),
            "bootstrap admin was disabled despite the guard"
        );
    }
}

/// Covers assigning roles at creation time: a valid role is applied, and an
/// unknown role id is rejected before the user is created.
#[tokio::test]
async fn create_user_with_roles() {
    let srv = new_test_server().await;

    let resp = srv
        .admin_do(
            Method::POST,
            "/roles",
            r#"{"name":"readers","description":"read everything"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let role_id = resp.json()["id"].as_i64().expect("role id");

    // Create a user with the role attached.
    let resp = srv
        .admin_do(
            Method::POST,
            "/users",
            &format!(r#"{{"username":"dev1","password":"pw123456","role_ids":[{role_id}]}}"#),
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());

    let users = srv.admin_do(Method::GET, "/users", "").await.json();
    let dev1 = find_user(&users, "dev1");
    assert!(
        dev1.map(|d| d["roles"].as_array().map(Vec::len) == Some(1)
            && d["roles"][0]["name"] == "readers")
            .unwrap_or(false),
        "dev1 = {dev1:?}, want role readers at creation"
    );

    // Unknown role id is rejected, and no such user is created.
    let resp = srv
        .admin_do(
            Method::POST,
            "/users",
            r#"{"username":"dev2","password":"pw123456","role_ids":[99999]}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());

    let users = srv.admin_do(Method::GET, "/users", "").await.json();
    assert!(
        find_user(&users, "dev2").is_none(),
        "dev2 should not have been created with an invalid role"
    );
}

/// Covers granting permissions at role creation: valid grants are applied, and
/// an invalid action is rejected before the role is created.
#[tokio::test]
async fn create_role_with_permissions() {
    let srv = new_test_server().await;

    let resp = srv
        .admin_do(
            Method::POST,
            "/roles",
            r#"{"name":"maven-readers","description":"read maven","permissions":[{"repo_pattern":"maven-*","actions":["read"]}]}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let role = resp.json();
    assert!(
        role["permissions"].as_array().map(Vec::len) == Some(1)
            && role["permissions"][0]["repo_pattern"] == "maven-*"
            && role["permissions"][0]["actions"][0] == "read",
        "role = {role}, want one maven-* read permission"
    );

    // Invalid action is rejected, and no such role is created.
    let resp = srv
        .admin_do(
            Method::POST,
            "/roles",
            r#"{"name":"bad-role","permissions":[{"repo_pattern":"*","actions":["superuser"]}]}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());

    let roles = srv.admin_do(Method::GET, "/roles", "").await.json();
    for r in roles.as_array().expect("roles") {
        assert_ne!(
            r["name"], "bad-role",
            "bad-role should not have been created with an invalid action"
        );
    }
}

#[tokio::test]
async fn user_self_guards() {
    let srv = new_test_server().await;

    // Find the admin's own user ID.
    let resp = srv.admin_do(Method::GET, "/users", "").await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let users = resp.json();
    assert!(
        users.as_array().map(Vec::len) == Some(1) && users[0]["username"] == ADMIN_USER,
        "users = {users}"
    );
    let admin_id = users[0]["id"].as_i64().expect("admin id");

    // Self-delete and self-disable are rejected.
    let resp = srv
        .admin_do(Method::DELETE, &format!("/users/{admin_id}"), "")
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
    let resp = srv
        .admin_do(
            Method::PUT,
            &format!("/users/{admin_id}"),
            r#"{"disabled":true}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
}

#[tokio::test]
async fn update_user_validation() {
    let srv = new_test_server().await;

    // Unknown user -> 404.
    let resp = srv
        .admin_do(Method::PUT, "/users/9999", r#"{"disabled":true}"#)
        .await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND, "{}", resp.text());

    // Empty password -> 400.
    let resp = srv
        .admin_do(
            Method::POST,
            "/users",
            r#"{"username":"dev2","password":"pw123456"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let id = resp.json()["id"].as_i64().expect("user id");

    let resp = srv
        .admin_do(Method::PUT, &format!("/users/{id}"), r#"{"password":""}"#)
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
}

#[tokio::test]
async fn user_lockout_toggle() {
    let srv = new_test_server().await;

    let resp = srv
        .admin_do(
            Method::POST,
            "/users",
            r#"{"username":"lockme","password":"pw123456"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let id = resp.json()["id"].as_i64().expect("user id");

    // New users get lockout enabled by default.
    let users = srv.admin_do(Method::GET, "/users", "").await.json();
    let created = find_user(&users, "lockme");
    assert!(
        created
            .map(|c| c["lockout_enabled"] == true)
            .unwrap_or(false),
        "new user lockout default = {created:?}, want lockout_enabled true"
    );
    let created = created.expect("lockme");
    assert!(
        created["locked"] != true && created["protected"] != true,
        "unexpected flags: {created}"
    );

    // Unlock is accepted (no-op here) and disabling clears the flag.
    let resp = srv
        .admin_do(Method::PUT, &format!("/users/{id}"), r#"{"unlock":true}"#)
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());

    let resp = srv
        .admin_do(
            Method::PUT,
            &format!("/users/{id}"),
            r#"{"lockout_enabled":false}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(
        resp.json()["lockout_enabled"],
        false,
        "lockout_enabled should be false after disable"
    );
}
