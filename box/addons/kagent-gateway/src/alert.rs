//! Alertmanager webhook payloads and their rendering for Slack and for the
//! analysis agent.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Deserialize;

/// The body Alertmanager POSTs to a webhook receiver.
/// Ref: <https://prometheus.io/docs/alerting/latest/configuration/#webhook_config>
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Payload {
    pub version: String,
    pub group_key: String,
    pub truncated_alerts: usize,
    pub status: String,
    pub receiver: String,
    pub group_labels: BTreeMap<String, String>,
    pub common_labels: BTreeMap<String, String>,
    pub common_annotations: BTreeMap<String, String>,
    #[serde(rename = "externalURL")]
    pub external_url: String,
    pub alerts: Vec<Alert>,
}

/// A single alert instance inside a [`Payload`]. Timestamps stay as the
/// RFC 3339 strings Alertmanager sends: the prompt quotes them verbatim, and
/// nothing else reads them.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Alert {
    pub status: String,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    pub starts_at: String,
    pub ends_at: String,
    #[serde(rename = "generatorURL")]
    pub generator_url: String,
    pub fingerprint: String,
}

/// Alertmanager's zero time, which it sends for an alert that has not ended.
const ZERO_TIME: &str = "0001-01-01T00:00:00Z";

impl Payload {
    /// Reports whether the whole group has stopped firing.
    #[must_use]
    pub fn resolved(&self) -> bool {
        self.status == "resolved"
    }

    /// The alertname, falling back to the receiver so a message is never
    /// titled with an empty string.
    #[must_use]
    pub fn name(&self) -> &str {
        if let Some(v) = self.label("alertname") {
            return v;
        }
        // Falco alerts group by rule, not alertname.
        if let Some(v) = self.label("rule") {
            return v;
        }
        if !self.receiver.is_empty() {
            return &self.receiver;
        }
        "unknown"
    }

    /// The common severity label, or `unknown` when the group mixes
    /// severities and Alertmanager therefore drops the label.
    #[must_use]
    pub fn severity(&self) -> &str {
        self.label("severity").unwrap_or("unknown")
    }

    /// The cluster label shared by the group.
    #[must_use]
    pub fn cluster(&self) -> &str {
        self.label("cluster").unwrap_or("")
    }

    /// Identifies the alert group across repeated notifications. Alertmanager
    /// reuses `groupKey` for the lifetime of a group, so it is stable across
    /// `repeat_interval` resends while still separating distinct groups.
    #[must_use]
    pub fn dedupe_key(&self) -> String {
        if !self.group_key.is_empty() {
            return self.group_key.clone();
        }
        if let Some(fp) = self
            .alerts
            .first()
            .map(|a| a.fingerprint.as_str())
            .filter(|fp| !fp.is_empty())
        {
            return fp.to_string();
        }
        format!("{}/{}", self.receiver, self.name())
    }

    /// The token an Alertmanager Slack template renders so the gateway can
    /// recognise its own alert in channel history. The first alert's
    /// fingerprint is short, opaque, and reproducible from the same
    /// notification, which is what a join key across two independent
    /// Alertmanager deliveries has to be.
    #[must_use]
    pub fn marker(&self) -> &str {
        match self.alerts.first() {
            Some(a) if !a.fingerprint.is_empty() => &a.fingerprint,
            _ => &self.group_key,
        }
    }

    /// Reads a label from the group, preferring the labels every alert in it
    /// shares over the ones it was grouped by.
    #[must_use]
    pub fn label(&self, key: &str) -> Option<&str> {
        self.common_labels
            .get(key)
            .or_else(|| self.group_labels.get(key))
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    /// The Slack message title, using the same emoji and status wording the
    /// Alertmanager `slack_configs` templates used before the gateway.
    #[must_use]
    pub fn title(&self) -> String {
        if self.resolved() {
            format!("✅ [RESOLVED] {}", self.name())
        } else if self.severity() == "info" {
            format!("ℹ️ [NOTED] {}", self.name())
        } else {
            format!("🚨 [FIRING] {}", self.name())
        }
    }

    /// The Slack attachment colour for the group.
    #[must_use]
    pub fn color(&self) -> &'static str {
        if self.resolved() {
            return "good";
        }
        match self.severity() {
            "critical" => "danger",
            "warning" => "warning",
            _ => "#439FE0",
        }
    }

    /// The alert body posted as the Slack parent message. At most `max_alerts`
    /// entries are rendered; the rest are summarised in a trailing line so a
    /// large group never produces an unreadable wall of text.
    #[must_use]
    pub fn slack_text(&self, max_alerts: usize) -> String {
        let mut b = String::new();
        for (i, a) in self.limit(max_alerts).iter().enumerate() {
            if i > 0 {
                b.push('\n');
            }
            write_line(&mut b, "Severity", a.labels.get("severity"));
            write_line(&mut b, "Summary", a.annotations.get("summary"));
            write_line(&mut b, "Environment", a.labels.get("cluster"));
            write_line(&mut b, "Description", a.annotations.get("description"));
        }
        let n = self.omitted(max_alerts);
        if n > 0 {
            let _ = write!(b, "\n_and {n} more alert(s) in this group_");
        }
        b.trim().to_string()
    }

    /// The analysis request sent to the agent. The alert is serialised as
    /// plain text rather than JSON because the agent reasons over it directly
    /// and the label set is small.
    #[must_use]
    pub fn prompt(&self, instructions: &str, max_alerts: usize) -> String {
        let mut b =
            String::from("An alert group fired in the monitoring stack. Investigate it.\n\n");
        b.push_str("## Alert group\n");
        write_field(&mut b, "status", &self.status);
        write_field(&mut b, "receiver", &self.receiver);
        write_field(&mut b, "groupKey", &self.group_key);
        write_field(&mut b, "cluster", self.cluster());
        write_field(&mut b, "alertname", self.name());
        write_field(&mut b, "externalURL", &self.external_url);
        write_field(&mut b, "alertCount", &self.alerts.len().to_string());

        for (i, a) in self.limit(max_alerts).iter().enumerate() {
            let _ = write!(b, "\n## Alert {}\n", i + 1);
            write_field(&mut b, "status", &a.status);
            write_field(&mut b, "fingerprint", &a.fingerprint);
            if !a.starts_at.is_empty() && a.starts_at != ZERO_TIME {
                write_field(&mut b, "startsAt", &a.starts_at);
            }
            if !a.ends_at.is_empty() && a.ends_at != ZERO_TIME && a.ends_at > a.starts_at {
                write_field(&mut b, "endsAt", &a.ends_at);
            }
            write_field(&mut b, "generatorURL", &a.generator_url);
            write_map(&mut b, "labels", &a.labels);
            write_map(&mut b, "annotations", &a.annotations);
        }
        let n = self.omitted(max_alerts);
        if n > 0 {
            let _ = write!(
                b,
                "\n{n} further alert(s) in this group were omitted from this prompt.\n"
            );
        }

        b.push_str("\n## Instructions\n");
        b.push_str(instructions);
        b.push('\n');
        b
    }

    fn limit(&self, max_alerts: usize) -> &[Alert] {
        if max_alerts > 0 && self.alerts.len() > max_alerts {
            &self.alerts[..max_alerts]
        } else {
            &self.alerts
        }
    }

    fn omitted(&self, max_alerts: usize) -> usize {
        self.alerts.len() - self.limit(max_alerts).len() + self.truncated_alerts
    }
}

fn write_line(b: &mut String, key: &str, value: Option<&String>) {
    if let Some(v) = value.filter(|v| !v.is_empty()) {
        let _ = writeln!(b, "*{key}:* {v}");
    }
}

fn write_field(b: &mut String, key: &str, value: &str) {
    if !value.is_empty() {
        let _ = writeln!(b, "{key}: {value}");
    }
}

/// Renders a label or annotation set. `BTreeMap` keeps the keys sorted so the
/// same alert always produces the same prompt, which keeps agent replies
/// comparable.
fn write_map(b: &mut String, name: &str, m: &BTreeMap<String, String>) {
    if m.is_empty() {
        return;
    }
    let _ = writeln!(b, "{name}:");
    for (k, v) in m {
        let _ = writeln!(b, "  {k}: {v}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn payload() -> Payload {
        Payload {
            group_key: "gk-1".into(),
            status: "firing".into(),
            receiver: "kagent-gateway".into(),
            common_labels: labels(&[
                ("alertname", "KubePodCrashLooping"),
                ("severity", "critical"),
                ("cluster", "prd"),
            ]),
            external_url: "http://am".into(),
            alerts: vec![
                Alert {
                    status: "firing".into(),
                    labels: labels(&[
                        ("alertname", "KubePodCrashLooping"),
                        ("severity", "critical"),
                        ("pod", "api-0"),
                    ]),
                    annotations: labels(&[
                        ("summary", "pod restarting"),
                        ("description", "7 restarts"),
                    ]),
                    starts_at: "2026-01-01T00:00:00Z".into(),
                    ends_at: ZERO_TIME.into(),
                    generator_url: "http://prom".into(),
                    fingerprint: "fp-1".into(),
                },
                Alert {
                    fingerprint: "fp-2".into(),
                    ..Alert::default()
                },
            ],
            ..Payload::default()
        }
    }

    #[test]
    fn decodes_alertmanager_payload() {
        let raw = r#"{"groupKey":"g","status":"firing","receiver":"r","commonLabels":{"alertname":"A"},
            "externalURL":"http://am","alerts":[{"status":"firing","fingerprint":"f","labels":{"a":"b"},
            "annotations":{},"startsAt":"2026-01-01T00:00:00Z","endsAt":"0001-01-01T00:00:00Z","generatorURL":"http://p"}]}"#;
        let p: Payload = serde_json::from_str(raw).unwrap();
        assert_eq!(p.name(), "A");
        assert_eq!(p.external_url, "http://am");
        assert_eq!(p.alerts[0].generator_url, "http://p");
        assert_eq!(p.marker(), "f");
    }

    #[test]
    fn name_falls_back() {
        let mut p = payload();
        assert_eq!(p.name(), "KubePodCrashLooping");
        p.common_labels.clear();
        p.group_labels = labels(&[("rule", "Falco Rule")]);
        assert_eq!(p.name(), "Falco Rule");
        p.group_labels.clear();
        assert_eq!(p.name(), "kagent-gateway");
        p.receiver.clear();
        assert_eq!(p.name(), "unknown");
    }

    #[test]
    fn severity_cluster_and_keys() {
        let mut p = payload();
        assert_eq!(p.severity(), "critical");
        assert_eq!(p.cluster(), "prd");
        assert_eq!(p.dedupe_key(), "gk-1");
        assert_eq!(p.marker(), "fp-1");
        p.group_key.clear();
        assert_eq!(p.dedupe_key(), "fp-1");
        p.alerts[0].fingerprint.clear();
        assert_eq!(p.marker(), "");
        p.alerts.clear();
        assert_eq!(p.dedupe_key(), "kagent-gateway/KubePodCrashLooping");
        p.common_labels.remove("severity");
        assert_eq!(p.severity(), "unknown");
    }

    #[test]
    fn title_and_color_follow_status() {
        let mut p = payload();
        assert_eq!(p.title(), "🚨 [FIRING] KubePodCrashLooping");
        assert_eq!(p.color(), "danger");
        p.common_labels.insert("severity".into(), "warning".into());
        assert_eq!(p.color(), "warning");
        p.common_labels.insert("severity".into(), "info".into());
        assert_eq!(p.title(), "ℹ️ [NOTED] KubePodCrashLooping");
        assert_eq!(p.color(), "#439FE0");
        p.status = "resolved".into();
        assert!(p.resolved());
        assert_eq!(p.title(), "✅ [RESOLVED] KubePodCrashLooping");
        assert_eq!(p.color(), "good");
    }

    #[test]
    fn slack_text_limits_alerts() {
        let p = payload();
        let text = p.slack_text(1);
        assert!(text.contains("*Severity:* critical"));
        assert!(text.contains("*Summary:* pod restarting"));
        assert!(text.contains("*Description:* 7 restarts"));
        assert!(text.ends_with("_and 1 more alert(s) in this group_"));
        let all = p.slack_text(0);
        assert!(!all.contains("more alert"));
    }

    #[test]
    fn prompt_renders_sorted_fields() {
        let mut p = payload();
        p.truncated_alerts = 2;
        let prompt = p.prompt("Do the thing.", 1);
        assert!(prompt.starts_with("An alert group fired"));
        assert!(prompt.contains("groupKey: gk-1"));
        assert!(prompt.contains("alertCount: 2"));
        assert!(prompt.contains("## Alert 1\n"));
        assert!(!prompt.contains("## Alert 2"));
        assert!(prompt.contains("startsAt: 2026-01-01T00:00:00Z"));
        assert!(!prompt.contains("endsAt"));
        assert!(prompt.contains(
            "labels:\n  alertname: KubePodCrashLooping\n  pod: api-0\n  severity: critical\n"
        ));
        assert!(prompt.contains("3 further alert(s) in this group were omitted"));
        assert!(prompt.ends_with("## Instructions\nDo the thing.\n"));
    }

    #[test]
    fn prompt_includes_ends_at_when_later() {
        let mut p = payload();
        p.alerts[0].ends_at = "2026-01-01T01:00:00Z".into();
        let prompt = p.prompt("", 5);
        assert!(prompt.contains("endsAt: 2026-01-01T01:00:00Z"));
    }
}
