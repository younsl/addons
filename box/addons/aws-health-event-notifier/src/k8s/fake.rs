//! Minimal in-memory Kubernetes API server for tests: GET, POST, PUT, and
//! PATCH on any object path, with `resourceVersion` compare-and-swap and
//! configurable `SelfSubjectAccessReview` answers.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use kube::{Client, Config};
use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

#[derive(Default)]
struct State {
    objects: HashMap<String, Value>,
    next_rv: u64,
    fail_all: bool,
    denied: HashSet<String>,
}

#[derive(Clone, Default)]
struct Responder(Arc<Mutex<State>>);

pub struct FakeApiServer {
    server: MockServer,
    state: Arc<Mutex<State>>,
}

impl FakeApiServer {
    pub async fn start() -> Self {
        let server = MockServer::start().await;
        let responder = Responder::default();
        let state = responder.0.clone();
        Mock::given(any())
            .respond_with(responder)
            .mount(&server)
            .await;
        Self { server, state }
    }

    pub fn client(&self) -> Client {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let config = Config::new(self.server.uri().parse().unwrap());
        Client::try_from(config).unwrap()
    }

    pub fn fail_all(&self) {
        self.state.lock().unwrap().fail_all = true;
    }

    /// Makes `SelfSubjectAccessReview` deny `verb` on `resource`.
    pub fn deny(&self, resource: &str, verb: &str) {
        self.state
            .lock()
            .unwrap()
            .denied
            .insert(format!("{resource}/{verb}"));
    }

    pub fn object(&self, path: &str) -> Option<Value> {
        self.state.lock().unwrap().objects.get(path).cloned()
    }

    /// Overwrites an object out of band, bumping its `resourceVersion`.
    pub fn put_object(&self, path: &str, mut body: Value) {
        let mut state = self.state.lock().unwrap();
        state.next_rv += 1;
        body["metadata"]["resourceVersion"] = json!(state.next_rv.to_string());
        state.objects.insert(path.to_string(), body);
    }

    pub fn post_count(&self, fragment: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .objects
            .keys()
            .filter(|k| k.contains(fragment))
            .count()
    }
}

fn status(code: u16, reason: &str) -> ResponseTemplate {
    ResponseTemplate::new(code).set_body_json(json!({
        "kind": "Status",
        "apiVersion": "v1",
        "status": "Failure",
        "message": reason,
        "reason": reason,
        "code": code,
    }))
}

impl Respond for Responder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let mut state = self.0.lock().unwrap();
        if state.fail_all {
            return status(500, "InternalError");
        }
        let path = request.url.path().to_string();
        let body: Value = request.body_json().unwrap_or(Value::Null);

        if path.ends_with("/selfsubjectaccessreviews") {
            let attrs = &body["spec"]["resourceAttributes"];
            let key = format!(
                "{}/{}",
                attrs["resource"].as_str().unwrap_or_default(),
                attrs["verb"].as_str().unwrap_or_default()
            );
            let mut review = body.clone();
            review["status"] = json!({ "allowed": !state.denied.contains(&key) });
            return ResponseTemplate::new(201).set_body_json(review);
        }

        match request.method.as_str() {
            "GET" => state.objects.get(&path).map_or_else(
                || status(404, "NotFound"),
                |o| ResponseTemplate::new(200).set_body_json(o),
            ),
            "POST" => {
                let name = body["metadata"]["name"].as_str().unwrap_or("generated");
                let key = format!("{path}/{name}");
                if state.objects.contains_key(&key) {
                    return status(409, "AlreadyExists");
                }
                state.next_rv += 1;
                let mut stored = body.clone();
                stored["metadata"]["resourceVersion"] = json!(state.next_rv.to_string());
                state.objects.insert(key, stored.clone());
                ResponseTemplate::new(201).set_body_json(stored)
            }
            "PUT" => {
                let Some(current) = state.objects.get(&path) else {
                    return status(404, "NotFound");
                };
                if current["metadata"]["resourceVersion"] != body["metadata"]["resourceVersion"] {
                    return status(409, "Conflict");
                }
                state.next_rv += 1;
                let mut stored = body;
                stored["metadata"]["resourceVersion"] = json!(state.next_rv.to_string());
                state.objects.insert(path, stored.clone());
                ResponseTemplate::new(200).set_body_json(stored)
            }
            "PATCH" => state.objects.get(&path).map_or_else(
                || status(404, "NotFound"),
                |o| ResponseTemplate::new(200).set_body_json(o),
            ),
            _ => status(405, "MethodNotAllowed"),
        }
    }
}
