//! Delivery ledger: which events and reminders were sent, and where polling
//! stopped. Replaces the former in-memory dedup.
//!
//! An event is (re)sent when its `lastUpdatedTime` advances, so status
//! transitions such as `upcoming -> open -> closed` each post once.

pub mod journal;

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeliveryState {
    /// Recorded before the POST. Outcome not yet known.
    Pending,
    Sent,
    /// Dropped by the filter, cold start, or an invalid payload.
    Skipped,
    /// The POST may or may not have posted. Never retried.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventEntry {
    pub last_updated: DateTime<Utc>,
    pub state: DeliveryState,
}

/// A send left `Pending` by a previous holder, now marked `Unknown`.
#[derive(Debug, PartialEq, Eq)]
pub struct Abandoned {
    pub arn: String,
    pub reminder_offset_hours: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ledger {
    /// Fencing term, bumped by every new leader.
    pub term: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    /// End of the last completed poll window. `None` until the first cycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<DateTime<Utc>>,
    #[serde(default)]
    pub events: BTreeMap<String, EventEntry>,
    #[serde(default)]
    pub reminders: BTreeMap<String, BTreeMap<u32, DeliveryState>>,
}

impl Ledger {
    /// True if the event is new or strictly newer than the recorded one.
    /// A missing `last_updated` is treated as always new.
    pub fn should_process(&self, arn: &str, last_updated: Option<DateTime<Utc>>) -> bool {
        match (self.events.get(arn), last_updated) {
            (None, _) | (Some(_), None) => true,
            (Some(prev), Some(curr)) => curr > prev.last_updated,
        }
    }

    pub fn event(&self, arn: &str) -> Option<&EventEntry> {
        self.events.get(arn)
    }

    pub fn set_event(
        &mut self,
        arn: &str,
        last_updated: Option<DateTime<Utc>>,
        state: DeliveryState,
    ) {
        self.events.insert(
            arn.to_string(),
            EventEntry {
                last_updated: last_updated.unwrap_or_else(Utc::now),
                state,
            },
        );
    }

    /// Puts back the entry seen before a send that turned out not to post.
    pub fn restore_event(&mut self, arn: &str, previous: Option<EventEntry>) {
        match previous {
            Some(entry) => {
                self.events.insert(arn.to_string(), entry);
            }
            None => {
                self.events.remove(arn);
            }
        }
    }

    pub fn should_remind(&self, arn: &str, offset_hours: u32) -> bool {
        self.reminders
            .get(arn)
            .is_none_or(|offsets| !offsets.contains_key(&offset_hours))
    }

    pub fn set_reminder(&mut self, arn: &str, offset_hours: u32, state: DeliveryState) {
        self.reminders
            .entry(arn.to_string())
            .or_default()
            .insert(offset_hours, state);
    }

    pub fn clear_reminder(&mut self, arn: &str, offset_hours: u32) {
        if let Some(offsets) = self.reminders.get_mut(arn) {
            offsets.remove(&offset_hours);
            if offsets.is_empty() {
                self.reminders.remove(arn);
            }
        }
    }

    /// Keeps reminder records only for events still in the upcoming window.
    pub fn retain_reminders<F: FnMut(&str) -> bool>(&mut self, mut still_relevant: F) {
        self.reminders.retain(|arn, _| still_relevant(arn));
    }

    /// Drops event records whose `last_updated` is older than `ttl`.
    pub fn prune_events(&mut self, ttl: Duration, now: DateTime<Utc>) {
        let cutoff = now - ttl;
        self.events.retain(|_, e| e.last_updated >= cutoff);
    }

    /// Marks every `Pending` send as `Unknown` and returns them. Called when a
    /// new holder takes over, since the previous holder's POST may have posted.
    pub fn abandon_pending(&mut self) -> Vec<Abandoned> {
        let mut out = Vec::new();
        for (arn, entry) in &mut self.events {
            if entry.state == DeliveryState::Pending {
                entry.state = DeliveryState::Unknown;
                out.push(Abandoned {
                    arn: arn.clone(),
                    reminder_offset_hours: None,
                });
            }
        }
        for (arn, offsets) in &mut self.reminders {
            for (offset, state) in offsets.iter_mut() {
                if *state == DeliveryState::Pending {
                    *state = DeliveryState::Unknown;
                    out.push(Abandoned {
                        arn: arn.clone(),
                        reminder_offset_hours: Some(*offset),
                    });
                }
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_arn_always_processed() {
        assert!(Ledger::default().should_process("arn:1", Some(Utc::now())));
    }

    #[test]
    fn same_timestamp_skipped_newer_processed() {
        let mut l = Ledger::default();
        let t = Utc::now();
        l.set_event("arn:1", Some(t), DeliveryState::Sent);
        assert!(!l.should_process("arn:1", Some(t)));
        assert!(l.should_process("arn:1", Some(t + Duration::seconds(1))));
        assert!(l.should_process("arn:1", None));
    }

    #[test]
    fn missing_timestamp_records_now() {
        let mut l = Ledger::default();
        l.set_event("arn:1", None, DeliveryState::Sent);
        assert!(!l.should_process("arn:1", Some(Utc::now() - Duration::hours(1))));
    }

    #[test]
    fn restore_puts_back_previous_or_removes() {
        let mut l = Ledger::default();
        let t = Utc::now();
        l.set_event("arn:1", Some(t), DeliveryState::Sent);
        let prev = l.event("arn:1").cloned();
        l.set_event(
            "arn:1",
            Some(t + Duration::seconds(5)),
            DeliveryState::Pending,
        );
        l.restore_event("arn:1", prev.clone());
        assert_eq!(l.event("arn:1"), prev.as_ref());

        l.set_event("arn:2", Some(t), DeliveryState::Pending);
        l.restore_event("arn:2", None);
        assert!(l.event("arn:2").is_none());
    }

    #[test]
    fn prune_drops_stale() {
        let mut l = Ledger::default();
        let now = Utc::now();
        l.set_event("old", Some(now - Duration::hours(80)), DeliveryState::Sent);
        l.set_event("new", Some(now), DeliveryState::Sent);
        l.prune_events(Duration::hours(72), now);
        assert_eq!(l.len(), 1);
        assert!(l.event("new").is_some());
    }

    #[test]
    fn reminders_fire_once_per_offset() {
        let mut l = Ledger::default();
        assert!(l.should_remind("arn:1", 24));
        l.set_reminder("arn:1", 24, DeliveryState::Sent);
        assert!(!l.should_remind("arn:1", 24));
        assert!(l.should_remind("arn:1", 6));
        l.clear_reminder("arn:1", 24);
        assert!(l.should_remind("arn:1", 24));
        assert!(l.reminders.is_empty());
        l.clear_reminder("arn:missing", 24);
    }

    #[test]
    fn retain_reminders_drops_others() {
        let mut l = Ledger::default();
        l.set_reminder("keep", 24, DeliveryState::Sent);
        l.set_reminder("drop", 24, DeliveryState::Sent);
        l.retain_reminders(|arn| arn == "keep");
        assert!(!l.should_remind("keep", 24));
        assert!(l.should_remind("drop", 24));
    }

    #[test]
    fn abandon_pending_marks_unknown() {
        let mut l = Ledger::default();
        l.set_event("e1", Some(Utc::now()), DeliveryState::Pending);
        l.set_event("e2", Some(Utc::now()), DeliveryState::Sent);
        l.set_reminder("r1", 24, DeliveryState::Pending);
        let abandoned = l.abandon_pending();
        assert_eq!(
            abandoned,
            vec![
                Abandoned {
                    arn: "e1".into(),
                    reminder_offset_hours: None
                },
                Abandoned {
                    arn: "r1".into(),
                    reminder_offset_hours: Some(24)
                },
            ]
        );
        assert_eq!(l.event("e1").unwrap().state, DeliveryState::Unknown);
        assert_eq!(l.event("e2").unwrap().state, DeliveryState::Sent);
        assert_eq!(l.reminders["r1"][&24], DeliveryState::Unknown);
        assert_eq!(l.abandon_pending(), Vec::<Abandoned>::new());
    }

    #[test]
    fn json_round_trip() {
        let mut l = Ledger {
            term: 3,
            holder: Some("pod-a".into()),
            cursor: Some(Utc::now()),
            ..Default::default()
        };
        l.set_event("arn:1", Some(Utc::now()), DeliveryState::Sent);
        l.set_reminder("arn:1", 24, DeliveryState::Unknown);
        let json = serde_json::to_string(&l).unwrap();
        assert!(json.contains("\"state\":\"sent\""));
        let back: Ledger = serde_json::from_str(&json).unwrap();
        assert_eq!(back, l);
    }
}
