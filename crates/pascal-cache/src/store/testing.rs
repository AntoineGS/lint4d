//! Test-only access to store internals (feature `test-support`).

use super::Store;
use super::outcome::Bypass;
use super::pool::Lease;
use super::sqlite::classify;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

type Hook = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
pub(crate) struct Hooks {
    before_commit: Mutex<Option<Hook>>,
    row_fault: Mutex<Option<usize>>,
    faults: Mutex<HashMap<FaultPoint, Hook>>,
    injected: Mutex<Option<Bypass>>,
    pretend_usage: std::sync::atomic::AtomicU64,
    pretend_free_space: Mutex<Option<u64>>,
}

impl Hooks {
    pub(crate) fn before_commit(&self) {
        let hook = self
            .before_commit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    pub(crate) fn pretend_usage(&self) -> u64 {
        self.pretend_usage
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn pretend_free_space(&self) -> Option<u64> {
        *self
            .pretend_free_space
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn take_injected(&self) -> Option<Bypass> {
        self.injected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

impl Store {
    pub fn advance_clock_for_test(&self, by: std::time::Duration) {
        self.clock.advance(by);
    }

    /// Count `extra_bytes` more as the store's physical size.
    pub fn pretend_usage_for_test(&self, extra_bytes: u64) {
        self.hooks
            .pretend_usage
            .store(extra_bytes, std::sync::atomic::Ordering::Relaxed);
    }

    /// Replace the filesystem's free space; `None` restores the real value.
    pub fn pretend_free_space_for_test(&self, free: Option<u64>) {
        *self
            .hooks
            .pretend_free_space
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = free;
    }

    /// The next guarded operation returns `bypass` without running.
    pub fn fail_next_for_test(&self, bypass: Bypass) {
        *self
            .hooks
            .injected
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(bypass);
    }

    /// Take the one-shot row fault: the number of rows to read before the
    /// next `next_batch` call fails with a SQLite I/O error.
    pub(crate) fn take_row_fault(&self) -> Option<usize> {
        self.hooks
            .row_fault
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// A pooled connection held by a test.
pub struct HeldConnection<'a> {
    _lease: Lease<'a>,
}

impl Store {
    pub fn connection_count_for_test(&self) -> usize {
        self.pool.open_connections()
    }

    pub fn hold_reader_for_test(&self) -> Result<HeldConnection<'_>, Bypass> {
        self.reader().map(|lease| HeldConnection { _lease: lease })
    }

    pub fn hold_writer_for_test(&self) -> Result<HeldConnection<'_>, Bypass> {
        self.writer().map(|lease| HeldConnection { _lease: lease })
    }

    /// Run `hook` inside every write transaction, just before it commits.
    pub fn set_before_commit_for_test(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self
            .hooks
            .before_commit
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(hook));
    }

    /// Make the next `next_batch` call fail with a SQLite I/O error after
    /// reading `rows` rows. One-shot; `None` clears a pending fault.
    pub fn fail_row_reads_after_for_test(&self, rows: Option<usize>) {
        *self
            .hooks
            .row_fault
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = rows;
    }

    /// Let the database grow by at most `extra_pages` pages through this
    /// store's write connection, so tests can fill the "disk".
    pub fn limit_growth_for_test(&self, extra_pages: i64) -> Result<(), Bypass> {
        let writer = self.writer()?;
        let pages: i64 = writer
            .pragma_query_value(None, "page_count", |row| row.get(0))
            .map_err(|error| classify(&error))?;
        writer
            .pragma_update(None, "max_page_count", pages + extra_pages)
            .map_err(|error| classify(&error))
    }
}

/// Named points of the publication protocol where a test may act or a test
/// process may abort (spec §12.1 amendment). They exist only in a
/// `test-support` build; `fault_point!` compiles to nothing otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaultPoint {
    /// Inside an evidence publication, before commit.
    EvidenceBeforeCommit,
    /// Inside a member record publication, before commit.
    RecordBeforeCommit,
    /// Inside `begin_snapshot`, before commit.
    BeginBeforeCommit,
    /// Inside an `add_members` batch, before commit.
    MembersBeforeCommit,
    /// After the seal digest streamed, before the seal transaction; no
    /// transaction or connection is held.
    SealDigestStreamed,
    /// Inside the seal transaction, before commit.
    SealBeforeCommit,
    /// After the seal committed; no transaction or connection is held.
    SealCommitted,
    /// Inside a snapshot retirement, before commit.
    RetireBeforeCommit,
    /// In `renew_lease`, after it found the lease due and before it takes
    /// the writer.
    LeaseRenewDecided,
    /// Inside a maintenance retirement batch, before commit.
    GcRetireBeforeCommit,
    /// Inside a `reclaim` deletion batch, before commit.
    GcDeleteBeforeCommit,
    /// After a maintenance checkpoint; no connection is held.
    CheckpointCompleted,
    /// Inside an incremental vacuum batch, before commit.
    VacuumBeforeCommit,
    /// Inside a namespace reset, before commit.
    ResetBeforeCommit,
}

impl FaultPoint {
    pub const ALL: [Self; 14] = [
        Self::EvidenceBeforeCommit,
        Self::RecordBeforeCommit,
        Self::BeginBeforeCommit,
        Self::MembersBeforeCommit,
        Self::SealDigestStreamed,
        Self::SealBeforeCommit,
        Self::SealCommitted,
        Self::RetireBeforeCommit,
        Self::LeaseRenewDecided,
        Self::GcRetireBeforeCommit,
        Self::GcDeleteBeforeCommit,
        Self::CheckpointCompleted,
        Self::VacuumBeforeCommit,
        Self::ResetBeforeCommit,
    ];

    /// The points inside maintenance batches.
    pub const MAINTENANCE: [Self; 5] = [
        Self::GcRetireBeforeCommit,
        Self::GcDeleteBeforeCommit,
        Self::CheckpointCompleted,
        Self::VacuumBeforeCommit,
        Self::ResetBeforeCommit,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::EvidenceBeforeCommit => "EvidenceBeforeCommit",
            Self::RecordBeforeCommit => "RecordBeforeCommit",
            Self::BeginBeforeCommit => "BeginBeforeCommit",
            Self::MembersBeforeCommit => "MembersBeforeCommit",
            Self::SealDigestStreamed => "SealDigestStreamed",
            Self::SealBeforeCommit => "SealBeforeCommit",
            Self::SealCommitted => "SealCommitted",
            Self::RetireBeforeCommit => "RetireBeforeCommit",
            Self::LeaseRenewDecided => "LeaseRenewDecided",
            Self::GcRetireBeforeCommit => "GcRetireBeforeCommit",
            Self::GcDeleteBeforeCommit => "GcDeleteBeforeCommit",
            Self::CheckpointCompleted => "CheckpointCompleted",
            Self::VacuumBeforeCommit => "VacuumBeforeCommit",
            Self::ResetBeforeCommit => "ResetBeforeCommit",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|point| point.name() == name)
    }
}

impl Store {
    pub(crate) fn fault(&self, point: FaultPoint) {
        let hook = self
            .hooks
            .faults
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&point)
            .cloned();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Run `hook` each time this store reaches `point`. A hook inside a
    /// write transaction must not write through this store's own pool.
    pub fn set_fault_for_test(&self, point: FaultPoint, hook: impl Fn() + Send + Sync + 'static) {
        self.hooks
            .faults
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(point, Arc::new(hook));
    }

    pub fn clear_fault_for_test(&self, point: FaultPoint) {
        self.hooks
            .faults
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&point);
    }
}

/// `maintenance.lock` held by a test until dropped.
pub struct MaintenanceLockHold(#[allow(dead_code)] super::maintenance::MaintenanceLock);

impl Store {
    /// Take `maintenance.lock` as another maintainer would; `Busy` when
    /// someone holds it.
    pub fn hold_maintenance_lock_for_test(&self) -> Result<MaintenanceLockHold, Bypass> {
        super::maintenance::MaintenanceLock::try_take(&self.location)?
            .map(MaintenanceLockHold)
            .ok_or(Bypass::Busy)
    }
}

/// Scratch held by a test until dropped.
#[derive(Debug)]
pub struct ScratchHold {
    _lease: super::reader::ScratchLease,
}

impl Store {
    /// Hold `bytes` of the store's scratch budget, or `None` when they do
    /// not fit.
    pub fn hold_scratch_for_test(&self, bytes: u64) -> Option<ScratchHold> {
        self.scratch
            .reserve(bytes)
            .map(|lease| ScratchHold { _lease: lease })
    }
}
