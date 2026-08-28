//! The Prometheus registry and the gateway metric set.
//!
//! Every label used here is bounded by configuration or by a fixed set of
//! outcomes. Alert identity (alertname, fingerprint, group key) is deliberately
//! absent: it belongs in the logs, where it costs one line, not in a time
//! series, where it costs one series per alert rule forever.

use std::sync::Mutex;
use std::time::Duration;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::metrics::info::Info;
use prometheus_client::registry::Registry;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ResultLabels {
    pub result: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct SeverityStatusLabels {
    pub severity: String,
    pub status: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct KindResultLabels {
    pub kind: String,
    pub result: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct KindLabels {
    pub kind: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct MethodResultLabels {
    pub method: String,
    pub result: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct MethodLabels {
    pub method: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct AgentResultLabels {
    pub agent: String,
    pub result: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct AgentStateLabels {
    pub agent: String,
    pub state: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ReasonLabels {
    pub reason: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct BuildInfoLabels {
    pub version: String,
    pub commit: String,
    pub rustc_version: String,
}

/// The gateway metric set, registered on its own registry so the exposed
/// series stay limited to what this binary owns.
pub struct Metrics {
    registry: Mutex<Registry>,

    pub webhooks_received: Family<ResultLabels, Counter>,
    pub webhook_duration: Histogram,
    pub alerts_received: Family<SeverityStatusLabels, Counter>,
    pub slack_messages: Family<KindResultLabels, Counter>,
    pub slack_truncations: Family<KindLabels, Counter>,
    pub slack_api_requests: Family<MethodResultLabels, Counter>,
    pub slack_api_duration: Family<MethodLabels, Histogram>,
    pub parent_lookups: Family<ResultLabels, Counter>,
    pub parent_lookup_tries: Histogram,
    pub analyses: Family<AgentResultLabels, Counter>,
    pub analyses_skipped: Family<ReasonLabels, Counter>,
    pub analysis_duration: Histogram,
    pub analysis_queue_wait: Histogram,
    pub analyses_inflight: Gauge,
    pub analyses_queued: Gauge,
    pub analysis_slots: Gauge,
    pub dedupe_entries: Gauge,
    pub agent_requests: Family<MethodResultLabels, Counter>,
    pub agent_duration: Family<MethodLabels, Histogram>,
    pub agent_task_duration: Family<AgentStateLabels, Histogram>,
    pub agent_task_polls: Histogram,

    pub socket_connected: Gauge,
    pub socket_connections: Family<ResultLabels, Counter>,
    pub chat_events: Family<ResultLabels, Counter>,
    pub chat_turns: Family<AgentResultLabels, Counter>,
    pub chat_turn_duration: Histogram,
    pub chat_inflight: Gauge,
    pub chat_slots: Gauge,
    pub chat_sessions: Gauge,
}

const TASK_BUCKETS: [f64; 10] = [1.0, 5.0, 10.0, 20.0, 30.0, 60.0, 90.0, 120.0, 180.0, 300.0];

fn histogram(buckets: &[f64]) -> Histogram {
    Histogram::new(buckets.iter().copied())
}

fn slack_histogram() -> Histogram {
    histogram(&[0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0])
}

fn agent_histogram() -> Histogram {
    histogram(&[0.05, 0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0])
}

fn task_histogram() -> Histogram {
    histogram(&TASK_BUCKETS)
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// Builds and registers the metric set.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn new() -> Self {
        let mut registry = Registry::with_prefix("kagent_gateway");

        let m = Self {
            registry: Mutex::new(Registry::default()),
            webhooks_received: Family::default(),
            webhook_duration: histogram(&[0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]),
            alerts_received: Family::default(),
            slack_messages: Family::default(),
            slack_truncations: Family::default(),
            slack_api_requests: Family::default(),
            slack_api_duration: Family::new_with_constructor(slack_histogram),
            parent_lookups: Family::default(),
            parent_lookup_tries: histogram(&[1.0, 2.0, 3.0, 5.0, 10.0]),
            analyses: Family::default(),
            analyses_skipped: Family::default(),
            analysis_duration: histogram(&TASK_BUCKETS),
            analysis_queue_wait: histogram(&[0.001, 0.1, 1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0]),
            analyses_inflight: Gauge::default(),
            analyses_queued: Gauge::default(),
            analysis_slots: Gauge::default(),
            dedupe_entries: Gauge::default(),
            agent_requests: Family::default(),
            agent_duration: Family::new_with_constructor(agent_histogram),
            agent_task_duration: Family::new_with_constructor(task_histogram),
            agent_task_polls: histogram(&[1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0]),
            socket_connected: Gauge::default(),
            socket_connections: Family::default(),
            chat_events: Family::default(),
            chat_turns: Family::default(),
            chat_turn_duration: histogram(&TASK_BUCKETS),
            chat_inflight: Gauge::default(),
            chat_slots: Gauge::default(),
            chat_sessions: Gauge::default(),
        };

        // Counters get the _total suffix from the encoder, so they are
        // registered without it.
        registry.register(
            "webhooks_received",
            "Alertmanager webhook requests received, by outcome",
            m.webhooks_received.clone(),
        );
        registry.register(
            "webhook_duration_seconds",
            "Time spent serving one Alertmanager webhook request. The analysis runs detached, so this only covers decoding, filtering, and the parent post in post mode",
            m.webhook_duration.clone(),
        );
        registry.register(
            "alerts_received",
            "Individual alerts carried by the received webhooks, by severity and status. One webhook is one alert group and can hold many alerts",
            m.alerts_received.clone(),
        );
        registry.register(
            "slack_messages",
            "Slack chat.postMessage calls, by message kind and outcome",
            m.slack_messages.clone(),
        );
        registry.register(
            "slack_messages_truncated",
            "Messages cut to SLACK_MAX_TEXT before posting, by message kind. A thread truncation means part of the analysis never reached the reader",
            m.slack_truncations.clone(),
        );
        registry.register(
            "slack_api_requests",
            "Slack Web API HTTP attempts, by method and outcome. Counts every attempt, so retries are visible against slack_messages_total",
            m.slack_api_requests.clone(),
        );
        registry.register(
            "slack_api_request_duration_seconds",
            "Latency of one Slack Web API attempt, by method",
            m.slack_api_duration.clone(),
        );
        registry.register(
            "parent_lookups",
            "Searches for the Alertmanager notification to thread under, by outcome. Only used in lookup parent mode",
            m.parent_lookups.clone(),
        );
        registry.register(
            "parent_lookup_attempts",
            "History scans spent on one parent lookup. Rising counts mean the Alertmanager notification keeps arriving after the webhook",
            m.parent_lookup_tries.clone(),
        );
        registry.register(
            "analyses",
            "Agent analysis runs, by the agent that handled the alert and the outcome",
            m.analyses.clone(),
        );
        registry.register(
            "analyses_skipped",
            "Alert groups that were posted but not analysed, by reason",
            m.analyses_skipped.clone(),
        );
        registry.register(
            "analysis_duration_seconds",
            "Wall-clock duration of one polled agent analysis run, measured by the gateway around the whole A2A exchange",
            m.analysis_duration.clone(),
        );
        registry.register(
            "analysis_queue_wait_seconds",
            "Time an accepted alert group waited for a free analysis slot before its agent run started",
            m.analysis_queue_wait.clone(),
        );
        registry.register(
            "analyses_inflight",
            "Agent analysis runs currently executing",
            m.analyses_inflight.clone(),
        );
        registry.register(
            "analyses_queued",
            "Accepted alert groups waiting for a free analysis slot",
            m.analyses_queued.clone(),
        );
        registry.register(
            "analysis_slots",
            "Configured MAX_CONCURRENT_ANALYSES, so saturation can be read as a ratio without hardcoding the limit in a query",
            m.analysis_slots.clone(),
        );
        registry.register(
            "dedupe_entries",
            "Alert groups currently held in the in-memory dedupe store",
            m.dedupe_entries.clone(),
        );
        registry.register(
            "agent_requests",
            "A2A JSON-RPC calls to the kagent controller, by method (message/send, tasks/get, tasks/cancel) and outcome",
            m.agent_requests.clone(),
        );
        registry.register(
            "agent_request_duration_seconds",
            "Latency of one A2A JSON-RPC call, by method. Bounded by KAGENT_REQUEST_TIMEOUT, and unrelated to how long the analysis itself takes",
            m.agent_duration.clone(),
        );
        registry.register(
            "agent_task_duration_seconds",
            "Time the kagent controller took to drive one task to a terminal state, from the accepted submission to the poll that observed the state, by agent and by that state. The gateway adds two states of its own: timeout when the analysis deadline hit first, and unreachable when polling was abandoned after repeated failures",
            m.agent_task_duration.clone(),
        );
        registry.register(
            "agent_task_polls",
            "tasks/get reads spent on one task before it reached a terminal state",
            m.agent_task_polls.clone(),
        );
        registry.register(
            "socket_connected",
            "1 while a Socket Mode connection is established. Readiness stays tied to the HTTP listener, so this gauge is what tells a dropped mention path from a healthy pod",
            m.socket_connected.clone(),
        );
        registry.register(
            "socket_connections",
            "Socket Mode connection attempts, by outcome: ok, error, or disconnect_requested when Slack asked for a reconnect",
            m.socket_connections.clone(),
        );
        registry.register(
            "chat_events",
            "Mention events received over Socket Mode, by outcome: accepted, or the reason the event was dropped",
            m.chat_events.clone(),
        );
        registry.register(
            "chat_turns",
            "Agent turns answering a mention, by the agent that handled it and the outcome",
            m.chat_turns.clone(),
        );
        registry.register(
            "chat_turn_duration_seconds",
            "Wall-clock duration of one mention turn, from the accepted event to the posted reply",
            m.chat_turn_duration.clone(),
        );
        registry.register(
            "chat_inflight",
            "Mention turns currently executing",
            m.chat_inflight.clone(),
        );
        registry.register(
            "chat_slots",
            "Configured MAX_CONCURRENT_CHATS, so saturation can be read as a ratio the same way analysis_slots allows",
            m.chat_slots.clone(),
        );
        registry.register(
            "chat_sessions",
            "Slack threads currently holding an A2A contextId",
            m.chat_sessions.clone(),
        );

        *m.registry.lock().expect("metrics registry lock poisoned") = registry;
        m
    }

    /// Registers the standard `build_info` series.
    pub fn register_build_info(&self, version: &str, commit: &str, rustc_version: &str) {
        let info = Info::new(BuildInfoLabels {
            version: version.to_string(),
            commit: commit.to_string(),
            rustc_version: rustc_version.to_string(),
        });
        self.registry.lock().expect("metrics registry lock poisoned").register(
            "build",
            "Build information. Value is always 1; labels carry the version, git commit, and rustc version",
            info,
        );
    }

    /// Encodes every registered series in the Prometheus text format.
    ///
    /// # Errors
    ///
    /// Returns an error when the encoder fails to write, which a `String`
    /// sink never does in practice.
    pub fn encode(&self) -> Result<String, std::fmt::Error> {
        let mut out = String::new();
        encode(
            &mut out,
            &self
                .registry
                .lock()
                .expect("metrics registry lock poisoned"),
        )?;
        Ok(out)
    }

    /// Records one Slack Web API attempt. `result` is `ok`, `rate_limited`, or
    /// `error`.
    pub fn observe_slack_request(&self, method: &str, result: &str, d: Duration) {
        self.slack_api_requests
            .get_or_create(&MethodResultLabels {
                method: method.to_string(),
                result: result.to_string(),
            })
            .inc();
        self.slack_api_duration
            .get_or_create(&MethodLabels {
                method: method.to_string(),
            })
            .observe(d.as_secs_f64());
    }

    /// Records one A2A JSON-RPC call. `result` is `ok` or `error`.
    pub fn observe_agent_request(&self, method: &str, result: &str, d: Duration) {
        self.agent_requests
            .get_or_create(&MethodResultLabels {
                method: method.to_string(),
                result: result.to_string(),
            })
            .inc();
        self.agent_duration
            .get_or_create(&MethodLabels {
                method: method.to_string(),
            })
            .observe(d.as_secs_f64());
    }

    /// Records how long the controller took to bring one agent's task to
    /// `state`, and how many polls that took.
    #[allow(clippy::cast_precision_loss)]
    pub fn observe_agent_task(&self, agent: &str, state: &str, polls: u32, d: Duration) {
        self.agent_task_duration
            .get_or_create(&AgentStateLabels {
                agent: agent.to_string(),
                state: state.to_string(),
            })
            .observe(d.as_secs_f64());
        self.agent_task_polls.observe(f64::from(polls));
    }

    /// Records one Socket Mode connection attempt. `result` is `ok`, `error`,
    /// or `disconnect_requested`.
    pub fn observe_socket_connection(&self, result: &str) {
        self.socket_connections
            .get_or_create(&ResultLabels {
                result: result.to_string(),
            })
            .inc();
    }

    /// Publishes whether a Socket Mode connection is up.
    pub fn set_socket_connected(&self, up: bool) {
        self.socket_connected.set(i64::from(up));
    }

    /// Counts a webhook by outcome.
    pub fn webhook(&self, result: &str) {
        self.webhooks_received
            .get_or_create(&ResultLabels {
                result: result.to_string(),
            })
            .inc();
    }

    /// Counts one Slack message by kind and outcome.
    pub fn slack_message(&self, kind: &str, result: &str) {
        self.slack_messages
            .get_or_create(&KindResultLabels {
                kind: kind.to_string(),
                result: result.to_string(),
            })
            .inc();
    }

    /// Counts one analysis run by agent and outcome.
    pub fn analysis(&self, agent: &str, result: &str) {
        self.analyses
            .get_or_create(&AgentResultLabels {
                agent: agent.to_string(),
                result: result.to_string(),
            })
            .inc();
    }

    /// Counts one mention turn by agent and outcome.
    pub fn chat_turn(&self, agent: &str, result: &str) {
        self.chat_turns
            .get_or_create(&AgentResultLabels {
                agent: agent.to_string(),
                result: result.to_string(),
            })
            .inc();
    }

    /// Counts one mention event by outcome.
    pub fn chat_event(&self, result: &str) {
        self.chat_events
            .get_or_create(&ResultLabels {
                result: result.to_string(),
            })
            .inc();
    }

    /// Counts one parent lookup by outcome and records the attempts it took.
    pub fn parent_lookup(&self, result: &str, attempts: u32) {
        self.parent_lookups
            .get_or_create(&ResultLabels {
                result: result.to_string(),
            })
            .inc();
        self.parent_lookup_tries.observe(f64::from(attempts));
    }

    /// Reads a counter value in tests and callers that need it.
    #[cfg(test)]
    pub fn counter<L>(family: &Family<L, Counter>, labels: &L) -> u64
    where
        L: Clone + std::hash::Hash + Eq,
    {
        family.get_or_create(labels).get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_registered_series_with_prefix() {
        let m = Metrics::new();
        m.register_build_info("1.2.3", "abc", "1.98.0");
        m.webhook("analyzing");
        m.observe_slack_request("chat.postMessage", "ok", Duration::from_millis(20));
        m.observe_agent_request("message/send", "ok", Duration::from_millis(5));
        m.observe_agent_task("agent", "completed", 3, Duration::from_secs(4));
        m.observe_socket_connection("ok");
        m.set_socket_connected(true);
        m.slack_message("thread", "ok");
        m.analysis("agent", "ok");
        m.chat_turn("agent", "ok");
        m.chat_event("accepted");
        m.parent_lookup("found", 1);
        m.analysis_slots.set(2);

        let out = m.encode().unwrap();
        assert!(out.contains("kagent_gateway_webhooks_received_total{result=\"analyzing\"} 1"));
        assert!(out.contains(
            "kagent_gateway_build_info{version=\"1.2.3\",commit=\"abc\",rustc_version=\"1.98.0\"} 1"
        ));
        assert!(out.contains(
            "kagent_gateway_slack_api_requests_total{method=\"chat.postMessage\",result=\"ok\"} 1"
        ));
        assert!(out.contains(
            "kagent_gateway_slack_api_request_duration_seconds_count{method=\"chat.postMessage\"} 1"
        ));
        assert!(out.contains("kagent_gateway_agent_task_duration_seconds_count{agent=\"agent\",state=\"completed\"} 1"));
        assert!(out.contains("kagent_gateway_socket_connected 1"));
        assert!(out.contains("kagent_gateway_analysis_slots 2"));
        assert!(out.contains("kagent_gateway_parent_lookups_total{result=\"found\"} 1"));
        assert_eq!(
            Metrics::counter(
                &m.chat_events,
                &ResultLabels {
                    result: "accepted".into()
                }
            ),
            1
        );
        m.set_socket_connected(false);
        assert_eq!(m.socket_connected.get(), 0);
    }
}
