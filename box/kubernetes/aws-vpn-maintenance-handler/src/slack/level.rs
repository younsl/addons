//! Classifies every message this controller sends to Slack.
//!
//! The level is rendered into the message body rather than signalled by an
//! icon or a colour, so it survives a forwarded screenshot, a phone
//! notification preview, and a thread read back months later during an
//! incident review. No message is posted without one.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Level {
    /// An expected step that needs nothing from anyone.
    Info,
    /// A step that ended the way it was supposed to.
    Success,
    /// Waiting on a human decision.
    Action,
    /// A run that stopped safely: nothing was changed, or the change is fine
    /// but worth reading.
    Warn,
    /// A run that failed, or a replacement that happened and did not come
    /// back. Needs a human, but the connection still has a path.
    Error,
    /// The connection having no healthy tunnel, or a deadline close enough
    /// that not answering is itself a decision.
    Critical,
}

impl Level {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Success => "SUCCESS",
            Self::Action => "ACTION",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
            Self::Critical => "CRITICAL",
        }
    }

    /// The rendered level marker. Plain brackets rather than mrkdwn, because
    /// the same tag has to read correctly inside a header block, which is plain
    /// text only.
    #[must_use]
    pub fn tag(self) -> String {
        format!("[{}]", self.as_str())
    }

    /// Puts the tag in front of a message.
    #[must_use]
    pub fn prefix(self, msg: &str) -> String {
        format!("{} {msg}", self.tag())
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Names a VPN connection by its Name tag and ID, falling back to the ID alone
/// when the connection carries no Name tag. Plain text rather than mrkdwn, so
/// the same label works in a header block.
#[must_use]
pub fn label(name: &str, id: &str) -> String {
    if name.is_empty() {
        id.to_string()
    } else {
        format!("{name} ({id})")
    }
}

/// One message posted to Slack.
///
/// Level and target are fields rather than something a caller may or may not
/// write into the text: an approver reads these on a phone, often one reply at
/// a time, where a message that does not say how bad it is or which VPN
/// connection it is about is not actionable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub level: Level,
    /// The VPN connection the message is about, from [`label`].
    pub target: String,
    /// The message itself, in mrkdwn.
    pub text: String,
}

impl Notice {
    /// Builds the posted string as sentences. Separator punctuation is avoided
    /// on purpose: a Slack message is read as prose on a phone, and a line held
    /// together by colons and middots reads as a log record instead.
    #[must_use]
    pub fn render(&self) -> String {
        if self.target.is_empty() {
            return self.level.prefix(&self.text);
        }
        self.level
            .prefix(&format!("VPN connection {}. {}", self.target, self.text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_and_prefixes() {
        assert_eq!(Level::Critical.tag(), "[CRITICAL]");
        assert_eq!(Level::Info.prefix("hello"), "[INFO] hello");
        assert_eq!(Level::Warn.to_string(), "WARN");
        assert_eq!(Level::Success.as_str(), "SUCCESS");
        assert_eq!(Level::Action.as_str(), "ACTION");
        assert_eq!(Level::Error.as_str(), "ERROR");
    }

    #[test]
    fn labels_fall_back_to_id() {
        assert_eq!(label("", "vpn-1"), "vpn-1");
        assert_eq!(label("prod", "vpn-1"), "prod (vpn-1)");
    }

    #[test]
    fn notice_renders_as_prose() {
        let n = Notice {
            level: Level::Warn,
            target: "prod (vpn-1)".into(),
            text: "Expired.".into(),
        };
        assert_eq!(n.render(), "[WARN] VPN connection prod (vpn-1). Expired.");
        let n = Notice {
            target: String::new(),
            ..n
        };
        assert_eq!(n.render(), "[WARN] Expired.");
    }
}
