//! Who may invoke an agent name through a mention, and which Agent answers:
//! the `AgentAccessPolicy` of that name, or the global `CHAT_ALLOWED_USERS`
//! and the name itself when it has none.

use std::collections::HashSet;

/// The access rule one policy sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rule {
    AllowAll,
    Users(HashSet<String>),
    /// The policy cannot be enforced as written. Everyone is denied, so a
    /// broken restrictive policy never opens the agent up.
    Invalid,
}

/// One Agent a policy sends threads to, with its relative share.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backend {
    pub agent: String,
    pub weight: u32,
}

/// What one policy decides: who may ask, and which Agents answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    pub rule: Rule,
    pub backends: Vec<Backend>,
}

/// What the policy source knows about one agent name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// The initial list has not completed, so a missing policy cannot be told
    /// apart from one not yet seen.
    Unsynced,
    Missing,
    Found(Policy),
}

/// Where policies come from.
pub trait PolicySource: Send + Sync {
    fn lookup(&self, agent: &str) -> Lookup;
}

/// A source that never syncs, installed when the policy cache cannot start.
/// It keeps the mention path failing closed.
pub struct Unavailable;

impl PolicySource for Unavailable {
    fn lookup(&self, _agent: &str) -> Lookup {
        Lookup::Unsynced
    }
}

/// Decides whether `user` may invoke the agent name `lookup` describes and
/// returns the Agents that may answer. A name without a policy is answered by
/// the Agent of that name. The error is the drop reason, which doubles as the
/// `chat_events` label.
pub fn check(
    global: &HashSet<String>,
    agent: &str,
    lookup: Lookup,
    user: &str,
) -> Result<Vec<Backend>, &'static str> {
    let (allowed, backends) = match lookup {
        Lookup::Unsynced => return Err("policy_unsynced"),
        Lookup::Missing => (
            global.is_empty() || global.contains(user),
            vec![Backend {
                agent: agent.to_string(),
                weight: 1,
            }],
        ),
        Lookup::Found(policy) => match policy.rule {
            Rule::Invalid => return Err("policy_invalid"),
            Rule::AllowAll => (true, policy.backends),
            Rule::Users(users) => (users.contains(user), policy.backends),
        },
    };
    if allowed {
        Ok(backends)
    } else {
        Err("user_denied")
    }
}

/// Sum of the weights, the range a roll is drawn from.
pub fn total_weight(backends: &[Backend]) -> u64 {
    backends.iter().map(|b| u64::from(b.weight)).sum()
}

/// Picks the Agent a roll in `0..total_weight` lands on, the way Gateway API
/// splits traffic across `backendRefs`. A zero-weight Agent is never picked.
pub fn pick(backends: &[Backend], roll: u64) -> Option<&str> {
    let mut acc = 0;
    backends.iter().find_map(|b| {
        acc += u64::from(b.weight);
        (roll < acc).then_some(b.agent.as_str())
    })
}

/// Whether a thread that started on `agent` may stay there: the Agent is
/// still a backend with a share. A drained or removed Agent sends the thread
/// back through `pick`, starting a fresh session.
pub fn keeps(backends: &[Backend], agent: &str) -> bool {
    backends.iter().any(|b| b.agent == agent && b.weight > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn backends(items: &[(&str, u32)]) -> Vec<Backend> {
        items
            .iter()
            .map(|(agent, weight)| Backend {
                agent: (*agent).to_string(),
                weight: *weight,
            })
            .collect()
    }

    fn found(rule: Rule) -> Lookup {
        Lookup::Found(Policy {
            rule,
            backends: backends(&[("a-v1", 9), ("a-v2", 1)]),
        })
    }

    #[test]
    fn missing_policy_falls_back_to_the_global_list_and_the_name() {
        assert_eq!(
            check(&set(&[]), "a", Lookup::Missing, "U1"),
            Ok(backends(&[("a", 1)]))
        );
        assert!(check(&set(&["U1"]), "a", Lookup::Missing, "U1").is_ok());
        assert_eq!(
            check(&set(&["U1"]), "a", Lookup::Missing, "U2"),
            Err("user_denied")
        );
    }

    #[test]
    fn a_policy_replaces_the_global_list_and_names_the_backends() {
        let users = found(Rule::Users(set(&["U2"])));
        assert_eq!(
            check(&set(&["U1"]), "a", users.clone(), "U2"),
            Ok(backends(&[("a-v1", 9), ("a-v2", 1)]))
        );
        assert_eq!(
            check(&set(&["U1"]), "a", users.clone(), "U1"),
            Err("user_denied")
        );
        assert_eq!(check(&set(&[]), "a", users, "U3"), Err("user_denied"));
        assert!(check(&set(&["U1"]), "a", found(Rule::AllowAll), "U9").is_ok());
    }

    #[test]
    fn unavailable_never_syncs() {
        assert_eq!(Unavailable.lookup("any"), Lookup::Unsynced);
    }

    #[test]
    fn unsynced_and_invalid_fail_closed() {
        assert_eq!(
            check(&set(&[]), "a", Lookup::Unsynced, "U1"),
            Err("policy_unsynced")
        );
        assert_eq!(
            check(&set(&[]), "a", found(Rule::Invalid), "U1"),
            Err("policy_invalid")
        );
    }

    #[test]
    fn pick_splits_by_weight_and_skips_zero() {
        let b = backends(&[("old", 9), ("off", 0), ("new", 1)]);
        assert_eq!(total_weight(&b), 10);
        let picks: Vec<_> = (0..10).map(|r| pick(&b, r).unwrap()).collect();
        assert_eq!(picks.iter().filter(|a| **a == "old").count(), 9);
        assert_eq!(picks.iter().filter(|a| **a == "new").count(), 1);
        assert!(!picks.contains(&"off"));
        assert_eq!(pick(&b, 10), None);
        assert_eq!(pick(&backends(&[("off", 0)]), 0), None);
    }

    #[test]
    fn a_thread_keeps_its_agent_while_it_has_a_share() {
        let b = backends(&[("old", 0), ("new", 1)]);
        assert!(keeps(&b, "new"));
        assert!(!keeps(&b, "old"));
        assert!(!keeps(&b, "gone"));
    }
}
