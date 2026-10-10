//! The annotation key prefix every subsystem writes under. More than one
//! subsystem annotates cluster objects (the throughput recommender writes on
//! Nodes, the unused volume scanner on `PersistentVolumeClaims` and
//! `PersistentVolumes`), and the keys are this addon's published interface: one
//! definition keeps them from drifting apart.

/// The prefix of every annotation key this addon writes, joined to a key
/// suffix as `<PREFIX>/<suffix>`. A single DNS label is a valid annotation key
/// prefix, so this needs no domain. Changing it orphans every annotation
/// already written on every object, which is not a per-install decision.
pub const PREFIX: &str = "external-ebs-autoresizer";

/// Joins [`PREFIX`] and a key suffix into a full annotation key.
#[must_use]
pub fn key(suffix: &str) -> String {
    format!("{PREFIX}/{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_joins_prefix() {
        assert_eq!(key("unused"), "external-ebs-autoresizer/unused");
    }
}
