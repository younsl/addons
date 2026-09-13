//! The crate's integration tests: one test crate, one binary.
//! Each file under `integration/` drives forklift from outside through
//! its public API. Kept as a single target because every extra file
//! directly under `tests/` would compile and link a separate binary.

mod api_artifacts_enrich;
mod api_audit;
mod api_console;
mod api_dangling;
mod api_dangling_list;
mod api_extra;
mod api_group;
mod api_impersonate;
mod api_label;
mod api_oci_views;
mod api_openapi;
mod api_rawupload;
mod api_rbac;
mod api_repositories;
mod api_repositories_errors;
mod api_security;
mod api_upstreamauth;
mod api_users;
mod meta_blob_gc;
mod meta_console_store;
mod meta_search_more;
mod meta_search_page;
mod meta_store_more;
mod meta_store_surfaces;
mod meta_swap;
