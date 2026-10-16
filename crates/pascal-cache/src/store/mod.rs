//! The persistent store: one private SQLite WAL container per cache
//! directory (spec §6, §9-§11). Every operation returns an admitted result,
//! a typed [`Miss`] or a [`Bypass`]; none is a semantic claim.

mod container;
mod limits;
mod location;
mod outcome;
mod sqlite;

pub use location::{DATABASE_FILE, StoreLocation, default_directory};
pub use outcome::{Bypass, Integrity, LocationIssue, Lookup, Malformed, Miss, Published, TooLarge};

pub use container::{APPLICATION_ID, CONTAINER_VERSION};
pub use limits::{
    LimitsError, MAX_ARTIFACT_CONTENT_BYTES, MAX_ARTIFACT_ROWS, MAX_INDEX_KEY_BYTES,
    MAX_INDEX_PAGE, MAX_MANIFEST_BYTES, MAX_OPEN_WAIT, MAX_PAGE_CACHE_BYTES, MAX_READERS,
    MAX_ROW_BATCH_BYTES, MAX_ROW_BATCH_ROWS, MAX_SCRATCH_BYTES, MAX_SUPPLEMENTAL_BYTES, MAX_WAIT,
    MIN_CONNECTION_PAGE_CACHE_BYTES, SQLITE_LENGTH_LIMIT, StoreLimits,
};
pub use sqlite::MIN_SQLITE_VERSION_NUMBER;
