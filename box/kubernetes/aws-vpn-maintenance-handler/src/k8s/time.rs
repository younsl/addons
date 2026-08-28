//! Go-compatible timestamp encoding. The Go controller wrote `time.Time`
//! values, whose zero value renders as `0001-01-01T00:00:00Z`, and a state
//! `ConfigMap` written by it must still decode after the port.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const ZERO: &str = "0001-01-01T00:00:00Z";

/// Serializes `None` as Go's zero time and reads the zero time back as `None`.
pub mod zero_as_none {
    use super::{DateTime, Deserialize, Deserializer, Serialize, Serializer, Utc, ZERO};

    #[allow(clippy::ref_option)]
    pub fn serialize<S: Serializer>(t: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => t
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
                .serialize(s),
            None => ZERO.serialize(s),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
        let raw: Option<String> = Option::deserialize(d)?;
        match raw {
            None => Ok(None),
            Some(s) if s.is_empty() || s == ZERO || s.starts_with("0001-01-01T") => Ok(None),
            Some(s) => DateTime::parse_from_rfc3339(&s)
                .map(|t| Some(t.with_timezone(&Utc)))
                .map_err(serde::de::Error::custom),
        }
    }
}

/// Like [`zero_as_none`], but omitted entirely when `None`. Pair with
/// `#[serde(default, skip_serializing_if = "Option::is_none")]`.
pub mod optional {
    pub use super::zero_as_none::{deserialize, serialize};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Wrap {
        #[serde(with = "zero_as_none")]
        t: Option<DateTime<Utc>>,
    }

    #[test]
    fn round_trips_go_zero_time() {
        let none: Wrap = serde_json::from_str(r#"{"t":"0001-01-01T00:00:00Z"}"#).unwrap();
        assert_eq!(none.t, None);
        assert_eq!(
            serde_json::to_string(&none).unwrap(),
            r#"{"t":"0001-01-01T00:00:00Z"}"#
        );
        let some: Wrap = serde_json::from_str(r#"{"t":"2026-07-27T02:14:09.113Z"}"#).unwrap();
        assert_eq!(some.t.unwrap().timestamp_millis(), 1_785_118_449_113);
        assert_eq!(
            serde_json::to_string(&some).unwrap(),
            r#"{"t":"2026-07-27T02:14:09.113Z"}"#
        );
        let null: Wrap = serde_json::from_str(r#"{"t":null}"#).unwrap();
        assert_eq!(null.t, None);
        let empty: Wrap = serde_json::from_str(r#"{"t":""}"#).unwrap();
        assert_eq!(empty.t, None);
        assert!(serde_json::from_str::<Wrap>(r#"{"t":"yesterday"}"#).is_err());
    }
}
