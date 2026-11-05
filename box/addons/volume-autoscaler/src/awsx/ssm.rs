//! Runs shell scripts on target instances via SSM Run Command.

use std::time::{Duration, Instant};

use aws_sdk_ssm::types::CommandInvocationStatus;

use super::{ApiError, Clients, CommandResult};
use crate::humanize::go_duration;

/// The AWS-managed document for ad-hoc shell execution.
const RUN_SHELL_DOCUMENT: &str = "AWS-RunShellScript";

impl Clients {
    /// Runs a shell script on the instance via SSM `RunShellScript` and polls
    /// until the invocation reaches a terminal state or the timeout elapses.
    pub async fn run_script(
        &self,
        instance_id: &str,
        script: &str,
        timeout: Duration,
    ) -> Result<CommandResult, ApiError> {
        let timeout_secs = i32::try_from(timeout.as_secs()).unwrap_or(i32::MAX);
        let send = self
            .ssm
            .send_command(instance_id, RUN_SHELL_DOCUMENT, timeout_secs, script)
            .await
            .map_err(|e| ApiError::new(format!("send command to {instance_id}: {}", e.message)))?;
        let command_id = send
            .command()
            .and_then(|c| c.command_id())
            .unwrap_or_default()
            .to_string();

        let deadline = Instant::now() + timeout;
        loop {
            tokio::time::sleep(self.poll_interval()).await;

            let inv = match self
                .ssm
                .get_command_invocation(&command_id, instance_id)
                .await
            {
                Ok(inv) => inv,
                Err(e) => {
                    // The invocation may not be registered immediately after
                    // SendCommand.
                    if Instant::now() > deadline {
                        return Err(ApiError::new(format!(
                            "get command invocation {command_id}: {}",
                            e.message
                        )));
                    }
                    continue;
                }
            };

            if let Some(status) = inv.status()
                && matches!(
                    status,
                    CommandInvocationStatus::Success
                        | CommandInvocationStatus::Failed
                        | CommandInvocationStatus::Cancelled
                        | CommandInvocationStatus::TimedOut
                )
            {
                let res = CommandResult {
                    status: status.as_str().to_string(),
                    exit_code: inv.response_code(),
                    stdout: inv
                        .standard_output_content()
                        .unwrap_or_default()
                        .to_string(),
                    stderr: inv.standard_error_content().unwrap_or_default().to_string(),
                };
                if *status != CommandInvocationStatus::Success {
                    return Err(ApiError::new(format!(
                        "command {command_id} on {instance_id} ended with status {} (exit {}): {}",
                        res.status, res.exit_code, res.stderr
                    )));
                }
                return Ok(res);
            }

            if Instant::now() > deadline {
                return Err(ApiError::new(format!(
                    "command {command_id} on {instance_id} did not finish within {}",
                    go_duration(timeout)
                )));
            }
        }
    }
}

#[cfg(test)]
#[allow(unused_variables)]
mod tests {
    use super::super::fake::{self, FakeEc2, FakeSsm, clients};
    use super::*;

    #[tokio::test]
    async fn run_script_success_after_polling() {
        let ssm = FakeSsm::default();
        *ssm.invocations.lock().unwrap() = vec![
            fake::invocation(CommandInvocationStatus::Pending, 0, "", ""),
            fake::invocation(CommandInvocationStatus::InProgress, 0, "", ""),
            fake::invocation(CommandInvocationStatus::Success, 0, "42\n", ""),
        ];
        let (c, _, ssm) = clients(FakeEc2::default(), ssm);
        let res = c
            .run_script("i-1", "echo 42", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(res.status, "Success");
        assert_eq!(res.stdout, "42\n");
        let sent = ssm.sent.lock().unwrap();
        assert_eq!(
            sent[0],
            (
                "i-1".into(),
                "AWS-RunShellScript".into(),
                5,
                "echo 42".into()
            )
        );
    }

    #[tokio::test]
    async fn run_script_failure_statuses() {
        let ssm = FakeSsm::default();
        *ssm.invocations.lock().unwrap() = vec![fake::invocation(
            CommandInvocationStatus::Failed,
            1,
            "",
            "no space",
        )];
        let (c, _, ssm) = clients(FakeEc2::default(), ssm);
        let err = c
            .run_script("i-1", "x", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(
            err.message,
            "command cmd-1 on i-1 ended with status Failed (exit 1): no space"
        );

        let ssm = FakeSsm::default();
        *ssm.send_error.lock().unwrap() = Some(ApiError::new("InvalidInstanceId"));
        let (c, _, ssm) = clients(FakeEc2::default(), ssm);
        let err = c
            .run_script("i-1", "x", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(err.message, "send command to i-1: InvalidInstanceId");
    }

    #[tokio::test]
    async fn run_script_times_out() {
        let ssm = FakeSsm::default();
        *ssm.invocations.lock().unwrap() = vec![fake::invocation(
            CommandInvocationStatus::InProgress,
            0,
            "",
            "",
        )];
        let (c, _, ssm) = clients(FakeEc2::default(), ssm);
        let err = c
            .run_script("i-1", "x", Duration::from_millis(10))
            .await
            .unwrap_err();
        assert!(err.message.contains("did not finish within 10ms"), "{err}");

        // An invocation that never registers times out too.
        let ssm = FakeSsm::default();
        *ssm.invocation_error.lock().unwrap() = Some(ApiError::new("InvocationDoesNotExist"));
        let (c, _, ssm) = clients(FakeEc2::default(), ssm);
        let err = c
            .run_script("i-1", "x", Duration::from_millis(10))
            .await
            .unwrap_err();
        assert!(
            err.message
                .contains("get command invocation cmd-1: InvocationDoesNotExist"),
            "{err}"
        );
    }
}
