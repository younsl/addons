//! The promotion rules: how an Application maps onto the promotion chain, how
//! container images are compared across environments, and the verdict itself.
//!
//! Everything here is pure. The facts a verdict depends on are gathered by the
//! engine and passed in, so the rules can be tested without a cluster.

pub mod chain;
pub mod evaluate;
pub mod image;
pub mod types;

pub use chain::{app_name_for, identity_of};
pub use evaluate::{Input, evaluate, with_upstream};
pub use image::{extract_images, parse_image};
pub use types::{AppSnapshot, Code, Decision, ImageComparison, ImageRef};
