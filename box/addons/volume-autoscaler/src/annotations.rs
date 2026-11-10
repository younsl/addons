//! The annotation key prefix every subsystem writes under. More than one
//! subsystem annotates cluster objects (the throughput recommender writes on
//! Nodes, the unused volume scanner on `PersistentVolumeClaims` and
//! `PersistentVolumes`), and the keys are this addon's published interface: one
//! definition keeps them from drifting apart.

/// The prefix of every annotation key this addon writes, joined to a key
/// suffix as `<PREFIX>/<suffix>`. A single DNS label is a valid annotation key
/// prefix, so this needs no domain. Changing it orphans every annotation
/// already written on every object, which is not a per-install decision.
pub const PREFIX: &str = "volume-autoscaler.sh";

/// The prefix 0.1.0 wrote. Its keys are read as a fallback and removed on the
/// next write, so an upgrade neither restarts an unused clock nor strands a
/// protective cordon.
pub const LEGACY_PREFIX: &str = "volume-autoscaler";

/// Joins [`PREFIX`] and a key suffix into a full annotation key.
#[must_use]
pub fn key(suffix: &str) -> String {
    format!("{PREFIX}/{suffix}")
}

/// Joins [`LEGACY_PREFIX`] and a key suffix.
#[must_use]
pub fn legacy_key(suffix: &str) -> String {
    format!("{LEGACY_PREFIX}/{suffix}")
}

/// The legacy keys among `suffixes` that `existing` still carries.
#[must_use]
pub fn legacy_present(
    existing: &std::collections::BTreeMap<String, String>,
    suffixes: &[&str],
) -> Vec<String> {
    suffixes
        .iter()
        .map(|s| legacy_key(s))
        .filter(|k| existing.contains_key(k))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_joins_prefix() {
        assert_eq!(key("unused"), "volume-autoscaler.sh/unused");
        assert_eq!(legacy_key("unused"), "volume-autoscaler/unused");
    }

    #[test]
    fn legacy_present_lists_only_carried_keys() {
        let existing = std::collections::BTreeMap::from([
            ("volume-autoscaler/unused".to_string(), "true".to_string()),
            (
                "volume-autoscaler.sh/unused".to_string(),
                "true".to_string(),
            ),
        ]);
        assert_eq!(
            legacy_present(&existing, &["unused", "unused-since"]),
            vec!["volume-autoscaler/unused"]
        );
    }
}
