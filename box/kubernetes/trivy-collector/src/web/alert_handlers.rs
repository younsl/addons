//! HTTP handlers for `/api/v1/alerts` (RBAC-gated CRUD over the alerts ConfigMap).

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use axum_extra::extract::PrivateCookieJar;
use serde::Deserialize;
use tracing::{error, info};

use crate::alerts::evaluator::TestRunError;
use crate::alerts::preview::{self, PreviewResult};
use crate::alerts::types::{AlertRule, Matchers, Receiver, validate_webhook_url};
use crate::alerts::{AlertEvaluator, AlertStore, AlertStoreError};
use crate::auth::session::{AuthSession, SESSION_COOKIE_NAME};
use crate::web::AppState;
use std::collections::BTreeMap;

#[derive(Deserialize, utoipa::ToSchema)]
pub struct AlertRuleInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub matchers: Matchers,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    pub receivers: Vec<Receiver>,
    pub cooldown_secs: Option<u64>,
}

fn default_true() -> bool {
    true
}

#[utoipa::path(
    get,
    path = "/api/v1/alerts",
    tag = "Alerts",
    responses((status = 200, description = "Alert rules"))
)]
pub async fn list_alerts(State(state): State<AppState>) -> impl IntoResponse {
    let store = match get_store(&state) {
        Some(s) => s,
        None => return unavailable(),
    };
    match store.list().await {
        Ok(rules) => Json(serde_json::json!({
            "items": rules,
            "total": rules.len(),
            "configmap": store.configmap_name(),
            "namespace": store.namespace(),
        }))
        .into_response(),
        Err(e) => store_error_response(e),
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/alerts/{name}",
    tag = "Alerts",
    params(("name" = String, Path, description = "Rule name")),
    responses((status = 200, description = "Alert rule"), (status = 404, description = "Not found"))
)]
pub async fn get_alert(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let store = match get_store(&state) {
        Some(s) => s,
        None => return unavailable(),
    };
    match store.get(&name).await {
        Ok(rule) => Json(rule).into_response(),
        Err(e) => store_error_response(e),
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/alerts",
    tag = "Alerts",
    request_body = AlertRuleInput,
    responses((status = 201, description = "Created"), (status = 400, description = "Invalid input"))
)]
pub async fn create_alert(
    State(state): State<AppState>,
    cookie_jar: PrivateCookieJar,
    Json(input): Json<AlertRuleInput>,
) -> impl IntoResponse {
    let store = match get_store(&state) {
        Some(s) => s,
        None => return unavailable(),
    };
    if let Err((name, msg)) = validate_receivers(&input.receivers) {
        return webhook_validation_error(&name, msg);
    }
    let user = current_user(&cookie_jar);
    let now = chrono::Utc::now().to_rfc3339();
    let rule = AlertRule {
        name: input.name,
        description: input.description,
        enabled: input.enabled,
        matchers: input.matchers,
        labels: input.labels,
        annotations: input.annotations,
        receivers: input.receivers,
        cooldown_secs: input.cooldown_secs,
        created_at: now,
        created_by: user,
        updated_at: None,
        updated_by: None,
    };
    match store.upsert(&rule).await {
        Ok(()) => {
            info!(rule = %rule.name, by = %rule.created_by, "Alert rule created");
            (StatusCode::CREATED, Json(rule)).into_response()
        }
        Err(e) => store_error_response(e),
    }
}

#[utoipa::path(
    put,
    path = "/api/v1/alerts/{name}",
    tag = "Alerts",
    params(("name" = String, Path, description = "Rule name")),
    request_body = AlertRuleInput,
    responses((status = 200, description = "Updated"), (status = 404, description = "Not found"))
)]
pub async fn update_alert(
    State(state): State<AppState>,
    cookie_jar: PrivateCookieJar,
    Path(name): Path<String>,
    Json(input): Json<AlertRuleInput>,
) -> impl IntoResponse {
    if input.name != name {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "name in path and body must match"})),
        )
            .into_response();
    }
    let store = match get_store(&state) {
        Some(s) => s,
        None => return unavailable(),
    };
    if let Err((name, msg)) = validate_receivers(&input.receivers) {
        return webhook_validation_error(&name, msg);
    }
    let user = current_user(&cookie_jar);
    let existing = match store.get(&name).await {
        Ok(r) => r,
        Err(e) => return store_error_response(e),
    };
    let now = chrono::Utc::now().to_rfc3339();
    let rule = AlertRule {
        name: input.name,
        description: input.description,
        enabled: input.enabled,
        matchers: input.matchers,
        labels: input.labels,
        annotations: input.annotations,
        receivers: input.receivers,
        cooldown_secs: input.cooldown_secs,
        created_at: existing.created_at,
        created_by: existing.created_by,
        updated_at: Some(now),
        updated_by: Some(user),
    };
    match store.upsert(&rule).await {
        Ok(()) => {
            info!(rule = %rule.name, by = ?rule.updated_by, "Alert rule updated");
            Json(rule).into_response()
        }
        Err(e) => store_error_response(e),
    }
}

#[utoipa::path(
    delete,
    path = "/api/v1/alerts/{name}",
    tag = "Alerts",
    params(("name" = String, Path, description = "Rule name")),
    responses((status = 204, description = "Deleted"), (status = 404, description = "Not found"))
)]
pub async fn delete_alert(
    State(state): State<AppState>,
    cookie_jar: PrivateCookieJar,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let store = match get_store(&state) {
        Some(s) => s,
        None => return unavailable(),
    };
    let user = current_user(&cookie_jar);
    match store.delete(&name).await {
        Ok(()) => {
            info!(rule = %name, by = %user, "Alert rule deleted");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => store_error_response(e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct PreviewRequest {
    pub matchers: Matchers,
}

#[utoipa::path(
    post,
    path = "/api/v1/alerts/preview",
    tag = "Alerts",
    request_body = PreviewRequest,
    responses(
        (status = 200, description = "Matching items in current data", body = PreviewResult),
        (status = 400, description = "Invalid matcher")
    )
)]
pub async fn preview_alert(
    State(state): State<AppState>,
    Json(req): Json<PreviewRequest>,
) -> impl IntoResponse {
    match preview::run(state.store.as_ref(), &req.matchers).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/alerts/test",
    tag = "Alerts",
    request_body = AlertRuleInput,
    responses(
        (status = 200, description = "Test dispatch results per receiver"),
        (status = 400, description = "Invalid input")
    )
)]
pub async fn test_alert_draft(
    State(state): State<AppState>,
    cookie_jar: PrivateCookieJar,
    Json(input): Json<AlertRuleInput>,
) -> impl IntoResponse {
    let evaluator = match get_evaluator(&state) {
        Some(e) => e,
        None => return unavailable(),
    };
    if input.receivers.iter().all(|r| r.slack.is_none()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "at least one Slack receiver is required to test"})),
        )
            .into_response();
    }
    if let Err((name, msg)) = validate_receivers(&input.receivers) {
        return webhook_validation_error(&name, msg);
    }
    let user = current_user(&cookie_jar);
    let now = chrono::Utc::now().to_rfc3339();
    let draft = AlertRule {
        name: if input.name.trim().is_empty() {
            "draft".to_string()
        } else {
            input.name
        },
        description: input.description,
        enabled: input.enabled,
        matchers: input.matchers,
        labels: input.labels,
        annotations: input.annotations,
        receivers: input.receivers,
        cooldown_secs: input.cooldown_secs,
        created_at: now,
        created_by: user.clone(),
        updated_at: None,
        updated_by: None,
    };
    let rule_name = draft.name.clone();
    match evaluator.test_with_rule(draft, state.store.as_ref()).await {
        Ok(results) => {
            let total = results.len();
            let succeeded = results.iter().filter(|r| r.success).count();
            info!(
                rule = %rule_name,
                by = %user,
                receivers = total,
                succeeded,
                "Alert draft test dispatched"
            );
            Json(serde_json::json!({
                "rule": rule_name,
                "total": total,
                "succeeded": succeeded,
                "results": results,
            }))
            .into_response()
        }
        Err(TestRunError::NoMatches) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "no current reports match these matchers — nothing realistic to send. Adjust matchers or wait for a matching report.",
            })),
        )
            .into_response(),
        Err(TestRunError::InvalidExpr(msg)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("invalid version expression: {msg}")})),
        )
            .into_response(),
        Err(TestRunError::Storage(msg)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("storage error: {msg}")})),
        )
            .into_response(),
    }
}

/// Reject any receiver whose Slack webhook URL is not a canonical Slack
/// hooks endpoint. Without this, a rule author could (accidentally or
/// maliciously) point the alerts subsystem at an arbitrary internal URL
/// and use the trivy-collector pod as an SSRF probe. Returns the offending
/// receiver name and validator error so the caller can build a 400
/// response.
fn validate_receivers(receivers: &[Receiver]) -> Result<(), (String, &'static str)> {
    for r in receivers {
        if let Some(slack) = &r.slack
            && let Err(msg) = validate_webhook_url(&slack.webhook_url)
        {
            return Err((r.name.clone(), msg));
        }
    }
    Ok(())
}

fn webhook_validation_error(name: &str, msg: &str) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": format!("receiver '{}': {}", name, msg),
        })),
    )
        .into_response()
}

fn get_store(state: &AppState) -> Option<&AlertStore> {
    state.alerts.as_ref().map(|e| e.store())
}

fn get_evaluator(state: &AppState) -> Option<&AlertEvaluator> {
    state.alerts.as_ref().map(|e| e.as_ref())
}

fn unavailable() -> axum::response::Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": "alerts subsystem unavailable (no Kubernetes API access)"
        })),
    )
        .into_response()
}

fn current_user(jar: &PrivateCookieJar) -> String {
    jar.get(SESSION_COOKIE_NAME)
        .and_then(|c| serde_json::from_str::<AuthSession>(c.value()).ok())
        .and_then(|s| {
            if s.is_expired() {
                None
            } else {
                Some(s.email.unwrap_or(s.sub))
            }
        })
        .unwrap_or_else(|| "anonymous".to_string())
}

fn store_error_response(err: AlertStoreError) -> axum::response::Response {
    match err {
        AlertStoreError::NotFound(n) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("rule '{}' not found", n)})),
        )
            .into_response(),
        AlertStoreError::Invalid(msg) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": msg})),
        )
            .into_response(),
        e => {
            error!(error = %e, "Alert store error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::types::SlackReceiver;
    use crate::web::test_support;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::{get, post};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// Alert rules live in a ConfigMap, so every route needs a Kubernetes
    /// client. Without one `state.alerts` is `None`, which is the path these
    /// tests pin: the API must say the subsystem is unavailable rather than
    /// answer as if there were no rules.
    async fn router_without_alerts() -> Router {
        let state = test_support::app_state().await;
        assert!(state.alerts.is_none());
        Router::new()
            .route("/api/v1/alerts", get(list_alerts).post(create_alert))
            .route("/api/v1/alerts/preview", post(preview_alert))
            .route("/api/v1/alerts/test", post(test_alert_draft))
            .route(
                "/api/v1/alerts/{name}",
                get(get_alert).put(update_alert).delete(delete_alert),
            )
            .with_state(state)
    }

    fn slack_receiver(name: &str, webhook_url: &str) -> Receiver {
        Receiver {
            name: name.to_string(),
            slack: Some(SlackReceiver {
                webhook_url: webhook_url.to_string(),
                channel: None,
                title: None,
            }),
        }
    }

    async fn send(method: &str, uri: &str, body: Option<&str>) -> axum::response::Response {
        let builder = Request::builder().method(method).uri(uri);
        let request = match body {
            Some(b) => builder
                .header("content-type", "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        router_without_alerts()
            .await
            .oneshot(request)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn every_route_reports_unavailable_without_a_kubernetes_client() {
        let draft = serde_json::json!({
            "name": "log4j",
            "description": "",
            "enabled": true,
            "matchers": {"package_name": "log4j-core"},
            "labels": {},
            "annotations": {},
            "receivers": [{
                "name": "sec",
                "slack": {"webhook_url": "https://hooks.slack.com/services/T0/B0/x"}
            }],
            "cooldown_secs": null
        })
        .to_string();

        for (method, uri, body) in [
            ("GET", "/api/v1/alerts", None),
            ("GET", "/api/v1/alerts/log4j", None),
            ("POST", "/api/v1/alerts", Some(draft.as_str())),
            ("PUT", "/api/v1/alerts/log4j", Some(draft.as_str())),
            ("DELETE", "/api/v1/alerts/log4j", None),
            ("POST", "/api/v1/alerts/test", Some(draft.as_str())),
        ] {
            let resp = send(method, uri, body).await;
            assert_eq!(
                resp.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "{method} {uri}"
            );
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert!(
                json["error"].as_str().unwrap().contains("unavailable"),
                "{method} {uri} must explain why"
            );
        }
    }

    #[tokio::test]
    async fn preview_runs_off_the_report_store_not_the_alert_store() {
        // Preview only reads reports, so it works even with no Kubernetes API.
        let resp = send(
            "POST",
            "/api/v1/alerts/preview",
            Some(r#"{"matchers":{"package_name":"log4j-core"}}"#),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["total"], 0);
        assert_eq!(json["scanned_reports"], 0);
    }

    #[tokio::test]
    async fn preview_rejects_an_unparseable_version_expression() {
        let resp = send(
            "POST",
            "/api/v1/alerts/preview",
            Some(r#"{"matchers":{"package_name":"axios","version_expr":">="}}"#),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json["error"].as_str().unwrap().contains("version_expr"));
    }

    #[test]
    fn receiver_validation_names_the_offending_receiver() {
        let receivers = vec![
            slack_receiver("good", "https://hooks.slack.com/services/T0/B0/x"),
            slack_receiver("bad", "http://evil.example.com/hook"),
        ];
        let (name, msg) = validate_receivers(&receivers).expect_err("must reject");
        assert_eq!(name, "bad");
        assert!(msg.contains("hooks.slack.com"));
    }

    #[test]
    fn receiver_validation_accepts_canonical_webhooks_and_no_slack_block() {
        assert!(
            validate_receivers(&[slack_receiver(
                "sec",
                "https://hooks.slack.com/services/T0/B0/x"
            )])
            .is_ok()
        );
        // A receiver with no Slack block has no URL to validate.
        assert!(
            validate_receivers(&[Receiver {
                name: "noop".to_string(),
                slack: None,
            }])
            .is_ok()
        );
        assert!(validate_receivers(&[]).is_ok());
    }

    #[test]
    fn receiver_validation_rejects_an_empty_webhook() {
        let (name, msg) =
            validate_receivers(&[slack_receiver("sec", "")]).expect_err("must reject");
        assert_eq!(name, "sec");
        assert!(msg.contains("empty"));
    }

    #[tokio::test]
    async fn webhook_errors_identify_the_receiver() {
        let resp = webhook_validation_error("sec", "webhook URL must not be empty");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap()
                .starts_with("receiver 'sec'")
        );
    }

    #[tokio::test]
    async fn store_errors_map_onto_their_status_codes() {
        for (err, expected) in [
            (
                AlertStoreError::NotFound("log4j".into()),
                StatusCode::NOT_FOUND,
            ),
            (
                AlertStoreError::Invalid("bad matcher".into()),
                StatusCode::BAD_REQUEST,
            ),
        ] {
            assert_eq!(store_error_response(err).status(), expected);
        }

        // Anything else is ours, not the caller's.
        let serde_err = serde_json::from_str::<AlertRule>("{").unwrap_err();
        assert_eq!(
            store_error_response(AlertStoreError::Serde(serde_err)).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn an_absent_session_is_attributed_to_anonymous() {
        // Audit fields must never be blank, so the fallback is explicit.
        let jar = PrivateCookieJar::new(cookie::Key::generate());
        assert_eq!(current_user(&jar), "anonymous");
    }
}
