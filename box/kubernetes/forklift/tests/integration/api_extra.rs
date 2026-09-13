use http::{Method, StatusCode};

use forklift::testing::api::{TestServer, new_test_server};

/// Serves the harness router on an ephemeral loopback port and returns its base
/// URL. The upstream-probe tests point the server at itself, which needs a real
/// address; the task is detached and stops when the test binary exits.
async fn serve_self(srv: &TestServer) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind self");
    let addr = listener.local_addr().expect("self addr");
    let app = srv.app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// Verifies a predefined seed repository cannot be deleted even by an admin
/// (403), is flagged seeded in its DTO, and that a non-seed repository still
/// deletes normally.
#[tokio::test]
async fn seed_repository_protected_from_delete() {
    let srv = new_test_server().await;

    // A repository whose name matches a predefined default is protected.
    let seed = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"npmjs","format":"npm","type":"proxy","upstream_url":"https://registry.npmjs.org"}"#,
        )
        .await
        .json();
    assert_eq!(
        seed["seeded"], true,
        "seeded = false for a predefined repository name"
    );
    let seed_id = seed["id"].as_i64().expect("repository id");
    let resp = srv
        .admin_do(Method::DELETE, &format!("/repositories/{seed_id}"), "")
        .await;
    assert_eq!(
        resp.status,
        StatusCode::FORBIDDEN,
        "delete seed repo = {}, want 403",
        resp.status
    );
    // It must still exist.
    let resp = srv
        .admin_do(Method::GET, &format!("/repositories/{seed_id}"), "")
        .await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "seed repo missing after refused delete = {}",
        resp.status
    );

    // A repository with a non-default name deletes normally.
    let custom = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"my-custom","format":"npm","type":"hosted"}"#,
        )
        .await
        .json();
    assert_ne!(
        custom["seeded"], true,
        "seeded = true for a non-default repository name"
    );
    let custom_id = custom["id"].as_i64().expect("repository id");
    let resp = srv
        .admin_do(Method::DELETE, &format!("/repositories/{custom_id}"), "")
        .await;
    assert_eq!(
        resp.status,
        StatusCode::NO_CONTENT,
        "delete non-seed repo = {}, want 204",
        resp.status
    );
}

#[tokio::test]
async fn token_create_validation() {
    let srv = new_test_server().await;

    // The full valid body, with one field replaced (or dropped when the replacement is empty).
    let valid = |field: &str, value: &str| -> String {
        let mut body = vec![
            ("name", r#""t1""#.to_string()),
            ("description", r#""ci token""#.to_string()),
            (
                "scopes",
                r#"[{"repo_pattern":"*","actions":["read"]}]"#.to_string(),
            ),
            ("expires_in", r#""720h""#.to_string()),
        ];
        if value.is_empty() {
            body.retain(|(k, _)| *k != field);
        } else if !field.is_empty() {
            for (k, v) in body.iter_mut() {
                if *k == field {
                    *v = value.to_string();
                }
            }
        }
        let fields: Vec<String> = body
            .into_iter()
            .map(|(k, v)| format!("\"{k}\":{v}"))
            .collect();
        format!("{{{}}}", fields.join(","))
    };

    // All required fields present.
    let resp = srv.admin_do(Method::POST, "/tokens", &valid("", "")).await;
    assert_eq!(
        resp.status,
        StatusCode::CREATED,
        "valid create = {}",
        resp.status
    );

    for (name, body) in [
        ("missing name", valid("name", "")),
        ("missing description", valid("description", "")),
        ("missing scopes", valid("scopes", "")),
        ("empty scopes", valid("scopes", "[]")),
        (
            "scope without pattern",
            valid("scopes", r#"[{"repo_pattern":"","actions":["read"]}]"#),
        ),
        (
            "scope without actions",
            valid("scopes", r#"[{"repo_pattern":"*","actions":[]}]"#),
        ),
        (
            "invalid scope action",
            valid("scopes", r#"[{"repo_pattern":"*","actions":["admin"]}]"#),
        ),
        ("missing expires_in", valid("expires_in", "")),
        ("invalid expires_in", valid("expires_in", r#""banana""#)),
        ("negative expires_in", valid("expires_in", r#""-1h""#)),
        ("expires_in over 1y", valid("expires_in", r#""8761h""#)),
    ] {
        let resp = srv.admin_do(Method::POST, "/tokens", &body).await;
        assert_eq!(
            resp.status,
            StatusCode::BAD_REQUEST,
            "{name} = {}, want 400",
            resp.status
        );
    }

    // One year exactly is allowed.
    let resp = srv
        .admin_do(Method::POST, "/tokens", &valid("expires_in", r#""8760h""#))
        .await;
    assert_eq!(
        resp.status,
        StatusCode::CREATED,
        "one year expiry = {}",
        resp.status
    );
}

#[tokio::test]
async fn role_and_user_errors() {
    let srv = new_test_server().await;

    // Duplicate role.
    srv.admin_do(Method::POST, "/roles", r#"{"name":"dup"}"#)
        .await;
    let resp = srv
        .admin_do(Method::POST, "/roles", r#"{"name":"dup"}"#)
        .await;
    assert_eq!(
        resp.status,
        StatusCode::CONFLICT,
        "dup role = {}",
        resp.status
    );

    // Empty role name.
    let resp = srv.admin_do(Method::POST, "/roles", r#"{"name":""}"#).await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "empty role = {}",
        resp.status
    );

    // Missing user fields.
    let resp = srv
        .admin_do(Method::POST, "/users", r#"{"username":"x"}"#)
        .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "missing password = {}",
        resp.status
    );

    // Delete non-existent user.
    let resp = srv.admin_do(Method::DELETE, "/users/9999", "").await;
    assert_eq!(
        resp.status,
        StatusCode::NOT_FOUND,
        "delete missing user = {}",
        resp.status
    );
}

#[tokio::test]
async fn role_assignment_and_deletion() {
    let srv = new_test_server().await;

    // Create user and role.
    let user = srv
        .admin_do(
            Method::POST,
            "/users",
            r#"{"username":"u1","password":"pw"}"#,
        )
        .await
        .json();
    let uid = user["id"].as_i64().expect("user id");

    let role = srv
        .admin_do(Method::POST, "/roles", r#"{"name":"r1"}"#)
        .await
        .json();
    let rid = role["id"].as_i64().expect("role id");

    // Assign then remove the role.
    srv.admin_do(
        Method::POST,
        &format!("/users/{uid}/roles"),
        &format!(r#"{{"role_id":{rid}}}"#),
    )
    .await;
    let resp = srv
        .admin_do(Method::DELETE, &format!("/users/{uid}/roles/{rid}"), "")
        .await;
    assert_eq!(
        resp.status,
        StatusCode::NO_CONTENT,
        "remove role = {}",
        resp.status
    );

    // Delete the role.
    let resp = srv
        .admin_do(Method::DELETE, &format!("/roles/{rid}"), "")
        .await;
    assert_eq!(
        resp.status,
        StatusCode::NO_CONTENT,
        "delete role = {}",
        resp.status
    );
}

#[tokio::test]
async fn group_mapping_deletion() {
    let srv = new_test_server().await;
    let role = srv
        .admin_do(Method::POST, "/roles", r#"{"name":"gm"}"#)
        .await
        .json();
    let rid = role["id"].as_i64().expect("role id");
    srv.admin_do(
        Method::POST,
        "/group-mappings",
        &format!(r#"{{"group_name":"g","role_id":{rid}}}"#),
    )
    .await;

    let ms = srv
        .admin_do(Method::GET, "/group-mappings", "")
        .await
        .json();
    let id = ms[0]["id"].as_i64().expect("mapping id");
    let resp = srv
        .admin_do(Method::DELETE, &format!("/group-mappings/{id}"), "")
        .await;
    assert_eq!(
        resp.status,
        StatusCode::NO_CONTENT,
        "delete mapping = {}",
        resp.status
    );
}

#[tokio::test]
async fn update_repository_bad_config() {
    let srv = new_test_server().await;
    let created = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"r","format":"go","type":"hosted"}"#,
        )
        .await
        .json();
    let id = created["id"].as_i64().expect("repository id");

    // Invalid eviction value -> 400.
    let resp = srv
        .admin_do(
            Method::PUT,
            &format!("/repositories/{id}"),
            r#"{"config":{"cache":{"eviction":"fifo"}}}"#,
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "bad config = {}",
        resp.status
    );

    // Update non-existent repo -> 404.
    let resp = srv
        .admin_do(Method::PUT, "/repositories/99999", r#"{"config":{}}"#)
        .await;
    assert_eq!(
        resp.status,
        StatusCode::NOT_FOUND,
        "update missing = {}",
        resp.status
    );
}

/// Guards against the PUT full-replace foot-gun: omitting `upstream_url` must
/// not silently zero out a proxy's upstream.
#[tokio::test]
async fn update_proxy_requires_upstream() {
    let srv = new_test_server().await;
    let created = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"px","format":"npm","type":"proxy","upstream_url":"https://registry.npmjs.org"}"#,
        )
        .await
        .json();
    let id = created["id"].as_i64().expect("repository id");

    // PUT with config only (no upstream_url) must be rejected, not accepted.
    let resp = srv
        .admin_do(
            Method::PUT,
            &format!("/repositories/{id}"),
            r#"{"config":{"cache":{"enabled":true}}}"#,
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "proxy update without upstream_url = {}, want 400",
        resp.status
    );

    // The upstream must still be intact.
    let got = srv
        .admin_do(Method::GET, &format!("/repositories/{id}"), "")
        .await
        .json();
    assert_eq!(
        got["upstream_url"], "https://registry.npmjs.org",
        "upstream_url = {}, want it unchanged",
        got["upstream_url"]
    );
}

/// Verifies the token-autocomplete endpoint: any authenticated user may list
/// repository names even though the full repositories list is admin-only.
#[tokio::test]
async fn repository_names_for_non_admin() {
    let srv = new_test_server().await;
    let resp = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"npm-proxy","format":"npm","type":"proxy","upstream_url":"https://registry.npmjs.org"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());

    // A role-less local user (authenticated, not admin).
    let resp = srv
        .admin_do(
            Method::POST,
            "/users",
            r#"{"username":"dev1","password":"pw123456"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());

    // /repository-names: allowed, returns the name.
    let resp = srv
        .do_as("dev1", "pw123456", Method::GET, "/repository-names", "")
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let names = resp.json();
    assert!(
        names.as_array().map(Vec::len) == Some(1)
            && names[0]["name"] == "npm-proxy"
            && names[0]["format"] == "npm",
        "unexpected names: {names}"
    );

    // /repositories: authenticated users may list, but the filter returns only
    // repositories they can read -- a role-less user sees an empty list.
    let resp = srv
        .do_as("dev1", "pw123456", Method::GET, "/repositories", "")
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let visible = resp.json();
    assert_eq!(
        visible.as_array().map(Vec::len),
        Some(0),
        "role-less user should see no repositories, got {visible}"
    );
}

#[tokio::test]
async fn get_repository_not_found() {
    let srv = new_test_server().await;
    let resp = srv.admin_do(Method::GET, "/repositories/424242", "").await;
    assert_eq!(
        resp.status,
        StatusCode::NOT_FOUND,
        "missing repo = {}",
        resp.status
    );

    let resp = srv
        .admin_do(Method::GET, "/repositories/notanint", "")
        .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "bad id = {}",
        resp.status
    );
}

#[tokio::test]
async fn list_artifacts_endpoint() {
    let srv = new_test_server().await;
    let created = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"r","format":"npm","type":"proxy","upstream_url":"https://registry.npmjs.org"}"#,
        )
        .await
        .json();
    let id = created["id"].as_i64().expect("repository id");

    let resp = srv
        .admin_do(Method::GET, &format!("/repositories/{id}/artifacts"), "")
        .await;
    assert_eq!(resp.status, StatusCode::OK, "artifacts = {}", resp.status);
    let out = resp.json();
    assert_eq!(out["count"], 0, "count = {}, want 0", out["count"]);
    assert!(out.get("artifacts").is_some(), "missing artifacts field");
}

#[tokio::test]
async fn upstream_health() {
    let srv = new_test_server().await;
    let base = serve_self(&srv).await;

    // Local repo -> not applicable.
    let loc = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"loc","format":"go","type":"hosted"}"#,
        )
        .await
        .json();
    let loc_id = loc["id"].as_i64().expect("repository id");
    let hl = srv
        .admin_do(
            Method::GET,
            &format!("/repositories/{loc_id}/upstream-health"),
            "",
        )
        .await
        .json();
    assert_eq!(
        hl["applicable"], false,
        "local applicable = {}, want false",
        hl["applicable"]
    );

    // Proxy pointing at the test server itself -> reachable.
    let px = srv
        .admin_do(
            Method::POST,
            "/repositories",
            &format!(r#"{{"name":"px","format":"maven","type":"proxy","upstream_url":"{base}"}}"#),
        )
        .await
        .json();
    let px_id = px["id"].as_i64().expect("repository id");
    let hp = srv
        .admin_do(
            Method::GET,
            &format!("/repositories/{px_id}/upstream-health"),
            "",
        )
        .await
        .json();
    assert!(
        hp["applicable"] == true && hp["reachable"] == true,
        "proxy health = {hp}, want reachable"
    );
}

#[tokio::test]
async fn check_upstream() {
    let srv = new_test_server().await;
    let base = serve_self(&srv).await;

    // Reachable: probe the test server itself.
    let ok = srv
        .admin_do(
            Method::POST,
            "/repositories/check-upstream",
            &format!(r#"{{"url":"{base}"}}"#),
        )
        .await
        .json();
    assert!(
        ok["applicable"] == true && ok["reachable"] == true,
        "check = {ok}, want reachable"
    );

    // Invalid scheme/host -> reachable false with an error, still HTTP 200.
    let bad = srv
        .admin_do(
            Method::POST,
            "/repositories/check-upstream",
            r#"{"url":"not-a-url"}"#,
        )
        .await
        .json();
    assert!(
        bad["reachable"] == false && !bad["error"].is_null(),
        "invalid url check = {bad}, want reachable=false with error"
    );

    // Empty url -> 400.
    let resp = srv
        .admin_do(
            Method::POST,
            "/repositories/check-upstream",
            r#"{"url":""}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
}

#[tokio::test]
async fn logout_clears_session() {
    let srv = new_test_server().await;
    let resp = srv.anon_do(Method::POST, "/logout", "").await;
    assert_eq!(
        resp.status,
        StatusCode::NO_CONTENT,
        "logout = {}, want 204",
        resp.status
    );
    // The check is against the wire form because the harness has no cookie jar to parse it
    // back.
    let cleared = resp
        .headers
        .get_all(http::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.starts_with("forklift_session=;") && v.contains("Max-Age=0"));
    assert!(
        cleared,
        "session cookie not cleared: {:?}",
        resp.headers.get_all(http::header::SET_COOKIE)
    );
}

#[tokio::test]
async fn repository_permissions() {
    let srv = new_test_server().await;

    // A proxy repo and a role granting read on a matching pattern.
    let repo = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"maven-central","format":"maven","type":"proxy","upstream_url":"https://repo1.maven.org/maven2"}"#,
        )
        .await
        .json();
    let repo_id = repo["id"].as_i64().expect("repository id");
    let resp = srv
        .admin_do(
            Method::POST,
            "/roles",
            r#"{"name":"maven-readers","permissions":[{"repo_pattern":"maven-*","actions":["read"]}]}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());

    // The matching role appears for this repo.
    let resp = srv
        .admin_do(
            Method::GET,
            &format!("/repositories/{repo_id}/permissions"),
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let perms = resp.json();
    let found = perms.as_array().expect("permissions").iter().any(|p| {
        p["role"] == "maven-readers"
            && p["repo_pattern"] == "maven-*"
            && p["actions"].as_array().map(Vec::len) == Some(1)
            && p["actions"][0] == "read"
    });
    assert!(
        found,
        "maven-readers not matched for maven-central: {perms}"
    );

    // A non-matching repo does not list it.
    let npm = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"npm-proxy","format":"npm","type":"proxy","upstream_url":"https://registry.npmjs.org"}"#,
        )
        .await
        .json();
    let npm_id = npm["id"].as_i64().expect("repository id");
    let perms = srv
        .admin_do(
            Method::GET,
            &format!("/repositories/{npm_id}/permissions"),
            "",
        )
        .await
        .json();
    for p in perms.as_array().expect("permissions") {
        assert_ne!(
            p["role"], "maven-readers",
            "maven-* must not match npm-proxy: {perms}"
        );
    }
}

#[tokio::test]
async fn repository_tokens() {
    let srv = new_test_server().await;
    let repo = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"maven-central","format":"maven","type":"proxy","upstream_url":"https://repo1.maven.org/maven2"}"#,
        )
        .await
        .json();
    let repo_id = repo["id"].as_i64().expect("repository id");

    // Admin (self) creates a scoped token matching the repo.
    let resp = srv
        .admin_do(
            Method::POST,
            "/tokens",
            r#"{"name":"ci","description":"ci","scopes":[{"repo_pattern":"maven-*","actions":["read"]}],"expires_in":"720h"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());

    let resp = srv
        .admin_do(Method::GET, &format!("/repositories/{repo_id}/tokens"), "")
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let toks = resp.json();
    let found = toks.as_array().expect("tokens").iter().any(|tk| {
        tk["name"] == "ci"
            && tk["repo_pattern"] == "maven-*"
            && tk["unscoped"] != true
            && tk["actions"].as_array().map(Vec::len) == Some(1)
            && tk["actions"][0] == "read"
    });
    assert!(found, "scoped token not listed for maven-central: {toks}");

    // Non-matching repo excludes the scoped token.
    let npm = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"npm-proxy","format":"npm","type":"proxy","upstream_url":"https://registry.npmjs.org"}"#,
        )
        .await
        .json();
    let npm_id = npm["id"].as_i64().expect("repository id");
    let toks = srv
        .admin_do(Method::GET, &format!("/repositories/{npm_id}/tokens"), "")
        .await
        .json();
    for tk in toks.as_array().expect("tokens") {
        assert!(
            !(tk["name"] == "ci" && tk["unscoped"] != true),
            "maven-* scoped token must not match npm-proxy: {toks}"
        );
    }
}
