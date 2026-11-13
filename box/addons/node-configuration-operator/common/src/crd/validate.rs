//! Checks the API server schema cannot express, run by the controller and the
//! agent on every reconcile.

use regex::Regex;

use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use std::collections::BTreeSet;

use super::{
    DNS_LABEL, FileModeRule, FileRule, LineEditRule, LoginShellRule, NamedRule,
    NodeConfigurationSpec, RuleAction, ScriptRule,
};

/// Every binding carries a copy of the rules and has to stay well below the
/// object size limit of the API server.
pub const MAX_PAYLOAD_BYTES: usize = 900 * 1024;

fn valid_max_unavailable(value: &IntOrString) -> bool {
    match value {
        IntOrString::Int(count) => *count >= 1,
        IntOrString::String(percent) => percent
            .strip_suffix('%')
            .and_then(|p| p.parse::<u32>().ok())
            .is_some_and(|p| (1..=100).contains(&p)),
    }
}

/// The name is used as a label value on the bindings.
pub const MAX_NAME_LEN: usize = 63;

pub fn parse_mode(mode: &str) -> Option<u32> {
    if !(3..=4).contains(&mode.len()) {
        return None;
    }
    u32::from_str_radix(mode, 8).ok()
}

fn wildcard_before_last_segment(path: &str) -> bool {
    path.rsplit_once('/')
        .is_some_and(|(dir, _)| dir.contains(['*', '?']))
}

/// Every problem found, empty when the resource is usable. The schema checks
/// are repeated for resources stored before a CRD upgrade.
pub fn problems(name: &str, spec: &NodeConfigurationSpec) -> Vec<String> {
    let mut out = Vec::new();
    if name.len() > MAX_NAME_LEN {
        out.push(format!(
            "metadata.name must be at most {MAX_NAME_LEN} characters"
        ));
    }
    if let Some(group) = &spec.group {
        let valid = Regex::new(DNS_LABEL).is_ok_and(|re| re.is_match(group));
        if !valid || group.len() > MAX_NAME_LEN {
            out.push(format!(
                "spec.group must be a lowercase DNS label of at most {MAX_NAME_LEN} characters"
            ));
        }
    }
    if spec.interval_seconds < 30 {
        out.push("spec.intervalSeconds must be at least 30".to_string());
    }
    if spec.progress_deadline_seconds < 60 {
        out.push("spec.progressDeadlineSeconds must be at least 60".to_string());
    }
    if let Some(rolling_update) = &spec.strategy.rolling_update {
        if spec.strategy.type_ != super::StrategyType::RollingUpdate {
            out.push("spec.strategy.rollingUpdate is allowed only with type RollingUpdate".into());
        }
        if rolling_update
            .max_unavailable
            .as_ref()
            .is_some_and(|value| !valid_max_unavailable(value))
        {
            out.push(
                "spec.strategy.rollingUpdate.maxUnavailable must be a positive count or a percentage from 1% to 100%"
                    .to_string(),
            );
        }
    }
    let payload = crate::payload::AgentPayload::of(spec).to_json().len();
    if payload > MAX_PAYLOAD_BYTES {
        out.push(format!(
            "spec.rules serialize to {payload} bytes, more than the {MAX_PAYLOAD_BYTES} a binding holds"
        ));
    }
    out.extend(rule_problems(&spec.rules));
    out
}

fn file_mode_problems(at: &str, rule: &FileModeRule, out: &mut Vec<String>) {
    match (&rule.mode, &rule.clear) {
        (Some(mode), None) | (None, Some(mode)) => {
            if parse_mode(mode).is_none() {
                out.push(format!("{at} holds an invalid octal mode {mode:?}"));
            }
        }
        _ => out.push(format!("{at} requires exactly one of mode and clear")),
    }
    if rule.paths.is_empty() {
        out.push(format!("{at} paths must not be empty"));
    }
    for path in &rule.paths {
        if !path.starts_with('/') {
            out.push(format!("{at} path {path:?} must be absolute"));
        } else if wildcard_before_last_segment(path) {
            out.push(format!(
                "{at} path {path:?} may hold wildcards in the last segment only"
            ));
        }
    }
}

fn login_shell_problems(at: &str, rule: &LoginShellRule, out: &mut Vec<String>) {
    if !rule.shell.starts_with('/') || rule.shell.contains([':', '\n']) {
        out.push(format!("{at} shell {:?} is not a valid path", rule.shell));
    }
    if rule
        .users
        .iter()
        .any(|u| u.is_empty() || u.contains([':', '\n']))
    {
        out.push(format!("{at} users hold an invalid account name"));
    }
}

fn line_edit_problems(at: &str, rule: &LineEditRule, out: &mut Vec<String>) {
    if !rule.path.starts_with('/') {
        out.push(format!("{at} path {:?} must be absolute", rule.path));
    }
    if let Err(e) = Regex::new(&rule.pattern) {
        let reason = e.to_string().lines().last().unwrap_or_default().to_string();
        out.push(format!(
            "{at} pattern is not a valid regular expression, {reason}"
        ));
    }
}

fn file_problems(at: &str, rule: &FileRule, out: &mut Vec<String>) {
    if !rule.path.starts_with('/') || rule.path.ends_with('/') {
        out.push(format!(
            "{at} path {:?} must be an absolute file path",
            rule.path
        ));
    }
    if parse_mode(&rule.mode).is_none() {
        out.push(format!("{at} holds an invalid octal mode {:?}", rule.mode));
    }
}

fn script_problems(at: &str, rule: &ScriptRule, out: &mut Vec<String>) {
    if rule.run.trim().is_empty() {
        out.push(format!("{at} run must not be empty"));
    }
    if rule.unless.as_ref().is_some_and(|u| u.trim().is_empty()) {
        out.push(format!("{at} unless must not be empty"));
    }
    if !(1..=600).contains(&rule.timeout_seconds) {
        out.push(format!("{at} timeoutSeconds must be from 1 to 600"));
    }
}

/// The rule checks alone, which the agent repeats on the revision it is given.
pub fn rule_problems(rules: &[NamedRule]) -> Vec<String> {
    let mut out = Vec::new();
    let mut names = BTreeSet::new();
    let label = Regex::new(DNS_LABEL).ok();
    for (i, rule) in rules.iter().enumerate() {
        let at = format!("rule {:?}", rule.name);
        let valid = label.as_ref().is_some_and(|re| re.is_match(&rule.name));
        if !valid || rule.name.len() > MAX_NAME_LEN {
            out.push(format!(
                "rules[{i}].name {:?} must be a lowercase DNS label of at most {MAX_NAME_LEN} characters",
                rule.name
            ));
        }
        if !names.insert(rule.name.as_str()) {
            out.push(format!("{at} is defined more than once"));
        }
        match rule.action() {
            Some(RuleAction::FileMode(spec)) => file_mode_problems(&at, spec, &mut out),
            Some(RuleAction::LoginShell(spec)) => login_shell_problems(&at, spec, &mut out),
            Some(RuleAction::LineEdit(spec)) => line_edit_problems(&at, spec, &mut out),
            Some(RuleAction::File(spec)) => file_problems(&at, spec, &mut out),
            Some(RuleAction::Script(spec)) => script_problems(&at, spec, &mut out),
            None => out.push(format!(
                "{at} requires exactly one of fileMode, loginShell, lineEdit, file, and script"
            )),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::Rules;

    fn spec(rules: Rules) -> NodeConfigurationSpec {
        crate::crd::testing::spec(rules)
    }

    #[test]
    fn max_unavailable_is_a_count_or_a_percentage() {
        for ok in [IntOrString::Int(1), IntOrString::String("25%".into())] {
            assert!(valid_max_unavailable(&ok), "{ok:?}");
        }
        for bad in [
            IntOrString::Int(0),
            IntOrString::String("0%".into()),
            IntOrString::String("101%".into()),
            IntOrString::String("3".into()),
        ] {
            assert!(!valid_max_unavailable(&bad), "{bad:?}");
        }
    }

    #[test]
    fn rollout_settings_are_checked() {
        let mut s = spec(Rules::default());
        s.progress_deadline_seconds = 5;
        s.strategy = crate::crd::RolloutStrategy {
            type_: crate::crd::StrategyType::OnDelete,
            rolling_update: Some(crate::crd::RollingUpdate {
                max_unavailable: Some(IntOrString::Int(0)),
                cordon: false,
            }),
        };
        s.rules = (0..16)
            .map(|i| {
                NamedRule::file(
                    &format!("file-{i}"),
                    FileRule {
                        path: format!("/etc/file-{i}"),
                        content: "x".repeat(65536),
                        mode: "0644".into(),
                    },
                )
            })
            .collect();
        let all = problems("mng", &s).join("\n");
        for needle in [
            "progressDeadlineSeconds must be at least 60",
            "rollingUpdate is allowed only with type RollingUpdate",
            "maxUnavailable must be a positive count",
            "more than the 921600 a binding holds",
        ] {
            assert!(all.contains(needle), "missing {needle:?} in:\n{all}");
        }
    }

    #[test]
    fn parses_octal_modes() {
        assert_eq!(parse_mode("640"), Some(0o640));
        assert_eq!(parse_mode("4750"), Some(0o4750));
        assert_eq!(parse_mode("0001"), Some(0o001));
        assert_eq!(parse_mode("64"), None);
        assert_eq!(parse_mode("06400"), None);
        assert_eq!(parse_mode("0888"), None);
    }

    #[test]
    fn a_valid_spec_has_no_problems() {
        let rules = vec![
            NamedRule::file_mode(
                "restrict-shell-profiles",
                FileModeRule {
                    paths: vec!["/root/.bash*".into()],
                    mode: Some("0640".into()),
                    clear: None,
                },
            ),
            NamedRule::file_mode(
                "drop-setuid",
                FileModeRule {
                    paths: vec!["/usr/bin/newgrp".into()],
                    mode: None,
                    clear: Some("4000".into()),
                },
            ),
            NamedRule::login_shell(
                "disable-sync-login",
                LoginShellRule {
                    users: vec!["sync".into()],
                    shell: "/sbin/nologin".into(),
                },
            ),
            NamedRule::line_edit(
                "deny-root-login",
                LineEditRule {
                    path: "/etc/ssh/sshd_config".into(),
                    pattern: "^#?PermitRootLogin .*".into(),
                    replacement: "PermitRootLogin no".into(),
                },
            ),
            NamedRule::file(
                "motd",
                FileRule {
                    path: "/etc/motd".into(),
                    content: "hi\n".into(),
                    mode: "0644".into(),
                },
            ),
        ];
        let mut rules = rules;
        rules.push(NamedRule::script(
            "reload-sshd",
            crate::crd::ScriptRule {
                run: "systemctl reload sshd".into(),
                unless: Some("sshd -T | grep -q '^permitrootlogin no'".into()),
                timeout_seconds: 30,
            },
        ));
        let mut s = spec(rules);
        s.group = Some("os-vulnerability-hardening".into());
        assert_eq!(problems("mng", &s), Vec::<String>::new());
    }

    #[test]
    fn every_kind_of_problem_is_reported() {
        let rules = vec![
            NamedRule::file_mode(
                "both",
                FileModeRule {
                    paths: vec!["/home/*/.bashrc".into(), "relative".into()],
                    mode: Some("0640".into()),
                    clear: Some("4000".into()),
                },
            ),
            NamedRule::file_mode(
                "bad-mode",
                FileModeRule {
                    paths: vec![],
                    mode: Some("999".into()),
                    clear: None,
                },
            ),
            NamedRule::login_shell(
                "shell",
                LoginShellRule {
                    users: vec!["a:b".into()],
                    shell: "nologin".into(),
                },
            ),
            NamedRule::line_edit(
                "edit",
                LineEditRule {
                    path: "sshd_config".into(),
                    pattern: "(".into(),
                    replacement: String::new(),
                },
            ),
            NamedRule::file(
                "file",
                FileRule {
                    path: "/etc/".into(),
                    content: String::new(),
                    mode: "rw".into(),
                },
            ),
            NamedRule::file("file", FileRule::default()),
            NamedRule::file_mode("Not A Label", FileModeRule::default()),
            NamedRule {
                file: Some(FileRule::default()),
                ..NamedRule::line_edit("two-actions", LineEditRule::default())
            },
            NamedRule::script(
                "blank",
                crate::crd::ScriptRule {
                    run: " ".into(),
                    unless: Some(String::new()),
                    timeout_seconds: 0,
                },
            ),
        ];
        let mut s = spec(rules);
        s.interval_seconds = 1;
        s.group = Some("Hardening".into());
        let found = problems(&"n".repeat(64), &s);
        let all = found.join("\n");
        for needle in [
            "metadata.name must be at most 63",
            "spec.group must be a lowercase DNS label",
            "intervalSeconds must be at least 30",
            "rule \"both\" requires exactly one of mode and clear",
            "last segment only",
            "\"relative\" must be absolute",
            "rule \"bad-mode\" holds an invalid octal mode",
            "rule \"bad-mode\" paths must not be empty",
            "rule \"shell\" shell",
            "rule \"shell\" users hold",
            "rule \"edit\" path",
            "rule \"edit\" pattern is not a valid regular expression",
            "rule \"file\" path",
            "rule \"file\" holds an invalid octal mode",
            "rule \"file\" is defined more than once",
            "rules[6].name \"Not A Label\" must be a lowercase DNS label",
            "rule \"two-actions\" requires exactly one of fileMode",
            "rule \"blank\" run must not be empty",
            "rule \"blank\" unless must not be empty",
            "rule \"blank\" timeoutSeconds must be from 1 to 600",
        ] {
            assert!(all.contains(needle), "missing {needle:?} in:\n{all}");
        }
    }
}
