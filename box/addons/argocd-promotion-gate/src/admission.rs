//! The `ValidatingAdmissionWebhook` that actually blocks a sync.
//!
//! A UI extension cannot enforce anything: disabling a button in the Argo CD
//! UI leaves `argocd app sync`, the REST API, and auto-sync untouched. A sync
//! is a write that sets the Application's top-level `operation` field, so
//! admission is the one place every path passes through.

pub mod handler;
pub mod review;

pub use handler::{AdmissionState, router};
