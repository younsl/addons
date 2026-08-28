//! The read-only API behind the Argo CD UI extension.
//!
//! The extension itself only renders. It cannot block a sync. Its value is
//! telling someone why the gate will refuse before they press Sync, and it does
//! that by asking this endpoint for the same verdict the webhook would return.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;

use crate::engine::Engine;

/// Identifies the Application a proxy-extension request is about, formatted as
/// `<namespace>:<name>`.
///
/// argocd-server does not add this. It requires the caller to send it, uses it
/// to authorize the request against Argo CD RBAC, and rejects the call with a
/// plain text "Invalid headers" when it is missing. The UI extension supplies
/// it from the application it is rendering.
const APP_HEADER: &str = "Argocd-Application-Name";

/// Builds the router serving the gate preview and the effective configuration.
pub fn router(engine: Arc<Engine>) -> Router {
    Router::new()
        .route("/api/v1/gate", get(gate))
        .route("/api/v1/config", get(config))
        .with_state(engine)
}

/// Returns the verdict for one Application.
async fn gate(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(app_name) = app_name_from(&headers, &query) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("neither the {APP_HEADER} header nor an app query parameter was set")
            })),
        )
            .into_response();
    };

    let snapshot = match engine.reader().get(&app_name).await {
        Ok(snapshot) => snapshot,
        Err(err) => {
            tracing::warn!(app = %app_name, error = %err, "gate preview lookup failed");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": err.to_string()})),
            )
                .into_response();
        }
    };
    let Some(snapshot) = snapshot else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("application {app_name} not found")})),
        )
            .into_response();
    };

    Json(engine.evaluate(snapshot).await).into_response()
}

/// Publishes the effective promotion chain so the extension can label the
/// upstream environment without hardcoding the order.
async fn config(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    let cfg = engine.config();
    Json(json!({
        "chain": cfg.chain,
        "gatedEnvs": cfg.gated_envs,
        "requireSync": cfg.require.sync,
        "requireHealth": cfg.require.health,
        "imageTagEnabled": cfg.image_tag.enabled,
        "imageTagMode": cfg.image_tag.mode.as_str(),
        "skipAnnotation": cfg.exempt.annotation,
    }))
}

/// Resolves the Application name from the header the UI extension sends,
/// falling back to an explicit query parameter for local debugging and for
/// calls that bypass argocd-server.
fn app_name_from(headers: &HeaderMap, query: &HashMap<String, String>) -> Option<String> {
    if let Some(raw) = headers.get(APP_HEADER).and_then(|v| v.to_str().ok()) {
        // The header is "<namespace>:<name>". Only the name matters because
        // the reader is already scoped to the Argo CD namespace.
        let name = raw.split_once(':').map_or(raw, |(_, name)| name).trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    query
        .get("app")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;
    use crate::config::Config;
    use crate::engine::testing::{FakeReader, snapshot};
    use crate::observability::Metrics;

    fn engine(reader: FakeReader) -> Arc<Engine> {
        let cfg =
            Config::parse("chain: [stg, prd]\ngatedEnvs: [prd]\nimageTag:\n  enabled: false\n")
                .unwrap();
        Arc::new(Engine::new(
            cfg,
            Arc::new(reader),
            None,
            Arc::new(Metrics::new()),
        ))
    }

    async fn get_json(engine: Arc<Engine>, uri: &str, header: Option<&str>) -> (StatusCode, Value) {
        let mut req = Request::builder().uri(uri);
        if let Some(h) = header {
            req = req.header(APP_HEADER, h);
        }
        let response = router(engine)
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn gate_resolves_app_from_header_or_query() {
        let apps = vec![
            snapshot("prd-api", "prd", "OutOfSync", "Healthy", &[]),
            snapshot("stg-api", "stg", "Synced", "Healthy", &[]),
        ];
        let eng = engine(FakeReader::with(apps));

        let (status, body) =
            get_json(Arc::clone(&eng), "/api/v1/gate", Some("argocd:prd-api")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "Passed");
        assert_eq!(body["upstream"]["app"], "stg-api");

        let (status, body) = get_json(Arc::clone(&eng), "/api/v1/gate?app=stg-api", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "NotGated");

        let (status, body) = get_json(Arc::clone(&eng), "/api/v1/gate?app=nope", Some(" ")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap().contains("nope"));

        let (status, body) = get_json(Arc::clone(&eng), "/api/v1/gate", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains(APP_HEADER));
    }

    #[tokio::test]
    async fn gate_reports_reader_failures_as_bad_gateway() {
        let eng = engine(FakeReader {
            fail: true,
            ..FakeReader::default()
        });
        let (status, body) = get_json(eng, "/api/v1/gate?app=prd-api", None).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body["error"].is_string());
    }

    #[tokio::test]
    async fn config_publishes_the_chain() {
        let (status, body) = get_json(engine(FakeReader::default()), "/api/v1/config", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["chain"], json!(["stg", "prd"]));
        assert_eq!(body["gatedEnvs"], json!(["prd"]));
        assert_eq!(body["requireSync"], true);
        assert_eq!(body["imageTagEnabled"], false);
        assert_eq!(body["imageTagMode"], "warn");
        assert_eq!(
            body["skipAnnotation"],
            "promotion-gate.younsl.github.io/skip"
        );
    }
}
