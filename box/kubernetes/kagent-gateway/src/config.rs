//! Gateway settings loaded from environment variables.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

/// Appended to every prompt sent to the agent. It is deliberately generic:
/// deployment-specific wording (language, runbook links, escalation policy)
/// belongs in `ANALYSIS_INSTRUCTIONS` or in the agent's own system message,
/// not in this binary.
pub const DEFAULT_INSTRUCTIONS: &str =
    "Investigate the alert above with the tools available to you.
Inspect only; never create, modify, or delete any resource.
Reply in Slack mrkdwn: *bold* uses single asterisks, no markdown headings, no tables.
Keep the whole reply under 3000 characters and use exactly these sections:
*Summary* - one or two sentences on what is broken.
*Evidence* - the queries or commands you ran and what they returned.
*Likely cause* - at most three ranked hypotheses.
*Next actions* - concrete steps for the on-call engineer.
*Confidence* - high, medium, or low, with the reason.";

/// Appended to every mention prompt. A question has no alert sections to
/// fill, so it asks for a direct answer rather than the analysis layout
/// `ANALYSIS_INSTRUCTIONS` prescribes.
pub const DEFAULT_CHAT_INSTRUCTIONS: &str =
    "Answer the question above with the tools available to you.
The alert the question was asked under is quoted above it. When the rest of that
conversation matters and you have a Slack tool, read the thread with the channel
and thread identifiers given above; otherwise answer from the alert alone.
Inspect only; never create, modify, or delete any resource.
Reply in Slack mrkdwn: *bold* uses single asterisks, no markdown headings, no tables.
Keep the reply under 2000 characters and answer directly, without restating the question.
Say so plainly when the tools cannot answer, instead of guessing.";

/// Built-in ephemeral hint for a mention outside a thread.
pub const DEFAULT_THREAD_HINT: &str =
    "스레드 안에서만 답변합니다. 질문할 스레드에서 다시 멘션해 주세요.";
/// Built-in ephemeral hint for a mention in a channel the bot does not serve.
/// It never names the channels that are served.
pub const DEFAULT_DENIED_HINT: &str = "이 채널에서는 답변하지 않습니다.";

/// How the parent message an analysis threads under comes to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentMode {
    /// Alertmanager posts the notification through its own `slack_configs`,
    /// and the gateway finds it again in channel history to thread under.
    Lookup,
    /// The gateway publishes the alert itself, which yields the thread
    /// timestamp directly. Alertmanager sends only the webhook.
    Post,
}

impl ParentMode {
    /// Renders the mode the way `SLACK_PARENT_MODE` spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lookup => "lookup",
            Self::Post => "post",
        }
    }
}

/// All runtime settings for the gateway.
#[derive(Debug, Clone)]
pub struct Config {
    // Slack
    pub slack_token: String,
    pub slack_api_url: String,
    pub slack_channel: String,
    pub slack_channel_map: HashMap<String, String>,
    pub channel_label: String,
    pub slack_max_text_chars: usize,
    pub parent_mode: ParentMode,
    pub lookup_window: Duration,
    pub lookup_attempts: u32,
    /// Emoji (without colons) the gateway puts on the alert notification
    /// while the agent works, and removes when the analysis lands. Empty
    /// disables the reaction. Needs `reactions:write`.
    pub investigating_reaction: String,
    /// Replaces `investigating_reaction` once the analysis has been posted.
    /// Empty disables it.
    pub completed_reaction: String,

    // kagent A2A
    pub kagent_url: String,
    pub kagent_namespace: String,
    /// Agent used when the routing label is absent or names no mapped agent.
    pub kagent_agent: String,
    /// Alert label that selects the agent. Defaults to `channel_label`,
    /// because a deployment that already routes alerts to per-topic Slack
    /// channels usually wants the same split across agents.
    pub kagent_agent_routing_label: String,
    /// Routes a label value to the agent that handles it. Unlike
    /// `slack_channel_map` it is not an alias table: a value it does not carry
    /// falls back to `kagent_agent` rather than being used as an agent name.
    pub kagent_agent_routing_map: HashMap<String, String>,
    /// Identity the alert path submits as, sent as `X-User-Id`.
    pub kagent_user_id: String,
    /// Deadline for one whole analysis: queueing for a slot, the parent
    /// lookup, and the polled agent run.
    pub kagent_timeout: Duration,
    /// Bounds a single HTTP call to the controller, not the analysis.
    pub kagent_request_timeout: Duration,
    /// Wait between two `tasks/get` reads.
    pub kagent_poll_interval: Duration,

    // Analysis gating
    pub analyze_severities: HashSet<String>,
    pub analyze_label: String,
    pub analyze_resolved: bool,
    pub dedupe_ttl: Duration,
    pub max_alerts_in_prompt: usize,
    pub max_concurrent: usize,
    pub instructions: String,

    // Slack mention invocation
    /// App-level token (`xapp-...`) that opens the Socket Mode connection.
    /// Empty leaves the whole mention path off.
    pub slack_app_token: String,
    /// Answers mentions that `chat_agent_map` does not route elsewhere.
    pub chat_agent: String,
    /// Routes a channel (name or ID) to a specialised agent.
    pub chat_agent_map: HashMap<String, String>,
    /// Channel allow list, holding names or IDs. Empty allows every channel
    /// the bot is a member of.
    pub chat_channels: Vec<String>,
    /// Slack member ID allow list. Empty allows everyone in the allowed
    /// channels.
    pub chat_allowed_users: HashSet<String>,
    pub chat_instructions: String,
    /// Identity the mention path submits as. Defaults to `kagent_user_id`, so
    /// a deployment that does not set it keeps both paths under one owner.
    /// Splitting them matters once agents carry long-term memory: memory is
    /// keyed by agent and user, so a shared identity lets the unattended alert
    /// path write into the pool a person's questions read from.
    pub chat_user_id: String,
    /// Deadline for one whole turn, including queueing.
    pub chat_timeout: Duration,
    /// How long a thread keeps its A2A `contextId` after its last turn.
    pub chat_session_ttl: Duration,
    /// How often the in-thread status message is rewritten while the agent
    /// works.
    pub chat_status_interval: Duration,
    /// Ephemeral hints for the two drops a person cannot tell from an outage.
    /// Empty restores a silent drop.
    pub chat_thread_hint: String,
    pub chat_denied_hint: String,
    pub max_concurrent_chats: usize,

    // Serving
    pub webhook_path: String,
    pub webhook_token: String,
    pub listen_port: u16,
    pub metrics_port: u16,
    pub log_level: String,
    pub log_format: String,
}

impl Config {
    /// Reads the configuration from the process environment.
    ///
    /// # Errors
    ///
    /// Returns an error when a required variable is missing or a value is out
    /// of range.
    pub fn load() -> Result<Self> {
        Self::from_env(|key| std::env::var(key).ok())
    }

    /// Reads the configuration through `env`, which returns `None` for an
    /// unset variable. Separating the lookup from the process environment is
    /// what keeps the parsing testable without mutating global state.
    ///
    /// # Errors
    ///
    /// Returns an error when a required variable is missing or a value is out
    /// of range.
    #[allow(clippy::too_many_lines)]
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let env = Env(&env);
        let channel_label = env.string("SLACK_CHANNEL_LABEL", "slack_channel");
        let kagent_agent = env.string("KAGENT_AGENT", "alert-triage-agent");
        let kagent_user_id = env.string("KAGENT_USER_ID", "gateway@kagent.dev");

        let parent_mode = match env.string("SLACK_PARENT_MODE", "lookup").as_str() {
            "lookup" => ParentMode::Lookup,
            "post" => ParentMode::Post,
            other => bail!("invalid SLACK_PARENT_MODE {other:?}: must be \"lookup\" or \"post\""),
        };

        let webhook_path = env.string("WEBHOOK_PATH", "/alert");
        if !webhook_path.starts_with('/') {
            bail!("WEBHOOK_PATH {webhook_path:?} must start with /");
        }

        let slack_token = env.raw("SLACK_BOT_TOKEN");
        if slack_token.is_empty() {
            bail!("SLACK_BOT_TOKEN is required");
        }
        let slack_channel = env.raw("SLACK_DEFAULT_CHANNEL");
        if slack_channel.is_empty() {
            bail!("SLACK_DEFAULT_CHANNEL is required");
        }

        let listen_port = env.int("LISTEN_PORT", 8080, 1, 65535)?;
        let metrics_port = env.int("METRICS_PORT", 8081, 1, 65535)?;
        if listen_port == metrics_port {
            bail!("LISTEN_PORT and METRICS_PORT must differ, both are {listen_port}");
        }

        let cfg = Self {
            slack_token,
            slack_api_url: env.string("SLACK_API_URL", "https://slack.com/api"),
            slack_channel,
            slack_channel_map: env.pairs("SLACK_CHANNEL_MAP")?,
            channel_label: channel_label.clone(),
            // Headroom over the 3000 characters the instructions ask for:
            // agents overrun that regularly, and truncation is a safety net
            // rather than a formatter. 8000 also stays inside the Slack
            // attachment text limit.
            slack_max_text_chars: env.int("SLACK_MAX_TEXT", 8000, 500, 39000)?,
            parent_mode,
            lookup_window: env.duration(
                "SLACK_LOOKUP_WINDOW",
                Duration::from_mins(15),
                Duration::from_secs(60),
            )?,
            lookup_attempts: env.int("SLACK_LOOKUP_ATTEMPTS", 3, 1, 10)?,
            // An explicitly empty value is meaningful: it turns the reaction
            // off, while an unset variable keeps the default.
            investigating_reaction: env.optional(
                "SLACK_INVESTIGATING_REACTION",
                "telescope",
                trim_colons,
            ),
            completed_reaction: env.optional(
                "SLACK_COMPLETED_REACTION",
                "white_check_mark",
                trim_colons,
            ),
            kagent_url: env.string("KAGENT_URL", "http://kagent-controller.kagent:8083"),
            kagent_namespace: env.string("KAGENT_NAMESPACE", "kagent"),
            kagent_agent: kagent_agent.clone(),
            kagent_agent_routing_label: env.string("KAGENT_AGENT_ROUTING_LABEL", &channel_label),
            kagent_agent_routing_map: env.pairs("KAGENT_AGENT_ROUTING_MAP")?,
            kagent_user_id: kagent_user_id.clone(),
            kagent_timeout: env.duration(
                "KAGENT_TIMEOUT",
                Duration::from_secs(120),
                Duration::from_secs(1),
            )?,
            kagent_request_timeout: env.duration(
                "KAGENT_REQUEST_TIMEOUT",
                Duration::from_secs(30),
                Duration::from_secs(1),
            )?,
            kagent_poll_interval: env.duration(
                "KAGENT_POLL_INTERVAL",
                Duration::from_secs(5),
                Duration::from_secs(1),
            )?,
            analyze_severities: env.set("ANALYZE_SEVERITIES", "critical"),
            analyze_label: env.string("ANALYZE_LABEL", "analyze"),
            analyze_resolved: env.bool("ANALYZE_RESOLVED", false)?,
            dedupe_ttl: env.duration("DEDUPE_TTL", Duration::from_hours(12), Duration::ZERO)?,
            max_alerts_in_prompt: env.int("MAX_ALERTS_IN_PROMPT", 5, 1, 100)?,
            max_concurrent: env.int("MAX_CONCURRENT_ANALYSES", 2, 1, 64)?,
            instructions: env.string("ANALYSIS_INSTRUCTIONS", DEFAULT_INSTRUCTIONS),
            slack_app_token: env.raw("SLACK_APP_TOKEN"),
            // The chat agent defaults to the alert agent, so enabling
            // mentions needs no second agent name in the common deployment.
            chat_agent: env.string("CHAT_AGENT", &kagent_agent),
            chat_agent_map: env.pairs("CHAT_AGENT_MAP")?,
            chat_channels: env.list("CHAT_CHANNELS"),
            chat_allowed_users: env.set("CHAT_ALLOWED_USERS", ""),
            chat_instructions: env.string("CHAT_INSTRUCTIONS", DEFAULT_CHAT_INSTRUCTIONS),
            chat_user_id: env.string("CHAT_USER_ID", &kagent_user_id),
            chat_timeout: env.duration(
                "CHAT_TIMEOUT",
                Duration::from_secs(180),
                Duration::from_secs(1),
            )?,
            chat_session_ttl: env.duration(
                "CHAT_SESSION_TTL",
                Duration::from_hours(2),
                Duration::ZERO,
            )?,
            // Slack rate limits chat.update, and a status line rewritten more
            // often than every second buys the reader nothing.
            chat_status_interval: env.duration(
                "CHAT_STATUS_INTERVAL",
                Duration::from_secs(10),
                Duration::from_secs(1),
            )?,
            // An explicitly empty hint turns that hint off and restores the
            // silent drop for that case alone.
            chat_thread_hint: env.optional("CHAT_THREAD_HINT", DEFAULT_THREAD_HINT, |v| {
                v.trim().to_string()
            }),
            chat_denied_hint: env.optional("CHAT_DENIED_HINT", DEFAULT_DENIED_HINT, |v| {
                v.trim().to_string()
            }),
            max_concurrent_chats: env.int("MAX_CONCURRENT_CHATS", 2, 1, 64)?,
            webhook_path,
            webhook_token: env.raw("WEBHOOK_BEARER_TOKEN"),
            listen_port,
            metrics_port,
            log_level: env.string("LOG_LEVEL", "info"),
            log_format: env.string("LOG_FORMAT", "json"),
        };
        Ok(cfg)
    }

    /// Reports whether mention invocation is configured. Without an app-level
    /// token there is no Socket Mode connection to receive a mention on, so
    /// every other chat setting is inert.
    #[must_use]
    pub const fn chat_enabled(&self) -> bool {
        !self.slack_app_token.is_empty()
    }

    /// Every agent the gateway may route to, the default first and the mapped
    /// ones sorted after it. It exists for startup logging: the agent a given
    /// alert reaches is otherwise only visible once one fires.
    #[must_use]
    pub fn agents(&self) -> Vec<String> {
        let mut agents = vec![self.kagent_agent.clone()];
        let mut seen: HashSet<&str> = HashSet::from([self.kagent_agent.as_str()]);
        let mut tables = vec![&self.kagent_agent_routing_map];
        if self.chat_enabled() {
            if seen.insert(&self.chat_agent) {
                agents.push(self.chat_agent.clone());
            }
            tables.push(&self.chat_agent_map);
        }
        for table in tables {
            for agent in table.values() {
                if seen.insert(agent) {
                    agents.push(agent.clone());
                }
            }
        }
        agents[1..].sort();
        agents
    }
}

fn trim_colons(value: &str) -> String {
    value.trim().trim_matches(':').to_string()
}

/// Typed accessors over one environment lookup function.
struct Env<'a>(&'a dyn Fn(&str) -> Option<String>);

impl Env<'_> {
    fn raw(&self, key: &str) -> String {
        (self.0)(key).unwrap_or_default()
    }

    fn string(&self, key: &str, fallback: &str) -> String {
        match (self.0)(key) {
            Some(v) if !v.is_empty() => v,
            _ => fallback.to_string(),
        }
    }

    /// Keeps an explicitly empty value meaningful: it is passed through
    /// `map`, while an unset variable keeps the fallback.
    fn optional(&self, key: &str, fallback: &str, map: impl Fn(&str) -> String) -> String {
        (self.0)(key).map_or_else(|| fallback.to_string(), |v| map(&v))
    }

    fn int<T>(&self, key: &str, fallback: T, min: T, max: T) -> Result<T>
    where
        T: std::str::FromStr + PartialOrd + std::fmt::Display + Copy,
    {
        let Some(v) = (self.0)(key).filter(|v| !v.is_empty()) else {
            return Ok(fallback);
        };
        v.trim()
            .parse::<T>()
            .ok()
            .filter(|n| *n >= min && *n <= max)
            .ok_or_else(|| {
                anyhow!("invalid {key} {v:?}: must be an integer between {min} and {max}")
            })
    }

    fn bool(&self, key: &str, fallback: bool) -> Result<bool> {
        let Some(v) = (self.0)(key).filter(|v| !v.is_empty()) else {
            return Ok(fallback);
        };
        match v.trim() {
            "1" | "t" | "T" | "true" | "TRUE" | "True" => Ok(true),
            "0" | "f" | "F" | "false" | "FALSE" | "False" => Ok(false),
            _ => bail!("invalid {key} {v:?}: must be a boolean"),
        }
    }

    fn duration(&self, key: &str, fallback: Duration, min: Duration) -> Result<Duration> {
        let Some(v) = (self.0)(key).filter(|v| !v.is_empty()) else {
            return Ok(fallback);
        };
        let d = parse_duration(&v).with_context(|| format!("invalid {key} {v:?}"))?;
        if d < min {
            bail!(
                "{key} must be at least {}, got {}",
                format_duration(min),
                format_duration(d)
            );
        }
        Ok(d)
    }

    /// Parses a comma-separated list into a lookup set. An explicitly empty
    /// value disables the filter, which callers read as "match everything".
    fn set(&self, key: &str, fallback: &str) -> HashSet<String> {
        let v = (self.0)(key).unwrap_or_else(|| fallback.to_string());
        split_list(&v).collect()
    }

    /// Parses a comma-separated list into a vector, keeping the order and the
    /// original spelling. Used where the entries are not compared literally:
    /// a channel entry may be a name or an ID and has to be resolved first.
    fn list(&self, key: &str) -> Vec<String> {
        split_list(&self.raw(key)).collect()
    }

    /// Parses a `key=value,key=value` list into a map.
    fn pairs(&self, key: &str) -> Result<HashMap<String, String>> {
        let mut pairs = HashMap::new();
        for item in split_list(&self.raw(key)) {
            let Some((k, v)) = item.split_once('=') else {
                bail!("invalid {key} entry {item:?}: expected key=value");
            };
            let (k, v) = (k.trim(), v.trim());
            if k.is_empty() || v.is_empty() {
                bail!("invalid {key} entry {item:?}: expected key=value");
            }
            pairs.insert(k.to_string(), v.to_string());
        }
        Ok(pairs)
    }
}

fn split_list(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parses a duration the way Go's `time.ParseDuration` does: a sequence of
/// decimal numbers each with a unit suffix, such as `300s`, `1m30s`, or
/// `1.5h`. The chart and the documentation spell durations this way, so the
/// port keeps the format rather than asking every deployment to change it.
///
/// # Errors
///
/// Returns an error for an empty string, an unknown unit, a negative value, or
/// a number without a unit.
pub fn parse_duration(value: &str) -> Result<Duration> {
    let s = value.trim();
    if s.is_empty() {
        bail!("empty duration");
    }
    if s == "0" {
        return Ok(Duration::ZERO);
    }
    let mut rest = s;
    let mut total = 0.0_f64;
    while !rest.is_empty() {
        let num_end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .ok_or_else(|| anyhow!("missing unit in duration {value:?}"))?;
        if num_end == 0 {
            bail!("invalid duration {value:?}");
        }
        let number: f64 = rest[..num_end]
            .parse()
            .map_err(|_| anyhow!("invalid duration {value:?}"))?;
        rest = &rest[num_end..];
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let (unit, tail) = rest.split_at(unit_end);
        let scale = match unit {
            "ns" => 1e-9,
            "us" | "µs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => bail!("unknown unit {unit:?} in duration {value:?}"),
        };
        total = number.mul_add(scale, total);
        rest = tail;
    }
    Ok(Duration::from_secs_f64(total))
}

/// Renders a duration in the compact form Go prints (`2m30s`, `1h`, `500ms`),
/// which is what the startup log and the in-thread notices quote back.
#[must_use]
pub fn format_duration(d: Duration) -> String {
    if d.is_zero() {
        return "0s".to_string();
    }
    if d < Duration::from_secs(1) {
        return format!("{}ms", d.as_millis());
    }
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    let mut out = String::new();
    if h > 0 {
        let _ = write!(out, "{h}h");
    }
    if m > 0 {
        let _ = write!(out, "{m}m");
    }
    if s > 0 || out.is_empty() {
        let _ = write!(out, "{s}s");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    fn base() -> Vec<(&'static str, &'static str)> {
        vec![
            ("SLACK_BOT_TOKEN", "xoxb-test"),
            ("SLACK_DEFAULT_CHANNEL", "alerts"),
        ]
    }

    #[test]
    fn chat_user_id_follows_the_alert_identity_until_set() {
        let mut env = base();
        env.push(("KAGENT_USER_ID", "bot@kagent.dev"));
        let cfg = Config::from_env(env_from(&env)).unwrap();
        assert_eq!(cfg.kagent_user_id, "bot@kagent.dev");
        assert_eq!(cfg.chat_user_id, "bot@kagent.dev");

        env.push(("CHAT_USER_ID", "chat@kagent.dev"));
        let cfg = Config::from_env(env_from(&env)).unwrap();
        assert_eq!(cfg.kagent_user_id, "bot@kagent.dev");
        assert_eq!(cfg.chat_user_id, "chat@kagent.dev");
    }

    #[test]
    fn defaults_apply() {
        let cfg = Config::from_env(env_from(&base())).unwrap();
        assert_eq!(cfg.slack_api_url, "https://slack.com/api");
        assert_eq!(cfg.parent_mode, ParentMode::Lookup);
        assert_eq!(cfg.kagent_agent, "alert-triage-agent");
        assert_eq!(cfg.chat_agent, "alert-triage-agent");
        assert_eq!(cfg.chat_user_id, "gateway@kagent.dev");
        assert_eq!(cfg.kagent_agent_routing_label, "slack_channel");
        assert_eq!(cfg.kagent_timeout, Duration::from_secs(120));
        assert_eq!(cfg.dedupe_ttl, Duration::from_hours(12));
        assert_eq!(cfg.max_concurrent, 2);
        assert_eq!(cfg.slack_max_text_chars, 8000);
        assert_eq!(cfg.investigating_reaction, "telescope");
        assert_eq!(cfg.completed_reaction, "white_check_mark");
        assert_eq!(cfg.chat_thread_hint, DEFAULT_THREAD_HINT);
        assert!(cfg.analyze_severities.contains("critical"));
        assert_eq!(cfg.analyze_severities.len(), 1);
        assert!(!cfg.analyze_resolved);
        assert!(!cfg.chat_enabled());
        assert_eq!(cfg.listen_port, 8080);
        assert_eq!(cfg.metrics_port, 8081);
        assert_eq!(cfg.instructions, DEFAULT_INSTRUCTIONS);
        assert_eq!(cfg.webhook_path, "/alert");
    }

    #[test]
    fn required_variables() {
        let err = Config::from_env(env_from(&[("SLACK_DEFAULT_CHANNEL", "x")])).unwrap_err();
        assert!(err.to_string().contains("SLACK_BOT_TOKEN"));
        let err = Config::from_env(env_from(&[("SLACK_BOT_TOKEN", "x")])).unwrap_err();
        assert!(err.to_string().contains("SLACK_DEFAULT_CHANNEL"));
    }

    #[test]
    fn overrides_and_lists() {
        let mut env = base();
        env.extend([
            ("SLACK_PARENT_MODE", "post"),
            ("SLACK_CHANNEL_MAP", "infra=infra-alerts, sec = security "),
            ("KAGENT_AGENT_ROUTING_MAP", "infra=infra-agent"),
            ("ANALYZE_SEVERITIES", "critical, warning"),
            ("ANALYZE_RESOLVED", "true"),
            ("KAGENT_TIMEOUT", "5m"),
            ("DEDUPE_TTL", "0s"),
            ("MAX_CONCURRENT_ANALYSES", "4"),
            ("SLACK_INVESTIGATING_REACTION", ":eyes:"),
            ("SLACK_COMPLETED_REACTION", ""),
            ("CHAT_THREAD_HINT", ""),
            ("SLACK_APP_TOKEN", "xapp-1"),
            ("CHAT_AGENT", "chat-agent"),
            ("CHAT_CHANNELS", "C1, dev-chat"),
            ("CHAT_ALLOWED_USERS", "U1,U2"),
            ("CHAT_AGENT_MAP", "C1=c1-agent"),
            ("WEBHOOK_BEARER_TOKEN", "secret"),
            ("LISTEN_PORT", "9000"),
        ]);
        let cfg = Config::from_env(env_from(&env)).unwrap();
        assert_eq!(cfg.parent_mode, ParentMode::Post);
        assert_eq!(cfg.slack_channel_map["infra"], "infra-alerts");
        assert_eq!(cfg.slack_channel_map["sec"], "security");
        assert_eq!(cfg.kagent_agent_routing_map["infra"], "infra-agent");
        assert_eq!(cfg.analyze_severities.len(), 2);
        assert!(cfg.analyze_resolved);
        assert_eq!(cfg.kagent_timeout, Duration::from_secs(300));
        assert_eq!(cfg.dedupe_ttl, Duration::ZERO);
        assert_eq!(cfg.max_concurrent, 4);
        assert_eq!(cfg.investigating_reaction, "eyes");
        assert_eq!(cfg.completed_reaction, "");
        assert_eq!(cfg.chat_thread_hint, "");
        assert_eq!(cfg.chat_denied_hint, DEFAULT_DENIED_HINT);
        assert!(cfg.chat_enabled());
        assert_eq!(cfg.chat_channels, vec!["C1", "dev-chat"]);
        assert_eq!(cfg.chat_allowed_users.len(), 2);
        assert_eq!(cfg.webhook_token, "secret");
        assert_eq!(cfg.listen_port, 9000);
        assert_eq!(
            cfg.agents(),
            vec![
                "alert-triage-agent",
                "c1-agent",
                "chat-agent",
                "infra-agent"
            ]
        );
    }

    #[test]
    fn agents_without_chat_ignores_chat_tables() {
        let mut env = base();
        env.extend([
            ("CHAT_AGENT", "chat-agent"),
            ("KAGENT_AGENT_ROUTING_MAP", "b=zeta,a=alpha"),
        ]);
        let cfg = Config::from_env(env_from(&env)).unwrap();
        assert_eq!(cfg.agents(), vec!["alert-triage-agent", "alpha", "zeta"]);
    }

    #[test]
    fn empty_severity_filter_means_everything() {
        let mut env = base();
        env.push(("ANALYZE_SEVERITIES", ""));
        let cfg = Config::from_env(env_from(&env)).unwrap();
        assert!(cfg.analyze_severities.is_empty());
    }

    #[test]
    fn rejects_invalid_values() {
        let cases: Vec<(&str, &str, &str)> = vec![
            ("SLACK_PARENT_MODE", "thread", "SLACK_PARENT_MODE"),
            ("WEBHOOK_PATH", "alert", "WEBHOOK_PATH"),
            ("MAX_CONCURRENT_ANALYSES", "0", "MAX_CONCURRENT_ANALYSES"),
            ("MAX_CONCURRENT_ANALYSES", "abc", "MAX_CONCURRENT_ANALYSES"),
            ("KAGENT_TIMEOUT", "500ms", "at least"),
            ("KAGENT_TIMEOUT", "soon", "KAGENT_TIMEOUT"),
            ("ANALYZE_RESOLVED", "maybe", "ANALYZE_RESOLVED"),
            ("SLACK_CHANNEL_MAP", "novalue", "SLACK_CHANNEL_MAP"),
            ("SLACK_CHANNEL_MAP", "=x", "SLACK_CHANNEL_MAP"),
            ("METRICS_PORT", "8080", "must differ"),
            ("SLACK_MAX_TEXT", "10", "SLACK_MAX_TEXT"),
        ];
        for (key, value, needle) in cases {
            let mut env = base();
            env.push((key, value));
            let err = Config::from_env(env_from(&env)).unwrap_err();
            assert!(err.to_string().contains(needle), "{key}={value}: {err}");
        }
    }

    #[test]
    fn go_durations_parse() {
        assert_eq!(parse_duration("300s").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1m30s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("1.5h").unwrap(), Duration::from_mins(90));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("0s").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("10").is_err());
        assert!(parse_duration("10d").is_err());
        assert!(parse_duration("s").is_err());
        assert!(parse_duration("-5s").is_err());
    }

    #[test]
    fn durations_format_compactly() {
        assert_eq!(format_duration(Duration::ZERO), "0s");
        assert_eq!(format_duration(Duration::from_millis(500)), "500ms");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m30s");
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1h2m5s");
        assert_eq!(format_duration(Duration::from_secs(120)), "2m");
    }

    #[test]
    fn parent_mode_round_trips() {
        assert_eq!(ParentMode::Lookup.as_str(), "lookup");
        assert_eq!(ParentMode::Post.as_str(), "post");
    }
}
