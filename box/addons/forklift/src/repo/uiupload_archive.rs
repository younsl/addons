//! Archive entry and JSON normalization shared by the UI upload publishers.

use std::collections::BTreeMap;

/// Cleans an archive entry name, keeping a leading `..` that escapes the root.
///
/// An entry that cleans to "." yields an empty string, which every caller treats as unsafe.
pub(crate) fn clean_archive_path(name: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for segment in name.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if out.pop().is_none() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out.join("/")
}

/// Cleans an archive entry name, dropping any `..` that would escape the root.
pub(crate) fn clamp_archive_path(name: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for segment in name.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out.join("/")
}

/// Recursively re-inserts object keys in sorted order.
pub(crate) fn sorted_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => {
            let sorted: BTreeMap<String, serde_json::Value> = object
                .into_iter()
                .map(|(k, v)| (k, sorted_json(v)))
                .collect();
            let mut out = serde_json::Map::new();
            for (k, v) in sorted {
                out.insert(k, v);
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sorted_json).collect())
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_keeps_escaping_parent_and_clamp_drops_it() {
        assert_eq!(clean_archive_path("./a//b/../c"), "a/c");
        assert_eq!(clean_archive_path("../etc/passwd"), "../etc/passwd");
        assert_eq!(clean_archive_path("."), "");
        assert_eq!(clamp_archive_path("./a//b/../c"), "a/c");
        assert_eq!(clamp_archive_path("../etc/passwd"), "etc/passwd");
    }

    #[test]
    fn sorted_json_orders_nested_keys() {
        let value = serde_json::json!({"b": [{"z": 1, "a": 2}], "a": {"y": 1, "x": 2}});
        assert_eq!(
            serde_json::to_string(&sorted_json(value)).unwrap(),
            r#"{"a":{"x":2,"y":1},"b":[{"a":2,"z":1}]}"#
        );
    }
}
