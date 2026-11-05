use std::net::IpAddr;
use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::api::AttachParams;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use super::error::K8sError;
use crate::procnet;

const TCP_TABLES: [&str; 3] = ["cat", "/proc/net/tcp", "/proc/net/tcp6"];

struct Output {
    stdout: Vec<u8>,
    stderr: String,
    status: Option<String>,
}

impl Output {
    fn reason(self) -> String {
        Some(self.stderr.trim().to_owned())
            .filter(|s| !s.is_empty())
            .or(self.status)
            .unwrap_or_else(|| "empty output".into())
    }
}

/// Reads the pod's TCP tables, trying each container until one has `cat`.
pub async fn read_tcp_tables(
    api: &Api<Pod>,
    pod: &str,
    containers: &[String],
    timeout: Duration,
) -> Result<String, K8sError> {
    let command: Vec<String> = TCP_TABLES.iter().map(|s| (*s).to_owned()).collect();
    first_success(containers, timeout, |container| {
        let command = command.clone();
        async move {
            let out = run(api, pod, container.clone(), command, None).await?;
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            // tcp6 is missing when IPv6 is disabled, so cat exits non-zero while tcp is complete.
            if procnet::has_table(&text) {
                Ok(text)
            } else {
                Err(K8sError::NoTable(format!("{container}: {}", out.reason())))
            }
        }
    })
    .await
}

/// Sends `payload` to `host:port` with `nc` and returns the raw reply. The server closes the
/// socket after the trailing `QUIT`, which ends the command.
pub async fn tunnel(
    api: &Api<Pod>,
    pod: &str,
    containers: &[String],
    host: IpAddr,
    port: u16,
    payload: Vec<u8>,
    timeout: Duration,
) -> Result<Vec<u8>, K8sError> {
    let command = vec!["nc".to_owned(), host.to_string(), port.to_string()];
    first_success(containers, timeout, |container| {
        let command = command.clone();
        let payload = payload.clone();
        async move {
            let out = run(api, pod, container.clone(), command, Some(payload)).await?;
            if out.stdout.is_empty() {
                Err(K8sError::Tunnel(format!("{container}: {}", out.reason())))
            } else {
                Ok(out.stdout)
            }
        }
    })
    .await
}

async fn first_success<T, F, Fut>(
    containers: &[String],
    timeout: Duration,
    mut attempt: F,
) -> Result<T, K8sError>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<T, K8sError>>,
{
    let mut last = K8sError::NoContainer;
    for container in containers {
        match tokio::time::timeout(timeout, attempt(container.clone())).await {
            Ok(Ok(v)) => return Ok(v),
            Ok(Err(e)) => last = e,
            Err(_) => last = K8sError::Timeout(timeout.as_secs()),
        }
    }
    Err(last)
}

async fn run(
    api: &Api<Pod>,
    pod: &str,
    container: String,
    command: Vec<String>,
    stdin: Option<Vec<u8>>,
) -> Result<Output, K8sError> {
    let params = AttachParams::default()
        .container(container)
        .stdin(stdin.is_some())
        .stdout(true)
        .stderr(true);
    tracing::trace!(pod, ?command, "exec start");
    let mut proc = api.exec(pod, command, &params).await?;

    let writer = proc.stdin();
    let feed = async move {
        if let (Some(mut w), Some(data)) = (writer, stdin) {
            w.write_all(&data).await?;
            w.shutdown().await?;
        }
        Ok::<(), std::io::Error>(())
    };
    let (written, stdout, stderr) = tokio::join!(feed, drain(proc.stdout()), drain(proc.stderr()));
    written?;
    let status = match proc.take_status() {
        Some(fut) => fut.await.and_then(|s| s.message),
        None => None,
    };
    tracing::trace!(pod, "exec streams drained");
    proc.join()
        .await
        .map_err(|e| K8sError::Stream(e.to_string()))?;
    tracing::trace!(pod, "exec joined");
    Ok(Output {
        stdout: stdout?,
        stderr: String::from_utf8_lossy(&stderr.unwrap_or_default()).into_owned(),
        status,
    })
}

async fn drain<R: AsyncRead + Unpin>(reader: Option<R>) -> Result<Vec<u8>, std::io::Error> {
    let mut buf = Vec::new();
    if let Some(mut r) = reader {
        r.read_to_end(&mut buf).await?;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_success_tries_in_order_and_reports_last_error() {
        let containers = vec!["a".to_owned(), "b".to_owned()];
        let ok = first_success(&containers, Duration::from_secs(1), |c| async move {
            if c == "b" {
                Ok(c)
            } else {
                Err(K8sError::NoTable(c))
            }
        })
        .await
        .unwrap();
        assert_eq!(ok, "b");
        let err = first_success(&containers, Duration::from_secs(1), |c| async move {
            Err::<(), _>(K8sError::NoTable(c))
        })
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "no /proc/net/tcp output: b");
        let timeout = first_success(&containers[..1], Duration::from_millis(10), |_| async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(())
        })
        .await
        .unwrap_err();
        assert!(matches!(timeout, K8sError::Timeout(_)));
        assert!(matches!(
            first_success(&[], Duration::from_secs(1), |_| async { Ok(()) }).await,
            Err(K8sError::NoContainer)
        ));
    }

    #[test]
    fn output_reason_prefers_stderr() {
        let out = |stderr: &str, status: Option<&str>| Output {
            stdout: vec![],
            stderr: stderr.into(),
            status: status.map(str::to_owned),
        };
        assert_eq!(out(" nc: not found\n", Some("x")).reason(), "nc: not found");
        assert_eq!(out("", Some("exit 127")).reason(), "exit 127");
        assert_eq!(out("", None).reason(), "empty output");
    }
}
