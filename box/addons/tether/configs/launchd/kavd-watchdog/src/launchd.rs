use std::process::ExitStatus;

use tokio::process::Command;

const LAUNCHCTL: &str = "/bin/launchctl";

#[derive(Debug, thiserror::Error)]
pub enum LaunchdError {
    #[error("failed to run {LAUNCHCTL}: {0}")]
    Spawn(std::io::Error),
    #[error("launchctl kickstart {service} exited with {status}: {stderr}")]
    Kickstart {
        service: String,
        status: ExitStatus,
        stderr: String,
    },
}

pub async fn kickstart(service: &str) -> Result<(), LaunchdError> {
    let output = Command::new(LAUNCHCTL)
        .args(["kickstart", "-k", service])
        .output()
        .await
        .map_err(LaunchdError::Spawn)?;

    if output.status.success() {
        return Ok(());
    }

    Err(LaunchdError::Kickstart {
        service: service.to_owned(),
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}
