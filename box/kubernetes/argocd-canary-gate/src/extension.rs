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
use crate::k8s::AppReader;

/// Identifies the Application a proxy-extension request is about, formatted as
/// `<namespace>:<name>`.
///
/// argocd-server does not add this. It requires the caller to send it, uses it
/// to authorize the request against Argo CD RBAC, and rejects the call with a
/// plain text "Invalid headers" when it is missing. The UI extension supplies
/// it from the application it is rendering.
const APP_HEADER: &str = "Argocd-Application-Name";

/// Everything the preview endpoint needs. The reader is what the webhook path
/// does not have: there the Application arrives inside the `AdmissionReview`,
/// here only its name does.
pub struct ExtensionState {
    pub engine: Arc<Engine>,
    pub reader: Arc<dyn AppReader>,
}

/// Builds the router serving the gate preview and the effective configuration.
pub fn router(state: Arc<ExtensionState>) -> Router {
    Router::new()
        .route("/api/v1/gate", get(gate))
        .route("/api/v1/config", get(config))
        .with_state(state)
}

/// Returns the verdict for one Application.
async fn gate(
    State(state): State<Arc<ExtensionState>>,
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

    let snapshot = match state.reader.get(&app_name).await {
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

    Json(state.engine.evaluate(snapshot).await).into_response()
}

/// Publishes the effective policy so the extension can explain the gate
/// without hardcoding its configuration.
async fn config(State(state): State<Arc<ExtensionState>>) -> Json<serde_json::Value> {
    let cfg = state.engine.config();
    Json(json!({
        "mode": cfg.mode.as_str(),
        "onError": cfg.on_error.as_str(),
        "trackingLabel": cfg.rollouts.tracking_label,
        "strategies": cfg.rollouts.strategies,
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
    use crate::engine::testing::{FakeReader, rollout};
    use crate::gate::AppSnapshot;
    use crate::k8s::RolloutReader;
    use crate::k8s::application::AppReadError;
    use crate::observability::Metrics;

    /// An [`AppReader`] over a fixed set of Applications, optionally failing.
    #[derive(Default)]
    struct FakeApps {
        apps: Vec<AppSnapshot>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl AppReader for FakeApps {
        async fn get(&self, name: &str) -> Result<Option<AppSnapshot>, AppReadError> {
            if self.fail {
                return Err(AppReadError::Snapshot(
                    crate::k8s::application::SnapshotError::MissingName,
                ));
            }
            Ok(self.apps.iter().find(|a| a.name == name).cloned())
        }
    }

    fn state(apps: FakeApps, rollouts: FakeReader) -> Arc<ExtensionState> {
        let cfg = Config::parse("{}").unwrap();
        let engine = Arc::new(Engine::new(
            cfg,
            Arc::new(rollouts) as Arc<dyn RolloutReader>,
            Arc::new(Metrics::new()),
        ));
        Arc::new(ExtensionState {
            engine,
            reader: Arc::new(apps),
        })
    }

    fn app(name: &str) -> AppSnapshot {
        AppSnapshot {
            name: name.to_string(),
            dest_namespace: "payments".to_string(),
            skip_requested: false,
        }
    }

    async fn get_json(
        state: Arc<ExtensionState>,
        uri: &str,
        header: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().uri(uri);
        if let Some(h) = header {
            req = req.header(APP_HEADER, h);
        }
        let response = router(state)
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn gate_resolves_app_from_header_or_query() {
        let apps = FakeApps {
            apps: vec![app("prd-api")],
            ..FakeApps::default()
        };
        let st = state(apps, FakeReader::with(vec![rollout("api", "a", "b")]));

        let (status, body) =
            get_json(Arc::clone(&st), "/api/v1/gate", Some("argocd:prd-api")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "CanaryInProgress");
        assert_eq!(body["allowed"], false);
        assert_eq!(body["rollouts"][0]["name"], "api");

        let (status, body) = get_json(Arc::clone(&st), "/api/v1/gate?app=prd-api", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "CanaryInProgress");

        let (status, body) = get_json(Arc::clone(&st), "/api/v1/gate?app=nope", Some(" ")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap().contains("nope"));

        let (status, body) = get_json(Arc::clone(&st), "/api/v1/gate", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains(APP_HEADER));
    }

    #[tokio::test]
    async fn gate_reports_reader_failures_as_bad_gateway() {
        let st = state(
            FakeApps {
                fail: true,
                ..FakeApps::default()
            },
            FakeReader::default(),
        );
        let (status, body) = get_json(st, "/api/v1/gate?app=prd-api", None).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body["error"].is_string());
    }

    #[tokio::test]
    async fn config_publishes_the_policy() {
        let st = state(FakeApps::default(), FakeReader::default());
        let (status, body) = get_json(st, "/api/v1/config", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["mode"], "enforce");
        assert_eq!(body["onError"], "deny");
        assert_eq!(body["trackingLabel"], "argocd.argoproj.io/instance");
        assert_eq!(body["strategies"], serde_json::json!(["canary"]));
        assert_eq!(body["skipAnnotation"], "canary-gate.younsl.github.io/skip");
    }
}
