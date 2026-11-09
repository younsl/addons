//! Kubernetes API communication: the `AgentAccessPolicy` controller and the
//! cache the mention path reads.

pub mod error;
pub mod policy;

pub use error::Error;

/// Builds a client from the in-cluster service account, or the local
/// kubeconfig outside a cluster.
pub async fn client() -> Result<kube::Client, Error> {
    crate::install_crypto_provider();
    Ok(kube::Client::try_default().await?)
}
