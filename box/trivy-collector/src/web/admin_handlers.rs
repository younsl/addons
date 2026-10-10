//! Admin API handlers.
//!
//! The API-log listing that used to live here is gone along with the table that
//! backed it: requests are structured stdout lines now, queried through the
//! cluster's log pipeline. What remains describes the state this pod actually
//! holds — the RBAC policy in force and the two Kubernetes objects carrying
//! authored state.

use axum::{Json, extract::State, response::IntoResponse};

use crate::web::AppState;

/// GET /api/v1/admin/info — Admin info summary
#[utoipa::path(
    get,
    path = "/api/v1/admin/info",
    tag = "Admin",
    responses(
        (status = 200, description = "Admin info summary"),
    )
)]
pub async fn admin_info(State(state): State<AppState>) -> impl IntoResponse {
    let notes = state.notes.as_ref().map(|n| {
        serde_json::json!({
            "configmap": n.configmap_name(),
            "namespace": n.namespace(),
            "count": n.cache().len(),
            "bytes": n.cache().bytes(),
            "max_bytes": crate::storage::notes::MAX_TOTAL_BYTES,
        })
    });

    let tokens = state.tokens.as_ref().map(|t| {
        serde_json::json!({
            "secret": t.secret_name(),
            "namespace": t.namespace(),
            "count": t.cache().len(),
        })
    });

    let hydration = state.store.hydration().await.ok();

    Json(serde_json::json!({
        "rbac": { "default_policy": state.rbac.default_policy_name() },
        "scraper_url": state.config.scraper_url,
        "notes": notes,
        "api_tokens": tokens,
        "hydration": hydration,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, http::Request, http::StatusCode, routing::get};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn admin_info_reports_rbac_and_absent_stores() {
        let state = crate::web::test_support::app_state().await;
        let app = Router::new()
            .route("/api/v1/admin/info", get(admin_info))
            .with_state(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/admin/info")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(json["rbac"]["default_policy"], "role:readonly");
        // Without a Kubernetes API the authored-state stores are absent, and
        // that is reported rather than faked as empty.
        assert!(json["notes"].is_null());
        assert!(json["api_tokens"].is_null());
        // A direct-Database store is its own source of truth.
        assert_eq!(json["hydration"]["hydrated"], true);
    }
}
