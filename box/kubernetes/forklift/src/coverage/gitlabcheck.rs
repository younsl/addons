//! The connection check the console runs against the configured GitLab.

use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::coverage::Scanner;
use crate::coverage::host::{GitLabCheck, validate_gitlab_url};

/// gitlabCheckTimeout bounds the connection check. The console calls it on load,
/// so an unreachable instance has to fail quickly rather than hang the page.
const GITLAB_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Default, Deserialize)]
struct VersionBody {
    #[serde(default)]
    version: String,
}

impl Scanner {
    /// CheckGitLab asks the configured instance for its version.
    ///
    /// `/api/v4/version` is the right endpoint for this: it requires
    /// authentication, so a 200 proves the URL is reachable and the token is
    /// accepted, while a 401 separates a bad token from a bad URL. The request is
    /// made directly rather than through the scan client so the raw status
    /// survives instead of being folded into the retry and back-pressure
    /// classification a crawl needs.
    pub async fn check_gitlab(&self) -> GitLabCheck {
        let base = self.gitlab_url();
        let mut out = GitLabCheck {
            url: base.clone(),
            ..GitLabCheck::default()
        };
        if base.is_empty() || self.gitlab_token.is_empty() {
            return out;
        }
        let msg = validate_gitlab_url(&base);
        if !msg.is_empty() {
            out.error = msg;
            return out;
        }
        out.configured = true;

        let client = match reqwest::Client::builder()
            .timeout(GITLAB_CHECK_TIMEOUT)
            // A redirect would carry the token somewhere the configuration never
            // named, so the check reports the redirect instead of following it.
            .redirect(reqwest::redirect::Policy::none())
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                out.error = e.to_string();
                return out;
            }
        };

        let started = Instant::now();
        let sent = client
            .get(format!("{base}/api/v4/version"))
            .header("PRIVATE-TOKEN", &self.gitlab_token)
            .send()
            .await;
        out.latency_ms = started.elapsed().as_millis() as i64;
        let resp = match sent {
            Ok(resp) => resp,
            Err(e) => {
                out.error = e.to_string();
                return out;
            }
        };
        let status = resp.status();
        out.status = i64::from(status.as_u16());

        let body = resp.bytes().await.unwrap_or_default();
        let body = &body[..body.len().min(8 << 10)];
        if status == reqwest::StatusCode::OK {
            if let Ok(v) = serde_json::from_slice::<VersionBody>(body) {
                out.version = v.version;
            }
            out.reachable = true;
        } else if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            out.error = "the access token was rejected; it needs read_api on the projects to scan"
                .to_string();
        } else {
            out.error = format!("the API returned HTTP {}", status.as_u16());
        }
        out
    }
}
