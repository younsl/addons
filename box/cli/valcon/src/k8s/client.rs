use kube::Client;
use kube::config::{KubeConfigOptions, Kubeconfig};

use super::error::K8sError;

/// Builds a client for `context`. Returns the context name and its default namespace.
pub async fn connect(context: Option<&str>) -> Result<(Client, String, String), K8sError> {
    let options = KubeConfigOptions {
        context: context.map(str::to_owned),
        ..KubeConfigOptions::default()
    };
    let name = context.map_or_else(
        || {
            Kubeconfig::read()
                .ok()
                .and_then(|k| k.current_context)
                .unwrap_or_else(|| "current-context".into())
        },
        str::to_owned,
    );
    let config = kube::Config::from_kubeconfig(&options).await?;
    let namespace = config.default_namespace.clone();
    Ok((Client::try_from(config)?, name, namespace))
}
