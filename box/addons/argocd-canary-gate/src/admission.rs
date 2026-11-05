//! The admission webhook: the review envelope and the endpoint.

pub mod handler;
pub mod review;

pub use handler::{AdmissionState, router};
