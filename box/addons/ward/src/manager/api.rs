//! Manager HTTP APIs: the agent API behind mutual TLS, and the read-only
//! admin API behind a bearer token.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, Utc};
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use serde::Serialize;
use subtle::ConstantTimeEq as _;

use super::relay::Relay;
use super::store::{self, Store};
use crate::policy::Detection;

pub const NODE_HEADER: &str = "x-ward-node";
const MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct HttpLabels {
    pub endpoint: String,
    pub code: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct RevisionLabels {
    pub revision: String,
}

#[derive(Clone)]
pub struct ManagerMetrics {
    pub received: Counter,
    pub stored: Gauge,
    pub served: Family<RevisionLabels, Counter>,
    pub http: Family<HttpLabels, Counter>,
}

impl ManagerMetrics {
    pub fn register(reg: &mut Registry) -> Self {
        let m = Self {
            received: Counter::default(),
            stored: Gauge::default(),
            served: Family::default(),
            http: Family::default(),
        };
        reg.register(
            "ward_manager_detections_received",
            "Detections posted by agents",
            m.received.clone(),
        );
        reg.register(
            "ward_manager_detections_stored",
            "Detections in the store",
            m.stored.clone(),
        );
        reg.register(
            "ward_manager_content_served",
            "Database downloads served to agents",
            m.served.clone(),
        );
        reg.register(
            "ward_manager_http_requests",
            "API requests by status code",
            m.http.clone(),
        );
        m
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentSeen {
    pub node: String,
    pub last_seen: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_content: Option<String>,
}

pub struct AppState {
    pub store: Arc<Store>,
    pub relay: Arc<Relay>,
    pub agents: RwLock<BTreeMap<String, AgentSeen>>,
    pub token: String,
    pub metrics: ManagerMetrics,
}

type Shared = Arc<AppState>;

impl AppState {
    fn count(&self, endpoint: &str, code: StatusCode) {
        self.metrics
            .http
            .get_or_create(&HttpLabels {
                endpoint: endpoint.into(),
                code: code.as_u16().to_string(),
            })
            .inc();
    }

    fn seen(&self, headers: &HeaderMap, content: Option<&str>) {
        let Some(node) = headers.get(NODE_HEADER).and_then(|v| v.to_str().ok()) else {
            return;
        };
        let node: String = node.chars().take(253).collect();
        if let Ok(mut a) = self.agents.write() {
            let entry = a.entry(node.clone()).or_insert_with(|| AgentSeen {
                node,
                last_seen: Utc::now(),
                last_content: None,
            });
            entry.last_seen = Utc::now();
            if let Some(c) = content {
                entry.last_content = Some(c.to_string());
            }
        }
    }
}

async fn content(
    State(s): State<Shared>,
    Path(digest): Path<String>,
    headers: HeaderMap,
) -> Response {
    s.seen(&headers, Some(&digest));
    let Some(blob) = s.relay.get(&digest) else {
        s.count("content", StatusCode::NOT_FOUND);
        return StatusCode::NOT_FOUND.into_response();
    };
    s.count("content", StatusCode::OK);
    s.metrics
        .served
        .get_or_create(&RevisionLabels {
            revision: blob.revision.clone(),
        })
        .inc();
    (
        [(header::CONTENT_TYPE, crate::content::MEDIA_TYPE_DB)],
        blob.db.as_ref().clone(),
    )
        .into_response()
}

async fn signature(State(s): State<Shared>, Path(digest): Path<String>) -> Response {
    match s.relay.get(&digest) {
        Some(blob) => {
            s.count("signature", StatusCode::OK);
            blob.sig.as_ref().clone().into_response()
        }
        None => {
            s.count("signature", StatusCode::NOT_FOUND);
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

async fn post_detections(
    State(s): State<Shared>,
    headers: HeaderMap,
    Json(batch): Json<Vec<Detection>>,
) -> StatusCode {
    s.seen(&headers, None);
    s.metrics.received.inc_by(batch.len() as u64);
    s.store.add(batch);
    #[allow(clippy::cast_possible_wrap)]
    s.metrics.stored.set(s.store.len() as i64);
    s.count("post_detections", StatusCode::ACCEPTED);
    StatusCode::ACCEPTED
}

fn authorized(s: &AppState, headers: &HeaderMap) -> bool {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    !s.token.is_empty() && bool::from(presented.as_bytes().ct_eq(s.token.as_bytes()))
}

async fn get_detections(
    State(s): State<Shared>,
    headers: HeaderMap,
    Query(q): Query<store::Query>,
) -> Response {
    if !authorized(&s, &headers) {
        s.count("detections", StatusCode::UNAUTHORIZED);
        return StatusCode::UNAUTHORIZED.into_response();
    }
    s.count("detections", StatusCode::OK);
    Json(s.store.query(&q)).into_response()
}

async fn get_agents(State(s): State<Shared>, headers: HeaderMap) -> Response {
    if !authorized(&s, &headers) {
        s.count("agents", StatusCode::UNAUTHORIZED);
        return StatusCode::UNAUTHORIZED.into_response();
    }
    s.count("agents", StatusCode::OK);
    let agents: Vec<AgentSeen> = s
        .agents
        .read()
        .map(|a| a.values().cloned().collect())
        .unwrap_or_default();
    Json(agents).into_response()
}

/// Served behind mutual TLS to agents.
pub fn agent_router(state: Shared) -> Router {
    Router::new()
        .route("/v1/content/{digest}", get(content))
        .route("/v1/content/{digest}/signature", get(signature))
        .route("/v1/detections", axum::routing::post(post_detections))
        .layer(DefaultBodyLimit::max(MAX_BATCH_BYTES))
        .with_state(state)
}

/// Read-only queries for operators.
pub fn admin_router(state: Shared) -> Router {
    Router::new()
        .route("/v1/detections", get(get_detections))
        .route("/v1/agents", get(get_agents))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use tokio::sync::mpsc;
    use tower::ServiceExt as _;

    use super::*;
    use crate::policy::{Engine, Severity};

    fn state() -> Shared {
        let (tx, _rx) = mpsc::unbounded_channel();
        Arc::new(AppState {
            store: Arc::new(Store::new(VecDeque::new(), 100, tx)),
            relay: Arc::new(Relay::default()),
            agents: RwLock::new(BTreeMap::new()),
            token: "secret".into(),
            metrics: ManagerMetrics::register(&mut Registry::default()),
        })
    }

    async fn call(app: &Router, req: Request<Body>) -> (StatusCode, String) {
        let res = app.clone().oneshot(req).await.unwrap();
        let code = res.status();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        (code, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn agents_post_and_admins_query() {
        let s = state();
        let agent = agent_router(s.clone());
        let admin = admin_router(s.clone());
        let d = Detection::new(
            "node-a",
            Engine::Signature,
            "Ward.Test.EICAR",
            Severity::High,
            String::new(),
        );
        let body = serde_json::to_string(&vec![d]).unwrap();
        let (code, _) = call(
            &agent,
            Request::post("/v1/detections")
                .header(header::CONTENT_TYPE, "application/json")
                .header(NODE_HEADER, "node-a")
                .body(Body::from(body))
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::ACCEPTED);
        assert_eq!(s.store.len(), 1);

        let (code, _) = call(
            &admin,
            Request::get("/v1/detections").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        let (code, _) = call(
            &admin,
            Request::get("/v1/detections")
                .header(header::AUTHORIZATION, "Bearer wrong")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        let (code, body) = call(
            &admin,
            Request::get("/v1/detections?severity=high&node=node-a")
                .header(header::AUTHORIZATION, "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert!(body.contains("Ward.Test.EICAR"));
        let (code, body) = call(
            &admin,
            Request::get("/v1/agents")
                .header(header::AUTHORIZATION, "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert!(body.contains("node-a"));
        let (code, _) = call(
            &admin,
            Request::get("/v1/agents").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn content_is_served_only_when_cached() {
        let s = state();
        let agent = agent_router(s);
        let (code, _) = call(
            &agent,
            Request::get("/v1/content/sha256:x")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        let (code, _) = call(
            &agent,
            Request::get("/v1/content/sha256:x/signature")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    }
}
