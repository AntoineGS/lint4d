//! The persistent store: one private SQLite WAL container per cache
//! directory (spec §6, §9-§11). Every operation returns an admitted result,
//! a typed [`Miss`] or a [`Bypass`]; none is a semantic claim.

/// Let a test act at a named protocol point (feature `test-support`);
/// expands to nothing otherwise.
macro_rules! fault_point {
    ($store:expr, $point:ident) => {
        #[cfg(feature = "test-support")]
        $store.fault($crate::store::FaultPoint::$point);
    };
}

mod artifacts;
mod budget;
mod clock;
mod container;
mod evidence;
mod facts;
mod gc;
mod health;
mod index;
mod keys;
mod leases;
mod limits;
mod location;
mod namespaces;
mod nonce;
mod outcome;
mod pool;
mod queries;
mod reader;
mod records;
mod sequence;
mod snapshots;
mod sqlite;
mod stream;
#[cfg(feature = "test-support")]
mod testing;
mod wal;

pub use artifacts::{
    ArtifactHeader, ArtifactRow, ArtifactWrite, FACT_CHECKSUM_CONTEXT, ROW_CHECKSUM_CONTEXT,
};
pub use evidence::{
    EvidenceHeader, EvidenceReader, EvidenceTarget, EvidenceWrite, ObservationBatch,
    PublishedEvidence, PublishedSet,
};
#[cfg(feature = "test-support")]
pub use index::INDEX_QUERY as INDEX_QUERY_FOR_TEST;
pub use index::{IndexHit, IndexPage};
pub use keys::{ArtifactKeyRef, RootMismatch};
pub use location::{DATABASE_FILE, StoreLocation, default_directory};
pub use outcome::SealRefusal;
pub use outcome::{Bypass, Integrity, LocationIssue, Lookup, Malformed, Miss, Published, TooLarge};
#[cfg(feature = "test-support")]
pub use queries::{
    IMPORTERS_QUERY as IMPORTERS_QUERY_FOR_TEST, MEMBER_QUERY as MEMBER_QUERY_FOR_TEST,
};
pub use queries::{ImporterHit, ImporterPage};
pub use reader::{ArtifactReader, RowBatch, StoredRow};
pub use records::{BindingBatch, MemberRecordHeader, MemberRecordReader, PublishedRecord};
pub use snapshots::{
    MemberBatch, SnapshotBuilder, SnapshotHeader, SnapshotId, SnapshotMember, SnapshotReader,
    SnapshotSeal,
};
#[cfg(feature = "test-support")]
pub use testing::FaultPoint;

pub use budget::{
    LOW_SPACE_MARGIN_BYTES, MIN_BUDGET_TO_WAL_RATIO, MIN_WAL_THRESHOLD_BYTES, PolicyError,
    PolicyState, Pressure, ROW_OVERHEAD_BYTES, StorePolicy, Usage,
};
pub use container::{APPLICATION_ID, CONTAINER_VERSION};
pub use gc::{MaintenanceLimits, MaintenanceLimitsError, RetireStep};
pub use health::{Access, MAX_HEALTH_BACKOFF, MIN_HEALTH_BACKOFF, Unhealthy};
pub use leases::{
    Hint, HintStats, LEASE_EXPIRY, LEASE_RENEWAL, MAX_LEASE_HOLDS, MAX_PENDING_HINTS,
};
pub use limits::{
    CONNECTION_OVERHEAD_BYTES, JOURNAL_SIZE_LIMIT_BYTES, LimitsError, MAX_ARTIFACT_CONTENT_BYTES,
    MAX_ARTIFACT_ROWS, MAX_EVIDENCE_CANDIDATES, MAX_EVIDENCE_OBSERVATIONS, MAX_INDEX_KEY_BYTES,
    MAX_INDEX_PAGE, MAX_MANIFEST_BYTES, MAX_MEMBER_BINDINGS, MAX_OPEN_WAIT, MAX_PAGE_CACHE_BYTES,
    MAX_READERS, MAX_ROW_BATCH_BYTES, MAX_ROW_BATCH_ROWS, MAX_SCRATCH_BYTES,
    MAX_SNAPSHOT_CANDIDATES, MAX_SNAPSHOT_MEMBERS, MAX_SUPPLEMENTAL_BYTES, MAX_WAIT,
    MIN_CONNECTION_PAGE_CACHE_BYTES, SQLITE_LENGTH_LIMIT, StoreLimits,
};
pub use namespaces::{Reclaimed, RetireReport, Scope};
pub use sqlite::MIN_SQLITE_VERSION_NUMBER;
#[cfg(feature = "test-support")]
pub use testing::{HeldConnection, ScratchHold};
pub use wal::Checkpointed;

use crate::Namespace;
use namespaces::NamespaceLock;
use pool::{Deadline, Lease, Pool, Role};
use std::path::Path;

/// One process's handle on a store directory, scoped to one namespace.
/// While it exists the namespace is held live (spec §10).
///
/// The scratch budget, the page cache and the 2+1 connection pool belong to
/// each `Store`, while the spec states them per process. One admitted store
/// per process is expected (the LSP opens it through
/// `workspace::admitted::AdmittedStore`, which charges
/// `StoreLimits::admission_bytes` to `maxCacheBytes` first): every
/// additional `Store` adds its own budgets.
pub struct Store {
    location: StoreLocation,
    limits: StoreLimits,
    namespace: Namespace,
    scope: Scope,
    pool: Pool,
    scratch: std::sync::Arc<reader::ScratchBudget>,
    _lock: NamespaceLock,
    clock: clock::Clock,
    lease: leases::ProcessLease,
    hints: leases::Hints,
    health: health::Health,
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
        let clock = clock::Clock::default();
        let lease = leases::ProcessLease::new(nonce::nonce());
        let scope = {
            let mut writer = pool.acquire(Role::Writer, deadline)?;
            container::prepare_container(&mut writer, deadline)?;
            let scope = namespaces::register(&mut writer, namespace, unix_seconds(), deadline)?;
            let transaction = sqlite::begin_within(
                &mut writer,
                rusqlite::TransactionBehavior::Immediate,
                deadline,
            )?;
            namespaces::ensure_live(&transaction, scope)?;
            leases::renew(&transaction, scope, &lease, clock.unix_seconds())?;
            transaction
                .commit()
                .map_err(|error| sqlite::classify(&error))?;
            scope
        };
        let store = Self {
            location,
            limits,
            namespace: namespace.clone(),
            scope,
            pool,
            scratch: reader::ScratchBudget::new(limits.scratch_bytes),
            _lock: lock,
            clock,
            lease,
            hints: leases::Hints::default(),
            health: health::Health::default(),
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
        self.guarded(Access::Read, || {
            let mut reader = self.reader()?;
            let transaction = sqlite::begin(&mut reader, rusqlite::TransactionBehavior::Deferred)?;
            namespaces::ensure_live(&transaction, self.scope)
        })
    }

    pub fn retire_obsolete(&self) -> Result<RetireReport, Bypass> {
        self.guarded(Access::Maintenance, || {
            self.retire_within(Deadline::after(self.limits.open_wait))
        })
    }

    fn retire_within(&self, deadline: Deadline) -> Result<RetireReport, Bypass> {
        let mut writer = self.pool.acquire(Role::Writer, deadline)?;
        namespaces::retire_obsolete(&mut writer, &self.location, self.scope.namespace, deadline)
    }

    /// Delete at most `max_rows` rows in one `IMMEDIATE` transaction: rows
    /// of retired scopes first, then retired rows nothing references and
    /// orphan key manifests of this store's scope while it is live. Run off
    /// interactive paths and repeat until `complete`. At the WAL threshold
    /// it writes nothing (`WalPressure`: only a checkpoint makes room, spec
    /// D13).
    pub fn reclaim(&self, max_rows: usize) -> Result<Reclaimed, Bypass> {
        self.guarded(Access::Maintenance, || {
            let fail = |error: rusqlite::Error| sqlite::classify(&error);
            let deadline = Deadline::after(self.limits.wait);
            let mut writer = self.pool.acquire(Role::Writer, deadline)?;
            let transaction = sqlite::begin_within(
                &mut writer,
                rusqlite::TransactionBehavior::Immediate,
                deadline,
            )?;
            let policy = budget::read_policy(&transaction)?.policy;
            if budget::pressure_of(policy, &budget::read_usage(self, &transaction)?)
                == budget::Pressure::WalPressure
            {
                return Err(Bypass::WalPressure);
            }
            let reclaimed = namespaces::reclaim(&transaction, max_rows, self.scope)?;
            fault_point!(self, GcDeleteBeforeCommit);
            self.before_commit();
            transaction.commit().map_err(fail)?;
            Ok(reclaimed)
        })
    }

    /// The process-level health state, if the store is unhealthy.
    pub fn health(&self) -> Option<Unhealthy> {
        self.health.state()
    }

    /// Run `operation` unless the health state blocks `access`, then
    /// record its outcome (spec §10 amendment 2026-11-03, D15). Only the
    /// outermost guarded call on a thread checks and records, so a failure
    /// in a nested call counts once.
    pub(crate) fn guarded<T>(
        &self,
        access: Access,
        operation: impl FnOnce() -> Result<T, Bypass>,
    ) -> Result<T, Bypass> {
        let depth = GuardDepth::enter();
        if !depth.outermost {
            return operation();
        }
        if let Some(bypass) = self.health.blocked(access, self.clock.now()) {
            return Err(bypass);
        }
        #[cfg(feature = "test-support")]
        let result = match self.hooks.take_injected() {
            Some(bypass) => Err(bypass),
            None => operation(),
        };
        #[cfg(not(feature = "test-support"))]
        let result = operation();
        self.health.record(access, &result, self.clock.now());
        result
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

    #[cfg(not(feature = "test-support"))]
    pub(crate) fn take_row_fault(&self) -> Option<usize> {
        None
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        leases::release(self);
    }
}

thread_local! {
    static GUARD_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Marks a thread as inside a guarded operation until dropped (also on
/// unwind or early return).
struct GuardDepth {
    outermost: bool,
}

impl GuardDepth {
    fn enter() -> Self {
        let outermost = GUARD_DEPTH.with(|depth| {
            let current = depth.get();
            depth.set(current + 1);
            current == 0
        });
        Self { outermost }
    }
}

impl Drop for GuardDepth {
    fn drop(&mut self) {
        GUARD_DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::Digest;

    #[test]
    fn a_failure_in_a_nested_guarded_call_is_recorded_once() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            directory.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let store = Store::open(
            directory.path(),
            &Namespace::new(Digest::from_bytes([3; 32])),
            StoreLimits::default(),
        )
        .unwrap();
        let nested = store.guarded(Access::Write, || {
            store.guarded(Access::Write, || Err::<(), _>(Bypass::Full))
        });
        assert_eq!(nested, Err(Bypass::Full));
        assert_eq!(store.health(), Some(Unhealthy::Full));
        store.advance_clock_for_test(MIN_HEALTH_BACKOFF);
        assert_eq!(
            store.guarded(Access::Write, || Ok(())),
            Ok(()),
            "recorded once: the backoff is the minimum, not doubled"
        );
        assert_eq!(store.health(), None);
    }
}
