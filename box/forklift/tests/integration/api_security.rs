use http::{Method, StatusCode};

use forklift::testing::api::{mk_proxy_repo, new_test_server};

/// Covers the non-admin policy-write flow: a role with the security action on a
/// repo pattern may rewrite the security policy of matching repositories through
/// PUT /repositories/{id}/security, and gets nothing else -- the admin-only
/// repository update stays closed, and the upstream URL and credentials are
/// unreachable even when the client puts them in the security body.
#[tokio::test]
async fn security_action_for_security_engineers() {
    let srv = new_test_server().await;
    let npm_id = mk_proxy_repo(&srv, "npm-gated").await;

    // pypi proxy outside the security role's npm-* pattern.
    let resp = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"pypi-gated","format":"pypi","type":"proxy","upstream_url":"https://pypi.org/simple"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let pypi_id = resp.json()["id"].as_i64().expect("repository id");

    let resp = srv
        .admin_do(
            Method::POST,
            "/users",
            r#"{"username":"sec1","password":"pw123456"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let user_id = resp.json()["id"].as_i64().expect("user id");
    let resp = srv
        .admin_do(
            Method::POST,
            "/roles",
            r#"{"name":"secops","description":"security engineer"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let role_id = resp.json()["id"].as_i64().expect("role id");
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/roles/{role_id}/permissions"),
            r#"{"repo_pattern":"npm-*","actions":["read","security"]}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/users/{user_id}/roles"),
            &format!(r#"{{"role_id":{role_id}}}"#),
        )
        .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());

    // /me reports the security capability without admin.
    let resp = srv.do_as("sec1", "pw123456", Method::GET, "/me", "").await;
    let me = resp.json();
    assert!(me["admin"] == false && me["security"] == true, "me = {me}");

    // Policy edits on a matching repository succeed and are persisted.
    let resp = srv
        .do_as(
            "sec1",
            "pw123456",
            Method::PUT,
            &format!("/repositories/{npm_id}/security"),
            r#"{"config":{"vuln":{"enabled":true,"action":"block","threshold":"high"},"approval":{"enabled":true,"mode":"enforce"}}}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let updated = resp.json();
    assert!(
        updated["config"]["vuln"]["enabled"] == true
            && updated["config"]["vuln"]["action"] == "block",
        "vuln policy not applied: {}",
        updated["config"]["vuln"]
    );
    assert_eq!(
        updated["config"]["approval"]["enabled"], true,
        "approval policy not applied: {}",
        updated["config"]["approval"]
    );

    // The upstream URL and its credentials are not part of the accepted shape:
    // sending them changes nothing, which is what keeps the security action from
    // becoming repository management.
    let resp = srv
        .do_as(
            "sec1",
            "pw123456",
            Method::PUT,
            &format!("/repositories/{npm_id}/security"),
            r#"{"upstream_url":"https://evil.example.com","config":{"upstream_auth":{"type":"bearer","token":"stolen"},"vuln":{"enabled":true,"action":"block","threshold":"high"}}}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let after = resp.json();
    assert_eq!(
        after["upstream_url"], "https://registry.npmjs.org",
        "upstream url changed to {}",
        after["upstream_url"]
    );
    assert_ne!(
        after["config"]["upstream_auth"]["type"], "bearer",
        "upstream auth set through the security route: {}",
        after["config"]["upstream_auth"]
    );

    // The admin repository update stays closed to the security action.
    let resp = srv
        .do_as(
            "sec1",
            "pw123456",
            Method::PUT,
            &format!("/repositories/{npm_id}"),
            r#"{"upstream_url":"https://registry.npmjs.org","config":{}}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());

    // The grant is per repository pattern: pypi-gated is outside npm-*.
    let resp = srv
        .do_as(
            "sec1",
            "pw123456",
            Method::PUT,
            &format!("/repositories/{pypi_id}/security"),
            r#"{"config":{"vuln":{"enabled":true,"action":"block","threshold":"high"}}}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());

    // No other admin surface comes with it.
    for path in ["/users", "/roles"] {
        let resp = srv.do_as("sec1", "pw123456", Method::GET, path, "").await;
        assert_eq!(
            resp.status,
            StatusCode::FORBIDDEN,
            "{path}: {}",
            resp.text()
        );
    }
}

/// Pins the negative case: read access to a repository does not carry
/// policy-write rights, and neither does the approve action, which decides
/// individual packages rather than the policy they are judged by.
#[tokio::test]
async fn security_route_denied_without_action() {
    let srv = new_test_server().await;
    let repo_id = mk_proxy_repo(&srv, "npm-ro").await;

    let resp = srv
        .admin_do(
            Method::POST,
            "/users",
            r#"{"username":"ro1","password":"pw123456"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let user_id = resp.json()["id"].as_i64().expect("user id");
    let resp = srv
        .admin_do(
            Method::POST,
            "/roles",
            r#"{"name":"approver-only","description":"approve but not policy"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let role_id = resp.json()["id"].as_i64().expect("role id");
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/roles/{role_id}/permissions"),
            r#"{"repo_pattern":"*","actions":["read","approve","audit"]}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/users/{user_id}/roles"),
            &format!(r#"{{"role_id":{role_id}}}"#),
        )
        .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());

    let resp = srv.do_as("ro1", "pw123456", Method::GET, "/me", "").await;
    let me = resp.json();
    assert!(
        me["security"] == false && me["approver"] == true,
        "me = {me}"
    );

    let resp = srv
        .do_as(
            "ro1",
            "pw123456",
            Method::PUT,
            &format!("/repositories/{repo_id}/security"),
            r#"{"config":{"vuln":{"enabled":false}}}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
}
