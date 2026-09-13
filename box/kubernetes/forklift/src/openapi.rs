//! Serves the embedded OpenAPI 3.1 specification and a Scalar API reference UI.

use axum::Router;
use axum::body::Body;
use axum::response::Response;
use axum::routing::get;
use http::{HeaderValue, header};

/// The specification, embedded at compile time.
static SPEC: &[u8] = include_bytes!("openapi/openapi.yaml");

/// Renders the spec with Scalar (loaded from a CDN).
const SCALAR_HTML: &str = r#"<!doctype html>
<html>
  <head>
    <title>forklift API</title>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
  </head>
  <body>
    <script id="api-reference" data-url="/openapi.yaml"></script>
    <!-- Pinned version (not floating @latest) to reduce CDN-rolling risk. -->
    <script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference@1.25.28" crossorigin="anonymous"></script>
  </body>
</html>"#;

/// Returns the raw OpenAPI document (for tests).
pub fn spec() -> &'static [u8] {
    SPEC
}

/// The spec and docs UI routes, merged into the main router by the caller.
pub fn routes() -> Router {
    Router::new()
        .route("/openapi.yaml", get(serve_spec))
        .route("/api-docs", get(serve_docs))
}

async fn serve_spec() -> Response {
    let mut resp = Response::new(Body::from(SPEC));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/yaml"),
    );
    resp
}

async fn serve_docs() -> Response {
    let mut resp = Response::new(Body::from(SCALAR_HTML));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp
}

#[cfg(test)]
pub(crate) mod tests {
    use axum::body::{Body, to_bytes};
    use http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use crate::openapi::*;

    /// Fetches one path from the routes and returns status, content type and body.
    async fn call(path: &str) -> (StatusCode, String, String) {
        let resp = routes()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, ct, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn routes_serve_spec_and_docs() {
        let (status, ct, body) = call("/openapi.yaml").await;
        assert_eq!(status, StatusCode::OK, "spec status");
        assert!(
            body.contains("openapi: 3.1.0"),
            "spec body: {}",
            &body[..20]
        );
        assert_eq!(ct, "application/yaml", "content-type");

        let (status, _, body) = call("/api-docs").await;
        assert_eq!(status, StatusCode::OK, "docs status");
        assert!(body.contains("api-reference"), "docs body");
    }

    #[tokio::test]
    async fn spec_parses() {
        let raw = String::from_utf8_lossy(spec()).into_owned();
        assert!(
            !spec().is_empty() && raw.contains("/api/v1/repositories"),
            "spec missing expected paths"
        );
    }
}
