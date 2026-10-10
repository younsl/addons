//! The HTTP client for instant and range queries.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use thiserror::Error;

/// A query that did not produce a usable value.
#[derive(Debug, Error)]
pub enum QueryError {
    #[error("prometheus endpoint is required")]
    MissingEndpoint,
    #[error("invalid prometheus endpoint {0:?}")]
    InvalidEndpoint(String),
    /// The query succeeded but matched no series. Distinct from a failure
    /// because the two deserve different treatment: no data may mean a
    /// genuinely idle tunnel, or a wrong query, and the caller decides which.
    #[error("query returned no data")]
    NoData,
    #[error("query prometheus at {endpoint}: {source}")]
    Transport {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("prometheus returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("decode prometheus response: {0}")]
    Decode(String),
    #[error("prometheus query failed: {0}")]
    Failed(String),
    #[error("query returned {0} series; it must aggregate to exactly one")]
    TooManySeries(usize),
    #[error("unsupported result type {0:?}; {1}")]
    ResultType(String, &'static str),
    #[error("sample value {0:?} is not a number")]
    NotANumber(String),
}

/// Parameterizes the client.
#[derive(Debug, Clone, Default)]
pub struct ClientConfig {
    /// The API base URL, the part before `/api/v1/query`.
    pub endpoint: String,
    /// Sent with every request, for tenant selectors such as `X-Scope-OrgID`.
    pub headers: BTreeMap<String, String>,
    /// Bounds a single query.
    pub timeout: Duration,
}

/// Runs queries against a Prometheus-compatible API.
#[derive(Debug, Clone)]
pub struct Client {
    base_url: String,
    headers: BTreeMap<String, String>,
    http: reqwest::Client,
}

/// One point of a range query, in the order the API returned it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub at: DateTime<Utc>,
    pub value: f64,
}

/// The subset of the Prometheus API response that matters here.
#[derive(Deserialize, Default)]
#[serde(default)]
struct QueryResponse {
    status: String,
    error: String,
    data: QueryData,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct QueryData {
    #[serde(rename = "resultType")]
    result_type: String,
    result: serde_json::Value,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Series {
    value: Option<(f64, String)>,
    values: Vec<(f64, String)>,
}

impl Client {
    /// Builds a client.
    pub fn new(cfg: ClientConfig) -> Result<Self, QueryError> {
        if cfg.endpoint.is_empty() {
            return Err(QueryError::MissingEndpoint);
        }
        if reqwest::Url::parse(&cfg.endpoint).is_err() {
            return Err(QueryError::InvalidEndpoint(cfg.endpoint));
        }
        let timeout = if cfg.timeout.is_zero() {
            Duration::from_secs(10)
        } else {
            cfg.timeout
        };
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|err| QueryError::Decode(err.to_string()))?;
        Ok(Self {
            base_url: cfg.endpoint.trim_end_matches('/').to_string(),
            headers: cfg.headers,
            http,
        })
    }

    /// Runs an instant query and returns a single sample value.
    ///
    /// Anything other than exactly one value is an error: a query meant to
    /// gate an irreversible operation has to be unambiguous, and silently
    /// taking the first of several series would hide a missing aggregation.
    pub async fn query(&self, promql: &str) -> Result<f64, QueryError> {
        let parsed = self.post("/api/v1/query", &[("query", promql)]).await?;
        match parsed.data.result_type.as_str() {
            "vector" => {
                let series: Vec<Series> = decode(parsed.data.result)?;
                match series.len() {
                    0 => Err(QueryError::NoData),
                    1 => series[0]
                        .value
                        .as_ref()
                        .map_or(Err(QueryError::NoData), |(_, v)| decode_value(v)),
                    n => Err(QueryError::TooManySeries(n)),
                }
            }
            "scalar" => {
                let (_, v): (f64, String) = decode(parsed.data.result)?;
                decode_value(&v)
            }
            other => Err(QueryError::ResultType(
                other.to_string(),
                "the query must return an instant vector or scalar",
            )),
        }
    }

    /// Runs a range query and returns the one series it must aggregate to.
    ///
    /// The gate judges "quiet" against how much this connection usually
    /// carries during its maintenance window, and that distribution is a range
    /// of samples rather than a single number.
    pub async fn query_range(
        &self,
        promql: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        step: Duration,
    ) -> Result<Vec<Sample>, QueryError> {
        let start_s = start.timestamp().to_string();
        let end_s = end.timestamp().to_string();
        let step_s = step.as_secs().to_string();
        let parsed = self
            .post(
                "/api/v1/query_range",
                &[
                    ("query", promql),
                    ("start", &start_s),
                    ("end", &end_s),
                    ("step", &step_s),
                ],
            )
            .await?;
        if parsed.data.result_type != "matrix" {
            return Err(QueryError::ResultType(
                parsed.data.result_type,
                "a range query must return a matrix",
            ));
        }
        let series: Vec<Series> = decode(parsed.data.result)?;
        match series.len() {
            0 => return Err(QueryError::NoData),
            1 => {}
            n => return Err(QueryError::TooManySeries(n)),
        }
        let mut samples = Vec::with_capacity(series[0].values.len());
        for (ts, raw) in &series[0].values {
            // A NaN in the middle of a range is a gap, not a failure: the
            // exporter may simply not have been scraped then.
            let value = match decode_value(raw) {
                Ok(v) => v,
                Err(QueryError::NoData) => continue,
                Err(err) => return Err(err),
            };
            #[allow(clippy::cast_possible_truncation)]
            let at = DateTime::from_timestamp(*ts as i64, 0)
                .ok_or_else(|| QueryError::Decode(format!("sample timestamp {ts} out of range")))?;
            samples.push(Sample { at, value });
        }
        if samples.is_empty() {
            return Err(QueryError::NoData);
        }
        Ok(samples)
    }

    /// Sends one form-encoded query and decodes the envelope both query shapes
    /// share.
    async fn post(&self, path: &str, form: &[(&str, &str)]) -> Result<QueryResponse, QueryError> {
        let endpoint = format!("{}{path}", self.base_url);
        let mut req = self.http.post(&endpoint).form(form);
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = req.send().await.map_err(|source| QueryError::Transport {
            endpoint: endpoint.clone(),
            source,
        })?;
        let status = resp.status();
        let body = resp.text().await.map_err(|source| QueryError::Transport {
            endpoint: endpoint.clone(),
            source,
        })?;
        if status != reqwest::StatusCode::OK {
            return Err(QueryError::Status {
                status: status.as_u16(),
                body: body.trim().to_string(),
            });
        }
        let parsed: QueryResponse =
            serde_json::from_str(&body).map_err(|err| QueryError::Decode(err.to_string()))?;
        if parsed.status != "success" {
            return Err(QueryError::Failed(parsed.error));
        }
        Ok(parsed)
    }
}

fn decode<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Result<T, QueryError> {
    serde_json::from_value(v).map_err(|err| QueryError::Decode(err.to_string()))
}

/// Reads the value half of a `[timestamp, "value"]` pair. NaN is what
/// Prometheus returns for an undefined expression, and comparing it against a
/// threshold would silently pass.
fn decode_value(raw: &str) -> Result<f64, QueryError> {
    let v: f64 = raw
        .parse()
        .map_err(|_| QueryError::NotANumber(raw.to_string()))?;
    if v.is_nan() {
        return Err(QueryError::NoData);
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn client(server: &MockServer) -> Client {
        let mut headers = BTreeMap::new();
        headers.insert("X-Scope-OrgID".to_string(), "tenant".to_string());
        Client::new(ClientConfig {
            endpoint: format!("{}/", server.uri()),
            headers,
            timeout: Duration::ZERO,
        })
        .unwrap()
    }

    fn vector(values: &[&str]) -> serde_json::Value {
        let result: Vec<_> = values
            .iter()
            .map(|v| json!({"metric": {}, "value": [1.0, v]}))
            .collect();
        json!({"status": "success", "data": {"resultType": "vector", "result": result}})
    }

    #[test]
    fn rejects_bad_config() {
        assert!(matches!(
            Client::new(ClientConfig::default()),
            Err(QueryError::MissingEndpoint)
        ));
        assert!(matches!(
            Client::new(ClientConfig {
                endpoint: "not a url".into(),
                ..ClientConfig::default()
            }),
            Err(QueryError::InvalidEndpoint(_))
        ));
    }

    #[tokio::test]
    async fn instant_query_returns_the_single_value() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/query"))
            .and(header("X-Scope-OrgID", "tenant"))
            .and(body_string_contains("query=up"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vector(&["42.5"])))
            .mount(&server)
            .await;
        let v = client(&server).query("up").await.unwrap();
        assert!((v - 42.5).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn instant_query_handles_scalar_empty_and_ambiguous() {
        let server = MockServer::start().await;
        let c = client(&server);

        let scalar =
            json!({"status": "success", "data": {"resultType": "scalar", "result": [1.0, "1"]}});
        let guard = Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(scalar))
            .mount_as_scoped(&server)
            .await;
        assert!((c.query("vector(1)").await.unwrap() - 1.0).abs() < f64::EPSILON);
        drop(guard);

        let guard = Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vector(&[])))
            .mount_as_scoped(&server)
            .await;
        assert!(matches!(c.query("x").await, Err(QueryError::NoData)));
        drop(guard);

        let guard = Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vector(&["1", "2"])))
            .mount_as_scoped(&server)
            .await;
        assert!(matches!(
            c.query("x").await,
            Err(QueryError::TooManySeries(2))
        ));
        drop(guard);

        let guard = Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vector(&["NaN"])))
            .mount_as_scoped(&server)
            .await;
        assert!(matches!(c.query("x").await, Err(QueryError::NoData)));
        drop(guard);

        let guard = Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vector(&["abc"])))
            .mount_as_scoped(&server)
            .await;
        assert!(matches!(c.query("x").await, Err(QueryError::NotANumber(_))));
        drop(guard);

        let matrix = json!({"status": "success", "data": {"resultType": "matrix", "result": []}});
        let guard = Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(matrix))
            .mount_as_scoped(&server)
            .await;
        let err = c.query("x").await.unwrap_err();
        assert!(
            err.to_string()
                .contains("unsupported result type \"matrix\""),
            "{err}"
        );
        drop(guard);
    }

    #[tokio::test]
    async fn transport_and_api_failures_are_reported() {
        let server = MockServer::start().await;
        let c = client(&server);

        let guard = Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom\n"))
            .mount_as_scoped(&server)
            .await;
        let err = c.query("x").await.unwrap_err();
        assert_eq!(err.to_string(), "prometheus returned 500: boom");
        drop(guard);

        let failed = json!({"status": "error", "error": "parse error"});
        let guard = Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(failed))
            .mount_as_scoped(&server)
            .await;
        assert!(matches!(c.query("x").await, Err(QueryError::Failed(e)) if e == "parse error"));
        drop(guard);

        let guard = Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{not json"))
            .mount_as_scoped(&server)
            .await;
        assert!(matches!(c.query("x").await, Err(QueryError::Decode(_))));
        drop(guard);

        let dead = Client::new(ClientConfig {
            endpoint: "http://127.0.0.1:9".into(),
            ..ClientConfig::default()
        })
        .unwrap();
        assert!(matches!(
            dead.query("x").await,
            Err(QueryError::Transport { .. })
        ));
    }

    #[tokio::test]
    async fn range_query_decodes_samples_and_skips_nan() {
        let server = MockServer::start().await;
        let matrix = json!({"status": "success", "data": {"resultType": "matrix", "result": [
            {"metric": {}, "values": [[1_700_000_000.0, "1"], [1_700_000_300.0, "NaN"], [1_700_000_600.0, "3.5"]]}
        ]}});
        Mock::given(method("POST"))
            .and(path("/api/v1/query_range"))
            .and(body_string_contains("step=300"))
            .and(body_string_contains("start=1699000000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(matrix))
            .mount(&server)
            .await;
        let samples = client(&server)
            .query_range(
                "q",
                DateTime::from_timestamp(1_699_000_000, 0).unwrap(),
                DateTime::from_timestamp(1_700_000_600, 0).unwrap(),
                Duration::from_secs(300),
            )
            .await
            .unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].at.timestamp(), 1_700_000_000);
        assert!((samples[1].value - 3.5).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn range_query_rejects_bad_shapes() {
        async fn range(c: &Client) -> Result<Vec<Sample>, QueryError> {
            c.query_range(
                "q",
                DateTime::from_timestamp(0, 0).unwrap(),
                DateTime::from_timestamp(600, 0).unwrap(),
                Duration::from_secs(300),
            )
            .await
        }

        let server = MockServer::start().await;
        let c = client(&server);

        let guard = Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vector(&["1"])))
            .mount_as_scoped(&server)
            .await;
        assert!(matches!(range(&c).await, Err(QueryError::ResultType(t, _)) if t == "vector"));
        drop(guard);

        let empty = json!({"status": "success", "data": {"resultType": "matrix", "result": []}});
        let guard = Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty))
            .mount_as_scoped(&server)
            .await;
        assert!(matches!(range(&c).await, Err(QueryError::NoData)));
        drop(guard);

        let two = json!({"status": "success", "data": {"resultType": "matrix", "result": [
            {"metric": {}, "values": [[1.0, "1"]]}, {"metric": {}, "values": [[1.0, "2"]]}
        ]}});
        let guard = Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(two))
            .mount_as_scoped(&server)
            .await;
        assert!(matches!(range(&c).await, Err(QueryError::TooManySeries(2))));
        drop(guard);

        let all_nan = json!({"status": "success", "data": {"resultType": "matrix", "result": [
            {"metric": {}, "values": [[1.0, "NaN"]]}
        ]}});
        let guard = Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(all_nan))
            .mount_as_scoped(&server)
            .await;
        assert!(matches!(range(&c).await, Err(QueryError::NoData)));
        drop(guard);

        let bad = json!({"status": "success", "data": {"resultType": "matrix", "result": [
            {"metric": {}, "values": [[1.0, "x"]]}
        ]}});
        let guard = Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(bad))
            .mount_as_scoped(&server)
            .await;
        assert!(matches!(range(&c).await, Err(QueryError::NotANumber(_))));
        drop(guard);
    }
}
