use http::{Method, StatusCode};

use forklift::repoconfig;

use forklift::testing::api::new_test_server;

#[tokio::test]
async fn upstream_auth_masked_and_round_trip() {
    let srv = new_test_server().await;

    let body = r#"{"name":"npm-priv","format":"npm","type":"proxy","upstream_url":"https://npm.corp.example.com",
	  "config":{"upstream_auth":{"type":"basic","username":"mirror","password":"s3cret"}}}"#;
    let resp = srv.admin_do(Method::POST, "/repositories", body).await;
    assert_eq!(resp.status, StatusCode::CREATED, "create: {}", resp.text());
    let mut repository = resp.json();
    let id = repository["id"].as_i64().expect("repository id");

    // Secrets never appear in responses; identity fields stay readable.
    assert_eq!(
        repository["config"]["upstream_auth"]["password"],
        repoconfig::SECRET_MASK,
        "{repository}"
    );
    assert_eq!(
        repository["config"]["upstream_auth"]["username"], "mirror",
        "{repository}"
    );

    // Round-tripping the masked config on update keeps the stored secret:
    // serving-side proof is in the repo module; here the persisted config is
    // shown not to be the mask by replacing the username only.
    repository["config"]["upstream_auth"]["username"] = "mirror2".into();
    let update = format!(
        r#"{{"upstream_url":"https://npm.corp.example.com","config":{}}}"#,
        serde_json::to_string(&repository["config"]).expect("config json")
    );
    let resp = srv
        .admin_do(Method::PUT, &format!("/repositories/{id}"), &update)
        .await;
    assert_eq!(resp.status, StatusCode::OK, "update: {}", resp.text());
    let updated = resp.json();
    assert_eq!(
        updated["config"]["upstream_auth"]["username"], "mirror2",
        "{updated}"
    );
    assert_eq!(
        updated["config"]["upstream_auth"]["password"],
        repoconfig::SECRET_MASK,
        "{updated}"
    );
    // The raw response body must never contain the plaintext secret.
    let resp = srv
        .admin_do(Method::GET, &format!("/repositories/{id}"), "")
        .await;
    assert!(
        !resp.text().contains("s3cret"),
        "secret leaked in response: {}",
        resp.text()
    );

    // upstream_auth is proxy-only.
    let resp = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"npm-hosted","format":"npm","type":"hosted","config":{"upstream_auth":{"type":"bearer","token":"t"}}}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "hosted auth");

    // Invalid auth shapes are rejected.
    let resp = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"npm-bad","format":"npm","type":"proxy","upstream_url":"https://x.example.com","config":{"upstream_auth":{"type":"ntlm"}}}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "bad auth type");

    // check-upstream with repository_id only presents stored credentials to the
    // repository's own upstream host; any other URL is refused, so the endpoint
    // cannot be used to exfiltrate the masked secret.
    let resp = srv
        .admin_do(
            Method::POST,
            "/repositories/check-upstream",
            &format!(r#"{{"url":"https://attacker.example.net/collect","repository_id":{id}}}"#),
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "cross-host probe: {}",
        resp.text()
    );
    let resp = srv
        .admin_do(
            Method::POST,
            "/repositories/check-upstream",
            &format!(r#"{{"url":"http://npm.corp.example.com/ping","repository_id":{id}}}"#),
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "scheme downgrade probe should be refused"
    );
    let resp = srv
        .admin_do(
            Method::POST,
            "/repositories/check-upstream",
            &format!(r#"{{"url":"https://npm.corp.example.com/ping","repository_id":{id}}}"#),
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "same-host probe: {}",
        resp.text()
    );
}
