#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbStat {
    pub db: u32,
    pub keys: u64,
    pub expires: u64,
    pub avg_ttl_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeInfo {
    pub version: String,
    pub role: String,
    pub uptime_days: String,
    pub used_memory: String,
    pub max_memory: String,
    pub clients: String,
    pub ops_per_sec: String,
    pub hit_rate: Option<f64>,
    pub evicted: String,
    pub expired: String,
    pub dbs: Vec<DbStat>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    pub key: Vec<u8>,
    pub kind: String,
    /// Milliseconds, `-1` without expiry, `-2` when the key vanished.
    pub ttl_ms: i64,
    pub size: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPage {
    /// `"0"` when the scan is complete.
    pub cursor: String,
    pub keys: Vec<KeyInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyValue {
    pub key: Vec<u8>,
    pub kind: String,
    pub len: Option<i64>,
    /// Field, index, or score in the first column. Strings use one row with an empty label.
    pub rows: Vec<(String, String)>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbRequest {
    Info,
    Keys {
        db: u32,
        pattern: String,
        cursor: String,
    },
    Value {
        db: u32,
        key: Vec<u8>,
        kind: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum DbReply {
    Info(NodeInfo),
    Keys(KeyPage),
    Value(KeyValue),
}

/// Renders key bytes for display: UTF-8 when valid, `\xNN` escapes otherwise.
pub fn printable(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) if !s.chars().any(char::is_control) => s.to_owned(),
        _ => bytes
            .iter()
            .flat_map(|b| std::ascii::escape_default(*b))
            .map(char::from)
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_escapes_binary_and_controls() {
        assert_eq!(printable(b"user:42"), "user:42");
        assert_eq!(printable(b"a\nb"), "a\\nb");
        assert_eq!(printable(&[0xff, b'a']), "\\xffa");
    }
}
