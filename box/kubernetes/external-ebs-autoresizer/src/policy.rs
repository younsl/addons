//! Resolves per-instance resize settings from the list of per-group policies
//! in the config file. Each policy selects a group of instances via an
//! `instanceSelector` (tag equality and/or a Name regex) and overrides a subset
//! of the global resize settings for that group. When several policies match
//! an instance, the highest weight wins; instances matching no policy use the
//! global defaults.

use std::collections::BTreeMap;

use regex::Regex;
use thiserror::Error;

use crate::config::{
    Config, GROW_MODE_ABSOLUTE, GROW_MODE_PERCENT, ResizePolicy, parse_grow_amount,
};

/// Labels the effective settings of instances that match no policy, in logs
/// and metrics.
pub const DEFAULT_POLICY_NAME: &str = "default";

/// A policy list that could not be compiled.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct PolicyError(String);

/// The fully resolved resize settings applied to one instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    /// The name of the matched policy, or [`DEFAULT_POLICY_NAME`].
    pub policy: String,
    /// When true, the resizer must not touch matching instances.
    pub paused: bool,
    /// When false, suppresses Alertmanager alerts for resize outcomes on
    /// matching instances. Only consulted when alerting is globally enabled.
    pub alert_enabled: bool,
    pub usage_threshold_percent: i32,
    pub grow_mode: String,
    pub grow_percent: i32,
    pub grow_amount_gib: i32,
    pub max_volume_size_gib: i32,
}

/// A validated policy with its regex compiled and grow amount parsed once at
/// load time.
#[derive(Debug)]
struct Compiled {
    policy: ResizePolicy,
    name_re: Option<Regex>,
    effective: Effective,
}

/// Maps an instance to its effective resize settings.
#[derive(Debug)]
pub struct Resolver {
    defaults: Effective,
    policies: Vec<Compiled>,
}

/// Builds the default effective settings from the global config.
#[must_use]
pub fn from_config(cfg: &Config) -> Effective {
    Effective {
        policy: DEFAULT_POLICY_NAME.into(),
        paused: cfg.paused,
        alert_enabled: cfg.alert_enabled,
        usage_threshold_percent: cfg.usage_threshold_percent,
        grow_mode: cfg.grow_mode.clone(),
        grow_percent: cfg.grow_percent,
        grow_amount_gib: cfg.grow_amount_gib,
        max_volume_size_gib: cfg.max_volume_size_gib,
    }
}

impl Resolver {
    /// Builds a resolver for a config: it derives the defaults from the global
    /// settings and validates and compiles the per-group policies. An empty
    /// policy list yields a resolver where every instance resolves to defaults.
    pub fn new(cfg: &Config) -> Result<Self, PolicyError> {
        let defaults = from_config(cfg);
        let mut policies = Vec::with_capacity(cfg.policies.len());
        let mut seen = std::collections::HashSet::new();
        for (i, p) in cfg.policies.iter().enumerate() {
            let c = compile(p, &defaults)
                .map_err(|err| PolicyError(format!("policy {i} ({:?}): {err}", p.name)))?;
            if p.name == DEFAULT_POLICY_NAME {
                return Err(PolicyError(format!(
                    "policy {i}: name {DEFAULT_POLICY_NAME:?} is reserved"
                )));
            }
            if !seen.insert(p.name.clone()) {
                return Err(PolicyError(format!(
                    "policy {i}: duplicate name {:?}",
                    p.name
                )));
            }
            policies.push(c);
        }
        Ok(Self { defaults, policies })
    }

    /// Returns the effective settings for an instance identified by its Name
    /// tag and full tag set. Among matching policies the highest weight wins;
    /// ties resolve to the policy listed first. No match returns the defaults.
    #[must_use]
    pub fn resolve(&self, name: &str, tags: &BTreeMap<String, String>) -> Effective {
        let mut best: Option<&Compiled> = None;
        for c in &self.policies {
            if !matches(c, name, tags) {
                continue;
            }
            if best.is_none_or(|b| c.policy.weight > b.policy.weight) {
                best = Some(c);
            }
        }
        best.map_or_else(|| self.defaults.clone(), |c| c.effective.clone())
    }

    /// Returns one `name(weight=N,paused=B,alert=B)` string per policy in file
    /// order, so the startup log shows each group's active state.
    #[must_use]
    pub fn summaries(&self) -> Vec<String> {
        self.policies
            .iter()
            .map(|c| {
                format!(
                    "{}(weight={},paused={},alert={})",
                    c.policy.name, c.policy.weight, c.effective.paused, c.effective.alert_enabled
                )
            })
            .collect()
    }

    /// Splits every policy bucket (the named ones plus the implicit default)
    /// by its effective alert switch, as `(enabled, muted)`.
    #[must_use]
    pub fn alert_policy_names(&self) -> (Vec<String>, Vec<String>) {
        let mut enabled = Vec::new();
        let mut muted = Vec::new();
        let mut push = |name: &str, on: bool| {
            if on {
                enabled.push(name.to_string());
            } else {
                muted.push(name.to_string());
            }
        };
        push(DEFAULT_POLICY_NAME, self.defaults.alert_enabled);
        for c in &self.policies {
            push(&c.policy.name, c.effective.alert_enabled);
        }
        (enabled, muted)
    }

    /// The number of loaded policies.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.policies.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }

    /// The policy names in file order, excluding the implicit default bucket.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.policies
            .iter()
            .map(|c| c.policy.name.clone())
            .collect()
    }

    /// The effective settings applied to instances matching no named policy.
    #[must_use]
    pub fn default(&self) -> Effective {
        self.defaults.clone()
    }

    /// The compiled effective settings of the named policy.
    #[must_use]
    pub fn effective_of(&self, name: &str) -> Option<Effective> {
        self.policies
            .iter()
            .find(|c| c.policy.name == name)
            .map(|c| c.effective.clone())
    }
}

/// Validates one policy against the defaults it overlays and pre-computes its
/// regex, parsed grow amount, and effective settings.
fn compile(p: &ResizePolicy, defaults: &Effective) -> Result<Compiled, String> {
    if p.name.is_empty() {
        return Err("name is required".into());
    }
    let sel = &p.instance_selector;
    if sel.tags.is_empty() && sel.name_regex.is_empty() {
        return Err(
            "instanceSelector requires tags and/or nameRegex (use nameRegex: \".*\" for a catch-all)"
                .into(),
        );
    }
    if sel.tags.iter().any(|(k, v)| k.is_empty() || v.is_empty()) {
        return Err("instanceSelector.tags entries need a non-empty key and value".into());
    }
    let name_re = if sel.name_regex.is_empty() {
        None
    } else {
        Some(
            Regex::new(&sel.name_regex)
                .map_err(|err| format!("invalid instanceSelector.nameRegex: {err}"))?,
        )
    };

    let mut eff = defaults.clone();
    eff.policy.clone_from(&p.name);
    let rs = &p.resize;
    if let Some(v) = rs.paused {
        eff.paused = v;
    }
    if let Some(v) = rs.alert_enabled {
        eff.alert_enabled = v;
    }
    if let Some(v) = rs.usage_threshold_percent {
        if !(0..=100).contains(&v) {
            return Err(format!(
                "resize.usageThresholdPercent must be between 0 and 100, got {v}"
            ));
        }
        eff.usage_threshold_percent = v;
    }
    if let Some(m) = &rs.grow_mode {
        match m.as_str() {
            GROW_MODE_PERCENT | GROW_MODE_ABSOLUTE => eff.grow_mode.clone_from(m),
            other => {
                return Err(format!(
                    "resize.growMode must be one of {GROW_MODE_PERCENT}, {GROW_MODE_ABSOLUTE}, got {other:?}"
                ));
            }
        }
    }
    if let Some(v) = rs.grow_percent {
        if v <= 0 {
            return Err(format!(
                "resize.growPercent must be greater than 0, got {v}"
            ));
        }
        eff.grow_percent = v;
    }
    if let Some(a) = &rs.grow_amount {
        eff.grow_amount_gib =
            parse_grow_amount(a).map_err(|err| format!("invalid resize.growAmount: {err}"))?;
    }
    if let Some(v) = rs.max_volume_size_gib {
        if v <= 0 {
            return Err(format!(
                "resize.maxVolumeSizeGiB must be greater than 0, got {v}"
            ));
        }
        eff.max_volume_size_gib = v;
    }
    if eff.grow_mode == GROW_MODE_ABSOLUTE && eff.grow_amount_gib <= 0 {
        return Err(
            "resize.growMode is absolute but no growAmount resolves (set resize.growAmount here or globally)"
                .into(),
        );
    }
    Ok(Compiled {
        policy: p.clone(),
        name_re,
        effective: eff,
    })
}

/// Reports whether the selector matches an instance's Name tag and tags.
fn matches(c: &Compiled, name: &str, tags: &BTreeMap<String, String>) -> bool {
    for (k, v) in &c.policy.instance_selector.tags {
        if tags.get(k) != Some(v) {
            return false;
        }
    }
    c.name_re.as_ref().is_none_or(|re| re.is_match(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{InstanceSelector, ResizeSpec};

    fn base_config() -> Config {
        Config {
            region: "r".into(),
            usage_threshold_percent: 80,
            grow_mode: GROW_MODE_PERCENT.into(),
            grow_percent: 10,
            grow_amount: "10GiB".into(),
            grow_amount_gib: 10,
            max_volume_size_gib: 1000,
            alert_enabled: true,
            ..Config::default()
        }
    }

    fn policy(name: &str, weight: i32, regex: &str, tags: &[(&str, &str)]) -> ResizePolicy {
        ResizePolicy {
            name: name.into(),
            weight,
            instance_selector: InstanceSelector {
                tags: tags
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
                name_regex: regex.into(),
            },
            resize: ResizeSpec::default(),
        }
    }

    fn tags(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn no_policies_resolves_to_defaults() {
        let cfg = base_config();
        let r = Resolver::new(&cfg).unwrap();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        let eff = r.resolve("anything", &tags(&[]));
        assert_eq!(eff, from_config(&cfg));
        assert_eq!(eff.policy, DEFAULT_POLICY_NAME);
        assert!(r.names().is_empty());
        assert!(r.summaries().is_empty());
        assert_eq!(r.default(), eff);
        assert!(r.effective_of("x").is_none());
    }

    #[test]
    fn tag_regex_and_precedence() {
        let mut cfg = base_config();
        let mut db = policy("db", 10, "", &[("Role", "database")]);
        db.resize.usage_threshold_percent = Some(70);
        let mut prod = policy("prod", 5, "^prod-", &[]);
        prod.resize.grow_percent = Some(30);
        let both = policy("both", 1, "^prod-", &[("Role", "database")]);
        let mut low_first = policy("low-first", 3, "^tie-", &[]);
        low_first.resize.paused = Some(true);
        let low_second = policy("low-second", 3, "^tie-", &[]);
        cfg.policies = vec![db, prod, both, low_first, low_second];
        let r = Resolver::new(&cfg).unwrap();
        assert_eq!(r.len(), 5);

        let eff = r.resolve("prod-db-1", &tags(&[("Role", "database")]));
        assert_eq!(eff.policy, "db", "highest weight wins");
        assert_eq!(eff.usage_threshold_percent, 70);
        assert_eq!(eff.grow_percent, 10, "inherits defaults");

        let eff = r.resolve("prod-web", &tags(&[("Role", "web")]));
        assert_eq!(eff.policy, "prod");
        assert_eq!(eff.grow_percent, 30);

        assert_eq!(
            r.resolve("x", &tags(&[("Role", "other")])).policy,
            "default"
        );
        assert_eq!(
            r.resolve("tie-1", &tags(&[])).policy,
            "low-first",
            "ties fall back to file order"
        );
        assert!(r.resolve("tie-1", &tags(&[])).paused);

        // Tag AND regex must both match.
        assert_eq!(
            r.resolve("staging-db", &tags(&[("Role", "database")]))
                .policy,
            "db"
        );
        assert_eq!(r.effective_of("prod").unwrap().grow_percent, 30);
        assert_eq!(
            r.names(),
            vec!["db", "prod", "both", "low-first", "low-second"]
        );
        assert_eq!(r.summaries()[0], "db(weight=10,paused=false,alert=true)");
    }

    #[test]
    fn alert_policy_names_split_by_switch() {
        let mut cfg = base_config();
        let mut muted = policy("muted", 1, "m", &[]);
        muted.resize.alert_enabled = Some(false);
        let loud = policy("loud", 1, "l", &[]);
        cfg.policies = vec![muted, loud];
        let r = Resolver::new(&cfg).unwrap();
        let (enabled, muted) = r.alert_policy_names();
        assert_eq!(enabled, vec!["default", "loud"]);
        assert_eq!(muted, vec!["muted"]);
        assert!(!r.resolve("m", &tags(&[])).alert_enabled);
        assert!(r.resolve("l", &tags(&[])).alert_enabled);

        cfg.alert_enabled = false;
        let r = Resolver::new(&cfg).unwrap();
        let (enabled, muted) = r.alert_policy_names();
        assert!(
            enabled.is_empty(),
            "an unset policy switch inherits the muted default"
        );
        assert_eq!(muted, vec!["default", "muted", "loud"]);
        assert_eq!(r.summaries()[0], "muted(weight=1,paused=false,alert=false)");
    }

    #[test]
    fn absolute_mode_inherits_and_validates_amount() {
        let mut cfg = base_config();
        let mut p = policy("abs", 1, "x", &[]);
        p.resize.grow_mode = Some(GROW_MODE_ABSOLUTE.into());
        cfg.policies = vec![p.clone()];
        let r = Resolver::new(&cfg).unwrap();
        let eff = r.resolve("x", &tags(&[]));
        assert_eq!(eff.grow_mode, GROW_MODE_ABSOLUTE);
        assert_eq!(eff.grow_amount_gib, 10, "inherits the global amount");

        p.resize.grow_amount = Some("2048MiB".into());
        cfg.policies = vec![p.clone()];
        let r = Resolver::new(&cfg).unwrap();
        assert_eq!(r.resolve("x", &tags(&[])).grow_amount_gib, 2);

        cfg.grow_amount_gib = 0;
        p.resize.grow_amount = None;
        cfg.policies = vec![p];
        let err = Resolver::new(&cfg).unwrap_err().to_string();
        assert!(err.contains("no growAmount resolves"), "{err}");
    }

    #[test]
    fn compile_errors() {
        let cases: Vec<(ResizePolicy, &str)> = vec![
            (policy("", 0, "x", &[]), "name is required"),
            (
                policy("a", 0, "", &[]),
                "instanceSelector requires tags and/or nameRegex",
            ),
            (policy("a", 0, "", &[("", "v")]), "non-empty key and value"),
            (policy("a", 0, "", &[("k", "")]), "non-empty key and value"),
            (
                policy("a", 0, "(", &[]),
                "invalid instanceSelector.nameRegex",
            ),
            (policy("default", 0, "x", &[]), "is reserved"),
            (
                {
                    let mut p = policy("a", 0, "x", &[]);
                    p.resize.usage_threshold_percent = Some(101);
                    p
                },
                "resize.usageThresholdPercent must be between 0 and 100",
            ),
            (
                {
                    let mut p = policy("a", 0, "x", &[]);
                    p.resize.grow_mode = Some("diag".into());
                    p
                },
                "resize.growMode must be one of",
            ),
            (
                {
                    let mut p = policy("a", 0, "x", &[]);
                    p.resize.grow_percent = Some(0);
                    p
                },
                "resize.growPercent must be greater than 0",
            ),
            (
                {
                    let mut p = policy("a", 0, "x", &[]);
                    p.resize.grow_amount = Some("10GB".into());
                    p
                },
                "invalid resize.growAmount",
            ),
            (
                {
                    let mut p = policy("a", 0, "x", &[]);
                    p.resize.max_volume_size_gib = Some(0);
                    p
                },
                "resize.maxVolumeSizeGiB must be greater than 0",
            ),
        ];
        for (p, want) in cases {
            let mut cfg = base_config();
            cfg.policies = vec![p];
            let err = Resolver::new(&cfg).unwrap_err().to_string();
            assert!(err.contains(want), "want {want:?}, got {err}");
            assert!(err.starts_with("policy 0"), "{err}");
        }
        let mut cfg = base_config();
        cfg.policies = vec![policy("dup", 0, "x", &[]), policy("dup", 0, "y", &[])];
        let err = Resolver::new(&cfg).unwrap_err().to_string();
        assert!(err.contains("policy 1: duplicate name \"dup\""), "{err}");
    }
}
