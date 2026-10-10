//! The admission endpoint.

use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::routing::post;
use axum::{Json, http::StatusCode};

use super::review::{Request, ReviewRequest, ReviewResponse};
use crate::engine::Engine;
use crate::events::Emitter;
use crate::gate::{Decision, RolloutState};
use crate::k8s::snapshot_from_value;
use crate::observability::Metrics;

/// The reason string attached to a denial, visible in the Argo CD error toast
/// alongside the message.
const DENY_REASON: &str = "CanaryGateBlocked";

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
    fn skipped(uid: &str, reason: &str) -> Self {
        Self {
            response: ReviewResponse::allow(uid, Vec::new()),
            outcome: "skipped",
            reason: reason.to_string(),
            verdict: None,
        }
    }
}

/// Validates one `AdmissionReview`.
///
/// The webhook registration already narrows traffic to sync requests, but
/// every condition is re-checked here: a misconfigured registration must not
/// be able to turn into a wrong verdict.
async fn validate(
    State(state): State<Arc<AdmissionState>>,
    body: Bytes,
) -> (StatusCode, Json<ReviewResponse>) {
    let started = Instant::now();

    let review: ReviewRequest = match serde_json::from_slice(&body) {
        Ok(review) => review,
        Err(err) => {
            tracing::warn!(outcome = "malformed", reason = "undecodable body", error = %err, "canary gate skipped a request it could not read");
            state.metrics.record_admission("malformed");
            // Failing open on an unreadable body keeps a malformed request
            // from blocking every sync. The gate cannot judge what it cannot
            // read.
            return (
                StatusCode::OK,
                Json(ReviewResponse::allow(
                    "",
                    vec!["canary gate could not decode the AdmissionReview".to_string()],
                )),
            );
        }
    };

    let Some(request) = review.request else {
        tracing::warn!(
            outcome = "malformed",
            reason = "no request in the review",
            "canary gate skipped a request it could not read"
        );
        state.metrics.record_admission("malformed");
        return (
            StatusCode::OK,
            Json(ReviewResponse::allow(
                "",
                vec!["canary gate received an AdmissionReview without a request".to_string()],
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
        return Outcome::skipped(uid, "not a sync request");
    }

    let cfg = state.engine.config();
    let principal = request.username();
    if cfg.exempt.usernames.iter().any(|u| u == principal) {
        return Outcome::skipped(uid, "principal is exempt");
    }

    if cfg.exempt.automated && request.is_automated() {
        return Outcome::skipped(uid, "operation is automated");
    }

    let Some(object) = request.object.as_ref() else {
        return Outcome {
            response: ReviewResponse::allow(
                uid,
                vec!["canary gate could not read the Application object".to_string()],
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
                        "canary gate could not parse the Application object: {err}"
                    )],
                ),
                outcome: "allowed",
                reason: format!("unparsable object: {err}"),
                verdict: None,
            };
        }
    };

    let verdict = state.engine.evaluate(snapshot).await;
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
                rollout_namespace = v.map_or("", |v| v.namespace.as_str()),
                code = v.map_or("", |v| v.code.as_str()),
                allowed = v.map(|v| v.allowed),
                message = v.map_or("", |v| v.message.as_str()),
                rollouts = %v.map_or_else(String::new, |v| rollout_summary(&v.rollouts)),
                warnings = ?v.map(|v| v.warnings.as_slice()).unwrap_or_default(),
                $message
            )
        };
    }
    match result.outcome {
        "denied" => emit!(warn, "canary gate denied a sync"),
        "allowed" => emit!(info, "canary gate allowed a sync"),
        _ => emit!(info, "canary gate skipped a request"),
    }
}

/// Compresses the Rollout states into one field, because a log line is read at
/// a glance and a nested structure is not.
fn rollout_summary(rollouts: &[RolloutState]) -> String {
    rollouts
        .iter()
        .map(|r| {
            let mut flags = Vec::new();
            if r.in_progress {
                flags.push("in-progress");
            }
            if r.paused {
                flags.push("paused");
            }
            if r.aborted {
                flags.push("aborted");
            }
            let flags = if flags.is_empty() {
                "settled".to_string()
            } else {
                flags.join("+")
            };
            let step = if r.step.is_empty() {
                String::new()
            } else {
                format!(" {}", r.step)
            };
            format!("{}={flags}{step}", r.name)
        })
        .collect::<Vec<_>>()
        .join(" ")
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
    use crate::engine::testing::{FakeReader, rollout};
    use crate::events::Recorder;
    use crate::events::testing::FakeSink;
    use crate::gate::RolloutSnapshot;
    use crate::k8s::RolloutReader;

    struct Harness {
        state: Arc<AdmissionState>,
        sink: Arc<FakeSink>,
    }

    fn harness(cfg: &str, rollouts: Vec<RolloutSnapshot>) -> Harness {
        let cfg = Config::parse(cfg).unwrap();
        let metrics = Arc::new(Metrics::new());
        let reader = FakeReader::with(rollouts);
        let engine = Arc::new(Engine::new(
            cfg,
            Arc::new(reader) as Arc<dyn RolloutReader>,
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

    fn sync_object() -> Value {
        json!({
            "metadata": {"name": "prd-api", "uid": "app-uid"},
            "spec": {"destination": {"namespace": "payments"}},
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
        let h = harness("{}", vec![]);
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
        let h = harness("{}", vec![rollout("api", "a", "b")]);
        let status_write = review(
            json!({"metadata": {"name": "prd-api"}}),
            json!({}),
            "controller",
            false,
        );
        let (_, body) = post(Arc::clone(&h.state), &status_write).await;
        assert_eq!(body["response"]["allowed"], true);

        let exempt = review(
            sync_object(),
            json!({}),
            "system:serviceaccount:argocd:argocd-application-controller",
            false,
        );
        let (_, body) = post(Arc::clone(&h.state), &exempt).await;
        assert_eq!(body["response"]["allowed"], true);

        let mut automated = sync_object();
        automated["operation"]["initiatedBy"]["automated"] = json!(true);
        let (_, body) = post(
            Arc::clone(&h.state),
            &review(automated, json!({}), "someone", false),
        )
        .await;
        assert_eq!(body["response"]["allowed"], true);

        let text = h.state.metrics.encode().unwrap();
        assert!(
            text.contains(r#"admission_requests_total{outcome="skipped"} 3"#),
            "{text}"
        );
    }

    #[tokio::test]
    async fn allows_unparsable_objects_with_a_warning() {
        let h = harness("{}", vec![]);
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
            serde_json::from_str(&review(sync_object(), json!({}), "someone", false)).unwrap();
        req["request"]["object"] = Value::Null;
        let (_, body) = post(Arc::clone(&h.state), &req.to_string()).await;
        assert_eq!(
            body["response"]["allowed"], true,
            "a null object is not a sync request"
        );
    }

    #[tokio::test]
    async fn denies_and_records_an_event() {
        let mut mid = rollout("api", "aaa", "bbb");
        mid.current_step = Some(2);
        mid.total_steps = 6;
        let h = harness("{}", vec![mid]);
        let (status, body) = post(
            Arc::clone(&h.state),
            &review(sync_object(), json!({}), "someone", false),
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
                .contains("step 2/6")
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
        let h = harness("{}", vec![rollout("api", "aaa", "bbb")]);
        let (_, body) = post(
            Arc::clone(&h.state),
            &review(sync_object(), json!({}), "someone", true),
        )
        .await;
        assert_eq!(body["response"]["allowed"], false);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(h.sink.events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn allows_with_warnings_in_warn_mode() {
        let h = harness("mode: warn\n", vec![rollout("api", "aaa", "bbb")]);
        let (_, body) = post(
            Arc::clone(&h.state),
            &review(sync_object(), json!({}), "someone", false),
        )
        .await;
        assert_eq!(body["response"]["allowed"], true);
        assert!(
            body["response"]["warnings"][0]
                .as_str()
                .unwrap()
                .contains("mode is set to warn")
        );
        wait_for_events(&h.sink, 1).await;
        assert_eq!(
            h.sink.events.lock().unwrap()[0].reason.as_deref(),
            Some("CanaryWarning")
        );
    }

    #[tokio::test]
    async fn no_emitter_is_fine() {
        let h = harness("{}", vec![]);
        let state = Arc::new(AdmissionState {
            engine: Arc::clone(&h.state.engine),
            metrics: Arc::clone(&h.state.metrics),
            events: None,
        });
        let (_, body) = post(state, &review(sync_object(), json!({}), "someone", false)).await;
        assert_eq!(body["response"]["allowed"], true, "no rollouts is allowed");
    }

    #[test]
    fn rollout_summary_reads_at_a_glance() {
        let states = vec![
            RolloutState {
                name: "api".into(),
                namespace: "payments".into(),
                strategy: "canary".into(),
                phase: "Paused".into(),
                step: "step 3/8".into(),
                paused: true,
                aborted: false,
                in_progress: true,
            },
            RolloutState {
                name: "worker".into(),
                namespace: "payments".into(),
                strategy: "canary".into(),
                phase: "Healthy".into(),
                step: String::new(),
                paused: false,
                aborted: false,
                in_progress: false,
            },
        ];
        assert_eq!(
            rollout_summary(&states),
            "api=in-progress+paused step 3/8 worker=settled"
        );
        assert_eq!(rollout_summary(&[]), "");
    }
}
