//! Explicit opt-in target for the deterministic protocol concurrency tests.
//!
//! Run with:
//! `cargo test -p pascal-lsp --features test-support --test protocol_barriers`
//!
//! The normal `protocol` target deliberately does not enable this feature.
mod common;

include!("protocol/barrier_cases.rs");

#[path = "protocol/persistent_cache.rs"]
mod persistent_cache;

#[path = "protocol/persistent_cache_in_root.rs"]
mod persistent_cache_in_root;

#[path = "protocol/persistent_index.rs"]
mod persistent_index;

#[path = "protocol/persistent_queries.rs"]
mod persistent_queries;

#[path = "protocol/persistent_acceptance.rs"]
mod persistent_acceptance;

#[path = "protocol/persistent_processes.rs"]
mod persistent_processes;
