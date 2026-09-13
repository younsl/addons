use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use axum::Router;
use http::{Method, StatusCode};

use forklift::meta::Receiver;
use forklift::repoconfig;

use forklift::api::HAStatus;
use forklift::testing::api::{TestServer, mk_proxy_repo, new_console_server, new_test_server};

/// The task is detached; it stops when the test binary exits.
async fn webhook_sink(status: StatusCode) -> (String, Arc<AtomicI32>) {
    let hits = Arc::new(AtomicI32::new(0));
    let counter = Arc::clone(&hits);
    let app = Router::new().fallback(axum::routing::any(move || {
        let counter = Arc::clone(&counter);
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            status
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind sink");
    let addr = listener.local_addr().expect("sink addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), hits)
}

#[tokio::test]
async fn receiver_handlers() {
    let srv = new_console_server().await;
    let (sink, _) = webhook_sink(StatusCode::OK).await;

    // Create.
    let body = format!(r#"{{"name":"slack","description":"sec chan","webhook_url":"{sink}"}}"#);
    let resp = srv
        .admin_do(Method::POST, "/notification/receivers", &body)
        .await;
    assert_eq!(
        resp.status,
        StatusCode::CREATED,
        "create status={} body={}",
        resp.status,
        resp.text()
    );
    let created = resp.json();
    assert!(
        created["id"].as_i64() != Some(0)
            && created["webhook_configured"] == true
            && created["enabled"] == true,
        "unexpected created receiver: {created}"
    );
    let created_id = created["id"].as_i64().expect("receiver id");

    // List -- URL never leaks; only webhook_configured.
    let list = srv
        .admin_do(Method::GET, "/notification/receivers", "")
        .await
        .json();
    assert!(
        list.as_array().map(Vec::len) == Some(1)
            && list[0]["name"] == "slack"
            && list[0]["webhook_configured"] == true,
        "list = {list}"
    );

    // Update: blank webhook keeps the stored one; toggle disabled.
    let upd = r#"{"name":"slack","description":"renamed","webhook_url":"","enabled":false}"#;
    let updated = srv
        .admin_do(
            Method::PUT,
            &format!("/notification/receivers/{created_id}"),
            upd,
        )
        .await
        .json();
    assert!(
        updated["description"] == "renamed"
            && updated["enabled"] != true
            && updated["webhook_configured"] == true,
        "update not applied: {updated}"
    );

    // Validation: bad name, bad webhook URL, invalid JSON.
    for c in [
        r#"{"name":"bad name","webhook_url":"https://h.example/x"}"#,
        r#"{"name":"ok","webhook_url":"notaurl"}"#,
        r#"{bad json"#,
    ] {
        let resp = srv
            .admin_do(Method::POST, "/notification/receivers", c)
            .await;
        assert_eq!(
            resp.status,
            StatusCode::BAD_REQUEST,
            "case {c:?} status={}, want 400",
            resp.status
        );
    }

    // Delete.
    let resp = srv
        .admin_do(
            Method::DELETE,
            &format!("/notification/receivers/{created_id}"),
            "",
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::NO_CONTENT,
        "delete status={}, want 204",
        resp.status
    );
}

#[tokio::test]
async fn receiver_test_and_adhoc() {
    let srv = new_console_server().await;
    let (sink, hits) = webhook_sink(StatusCode::OK).await;

    let rec = srv
        .store
        .create_receiver(Receiver {
            name: "chan".to_string(),
            webhook_url: sink.clone(),
            enabled: true,
            ..Default::default()
        })
        .await
        .expect("create receiver");
    // Stored-receiver test delivers and reports sent.
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/notification/receivers/{}/test", rec.id),
            "",
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "test status={}, want 200",
        resp.status
    );
    assert_ne!(hits.load(Ordering::SeqCst), 0, "expected webhook delivery");

    // Receiver without a webhook URL -> 400.
    let no_url = srv
        .store
        .create_receiver(Receiver {
            name: "nourl".to_string(),
            enabled: true,
            ..Default::default()
        })
        .await
        .expect("create receiver");
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/notification/receivers/{}/test", no_url.id),
            "",
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "no-url test status={}, want 400",
        resp.status
    );

    // Ad-hoc URL test.
    let resp = srv
        .admin_do(
            Method::POST,
            "/notification/test",
            &format!(r#"{{"webhook_url":"{sink}","name":"probe"}}"#),
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "adhoc status={}, want 200",
        resp.status
    );
    // Ad-hoc with an invalid URL -> 400.
    let resp = srv
        .admin_do(
            Method::POST,
            "/notification/test",
            r#"{"webhook_url":"nope"}"#,
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "adhoc bad url status={}, want 400",
        resp.status
    );
}

#[tokio::test]
async fn receiver_test_no_notifier() {
    // A server without a notifier reports 503 for delivery endpoints.
    let srv = new_test_server().await;
    let rec = srv
        .store
        .create_receiver(Receiver {
            name: "chan".to_string(),
            webhook_url: "https://hooks.example/x".to_string(),
            enabled: true,
            ..Default::default()
        })
        .await
        .expect("create receiver");
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/notification/receivers/{}/test", rec.id),
            "",
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "no-notifier test status={}, want 503",
        resp.status
    );
}

#[tokio::test]
async fn repo_sample() {
    let srv = new_console_server().await;
    let (sink, hits) = webhook_sink(StatusCode::OK).await;

    let id = mk_proxy_repo(&srv, "npmjs").await;
    srv.store
        .create_receiver(Receiver {
            name: "chan".to_string(),
            webhook_url: sink,
            enabled: true,
            ..Default::default()
        })
        .await
        .expect("create receiver");
    // Wire the receiver into the repository's notify config.
    let mut cfg = repoconfig::default();
    cfg.notify.receivers = vec!["chan".to_string()];
    let cfg_json = cfg.json().expect("encode config");
    srv.store
        .update_repository_config(id, "https://registry.npmjs.org", &cfg_json)
        .await
        .expect("update repository config");

    // Preview returns the payload and the resolved receivers without delivering.
    let resp = srv
        .admin_do(
            Method::GET,
            &format!("/repositories/{id}/notification/sample"),
            "",
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "preview status={}, want 200",
        resp.status
    );
    let prev = resp.json();
    assert!(
        prev["receivers"].as_array().map(Vec::len) == Some(1)
            && prev["receivers"][0]["enabled"] == true
            && prev["payload"]["event"] == "approval.request",
        "unexpected preview: {prev}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0, "preview must not deliver");

    // Send delivers to the enabled receiver.
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/repositories/{id}/notification/sample"),
            "",
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "send status={}, want 200",
        resp.status
    );
    let sent = resp.json();
    assert!(
        sent["results"].as_array().map(Vec::len) == Some(1) && sent["results"][0]["ok"] == true,
        "unexpected send results: {}",
        sent["results"]
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "expected 1 delivery, got {}",
        hits.load(Ordering::SeqCst)
    );

    // A repository with no selected receivers -> 400 on send.
    let bare = mk_proxy_repo(&srv, "pypi-bare").await;
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/repositories/{bare}/notification/sample"),
            "",
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "send no-receivers status={}, want 400",
        resp.status
    );
}

#[tokio::test]
async fn ha_status_and_step_down() {
    let srv = new_console_server().await;

    // No provider wired -> single-instance leader.
    let st = srv.admin_do(Method::GET, "/ha", "").await.json();
    assert!(
        st["mode"] == "single" && st["is_leader"] == true,
        "default HA = {st}, want single leader"
    );

    // Injected provider is reflected.
    srv.handler.set_ha_status(Arc::new(|| HAStatus {
        enabled: true,
        mode: "object-storage".to_string(),
        backend: "s3".to_string(),
        is_leader: true,
        role: "leader".to_string(),
        fencing_token: 7,
        runtime: "rustc 1.98.1".to_string(),
        ..Default::default()
    }));
    let st = srv.admin_do(Method::GET, "/ha", "").await.json();
    assert!(
        st["backend"] == "s3" && st["fencing_token"] == 7 && st["runtime"] == "rustc 1.98.1",
        "injected HA = {st}"
    );

    // Step-down with no provider -> 409.
    let resp = srv.admin_do(Method::POST, "/ha/step-down", "").await;
    assert_eq!(
        resp.status,
        StatusCode::CONFLICT,
        "stepdown no-provider status={}, want 409",
        resp.status
    );

    // Not the leader -> 409.
    srv.handler.set_ha_step_down(Arc::new(|| false));
    let resp = srv.admin_do(Method::POST, "/ha/step-down", "").await;
    assert_eq!(
        resp.status,
        StatusCode::CONFLICT,
        "stepdown non-leader status={}, want 409",
        resp.status
    );

    // Leader steps down -> 200.
    srv.handler.set_ha_step_down(Arc::new(|| true));
    let resp = srv.admin_do(Method::POST, "/ha/step-down", "").await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "stepdown leader status={}, want 200",
        resp.status
    );
}

#[tokio::test]
async fn set_repository_disabled() {
    let srv = new_console_server().await;
    let id = mk_proxy_repo(&srv, "npmjs").await;

    // Take offline.
    let dto = srv
        .admin_do(
            Method::POST,
            &format!("/repositories/{id}/disabled"),
            r#"{"disabled":true}"#,
        )
        .await
        .json();
    assert_eq!(dto["disabled"], true, "expected disabled=true, got {dto}");

    // Back online.
    let dto = srv
        .admin_do(
            Method::POST,
            &format!("/repositories/{id}/disabled"),
            r#"{"disabled":false}"#,
        )
        .await
        .json();
    assert_ne!(dto["disabled"], true, "expected disabled=false");

    // Invalid JSON -> 400; unknown id -> 404.
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/repositories/{id}/disabled"),
            r#"{bad"#,
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "bad json status={}, want 400",
        resp.status
    );
    let resp = srv
        .admin_do(
            Method::POST,
            "/repositories/99999/disabled",
            r#"{"disabled":true}"#,
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::NOT_FOUND,
        "unknown id status={}, want 404",
        resp.status
    );
}

/// Verifies that renaming a receiver rewrites the name inside every repository
/// notify config that selected it, and that a receiver with linked repositories
/// cannot be deleted until detached.
#[tokio::test]
async fn receiver_rename_propagates() {
    let srv: TestServer = new_console_server().await;
    let (sink, _) = webhook_sink(StatusCode::OK).await;

    let body = format!(r#"{{"name":"slack-sec","description":"d","webhook_url":"{sink}"}}"#);
    let created = srv
        .admin_do(Method::POST, "/notification/receivers", &body)
        .await
        .json();
    let created_id = created["id"].as_i64().expect("receiver id");

    // Repository selecting the receiver by name.
    let resp = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"npm-proxy","format":"npm","type":"proxy","upstream_url":"https://registry.npmjs.org",
		  "config":{"notify":{"receivers":["slack-sec"]}}}"#,
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::CREATED,
        "create repo status={} body={}",
        resp.status,
        resp.text()
    );

    // Deletion is refused while the repository still selects the receiver.
    let resp = srv
        .admin_do(
            Method::DELETE,
            &format!("/notification/receivers/{created_id}"),
            "",
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::CONFLICT,
        "delete with linked repo status={}, want 409",
        resp.status
    );

    // Rename: the repository config must follow.
    let resp = srv
        .admin_do(
            Method::PUT,
            &format!("/notification/receivers/{created_id}"),
            r#"{"name":"slack-security","description":"d","webhook_url":""}"#,
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "rename status={} body={}",
        resp.status,
        resp.text()
    );

    let repo = srv
        .store
        .get_repository_by_name("npm-proxy")
        .await
        .expect("get repository");
    let cfg = repoconfig::parse(&repo.config_json).expect("parse config");
    assert!(
        cfg.notify.receivers.len() == 1 && cfg.notify.receivers[0] == "slack-security",
        "repo notify receivers = {:?}, want [slack-security]",
        cfg.notify.receivers
    );

    // The usage listing follows the new name too.
    let list = srv
        .admin_do(Method::GET, "/notification/receivers", "")
        .await
        .json();
    assert!(
        list.as_array().map(Vec::len) == Some(1)
            && list[0]["repositories"].as_array().map(Vec::len) == Some(1)
            && list[0]["repositories"][0] == "npm-proxy",
        "usage after rename = {list}"
    );
}
