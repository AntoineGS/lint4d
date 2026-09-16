//! Explicit opt-in target for the deterministic protocol concurrency tests.
//!
//! Run with:
//! `cargo test -p pascal-lsp --features test-support --test protocol_barriers`
//!
//! The normal `protocol` target deliberately does not enable this feature.
include!("protocol.rs");
