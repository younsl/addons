//! HTTP client for the upstream forklift management API.

use std::sync::Arc;
use std::time::Duration;

use crate::mcp::metrics::Metrics;
use crate::mcp::server::{Body, Query};

const MAX_RESPONSE_BYTES: usize = 8 << 20;

/// Errors raised while proxying one management-API call.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A non-2xx response from the upstream API, preserved verbatim so the
    /// model sees the same message a human operator would.
    #[error("forklift API returned {status}: {body}")]
    Api { status: u16, body: String },
    /// The tool arguments could not be encoded as a JSON request body.
    #[error("encode request body: {0}")]
    EncodeBody(#[source] serde_json::Error),
    /// The request could not be built, sent or read.
    #[error("{0}")]
    Request(#[source] reqwest::Error),
    /// The method or URL was not valid.
    #[error("{0}")]
    Url(String),
}

/// Client calls the upstream forklift management API.
pub struct Client {
    base_url: String,
    fallback_token: String,
    http: reqwest::Client,
    metrics: Option<Arc<Metrics>>,
}

impl Client {
    /// Builds a Client for the forklift instance at `upstream`.
    ///
    /// `token`, when non-empty, authenticates tool calls whose MCP request carried no
    /// Authorization header.
    pub fn new(upstream: &str, token: &str, metrics: Option<Arc<Metrics>>) -> Arc<Client> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        Arc::new(Client {
            base_url: upstream.trim_end_matches('/').to_string(),
            fallback_token: token.to_string(),
            http,
            metrics,
        })
    }

    /// Resolves the credential for one tool call: the incoming MCP HTTP
    /// request's Authorization header wins; otherwise the fallback token.
    ///
    pub fn authorization(&self, request_header: Option<&str>) -> Option<String> {
        if let Some(v) = request_header
            && !v.is_empty()
        {
            return Some(v.to_string());
        }
        if !self.fallback_token.is_empty() {
            return Some(format!("Bearer {}", self.fallback_token));
        }
        None
    }

    /// Performs one management-API request and returns the raw response body.
    pub async fn do_request(
        &self,
        auth: Option<&str>,
        method: &str,
        path: &str,
        query: &Query,
        body: Option<&Body>,
    ) -> Result<Vec<u8>, Error> {
        let mut u = format!("{}{}", self.base_url, path);
        if !query.is_empty() {
            u.push('?');
            u.push_str(&query.encode());
        }
        let payload = match body {
            Some(b) => Some(serde_json::to_vec(b.as_map()).map_err(Error::EncodeBody)?),
            None => None,
        };
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| Error::Url(format!("invalid method: {e}")))?;
        let mut req = self.http.request(method.clone(), &u);
        if let Some(payload) = payload {
            req = req
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(payload);
        }
        if let Some(auth) = self.authorization(auth) {
            req = req.header(reqwest::header::AUTHORIZATION, auth);
        }
        let resp = match req.send().await {
            Ok(resp) => resp,
            Err(e) => {
                self.record_upstream(method.as_str(), 0, true);
                return Err(Error::Request(e));
            }
        };
        let status = resp.status().as_u16();
        self.record_upstream(method.as_str(), status, false);
        let data = read_limited(resp).await.map_err(Error::Request)?;
        if !(200..300).contains(&status) {
            return Err(Error::Api {
                status,
                body: String::from_utf8_lossy(&data).trim().to_string(),
            });
        }
        Ok(data)
    }

    fn record_upstream(&self, method: &str, status_code: u16, transport_err: bool) {
        if let Some(metrics) = &self.metrics {
            metrics.record_upstream(method, status_code, transport_err);
        }
    }
}

/// Reads at most [`MAX_RESPONSE_BYTES`] of the response body.
async fn read_limited(mut resp: reqwest::Response) -> Result<Vec<u8>, reqwest::Error> {
    let mut data: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        let room = MAX_RESPONSE_BYTES - data.len();
        if chunk.len() >= room {
            data.extend_from_slice(&chunk[..room]);
            break;
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

const UPPER_HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Notably `*` and `/` are escaped, which `application/x-www-form-urlencoded` serializers in
/// Rust leave alone.
pub(crate) fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(UPPER_HEX[(b >> 4) as usize] as char);
                out.push(UPPER_HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}

pub(crate) fn path_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'$'
            | b'&'
            | b'+'
            | b':'
            | b'='
            | b'@' => out.push(b as char),
            _ => {
                out.push('%');
                out.push(UPPER_HEX[(b >> 4) as usize] as char);
                out.push(UPPER_HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}
