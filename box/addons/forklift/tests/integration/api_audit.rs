use http::{Method, StatusCode};
use serde_json::Value;

use forklift::meta;

use forklift::testing::api::{ADMIN_USER, TestServer, new_audit_test_server};

async fn get_audit_logs(srv: &TestServer, repo_id: i64, query: &str) -> Value {
    let resp = srv
        .admin_do(
            Method::GET,
            &format!("/repositories/{repo_id}/audit-logs{query}"),
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "audit-logs: {}", resp.text());
    resp.json()
}

#[tokio::test]
async fn repository_lifecycle_is_audited() {
    let (srv, rec) = new_audit_test_server().await;

    // Create.
    let body = r#"{"name":"npm-local","format":"npm","type":"hosted"}"#;
    let resp = srv.admin_do(Method::POST, "/repositories", body).await;
    assert_eq!(resp.status, StatusCode::CREATED, "create: {}", resp.text());
    let created = resp.json();
    let id = created["id"].as_i64().expect("repository id");

    // Update.
    let update = format!(
        r#"{{"upstream_url":"","config":{}}}"#,
        serde_json::to_string(&created["config"]).expect("config json")
    );
    let resp = srv
        .admin_do(Method::PUT, &format!("/repositories/{id}"), &update)
        .await;
    assert_eq!(resp.status, StatusCode::OK, "update: {}", resp.text());

    rec.close().await; // flush async events before asserting

    let list = get_audit_logs(&srv, id, "").await;
    assert_eq!(list["count"], 2, "{list}");
    let logs = list["logs"].as_array().expect("logs");
    assert_eq!(logs.len(), 2, "{list}");
    // Newest first.
    assert_eq!(logs[0]["event"], meta::EVENT_REPO_UPDATE, "{list}");
    assert_eq!(logs[1]["event"], meta::EVENT_REPO_CREATE, "{list}");
    assert_eq!(logs[1]["username"], ADMIN_USER, "{list}");
    assert_eq!(logs[1]["status"], 201, "{list}");

    // Event filter and pagination params.
    let filtered = get_audit_logs(&srv, id, "?event=repo.create&limit=1&offset=0").await;
    assert_eq!(filtered["count"], 1, "{filtered}");
    let logs = filtered["logs"].as_array().expect("logs");
    assert_eq!(logs.len(), 1, "{filtered}");
    assert_eq!(logs[0]["event"], meta::EVENT_REPO_CREATE, "{filtered}");
}

#[tokio::test]
async fn audit_logs_repo_not_found() {
    let (srv, rec) = new_audit_test_server().await;
    let resp = srv
        .admin_do(Method::GET, "/repositories/9999/audit-logs", "")
        .await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
    rec.close().await;
}
