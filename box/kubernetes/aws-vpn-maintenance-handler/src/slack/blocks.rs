//! The approval card and its resolved form.

use std::fmt::Write as _;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::level::{Level, Notice, label};
use super::socket::{ACTION_APPROVE, ACTION_DENY};
use crate::humanize;

/// The display form of a proposed replacement, a plain value type so the Slack
/// layer stays free of domain and AWS types.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Proposal {
    pub request_id: String,
    pub connection_id: String,
    /// The Name tag, or empty.
    pub connection_name: String,
    /// The transit or virtual private gateway ID, and its Name tag. The name is
    /// what an approver recognizes; the ID is what they can paste into the
    /// console.
    pub gateway: String,
    pub gateway_name: String,
    pub customer_gateway_id: String,
    pub customer_gateway_name: String,
    pub region: String,
    /// The tunnel about to be replaced, and the first step of the plan.
    pub tunnel_ip: String,
    /// The connection's remaining tunnels, in the order they will be replaced
    /// under this same approval. Empty when the approval covers one tunnel.
    pub queue: Vec<String>,
    /// How long a replaced tunnel must hold UP before the next one may start,
    /// which is what makes the plan a sequence rather than a batch.
    pub stable_requirement: Duration,
    /// The tunnel that carries traffic during the replacement.
    pub peer_ip: String,
    /// Shown so the approver can judge the risk instead of trusting the
    /// controller's verdict.
    pub peer_routes: i32,
    pub peer_stable_for: Duration,
    pub static_routes: bool,
    pub deadline_in: Duration,
    pub deadline: Option<DateTime<Utc>>,
    pub escalate: bool,
    pub dry_run: bool,
    pub approval_expiry: Duration,
    /// Renders the configured maintenance window.
    pub window: String,
    /// Whether the traffic gate ran, and what it found. Shown so the approver
    /// sees the measured load rather than trusting that the schedule happened
    /// to be quiet.
    pub traffic_checked: bool,
    pub traffic_detail: String,
}

impl Proposal {
    /// Names what the message is about, without its level.
    fn subject(&self) -> String {
        let mut s = if self.escalate {
            "URGENT VPN tunnel replacement approval".to_string()
        } else {
            "VPN tunnel replacement approval".to_string()
        };
        if self.dry_run {
            s.push_str(" (dry run)");
        }
        s
    }

    /// An escalated request is CRITICAL because the AWS deadline is close
    /// enough that not answering has a cost.
    const fn level(&self) -> Level {
        if self.escalate {
            Level::Critical
        } else {
            Level::Action
        }
    }

    /// Names the VPN connection this proposal is about.
    #[must_use]
    pub fn target(&self) -> String {
        label(&self.connection_name, &self.connection_id)
    }

    /// Also the notification fallback, so it is the whole content of a phone
    /// push notification: the level and the connection have to be inside it.
    fn title(&self) -> String {
        Notice {
            level: self.level(),
            target: self.target(),
            text: self.subject(),
        }
        .render()
    }

    fn connection_label(&self) -> String {
        if self.connection_name.is_empty() {
            format!("`{}`", self.connection_id)
        } else {
            format!("{} (`{}`)", self.connection_name, self.connection_id)
        }
    }

    /// Spells out the order the tunnels are replaced in, because approving
    /// covers the whole connection and the approver is authorizing every step.
    fn plan_summary(&self) -> String {
        let mut b = String::from("*Replacement order*\n");
        let _ = writeln!(
            b,
            "1. `{}` starts now. Traffic rides `{}` while it is down.",
            self.tunnel_ip, self.peer_ip
        );
        let mut previous = self.tunnel_ip.as_str();
        for (i, next) in self.queue.iter().enumerate() {
            let _ = writeln!(
                b,
                "{}. `{next}` starts only once `{previous}` is back UP, carrying routes, and has held steady for {}.",
                i + 2,
                human_duration(self.stable_requirement)
            );
            previous = next;
        }
        if self.queue.is_empty() {
            let _ = write!(b, "`{}` is not touched by this approval.", self.peer_ip);
            return b;
        }
        b.push_str("Never two at once. Any step that would be unsafe stops the rest and leaves them for a later window.");
        b
    }

    /// Lists the passed checks with their numbers, so approval is an informed
    /// decision rather than a rubber stamp.
    fn preflight_summary(&self) -> String {
        let mut b = String::from("*Preflight checks passed*\n");
        let _ = writeln!(
            b,
            "• Peer tunnel `{}` is UP and has been stable for {}",
            self.peer_ip,
            human_duration(self.peer_stable_for)
        );
        if self.static_routes {
            b.push_str(
                "• Connection is static-routes-only, so BGP route count is not a health signal\n",
            );
        } else {
            let _ = writeln!(
                b,
                "• Peer tunnel is accepting {} BGP route(s)",
                self.peer_routes
            );
        }
        b.push_str("• AWS reports pending endpoint maintenance and lifecycle control is enabled\n");
        if self.traffic_checked {
            let _ = writeln!(b, "• Traffic gate reports that {}", self.traffic_detail);
        }
        b.push_str("• No other replacement is running, and this connection is out of cooldown");
        b
    }

    /// Explains the cost of not approving: the real choice is between a known
    /// window and an AWS-chosen time.
    fn deadline_summary(&self) -> String {
        let Some(deadline) = self.deadline else {
            return "*If you do nothing*\nAWS has not published an auto-apply deadline for this maintenance yet. The request expires and the tunnel is proposed again in a later window.".to_string();
        };
        let line = format!(
            "*If you do nothing*\nAWS applies this maintenance itself after *{}* (in {}), at a time of its choosing, which may be during business hours.",
            humanize::clock(&deadline),
            human_duration(self.deadline_in)
        );
        if self.escalate {
            format!("*URGENT.* {line}")
        } else {
            line
        }
    }
}

/// Renders the approval card with approve and deny buttons, returning the
/// notification fallback and the blocks.
#[must_use]
pub fn approval_blocks(p: &Proposal) -> (String, Vec<Value>) {
    let fallback = format!(
        "{} Tunnel {} is the one to replace.",
        p.title(),
        p.tunnel_ip
    );

    let details = vec![
        mrkdwn_field("VPN connection", &p.connection_label()),
        mrkdwn_field("Region", &p.region),
        mrkdwn_field("Tunnel to replace", &format!("`{}`", p.tunnel_ip)),
        mrkdwn_field("Tunnel carrying traffic", &format!("`{}`", p.peer_ip)),
        mrkdwn_field("Gateway", &or_dash(&label(&p.gateway_name, &p.gateway))),
        mrkdwn_field(
            "Customer gateway",
            &or_dash(&label(&p.customer_gateway_name, &p.customer_gateway_id)),
        ),
    ];

    let mut blocks = vec![
        header(&p.title()),
        json!({"type": "section", "fields": details}),
        section(&p.plan_summary()),
        section(&p.preflight_summary()),
        section(&p.deadline_summary()),
    ];

    if p.dry_run {
        blocks.push(context(
            "dryrun",
            "*Dry run is enabled.* Approving validates IAM permissions and arguments through the AWS DryRun flag. No tunnel is replaced.",
        ));
    } else {
        blocks.push(context(
            "irreversible",
            "*This cannot be undone.* Approving replaces the tunnel endpoint immediately. The tunnel drops for the duration of the replacement and traffic rides the other tunnel.",
        ));
    }

    blocks.push(json!({
        "type": "actions",
        "block_id": "approval",
        "elements": [approve_button(p), deny_button(p)],
    }));
    blocks.push(context(
        "meta",
        &format!(
            "The maintenance window is {}. This request expires in {}. Request ID is `{}`.",
            p.window,
            human_duration(p.approval_expiry),
            p.request_id
        ),
    ));
    (fallback, blocks)
}

/// The approve button carries a confirmation dialog. The extra click is
/// deliberate: the API call is irreversible and a mis-tap on a phone is easy.
fn approve_button(p: &Proposal) -> Value {
    let confirm_body = if p.dry_run {
        format!(
            "This is a dry run. It validates the replacement of tunnel {} of {}.\n\nNothing will actually be replaced.",
            p.tunnel_ip,
            p.connection_label()
        )
    } else {
        format!(
            "Replace tunnel {} of {} now.\n\nThis is irreversible. The tunnel will drop and traffic will ride {}.",
            p.tunnel_ip,
            p.connection_label(),
            p.peer_ip
        )
    };
    json!({
        "type": "button",
        "action_id": ACTION_APPROVE,
        "value": p.request_id,
        "style": "primary",
        "text": plain("Approve replacement"),
        "confirm": {
            "title": plain("Confirm replacement"),
            "text": plain(&confirm_body),
            "confirm": plain("Replace it"),
            "deny": plain("Cancel"),
            "style": "danger",
        },
    })
}

fn deny_button(p: &Proposal) -> Value {
    json!({
        "type": "button",
        "action_id": ACTION_DENY,
        "value": p.request_id,
        "style": "danger",
        "text": plain("Deny"),
    })
}

/// Renders the card after a decision, without its buttons. `level` is the
/// outcome's level, not the request's: a card that ended in ERROR must not
/// keep reading as a pending action.
#[must_use]
pub fn resolved_blocks(p: &Proposal, level: Level, outcome: &str) -> (String, Vec<Value>) {
    let resolved = Notice {
        level,
        target: p.target(),
        text: p.subject(),
    }
    .render();
    let fallback = format!("{resolved} Tunnel {}. {outcome}", p.tunnel_ip);
    let details = vec![
        mrkdwn_field("VPN connection", &p.connection_label()),
        mrkdwn_field("Tunnel", &format!("`{}`", p.tunnel_ip)),
        mrkdwn_field("Peer tunnel", &format!("`{}`", p.peer_ip)),
        mrkdwn_field("Region", &p.region),
    ];
    (
        fallback,
        vec![
            header(&resolved),
            json!({"type": "section", "fields": details}),
            section(outcome),
            context("meta", &format!("`{}`", p.request_id)),
        ],
    )
}

pub(super) fn plain(text: &str) -> Value {
    json!({"type": "plain_text", "text": text, "emoji": true})
}

pub(super) fn mrkdwn(text: &str) -> Value {
    json!({"type": "mrkdwn", "text": text})
}

pub(super) fn header(text: &str) -> Value {
    json!({"type": "header", "text": plain(text)})
}

pub(super) fn section(text: &str) -> Value {
    json!({"type": "section", "text": mrkdwn(text)})
}

pub(super) fn context(block_id: &str, text: &str) -> Value {
    json!({"type": "context", "block_id": block_id, "elements": [mrkdwn(text)]})
}

pub(super) fn mrkdwn_field(label: &str, value: &str) -> Value {
    mrkdwn(&format!("*{label}*\n{value}"))
}

pub(super) fn or_dash(s: &str) -> String {
    if s.is_empty() {
        "-".to_string()
    } else {
        s.to_string()
    }
}

/// Renders minutes below a day and whole hours above it.
#[must_use]
pub fn human_duration(d: Duration) -> String {
    if d.is_zero() {
        return "0s".to_string();
    }
    if d < Duration::from_secs(60) {
        return humanize::go_duration(humanize::round_to_second(d));
    }
    if d < Duration::from_hours(24) {
        return humanize::go_duration(humanize::round_to_minute(d));
    }
    let hours = d.as_secs() / 3600;
    let days = hours / 24;
    let rem = hours % 24;
    if rem == 0 {
        format!("{days}d")
    } else {
        format!("{days}d{rem}h")
    }
}

#[cfg(test)]
pub(crate) fn text_of(blocks: &[Value]) -> String {
    blocks
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal() -> Proposal {
        Proposal {
            request_id: "vpn-1|1.1.1.1|1785000000".into(),
            connection_id: "vpn-1".into(),
            connection_name: "prod".into(),
            gateway: "tgw-1".into(),
            gateway_name: "prod-tgw".into(),
            customer_gateway_id: "cgw-1".into(),
            region: "ap-northeast-2".into(),
            tunnel_ip: "1.1.1.1".into(),
            queue: vec!["2.2.2.2".into()],
            stable_requirement: Duration::from_secs(300),
            peer_ip: "2.2.2.2".into(),
            peer_routes: 4,
            peer_stable_for: Duration::from_secs(3 * 3600 + 20),
            deadline_in: Duration::from_secs(50 * 3600),
            deadline: Some(DateTime::from_timestamp(1_785_000_000, 0).unwrap()),
            approval_expiry: Duration::from_secs(3600),
            window: "\"0 2 * * *\" for 3h0m0s (UTC), min remaining 30m0s".into(),
            traffic_checked: true,
            traffic_detail: "traffic is 2.00, inside the quietest 20%".into(),
            ..Proposal::default()
        }
    }

    #[test]
    fn approval_card_carries_everything_an_approver_needs() {
        let p = proposal();
        let (fallback, blocks) = approval_blocks(&p);
        assert_eq!(
            fallback,
            "[ACTION] VPN connection prod (vpn-1). VPN tunnel replacement approval Tunnel 1.1.1.1 is the one to replace."
        );
        let text = text_of(&blocks);
        assert!(text.contains("prod (`vpn-1`)"), "{text}");
        assert!(text.contains("prod-tgw (tgw-1)"), "{text}");
        assert!(text.contains("*Customer gateway*\\ncgw-1"), "{text}");
        assert!(text.contains("2. `2.2.2.2` starts only once `1.1.1.1` is back UP, carrying routes, and has held steady for 5m0s."), "{text}");
        assert!(text.contains("Never two at once."), "{text}");
        assert!(text.contains("stable for 3h0m0s"), "{text}");
        assert!(text.contains("accepting 4 BGP route(s)"), "{text}");
        assert!(
            text.contains("Traffic gate reports that traffic is 2.00"),
            "{text}"
        );
        assert!(
            text.contains("after *2026-07-25 17:20 UTC* (in 2d2h)"),
            "{text}"
        );
        assert!(text.contains("*This cannot be undone.*"), "{text}");
        assert!(text.contains("\"action_id\":\"vtr_approve\""), "{text}");
        assert!(text.contains("\"action_id\":\"vtr_deny\""), "{text}");
        assert!(
            text.contains(
                "This is irreversible. The tunnel will drop and traffic will ride 2.2.2.2."
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "This request expires in 1h0m0s. Request ID is `vpn-1|1.1.1.1|1785000000`."
            ),
            "{text}"
        );
        assert_eq!(blocks[0]["type"], "header");
        assert_eq!(blocks.len(), 8);
    }

    #[test]
    fn dry_run_and_escalation_change_wording() {
        let p = Proposal {
            dry_run: true,
            escalate: true,
            queue: Vec::new(),
            static_routes: true,
            traffic_checked: false,
            deadline: None,
            connection_name: String::new(),
            gateway: String::new(),
            gateway_name: String::new(),
            ..proposal()
        };
        let (fallback, blocks) = approval_blocks(&p);
        assert!(
            fallback.starts_with(
                "[CRITICAL] VPN connection vpn-1. URGENT VPN tunnel replacement approval (dry run)"
            ),
            "{fallback}"
        );
        let text = text_of(&blocks);
        assert!(
            text.contains("`2.2.2.2` is not touched by this approval."),
            "{text}"
        );
        assert!(text.contains("static-routes-only"), "{text}");
        assert!(!text.contains("Traffic gate"), "{text}");
        assert!(
            text.contains("AWS has not published an auto-apply deadline"),
            "{text}"
        );
        assert!(text.contains("*Dry run is enabled.*"), "{text}");
        assert!(
            text.contains(
                "This is a dry run. It validates the replacement of tunnel 1.1.1.1 of `vpn-1`."
            ),
            "{text}"
        );
        assert!(text.contains("*Gateway*\\n-"), "{text}");
    }

    #[test]
    fn escalated_deadline_is_marked_urgent() {
        let p = Proposal {
            escalate: true,
            ..proposal()
        };
        let text = text_of(&approval_blocks(&p).1);
        assert!(text.contains("*URGENT.* *If you do nothing*"), "{text}");
    }

    #[test]
    fn resolved_card_drops_the_buttons() {
        let p = proposal();
        let (fallback, blocks) = resolved_blocks(&p, Level::Error, "*Rejected by AWS.*");
        assert_eq!(
            fallback,
            "[ERROR] VPN connection prod (vpn-1). VPN tunnel replacement approval Tunnel 1.1.1.1. *Rejected by AWS.*"
        );
        let text = text_of(&blocks);
        assert!(!text.contains("vtr_approve"));
        assert!(text.contains("*Peer tunnel*\\n`2.2.2.2`"), "{text}");
        assert!(text.contains("*Rejected by AWS.*"));
        assert_eq!(blocks.len(), 4);
        assert_eq!(p.target(), "prod (vpn-1)");
    }

    #[test]
    fn human_duration_scales() {
        assert_eq!(human_duration(Duration::ZERO), "0s");
        assert_eq!(human_duration(Duration::from_millis(45_400)), "45s");
        assert_eq!(human_duration(Duration::from_secs(90 * 60 + 40)), "1h31m0s");
        assert_eq!(human_duration(Duration::from_secs(48 * 3600)), "2d");
        assert_eq!(human_duration(Duration::from_secs(50 * 3600 + 60)), "2d2h");
    }
}
