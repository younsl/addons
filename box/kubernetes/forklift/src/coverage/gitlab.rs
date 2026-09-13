//! The GitLab REST client the crawl runs on.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use crate::coverage::adaptive::{
    AdaptiveLimiter, OUTCOME_BACK_PRESSURE, OUTCOME_IGNORED, OUTCOME_OK, new_adaptive_limiter,
    sleep_cancellable,
};
use crate::coverage::{Error, Res};

/// maxResponseBytes caps a single GitLab response body. A repository file is
/// read whole into memory, and the scan reads many of them concurrently.
const MAX_RESPONSE_BYTES: usize = 8 << 20;

/// gitlabTimeout bounds one GitLab request. Tree listings on a large monorepo
/// are the slow case.
const GITLAB_TIMEOUT: Duration = Duration::from_secs(60);

/// saturationThreshold is how long a caller must wait for a slot before the
/// limit counts as the thing holding the scan back. A non-zero floor keeps
/// scheduling noise from reading as demand.
const SATURATION_THRESHOLD: Duration = Duration::from_millis(1);

/// retries bounds how often one request is re-attempted after a retryable
/// failure. It is a constant rather than a setting: the useful range is small,
/// and how hard to push is now the limiter's decision, not a number to tune.
const RETRIES: i64 = 3;

/// maxPages bounds pagination so a GitLab instance that keeps advertising a next
/// page cannot make one scan run forever.
const MAX_PAGES: i64 = 500;

/// GitLabOptions configures a client.
#[derive(Debug, Clone, Default)]
pub struct GitLabOptions {
    pub api_base_url: String,
    pub token: String,
    /// Cancels every in-flight and future request.
    pub cancel: CancellationToken,
}

/// GitLabClient is a GitLab REST client that finds its own safe request rate.
///
/// It has no rate setting. In-flight requests are bounded by an adaptive limiter
/// that ramps up while the instance keeps answering promptly and cuts back the
/// moment it does not; see `adaptive.rs`. Back-pressure applies to every caller
/// rather than only the one that hit it, because per-request backoff alone
/// leaves the other scan workers firing at the same rate and the instance never
/// gets back under its limit.
pub struct GitLabClient {
    api_base_url: String,
    token: String,
    client: reqwest::Client,
    limiter: Arc<AdaptiveLimiter>,
    cancel: CancellationToken,
}

/// NewGitLabClient builds a client against an API base such as
/// "https://gitlab.example.com/api/v4".
pub fn new_gitlab_client(o: GitLabOptions) -> GitLabClient {
    let client = reqwest::Client::builder()
        .timeout(GITLAB_TIMEOUT)
        // Redirects are never followed. The scan sends the access token in a
        // PRIVATE-TOKEN header, which is not stripped on a cross-host redirect,
        // so following one would hand the token to whatever the instance named.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default();
    GitLabClient {
        api_base_url: o.api_base_url.trim_end_matches('/').to_string(),
        token: o.token,
        client,
        limiter: new_adaptive_limiter(),
        cancel: o.cancel,
    }
}

impl GitLabClient {
    /// ConcurrencyStats reports the limiter's current and peak in-flight limit,
    /// so a scan can say what rate it settled on instead of that being invisible.
    pub fn concurrency_stats(&self) -> (i64, i64) {
        self.limiter.stats()
    }

    /// do performs one GET with rate limiting, retrying transport errors, 429s
    /// and 5xx. The caller owns reading the returned body.
    async fn do_request(&self, path: &str) -> Res<reqwest::Response> {
        let url = if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}/{}", self.api_base_url, path.trim_start_matches('/'))
        };

        let mut last_err: Option<Error> = None;
        for attempt in 0..=RETRIES {
            // Waiting for a slot is itself the signal that the limit, not the
            // workload, is the constraint; the limiter only ramps up on that
            // evidence.
            let wait_start = Instant::now();
            self.limiter.acquire(&self.cancel).await?;
            let saturated = wait_start.elapsed() > SATURATION_THRESHOLD;

            let mut req = self.client.get(&url);
            if !self.token.is_empty() {
                req = req.header("PRIVATE-TOKEN", &self.token);
            }
            let started = Instant::now();
            let sent = req.send().await;
            let rtt = started.elapsed();
            let resp = match sent {
                Ok(resp) => resp,
                Err(e) => {
                    // A transport failure or a timeout is what an overloaded
                    // instance looks like from here, so it counts as back-pressure.
                    self.limiter.release(OUTCOME_BACK_PRESSURE, rtt, saturated);
                    if self.cancel.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                    last_err = Some(Error::Http(e.to_string()));
                    sleep_cancellable(
                        &self.cancel,
                        Duration::from_millis(((attempt + 1) * 500) as u64),
                    )
                    .await?;
                    continue;
                }
            };

            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                // The service named a wait, so honour it; the limiter's own cut
                // is not a substitute for a Retry-After.
                if let Some(v) = resp.headers().get("Retry-After")
                    && let Ok(v) = v.to_str()
                    && let Ok(secs) = v.parse::<u64>()
                    && secs > 0
                {
                    let wait = Duration::from_secs(secs);
                    tracing::warn!(r#for = ?wait, "coverage: gitlab asked for a pause");
                    self.limiter.pause(wait);
                }
                drain(resp).await;
                self.limiter.release(OUTCOME_BACK_PRESSURE, rtt, saturated);
                last_err = Some(Error::Msg("gitlab API rate limited (429)".to_string()));
                continue;
            }
            if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::FORBIDDEN
            {
                drain(resp).await;
                // A missing or invisible resource says nothing about capacity.
                self.limiter.release(OUTCOME_IGNORED, rtt, saturated);
                return Err(Error::NotFoundAt {
                    path: path.to_string(),
                    status: status.as_u16(),
                });
            }
            if status.as_u16() >= 500 {
                drain(resp).await;
                self.limiter.release(OUTCOME_BACK_PRESSURE, rtt, saturated);
                last_err = Some(Error::Msg(format!(
                    "gitlab API {path} returned {}",
                    status.as_u16()
                )));
                sleep_cancellable(
                    &self.cancel,
                    Duration::from_millis(((attempt + 1) * 500) as u64),
                )
                .await?;
                continue;
            }
            if status.as_u16() >= 300 {
                drain(resp).await;
                self.limiter.release(OUTCOME_IGNORED, rtt, saturated);
                return Err(Error::Msg(format!(
                    "gitlab API {path} returned {}",
                    status.as_u16()
                )));
            }

            // The body is still to be read by the caller, so the slot is released
            // here rather than after: what the limiter measures is the service's
            // response time, not how long forklift takes to parse it.
            self.limiter.release(OUTCOME_OK, rtt, saturated);
            return Ok(resp);
        }
        Err(last_err.unwrap_or_else(|| Error::Msg(format!("gitlab API {path} failed"))))
    }

    /// GetJSON decodes one JSON response into `T`.
    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Res<T> {
        let resp = self.do_request(path).await?;
        let body = read_capped(resp).await?;
        serde_json::from_slice(&body).map_err(Error::from)
    }

    /// GetText reads one response body as text, capped at
    /// [`MAX_RESPONSE_BYTES`].
    pub async fn get_text(&self, path: &str) -> Res<String> {
        let resp = self.do_request(path).await?;
        let body = read_capped(resp).await?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
}

async fn drain(resp: reqwest::Response) {
    let _ = resp.bytes().await;
}

async fn read_capped(resp: reqwest::Response) -> Res<Vec<u8>> {
    let body = resp.bytes().await.map_err(|e| Error::Http(e.to_string()))?;
    let end = body.len().min(MAX_RESPONSE_BYTES);
    Ok(body[..end].to_vec())
}

/// get_pages follows GitLab's `x-next-page` header, appending each page's items.
/// `T` is decoded per page rather than accumulated as raw JSON so a large
/// project list is not held twice.
pub async fn get_pages<T: DeserializeOwned>(c: &GitLabClient, path: &str) -> Res<Vec<T>> {
    let mut collected: Vec<T> = Vec::new();
    let mut page: i64 = 1;
    for _ in 0..MAX_PAGES {
        let sep = if path.contains('?') { "&" } else { "?" };
        let resp = c.do_request(&format!("{path}{sep}page={page}")).await?;
        let next = resp
            .headers()
            .get("x-next-page")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = read_capped(resp).await?;
        let batch: Vec<T> = serde_json::from_slice(&body)?;
        collected.extend(batch);
        match next.parse::<i64>() {
            Ok(parsed) if !next.is_empty() && parsed > page => page = parsed,
            _ => break,
        }
    }
    Ok(collected)
}
