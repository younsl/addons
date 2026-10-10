//! JSON API behind the console, plus health and metrics.

use axum::Json;
use std::path::PathBuf;

use axum::extract::{Query, State as Extract};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;

use super::AppState;

const OPENMETRICS: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

pub async fn healthz() -> StatusCode {
    StatusCode::OK
}

pub async fn readyz(Extract(app): Extract<AppState>) -> StatusCode {
    if app.state.is_ready() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

pub async fn metrics(Extract(app): Extract<AppState>) -> Response {
    app.state.metrics().map_or_else(
        |_| StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        |body| ([(header::CONTENT_TYPE, OPENMETRICS)], body).into_response(),
    )
}

pub async fn info(Extract(app): Extract<AppState>) -> Response {
    Json(app.info.as_ref().clone()).into_response()
}

pub async fn status(Extract(app): Extract<AppState>) -> Response {
    app.state.report().map_or_else(
        || {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "no reconcile has finished yet",
            )
                .into_response()
        },
        |report| Json(report).into_response(),
    )
}

#[derive(Debug, Deserialize)]
pub struct PathQuery {
    path: PathBuf,
}

fn not_found(what: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        format!("{what} is not in the last reconcile"),
    )
        .into_response()
}

/// Files under one link source or package file, without their content.
pub async fn tree(Extract(app): Extract<AppState>, Query(query): Query<PathQuery>) -> Response {
    app.state
        .report()
        .and_then(|r| r.files.tree(&query.path))
        .map_or_else(
            || not_found("this source"),
            |tree| Json(tree).into_response(),
        )
}

/// One tracked file with its content. Read only, served from memory.
pub async fn file(Extract(app): Extract<AppState>, Query(query): Query<PathQuery>) -> Response {
    app.state
        .report()
        .and_then(|r| r.files.file(&query.path).cloned())
        .map_or_else(|| not_found("this file"), |file| Json(file).into_response())
}

#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    #[serde(default)]
    after: u64,
}

/// tether's own log events newer than `after`, from memory.
pub async fn logs(Extract(app): Extract<AppState>, Query(query): Query<LogsQuery>) -> Response {
    Json(app.logs.since(query.after, 1000)).into_response()
}

pub async fn reconcile(Extract(app): Extract<AppState>) -> StatusCode {
    app.state.request_reconcile();
    StatusCode::ACCEPTED
}

#[derive(Debug, Deserialize, Serialize)]
pub struct LogLevel {
    filter: String,
}

pub async fn log_level(Extract(app): Extract<AppState>, Json(body): Json<LogLevel>) -> Response {
    let filter = match EnvFilter::try_new(&body.filter) {
        Ok(filter) => filter,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": err.to_string() })),
            )
                .into_response();
        }
    };
    match app.log_filter.reload(filter) {
        Ok(()) => {
            tracing::info!(filter = %body.filter, "log filter changed");
            Json(body).into_response()
        }
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Method, Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::{Registry, reload};

    use super::*;
    use crate::State;
    use crate::config::BuildInfo;
    use crate::controller::reconcile;
    use crate::fixtures::TempHome;
    use crate::web::{Info, router};

    struct Harness {
        app: AppState,
        _filter: reload::Layer<EnvFilter, Registry>,
    }

    fn harness() -> Harness {
        let (filter, handle) = reload::Layer::new(EnvFilter::new("info"));
        let build = BuildInfo::CURRENT;
        Harness {
            app: AppState {
                state: State::default(),
                info: Arc::new(Info {
                    version: build.version,
                    commit: build.commit,
                    built: build.date,
                    rustc: build.rustc,
                    config_file: "/etc/tether/config.toml".into(),
                    home: "/home/dev".into(),
                    dry_run: false,
                    reconcile_interval_secs: 300,
                }),
                log_filter: handle,
                logs: crate::telemetry::LogBuffer::default(),
            },
            _filter: filter,
        }
    }

    async fn call(
        app: &AppState,
        method: Method,
        uri: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (StatusCode, String, axum::http::HeaderMap) {
        let mut request = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = router(app.clone())
            .oneshot(request.body(Body::from(body.to_string())).expect("request"))
            .await
            .expect("response");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        (
            status,
            String::from_utf8(bytes.to_vec()).expect("utf8"),
            headers,
        )
    }

    #[tokio::test]
    async fn before_first_reconcile() {
        let h = harness();
        assert_eq!(
            call(&h.app, Method::GET, "/healthz", &[], "").await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(&h.app, Method::GET, "/readyz", &[], "").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            call(&h.app, Method::GET, "/api/status", &[], "").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let (code, body, _) = call(&h.app, Method::GET, "/metrics", &[], "").await;
        assert_eq!(code, StatusCode::OK);
        assert!(body.ends_with("# EOF\n"), "{body}");
    }

    #[tokio::test]
    async fn status_and_info_after_reconcile() {
        let h = harness();
        let home = TempHome::new();
        let ctx = home.context("source_root = \"~\"\nbackup_root = \"~/b\"\n", true);
        h.app
            .state
            .publish(reconcile(&ctx, jiff::Timestamp::now()).expect("reconcile"));

        assert_eq!(
            call(&h.app, Method::GET, "/readyz", &[], "").await.0,
            StatusCode::OK
        );
        let (code, body, _) = call(&h.app, Method::GET, "/api/status", &[], "").await;
        assert_eq!(code, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(json["dry_run"], true);
        assert_eq!(json["entries"], serde_json::json!([]));
        assert_eq!(json["packages"], serde_json::json!([]));

        let (code, body, _) = call(&h.app, Method::GET, "/api/info", &[], "").await;
        assert_eq!(code, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(json["home"], "/home/dev");
        assert_eq!(json["reconcile_interval_secs"], 300);
    }

    #[tokio::test]
    async fn tree_and_file_serve_only_tracked_snapshot() {
        let h = harness();
        let home = TempHome::new();
        std::fs::create_dir_all(home.repo.join(".git")).expect("git");
        std::fs::write(
            home.repo.join(".git/index"),
            crate::files::git_index::tests::index_v2(&["zshrc"]),
        )
        .expect("index");
        let zshrc = home.source_file("zshrc");
        let ctx = home.context(
            "source_root = \"~/repo\"\nbackup_root = \"~/b\"\n\n[[links]]\nsource = \"zshrc\"\ntarget = \"~/.zshrc\"\n",
            true,
        );
        h.app
            .state
            .publish(reconcile(&ctx, jiff::Timestamp::now()).expect("reconcile"));
        let local = [("host", "127.0.0.1:8080")];
        let q = |path: &std::path::Path| format!("path={}", path.display()).replace('/', "%2F");

        let (code, body, _) = call(
            &h.app,
            Method::GET,
            &format!("/api/tree?{}", q(&zshrc)),
            &local,
            "",
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        let tree: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(tree["files"].as_array().expect("files").len(), 1);

        let (code, body, _) = call(
            &h.app,
            Method::GET,
            &format!("/api/file?{}", q(&zshrc)),
            &local,
            "",
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let file: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(file["content"], "zshrc");

        for uri in ["/api/file?path=%2Fetc%2Fpasswd", "/api/tree?path=%2Fetc"] {
            assert_eq!(
                call(&h.app, Method::GET, uri, &local, "").await.0,
                StatusCode::NOT_FOUND,
                "{uri}"
            );
        }
        assert_eq!(
            call(&h.app, Method::GET, "/api/file", &local, "").await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(
                &h.app,
                Method::POST,
                &format!("/api/file?{}", q(&zshrc)),
                &local,
                ""
            )
            .await
            .0,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    #[tokio::test]
    async fn logs_are_served_incrementally() {
        let h = harness();
        let subscriber = tracing_subscriber::registry().with(h.app.logs.layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("first");
            tracing::warn!(path = "/x", "second");
        });
        let local = [("host", "127.0.0.1:8080")];

        let (code, body, _) = call(&h.app, Method::GET, "/api/logs", &local, "").await;
        assert_eq!(code, StatusCode::OK);
        let all: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(all.as_array().expect("array").len(), 2);
        assert_eq!(all[1]["level"], "warn");
        assert_eq!(all[1]["fields"][0], serde_json::json!(["path", "/x"]));

        let (_, body, _) = call(&h.app, Method::GET, "/api/logs?after=1", &local, "").await;
        let newer: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(newer.as_array().expect("array").len(), 1);
        assert_eq!(newer[0]["message"], "second");

        assert_eq!(
            call(
                &h.app,
                Method::GET,
                "/api/logs",
                &[("host", "evil.example.com")],
                ""
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn reconcile_accepts_same_origin_only() {
        let h = harness();
        let host = ("host", "127.0.0.1:8080");
        assert_eq!(
            call(&h.app, Method::POST, "/api/reconcile", &[host], "")
                .await
                .0,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            call(
                &h.app,
                Method::POST,
                "/api/reconcile",
                &[host, ("origin", "http://127.0.0.1:8080")],
                ""
            )
            .await
            .0,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            call(
                &h.app,
                Method::POST,
                "/api/reconcile",
                &[host, ("origin", "https://evil.example.com")],
                ""
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&h.app, Method::GET, "/api/reconcile", &[host], "")
                .await
                .0,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    #[tokio::test]
    async fn console_rejects_foreign_host_but_health_does_not() {
        let h = harness();
        let foreign = [("host", "rebind.example.com:8080")];
        assert_eq!(
            call(&h.app, Method::GET, "/api/status", &foreign, "")
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&h.app, Method::GET, "/", &foreign, "").await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&h.app, Method::GET, "/healthz", &foreign, "").await.0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn console_assets_carry_csp() {
        let h = harness();
        let local = [("host", "localhost:8080")];
        for (path, kind) in [
            ("/", "text/html"),
            ("/logs", "text/html"),
            ("/assets/app.css", "text/css"),
            ("/assets/app.js", "text/javascript"),
        ] {
            let (code, body, headers) = call(&h.app, Method::GET, path, &local, "").await;
            assert_eq!(code, StatusCode::OK, "{path}");
            assert_ne!(body.len(), 0);
            assert!(
                headers[header::CONTENT_TYPE]
                    .to_str()
                    .expect("type")
                    .starts_with(kind)
            );
            assert!(
                headers[header::CONTENT_SECURITY_POLICY]
                    .to_str()
                    .expect("csp")
                    .contains("frame-ancestors 'none'")
            );
        }
    }

    #[tokio::test]
    async fn log_level_reloads_or_rejects() {
        let h = harness();
        let json = [
            ("host", "localhost:8080"),
            ("content-type", "application/json"),
        ];
        let (code, body, _) = call(
            &h.app,
            Method::PUT,
            "/api/log-level",
            &json,
            r#"{"filter":"debug"}"#,
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(
            h.app
                .log_filter
                .with_current(ToString::to_string)
                .expect("current"),
            "debug"
        );
        let (code, _, _) = call(
            &h.app,
            Method::PUT,
            "/api/log-level",
            &json,
            r#"{"filter":"tether=nope"}"#,
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
    }
}
