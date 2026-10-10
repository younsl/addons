//! Answers whether a tunnel is quiet enough to replace right now, and when
//! this window's calmest moment usually is.

use std::fmt;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use thiserror::Error;
use tracing::{info, warn};

use super::client::{Client, QueryError};
use super::profile::{self, Profile};
use super::quiet::{self, History, LOOKBACK, MIN_SAMPLES, STEP, SUSTAIN, URGENT_PERCENTILE};

/// Decides what an unavailable metric source means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnError {
    /// A failed or empty query is "do not replace". The safe default: no data
    /// means no evidence the moment is quiet.
    #[default]
    Block,
    /// Fall through to the other gates when metrics are unavailable.
    Allow,
}

impl OnError {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Allow => "allow",
        }
    }

    /// Validates an `onError` setting. Empty means the default.
    pub fn parse(s: &str) -> Result<Self, GateError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "block" => Ok(Self::Block),
            "allow" => Ok(Self::Allow),
            _ => Err(GateError::OnError(s.to_string())),
        }
    }
}

impl fmt::Display for OnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The share of the window's traffic distribution that counts as quiet when
/// none is configured.
pub const DEFAULT_PERCENTILE: f64 = 20.0;

/// A gate that could not be built or verified.
#[derive(Debug, Error)]
pub enum GateError {
    #[error("onError must be \"block\" or \"allow\", got {0:?}")]
    OnError(String),
    #[error("traffic gate is enabled but no metric endpoint was configured")]
    MissingClient,
    #[error("traffic gate percentile must be between 0 and 100, got {0}")]
    Percentile(f64),
    #[error(
        "the metric endpoint did not answer a trivial query, so the endpoint, its headers, or network access to it is wrong: {0}"
    )]
    Endpoint(QueryError),
    #[error("no usable VPN traffic metric for {0}: {1}")]
    NoMetric(String, String),
    #[error("the traffic query for {0} returned nothing usable ({1}): {2}")]
    Query(String, String, QueryError),
}

/// Configures the traffic gate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateConfig {
    /// Off, every candidate passes.
    pub enabled: bool,
    /// The only threshold: the share of this connection's own traffic during
    /// past maintenance windows that counts as quiet. A percentile rather than
    /// a byte figure because it needs nothing known about the connection in
    /// advance, and it moves with the connection.
    pub percentile: f64,
    pub on_error: OnError,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            percentile: DEFAULT_PERCENTILE,
            on_error: OnError::Block,
        }
    }
}

/// The gate's verdict on one tunnel.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Assessment {
    pub allowed: bool,
    /// False when the gate is disabled, so callers can tell "passed" from "not
    /// checked".
    pub evaluated: bool,
    /// Explains the verdict, for logs and the Slack card.
    pub detail: String,
    /// The traffic the tunnel is carrying now.
    pub current: f64,
    /// The value at the configured percentile of this window's history.
    pub threshold: f64,
    /// Where `current` falls in that history, in percent.
    pub rank: f64,
    /// How many historical points the distribution was built from.
    pub samples: usize,
    /// False when the verdict could not be drawn from a distribution, so a
    /// caller can tell a measured verdict from an `onError` one.
    pub has_history: bool,
    /// `current` over `threshold`, for the metric that tracks how far the gate
    /// is from opening.
    pub ratio: f64,
    /// The clock time of the window's habitually calmest slot. Empty when
    /// unknown.
    pub recommended_at: String,
    pub recommended_level: f64,
}

/// Describes the tunnel being judged and the window it would be replaced in.
#[derive(Default, Clone)]
pub struct Vars {
    pub vpn_connection_id: String,
    /// Whether a past instant fell inside a maintenance window. It restricts
    /// the distribution to comparable moments; without it the whole lookback
    /// is used, which mixes business hours with nights.
    pub in_window: Option<Arc<dyn Fn(DateTime<Utc>) -> bool + Send + Sync>>,
    /// The timezone recommended clock times are rendered in.
    pub tz: Option<Tz>,
    /// Relaxes the target to the median, for a tunnel whose AWS auto-apply
    /// deadline is close enough that holding out for the quietest slot risks
    /// letting AWS pick the moment instead.
    pub urgent: bool,
}

/// The traffic gate.
///
/// The maintenance window says when maintenance is permitted; a fixed schedule
/// cannot know whether this particular window is busy. This closes that gap by
/// asking the metric store where the present moment sits in the traffic the
/// connection normally carries during that same window.
#[derive(Debug)]
pub struct Gate {
    client: Option<Client>,
    cfg: GateConfig,
    /// The detected profile, resolved on first use and reused.
    profile: Mutex<Option<Profile>>,
}

impl Gate {
    /// Builds a gate. A missing client is only valid when the gate is disabled.
    pub fn new(client: Option<Client>, mut cfg: GateConfig) -> Result<Self, GateError> {
        if cfg.percentile == 0.0 {
            cfg.percentile = DEFAULT_PERCENTILE;
        }
        if cfg.enabled {
            if client.is_none() {
                return Err(GateError::MissingClient);
            }
            if !(0.0..=100.0).contains(&cfg.percentile) {
                return Err(GateError::Percentile(cfg.percentile));
            }
        }
        Ok(Self {
            client,
            cfg,
            profile: Mutex::new(None),
        })
    }

    /// A gate that lets everything through, for callers without a metric
    /// store.
    #[cfg(test)]
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            client: None,
            cfg: GateConfig::default(),
            profile: Mutex::new(None),
        }
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Whether an unusable metric source blocks replacements. When it does, a
    /// metric source that cannot answer at startup is not a warning: every
    /// candidate would be blocked for as long as it stays that way.
    #[must_use]
    pub fn fail_closed(&self) -> bool {
        self.cfg.enabled && self.cfg.on_error == OnError::Block
    }

    /// Runs the query and returns the verdict. It never fails: a metric source
    /// that cannot answer is itself a verdict, decided by `on_error`.
    pub async fn evaluate(&self, v: &Vars) -> Assessment {
        self.evaluate_at(v, Utc::now()).await
    }

    /// `evaluate` with an explicit clock, for tests.
    pub async fn evaluate_at(&self, v: &Vars, now: DateTime<Utc>) -> Assessment {
        if !self.cfg.enabled {
            return Assessment {
                allowed: true,
                ..Assessment::default()
            };
        }
        let Some(client) = &self.client else {
            return self.on_query_failure("metric discovery", "no metric client");
        };

        let profile = match self.resolve_profile(client, v).await {
            Ok(p) => p,
            Err(err) => return self.on_query_failure("metric discovery", &err),
        };

        // One range query answers both halves of the question, so the current
        // value and the distribution it is judged against can never drift
        // apart.
        let lookback = chrono::TimeDelta::from_std(LOOKBACK).unwrap_or_default();
        let samples = match client
            .query_range(&profile.traffic_query(v), now - lookback, now, STEP)
            .await
        {
            Ok(s) => s,
            Err(err) => return self.on_query_failure("traffic history", &failure_reason(&err)),
        };

        let Some(current) = quiet::sustained_now(&samples, now) else {
            return self.on_query_failure(
                "current traffic",
                &format!(
                    "no sample in the last {}, so the exporter is not reporting rather than the tunnel being idle",
                    crate::humanize::go_duration(SUSTAIN)
                ),
            );
        };

        let h = History::new(&samples, v.in_window.as_deref(), v.tz);
        if h.values.len() < MIN_SAMPLES {
            return self.on_query_failure(
                "traffic history",
                &format!(
                    "only {} sample(s) fall inside past maintenance windows and {MIN_SAMPLES} are needed before a percentile means anything; the window or the exporter may be new",
                    h.values.len()
                ),
            );
        }

        let relaxed = v.urgent && self.cfg.percentile < URGENT_PERCENTILE;
        let target = if relaxed {
            URGENT_PERCENTILE
        } else {
            self.cfg.percentile
        };

        let threshold = h.percentile(target);
        let mut a = Assessment {
            evaluated: true,
            current,
            threshold,
            rank: h.rank(current),
            samples: h.values.len(),
            has_history: true,
            ..Assessment::default()
        };
        if threshold > 0.0 {
            a.ratio = current / threshold;
        }
        if let Some((at, level)) = h.quietest() {
            a.recommended_at = at;
            a.recommended_level = level;
        }
        // At or below, so a connection that is genuinely idle during its
        // window passes on a threshold of zero.
        a.allowed = current <= threshold;
        a.detail = self.explain(&a, target, relaxed, v, now);
        a
    }

    /// Renders the verdict the way an approver has to be able to check it: the
    /// measured value, where it sits, and what it was compared against.
    fn explain(
        &self,
        a: &Assessment,
        target: f64,
        relaxed: bool,
        v: &Vars,
        now: DateTime<Utc>,
    ) -> String {
        let days = LOOKBACK.as_secs() / 86_400;
        let mut b = if a.allowed {
            format!(
                "traffic is {}, inside the quietest {target:.0}% of what this connection carries during this window (P{target:.0} is {} across {} samples from the last {days} days)",
                format_value(a.current),
                format_value(a.threshold),
                a.samples
            )
        } else {
            let mut s = format!(
                "traffic is {}, at P{:.0} of what this connection carries during this window, above the P{target:.0} target of {}",
                format_value(a.current),
                a.rank,
                format_value(a.threshold)
            );
            if !a.recommended_at.is_empty() {
                let _ = write!(
                    s,
                    "; this window is usually calmest around {}, at {}",
                    quiet::format_clock(&a.recommended_at, v.tz, now),
                    format_value(a.recommended_level)
                );
            }
            s
        };
        if relaxed {
            let _ = write!(
                b,
                "; the AWS deadline is near, so the target was relaxed from P{:.0} to the median",
                self.cfg.percentile
            );
        }
        b
    }

    /// Proves at startup that the gate can actually answer, rather than
    /// finding out during the first maintenance window.
    ///
    /// `v` may name a real connection or be empty. With a connection the check
    /// goes all the way through detection and the real query; without one it
    /// can only prove the endpoint answers.
    pub async fn verify(&self, v: &Vars) -> Result<(), GateError> {
        if !self.cfg.enabled {
            return Ok(());
        }
        let client = self.client.as_ref().ok_or(GateError::MissingClient)?;

        // vector(1) needs no exporter and no tenant data, so a failure here is
        // the endpoint, the network, or the headers, and nothing else.
        client
            .query("vector(1)")
            .await
            .map_err(GateError::Endpoint)?;
        if v.vpn_connection_id.is_empty() {
            warn!(
                "traffic gate endpoint answered, but no managed VPN connection was available to probe the traffic metric with; the exporter is verified on the first evaluation instead"
            );
            return Ok(());
        }

        let profile = self
            .resolve_profile(client, v)
            .await
            .map_err(|err| GateError::NoMetric(v.vpn_connection_id.clone(), err))?;
        let query = profile.traffic_query(v);
        client
            .query(&query)
            .await
            .map_err(|err| GateError::Query(v.vpn_connection_id.clone(), query.clone(), err))?;
        info!(
            vpn_connection_id = %v.vpn_connection_id,
            query = %query,
            percentile = self.cfg.percentile,
            "traffic gate verified"
        );
        Ok(())
    }

    /// Returns the detected exporter profile, detecting it on first use. The
    /// convention is detected once and reused: probing on every pass would
    /// multiply queries for an answer that does not change while the exporter
    /// stays the same.
    async fn resolve_profile(&self, client: &Client, v: &Vars) -> Result<Profile, String> {
        let cached = self
            .profile
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(p) = cached {
            return Ok(p);
        }
        // Left undetected on failure so a later pass retries: the metric may
        // simply not have been scraped yet when the controller started.
        let profile = profile::detect(client, &v.vpn_connection_id).await?;
        info!(
            profile = %profile,
            vpn_connection_id = %v.vpn_connection_id,
            "detected VPN traffic metric for the traffic gate"
        );
        *self
            .profile
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(profile.clone());
        Ok(profile)
    }

    /// Turns an unusable metric source into a verdict.
    fn on_query_failure(&self, what: &str, reason: &str) -> Assessment {
        let allowed = self.cfg.on_error == OnError::Allow;
        let mut detail = format!("{what} query failed ({reason}); ");
        if allowed {
            detail.push_str("onError is allow, so the traffic gate is skipped");
        } else {
            detail.push_str(
                "onError is block, so the replacement is held until metrics are readable",
            );
        }
        Assessment {
            allowed,
            evaluated: true,
            detail,
            ..Assessment::default()
        }
    }
}

fn failure_reason(err: &QueryError) -> String {
    if matches!(err, QueryError::NoData) {
        "query returned no data".to_string()
    } else {
        err.to_string()
    }
}

/// Renders a metric value compactly. The unit is whatever the query returns,
/// so none is printed.
#[must_use]
pub fn format_value(v: f64) -> String {
    if v == 0.0 {
        "0".to_string()
    } else if v >= 1e9 {
        format!("{:.2}G", v / 1e9)
    } else if v >= 1e6 {
        format!("{:.2}M", v / 1e6)
    } else if v >= 1e3 {
        format!("{:.2}k", v / 1e3)
    } else {
        format!("{v:.2}")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::promx::client::ClientConfig;

    fn client(server: &MockServer) -> Client {
        Client::new(ClientConfig {
            endpoint: server.uri(),
            ..ClientConfig::default()
        })
        .unwrap()
    }

    fn gate(server: &MockServer, on_error: OnError, percentile: f64) -> Gate {
        Gate::new(
            Some(client(server)),
            GateConfig {
                enabled: true,
                percentile,
                on_error,
            },
        )
        .unwrap()
    }

    fn vars() -> Vars {
        Vars {
            vpn_connection_id: "vpn-1".into(),
            tz: Some(chrono_tz::UTC),
            ..Vars::default()
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_785_000_000, 0).unwrap()
    }

    /// Detection answers the first profile probe with data.
    async fn mount_detection(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/api/v1/query"))
            .and(body_string_contains("count%28aws_ec2_vpn_tunnel_data_out_sum"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "success", "data": {"resultType": "vector", "result": [{"metric": {}, "value": [1.0, "2"]}]}
            })))
            .mount(server)
            .await;
    }

    /// A range of `history` samples ending 20 minutes ago, then `recent`
    /// samples at 5m steps up to now.
    fn matrix(history: &[f64], recent: &[f64]) -> serde_json::Value {
        let step = 300_i64;
        let end = now().timestamp();
        let mut values = Vec::new();
        let hist_start = end - step * i64::try_from(history.len() + recent.len()).unwrap();
        for (i, v) in history.iter().enumerate() {
            values.push(json!([
                hist_start + step * i64::try_from(i).unwrap(),
                v.to_string()
            ]));
        }
        let recent_start = end - step * (i64::try_from(recent.len()).unwrap() - 1);
        for (i, v) in recent.iter().enumerate() {
            values.push(json!([
                recent_start + step * i64::try_from(i).unwrap(),
                v.to_string()
            ]));
        }
        json!({"status": "success", "data": {"resultType": "matrix", "result": [{"metric": {}, "values": values}]}})
    }

    async fn mount_range(server: &MockServer, body: serde_json::Value) {
        Mock::given(method("POST"))
            .and(path("/api/v1/query_range"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    #[test]
    fn on_error_parses() {
        assert_eq!(OnError::parse("").unwrap(), OnError::Block);
        assert_eq!(OnError::parse(" Allow ").unwrap(), OnError::Allow);
        assert!(OnError::parse("maybe").is_err());
        assert_eq!(OnError::Allow.to_string(), "allow");
    }

    #[test]
    fn construction_rules() {
        assert!(matches!(
            Gate::new(
                None,
                GateConfig {
                    enabled: true,
                    ..GateConfig::default()
                }
            ),
            Err(GateError::MissingClient)
        ));
        let g = Gate::disabled();
        assert!(!g.enabled());
        assert!(!g.fail_closed());
        // A zero percentile falls back to the default.
        let g = Gate::new(
            None,
            GateConfig {
                enabled: false,
                percentile: 0.0,
                on_error: OnError::Allow,
            },
        )
        .unwrap();
        assert!((g.cfg.percentile - DEFAULT_PERCENTILE).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn percentile_out_of_range_is_rejected_when_enabled() {
        let server = MockServer::start().await;
        let err = Gate::new(
            Some(client(&server)),
            GateConfig {
                enabled: true,
                percentile: 150.0,
                on_error: OnError::Block,
            },
        )
        .unwrap_err();
        assert!(matches!(err, GateError::Percentile(_)));
    }

    #[tokio::test]
    async fn disabled_gate_passes_without_evaluating() {
        let a = Gate::disabled().evaluate(&vars()).await;
        assert!(a.allowed);
        assert!(!a.evaluated);
        assert!(Gate::disabled().verify(&vars()).await.is_ok());
    }

    #[tokio::test]
    async fn quiet_moment_is_allowed_with_a_measured_verdict() {
        let server = MockServer::start().await;
        mount_detection(&server).await;
        // History: 30 samples 1..30, recent three samples all at 2.
        let history: Vec<f64> = (1..=30).map(f64::from).collect();
        mount_range(&server, matrix(&history, &[2.0, 2.0, 2.0])).await;

        let g = gate(&server, OnError::Block, 20.0);
        let a = g.evaluate_at(&vars(), now()).await;
        assert!(a.allowed, "{}", a.detail);
        assert!(a.evaluated && a.has_history);
        assert_eq!(a.samples, 33);
        assert!((a.current - 2.0).abs() < f64::EPSILON);
        assert!(
            a.detail
                .starts_with("traffic is 2.00, inside the quietest 20%"),
            "{}",
            a.detail
        );
        assert!(
            a.detail.contains("across 33 samples from the last 28 days"),
            "{}",
            a.detail
        );
        assert!(a.ratio > 0.0);

        // The profile is cached: a second evaluation needs no probe.
        server.reset().await;
        mount_range(&server, matrix(&history, &[2.0, 2.0, 2.0])).await;
        assert!(g.evaluate_at(&vars(), now()).await.allowed);
    }

    #[tokio::test]
    async fn busy_moment_is_blocked_with_recommendation_and_urgency() {
        let server = MockServer::start().await;
        mount_detection(&server).await;
        // Two days of the same daily shape give every slot two samples.
        let step = 300_i64;
        let end = now().timestamp();
        let mut values = Vec::new();
        for day in [2_i64, 1] {
            for i in 0..24_i64 {
                let t = end - day * 86_400 - step * (23 - i);
                let v = if i == 5 {
                    1.0
                } else {
                    20.0 + f64::from(i32::try_from(i).unwrap())
                };
                values.push(json!([t, v.to_string()]));
            }
        }
        values.push(json!([end, "500"]));
        let body = json!({"status": "success", "data": {"resultType": "matrix", "result": [{"metric": {}, "values": values}]}});
        mount_range(&server, body).await;

        let g = gate(&server, OnError::Block, 20.0);
        let a = g.evaluate_at(&vars(), now()).await;
        assert!(!a.allowed);
        assert!(a.has_history);
        assert!(a.detail.contains("above the P20 target"), "{}", a.detail);
        assert!(
            a.detail.contains("this window is usually calmest around"),
            "{}",
            a.detail
        );
        assert!(a.detail.contains("UTC, at 1.00"), "{}", a.detail);
        assert!(a.rank > 95.0, "{}", a.rank);

        let urgent = Vars {
            urgent: true,
            ..vars()
        };
        let a = g.evaluate_at(&urgent, now()).await;
        assert!(
            a.detail.contains("relaxed from P20 to the median"),
            "{}",
            a.detail
        );
    }

    #[tokio::test]
    async fn window_filter_restricts_the_distribution() {
        let server = MockServer::start().await;
        mount_detection(&server).await;
        let history: Vec<f64> = (1..=30).map(f64::from).collect();
        mount_range(&server, matrix(&history, &[2.0, 2.0, 2.0])).await;
        let g = gate(&server, OnError::Allow, 20.0);
        let v = Vars {
            in_window: Some(Arc::new(|_: DateTime<Utc>| false)),
            ..vars()
        };
        let a = g.evaluate_at(&v, now()).await;
        assert!(a.allowed, "onError allow");
        assert!(!a.has_history);
        assert!(
            a.detail
                .contains("only 0 sample(s) fall inside past maintenance windows"),
            "{}",
            a.detail
        );
        assert!(
            a.detail
                .ends_with("onError is allow, so the traffic gate is skipped"),
            "{}",
            a.detail
        );
    }

    #[tokio::test]
    async fn failures_become_verdicts() {
        let server = MockServer::start().await;
        // No profile answers.
        Mock::given(method("POST"))
            .and(path("/api/v1/query"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "success", "data": {"resultType": "vector", "result": []}
            })))
            .mount(&server)
            .await;
        let g = gate(&server, OnError::Block, 20.0);
        let a = g.evaluate_at(&vars(), now()).await;
        assert!(!a.allowed);
        assert!(
            a.detail.starts_with(
                "metric discovery query failed (no known VPN traffic metric found for vpn-1"
            ),
            "{}",
            a.detail
        );
        assert!(
            a.detail.ends_with("held until metrics are readable"),
            "{}",
            a.detail
        );

        // Detection works, but the range query is empty.
        server.reset().await;
        mount_detection(&server).await;
        mount_range(
            &server,
            json!({"status": "success", "data": {"resultType": "matrix", "result": []}}),
        )
        .await;
        let a = g.evaluate_at(&vars(), now()).await;
        assert!(
            a.detail
                .contains("traffic history query failed (query returned no data)"),
            "{}",
            a.detail
        );

        // Samples exist but none recent.
        server.reset().await;
        mount_detection(&server).await;
        let old: Vec<f64> = (1..=30).map(f64::from).collect();
        let stale = matrix(&old, &[]);
        mount_range(&server, stale).await;
        let a = g
            .evaluate_at(&vars(), now() + chrono::TimeDelta::hours(2))
            .await;
        assert!(
            a.detail.contains("no sample in the last 15m0s"),
            "{}",
            a.detail
        );
    }

    #[tokio::test]
    async fn verify_checks_endpoint_then_metric() {
        let server = MockServer::start().await;
        let g = gate(&server, OnError::Block, 20.0);
        assert!(g.fail_closed());

        // Nothing mounted: the trivial query fails.
        let err = g.verify(&vars()).await.unwrap_err();
        assert!(matches!(err, GateError::Endpoint(_)), "{err}");

        // vector(1) answers but no exporter has data.
        Mock::given(method("POST"))
            .and(path("/api/v1/query"))
            .and(body_string_contains("vector%281%29"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "success", "data": {"resultType": "scalar", "result": [1.0, "1"]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/query"))
            .and(body_string_contains("count%28"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "success", "data": {"resultType": "vector", "result": []}
            })))
            .mount(&server)
            .await;
        let err = g.verify(&vars()).await.unwrap_err();
        assert!(matches!(err, GateError::NoMetric(..)), "{err}");

        // Without a connection to probe, the endpoint answering is enough.
        let empty = Vars {
            vpn_connection_id: String::new(),
            ..vars()
        };
        assert!(g.verify(&empty).await.is_ok());

        // With a profile detected, the traffic query itself must answer.
        server.reset().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/query"))
            .and(body_string_contains("vector%281%29"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "success", "data": {"resultType": "scalar", "result": [1.0, "1"]}
            })))
            .mount(&server)
            .await;
        mount_detection(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/v1/query"))
            .and(body_string_contains("avg_over_time"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let err = g.verify(&vars()).await.unwrap_err();
        assert!(matches!(err, GateError::Query(..)), "{err}");

        server.reset().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/query"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "success", "data": {"resultType": "vector", "result": [{"metric": {}, "value": [1.0, "3"]}]}
            })))
            .mount(&server)
            .await;
        assert!(g.verify(&vars()).await.is_ok());
    }

    #[test]
    fn format_value_scales() {
        assert_eq!(format_value(0.0), "0");
        assert_eq!(format_value(12.345), "12.35");
        assert_eq!(format_value(1_500.0), "1.50k");
        assert_eq!(format_value(2_500_000.0), "2.50M");
        assert_eq!(format_value(3_000_000_000.0), "3.00G");
    }
}
