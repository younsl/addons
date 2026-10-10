//! The admission endpoint.

use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::routing::post;
use axum::{Json, http::StatusCode};

use super::review::{Request, ReviewRequest, ReviewResponse};
use crate::argocd::snapshot_from_value;
use crate::engine::Engine;
use crate::events::Emitter;
use crate::gate::{Code, Decision, ImageComparison};
use crate::observability::Metrics;

/// The reason string attached to a denial, visible in the Argo CD error toast
/// alongside the message.
const DENY_REASON: &str = "PromotionGateBlocked";

/// Bounds the request body. An `AdmissionReview` for one Application is
/// small. Anything larger is not a request this webhook should spend memory
/// on.
const MAX_BODY_BYTES: usize = 4 << 20;

/// Everything the admission handler needs.
pub struct AdmissionState {
    pub engine: Arc<Engine>,
    pub metrics: Arc<Metrics>,
    /// `None` turns event recording off without the handler having to know
    /// why.
    pub events: Option<Arc<dyn Emitter>>,
}

/// Builds the webhook router serving `POST /validate`.
pub fn router(state: Arc<AdmissionState>) -> Router {
    Router::new()
        .route("/validate", post(validate))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

/// One verdict plus everything worth writing down about it.
#[allow(clippy::struct_field_names)]
struct Outcome {
    response: ReviewResponse,
    /// The metric label: allowed, denied, or skipped.
    outcome: &'static str,
    reason: String,
    /// `None` on the early exits that never reach the rules.
    verdict: Option<Decision>,
}

impl Outcome {
    fn skipped(uid: &str, reason: &str, verdict: Option<Decision>) -> Self {
        Self {
            response: ReviewResponse::allow(uid, Vec::new()),
            outcome: "skipped",
            reason: reason.to_string(),
            verdict,
        }
    }
}

/// Validates one `AdmissionReview`.
///
/// The webhook registration already narrows traffic to sync requests in gated
/// projects, but every condition is re-checked here: a misconfigured
/// registration must not be able to turn into a wrong verdict.
async fn validate(
    State(state): State<Arc<AdmissionState>>,
    body: Bytes,
) -> (StatusCode, Json<ReviewResponse>) {
    let started = Instant::now();

    let review: ReviewRequest = match serde_json::from_slice(&body) {
        Ok(review) => review,
        Err(err) => {
            tracing::warn!(outcome = "malformed", reason = "undecodable body", error = %err, "promotion gate skipped a request it could not read");
            state.metrics.record_admission("malformed");
            // Failing open on an unreadable body keeps a malformed request
            // from blocking every sync. The gate cannot judge what it cannot
            // read.
            return (
                StatusCode::OK,
                Json(ReviewResponse::allow(
                    "",
                    vec!["promotion gate could not decode the AdmissionReview".to_string()],
                )),
            );
        }
    };

    let Some(request) = review.request else {
        tracing::warn!(
            outcome = "malformed",
            reason = "no request in the review",
            "promotion gate skipped a request it could not read"
        );
        state.metrics.record_admission("malformed");
        return (
            StatusCode::OK,
            Json(ReviewResponse::allow(
                "",
                vec!["promotion gate received an AdmissionReview without a request".to_string()],
            )),
        );
    };

    let result = decide(&state, &request).await;
    emit(&state, &request, &result);

    // Measured before the response is written rather than after, so the
    // number is the gate's own cost and not the API server's read of the
    // socket.
    let elapsed = started.elapsed();
    state.metrics.record_admission(result.outcome);
    state.metrics.observe_admission(result.outcome, elapsed);
    log(&request, &result, elapsed.as_millis());
    (StatusCode::OK, Json(result.response))
}

/// Records the verdict on the Application itself.
///
/// Skipped on a dry run, which is what `sideEffects: NoneOnDryRun` commits the
/// webhook to: the API server is asking what would happen, and answering by
/// writing to the cluster would make the question change the answer.
fn emit(state: &AdmissionState, request: &Request, result: &Outcome) {
    let (Some(events), Some(verdict)) = (&state.events, &result.verdict) else {
        return;
    };
    if request.dry_run {
        return;
    }
    events.emit(
        &request.namespace,
        &request.name,
        request.object_uid(),
        verdict,
    );
}

async fn decide(state: &AdmissionState, request: &Request) -> Outcome {
    let uid = request.uid.as_str();

    if !request.is_sync_request() {
        return Outcome::skipped(uid, "not a sync request", None);
    }

    let cfg = state.engine.config();
    let principal = request.username();
    if cfg.exempt.usernames.iter().any(|u| u == principal) {
        return Outcome::skipped(uid, "principal is exempt", None);
    }

    if cfg.exempt.automated && request.is_automated() {
        return Outcome::skipped(uid, "operation is automated", None);
    }

    let Some(object) = request.object.as_ref() else {
        return Outcome {
            response: ReviewResponse::allow(
                uid,
                vec!["promotion gate could not read the Application object".to_string()],
            ),
            outcome: "allowed",
            reason: "no object in the request".to_string(),
            verdict: None,
        };
    };

    let snapshot = match snapshot_from_value(object, &cfg.exempt.annotation) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            return Outcome {
                response: ReviewResponse::allow(
                    uid,
                    vec![format!(
                        "promotion gate could not parse the Application object: {err}"
                    )],
                ),
                outcome: "allowed",
                reason: format!("unparsable object: {err}"),
                verdict: None,
            };
        }
    };

    let verdict = state.engine.evaluate(snapshot).await;
    if verdict.code == Code::NotGated {
        return Outcome::skipped(uid, "environment is not gated", Some(verdict));
    }

    if verdict.allowed {
        return Outcome {
            response: ReviewResponse::allow(uid, verdict.warnings.clone()),
            outcome: "allowed",
            reason: verdict.code.to_string(),
            verdict: Some(verdict),
        };
    }
    Outcome {
        response: ReviewResponse::deny(uid, DENY_REASON, &verdict.message),
        outcome: "denied",
        reason: verdict.code.to_string(),
        verdict: Some(verdict),
    }
}

/// Writes exactly one line per request, whatever happened.
///
/// A gate that refuses a deploy has to be able to answer "which application,
/// and why" from the logs alone, and the same is true of one that let a deploy
/// through. So the allow path is as loud as the deny path, and both name the
/// target and the reason. Denials go out at warn because they are what
/// somebody will come asking about.
fn log(request: &Request, result: &Outcome, duration_ms: u128) {
    let v = result.verdict.as_ref();
    let upstream = v.and_then(|v| v.upstream.as_ref());
    // The field list lives in a local macro because tracing needs the level
    // at compile time and the same fields go out at two levels.
    macro_rules! emit {
        ($level:ident, $message:literal) => {
            tracing::$level!(
                outcome = result.outcome,
                reason = %result.reason,
                app = %request.name,
                namespace = %request.namespace,
                principal = %request.username(),
                duration_ms,
                initiated_by = %request.initiated_by(),
                revision = %request.sync_revision(),
                env = v.map_or("", |v| v.env.as_str()),
                identity = v.map_or("", |v| v.identity.as_str()),
                code = v.map_or("", |v| v.code.as_str()),
                allowed = v.map(|v| v.allowed),
                message = v.map_or("", |v| v.message.as_str()),
                upstream = upstream.map_or("", |u| u.app.as_str()),
                upstream_env = upstream.map_or("", |u| u.env.as_str()),
                upstream_exists = upstream.map(|u| u.exists),
                upstream_sync = upstream.map_or("", |u| u.sync_status.as_str()),
                upstream_health = upstream.map_or("", |u| u.health_status.as_str()),
                images = %v.map_or_else(String::new, |v| image_summary(&v.images)),
                warnings = ?v.map(|v| v.warnings.as_slice()).unwrap_or_default(),
                $message
            )
        };
    }
    match result.outcome {
        "denied" => emit!(warn, "promotion gate denied a sync"),
        "allowed" => emit!(info, "promotion gate allowed a sync"),
        _ => emit!(info, "promotion gate skipped a request"),
    }
}

/// Compresses the comparison into one field, because a log line is read at a
/// glance and a nested structure is not.
fn image_summary(images: &[ImageComparison]) -> String {
    images
        .iter()
        .map(|image| {
            let operator = if image.matched { "==" } else { "!=" };
            format!(
                "{} {}{operator}{}",
                image.repository,
                or_dash(&image.desired_tag),
                or_dash(&image.upstream_tag)
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

const fn or_dash(value: &str) -> &str {
    if value.is_empty() { "-" } else { value }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::*;
    use crate::config::Config;
    use crate::engine::testing::{FakeReader, FakeResolver, snapshot};
    use crate::events::Recorder;
    use crate::events::testing::FakeSink;

    struct Harness {
        state: Arc<AdmissionState>,
        sink: Arc<FakeSink>,
    }

    fn harness(cfg: &str, upstream: Option<crate::gate::AppSnapshot>) -> Harness {
        let cfg = Config::parse(&format!("chain: [stg, prd]\n{cfg}")).unwrap();
        let metrics = Arc::new(Metrics::new());
        let reader = FakeReader::with(upstream.into_iter().collect());
        let resolver: Arc<dyn crate::argocd::ImageResolver> =
            Arc::new(FakeResolver::returning(&["r/api:2"]));
        let engine = Arc::new(Engine::new(
            cfg,
            Arc::new(reader),
            Some(resolver),
            Arc::clone(&metrics),
        ));
        let sink = Arc::new(FakeSink::default());
        let recorder = Recorder::new(
            Arc::clone(&sink) as Arc<dyn crate::events::EventSink>,
            Arc::clone(&metrics),
        );
        Harness {
            state: Arc::new(AdmissionState {
                engine,
                metrics,
                events: Some(Arc::new(recorder)),
            }),
            sink,
        }
    }

    async fn post(state: Arc<AdmissionState>, body: &str) -> (StatusCode, Value) {
        let app = router(state);
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/validate")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[allow(clippy::needless_pass_by_value)]
    fn review(object: Value, old_object: Value, username: &str, dry_run: bool) -> String {
        json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": "req-1",
                "name": "prd-api",
                "namespace": "argocd",
                "operation": "UPDATE",
                "dryRun": dry_run,
                "userInfo": {"username": username},
                "object": object,
                "oldObject": old_object
            }
        })
        .to_string()
    }

    fn sync_object(project: &str) -> Value {
        json!({
            "metadata": {"name": format!("{project}-api"), "uid": "app-uid"},
            "spec": {"project": project},
            "operation": {"sync": {"revision": "abc"}, "initiatedBy": {"username": "dev"}}
        })
    }

    async fn wait_for_events(sink: &FakeSink, want: usize) {
        for _ in 0..50 {
            if sink.events.lock().unwrap().len() >= want {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn fails_open_on_malformed_input() {
        let h = harness("", None);
        let (status, body) = post(Arc::clone(&h.state), "{not json").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["response"]["allowed"], true);
        assert!(
            body["response"]["warnings"][0]
                .as_str()
                .unwrap()
                .contains("could not decode")
        );

        let (_, body) = post(
            Arc::clone(&h.state),
            r#"{"apiVersion":"admission.k8s.io/v1"}"#,
        )
        .await;
        assert_eq!(body["response"]["allowed"], true);
        assert!(
            body["response"]["warnings"][0]
                .as_str()
                .unwrap()
                .contains("without a request")
        );

        let text = h.state.metrics.encode().unwrap();
        assert!(
            text.contains(r#"admission_requests_total{outcome="malformed"} 2"#),
            "{text}"
        );
    }

    #[tokio::test]
    async fn skips_non_sync_exempt_and_automated_requests() {
        let h = harness("", None);
        let status_write = review(
            json!({"metadata": {"name": "prd-api"}}),
            json!({}),
            "controller",
            false,
        );
        let (_, body) = post(Arc::clone(&h.state), &status_write).await;
        assert_eq!(body["response"]["allowed"], true);

        let exempt = review(
            sync_object("prd"),
            json!({}),
            "system:serviceaccount:argocd:argocd-application-controller",
            false,
        );
        let (_, body) = post(Arc::clone(&h.state), &exempt).await;
        assert_eq!(body["response"]["allowed"], true);

        let mut automated = sync_object("prd");
        automated["operation"]["initiatedBy"]["automated"] = json!(true);
        let (_, body) = post(
            Arc::clone(&h.state),
            &review(automated, json!({}), "someone", false),
        )
        .await;
        assert_eq!(body["response"]["allowed"], true);

        let not_gated = review(sync_object("stg"), json!({}), "someone", false);
        let (_, body) = post(Arc::clone(&h.state), &not_gated).await;
        assert_eq!(body["response"]["allowed"], true);

        let text = h.state.metrics.encode().unwrap();
        assert!(
            text.contains(r#"admission_requests_total{outcome="skipped"} 4"#),
            "{text}"
        );
    }

    #[tokio::test]
    async fn allows_unparsable_objects_with_a_warning() {
        let h = harness("", None);
        let no_name = review(
            json!({"operation": {"sync": {}}}),
            json!({}),
            "someone",
            false,
        );
        let (_, body) = post(Arc::clone(&h.state), &no_name).await;
        assert_eq!(body["response"]["allowed"], true);
        assert!(
            body["response"]["warnings"][0]
                .as_str()
                .unwrap()
                .contains("could not parse")
        );

        let mut req: Value =
            serde_json::from_str(&review(sync_object("prd"), json!({}), "someone", false)).unwrap();
        req["request"]["object"] = Value::Null;
        let (_, body) = post(Arc::clone(&h.state), &req.to_string()).await;
        assert_eq!(
            body["response"]["allowed"], true,
            "a null object is not a sync request"
        );
    }

    #[tokio::test]
    async fn denies_and_records_an_event() {
        let upstream = snapshot("stg-api", "stg", "OutOfSync", "Healthy", &[]);
        let h = harness("", Some(upstream));
        let (status, body) = post(
            Arc::clone(&h.state),
            &review(sync_object("prd"), json!({}), "someone", false),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["response"]["allowed"], false);
        assert_eq!(body["response"]["status"]["code"], 403);
        assert_eq!(body["response"]["status"]["reason"], DENY_REASON);
        assert!(
            body["response"]["status"]["message"]
                .as_str()
                .unwrap()
                .contains("stg-api")
        );

        wait_for_events(&h.sink, 1).await;
        let events = h.sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].involved_object.uid.as_deref(), Some("app-uid"));
        drop(events);

        let text = h.state.metrics.encode().unwrap();
        assert!(
            text.contains(r#"admission_requests_total{outcome="denied"} 1"#),
            "{text}"
        );
        assert!(
            text.contains(r#"admission_duration_seconds_count{outcome="denied"} 1"#),
            "{text}"
        );
    }

    #[tokio::test]
    async fn dry_run_computes_but_never_writes() {
        let upstream = snapshot("stg-api", "stg", "OutOfSync", "Healthy", &[]);
        let h = harness("", Some(upstream));
        let (_, body) = post(
            Arc::clone(&h.state),
            &review(sync_object("prd"), json!({}), "someone", true),
        )
        .await;
        assert_eq!(body["response"]["allowed"], false);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(h.sink.events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn allows_with_warnings_in_warn_mode() {
        let upstream = snapshot("stg-api", "stg", "Synced", "Healthy", &["r/api:1"]);
        let h = harness("", Some(upstream));
        let (_, body) = post(
            Arc::clone(&h.state),
            &review(sync_object("prd"), json!({}), "someone", false),
        )
        .await;
        assert_eq!(body["response"]["allowed"], true);
        assert!(
            body["response"]["warnings"][0]
                .as_str()
                .unwrap()
                .contains("imageTag.mode is set to warn")
        );
        wait_for_events(&h.sink, 1).await;
        assert_eq!(
            h.sink.events.lock().unwrap()[0].reason.as_deref(),
            Some("PromotionWarning")
        );
    }

    #[tokio::test]
    async fn no_emitter_is_fine() {
        let h = harness("", None);
        let state = Arc::new(AdmissionState {
            engine: Arc::clone(&h.state.engine),
            metrics: Arc::clone(&h.state.metrics),
            events: None,
        });
        let (_, body) = post(
            state,
            &review(sync_object("prd"), json!({}), "someone", false),
        )
        .await;
        assert_eq!(
            body["response"]["allowed"], true,
            "missing upstream is allowed"
        );
    }

    #[test]
    fn image_summary_reads_at_a_glance() {
        let images = vec![
            ImageComparison {
                repository: "api".into(),
                desired_tag: "2".into(),
                upstream_tag: "1".into(),
                matched: false,
            },
            ImageComparison {
                repository: "worker".into(),
                desired_tag: String::new(),
                upstream_tag: "1".into(),
                matched: false,
            },
            ImageComparison {
                repository: "web".into(),
                desired_tag: "3".into(),
                upstream_tag: "3".into(),
                matched: true,
            },
        ];
        assert_eq!(image_summary(&images), "api 2!=1 worker -!=1 web 3==3");
        assert_eq!(image_summary(&[]), "");
    }
}
