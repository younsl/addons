//! The console page and its assets, compiled into the binary.

use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
};
use axum::response::IntoResponse;

const INDEX: &str = include_str!("../webui/index.html");
const CSS: &str = include_str!("../webui/app.css");
const JS: &str = include_str!("../webui/app.js");

const CSP: &str = "default-src 'self'; script-src 'self' https://cdnjs.cloudflare.com; \
style-src 'self' https://fonts.googleapis.com; \
font-src https://fonts.gstatic.com; connect-src 'self'; img-src 'self' data:; \
base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

fn asset(content_type: &'static str, body: &'static str) -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, content_type),
            (CACHE_CONTROL, "no-cache"),
            (X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (REFERRER_POLICY, "no-referrer"),
            (CONTENT_SECURITY_POLICY, CSP),
        ],
        body,
    )
}

pub async fn index() -> impl IntoResponse {
    asset("text/html; charset=utf-8", INDEX)
}

pub async fn css() -> impl IntoResponse {
    asset("text/css; charset=utf-8", CSS)
}

pub async fn js() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", JS)
}
