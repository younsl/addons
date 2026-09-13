//! Process logging in `log/slog`'s two output shapes.
//!
//! Dashboards, alerts and log queries were written against slog's output, so
//! the format is part of the contract, not a detail: JSON lines carry `time`,
//! `level`, `msg` and the event's fields flattened next to them, and the text
//! handler writes `key=value` pairs with slog's quoting rules. `tracing` has
//! no such formatter, so this module implements one.

use std::fmt;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

/// Initialises the global subscriber for `level` (debug, info, warn, error) and `format` (json,
/// text).
pub fn init_logging(level: &str, format: &str) {
    let filter = match level.to_ascii_lowercase().as_str() {
        "debug" => Level::DEBUG,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    };
    let builder = tracing_subscriber::fmt()
        .with_max_level(filter)
        .with_writer(std::io::stdout);
    if format.eq_ignore_ascii_case("text") {
        let _ = builder.event_format(SlogFormat::Text).try_init();
    } else {
        let _ = builder.event_format(SlogFormat::Json).try_init();
    }
}

/// The two slog handlers, as a `tracing` event formatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlogFormat {
    /// `slog.NewJSONHandler`: one JSON object per line.
    Json,
    /// `slog.NewTextHandler`: `key=value` pairs.
    Text,
}

impl<S, N> FormatEvent<S, N> for SlogFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let time = crate::meta::time::now_rfc3339();
        let level = level_text(*event.metadata().level());
        let line = match self {
            SlogFormat::Json => format_json(&time, level, &visitor.message, &visitor.fields),
            SlogFormat::Text => format_text(&time, level, &visitor.message, &visitor.fields),
        };
        writeln!(writer, "{line}")
    }
}

/// slog's level names.
fn level_text(level: Level) -> &'static str {
    match level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARN",
        Level::INFO => "INFO",
        Level::DEBUG | Level::TRACE => "DEBUG",
    }
}

/// One recorded field: the JSON encoding of its value, plus whether the value
/// is a bare literal (numbers, booleans) or a string.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FieldValue {
    /// JSON encoding, quoted for strings.
    json: String,
    /// Unquoted rendering, used by the text handler.
    text: String,
}

/// Collects an event's message and fields in the order they were recorded.
#[derive(Debug, Default)]
pub(crate) struct FieldVisitor {
    pub(crate) message: String,
    pub(crate) fields: Vec<(String, FieldValue)>,
}

impl FieldVisitor {
    fn push(&mut self, field: &Field, json: String, text: String) {
        if field.name() == "message" {
            self.message = text;
            return;
        }
        self.fields
            .push((field.name().to_string(), FieldValue { json, text }));
    }
}

impl Visit for FieldVisitor {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.push(field, value.to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.push(field, value.to_string(), value.to_string());
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        self.push(field, value.to_string(), value.to_string());
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        self.push(field, value.to_string(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.push(field, value.to_string(), value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        let json = if value.is_finite() {
            value.to_string()
        } else {
            quote_json(&value.to_string())
        };
        self.push(field, json, value.to_string());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, quote_json(value), value.to_string());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        let text = value.to_string();
        self.push(field, quote_json(&text), text);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let text = format!("{value:?}");
        self.push(field, quote_json(&text), text);
    }
}

/// Renders one JSON line: `time`, `level`, `msg`, then the fields flattened at
/// the top level in the order they were recorded.
pub(crate) fn format_json(
    time: &str,
    level: &str,
    msg: &str,
    fields: &[(String, FieldValue)],
) -> String {
    let mut out = String::with_capacity(96 + msg.len());
    out.push_str("{\"time\":");
    out.push_str(&quote_json(time));
    out.push_str(",\"level\":");
    out.push_str(&quote_json(level));
    out.push_str(",\"msg\":");
    out.push_str(&quote_json(msg));
    for (name, value) in fields {
        out.push(',');
        out.push_str(&quote_json(name));
        out.push(':');
        out.push_str(&value.json);
    }
    out.push('}');
    out
}

/// Renders one text line the way slog's `TextHandler` does.
pub(crate) fn format_text(
    time: &str,
    level: &str,
    msg: &str,
    fields: &[(String, FieldValue)],
) -> String {
    let mut out = String::with_capacity(64 + msg.len());
    out.push_str("time=");
    out.push_str(&quote_text(time));
    out.push_str(" level=");
    out.push_str(&quote_text(level));
    out.push_str(" msg=");
    out.push_str(&quote_text(msg));
    for (name, value) in fields {
        out.push(' ');
        out.push_str(&quote_text(name));
        out.push('=');
        out.push_str(&quote_text(&value.text));
    }
    out
}

/// JSON string encoding, the subset `serde_json` would produce for a string.
fn quote_json(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// slog's text quoting: a value is quoted when it is empty or contains a
/// space, an `=`, a quote or a control character; everything else is written
/// bare, which is what makes the text handler readable.
fn quote_text(s: &str) -> String {
    if needs_quoting(s) {
        quote_json(s)
    } else {
        s.to_string()
    }
}

/// Mirrors slog's `needsQuoting`.
fn needs_quoting(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    s.bytes().any(|b| {
        b < 0x80 && b != b'\\' && (b == b' ' || b == b'=' || b == b'"' || b < 0x20 || b == 0x7f)
    })
}

#[cfg(test)]
pub(crate) mod tests {
    //! The log format is a contract: dashboards and alerts match on these field
    //! names and on the shape of the line, so both handlers are pinned here.

    use std::io;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::fmt::MakeWriter;

    use crate::server::logging::*;

    /// A `MakeWriter` that appends into a shared buffer, so a formatted event can
    /// be read back in the test.
    #[derive(Clone, Default)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("buffer").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for BufWriter {
        type Writer = BufWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Emits one event through a subscriber using `format` and returns the line.
    fn capture(format: SlogFormat, f: impl FnOnce()) -> String {
        let buf = BufWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(buf.clone())
            .event_format(format)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let out = buf.0.lock().expect("buffer").clone();
        String::from_utf8(out).expect("utf-8")
    }

    #[test]
    fn json_line_has_slog_shape() {
        let line = capture(SlogFormat::Json, || {
            tracing::info!(
                addr = "127.0.0.1:8080",
                count = 3,
                ok = true,
                "http listening"
            );
        });
        let v: serde_json::Value = serde_json::from_str(line.trim()).expect("valid json");
        assert_eq!(v["level"], "INFO");
        assert_eq!(v["msg"], "http listening");
        assert_eq!(v["addr"], "127.0.0.1:8080");
        // Numbers and booleans keep their JSON type; only text is quoted.
        assert_eq!(v["count"], serde_json::json!(3));
        assert_eq!(v["ok"], serde_json::json!(true));
        let time = v["time"].as_str().expect("time");
        chrono::DateTime::parse_from_rfc3339(time).expect("RFC3339Nano time");
    }

    #[test]
    fn json_levels_use_slog_names() {
        for (emit, want) in [(0u8, "DEBUG"), (1, "INFO"), (2, "WARN"), (3, "ERROR")] {
            let line = capture(SlogFormat::Json, || match emit {
                0 => tracing::debug!("m"),
                1 => tracing::info!("m"),
                2 => tracing::warn!("m"),
                _ => tracing::error!("m"),
            });
            let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(v["level"], want);
        }
    }

    #[test]
    fn text_line_quotes_like_slog() {
        let line = capture(SlogFormat::Text, || {
            tracing::warn!(
                err = "boom now",
                repo = "npm-proxy",
                n = 2,
                "readiness check failed"
            );
        });
        let line = line.trim();
        assert!(line.starts_with("time="), "{line}");
        assert!(line.contains(" level=WARN "), "{line}");
        // Spaces force quoting; bare words stay bare; numbers stay numbers.
        assert!(line.contains(r#"msg="readiness check failed""#), "{line}");
        assert!(line.contains(r#"err="boom now""#), "{line}");
        assert!(line.contains(" repo=npm-proxy"), "{line}");
        assert!(line.contains(" n=2"), "{line}");
    }

    #[test]
    fn text_quoting_rules() {
        let fields = |v: &str| {
            format_text(
                "T",
                "INFO",
                "m",
                &[(
                    "k".to_string(),
                    FieldValue {
                        json: quote_json(v),
                        text: v.to_string(),
                    },
                )],
            )
        };
        assert!(fields("plain").ends_with("k=plain"));
        assert!(fields("with space").ends_with(r#"k="with space""#));
        assert!(fields("a=b").ends_with(r#"k="a=b""#));
        assert!(fields("").ends_with(r#"k="""#));
        assert!(fields("tab\there").ends_with(r#"k="tab\there""#));
    }

    #[test]
    fn json_escapes_field_names_and_values() {
        let line = format_json(
            "T",
            "ERROR",
            "quote \" here",
            &[(
                "a\"b".to_string(),
                FieldValue {
                    json: quote_json("v\"w"),
                    text: "v\"w".to_string(),
                },
            )],
        );
        let v: serde_json::Value = serde_json::from_str(&line).expect("valid json");
        assert_eq!(v["msg"], "quote \" here");
        assert_eq!(v["a\"b"], "v\"w");
    }

    /// `init_logging` must accept every documented level/format pair without
    /// panicking (a second call is a no-op: the global subscriber is already set).
    #[test]
    fn init_logging_accepts_documented_values() {
        for level in ["debug", "info", "warn", "error", "nonsense"] {
            for format in ["json", "text", "nonsense"] {
                init_logging(level, format);
            }
        }
    }
}
