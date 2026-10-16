//! The persistent store: one private SQLite WAL container per cache
//! directory (spec §6, §9-§11). Every operation returns an admitted result,
//! a typed [`Miss`] or a [`Bypass`]; none is a semantic claim.

mod artifacts;
mod container;
mod index;
mod keys;
mod limits;
mod location;
mod namespaces;
mod outcome;
mod pool;
mod reader;
mod sqlite;
#[cfg(feature = "test-support")]
mod testing;

pub use artifacts::{
    ArtifactHeader, ArtifactRow, ArtifactWrite, FACT_CHECKSUM_CONTEXT, ROW_CHECKSUM_CONTEXT,
};
#[cfg(feature = "test-support")]
pub use index::INDEX_QUERY as INDEX_QUERY_FOR_TEST;
pub use index::{IndexHit, IndexPage};
pub use keys::{ArtifactKeyRef, RootMismatch};
pub use location::{DATABASE_FILE, StoreLocation, default_directory};
pub use outcome::{Bypass, Integrity, LocationIssue, Lookup, Malformed, Miss, Published, TooLarge};
pub use reader::{ArtifactReader, RowBatch, StoredRow};

pub use container::{APPLICATION_ID, CONTAINER_VERSION};
pub use limits::{
    LimitsError, MAX_ARTIFACT_CONTENT_BYTES, MAX_ARTIFACT_ROWS, MAX_INDEX_KEY_BYTES,
    MAX_INDEX_PAGE, MAX_MANIFEST_BYTES, MAX_OPEN_WAIT, MAX_PAGE_CACHE_BYTES, MAX_READERS,
    MAX_ROW_BATCH_BYTES, MAX_ROW_BATCH_ROWS, MAX_SCRATCH_BYTES, MAX_SUPPLEMENTAL_BYTES, MAX_WAIT,
    MIN_CONNECTION_PAGE_CACHE_BYTES, SQLITE_LENGTH_LIMIT, StoreLimits,
};
pub use namespaces::{Reclaimed, RetireReport, Scope};
pub use sqlite::MIN_SQLITE_VERSION_NUMBER;
#[cfg(feature = "test-support")]
pub use testing::HeldConnection;

use crate::Namespace;
use namespaces::NamespaceLock;
use pool::{Deadline, Lease, Pool, Role};
use std::path::Path;

/// One process's handle on a store directory, scoped to one namespace.
/// While it exists the namespace is held live (spec §10).
pub struct Store {
    location: StoreLocation,
    limits: StoreLimits,
    namespace: Namespace,
    scope: Scope,
    pool: Pool,
    scratch: std::sync::Arc<reader::ScratchBudget>,
    _lock: NamespaceLock,
    #[cfg(feature = "test-support")]
    hooks: testing::Hooks,
}

impl Store {
    /// Open or create the store in `directory` for `namespace`, then retire
    /// namespaces no live process holds. Any failure is a bypass: the
    /// caller analyses without the cache.
    pub fn open(
        directory: &Path,
        namespace: &Namespace,
        limits: StoreLimits,
    ) -> Result<Self, Bypass> {
        limits.validate().map_err(Bypass::InvalidLimits)?;
        sqlite::check_sqlite_version(rusqlite::version_number())?;
        let location = StoreLocation::prepare(directory)?;
        location.prepare_namespace_locks()?;
        for file in location.database_files() {
            StoreLocation::check_file(&file)?;
        }
        let deadline = Deadline::after(limits.open_wait);
        let lock = NamespaceLock::share(location.namespace_lock(namespace.id()), deadline)?;
        let pool = Pool::new(location.database(), limits);
        let scope = {
            let mut writer = pool.acquire(Role::Writer, deadline)?;
            container::prepare_container(&mut writer, deadline)?;
            namespaces::register(&mut writer, namespace, unix_seconds(), deadline)?
        };
        let store = Self {
            location,
            limits,
            namespace: namespace.clone(),
            scope,
            pool,
            scratch: reader::ScratchBudget::new(limits.scratch_bytes),
            _lock: lock,
            #[cfg(feature = "test-support")]
            hooks: testing::Hooks::default(),
        };
        // Best effort: whatever is left is retired by a later start.
        let _ = store.retire_within(deadline);
        Ok(store)
    }

    pub fn scope(&self) -> Scope {
        self.scope
    }

    pub fn namespace(&self) -> &Namespace {
        &self.namespace
    }

    pub fn location(&self) -> &StoreLocation {
        &self.location
    }

    pub fn limits(&self) -> StoreLimits {
        self.limits
    }

    /// Whether this store's scope is still live (not reset or retired).
    pub fn check_scope(&self) -> Result<(), Bypass> {
        let mut reader = self.reader()?;
        let transaction = sqlite::begin(&mut reader, rusqlite::TransactionBehavior::Deferred)?;
        namespaces::ensure_live(&transaction, self.scope)
    }

    pub fn retire_obsolete(&self) -> Result<RetireReport, Bypass> {
        self.retire_within(Deadline::after(self.limits.open_wait))
    }

    fn retire_within(&self, deadline: Deadline) -> Result<RetireReport, Bypass> {
        let mut writer = self.pool.acquire(Role::Writer, deadline)?;
        namespaces::retire_obsolete(&mut writer, &self.location, self.scope.namespace, deadline)
    }

    /// Delete at most `max_rows` rows of retired scopes. Run off
    /// interactive paths and repeat until `complete`.
    pub fn reclaim(&self, max_rows: usize) -> Result<Reclaimed, Bypass> {
        let deadline = Deadline::after(self.limits.wait);
        let mut writer = self.pool.acquire(Role::Writer, deadline)?;
        namespaces::reclaim(&mut writer, max_rows, deadline)
    }

    pub(crate) fn writer(&self) -> Result<Lease<'_>, Bypass> {
        self.pool
            .acquire(Role::Writer, Deadline::after(self.limits.wait))
    }

    pub(crate) fn reader(&self) -> Result<Lease<'_>, Bypass> {
        self.pool
            .acquire(Role::Reader, Deadline::after(self.limits.wait))
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn before_commit(&self) {
        self.hooks.before_commit();
    }

    #[cfg(not(feature = "test-support"))]
    pub(crate) fn before_commit(&self) {}
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
