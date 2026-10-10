//! The detection notice: maintenance AWS has queued on one VPN connection that
//! is not being replaced yet.
//!
//! The approval card is not the right message for this. A card asks for a
//! decision that cannot be made yet: the window is shut, or a preflight rule is
//! holding the tunnels back. Sending nothing instead leaves the first anyone
//! hears of queued maintenance to be a button appearing in the middle of the
//! night, so this is the message in between, and it carries no buttons.

use std::fmt::Write as _;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::blocks::{context, header, human_duration, mrkdwn_field, section};
use super::level::{Level, Notice, label};
use crate::humanize;

/// One notice covers the connection, not one of its tunnels, which is the same
/// scope the approval that follows will have.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detected {
    /// The identity this notice was sent under: the request ID of every tunnel
    /// cycle it covers.
    pub notice_id: String,
    pub connection_id: String,
    pub connection_name: String,
    pub region: String,
    /// The tunnels with maintenance queued, in address order.
    pub tunnels: Vec<DetectedTunnel>,
    /// When the maintenance window next opens. `None` when it is open now,
    /// where the wait is a preflight rule rather than the schedule.
    pub next_window: Option<DateTime<Utc>>,
    /// Renders the configured maintenance window.
    pub window: String,
}

/// One tunnel of the connection with maintenance queued.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DetectedTunnel {
    pub ip: String,
    pub deadline: Option<DateTime<Utc>>,
    pub deadline_in: Duration,
    /// Why this tunnel is not being replaced, in mrkdwn. It is the planner's
    /// own explanation, so the notice and the logs give the same account.
    pub reason: String,
    /// A deadline already inside `safety.escalateBefore`.
    pub escalate: bool,
    /// Tunnel endpoint lifecycle control is off. A configuration gap only a
    /// human can close, not something the next window fixes.
    pub unmanageable: bool,
}

impl DetectedTunnel {
    fn deadline_text(&self) -> String {
        self.deadline.map_or_else(
            || "no published deadline".to_string(),
            |d| {
                format!(
                    "{} (in {})",
                    humanize::clock(&d),
                    human_duration(self.deadline_in)
                )
            },
        )
    }
}

impl Detected {
    /// Only a connection whose every queued tunnel is unmanageable gets the
    /// harder wording: with one of two tunnels still under control,
    /// maintenance is going to be taken over, just not all of it.
    fn subject(&self) -> &'static str {
        if self.every(|t| t.unmanageable) {
            "VPN tunnel maintenance cannot be taken over"
        } else {
            "Pending VPN tunnel maintenance detected"
        }
    }

    /// Nothing is being asked of anyone, so it is never ACTION. Lifecycle
    /// control being off and a deadline inside the escalation horizon are both
    /// WARN, because in neither case does waiting for the next window resolve
    /// anything.
    fn level(&self) -> Level {
        if self.tunnels.iter().any(|t| t.unmanageable || t.escalate) {
            Level::Warn
        } else {
            Level::Info
        }
    }

    #[must_use]
    pub fn target(&self) -> String {
        label(&self.connection_name, &self.connection_id)
    }

    fn title(&self) -> String {
        Notice {
            level: self.level(),
            target: self.target(),
            text: self.subject().to_string(),
        }
        .render()
    }

    /// Lists what AWS has queued, one line per tunnel with its own deadline.
    fn queued_summary(&self) -> String {
        let mut b = String::from("*Maintenance queued*\n");
        for t in &self.tunnels {
            let _ = writeln!(
                b,
                "• `{}`, applied by AWS itself after {}",
                t.ip,
                t.deadline_text()
            );
        }
        b.trim_end_matches('\n').to_string()
    }

    /// Explains the wait in the planner's own words, collapsed to one line when
    /// every tunnel is held by the same thing.
    fn reason_summary(&self) -> String {
        let head = "*Why it is not being replaced yet*\n";
        let Some(first) = self.tunnels.first() else {
            return head.to_string();
        };
        if self.tunnels.len() == 1 || self.every(|t| t.reason == first.reason) {
            return format!("{head}{}", first.reason);
        }
        let mut b = String::from(head);
        for t in &self.tunnels {
            let _ = writeln!(b, "• `{}`: {}", t.ip, t.reason);
        }
        b.trim_end_matches('\n').to_string()
    }

    /// Says what happens next, and what has to be done by hand for any tunnel
    /// lifecycle control rules out.
    fn next_summary(&self) -> String {
        let unmanageable: Vec<&DetectedTunnel> =
            self.tunnels.iter().filter(|t| t.unmanageable).collect();
        let fix = format!(
            "Enable tunnel endpoint lifecycle control on {} with `ModifyVpnTunnelOptions` (`EnableTunnelLifecycleControl`). Until then AWS applies that maintenance on its own schedule, which may be during business hours, and no approval request can be offered for it.",
            quoted(&unmanageable)
        );
        if unmanageable.len() == self.tunnels.len() {
            return format!("*What to do*\n{fix}");
        }
        let mut line = String::from(
            "*What happens next*\nOne approval request arrives here covering this connection, once it clears every preflight check inside the maintenance window. Nothing is needed from you until then.",
        );
        if let Some(next) = self.next_window {
            let _ = write!(
                line,
                " The window next opens at *{}*.",
                humanize::clock(&next)
            );
        }
        if !unmanageable.is_empty() {
            let _ = write!(line, "\n\n*What to do*\n{fix}");
        }
        line
    }

    /// The one-line version for the notification fallback. It reports the
    /// nearest deadline, which is the one that decides how soon this matters.
    fn deadline_summary(&self) -> String {
        self.nearest().map_or_else(
            || "AWS has published no auto-apply deadline yet.".to_string(),
            |t| format!("AWS applies it itself after {}.", t.deadline_text()),
        )
    }

    fn nearest(&self) -> Option<&DetectedTunnel> {
        self.tunnels
            .iter()
            .filter(|t| t.deadline.is_some())
            .min_by_key(|t| t.deadline)
    }

    fn tunnel_list(&self) -> String {
        let ips: Vec<&str> = self.tunnels.iter().map(|t| t.ip.as_str()).collect();
        if ips.len() == 1 {
            format!("Tunnel {}", ips[0])
        } else {
            format!("{} tunnels: {}", ips.len(), ips.join(", "))
        }
    }

    fn connection_label(&self) -> String {
        if self.connection_name.is_empty() {
            format!("`{}`", self.connection_id)
        } else {
            format!("{} (`{}`)", self.connection_name, self.connection_id)
        }
    }

    /// A notice is never built without a tunnel, so the vacuous true is not
    /// reachable.
    fn every(&self, pred: impl Fn(&DetectedTunnel) -> bool) -> bool {
        !self.tunnels.is_empty() && self.tunnels.iter().all(pred)
    }
}

/// Renders the detection notice. No buttons: approving here would be a
/// decision made without the preflight evidence the approval card carries.
#[must_use]
pub fn detected_blocks(d: &Detected) -> (String, Vec<Value>) {
    let fallback = format!(
        "{} {}. {}",
        d.title(),
        d.tunnel_list(),
        d.deadline_summary()
    );
    let details = vec![
        mrkdwn_field("VPN connection", &d.connection_label()),
        mrkdwn_field("Region", &d.region),
    ];
    let blocks = vec![
        header(&d.title()),
        serde_json::json!({"type": "section", "fields": details}),
        section(&d.queued_summary()),
        section(&d.reason_summary()),
        section(&d.next_summary()),
        context(
            "meta",
            &format!(
                "The maintenance window is {}. This notice is sent once per maintenance cycle. Request IDs are `{}`.",
                d.window, d.notice_id
            ),
        ),
    ];
    (fallback, blocks)
}

/// Renders tunnel addresses as a readable mrkdwn list.
fn quoted(tunnels: &[&DetectedTunnel]) -> String {
    let ips: Vec<String> = tunnels.iter().map(|t| format!("`{}`", t.ip)).collect();
    match ips.len() {
        0 => "this connection's tunnels".to_string(),
        1 => ips[0].clone(),
        n => format!("{} and {}", ips[..n - 1].join(", "), ips[n - 1]),
    }
}

#[cfg(test)]
mod tests {
    use super::super::blocks::text_of;
    use super::*;

    fn at(h: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_785_000_000 + h * 3600, 0).unwrap()
    }

    fn tunnel(ip: &str, deadline_h: Option<i64>, reason: &str) -> DetectedTunnel {
        DetectedTunnel {
            ip: ip.into(),
            deadline: deadline_h.map(at),
            deadline_in: Duration::from_secs(
                u64::try_from(deadline_h.unwrap_or(0)).unwrap() * 3600,
            ),
            reason: reason.into(),
            escalate: false,
            unmanageable: false,
        }
    }

    fn detected(tunnels: Vec<DetectedTunnel>) -> Detected {
        Detected {
            notice_id: "vpn-1|1.1.1.1|1 vpn-1|2.2.2.2|2".into(),
            connection_id: "vpn-1".into(),
            connection_name: "prod".into(),
            region: "ap-northeast-2".into(),
            tunnels,
            next_window: Some(at(10)),
            window: "W".into(),
        }
    }

    #[test]
    fn info_notice_for_a_closed_window() {
        let d = detected(vec![
            tunnel("1.1.1.1", Some(72), "outside window"),
            tunnel("2.2.2.2", Some(48), "outside window"),
        ]);
        let (fallback, blocks) = detected_blocks(&d);
        assert_eq!(
            fallback,
            "[INFO] VPN connection prod (vpn-1). Pending VPN tunnel maintenance detected 2 tunnels: 1.1.1.1, 2.2.2.2. AWS applies it itself after 2026-07-27 17:20 UTC (in 2d)."
        );
        let text = text_of(&blocks);
        assert!(
            text.contains("• `1.1.1.1`, applied by AWS itself after 2026-07-28 17:20 UTC (in 3d)"),
            "{text}"
        );
        assert!(
            text.contains("*Why it is not being replaced yet*\\noutside window"),
            "{text}"
        );
        assert!(
            text.contains("The window next opens at *2026-07-26 03:20 UTC*."),
            "{text}"
        );
        assert!(!text.contains("*What to do*"), "{text}");
        assert!(
            text.contains("Request IDs are `vpn-1|1.1.1.1|1 vpn-1|2.2.2.2|2`"),
            "{text}"
        );
        assert_eq!(blocks.len(), 6);
        assert_eq!(d.target(), "prod (vpn-1)");
    }

    #[test]
    fn differing_reasons_are_listed_per_tunnel() {
        let mut d = detected(vec![
            tunnel("1.1.1.1", None, "peer is DOWN"),
            tunnel("2.2.2.2", None, "cooldown"),
        ]);
        d.next_window = None;
        d.connection_name = String::new();
        let (fallback, blocks) = detected_blocks(&d);
        assert!(
            fallback.ends_with("AWS has published no auto-apply deadline yet."),
            "{fallback}"
        );
        assert!(fallback.contains("VPN connection vpn-1."), "{fallback}");
        let text = text_of(&blocks);
        assert!(
            text.contains("• `1.1.1.1`: peer is DOWN\\n• `2.2.2.2`: cooldown"),
            "{text}"
        );
        assert!(text.contains("no published deadline"), "{text}");
        assert!(!text.contains("The window next opens"), "{text}");
        assert!(text.contains("*VPN connection*\\n`vpn-1`"), "{text}");
    }

    #[test]
    fn unmanageable_tunnels_get_the_fix_and_warn_level() {
        let mut one = tunnel("1.1.1.1", Some(48), "lifecycle off");
        one.unmanageable = true;
        let d = detected(vec![
            one.clone(),
            tunnel("2.2.2.2", Some(48), "outside window"),
        ]);
        let (fallback, blocks) = detected_blocks(&d);
        assert!(
            fallback.starts_with(
                "[WARN] VPN connection prod (vpn-1). Pending VPN tunnel maintenance detected"
            ),
            "{fallback}"
        );
        let text = text_of(&blocks);
        assert!(text.contains("*What happens next*"), "{text}");
        assert!(
            text.contains(
                "*What to do*\\nEnable tunnel endpoint lifecycle control on `1.1.1.1` with"
            ),
            "{text}"
        );

        let mut two = tunnel("2.2.2.2", Some(48), "lifecycle off");
        two.unmanageable = true;
        let d = detected(vec![one, two]);
        let (fallback, blocks) = detected_blocks(&d);
        assert!(
            fallback.contains("VPN tunnel maintenance cannot be taken over"),
            "{fallback}"
        );
        let text = text_of(&blocks);
        assert!(text.contains("*What to do*\\nEnable tunnel endpoint lifecycle control on `1.1.1.1` and `2.2.2.2` with"), "{text}");
        assert!(!text.contains("*What happens next*"), "{text}");
    }

    #[test]
    fn escalation_alone_is_warn_and_single_tunnel_reads_singular() {
        let mut t = tunnel("1.1.1.1", Some(20), "outside window");
        t.escalate = true;
        let d = detected(vec![t]);
        let (fallback, _) = detected_blocks(&d);
        assert!(fallback.starts_with("[WARN]"), "{fallback}");
        assert!(fallback.contains("Tunnel 1.1.1.1."), "{fallback}");
    }

    #[test]
    fn quoted_list_forms() {
        let a = tunnel("1.1.1.1", None, "");
        let b = tunnel("2.2.2.2", None, "");
        let c = tunnel("3.3.3.3", None, "");
        assert_eq!(quoted(&[]), "this connection's tunnels");
        assert_eq!(quoted(&[&a]), "`1.1.1.1`");
        assert_eq!(quoted(&[&a, &b, &c]), "`1.1.1.1`, `2.2.2.2` and `3.3.3.3`");
        assert_eq!(
            Detected::default().reason_summary(),
            "*Why it is not being replaced yet*\n"
        );
    }
}
