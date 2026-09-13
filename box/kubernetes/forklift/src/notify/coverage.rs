//! Coverage report alarms: the scheduled scan's headline numbers plus the
//! projects still to migrate, rendered for Slack/Mattermost.

use std::sync::Arc;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use super::{AlarmField, Notifier, Result, Target, render_alarm, timestamp};

/// Caps the project list in a coverage report. Slack and Mattermost both reject
/// very long messages, and a reader scrolling past fifty entries is being
/// handed the page, not a report.
pub(crate) const MAX_LISTED_PROJECTS: usize = 50;

/// One project named in a coverage report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoverageProject {
    pub path: String,
    /// Marks a project wired on one side only (CI or registry, not both).
    pub partial: bool,
    /// Deep-links the project in GitLab; empty renders as plain text.
    pub web_url: String,
}

/// The input to a coverage alarm: the headline counts plus the projects still
/// to migrate. It is a plain struct rather than the coverage module's own types
/// so notify stays free of a dependency on the scanner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoverageReport {
    pub target: i64,
    pub applied: i64,
    pub partial: i64,
    pub not_applied: i64,
    pub errored: i64,
    pub skipped: i64,
    pub not_applied_projects: Vec<CoverageProject>,
    /// Marks a preview built from made-up numbers, so a delivered sample can
    /// never be mistaken for a real report.
    pub sample: bool,
}

/// The body posted for a coverage report. `text` is the human-readable summary
/// Slack/Mattermost render as-is; the structured fields let any other consumer
/// route on the raw numbers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CoveragePayload {
    pub text: String,
    pub event: String,
    pub target: i64,
    pub applied: i64,
    pub partial: i64,
    pub not_applied: i64,
    pub errored: i64,
    pub skipped: i64,
    pub percent: i64,
    /// The paths still to migrate, capped the same as the text.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<String>,
    pub timestamp: String,
}

/// Reports that the last scan left nothing to act on: every target project is
/// applied, with no partial, none missing, and no scan error hiding a verdict.
/// A scan that found no target at all is not full coverage: that usually means
/// the scope is wrong rather than the work being done.
pub fn is_full_coverage(r: &CoverageReport) -> bool {
    r.target > 0 && r.partial == 0 && r.not_applied == 0 && r.errored == 0
}

/// Renders applied/target to one decimal, or "-" when there is no denominator
/// to divide by.
fn coverage_percent(applied: i64, target: i64) -> String {
    if target == 0 {
        return "-".to_string();
    }
    format!("{:.1}%", applied as f64 * 100.0 / target as f64)
}

impl Notifier {
    /// Renders the console's coverage page as an mrkdwn link. `query` filters
    /// the page to the projects a line is about, so the screen matches the
    /// message; empty lands on the page as it opens. Without an external URL
    /// there is nowhere to point, and the label is returned as plain text.
    fn coverage_link(&self, label: &str, query: &str) -> String {
        let ext = self.external_url();
        if ext.is_empty() {
            return label.to_string();
        }
        let mut url = format!("{ext}/workspace/coverage");
        if !query.is_empty() {
            url.push('?');
            url.push_str(query);
        }
        format!("<{url}|{label}>")
    }

    /// Renders a coverage alarm.
    pub fn build_coverage_report(&self, r: &CoverageReport) -> CoveragePayload {
        // The title carries the link to the coverage page: it is the first thing a
        // reader reaches for, and it lands on the page unfiltered, because the
        // message is about the whole number. The [TEST] marker stays outside the
        // link so a sample still reads as a sample.
        let mut title = self.coverage_link("Forklift coverage report", "");
        if r.sample {
            title = format!("[TEST] {title}");
        }

        let fields = [
            AlarmField::new("Coverage", coverage_percent(r.applied, r.target)),
            AlarmField::new(
                "Target (has CI)",
                format!(
                    "{}, applied {}, partial {}, not applied {}, scan errors {}",
                    r.target, r.applied, r.partial, r.not_applied, r.errored
                ),
            ),
            AlarmField::new("Out of scope (no CI)", r.skipped.to_string()),
        ];
        let subtitle = if r.sample {
            "Sample report with example numbers. No action needed.".to_string()
        } else {
            format!("Projects not yet building through {}.", self.forklift())
        };
        let mut text = render_alarm(&title, &subtitle, &fields);

        let shown = if r.not_applied_projects.len() > MAX_LISTED_PROJECTS {
            &r.not_applied_projects[..MAX_LISTED_PROJECTS]
        } else {
            &r.not_applied_projects[..]
        };
        let mut paths = Vec::with_capacity(shown.len());
        if !shown.is_empty() {
            let noun = if r.not_applied_projects.len() == 1 {
                "project"
            } else {
                "projects"
            };
            let mut lines = vec![
                String::new(),
                format!(
                    "*Not fully applied, {} {noun}*",
                    r.not_applied_projects.len()
                ),
            ];
            for (i, p) in shown.iter().enumerate() {
                let label = if p.web_url.is_empty() {
                    p.path.clone()
                } else {
                    format!("<{}|{}>", p.web_url, p.path)
                };
                let suffix = if p.partial { " (partial)" } else { "" };
                lines.push(format!("{}. {label}{suffix}", i + 1));
                paths.push(p.path.clone());
            }
            // The cut is exactly where a reader needs a way out, so the pointer
            // carries the link rather than only naming the page.
            let remaining = r.not_applied_projects.len() - shown.len();
            if remaining > 0 {
                lines.push(format!(
                    "_{remaining} more, {}_",
                    self.coverage_link("see the coverage page", "status=no")
                ));
            }
            text.push('\n');
            text.push_str(&lines.join("\n"));
        }

        let percent = if r.target > 0 {
            (r.applied as f64 / r.target as f64 * 100.0 + 0.5) as i64
        } else {
            0
        };
        let event = if r.sample { "test" } else { "coverage.report" };
        CoveragePayload {
            text,
            event: event.to_string(),
            target: r.target,
            applied: r.applied,
            partial: r.partial,
            not_applied: r.not_applied,
            errored: r.errored,
            skipped: r.skipped,
            percent,
            projects: paths,
            timestamp: timestamp(),
        }
    }

    /// Delivers a coverage payload to one webhook URL synchronously and reports
    /// the outcome, so a manual send can tell the administrator per receiver
    /// whether it landed.
    pub async fn send_coverage_report(&self, url: &str, p: &CoveragePayload) -> Result<()> {
        let body = serde_json::to_vec(p)?;
        self.send_json(url, body).await
    }

    /// Delivers a report to every target, detached from the caller. Used by the
    /// scheduled scan, where nobody is waiting on the outcome and a failed
    /// delivery must not fail the scan.
    pub fn notify_coverage_report(self: &Arc<Self>, targets: &[Target], r: &CoverageReport) {
        if targets.is_empty() {
            return;
        }
        let body = match serde_json::to_vec(&self.build_coverage_report(r)) {
            Ok(b) => Bytes::from(b),
            Err(e) => {
                tracing::error!(err = %e, "notify: marshal coverage payload failed");
                return;
            }
        };
        for t in targets {
            if t.url.is_empty() {
                continue;
            }
            tokio::spawn(Arc::clone(self).post(t.clone(), body.clone(), Vec::new()));
        }
    }
}

/// A representative report with made-up numbers, used to preview the message
/// shape before the first scan has produced any.
pub fn sample_coverage_report() -> CoverageReport {
    CoverageReport {
        target: 56,
        applied: 36,
        partial: 1,
        not_applied: 19,
        errored: 0,
        skipped: 18,
        not_applied_projects: vec![
            CoverageProject {
                path: "payments/checkout-api".to_string(),
                ..Default::default()
            },
            CoverageProject {
                path: "platform/notification-worker".to_string(),
                ..Default::default()
            },
            CoverageProject {
                path: "data/etl-batch".to_string(),
                partial: true,
                ..Default::default()
            },
        ],
        sample: true,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use crate::notify::coverage::MAX_LISTED_PROJECTS;
    use crate::notify::tests::webhook::{install_crypto, webhook};
    use crate::notify::*;

    fn test_notifier() -> Notifier {
        install_crypto();
        let n = Notifier::new(Duration::from_secs(2));
        n.set_external_url("https://forklift.example.com");
        n
    }

    #[test]
    fn is_full_coverage_cases() {
        let cases: [(&str, CoverageReport, bool); 5] = [
            (
                "everything applied",
                CoverageReport {
                    target: 10,
                    applied: 10,
                    ..Default::default()
                },
                true,
            ),
            (
                "one partial is not done",
                CoverageReport {
                    target: 10,
                    applied: 9,
                    partial: 1,
                    ..Default::default()
                },
                false,
            ),
            (
                "one missing is not done",
                CoverageReport {
                    target: 10,
                    applied: 9,
                    not_applied: 1,
                    ..Default::default()
                },
                false,
            ),
            // An error hides a verdict, so the run cannot claim full coverage.
            (
                "a scan error is not done",
                CoverageReport {
                    target: 10,
                    applied: 9,
                    errored: 1,
                    ..Default::default()
                },
                false,
            ),
            // No target usually means the scope is wrong, not that the work is
            // finished, so it must not silence the report.
            ("no target at all", CoverageReport::default(), false),
        ];
        for (name, input, want) in cases {
            assert_eq!(
                is_full_coverage(&input),
                want,
                "{name}: is_full_coverage({input:?})"
            );
        }
    }

    #[test]
    fn build_coverage_report() {
        let n = test_notifier();
        let payload = n.build_coverage_report(&CoverageReport {
            target: 4,
            applied: 1,
            partial: 1,
            not_applied: 2,
            skipped: 3,
            not_applied_projects: vec![
                CoverageProject {
                    path: "a/missing".into(),
                    web_url: "https://gitlab.example.com/a/missing".into(),
                    ..Default::default()
                },
                CoverageProject {
                    path: "b/half".into(),
                    partial: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        });

        assert_eq!(payload.event, "coverage.report");
        assert_eq!(payload.percent, 25, "percent");
        assert_eq!(payload.projects.len(), 2, "structured projects");
        assert!(
            payload.text.contains("2 projects"),
            "the heading does not report the count:\n{}",
            payload.text
        );
        for want in [
            "*<https://forklift.example.com/workspace/coverage|Forklift coverage report>*",
            "25.0%",
            "<https://gitlab.example.com/a/missing|a/missing>",
            "(partial)",
            "<https://forklift.example.com|Forklift>",
        ] {
            assert!(
                payload.text.contains(want),
                "text is missing {want:?}:\n{}",
                payload.text
            );
        }
        // The console's own copy rules apply to alarm text too.
        assert!(
            !payload.text.contains(['—', '·']),
            "text uses an em-dash or middot:\n{}",
            payload.text
        );
        assert_eq!(
            payload.text,
            "*<https://forklift.example.com/workspace/coverage|Forklift coverage report>*\n\
         Projects not yet building through <https://forklift.example.com|Forklift>.\n\
         *Coverage*: 25.0%\n\
         *Target (has CI)*: 4, applied 1, partial 1, not applied 2, scan errors 0\n\
         *Out of scope (no CI)*: 3\n\
         \n\
         *Not fully applied, 2 projects*\n\
         1. <https://gitlab.example.com/a/missing|a/missing>\n\
         2. b/half (partial)"
        );
        let v = serde_json::to_value(&payload).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "text",
                "event",
                "target",
                "applied",
                "partial",
                "not_applied",
                "errored",
                "skipped",
                "percent",
                "projects",
                "timestamp"
            ]
        );
    }

    #[test]
    fn build_coverage_report_caps_the_list() {
        let n = test_notifier();
        let many: Vec<CoverageProject> = (0..MAX_LISTED_PROJECTS + 5)
            .map(|_| CoverageProject {
                path: "team/p".into(),
                ..Default::default()
            })
            .collect();
        let payload = n.build_coverage_report(&CoverageReport {
            target: 100,
            applied: 45,
            not_applied: many.len() as i64,
            not_applied_projects: many,
            ..Default::default()
        });
        assert_eq!(
            payload.projects.len(),
            MAX_LISTED_PROJECTS,
            "listed projects, want the list capped at {MAX_LISTED_PROJECTS}"
        );
        assert!(
            payload.text.contains("55 projects"),
            "the heading does not report the full count:\n{}",
            payload.text
        );
        // The cut is where a reader most needs a way out, so it carries the link.
        assert!(
            payload.text.contains("5 more")
                && payload
                    .text
                    .contains("https://forklift.example.com/workspace/coverage?status=no"),
            "the truncation pointer is missing its link:\n{}",
            payload.text
        );
    }

    /// Without an external URL there is nowhere for the title to point, so it stays
    /// plain text rather than rendering a link to nothing.
    #[test]
    fn build_coverage_report_title_without_external_url() {
        install_crypto();
        let n = Notifier::new(Duration::from_secs(2));
        let payload = n.build_coverage_report(&CoverageReport {
            target: 2,
            applied: 2,
            ..Default::default()
        });
        assert!(
            payload.text.contains("*Forklift coverage report*"),
            "the title is not plain text:\n{}",
            payload.text
        );
        assert!(
            !payload.text.contains('<'),
            "text carries a link with no external URL set:\n{}",
            payload.text
        );
    }

    /// A single remaining project reads as "1 project", not "1 projects".
    #[test]
    fn build_coverage_report_singular() {
        let payload = test_notifier().build_coverage_report(&CoverageReport {
            target: 2,
            applied: 1,
            not_applied: 1,
            not_applied_projects: vec![CoverageProject {
                path: "team/only".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        assert!(
            payload.text.contains("1 project*"),
            "singular count is not used:\n{}",
            payload.text
        );
    }

    #[test]
    fn build_coverage_report_marks_a_sample() {
        let n = test_notifier();
        let payload = n.build_coverage_report(&sample_coverage_report());
        assert_eq!(
            payload.event, "test",
            "want test so a sample is never mistaken for a real report"
        );
        assert!(
            payload.text.starts_with("*[TEST]"),
            "a sample is not marked:\n{}",
            payload.text
        );
    }

    #[test]
    fn build_coverage_report_with_no_target() {
        // A report before anything is measured must not divide by zero.
        let payload = test_notifier().build_coverage_report(&CoverageReport::default());
        assert!(
            payload.percent == 0 && payload.text.contains("*Coverage*: -"),
            "empty report = {payload:?}\n{}",
            payload.text
        );
    }

    #[tokio::test]
    async fn send_coverage_report() {
        install_crypto();
        let srv = webhook(200).await;
        let n = test_notifier();
        let payload = n.build_coverage_report(&CoverageReport {
            target: 2,
            applied: 2,
            ..Default::default()
        });
        n.send_coverage_report(&srv.uri(), &payload)
            .await
            .expect("send_coverage_report");
        let reqs = srv.received_requests().await.unwrap();
        let got: CoveragePayload = serde_json::from_slice(&reqs[0].body).unwrap();
        assert!(
            got.event == "coverage.report" && got.applied == 2,
            "delivered payload = {got:?}"
        );
    }

    #[tokio::test]
    async fn send_coverage_report_reports_a_non_2xx() {
        install_crypto();
        let srv = webhook(500).await;
        let n = test_notifier();
        let err = n
            .send_coverage_report(
                &srv.uri(),
                &n.build_coverage_report(&CoverageReport::default()),
            )
            .await;
        assert!(
            err.is_err(),
            "a 500 from the webhook was reported as success"
        );
    }

    #[tokio::test]
    async fn notify_coverage_report_delivers_to_every_target() {
        install_crypto();
        let srv = webhook(200).await;
        let n = std::sync::Arc::new(test_notifier());
        n.notify_coverage_report(
            &[
                Target {
                    name: "a".into(),
                    url: srv.uri(),
                },
                Target {
                    name: "empty".into(),
                    url: String::new(),
                },
            ],
            &sample_coverage_report(),
        );
        let reqs = crate::notify::tests::webhook::await_requests(&srv, 1).await;
        assert_eq!(reqs.len(), 1, "empty URL skipped");
        let got: CoveragePayload = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(got.event, "test");
        assert_eq!(got.target, 56);
    }
}
