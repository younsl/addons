//! Who may invoke an agent through a mention: the rule an `AgentAccessPolicy`
//! sets for that agent, or the global `CHAT_ALLOWED_USERS` when it has none.

use std::collections::HashSet;

/// The rule one policy sets for its agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rule {
    AllowAll,
    Users(HashSet<String>),
    /// The policy cannot be enforced as written. Everyone is denied, so a
    /// broken restrictive policy never opens the agent up.
    Invalid,
}

/// What the policy source knows about one agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// The initial list has not completed, so a missing policy cannot be told
    /// apart from one not yet seen.
    Unsynced,
    Missing,
    Found(Rule),
}

/// Where per-agent rules come from.
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

/// Decides whether `user` may invoke the agent `lookup` describes. The error
/// is the drop reason, which doubles as the `chat_events` label.
pub fn check(global: &HashSet<String>, lookup: Lookup, user: &str) -> Result<(), &'static str> {
    let allowed = match lookup {
        Lookup::Unsynced => return Err("policy_unsynced"),
        Lookup::Found(Rule::Invalid) => return Err("policy_invalid"),
        Lookup::Found(Rule::AllowAll) => true,
        Lookup::Found(Rule::Users(users)) => users.contains(user),
        Lookup::Missing => global.is_empty() || global.contains(user),
    };
    if allowed { Ok(()) } else { Err("user_denied") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn missing_policy_falls_back_to_the_global_list() {
        assert_eq!(check(&set(&[]), Lookup::Missing, "U1"), Ok(()));
        assert_eq!(check(&set(&["U1"]), Lookup::Missing, "U1"), Ok(()));
        assert_eq!(
            check(&set(&["U1"]), Lookup::Missing, "U2"),
            Err("user_denied")
        );
    }

    #[test]
    fn a_policy_replaces_the_global_list() {
        let users = Lookup::Found(Rule::Users(set(&["U2"])));
        assert_eq!(check(&set(&["U1"]), users.clone(), "U2"), Ok(()));
        assert_eq!(
            check(&set(&["U1"]), users.clone(), "U1"),
            Err("user_denied")
        );
        assert_eq!(check(&set(&[]), users, "U3"), Err("user_denied"));
        assert_eq!(
            check(&set(&["U1"]), Lookup::Found(Rule::AllowAll), "U9"),
            Ok(())
        );
    }

    #[test]
    fn unavailable_never_syncs() {
        assert_eq!(Unavailable.lookup("any"), Lookup::Unsynced);
    }

    #[test]
    fn unsynced_and_invalid_fail_closed() {
        assert_eq!(
            check(&set(&[]), Lookup::Unsynced, "U1"),
            Err("policy_unsynced")
        );
        assert_eq!(
            check(&set(&[]), Lookup::Found(Rule::Invalid), "U1"),
            Err("policy_invalid")
        );
    }
}
