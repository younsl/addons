use std::collections::VecDeque;

use chrono::{DateTime, Local};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Change,
    Warn,
}

#[derive(Debug, Clone)]
pub struct Event {
    pub at: DateTime<Local>,
    pub level: Level,
    pub text: String,
}

/// Newest-first event log capped at `capacity`.
#[derive(Debug)]
pub struct EventLog {
    events: VecDeque<Event>,
    capacity: usize,
}

impl EventLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            events: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, at: DateTime<Local>, level: Level, text: impl Into<String>) {
        let text = text.into();
        tracing::info!(?level, "{text}");
        self.events.push_front(Event { at, level, text });
        self.events.truncate(self.capacity);
    }

    pub fn iter(&self) -> impl Iterator<Item = &Event> {
        self.events.iter()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.events.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_newest_first_within_capacity() {
        let mut log = EventLog::new(2);
        let now = Local::now();
        log.push(now, Level::Info, "a");
        log.push(now, Level::Change, "b");
        log.push(now, Level::Warn, "c");
        let texts: Vec<_> = log.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["c", "b"]);
        assert_eq!(log.len(), 2);
    }
}
