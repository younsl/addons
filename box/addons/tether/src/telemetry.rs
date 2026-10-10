//! Logging setup with a filter that PUT /api/log-level can change at runtime, and
//! an in-memory buffer of recent events that the console reads.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::Timestamp;
use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::{EnvFilter, Layer, Registry, fmt as tracing_fmt, prelude::*, reload};

use crate::config::LogFormat;

pub type LogFilterHandle = reload::Handle<EnvFilter, Registry>;

const CAPACITY: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogRecord {
    pub seq: u64,
    pub time: Timestamp,
    pub level: String,
    pub target: String,
    pub message: String,
    pub fields: Vec<(String, String)>,
}

#[derive(Debug)]
struct Records {
    entries: VecDeque<LogRecord>,
    next: u64,
}

impl Default for Records {
    fn default() -> Self {
        Self {
            entries: VecDeque::with_capacity(CAPACITY),
            next: 1,
        }
    }
}

/// The last 1000 log events, oldest first, each with a rising sequence number.
#[derive(Debug, Clone, Default)]
pub struct LogBuffer {
    records: Arc<Mutex<Records>>,
}

impl LogBuffer {
    fn push(&self, mut record: LogRecord) {
        let Ok(mut records) = self.records.lock() else {
            return;
        };
        record.seq = records.next;
        records.next += 1;
        if records.entries.len() == CAPACITY {
            records.entries.pop_front();
        }
        records.entries.push_back(record);
    }

    /// Records newer than `after`, at most `limit`, oldest first.
    pub fn since(&self, after: u64, limit: usize) -> Vec<LogRecord> {
        let Ok(records) = self.records.lock() else {
            return Vec::new();
        };
        records
            .entries
            .iter()
            .filter(|r| r.seq > after)
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn layer(&self) -> BufferLayer {
        BufferLayer {
            buffer: self.clone(),
        }
    }
}

pub struct BufferLayer {
    buffer: LogBuffer,
}

#[derive(Default)]
struct Fields {
    message: String,
    fields: Vec<(String, String)>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields
                .push((field.name().to_string(), format!("{value:?}")));
        }
    }
}

impl<S: Subscriber> Layer<S> for BufferLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let meta = event.metadata();
        self.buffer.push(LogRecord {
            seq: 0,
            time: Timestamp::now(),
            level: meta.level().as_str().to_ascii_lowercase(),
            target: meta.target().to_string(),
            message: fields.message,
            fields: fields.fields,
        });
    }
}

/// Install the global subscriber. `RUST_LOG` overrides `level`.
pub fn init(level: &str, format: LogFormat) -> (LogFilterHandle, LogBuffer) {
    let (filter, handle) = reload::Layer::new(initial_filter(level));
    let buffer = LogBuffer::default();
    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(buffer.layer());
    match format {
        LogFormat::Json => registry
            .with(tracing_fmt::layer().json().flatten_event(true))
            .init(),
        LogFormat::Text => registry
            .with(tracing_fmt::layer().with_target(false))
            .init(),
    }
    (handle, buffer)
}

/// A duration for people reading logs: milliseconds under a minute, whole
/// seconds above, such as 12ms, 1s 250ms, or 2h 3m 4s.
pub fn human_duration(duration: Duration) -> String {
    let rounded = if duration < Duration::from_secs(60) {
        Duration::from_millis(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
    } else {
        Duration::from_secs(duration.as_secs())
    };
    humantime::format_duration(rounded).to_string()
}

fn initial_filter(level: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_reloads_while_layer_lives() {
        let (layer, handle): (reload::Layer<EnvFilter, Registry>, LogFilterHandle) =
            reload::Layer::new(EnvFilter::new("info"));
        assert!(handle.reload(EnvFilter::new("debug")).is_ok());
        assert_eq!(
            handle.with_current(ToString::to_string).expect("current"),
            "debug"
        );
        drop(layer);
        assert!(handle.reload(EnvFilter::new("warn")).is_err());
        assert_ne!(initial_filter("trace").to_string(), "");
    }

    #[test]
    fn buffer_captures_filtered_events_with_fields() {
        let buffer = LogBuffer::default();
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new("info"))
            .with(buffer.layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!("hidden by the filter");
            tracing::info!(entries = 38, path = "/x", "reconcile finished");
            tracing::warn!(prefix = %"/opt/homebrew", "prefix {} missing", "is");
        });

        let records = buffer.since(0, 10);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].seq, 1);
        assert_eq!(records[0].level, "info");
        assert_eq!(records[0].message, "reconcile finished");
        assert_eq!(
            records[0].fields,
            vec![
                ("entries".to_string(), "38".to_string()),
                ("path".to_string(), "/x".to_string()),
            ]
        );
        assert_eq!(records[1].level, "warn");
        assert_eq!(records[1].message, "prefix is missing");
        assert_eq!(
            records[1].fields,
            vec![("prefix".to_string(), "/opt/homebrew".to_string())]
        );
        assert_eq!(buffer.since(1, 10).len(), 1);
        assert_eq!(buffer.since(0, 1).len(), 1);
    }

    #[test]
    fn human_duration_drops_noise() {
        assert_eq!(human_duration(Duration::from_micros(12_345)), "12ms");
        assert_eq!(human_duration(Duration::from_millis(1250)), "1s 250ms");
        assert_eq!(human_duration(Duration::from_millis(7_384_567)), "2h 3m 4s");
        assert_eq!(human_duration(Duration::ZERO), "0s");
    }

    #[test]
    fn buffer_keeps_only_the_latest_records() {
        let buffer = LogBuffer::default();
        for i in 0..CAPACITY + 5 {
            buffer.push(LogRecord {
                seq: 0,
                time: Timestamp::UNIX_EPOCH,
                level: "info".into(),
                target: "t".into(),
                message: i.to_string(),
                fields: Vec::new(),
            });
        }
        let all = buffer.since(0, usize::MAX);
        assert_eq!(all.len(), CAPACITY);
        assert_eq!(all[0].seq, 6);
        assert_eq!(
            all[CAPACITY - 1].seq,
            u64::try_from(CAPACITY).expect("seq") + 5
        );
    }
}
