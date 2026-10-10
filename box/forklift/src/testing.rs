//! Test harnesses shared by the unit tests in `src/` and the integration
//! tests in `tests/`. Gated behind the `testing` feature so it never lands
//! in a production build.

pub mod api;
pub(crate) mod auth;
pub mod meta;
pub(crate) mod repo;
pub(crate) mod server;
