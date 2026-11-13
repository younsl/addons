//! Applies the rules of a `NodeConfiguration` to the host. Every rule compares
//! first and writes only on a difference, so a run on a configured node changes
//! nothing.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::time::Duration;

use regex::Regex;

use crate::hostfs::{Attributes, HostRoot, has_wildcard, open_entry, write_atomic};
use crate::script::Scripts;
#[cfg(test)]
use node_configuration_common::crd::FailurePolicy;
use node_configuration_common::crd::validate::parse_mode;
use node_configuration_common::crd::{
    FileModeRule, FileRule, LineEditRule, LoginShellRule, NamedRule, RuleAction, ScriptRule,
};

const PASSWD: &str = "/etc/passwd";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// The node drifted and was corrected.
    Changed,
    /// The node drifted and a dry run left it as it is.
    Pending,
    Unchanged,
    /// The target does not exist on this node.
    Skipped,
    Failed,
}

impl Outcome {
    /// How much an outcome matters when a rule produced several.
    const fn weight(self) -> u8 {
        match self {
            Self::Skipped => 0,
            Self::Unchanged => 1,
            Self::Changed => 2,
            Self::Pending => 3,
            Self::Failed => 4,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Changed => "changed",
            Self::Pending => "pending",
            Self::Unchanged => "unchanged",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Name of the rule the entry belongs to.
    pub rule: String,
    pub kind: &'static str,
    pub target: String,
    pub outcome: Outcome,
    pub detail: String,
    /// The failure policy of the rule is `Ignore`.
    pub ignore_failure: bool,
}

impl Entry {
    /// Whether the entry keeps the node from counting as configured.
    pub fn blocks(&self) -> bool {
        self.outcome == Outcome::Failed && !self.ignore_failure
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub entries: Vec<Entry>,
}

impl Report {
    fn push(
        &mut self,
        rule: &str,
        kind: &'static str,
        target: &str,
        result: io::Result<(Outcome, String)>,
    ) {
        let (outcome, detail) = result.unwrap_or_else(|e| (Outcome::Failed, e.to_string()));
        self.entries.push(Entry {
            rule: rule.to_string(),
            kind,
            target: target.to_string(),
            outcome,
            detail,
            ignore_failure: false,
        });
    }

    /// One result per rule, in rule order: the outcome that matters most
    /// among its targets and where it came from.
    pub fn by_rule(&self) -> Vec<(&str, Outcome, Option<String>)> {
        let mut rules: Vec<(&str, &Entry)> = Vec::new();
        for entry in &self.entries {
            match rules.last_mut() {
                Some((name, worst)) if *name == entry.rule => {
                    if entry.outcome.weight() > worst.outcome.weight() {
                        *worst = entry;
                    }
                }
                _ => rules.push((entry.rule.as_str(), entry)),
            }
        }
        rules
            .into_iter()
            .map(|(name, entry)| {
                let message = (entry.outcome.weight() >= Outcome::Changed.weight())
                    .then(|| format!("{} {}, {}", entry.kind, entry.target, entry.detail));
                (name, entry.outcome, message)
            })
            .collect()
    }

    pub fn count(&self, outcome: Outcome) -> usize {
        self.entries.iter().filter(|e| e.outcome == outcome).count()
    }

    /// Failures that are not ignored.
    pub fn blocking_failures(&self) -> usize {
        self.entries.iter().filter(|e| e.blocks()).count()
    }
}

fn skip_missing(result: io::Result<(Outcome, String)>) -> io::Result<(Outcome, String)> {
    match result {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Ok((Outcome::Skipped, "not found".to_string()))
        }
        other => other,
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

const fn drift(dry_run: bool) -> Outcome {
    if dry_run {
        Outcome::Pending
    } else {
        Outcome::Changed
    }
}

/// Runs the rules with scripts disabled.
#[cfg(test)]
pub fn apply(host: &HostRoot, rules: &[NamedRule], dry_run: bool) -> Report {
    apply_with(host, rules, dry_run, &Scripts::Disabled)
}

/// Runs the rules in list order. A dry run compares and reports without
/// writing.
pub fn apply_with(
    host: &HostRoot,
    rules: &[NamedRule],
    dry_run: bool,
    scripts: &Scripts,
) -> Report {
    let mut report = Report::default();
    for rule in rules {
        let name = rule.name.as_str();
        let first = report.entries.len();
        match rule.action() {
            Some(RuleAction::FileMode(spec)) => file_modes(host, name, spec, dry_run, &mut report),
            Some(RuleAction::LoginShell(spec)) => {
                login_shell(host, name, spec, dry_run, &mut report);
            }
            Some(RuleAction::LineEdit(spec)) => report.push(
                name,
                "lineEdit",
                &spec.path,
                skip_missing(line_edit(host, spec, dry_run)),
            ),
            Some(RuleAction::File(spec)) => {
                report.push(name, "file", &spec.path, file(host, spec, dry_run));
            }
            Some(RuleAction::Script(spec)) => {
                report.push(name, "script", "run", script(spec, dry_run, scripts));
            }
            None => report.push(
                name,
                "rule",
                name,
                Err(invalid("exactly one action is required")),
            ),
        }
        for entry in &mut report.entries[first..] {
            entry.ignore_failure = rule.ignores_failure();
        }
    }
    report
}

fn file_modes(
    host: &HostRoot,
    name: &str,
    rule: &FileModeRule,
    dry_run: bool,
    report: &mut Report,
) {
    const KIND: &str = "fileMode";
    for pattern in &rule.paths {
        if !has_wildcard(pattern) {
            report.push(
                name,
                KIND,
                pattern,
                skip_missing(file_mode(host, rule, pattern, dry_run)),
            );
            continue;
        }
        match host.glob(pattern) {
            Ok(matches) if matches.is_empty() => {
                report.push(
                    name,
                    KIND,
                    pattern,
                    Ok((Outcome::Skipped, "no match".to_string())),
                );
            }
            Ok(matches) => {
                for path in matches {
                    report.push(
                        name,
                        KIND,
                        &path,
                        skip_missing(file_mode(host, rule, &path, dry_run)),
                    );
                }
            }
            Err(e) => report.push(name, KIND, pattern, Err(e)),
        }
    }
}

fn file_mode(
    host: &HostRoot,
    rule: &FileModeRule,
    path: &str,
    dry_run: bool,
) -> io::Result<(Outcome, String)> {
    let file = open_entry(&host.resolve(path)?)?;
    let current = file.metadata()?.mode() & 0o7777;
    let parse = |mode: &str| parse_mode(mode).ok_or_else(|| invalid("invalid octal mode"));
    let wanted = match (&rule.mode, &rule.clear) {
        (Some(mode), None) => parse(mode)?,
        (None, Some(clear)) => current & !parse(clear)?,
        _ => return Err(invalid("exactly one of mode and clear is required")),
    };
    if wanted == current {
        return Ok((Outcome::Unchanged, format!("{current:04o}")));
    }
    if !dry_run {
        file.set_permissions(fs::Permissions::from_mode(wanted))?;
    }
    Ok((drift(dry_run), format!("{current:04o} to {wanted:04o}")))
}

/// Rewrites the shell field of the named accounts, returning the new content
/// and what happened to each requested account.
fn rewrite_passwd(
    content: &str,
    rules: &[LoginShellRule],
) -> (String, Vec<(String, Outcome, String)>) {
    let wanted: Vec<(&str, &str)> = rules
        .iter()
        .flat_map(|r| r.users.iter().map(|u| (u.as_str(), r.shell.as_str())))
        .collect();
    let mut seen = vec![false; wanted.len()];
    let mut results = Vec::new();
    let mut out = String::with_capacity(content.len());
    for line in content.split_inclusive('\n') {
        let (body, ending) = line
            .strip_suffix('\n')
            .map_or((line, ""), |body| (body, "\n"));
        let fields: Vec<&str> = body.split(':').collect();
        let hit = (fields.len() == 7)
            .then(|| wanted.iter().rposition(|(user, _)| *user == fields[0]))
            .flatten();
        let Some(index) = hit else {
            out.push_str(line);
            continue;
        };
        let (user, shell) = wanted[index];
        for (i, (other, _)) in wanted.iter().enumerate() {
            seen[i] |= *other == user;
        }
        if fields[6] == shell {
            results.push((user.to_string(), Outcome::Unchanged, shell.to_string()));
            out.push_str(line);
        } else {
            results.push((
                user.to_string(),
                Outcome::Changed,
                format!("{} to {shell}", fields[6]),
            ));
            out.push_str(&fields[..6].join(":"));
            out.push(':');
            out.push_str(shell);
            out.push_str(ending);
        }
    }
    for (i, (user, _)) in wanted.iter().enumerate() {
        if !seen[i] && !results.iter().any(|(u, ..)| u == user) {
            results.push((
                (*user).to_string(),
                Outcome::Skipped,
                "no such account".to_string(),
            ));
        }
    }
    (out, results)
}

fn login_shell(
    host: &HostRoot,
    name: &str,
    rule: &LoginShellRule,
    dry_run: bool,
    report: &mut Report,
) {
    const KIND: &str = "loginShell";
    let outcome = (|| {
        let real = host.resolve(PASSWD)?;
        let content = fs::read_to_string(&real)?;
        let (updated, results) = rewrite_passwd(&content, std::slice::from_ref(rule));
        if updated != content && !dry_run {
            write_atomic(
                &real,
                updated.as_bytes(),
                Attributes::of(&fs::metadata(&real)?),
            )?;
        }
        Ok(results)
    })();
    match outcome {
        Ok(results) => {
            for (user, outcome, detail) in results {
                let outcome = if outcome == Outcome::Changed {
                    drift(dry_run)
                } else {
                    outcome
                };
                report.push(name, KIND, &user, Ok((outcome, detail)));
            }
        }
        Err(e) => report.push(name, KIND, PASSWD, Err(e)),
    }
}

/// Replaces the first match on each line, line breaks untouched.
fn edit_lines(content: &str, pattern: &Regex, replacement: &str) -> (String, usize) {
    let mut changed = 0;
    let mut out = String::with_capacity(content.len());
    for line in content.split_inclusive('\n') {
        let (body, ending) = line
            .strip_suffix('\n')
            .map_or((line, ""), |body| (body, "\n"));
        let edited = pattern.replace(body, replacement);
        if edited != body {
            changed += 1;
        }
        out.push_str(&edited);
        out.push_str(ending);
    }
    (out, changed)
}

fn line_edit(host: &HostRoot, rule: &LineEditRule, dry_run: bool) -> io::Result<(Outcome, String)> {
    let pattern = Regex::new(&rule.pattern).map_err(|_| invalid("invalid regular expression"))?;
    let real = host.resolve(&rule.path)?;
    let content = fs::read_to_string(&real)?;
    let (updated, changed) = edit_lines(&content, &pattern, &rule.replacement);
    if changed == 0 {
        return Ok((Outcome::Unchanged, String::new()));
    }
    if !dry_run {
        write_atomic(
            &real,
            updated.as_bytes(),
            Attributes::of(&fs::metadata(&real)?),
        )?;
    }
    Ok((drift(dry_run), format!("{changed} lines")))
}

fn file(host: &HostRoot, rule: &FileRule, dry_run: bool) -> io::Result<(Outcome, String)> {
    let mode = parse_mode(&rule.mode).ok_or_else(|| invalid("invalid octal mode"))?;
    let real = host.resolve_entry(&rule.path)?;
    let existing = fs::symlink_metadata(&real)
        .ok()
        .filter(fs::Metadata::is_file);
    let detail = match &existing {
        Some(meta) if fs::read(&real)? == rule.content.as_bytes() => {
            let current = meta.mode() & 0o7777;
            if current == mode {
                return Ok((Outcome::Unchanged, String::new()));
            }
            format!("mode {current:04o} to {mode:04o}")
        }
        Some(_) => "content".to_string(),
        None => "created".to_string(),
    };
    let attributes = Attributes {
        mode,
        owner: existing.as_ref().map(|meta| (meta.uid(), meta.gid())),
    };
    if !dry_run {
        write_atomic(&real, rule.content.as_bytes(), attributes)?;
    }
    Ok((drift(dry_run), detail))
}

fn script(rule: &ScriptRule, dry_run: bool, scripts: &Scripts) -> io::Result<(Outcome, String)> {
    let timeout = Duration::from_secs(u64::from(rule.timeout_seconds));
    if let Some(unless) = &rule.unless
        && scripts.run(unless, timeout)?.success
    {
        return Ok((Outcome::Unchanged, "unless passed".to_string()));
    }
    if dry_run {
        return Ok((Outcome::Pending, "would run".to_string()));
    }
    let finished = scripts.run(&rule.run, timeout)?;
    if finished.success {
        Ok((Outcome::Changed, "ran".to_string()))
    } else {
        Err(io::Error::other(finished.detail))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    const PASSWD_FILE: &str = "root:x:0:0:root:/root:/bin/bash\nsync:x:5:0:sync:/sbin:/bin/sync\nshutdown:x:6:0:shutdown:/sbin:/sbin/shutdown\nhalt:x:7:0:halt:/sbin:/sbin/halt\nbroken line\n";
    const SSHD: &str = "#PermitRootLogin prohibit-password\nPort 22\n#Banner none\n";
    const PAM_SU: &str =
        "auth\t\tsufficient\tpam_rootok.so\n#auth\t\trequired\tpam_wheel.so use_uid\n";
    const BANNER: &str =
        "WARNING: Authorized users only. All activities are monitored and recorded.\n";

    fn write(root: &Path, path: &str, content: &str, mode: u32) {
        let real = root.join(path.trim_start_matches('/'));
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, content).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn mode(root: &Path, path: &str) -> u32 {
        fs::metadata(root.join(path.trim_start_matches('/')))
            .unwrap()
            .mode()
            & 0o7777
    }

    fn read(root: &Path, path: &str) -> String {
        fs::read_to_string(root.join(path.trim_start_matches('/'))).unwrap()
    }

    fn node() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "/root/.bashrc", "", 0o644);
        write(root, "/root/.bash_profile", "", 0o644);
        write(root, "/root/.cshrc", "", 0o644);
        write(root, "/home/ec2-user/.bashrc", "", 0o644);
        write(root, "/usr/bin/newgrp", "", 0o4755);
        write(root, "/usr/bin/unix_chkpwd", "", 0o4755);
        write(root, "/usr/bin/gcc", "", 0o755);
        write(root, "/usr/bin/su", "", 0o4755);
        symlink("bin", root.join("usr/sbin")).unwrap();
        write(root, "/etc/passwd", PASSWD_FILE, 0o644);
        write(root, "/etc/ssh/sshd_config", SSHD, 0o600);
        write(root, "/etc/pam.d/su", PAM_SU, 0o644);
        symlink("/run/motd", root.join("etc/motd")).unwrap();
        dir
    }

    fn mode_rule(paths: &[&str], mode: Option<&str>, clear: Option<&str>) -> FileModeRule {
        FileModeRule {
            paths: paths.iter().map(ToString::to_string).collect(),
            mode: mode.map(str::to_string),
            clear: clear.map(str::to_string),
        }
    }

    fn edit(path: &str, pattern: &str, replacement: &str) -> LineEditRule {
        LineEditRule {
            path: path.to_string(),
            pattern: pattern.to_string(),
            replacement: replacement.to_string(),
        }
    }

    fn baseline() -> Vec<NamedRule> {
        let file = |name: &str, path: &str| {
            NamedRule::file(
                name,
                FileRule {
                    path: path.into(),
                    content: BANNER.into(),
                    mode: "0644".into(),
                },
            )
        };
        vec![
            NamedRule::file_mode(
                "restrict-shell-profiles",
                mode_rule(
                    &[
                        "/home/ec2-user/.bash*",
                        "/home/ssm-user/.bash*",
                        "/root/.bash*",
                        "/root/.cshrc",
                    ],
                    Some("0640"),
                    None,
                ),
            ),
            NamedRule::file_mode(
                "drop-setuid",
                mode_rule(
                    &["/usr/bin/newgrp", "/usr/sbin/unix_chkpwd"],
                    None,
                    Some("4000"),
                ),
            ),
            NamedRule::file_mode(
                "restrict-compilers",
                mode_rule(&["/usr/bin/gcc", "/usr/bin/cc"], None, Some("0001")),
            ),
            NamedRule::file_mode(
                "restrict-su",
                mode_rule(&["/usr/bin/su"], Some("4750"), None),
            ),
            NamedRule::login_shell(
                "disable-system-logins",
                LoginShellRule {
                    users: vec![
                        "sync".into(),
                        "shutdown".into(),
                        "halt".into(),
                        "ghost".into(),
                    ],
                    shell: "/sbin/nologin".into(),
                },
            ),
            NamedRule::line_edit(
                "deny-root-login",
                edit(
                    "/etc/ssh/sshd_config",
                    "^#?PermitRootLogin .*",
                    "PermitRootLogin no",
                ),
            ),
            NamedRule::line_edit(
                "require-wheel-for-su",
                edit(
                    "/etc/pam.d/su",
                    r"^#(auth\s*required\s*pam_wheel.so use_uid)",
                    "$1",
                ),
            ),
            NamedRule::line_edit(
                "ssh-banner",
                edit(
                    "/etc/ssh/sshd_config",
                    "^#?Banner .*",
                    "Banner /etc/issue.net",
                ),
            ),
            NamedRule::line_edit("missing-file", edit("/etc/missing.conf", "a", "b")),
            file("issue-net", "/etc/issue.net"),
            file("motd", "/etc/motd"),
        ]
    }

    #[test]
    fn a_dry_run_reports_every_drift_and_leaves_the_node_alone() {
        let dir = node();
        let root = dir.path();
        let host = HostRoot::new(root);

        let report = apply(&host, &baseline(), true);
        assert_eq!(report.count(Outcome::Failed), 0, "{report:?}");
        assert_eq!(report.count(Outcome::Changed), 0, "{report:?}");
        assert!(report.count(Outcome::Pending) > 0, "{report:?}");
        assert_eq!(mode(root, "/usr/bin/su"), 0o4755);
        assert_eq!(read(root, "/etc/passwd"), PASSWD_FILE);
        assert_eq!(read(root, "/etc/ssh/sshd_config"), SSHD);
        assert!(!root.join("etc/issue.net").exists());

        let real = apply(&host, &baseline(), false);
        assert_eq!(
            real.count(Outcome::Changed),
            report.count(Outcome::Pending),
            "{real:?}"
        );
        assert_eq!(apply(&host, &baseline(), true).count(Outcome::Pending), 0);
    }

    #[test]
    fn a_drifted_node_is_corrected_then_left_alone() {
        let dir = node();
        let root = dir.path();
        let host = HostRoot::new(root);

        let first = apply(&host, &baseline(), false);
        assert_eq!(first.count(Outcome::Failed), 0, "{first:#?}");
        assert_eq!(first.count(Outcome::Changed), 16, "{first:#?}");
        // ssm-user home, /usr/bin/cc, the ghost account, and /etc/missing.conf
        assert_eq!(first.count(Outcome::Skipped), 4, "{first:#?}");

        assert_eq!(mode(root, "/root/.bashrc"), 0o640);
        assert_eq!(mode(root, "/root/.bash_profile"), 0o640);
        assert_eq!(mode(root, "/root/.cshrc"), 0o640);
        assert_eq!(mode(root, "/home/ec2-user/.bashrc"), 0o640);
        assert_eq!(mode(root, "/usr/bin/newgrp"), 0o755);
        assert_eq!(mode(root, "/usr/bin/unix_chkpwd"), 0o755);
        assert_eq!(mode(root, "/usr/bin/gcc"), 0o754);
        assert_eq!(mode(root, "/usr/bin/su"), 0o4750);
        assert_eq!(
            read(root, "/etc/passwd"),
            "root:x:0:0:root:/root:/bin/bash\nsync:x:5:0:sync:/sbin:/sbin/nologin\nshutdown:x:6:0:shutdown:/sbin:/sbin/nologin\nhalt:x:7:0:halt:/sbin:/sbin/nologin\nbroken line\n"
        );
        assert_eq!(mode(root, "/etc/passwd"), 0o644);
        assert_eq!(
            read(root, "/etc/ssh/sshd_config"),
            "PermitRootLogin no\nPort 22\nBanner /etc/issue.net\n"
        );
        assert_eq!(mode(root, "/etc/ssh/sshd_config"), 0o600);
        assert_eq!(
            read(root, "/etc/pam.d/su"),
            "auth\t\tsufficient\tpam_rootok.so\nauth\t\trequired\tpam_wheel.so use_uid\n"
        );
        assert_eq!(read(root, "/etc/issue.net"), BANNER);
        assert!(
            fs::symlink_metadata(root.join("etc/motd"))
                .unwrap()
                .is_file()
        );
        assert_eq!(read(root, "/etc/motd"), BANNER);

        let second = apply(&host, &baseline(), false);
        assert_eq!(second.count(Outcome::Changed), 0, "{second:#?}");
        assert_eq!(second.count(Outcome::Failed), 0, "{second:#?}");
        assert_eq!(second.count(Outcome::Unchanged), 16, "{second:#?}");
    }

    #[test]
    fn drift_after_a_run_is_reverted() {
        let dir = node();
        let root = dir.path();
        let host = HostRoot::new(root);
        apply(&host, &baseline(), false);

        write(root, "/usr/bin/su", "", 0o4755);
        write(root, "/etc/motd", "welcome\n", 0o644);
        write(root, "/etc/issue.net", BANNER, 0o600);
        let report = apply(&host, &baseline(), false);
        let changed: Vec<(&str, &str)> = report
            .entries
            .iter()
            .filter(|e| e.outcome == Outcome::Changed)
            .map(|e| (e.target.as_str(), e.detail.as_str()))
            .collect();
        assert_eq!(
            changed,
            vec![
                ("/usr/bin/su", "4755 to 4750"),
                ("/etc/issue.net", "mode 0600 to 0644"),
                ("/etc/motd", "content"),
            ]
        );
    }

    #[test]
    fn failures_are_reported_per_rule() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "/etc/binary", "", 0o644);
        fs::write(root.join("etc/binary"), [0xff, 0xfe]).unwrap();
        write(root, "/etc/hosts", "x\n", 0o644);
        let rules = vec![
            NamedRule::file_mode(
                "both",
                mode_rule(&["/etc/hosts"], Some("0640"), Some("0001")),
            ),
            NamedRule::file_mode("bad-mode", mode_rule(&["/etc/hosts"], Some("9"), None)),
            NamedRule::login_shell(
                "no-passwd",
                LoginShellRule {
                    users: vec!["sync".into()],
                    shell: "/sbin/nologin".into(),
                },
            ),
            NamedRule::line_edit("bad-pattern", edit("/etc/hosts", "(", "")),
            NamedRule::line_edit("binary", edit("/etc/binary", "a", "b")),
            NamedRule::file(
                "no-directory",
                FileRule {
                    path: "/missing/dir/file".into(),
                    content: String::new(),
                    mode: "0644".into(),
                },
            ),
            NamedRule::file(
                "bad-file-mode",
                FileRule {
                    path: "/etc/hosts".into(),
                    content: String::new(),
                    mode: "rw".into(),
                },
            ),
            NamedRule::default(),
            NamedRule {
                file: Some(FileRule::default()),
                ..NamedRule::line_edit("two-actions", edit("/etc/hosts", "x", "y"))
            },
        ];
        let report = apply(&HostRoot::new(root), &rules, false);
        assert_eq!(report.count(Outcome::Failed), 9, "{report:#?}");
        assert_eq!(report.entries[2].target, PASSWD);
        assert_eq!(report.entries[2].rule, "no-passwd");
        assert_eq!(report.entries[8].kind, "rule");
        assert_eq!(read(root, "/etc/hosts"), "x\n");
    }

    #[test]
    fn passwd_rewrite_keeps_other_lines_and_a_missing_final_newline() {
        let rules = [
            LoginShellRule {
                users: vec!["halt".into()],
                shell: "/bin/false".into(),
            },
            LoginShellRule {
                users: vec!["halt".into(), "nobody".into()],
                shell: "/sbin/nologin".into(),
            },
        ];
        let (out, results) = rewrite_passwd(
            "a:x:1:1::/:/bin/sh\nhalt:x:7:0:halt:/sbin:/sbin/halt",
            &rules,
        );
        assert_eq!(
            out,
            "a:x:1:1::/:/bin/sh\nhalt:x:7:0:halt:/sbin:/sbin/nologin"
        );
        assert_eq!(
            results,
            vec![
                (
                    "halt".to_string(),
                    Outcome::Changed,
                    "/sbin/halt to /sbin/nologin".to_string()
                ),
                (
                    "nobody".to_string(),
                    Outcome::Skipped,
                    "no such account".to_string()
                ),
            ]
        );
    }

    fn marker(root: &Path, unless: bool) -> Vec<NamedRule> {
        let file = root.join("marker");
        vec![NamedRule::script(
            "touch-marker",
            ScriptRule {
                run: format!("echo ran >> {}", file.display()),
                unless: unless.then(|| format!("test -f {}", file.display())),
                timeout_seconds: 10,
            },
        )]
    }

    #[test]
    fn a_script_runs_until_its_check_passes() {
        let dir = tempfile::tempdir().unwrap();
        let host = HostRoot::new(dir.path());
        let rules = marker(dir.path(), true);

        let dry = apply_with(&host, &rules, true, &Scripts::Local);
        assert_eq!(dry.count(Outcome::Pending), 1, "{dry:?}");
        assert!(!dir.path().join("marker").exists());

        let first = apply_with(&host, &rules, false, &Scripts::Local);
        assert_eq!(first.count(Outcome::Changed), 1, "{first:?}");
        assert_eq!(first.entries[0].kind, "script");
        let second = apply_with(&host, &rules, false, &Scripts::Local);
        assert_eq!(second.count(Outcome::Unchanged), 1, "{second:?}");
        assert_eq!(apply_with(&host, &rules, true, &Scripts::Local), second);
        assert_eq!(read(dir.path(), "/marker"), "ran\n");
    }

    #[test]
    fn a_script_without_a_check_runs_every_time() {
        let dir = tempfile::tempdir().unwrap();
        let host = HostRoot::new(dir.path());
        let rules = marker(dir.path(), false);
        for _ in 0..2 {
            let report = apply_with(&host, &rules, false, &Scripts::Local);
            assert_eq!(report.count(Outcome::Changed), 1, "{report:?}");
        }
        assert_eq!(read(dir.path(), "/marker"), "ran\nran\n");
    }

    #[test]
    fn a_failing_or_disabled_script_fails_its_rule() {
        let dir = tempfile::tempdir().unwrap();
        let host = HostRoot::new(dir.path());
        let rules = vec![NamedRule::script(
            "broken",
            ScriptRule {
                run: "echo nope >&2; exit 2".into(),
                ..ScriptRule::default()
            },
        )];
        let report = apply_with(&host, &rules, false, &Scripts::Local);
        assert_eq!(report.entries[0].outcome, Outcome::Failed);
        assert_eq!(report.entries[0].detail, "exit code 2, nope");

        let disabled = apply(&host, &rules, false);
        assert_eq!(disabled.entries[0].outcome, Outcome::Failed);
        assert!(disabled.entries[0].detail.contains("agent.allowScripts"));
    }

    #[test]
    fn an_ignored_failure_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let host = HostRoot::new(dir.path());
        let missing = |name: &str| {
            NamedRule::file(
                name,
                FileRule {
                    path: format!("/missing/{name}"),
                    content: String::new(),
                    mode: "0644".into(),
                },
            )
        };
        let rules = vec![
            NamedRule {
                failure_policy: FailurePolicy::Ignore,
                ..missing("nice-to-have")
            },
            missing("must-have"),
        ];
        let report = apply(&host, &rules, false);
        assert_eq!(report.count(Outcome::Failed), 2, "{report:?}");
        assert_eq!(report.blocking_failures(), 1);
        assert!(report.entries[0].ignore_failure && !report.entries[0].blocks());
        assert!(report.entries[1].blocks());
        assert_eq!(report.by_rule()[0].1, Outcome::Failed);

        let report = apply(&host, &rules[..1], false);
        assert_eq!(report.blocking_failures(), 0);
    }

    #[test]
    fn a_rule_is_summarized_by_its_worst_target() {
        let dir = node();
        let host = HostRoot::new(dir.path());
        let first = apply(&host, &baseline(), false);
        let summary = first.by_rule();
        assert_eq!(summary.len(), baseline().len());
        assert_eq!(
            summary[0],
            (
                "restrict-shell-profiles",
                Outcome::Changed,
                Some("fileMode /home/ec2-user/.bashrc, 0644 to 0640".to_string())
            )
        );
        assert_eq!(summary[8], ("missing-file", Outcome::Skipped, None));

        let second = apply(&host, &baseline(), false);
        assert!(
            second
                .by_rule()
                .iter()
                .all(|(_, outcome, message)| outcome.weight() <= 1 && message.is_none())
        );
    }

    #[test]
    fn outcomes_have_metric_labels() {
        let labels: Vec<&str> = [
            Outcome::Changed,
            Outcome::Unchanged,
            Outcome::Skipped,
            Outcome::Failed,
        ]
        .into_iter()
        .map(Outcome::as_str)
        .collect();
        assert_eq!(labels, ["changed", "unchanged", "skipped", "failed"]);
    }
}
