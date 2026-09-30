//! External command execution. The pipeline talks to git, make and docker only
//! through [`Runner`], so tests replace it with a recording fake.

use std::fmt;
use std::process::{Command, Stdio};

use tracing::info;

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
}

impl Cmd {
    pub fn new(program: &str) -> Self {
        Self {
            program: program.to_string(),
            args: Vec::new(),
        }
    }

    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.program)?;
        for arg in &self.args {
            if arg.contains(char::is_whitespace) {
                write!(f, " '{arg}'")?;
            } else {
                write!(f, " {arg}")?;
            }
        }
        Ok(())
    }
}

pub trait Runner {
    /// Run with inherited stdio so long builds stream their logs.
    fn run(&self, cmd: &Cmd) -> Result<()>;
    /// Run and return trimmed stdout.
    fn output(&self, cmd: &Cmd) -> Result<String>;
}

pub struct SystemRunner;

impl SystemRunner {
    fn command(cmd: &Cmd) -> Command {
        let mut command = Command::new(&cmd.program);
        command.args(&cmd.args);
        command
    }

    fn check(cmd: &Cmd, status: std::process::ExitStatus) -> Result<()> {
        if status.success() {
            Ok(())
        } else {
            Err(Error::CommandFailed {
                cmd: cmd.to_string(),
                status: status.to_string(),
            })
        }
    }
}

impl Runner for SystemRunner {
    fn run(&self, cmd: &Cmd) -> Result<()> {
        let status = Self::command(cmd)
            .status()
            .map_err(|source| Error::CommandSpawn {
                cmd: cmd.to_string(),
                source,
            })?;
        Self::check(cmd, status)
    }

    fn output(&self, cmd: &Cmd) -> Result<String> {
        let out = Self::command(cmd)
            .stderr(Stdio::inherit())
            .output()
            .map_err(|source| Error::CommandSpawn {
                cmd: cmd.to_string(),
                source,
            })?;
        Self::check(cmd, out.status)?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

/// Logs every command and runs none.
pub struct DryRunner;

impl Runner for DryRunner {
    fn run(&self, cmd: &Cmd) -> Result<()> {
        info!("+ {cmd}");
        Ok(())
    }

    fn output(&self, cmd: &Cmd) -> Result<String> {
        info!("+ {cmd}");
        Ok(String::new())
    }
}

#[cfg(test)]
pub mod fake {
    use std::cell::RefCell;

    use super::{Cmd, Runner};
    use crate::error::{Error, Result};

    type Handler = Box<dyn Fn(&Cmd) -> Option<Result<String>>>;

    /// Records every command. Handlers are tried in order, and the first one that
    /// returns `Some` decides the result, otherwise the command succeeds with
    /// empty output.
    #[derive(Default)]
    pub struct FakeRunner {
        pub calls: RefCell<Vec<String>>,
        handlers: Vec<Handler>,
    }

    impl FakeRunner {
        pub fn on(mut self, handler: impl Fn(&Cmd) -> Option<Result<String>> + 'static) -> Self {
            self.handlers.push(Box::new(handler));
            self
        }

        /// Fail every command whose rendering starts with `prefix`.
        pub fn fail_on(self, prefix: &'static str) -> Self {
            self.on(move |cmd| {
                cmd.to_string().starts_with(prefix).then(|| {
                    Err(Error::CommandFailed {
                        cmd: cmd.to_string(),
                        status: "exit status: 1".to_string(),
                    })
                })
            })
        }

        pub fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        fn dispatch(&self, cmd: &Cmd) -> Result<String> {
            self.calls.borrow_mut().push(cmd.to_string());
            self.handlers
                .iter()
                .find_map(|h| h(cmd))
                .unwrap_or_else(|| Ok(String::new()))
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, cmd: &Cmd) -> Result<()> {
            self.dispatch(cmd).map(|_| ())
        }

        fn output(&self, cmd: &Cmd) -> Result<String> {
            self.dispatch(cmd)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_quotes_whitespace() {
        let cmd = Cmd::new("make")
            .arg("build")
            .args(["GOBUILDTAGS=include_oss include_gcs"]);
        assert_eq!(
            cmd.to_string(),
            "make build 'GOBUILDTAGS=include_oss include_gcs'"
        );
    }

    #[test]
    fn system_runner_captures_stdout() {
        let out = SystemRunner
            .output(&Cmd::new("sh").args(["-c", "echo ' hello '"]))
            .unwrap();
        assert_eq!(out, "hello");
    }

    #[test]
    fn system_runner_run_succeeds() {
        SystemRunner.run(&Cmd::new("true")).unwrap();
    }

    #[test]
    fn system_runner_reports_failure() {
        let err = SystemRunner.run(&Cmd::new("false")).unwrap_err();
        assert!(matches!(err, Error::CommandFailed { .. }));
        let err = SystemRunner.output(&Cmd::new("false")).unwrap_err();
        assert!(matches!(err, Error::CommandFailed { .. }));
    }

    #[test]
    fn system_runner_reports_missing_program() {
        let cmd = Cmd::new("definitely-not-a-real-program-xyz");
        assert!(matches!(
            SystemRunner.run(&cmd).unwrap_err(),
            Error::CommandSpawn { .. }
        ));
        assert!(matches!(
            SystemRunner.output(&cmd).unwrap_err(),
            Error::CommandSpawn { .. }
        ));
    }

    #[test]
    fn dry_runner_runs_nothing() {
        let cmd = Cmd::new("false");
        DryRunner.run(&cmd).unwrap();
        assert_eq!(DryRunner.output(&cmd).unwrap(), "");
    }
}
