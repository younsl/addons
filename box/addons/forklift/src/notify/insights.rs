//! Security insight alarms: the score, the finding mix and the checks that
//! need action, rendered for Slack/Mattermost.

use serde::{Deserialize, Serialize};

use super::{AlarmField, Notifier, Result, render_alarm, timestamp};

/// Caps the findings listed in an alarm, the same reason as the coverage list:
/// past this a reader is being handed the page, not a summary.
pub(crate) const MAX_LISTED_FINDINGS: usize = 20;

/// One finding named in an insight alarm.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InsightsFindingLine {
    pub id: String,
    pub title: String,
    pub severity: String,
    pub flagged: i64,
}

/// The input to an insight alarm. A plain struct rather than the insights
/// module's report, so notify does not depend on the analyzer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InsightsReport {
    pub score: i64,
    pub grade: String,
    pub critical: i64,
    pub high: i64,
    pub medium: i64,
    pub low: i64,
    pub info: i64,
    pub checks_passed: i64,
    pub checks_total: i64,
    /// Worst first, as the report orders them.
    pub findings: Vec<InsightsFindingLine>,
}

/// The body posted for an insight alarm. `text` is what Slack/Mattermost
/// render; the structured fields let other consumers route on the numbers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InsightsPayload {
    pub text: String,
    pub event: String,
    pub score: i64,
    pub grade: String,
    pub critical: i64,
    pub high: i64,
    pub medium: i64,
    pub low: i64,
    pub info: i64,
    /// The ids of the listed findings, capped the same as the text.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<String>,
    pub timestamp: String,
}

impl Notifier {
    /// Links a label to an insights page (`path` under /workspace/insights), or
    /// returns it as plain text without an external URL.
    fn insights_link(&self, label: &str, path: &str) -> String {
        let ext = self.external_url();
        if ext.is_empty() {
            return label.to_string();
        }
        format!("<{ext}/workspace/insights{path}|{label}>")
    }

    /// Renders an insight alarm.
    pub fn build_insights_report(&self, r: &InsightsReport) -> InsightsPayload {
        let title = self.insights_link("Forklift security insights", "");
        let subtitle = format!(
            "{}\nScore {} / 100 ({}), {} of {} checks passed.",
            describe(r),
            r.score,
            r.grade,
            r.checks_passed,
            r.checks_total
        );
        let fields = [AlarmField::new(
            "By severity",
            format!(
                "critical {}, high {}, medium {}, low {}, info {}",
                r.critical, r.high, r.medium, r.low, r.info
            ),
        )];
        let mut text = render_alarm(&title, &subtitle, &fields);
        let shown = &r.findings[..r.findings.len().min(MAX_LISTED_FINDINGS)];
        if !shown.is_empty() {
            let mut lines = vec![String::new(), "*Findings*".to_string()];
            for (i, f) in shown.iter().enumerate() {
                lines.push(format!(
                    "{}. {} ({}, {} flagged)",
                    i + 1,
                    self.insights_link(&f.title, &format!("/{}", f.id)),
                    f.severity,
                    f.flagged
                ));
            }
            let remaining = r.findings.len() - shown.len();
            if remaining > 0 {
                lines.push(format!(
                    "_{remaining} more, {}_",
                    self.insights_link("see the insights page", "")
                ));
            }
            text.push('\n');
            text.push_str(&lines.join("\n"));
        }
        InsightsPayload {
            text,
            event: "insights.report".to_string(),
            score: r.score,
            grade: r.grade.clone(),
            critical: r.critical,
            high: r.high,
            medium: r.medium,
            low: r.low,
            info: r.info,
            findings: shown.iter().map(|f| f.id.clone()).collect(),
            timestamp: timestamp(),
        }
    }

    /// Delivers an insight payload to one webhook synchronously, so a manual
    /// send can report each receiver's outcome.
    pub async fn send_insights_report(&self, url: &str, p: &InsightsPayload) -> Result<()> {
        self.send_json(url, super::rich::webhook_body(p)?).await
    }
}

/// Says in words, without figures, what Forklift is, what the alarm is and what
/// state it reports, so a reader who has never used Forklift knows why it arrived.
fn describe(r: &InsightsReport) -> String {
    let what = "Forklift is the artifact registry that stores and proxies the packages and container images builds depend on. This is its security insights alarm: Forklift regularly reviews its repositories, packages, images, user accounts and access tokens for security risks, and this message sums up the latest review.";
    let state = if r.critical > 0 {
        "Some checks found critical risks, so fix those first. The findings below link to each check with why it matters and what to do."
    } else if r.high > 0 {
        "Nothing critical was found, but some checks found high risks worth fixing soon. The findings below link to each check with why it matters and what to do."
    } else if r.medium + r.low + r.info > 0 {
        "Nothing critical or high was found, so the findings below can go into regular planning. Each links to its check with why it matters and what to do."
    } else {
        "Every check passed, so nothing needs attention right now."
    };
    format!("{what} {state}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::tests::webhook::install_crypto;

    fn report(findings: usize) -> InsightsReport {
        InsightsReport {
            score: 45,
            grade: "D".to_string(),
            high: 3,
            checks_passed: 13,
            checks_total: 22,
            findings: (0..findings)
                .map(|i| InsightsFindingLine {
                    id: format!("rule-{i}"),
                    title: format!("Rule {i}"),
                    severity: "high".to_string(),
                    flagged: 2,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn describe_names_the_alarm_and_the_state_without_figures() {
        let state = |r: InsightsReport| describe(&r);
        let critical = state(InsightsReport {
            critical: 1,
            high: 2,
            ..Default::default()
        });
        assert!(
            critical.starts_with("Forklift is the artifact registry"),
            "{critical}"
        );
        assert!(
            critical.contains("critical risks, so fix those first"),
            "{critical}"
        );
        assert!(
            !critical.chars().any(|c| c.is_ascii_digit()),
            "no figures: {critical}"
        );
        assert!(
            state(InsightsReport {
                high: 2,
                medium: 3,
                ..Default::default()
            })
            .contains("Nothing critical was found, but some checks found high risks")
        );
        assert!(
            state(InsightsReport {
                low: 1,
                ..Default::default()
            })
            .contains("Nothing critical or high was found")
        );
        assert!(
            state(InsightsReport {
                checks_total: 25,
                ..Default::default()
            })
            .ends_with("Every check passed, so nothing needs attention right now.")
        );
    }

    #[test]
    fn insights_alarm_lists_findings_and_caps() {
        install_crypto();
        let n = Notifier::new(std::time::Duration::from_secs(2));
        n.set_external_url("https://forklift.example.com/");
        let p = n.build_insights_report(&report(MAX_LISTED_FINDINGS + 2));
        assert_eq!(p.event, "insights.report");
        assert_eq!(p.findings.len(), MAX_LISTED_FINDINGS);
        assert!(
            p.text.contains(
                "worth fixing soon. The findings below link to each check with why it matters and what to do.\nScore 45 / 100 (D), 13 of 22 checks passed."
            ),
            "{}",
            p.text
        );
        assert!(
            p.text.contains(
                "<https://forklift.example.com/workspace/insights/rule-0|Rule 0> (high, 2 flagged)"
            ),
            "{}",
            p.text
        );
        assert!(p.text.contains(
            "_2 more, <https://forklift.example.com/workspace/insights|see the insights page>_"
        ));

        let plain =
            Notifier::new(std::time::Duration::from_secs(2)).build_insights_report(&report(0));
        assert!(
            plain.text.starts_with("*Forklift security insights*"),
            "{}",
            plain.text
        );
        assert!(
            !plain.text.contains("*Findings*\n"),
            "no list without findings"
        );
    }
}
