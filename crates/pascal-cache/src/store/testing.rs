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
}

impl Store {
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
}

impl FaultPoint {
    pub const ALL: [Self; 8] = [
        Self::EvidenceBeforeCommit,
        Self::RecordBeforeCommit,
        Self::BeginBeforeCommit,
        Self::MembersBeforeCommit,
        Self::SealDigestStreamed,
        Self::SealBeforeCommit,
        Self::SealCommitted,
        Self::RetireBeforeCommit,
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
