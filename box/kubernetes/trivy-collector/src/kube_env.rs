//! Where the process thinks it is running.
//!
//! Several subsystems need "the namespace I am deployed in" to address their
//! backing ConfigMap or Secret, and they all resolved it slightly differently
//! before. One implementation keeps them consistent.

use std::path::Path;

use anyhow::{Context, Result};

/// Path the kubelet projects the pod's namespace to.
pub const SERVICE_ACCOUNT_NAMESPACE_PATH: &str =
    "/var/run/secrets/kubernetes.io/serviceaccount/namespace";

/// The namespace from the projected ServiceAccount, or `None` when running
/// outside a pod.
pub fn in_cluster_namespace() -> Option<String> {
    read_namespace_file(Path::new(SERVICE_ACCOUNT_NAMESPACE_PATH))
}

/// Read a namespace from a projected-token-style file, ignoring blanks.
fn read_namespace_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Resolve the namespace to operate in: the pod's own when in a cluster,
/// otherwise the active kubeconfig context's.
pub fn namespace_or(kube_config: &kube::Config) -> String {
    in_cluster_namespace().unwrap_or_else(|| kube_config.default_namespace.clone())
}

/// Build a client and resolve the namespace in one step, since every caller
/// needs both.
pub async fn client_and_namespace() -> Result<(kube::Client, String)> {
    let kube_config = kube::Config::infer()
        .await
        .context("Failed to infer Kubernetes configuration")?;
    let namespace = namespace_or(&kube_config);
    let client =
        kube::Client::try_from(kube_config).context("Failed to build Kubernetes client")?;
    Ok((client, namespace))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "trivy-collector-ns-{}-{}",
            std::process::id(),
            contents.len()
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn a_namespace_file_is_trimmed() {
        let path = temp_file("  trivy-system\n");
        assert_eq!(read_namespace_file(&path), Some("trivy-system".to_string()));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn a_blank_namespace_file_is_ignored() {
        let path = temp_file("   \n\n");
        assert!(read_namespace_file(&path).is_none());
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn a_missing_namespace_file_is_ignored() {
        assert!(read_namespace_file(Path::new("/nonexistent/namespace")).is_none());
    }

    #[test]
    fn the_projected_path_is_the_kubelet_default() {
        assert!(SERVICE_ACCOUNT_NAMESPACE_PATH.ends_with("/serviceaccount/namespace"));
    }
}
