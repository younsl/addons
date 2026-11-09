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

/// What one policy matches once conflicts are settled: the channels and
/// label values it won. Routes come oldest first, the order matching uses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Route {
    pub name: String,
    pub slack_channels: Vec<String>,
    pub alert_label: Option<(String, Vec<String>)>,
    pub alert_channel: Option<String>,
}

/// The entry that takes every channel, or every alert, no route names.
pub const ANY_CHANNEL: &str = "*";

/// Where policies come from.
pub trait PolicySource: Send + Sync {
    fn lookup(&self, agent: &str) -> Lookup;
    /// The routing table, `None` until the source has synced.
    fn routes(&self) -> Option<Vec<Route>>;
}

/// A source that never syncs, installed when the policy cache cannot start.
/// It keeps the mention path failing closed.
pub struct Unavailable;

impl PolicySource for Unavailable {
    fn lookup(&self, _agent: &str) -> Lookup {
        Lookup::Unsynced
    }

    fn routes(&self) -> Option<Vec<Route>> {
        None
    }
}

/// Folds the spellings of one channel together: `#ops`, `ops`, and `OPS`
/// are the same entry. Conversation IDs are upper case already.
pub fn channel_key(entry: &str) -> String {
    entry.trim().trim_start_matches('#').to_lowercase()
}

/// The route an alert takes and the label value it matched on: the oldest
/// route whose label matches, then the `*` catch-all, which matches with an
/// empty value. `label` reads one label value from the alert, empty when
/// absent.
pub fn alert_route<'a>(
    routes: &'a [Route],
    label: &dyn Fn(&str) -> String,
) -> Option<(&'a Route, String)> {
    let mut catch_all = None;
    for r in routes {
        let Some((name, values)) = r.alert_label.as_ref() else {
            continue;
        };
        if values.iter().any(|v| v == ANY_CHANNEL) {
            catch_all.get_or_insert((r, String::new()));
            continue;
        }
        let value = label(name);
        if !value.is_empty() && values.contains(&value) {
            return Some((r, value));
        }
    }
    catch_all
}

/// Draws one Agent by weight with a fresh roll. Alerts use it directly:
/// every analysis is a new task, so there is no session to keep.
pub fn pick_random(backends: &[Backend]) -> Option<&str> {
    let total = total_weight(backends);
    if total == 0 {
        return None;
    }
    pick(backends, rand::random_range(0..total))
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
        assert_eq!(Unavailable.routes(), None);
    }

    #[test]
    fn channel_keys_fold_spellings() {
        assert_eq!(channel_key(" #Ops "), "ops");
        assert_eq!(channel_key("C0123"), "c0123");
    }

    #[test]
    fn alert_route_takes_the_oldest_matching_label() {
        let routes = vec![
            Route {
                name: "db".into(),
                alert_label: Some(("slack_channel".into(), vec!["dba".into(), "slow".into()])),
                ..Route::default()
            },
            Route {
                name: "team-db".into(),
                alert_label: Some(("team".into(), vec!["db".into()])),
                ..Route::default()
            },
            Route {
                name: "chat-only".into(),
                slack_channels: vec!["ops".into()],
                ..Route::default()
            },
        ];
        let labels = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map_or(String::new(), |(_, v)| (*v).to_string())
            }
        };
        let name = |r: Option<(&Route, String)>| r.map(|(r, v)| (r.name.clone(), v));
        assert_eq!(
            name(alert_route(
                &routes,
                &labels(&[("slack_channel", "slow"), ("team", "db")])
            )),
            Some(("db".into(), "slow".into()))
        );
        assert_eq!(
            name(alert_route(&routes, &labels(&[("team", "db")]))),
            Some(("team-db".into(), "db".into()))
        );
        assert_eq!(
            name(alert_route(&routes, &labels(&[("slack_channel", "ops")]))),
            None
        );
        assert_eq!(name(alert_route(&routes, &labels(&[]))), None);

        let mut with_default = vec![Route {
            name: "default".into(),
            alert_label: Some(("any".into(), vec!["*".into()])),
            ..Route::default()
        }];
        with_default.extend(routes);
        assert_eq!(
            name(alert_route(
                &with_default,
                &labels(&[("slack_channel", "dba")])
            )),
            Some(("db".into(), "dba".into()))
        );
        assert_eq!(
            name(alert_route(&with_default, &labels(&[]))),
            Some(("default".into(), String::new()))
        );
    }

    #[test]
    fn pick_random_respects_zero_weights() {
        let b = backends(&[("off", 0), ("on", 3)]);
        for _ in 0..20 {
            assert_eq!(pick_random(&b), Some("on"));
        }
        assert_eq!(pick_random(&backends(&[("off", 0)])), None);
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
