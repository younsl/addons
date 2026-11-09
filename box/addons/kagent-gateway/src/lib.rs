//! kagent-gateway receives Alertmanager webhooks, posts each alert to Slack,
//! asks a kagent agent to investigate it over A2A, and replies with the
//! analysis in the alert's thread. Mentioning the bot inside a thread runs the
//! agent again and answers there.

mod a2a;
mod access;
mod alert;
mod app;
mod config;
pub mod crd;
mod gateway;
mod k8s;
mod observability;
mod slack;

pub use app::start;

/// Installs aws-lc-rs as the process-wide rustls provider. reqwest and kube
/// are built without one, so every client constructor calls this and repeats
/// are no-ops.
fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
