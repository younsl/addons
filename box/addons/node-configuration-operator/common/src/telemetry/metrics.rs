//! Prometheus metrics the controller and the agent both expose.

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::info::Info;
use prometheus_client::registry::Registry;

use crate::boot::Build;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ReconcileLabels {
    pub result: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct BuildLabels {
    version: &'static str,
    revision: &'static str,
    rust_version: &'static str,
}

/// Reconciles of the controller or runs of the agent by result, next to the
/// build information of the binary.
#[derive(Clone, Default)]
pub struct Reconciles(Family<ReconcileLabels, Counter>);

impl Reconciles {
    pub fn new(registry: &mut Registry, build: &Build) -> Self {
        let reconciles = Self::default();
        registry.register(
            "node_configuration_build",
            "Build information",
            Info::new(BuildLabels {
                version: build.version,
                revision: build.commit,
                rust_version: build.rustc_version,
            }),
        );
        registry.register(
            "node_configuration_reconcile",
            "Reconciles in the controller and runs in the agent by result",
            reconciles.0.clone(),
        );
        reconciles
    }

    pub fn record(&self, ok: bool) {
        let result = if ok { "success" } else { "error" };
        self.0.get_or_create(&ReconcileLabels { result }).inc();
    }
}
