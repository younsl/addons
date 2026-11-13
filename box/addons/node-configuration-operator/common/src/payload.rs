//! The part of the spec that decides what a node looks like, and the revision
//! hash that names it.

use serde::Serialize;

use crate::crd::{NodeConfigurationSpec, Rules};

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub priority: i32,
    pub dry_run: bool,
    pub interval_seconds: u32,
    pub rules: Rules,
}

/// FNV-1a, written out so the revision of a spec never changes with the
/// toolchain the way the standard hasher may.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

impl AgentPayload {
    pub fn of(spec: &NodeConfigurationSpec) -> Self {
        Self {
            group: spec.group.clone(),
            priority: spec.priority,
            dry_run: spec.dry_run,
            interval_seconds: spec.interval_seconds,
            rules: spec.rules.clone(),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a payload serializes")
    }

    /// Ten hex characters naming this payload, like a pod template hash.
    pub fn revision(&self) -> String {
        format!("{:016x}", fnv1a(self.to_json().as_bytes()))[..10].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::testing::spec;
    use crate::crd::{FailurePolicy, FileRule, NamedRule};

    #[test]
    fn fnv1a_matches_the_reference_vectors() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn revision_follows_what_the_agent_runs_only() {
        let mut s = spec(Rules::default());
        let base = AgentPayload::of(&s).revision();
        assert_eq!(base.len(), 10);

        s.paused = true;
        s.min_ready_seconds = 30;
        s.target.node_groups = vec!["system".into()];
        s.target.exclude_nodes = vec!["node-1".into()];
        assert_eq!(AgentPayload::of(&s).revision(), base);

        let mut ordered = s.clone();
        ordered.group = Some("hardening".into());
        ordered.priority = 10;
        let grouped = AgentPayload::of(&ordered).revision();
        assert_ne!(grouped, base);
        ordered.dry_run = true;
        assert_ne!(AgentPayload::of(&ordered).revision(), grouped);

        s.interval_seconds = 60;
        let interval = AgentPayload::of(&s).revision();
        assert_ne!(interval, base);

        s.rules.push(NamedRule::file(
            "motd",
            FileRule {
                path: "/etc/motd".into(),
                content: "hi\n".into(),
                mode: "0644".into(),
            },
        ));
        let required = AgentPayload::of(&s);
        assert_ne!(required.revision(), interval);
        assert!(!required.to_json().contains("failurePolicy"));

        s.rules[0].failure_policy = FailurePolicy::Ignore;
        assert_ne!(AgentPayload::of(&s).revision(), required.revision());
    }
}
