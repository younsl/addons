//! Runs instant queries against a Prometheus-compatible HTTP API and is
//! deliberately limited to that one read-only operation.
//!
//! Both Prometheus and Grafana Mimir are supported, since Mimir implements the
//! same `/api/v1/query` contract. Two deployment differences are handled here
//! so the same config works against either backend: the API path prefix
//! (Prometheus serves `<base>/api/v1`, Mimir usually `<base>/prometheus/api/v1`;
//! preflight probes both and pins whichever answers) and the tenant header
//! (`X-Scope-OrgID`, which Mimir requires and Prometheus ignores).
//!
//! Queries are sent as POST with a form-encoded body rather than GET, because
//! a generated `PromQL` expression is long enough to hit URL length limits in
//! proxies that sit in front of either backend.

use std::collections::BTreeMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tracing::{debug, warn};

use crate::alertmanager::{Preflight, snippet};

/// The instant-query endpoint, appended after the resolved API prefix.
const QUERY_PATH: &str = "/api/v1/query";

/// The base-URL suffixes probed by preflight, in order: the empty prefix
/// matches Prometheus and a Mimir URL that already includes the prefix,
/// `/prometheus` matches a bare Mimir gateway or query-frontend URL.
const API_PREFIXES: [&str; 2] = ["", "/prometheus"];

/// The cheapest expression that exercises the full query path (routing,
/// auth, tenant resolution) without reading any series.
const PREFLIGHT_QUERY: &str = "vector(1)";

/// Carries the Mimir tenant ID. It is ignored by Prometheus.
const TENANT_HEADER: &str = "X-Scope-OrgID";

/// One series of an instant-query result: its label set and the single value
/// at the evaluation timestamp.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub labels: BTreeMap<String, String>,
    pub value: f64,
}

/// A query that failed. `status` is the HTTP status when the request
/// completed, 0 otherwise.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct QueryError {
    pub status: u16,
    pub message: String,
}

/// Queries a Prometheus-compatible endpoint. Safe for concurrent use.
pub struct Client {
    base: String,
    http: reqwest::Client,
    headers: BTreeMap<String, String>,
    /// Pinned once by preflight and read on every query.
    prefix: RwLock<String>,
}

impl Client {
    /// Builds a client targeting `base_url`, which may be a Prometheus server,
    /// a Mimir query-frontend, or a Mimir gateway, with or without the
    /// `/prometheus` API prefix. `tenant_id`, when non-empty, is sent as
    /// `X-Scope-OrgID`. `headers` are extra static headers merged into every
    /// request and win over the tenant header on a key collision.
    #[must_use]
    pub fn new(
        base_url: &str,
        tenant_id: &str,
        timeout: Duration,
        headers: BTreeMap<String, String>,
    ) -> Self {
        let timeout = if timeout.is_zero() {
            Duration::from_secs(30)
        } else {
            timeout
        };
        let mut merged = BTreeMap::new();
        if !tenant_id.is_empty() {
            merged.insert(TENANT_HEADER.to_string(), tenant_id.to_string());
        }
        merged.extend(headers);
        Self {
            base: base_url.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .unwrap_or_default(),
            headers: merged,
            prefix: RwLock::new(String::new()),
        }
    }

    /// The full query endpoint currently in use. Before preflight runs it
    /// reflects the first candidate prefix.
    #[must_use]
    pub fn endpoint(&self) -> String {
        let prefix = self
            .prefix
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        format!("{}{}{QUERY_PATH}", self.base, *prefix)
    }

    /// Resolves the API prefix and verifies the endpoint answers a trivial
    /// query. A prefix is rejected and the next one tried only on 404 or 405,
    /// the statuses a wrong path produces. Any other failure (connection
    /// refused, 401, 500) is reported as-is, so a genuine auth or
    /// connectivity problem is not misreported as a path problem.
    pub async fn preflight(&self) -> Result<Preflight, (Preflight, String)> {
        let mut last: Option<(Preflight, String)> = None;
        for prefix in API_PREFIXES {
            let endpoint = format!("{}{prefix}{QUERY_PATH}", self.base);
            let start = Instant::now();
            let result = self.query_at(&endpoint, PREFLIGHT_QUERY).await;
            let latency = start.elapsed();
            match result {
                Ok(_) => {
                    *self
                        .prefix
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = prefix.to_string();
                    return Ok(Preflight {
                        endpoint,
                        status: 200,
                        latency,
                    });
                }
                Err(err) => {
                    let p = Preflight {
                        endpoint: endpoint.clone(),
                        status: err.status,
                        latency,
                    };
                    if err.status == 404 || err.status == 405 {
                        debug!(
                            endpoint,
                            status = err.status,
                            "prometheus api prefix not found, trying next"
                        );
                        last = Some((p, err.message));
                        continue;
                    }
                    return Err((p, err.message));
                }
            }
        }
        let (p, err) = last.expect("at least one prefix was tried");
        Err((
            p,
            format!(
                "no Prometheus-compatible API found under {} (tried prefixes {API_PREFIXES:?}): {err}",
                self.base
            ),
        ))
    }

    /// Runs one instant query and returns its result vector. A scalar result
    /// is returned as a single label-less sample. An empty result is not an
    /// error: the caller decides whether a node with no data is a problem.
    pub async fn query(&self, query: &str) -> Result<Vec<Sample>, QueryError> {
        self.query_at(&self.endpoint(), query).await
    }

    async fn query_at(&self, endpoint: &str, query: &str) -> Result<Vec<Sample>, QueryError> {
        let mut req = self
            .http
            .post(endpoint)
            .header("Accept", "application/json")
            .form(&[("query", query)]);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|err| QueryError {
            status: 0,
            message: format!("post {endpoint}: {err}"),
        })?;
        let status = resp.status().as_u16();
        let status_text = resp.status().to_string();
        // Prometheus and Mimir report a bad query as 400 with the reason in
        // the JSON body, so the body is read before the status is judged: it
        // carries the only actionable detail.
        let body = resp.text().await.map_err(|err| QueryError {
            status,
            message: format!("read response from {endpoint}: {err}"),
        })?;
        let decoded: Result<WireResponse, _> = serde_json::from_str(&body);
        if let Ok(wire) = &decoded
            && wire.status == "error"
        {
            return Err(QueryError {
                status,
                message: format!(
                    "query rejected by {endpoint} ({}): {}",
                    wire.error_type, wire.error
                ),
            });
        }
        if status >= 300 {
            return Err(QueryError {
                status,
                message: format!(
                    "query {endpoint} returned {status_text}: {}",
                    snippet(&body, 512)
                ),
            });
        }
        let wire = decoded.map_err(|err| QueryError {
            status,
            message: format!("decode response from {endpoint}: {err}"),
        })?;
        for w in &wire.warnings {
            warn!(warning = %w, query, "prometheus query warning");
        }
        decode_result(&wire.data.result_type, &wire.data.result).map_err(|err| QueryError {
            status,
            message: format!("{err} (query {query:?})"),
        })
    }
}

/// The JSON envelope Prometheus and Mimir both return.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WireResponse {
    status: String,
    #[serde(rename = "errorType")]
    error_type: String,
    error: String,
    warnings: Vec<String>,
    data: WireData,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WireData {
    #[serde(rename = "resultType")]
    result_type: String,
    result: serde_json::Value,
}

/// One vector element. `value` is `[<unix seconds>, "<value>"]`: the value is
/// a JSON string, not a number, so NaN and Inf survive the encoding.
#[derive(Debug, Deserialize)]
struct WireSample {
    #[serde(default)]
    metric: BTreeMap<String, String>,
    value: Vec<serde_json::Value>,
}

/// Converts a vector or scalar result into samples. Matrix and string results
/// are rejected: every query this module sends is an instant query that must
/// reduce to one value per series.
fn decode_result(result_type: &str, raw: &serde_json::Value) -> Result<Vec<Sample>, String> {
    match result_type {
        "vector" => {
            let wire: Vec<WireSample> = serde_json::from_value(raw.clone())
                .map_err(|err| format!("decode vector result: {err}"))?;
            wire.into_iter()
                .map(|w| {
                    Ok(Sample {
                        labels: w.metric,
                        value: sample_value(&w.value)?,
                    })
                })
                .collect()
        }
        "scalar" => {
            let pair: Vec<serde_json::Value> = serde_json::from_value(raw.clone())
                .map_err(|err| format!("decode scalar result: {err}"))?;
            Ok(vec![Sample {
                labels: BTreeMap::new(),
                value: sample_value(&pair)?,
            }])
        }
        other => Err(format!(
            "unsupported result type {other:?}, expected vector or scalar"
        )),
    }
}

/// Extracts the float from a `[timestamp, "value"]` pair.
fn sample_value(pair: &[serde_json::Value]) -> Result<f64, String> {
    if pair.len() != 2 {
        return Err(format!(
            "malformed sample: expected [timestamp, value], got {} elements",
            pair.len()
        ));
    }
    let s = pair[1]
        .as_str()
        .ok_or_else(|| "decode sample value: expected a string".to_string())?;
    match s {
        "NaN" => Ok(f64::NAN),
        "+Inf" | "Inf" => Ok(f64::INFINITY),
        "-Inf" => Ok(f64::NEG_INFINITY),
        _ => s
            .parse::<f64>()
            .map_err(|err| format!("parse sample value {s:?}: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn vector(body: &serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "success",
            "data": {"resultType": "vector", "result": body}
        }))
    }

    #[tokio::test]
    async fn query_vector_with_headers() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/query"))
            .and(header("X-Scope-OrgID", "tenant"))
            .and(header("Authorization", "Bearer t"))
            .and(header("content-type", "application/x-www-form-urlencoded"))
            .and(body_string_contains("query=up%7Bjob%3D%22x%22%7D"))
            .respond_with(vector(&serde_json::json!([
                {"metric": {"node": "a"}, "value": [1.0, "12.5"]},
                {"metric": {"node": "b"}, "value": [1.0, "NaN"]},
                {"metric": {}, "value": [1.0, "+Inf"]}
            ])))
            .expect(1)
            .mount(&server)
            .await;
        let c = Client::new(
            &format!("{}/", server.uri()),
            "tenant",
            Duration::ZERO,
            BTreeMap::from([("Authorization".to_string(), "Bearer t".to_string())]),
        );
        assert_eq!(c.endpoint(), format!("{}/api/v1/query", server.uri()));
        let got = c.query("up{job=\"x\"}").await.unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].labels["node"], "a");
        assert!((got[0].value - 12.5).abs() < f64::EPSILON);
        assert!(got[1].value.is_nan());
        assert!(got[2].value.is_infinite());
    }

    #[tokio::test]
    async fn custom_header_overrides_tenant() {
        let c = Client::new(
            "http://x",
            "tenant",
            Duration::from_secs(1),
            BTreeMap::from([(TENANT_HEADER.to_string(), "other".to_string())]),
        );
        assert_eq!(c.headers[TENANT_HEADER], "other");
        assert_eq!(c.headers.len(), 1);
    }

    #[tokio::test]
    async fn query_scalar_and_errors() {
        let server = MockServer::start().await;
        Mock::given(body_string_contains("scalar"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "success", "warnings": ["slow"],
                "data": {"resultType": "scalar", "result": [1.0, "3"]}
            })))
            .mount(&server)
            .await;
        Mock::given(body_string_contains("matrix"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "success", "data": {"resultType": "matrix", "result": []}
            })))
            .mount(&server)
            .await;
        Mock::given(body_string_contains("rejected"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "status": "error", "errorType": "bad_data", "error": "parse error"
            })))
            .mount(&server)
            .await;
        Mock::given(body_string_contains("malformed"))
            .respond_with(vector(&serde_json::json!([{"metric": {}, "value": [1.0]}])))
            .mount(&server)
            .await;
        Mock::given(body_string_contains("unparseable"))
            .respond_with(vector(
                &serde_json::json!([{"metric": {}, "value": [1.0, "abc"]}]),
            ))
            .mount(&server)
            .await;
        Mock::given(body_string_contains("notjson"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
            .mount(&server)
            .await;
        Mock::given(body_string_contains("servererror"))
            .respond_with(ResponseTemplate::new(502).set_body_string("upstream"))
            .mount(&server)
            .await;
        let c = Client::new(&server.uri(), "", Duration::from_secs(1), BTreeMap::new());
        let got = c.query("scalar(1)").await.unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].labels.is_empty());
        assert!((got[0].value - 3.0).abs() < f64::EPSILON);

        let err = c.query("matrix").await.unwrap_err();
        assert!(
            err.message.contains("unsupported result type \"matrix\""),
            "{err}"
        );
        assert!(err.message.contains("(query \"matrix\")"));
        let err = c.query("rejected").await.unwrap_err();
        assert_eq!(err.status, 400);
        assert!(
            err.message.contains("query rejected by")
                && err.message.contains("(bad_data): parse error"),
            "{err}"
        );
        let err = c.query("malformed").await.unwrap_err();
        assert!(err.message.contains("malformed sample"), "{err}");
        let err = c.query("unparseable").await.unwrap_err();
        assert!(err.message.contains("parse sample value \"abc\""), "{err}");
        let err = c.query("notjson").await.unwrap_err();
        assert!(err.message.contains("decode response from"), "{err}");
        let err = c.query("servererror").await.unwrap_err();
        assert_eq!(err.status, 502);
        assert!(
            err.message.contains("returned 502 Bad Gateway: upstream"),
            "{err}"
        );

        let c = Client::new(
            "http://127.0.0.1:1",
            "",
            Duration::from_secs(1),
            BTreeMap::new(),
        );
        let err = c.query("up").await.unwrap_err();
        assert_eq!(err.status, 0);
        assert!(
            err.message
                .starts_with("post http://127.0.0.1:1/api/v1/query"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn preflight_pins_the_mimir_prefix() {
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/query"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(path("/prometheus/api/v1/query"))
            .respond_with(vector(
                &serde_json::json!([{"metric": {}, "value": [1.0, "1"]}]),
            ))
            .mount(&server)
            .await;
        let c = Client::new(&server.uri(), "", Duration::from_secs(1), BTreeMap::new());
        let p = c.preflight().await.unwrap();
        assert_eq!(
            p.endpoint,
            format!("{}/prometheus/api/v1/query", server.uri())
        );
        assert_eq!(c.endpoint(), p.endpoint);
        assert_eq!(c.query("up").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn preflight_prometheus_needs_no_prefix() {
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/query"))
            .respond_with(vector(&serde_json::json!([])))
            .mount(&server)
            .await;
        let c = Client::new(&server.uri(), "", Duration::from_secs(1), BTreeMap::new());
        let p = c.preflight().await.unwrap();
        assert_eq!(p.endpoint, format!("{}/api/v1/query", server.uri()));
        assert_eq!(p.status, 200);
    }

    #[tokio::test]
    async fn preflight_failures() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(405))
            .mount(&server)
            .await;
        let c = Client::new(&server.uri(), "", Duration::from_secs(1), BTreeMap::new());
        let (p, err) = c.preflight().await.unwrap_err();
        assert_eq!(p.status, 405);
        assert!(
            err.contains("no Prometheus-compatible API found under"),
            "{err}"
        );

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;
        let c = Client::new(&server.uri(), "", Duration::from_secs(1), BTreeMap::new());
        let (p, err) = c.preflight().await.unwrap_err();
        assert_eq!(p.status, 401, "auth failure is not a wrong path");
        assert!(err.contains("returned 401"), "{err}");
        assert_eq!(p.endpoint, format!("{}/api/v1/query", server.uri()));
    }

    #[test]
    fn decode_helpers() {
        assert!(
            sample_value(&[serde_json::json!(1.0), serde_json::json!("-Inf")])
                .unwrap()
                .is_infinite()
        );
        assert!(
            sample_value(&[serde_json::json!(1.0), serde_json::json!(2)])
                .unwrap_err()
                .contains("expected a string")
        );
        assert!(
            decode_result("vector", &serde_json::json!({}))
                .unwrap_err()
                .contains("decode vector result")
        );
        assert!(
            decode_result("scalar", &serde_json::json!("x"))
                .unwrap_err()
                .contains("decode scalar result")
        );
    }
}
