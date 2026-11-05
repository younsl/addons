use std::net::IpAddr;
use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::{Api, Client};

use super::error::K8sError;
use super::exec;
use super::pods::{self, Scope};
use super::probe;
use crate::source::{PodSource, Tunnel};
use crate::types::PodRef;

/// Settings for the ephemeral probe used by database queries.
#[derive(Debug, Clone)]
pub struct ProbeConfig {
    pub image: String,
    pub start_timeout: Duration,
}

#[derive(Clone)]
pub struct KubePods {
    client: Client,
    scope: Scope,
    selector: Option<String>,
    timeout: Duration,
    probe: ProbeConfig,
}

impl KubePods {
    pub const fn new(
        client: Client,
        scope: Scope,
        selector: Option<String>,
        timeout: Duration,
        probe: ProbeConfig,
    ) -> Self {
        Self {
            client,
            scope,
            selector,
            timeout,
            probe,
        }
    }

    fn api(&self, pod: &PodRef) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &pod.namespace)
    }
}

impl PodSource for KubePods {
    type Error = K8sError;

    async fn list(&self) -> Result<Vec<PodRef>, K8sError> {
        pods::list_running(&self.client, &self.scope, self.selector.as_deref()).await
    }

    async fn read_tcp_tables(&self, pod: &PodRef) -> Result<String, K8sError> {
        exec::read_tcp_tables(&self.api(pod), &pod.name, &pod.containers, self.timeout).await
    }
}

impl Tunnel for KubePods {
    type Error = K8sError;

    /// Runs `nc` in the pod's probe container, which shares the pod network and so its
    /// security group, never in the application containers.
    async fn roundtrip(
        &self,
        via: &PodRef,
        host: IpAddr,
        port: u16,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, K8sError> {
        let api = self.api(via);
        let container =
            probe::ensure(&api, &via.name, &self.probe.image, self.probe.start_timeout).await?;
        exec::tunnel(
            &api,
            &via.name,
            &[container],
            host,
            port,
            payload,
            self.timeout,
        )
        .await
    }
}
