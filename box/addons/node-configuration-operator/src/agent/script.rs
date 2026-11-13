//! Runs the `script` action on the host. The image has no shell, so the agent
//! starts itself again as a helper that changes its root to the host mount and
//! replaces itself with the host shell.

use std::io::{self, Read as _};
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const HELPER_COMMAND: &str = "host-exec";
const SHELL: &str = "/bin/sh";
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const POLL: Duration = Duration::from_millis(50);
const STDERR_TAIL: usize = 300;

/// How scripts reach a shell.
#[derive(Clone, Debug)]
pub enum Scripts {
    /// The agent runs without the privileges a script needs.
    Disabled,
    /// Through the helper, inside the host root at `root`.
    Host { exe: PathBuf, root: PathBuf },
    /// Straight through the local shell.
    #[cfg(test)]
    Local,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Finished {
    pub success: bool,
    /// The exit status and the end of the error output.
    pub detail: String,
}

impl Scripts {
    pub fn host(root: impl Into<PathBuf>) -> io::Result<Self> {
        Ok(Self::Host {
            exe: std::env::current_exe()?,
            root: root.into(),
        })
    }

    fn command(&self, script: &str) -> io::Result<Command> {
        let mut command = match self {
            Self::Disabled => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "scripts are disabled on this agent, set agent.allowScripts in the chart",
                ));
            }
            Self::Host { exe, root } => {
                let mut command = Command::new(exe);
                command
                    .arg(HELPER_COMMAND)
                    .arg("--root")
                    .arg(root)
                    .arg("--script")
                    .arg(script);
                command
            }
            #[cfg(test)]
            Self::Local => {
                let mut command = Command::new(SHELL);
                command.arg("-c").arg(script);
                command
            }
        };
        command
            .env_clear()
            .env("PATH", PATH)
            .env("HOME", "/root")
            .env("LANG", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        Ok(command)
    }

    /// Runs `script` to its end, or kills the shell after `timeout`.
    pub fn run(&self, script: &str, timeout: Duration) -> io::Result<Finished> {
        let mut child = self.command(script)?.spawn()?;
        let stderr = child.stderr.take();
        let reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut stderr) = stderr {
                let _ = stderr.read_to_end(&mut bytes);
            }
            String::from_utf8_lossy(&bytes).trim().to_string()
        });

        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out after {} seconds", timeout.as_secs()),
                ));
            }
            thread::sleep(POLL);
        };
        let errors = reader.join().unwrap_or_default();
        let tail: String = errors
            .chars()
            .rev()
            .take(STDERR_TAIL)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let code = status.code().map_or_else(
            || "killed by a signal".to_string(),
            |c| format!("exit code {c}"),
        );
        Ok(Finished {
            success: status.success(),
            detail: if tail.is_empty() {
                code
            } else {
                format!("{code}, {tail}")
            },
        })
    }
}

/// The helper: enters the host root and becomes the host shell. It returns
/// only when that failed.
pub fn host_exec(root: &str, script: &str) -> io::Error {
    if let Err(e) = std::os::unix::fs::chroot(root) {
        return e;
    }
    if let Err(e) = std::env::set_current_dir("/") {
        return e;
    }
    Command::new(SHELL).arg("-c").arg(script).exec()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: Duration = Duration::from_secs(10);

    #[test]
    fn a_script_reports_its_exit_status_and_error_output() {
        let ok = Scripts::Local.run("true", SHORT).unwrap();
        assert_eq!(
            ok,
            Finished {
                success: true,
                detail: "exit code 0".into()
            }
        );
        let failed = Scripts::Local
            .run("echo hidden; echo broken >&2; exit 3", SHORT)
            .unwrap();
        assert_eq!(
            failed,
            Finished {
                success: false,
                detail: "exit code 3, broken".into()
            }
        );
    }

    #[test]
    fn the_environment_is_fixed() {
        let run = Scripts::Local
            .run("echo \"$PATH $HOME $NODE_NAME.\" >&2", SHORT)
            .unwrap();
        assert_eq!(run.detail, format!("exit code 0, {PATH} /root ."));
    }

    #[test]
    fn a_script_is_killed_at_its_timeout() {
        let err = Scripts::Local
            .run("sleep 30", Duration::from_millis(200))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn scripts_can_be_disabled() {
        let err = Scripts::Disabled.run("true", SHORT).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn the_helper_command_carries_the_root_and_the_script() {
        let scripts = Scripts::host("/host").unwrap();
        let command = scripts.command("echo hi").unwrap();
        let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy()).collect();
        assert_eq!(
            args,
            [HELPER_COMMAND, "--root", "/host", "--script", "echo hi"]
        );
    }

    #[test]
    fn the_helper_fails_without_a_host_root() {
        let err = host_exec("/nonexistent/host/root", "true");
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ),
            "{err}"
        );
    }
}
