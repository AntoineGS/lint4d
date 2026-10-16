//! Test-only access to store internals (feature `test-support`).

use super::Store;
use super::outcome::Bypass;
use super::pool::Lease;
use super::sqlite::classify;
use std::sync::{Arc, Mutex, PoisonError};

type Hook = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
pub(crate) struct Hooks {
    before_commit: Mutex<Option<Hook>>,
    row_fault: Mutex<Option<usize>>,
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
