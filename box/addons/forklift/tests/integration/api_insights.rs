use std::sync::Arc;
use std::time::Duration;

use http::{Method, StatusCode};

use forklift::insights::Analyzer;
use forklift::testing::api::{USER_PASS, mk_read_user, new_test_server};

#[tokio::test]
async fn insights_report_is_scoped_to_the_caller() {
    let srv = new_test_server().await;
    let resp = srv.admin_do(Method::GET, "/insights", "").await;
    assert_eq!(
        resp.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        resp.text()
    );

    let analyzer = Analyzer::new(Arc::clone(&srv.store), None, Duration::from_hours(1));
    srv.handler
        .set_insights(Arc::clone(&analyzer), Arc::new(|_: &str| true));
    let empty = srv.admin_do(Method::GET, "/insights", "").await.json();
    assert!(empty["report"].is_null(), "{empty}");
    assert_eq!(empty["can_refresh"], true, "{empty}");

    for name in ["team-a-public", "team-b-public"] {
        let body = format!(
            r#"{{"name":"{name}","format":"raw","type":"hosted","config":{{"public":true}}}}"#
        );
        let resp = srv.admin_do(Method::POST, "/repositories", &body).await;
        assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    }
    let report = analyzer.run_once("test").await.expect("analyze");
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.id == "public-repositories" && f.total == 2),
        "{:?}",
        report.findings
    );

    let full = srv.admin_do(Method::GET, "/insights", "").await.json();
    assert_eq!(full["scope"], "all", "{full}");
    assert_eq!(full["history"].as_array().map(Vec::len), Some(1), "{full}");
    assert_eq!(full["report"]["triggered_by"], "test", "{full}");

    mk_read_user(&srv.store, "dev", "team-a-*").await;
    let resp = srv
        .do_as("dev", USER_PASS, Method::GET, "/insights", "")
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let narrowed = resp.json();
    assert_eq!(narrowed["scope"], "repositories", "{narrowed}");
    assert_eq!(narrowed["history"], serde_json::json!([]), "{narrowed}");
    assert_eq!(narrowed["can_refresh"], false, "{narrowed}");
    let findings = narrowed["report"]["findings"].as_array().expect("findings");
    let public = findings
        .iter()
        .find(|f| f["id"] == "public-repositories")
        .expect("public finding kept for the readable repository");
    assert_eq!(public["total"], 1, "{public}");
    assert_eq!(public["affected"][0]["repo"], "team-a-public", "{public}");
    assert!(
        findings.iter().all(|f| f["category"] != "access"),
        "account findings hidden: {narrowed}"
    );
    let inventory = &narrowed["report"]["inventory"];
    assert_eq!(
        inventory["repositories"], 1,
        "only team-a-public: {inventory}"
    );
    assert_eq!(
        inventory["tokens"], 0,
        "the developer owns none: {inventory}"
    );
    assert!(inventory.get("scan_by_repo").is_none(), "{inventory}");
    assert_eq!(inventory["users"], 0, "{inventory}");

    let resp = srv
        .do_as("dev", USER_PASS, Method::POST, "/insights/refresh", "")
        .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
    let resp = srv.admin_do(Method::POST, "/insights/refresh", "").await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());

    srv.handler
        .set_insights(Arc::clone(&analyzer), Arc::new(|_: &str| false));
    let resp = srv.admin_do(Method::POST, "/insights/refresh", "").await;
    assert_eq!(resp.status, StatusCode::CONFLICT, "{}", resp.text());
}

#[tokio::test]
async fn insights_alarm_preview_and_send() {
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    forklift::server::install_crypto_provider();
    let srv = new_test_server().await;
    let analyzer = Analyzer::new(Arc::clone(&srv.store), None, Duration::from_hours(1));
    srv.handler
        .set_insights(Arc::clone(&analyzer), Arc::new(|_: &str| true));
    let resp = srv
        .admin_do(Method::GET, "/insights/notification/preview", "")
        .await;
    assert_eq!(
        resp.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        resp.text()
    );

    let notifier = Arc::new(forklift::notify::Notifier::new(Duration::from_secs(2)));
    notifier.set_external_url("https://forklift.example.com");
    srv.handler.set_notifier(notifier);
    let resp = srv
        .admin_do(Method::GET, "/insights/notification/preview", "")
        .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "no analysis yet: {}",
        resp.text()
    );

    let body = r#"{"name":"open","format":"raw","type":"hosted","config":{"public":true}}"#;
    srv.admin_do(Method::POST, "/repositories", body).await;
    analyzer.run_once("test").await.expect("analyze");

    let hook = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("insights.report"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&hook)
        .await;
    let body = format!(
        r#"{{"name":"sec-alerts","webhook_url":"{}/hook","enabled":true}}"#,
        hook.uri()
    );
    let created = srv
        .admin_do(Method::POST, "/notification/receivers", &body)
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text());
    let id = created.json()["id"].as_i64().expect("receiver id");

    let preview = srv
        .admin_do(Method::GET, "/insights/notification/preview", "")
        .await
        .json();
    let text = preview["payload"]["text"].as_str().unwrap_or_default();
    assert!(text.contains("/ 100"), "{preview}");
    assert!(
        text.contains("workspace/insights/public-repositories"),
        "{preview}"
    );
    assert_eq!(preview["receivers"][0]["name"], "sec-alerts", "{preview}");

    let resp = srv
        .admin_do(
            Method::POST,
            "/insights/notification/send",
            r#"{"receiver_ids":[]}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
    let resp = srv
        .admin_do(
            Method::POST,
            "/insights/notification/send",
            &format!(r#"{{"receiver_ids":[{id},999]}}"#),
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let results = resp.json();
    assert_eq!(results["results"][0]["ok"], true, "{results}");
    assert_eq!(results["results"][1]["ok"], false, "{results}");

    let history = srv
        .admin_do(Method::GET, "/insights/notification/history", "")
        .await
        .json();
    let deliveries = history["deliveries"].as_array().expect("deliveries");
    assert_eq!(
        deliveries.len(),
        1,
        "an unknown receiver id is not a delivery: {history}"
    );
    assert_eq!(deliveries[0]["receiver"], "sec-alerts", "{history}");
    assert_eq!(deliveries[0]["trigger"], "manual", "{history}");
    assert_eq!(deliveries[0]["ok"], true, "{history}");
    assert!(deliveries[0]["items"].as_i64() >= Some(1), "{history}");
    let resp = srv
        .admin_do(Method::GET, "/insights/notification/history?days=91", "")
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());

    mk_read_user(&srv.store, "dev", "*").await;
    let resp = srv
        .do_as(
            "dev",
            USER_PASS,
            Method::GET,
            "/insights/notification/history",
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
    let resp = srv
        .do_as(
            "dev",
            USER_PASS,
            Method::POST,
            "/insights/notification/send",
            r#"{"receiver_ids":[1]}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
}

#[tokio::test]
async fn acknowledging_a_check_takes_it_out_of_the_score() {
    let srv = new_test_server().await;
    let analyzer = Analyzer::new(Arc::clone(&srv.store), None, Duration::from_hours(1));
    srv.handler
        .set_insights(Arc::clone(&analyzer), Arc::new(|_: &str| true));
    let body = r#"{"name":"open","format":"raw","type":"hosted","config":{"public":true}}"#;
    srv.admin_do(Method::POST, "/repositories", body).await;
    analyzer.run_once("test").await.expect("analyze");
    let before = srv.admin_do(Method::GET, "/insights", "").await.json();
    let score_before = before["report"]["score"].as_i64().expect("score");

    let uri = "/insights/checks/public-repositories/acknowledgement";
    mk_read_user(&srv.store, "dev", "*").await;
    let resp = srv
        .do_as("dev", USER_PASS, Method::PUT, uri, r#"{"note":"mine"}"#)
        .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
    for (path, body, want) in [
        (uri, r#"{"note":"  "}"#, StatusCode::BAD_REQUEST),
        (uri, r#"{"note":"ok","days":0}"#, StatusCode::BAD_REQUEST),
        (
            "/insights/checks/plaintext-upstream/acknowledgement",
            r#"{"note":"ok"}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            "/insights/checks/no-such-check/acknowledgement",
            r#"{"note":"ok"}"#,
            StatusCode::NOT_FOUND,
        ),
    ] {
        let resp = srv.admin_do(Method::PUT, path, body).await;
        assert_eq!(resp.status, want, "{path} {body}: {}", resp.text());
    }
    let resp = srv
        .admin_do(
            Method::PUT,
            uri,
            r#"{"note":"intended public mirror","days":30}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.json()["flagged"], 1);

    let after = srv.admin_do(Method::GET, "/insights", "").await.json();
    let check = after["report"]["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["id"] == "public-repositories")
        .expect("check")
        .clone();
    assert_eq!(check["status"], "acknowledged", "{check}");
    assert_eq!(
        check["acknowledgement"]["note"], "intended public mirror",
        "{check}"
    );
    assert_eq!(check["acknowledgement"]["state"], "active", "{check}");
    assert!(
        after["report"]["score"].as_i64().expect("score") > score_before,
        "acknowledged high finding stops costing points: {after}"
    );
    assert_eq!(after["report"]["counts"]["high"], 0, "{after}");

    let resp = srv.admin_do(Method::DELETE, uri, "").await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
    let resp = srv.admin_do(Method::DELETE, uri, "").await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND, "{}", resp.text());
    let again = srv.admin_do(Method::GET, "/insights", "").await.json();
    assert_eq!(
        again["report"]["score"].as_i64(),
        Some(score_before),
        "{again}"
    );
}

#[tokio::test]
async fn check_history_records_each_run() {
    let srv = new_test_server().await;
    let analyzer = Analyzer::new(Arc::clone(&srv.store), None, Duration::from_hours(1));
    srv.handler
        .set_insights(Arc::clone(&analyzer), Arc::new(|_: &str| true));
    let body = r#"{"name":"open","format":"raw","type":"hosted","config":{"public":true}}"#;
    srv.admin_do(Method::POST, "/repositories", body).await;
    analyzer.run_once("test").await.expect("first run");
    let uri = "/insights/checks/public-repositories/acknowledgement";
    srv.admin_do(Method::PUT, uri, r#"{"note":"intended"}"#)
        .await;
    analyzer.run_once("test").await.expect("second run");

    let resp = srv
        .admin_do(
            Method::GET,
            "/insights/checks/public-repositories/history",
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let points = resp.json()["points"].clone();
    let statuses: Vec<&str> = points
        .as_array()
        .expect("points")
        .iter()
        .map(|p| p["status"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(statuses, ["action", "acknowledged"], "{points}");
    assert_eq!(points[0]["flagged"], 1, "{points}");
    assert_eq!(points[0]["severity"], "high", "{points}");

    let resp = srv
        .admin_do(
            Method::GET,
            "/insights/checks/public-repositories/history?days=0",
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
    mk_read_user(&srv.store, "dev", "*").await;
    let resp = srv
        .do_as(
            "dev",
            USER_PASS,
            Method::GET,
            "/insights/checks/public-repositories/history",
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
}

#[tokio::test]
async fn insight_settings_drive_the_scheduled_alarm() {
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let srv = new_test_server().await;
    let analyzer = Analyzer::new(Arc::clone(&srv.store), None, Duration::from_hours(1));
    srv.handler
        .set_insights(Arc::clone(&analyzer), Arc::new(|_: &str| true));

    let defaults = srv.admin_do(Method::GET, "/insights/settings", "").await;
    assert_eq!(defaults.status, StatusCode::OK, "{}", defaults.text());
    let defaults = defaults.json();
    assert_eq!(defaults["auto_analyze_enabled"], true, "{defaults}");
    assert_eq!(defaults["interval_minutes"], 60, "{defaults}");
    assert_eq!(defaults["report_enabled"], false, "{defaults}");
    assert_eq!(defaults["min_severity"], "high", "{defaults}");
    assert_ne!(defaults["next_analysis_at"], "", "{defaults}");
    assert_eq!(defaults["next_report_at"], "", "{defaults}");

    for body in [
        r#"{"interval_minutes":1}"#,
        r#"{"report_cron":"not a cron"}"#,
        r#"{"timezone":"Mars/Base"}"#,
        r#"{"receiver":"nobody"}"#,
        r#"{"min_severity":"urgent"}"#,
    ] {
        let resp = srv.admin_do(Method::PUT, "/insights/settings", body).await;
        assert_eq!(
            resp.status,
            StatusCode::BAD_REQUEST,
            "{body}: {}",
            resp.text()
        );
    }

    mk_read_user(&srv.store, "dev", "*").await;
    for m in [Method::GET, Method::PUT] {
        let resp = srv
            .do_as("dev", USER_PASS, m, "/insights/settings", "{}")
            .await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
    }

    let hook = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("insights.report"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&hook)
        .await;
    let body = format!(
        r#"{{"name":"sec-alerts","webhook_url":"{}/hook","enabled":true}}"#,
        hook.uri()
    );
    let created = srv
        .admin_do(Method::POST, "/notification/receivers", &body)
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text());

    let saved = srv
        .admin_do(
            Method::PUT,
            "/insights/settings",
            r#"{"interval_minutes":30,"report_enabled":true,"receiver":"sec-alerts","timezone":"Asia/Seoul","skip_when_unchanged":true}"#,
        )
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.text());
    let saved = saved.json();
    assert_eq!(saved["interval_minutes"], 30, "{saved}");
    assert_eq!(saved["updated_by"], "admin", "{saved}");
    assert_ne!(saved["next_report_at"], "", "{saved}");
    let overview = srv.admin_do(Method::GET, "/insights", "").await.json();
    assert_eq!(overview["interval_seconds"], 1800, "{overview}");

    let notifier = forklift::notify::Notifier::new(Duration::from_secs(2));
    let now = chrono::Utc::now();
    let settings = analyzer.settings().await.expect("settings");
    assert!(
        !analyzer
            .send_scheduled_report(&settings, &notifier, now)
            .await
            .expect("send"),
        "nothing analyzed yet"
    );

    let body = r#"{"name":"open","format":"raw","type":"hosted","config":{"public":true}}"#;
    srv.admin_do(Method::POST, "/repositories", body).await;
    analyzer.run_once("test").await.expect("analyze");
    assert!(
        analyzer
            .send_scheduled_report(&settings, &notifier, now)
            .await
            .expect("send"),
        "a high finding is open"
    );
    assert!(
        !analyzer
            .send_scheduled_report(&settings, &notifier, now)
            .await
            .expect("send"),
        "nothing new since the last alarm"
    );
    let mut critical_only = settings.clone();
    critical_only.skip_when_unchanged = false;
    critical_only.min_severity = forklift::insights::Severity::Critical;
    assert!(
        !analyzer
            .send_scheduled_report(&critical_only, &notifier, now)
            .await
            .expect("send"),
        "nothing critical"
    );
}
