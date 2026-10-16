//! The persistent store: one private SQLite WAL container per cache
//! directory (spec §6, §9-§11). Every operation returns an admitted result,
//! a typed [`Miss`] or a [`Bypass`]; none is a semantic claim.

mod location;
mod outcome;

pub use location::{DATABASE_FILE, StoreLocation, default_directory};
pub use outcome::{Bypass, Integrity, LocationIssue, Lookup, Malformed, Miss, Published, TooLarge};

/// Replaced by `limits::LimitsError` in Task 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitsError {}
