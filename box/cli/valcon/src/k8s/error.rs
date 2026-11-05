use thiserror::Error;

#[derive(Debug, Error)]
pub enum K8sError {
    #[error(transparent)]
    Kube(#[from] kube::Error),

    #[error("kubeconfig: {0}")]
    Kubeconfig(#[from] kube::config::KubeconfigError),

    #[error("exec stream: {0}")]
    Stream(String),

    #[error("reading exec stream: {0}")]
    Io(#[from] std::io::Error),

    #[error("exec timed out after {0}s")]
    Timeout(u64),

    #[error("no /proc/net/tcp output: {0}")]
    NoTable(String),

    #[error("tunnel produced no reply: {0}")]
    Tunnel(String),

    #[error("probe container: {0}")]
    Probe(String),

    #[error("pod has no containers")]
    NoContainer,
}
