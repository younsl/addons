use std::sync::Arc;

use axum::extract::{Request, State};
use axum::response::Response;
use chrono::{DateTime, Utc};
use http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::meta;

use super::{Handler, map_error, principal_name, write_error, write_json};

/// Bounds the Markdown source. An announcement is a short banner, not a
/// document; the cap keeps a pathological body out of every main page load.
const MAX_ANNOUNCEMENT_BYTES: usize = 16 << 10;

/// The fixed character limit shared with the web editor's live counter. It fits
/// inside [`MAX_ANNOUNCEMENT_BYTES`] even for scripts that encode to three bytes
/// per character, so the byte cap stays a transport guard rather than something
/// a user can hit first.
const MAX_ANNOUNCEMENT_CHARS: usize = 1000;

#[derive(Debug, Clone, Serialize)]
struct AnnouncementDTO {
    body: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    updated_by: String,
    updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<DateTime<Utc>>,
    /// Whether the banner shows now: a body that has not reached its end.
    active: bool,
    /// Whether signed-in users may dismiss the banner.
    user_dismissible: bool,
}

impl Default for AnnouncementDTO {
    fn default() -> Self {
        Self {
            body: String::new(),
            updated_by: String::new(),
            updated_at: DateTime::parse_from_rfc3339("0001-01-01T00:00:00Z")
                .expect("valid instant")
                .with_timezone(&Utc),
            expires_at: None,
            active: false,
            user_dismissible: false,
        }
    }
}

impl From<meta::Announcement> for AnnouncementDTO {
    fn from(a: meta::Announcement) -> Self {
        let active = a.active(Utc::now());
        Self {
            body: a.body,
            updated_by: a.updated_by,
            updated_at: a.updated_at,
            expires_at: a.expires_at,
            active,
            user_dismissible: a.user_dismissible,
        }
    }
}

/// The PUT body. A named type rather than an inline struct so the OpenAPI
/// document can be pinned to it.
#[derive(Debug, Clone, Default, Deserialize)]
struct AnnouncementInput {
    #[serde(default)]
    body: String,
    /// When the banner stops showing. Absent or null keeps it up until cleared.
    #[serde(default)]
    expires_at: Option<DateTime<Utc>>,
    /// Lets signed-in users dismiss the banner. Off by default, as on GitHub
    /// Enterprise Server.
    #[serde(default)]
    user_dismissible: bool,
}

/// Returns the site-wide notice. Never-set and cleared both come back as an
/// empty body so the UI has one state to handle.
pub(super) async fn get(State(h): State<Arc<Handler>>) -> Response {
    match h.store.get_announcement().await {
        Ok(a) => write_json(StatusCode::OK, AnnouncementDTO::from(a)),
        Err(meta::Error::NotFound) => write_json(StatusCode::OK, AnnouncementDTO::default()),
        Err(err) => map_error(err),
    }
}

/// Replaces the notice (admin only, enforced by routing). The body is stored as
/// Markdown source verbatim; rendering and sanitization are the UI's
/// responsibility, and it never injects raw HTML.
pub(super) async fn put(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_ANNOUNCEMENT_BYTES * 2).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid body");
    };
    let Ok(input) = serde_json::from_slice::<AnnouncementInput>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid body");
    };
    if input.body.len() > MAX_ANNOUNCEMENT_BYTES {
        return write_error(StatusCode::BAD_REQUEST, "announcement too long (max 16KiB)");
    }
    if input.body.chars().count() > MAX_ANNOUNCEMENT_CHARS {
        return write_error(
            StatusCode::BAD_REQUEST,
            "announcement too long (max 1000 characters)",
        );
    }
    // A cleared notice has nothing to end. A set one must end in the future,
    // or it would be saved already hidden.
    let expires_at = if input.body.is_empty() {
        None
    } else {
        input.expires_at
    };
    if expires_at.is_some_and(|end| end <= Utc::now()) {
        return write_error(StatusCode::BAD_REQUEST, "expires_at must be in the future");
    }
    match h
        .store
        .set_announcement(
            &input.body,
            &principal_name(&parts),
            expires_at,
            input.user_dismissible && !input.body.is_empty(),
        )
        .await
    {
        Ok(a) => write_json(StatusCode::OK, AnnouncementDTO::from(a)),
        Err(err) => map_error(err),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use http::{Method, StatusCode};

    use crate::testing::api::{ADMIN_USER, new_test_server};

    /// Covers the single-notice flow: empty by default, admin sets Markdown, any
    /// authenticated user reads it back, admin clears it, and a non-admin cannot
    /// write.
    #[tokio::test]
    async fn announcement_lifecycle() {
        let srv = new_test_server().await;

        let get = async || {
            let resp = srv.admin_do(Method::GET, "/announcement", "").await;
            assert_eq!(resp.status, StatusCode::OK, "get announcement");
            resp.json()
        };

        assert_eq!(
            get().await["body"],
            "",
            "expected an empty default announcement"
        );

        let resp = srv
            .admin_do(
                Method::PUT,
                "/announcement",
                r##"{"body":"# Maintenance :warning:\n\n**tonight** 22:00 KST"}"##,
            )
            .await;
        assert_eq!(resp.status, StatusCode::OK, "put announcement");
        let dto = get().await;
        assert!(
            dto["body"]
                .as_str()
                .is_some_and(|body| body.contains("# Maintenance")),
            "{dto}"
        );
        assert_eq!(dto["updated_by"], ADMIN_USER, "{dto}");

        // Oversized bodies are rejected before storage: over the byte cap, and over
        // the character cap while still under the byte cap (multi-byte characters
        // count as one each, so the boundary case stays accepted).
        let resp = srv
            .admin_do(
                Method::PUT,
                "/announcement",
                &format!(r#"{{"body":"{}"}}"#, "x".repeat((16 << 10) + 1)),
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::BAD_REQUEST,
            "oversized announcement"
        );
        let resp = srv
            .admin_do(
                Method::PUT,
                "/announcement",
                &format!(r#"{{"body":"{}"}}"#, "x".repeat(1001)),
            )
            .await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "over char cap");
        let resp = srv
            .admin_do(
                Method::PUT,
                "/announcement",
                &format!(r#"{{"body":"{}"}}"#, "가".repeat(1000)),
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::OK,
            "at char cap with multi-byte characters: {}",
            resp.text()
        );

        // An optional end: past ends are refused, a future one is kept and the
        // notice stays active until it passes.
        let resp = srv
            .admin_do(
                Method::PUT,
                "/announcement",
                r#"{"body":"window","expires_at":"2000-01-01T00:00:00Z"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "past end");
        let resp = srv
            .admin_do(
                Method::PUT,
                "/announcement",
                r#"{"body":"window","expires_at":"2999-01-01T00:00:00Z"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::OK, "future end: {}", resp.text());
        let dto = get().await;
        assert_eq!(dto["expires_at"], "2999-01-01T00:00:00Z", "{dto}");
        assert_eq!(dto["active"], true, "{dto}");
        assert_eq!(dto["user_dismissible"], false, "off unless asked: {dto}");
        let resp = srv
            .admin_do(
                Method::PUT,
                "/announcement",
                r#"{"body":"window","user_dismissible":true}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
        assert_eq!(get().await["user_dismissible"], true);

        // Clearing stores an empty body.
        let resp = srv
            .admin_do(Method::PUT, "/announcement", r#"{"body":""}"#)
            .await;
        assert_eq!(resp.status, StatusCode::OK, "clear announcement");
        let cleared = get().await;
        assert_eq!(cleared["body"], "", "expected a cleared announcement");
        assert_eq!(cleared["active"], false, "{cleared}");
        assert!(cleared.get("expires_at").is_none(), "{cleared}");
    }

    /// A plain authenticated user can read but not write the announcement.
    #[tokio::test]
    async fn announcement_requires_admin() {
        let srv = new_test_server().await;

        // Create a non-admin user with a known password.
        let resp = srv
            .admin_do(
                Method::POST,
                "/users",
                r#"{"username":"reader","password":"reader-pass-123"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::CREATED, "create user");

        let resp = srv
            .do_as(
                "reader",
                "reader-pass-123",
                Method::GET,
                "/announcement",
                "",
            )
            .await;
        assert_eq!(resp.status, StatusCode::OK, "reader GET");
        let resp = srv
            .do_as(
                "reader",
                "reader-pass-123",
                Method::PUT,
                "/announcement",
                r#"{"body":"hijack"}"#,
            )
            .await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "reader PUT");
    }
}
