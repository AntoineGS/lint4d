//! Streams a multi-row manifest (an evidence set, a member record's
//! bindings, a snapshot's members) in bounded batches, each in its own short
//! read transaction, while a witness recomputes its digest (spec §6.2, §9,
//! §10). Only a read that reached the end, found no row past the count and
//! matched the stored digest while its parent was still readable is
//! verified.

use super::Store;
use super::health::Access;
use super::limits::{MAX_ROW_BATCH_BYTES, MAX_ROW_BATCH_ROWS};
use super::namespaces::{Scope, ensure_live, epoch};
use super::outcome::{Bypass, Lookup, Malformed, Miss};
use super::reader::ScratchLease;
use super::sqlite::{begin, classify, classify_read};
use crate::Digest;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

/// Why a row could not be taken: what it says makes the manifest unusable,
/// or SQLite could not read it.
#[derive(Debug)]
pub(crate) enum RowError {
    Miss(Miss),
    Sql(rusqlite::Error),
}

impl From<rusqlite::Error> for RowError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

impl From<rusqlite::types::FromSqlError> for RowError {
    fn from(error: rusqlite::types::FromSqlError) -> Self {
        Self::Sql(error.into())
    }
}

/// One kind of streamed manifest. Every statement binds `?1` namespace,
/// `?2` reset epoch and `?3` the parent's ID.
pub(crate) trait RowSource: Clone {
    type Item;
    /// The parent's eligibility: one row whose first column is 1 when
    /// readable, anything else when retired; no row when it vanished.
    const PARENT_SQL: &'static str;
    /// `ordinal` first, then the source's columns, for `ordinal >= ?4`,
    /// `ORDER BY ordinal LIMIT ?5`.
    const ROWS_SQL: &'static str;
    /// `SELECT 1` of any row with `ordinal >= ?4`, `LIMIT 1`.
    const TRAILING_SQL: &'static str;

    /// The row's stored bytes, borrowed in place, so an oversized row is
    /// refused before anything is copied.
    fn size(row: &rusqlite::Row<'_>) -> rusqlite::Result<usize>;

    /// Decode and check the row and fold it into the witness.
    fn take(&mut self, row: &rusqlite::Row<'_>) -> Result<Self::Item, RowError>;

    /// The digest of everything taken so far.
    fn finish(&self) -> Digest;
}

/// What the parent's header promises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Expected {
    pub(crate) scope: Scope,
    pub(crate) parent: Vec<u8>,
    pub(crate) count: u64,
    /// The rows' stored bytes, when the header records them.
    pub(crate) content_bytes: Option<u64>,
    pub(crate) witness: Digest,
}

/// At most 256 items decoded from at most 1 MiB of rows, charged to the
/// store's scratch budget until dropped.
#[derive(Debug)]
pub(crate) struct Batch<T> {
    pub(crate) items: Vec<T>,
    pub(crate) scratch: ScratchLease,
}

/// Why the parent can no longer be read, by `S::PARENT_SQL` in the
/// caller's transaction; `None` while it is readable.
pub(crate) fn ineligible<S: RowSource>(
    transaction: &Connection,
    namespace: &[u8],
    scope: Scope,
    parent: &[u8],
) -> Result<Option<Miss>, Bypass> {
    let eligibility: Option<i64> = transaction
        .query_row(
            S::PARENT_SQL,
            params![namespace, epoch(scope), parent],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| classify(&error))?;
    Ok(match eligibility {
        None => Some(Miss::Vanished),
        Some(1) => None,
        Some(_) => Some(Miss::Retired),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Reading,
    Verified,
    Failed(Miss),
}

/// Reads one manifest of `S` rows against the parent's [`Expected`]
/// header. Items handed out are unverified until `next_batch` returns
/// `Hit(None)`; any miss is sticky and leaves the read unverified.
pub(crate) struct ManifestReader<'a, S: RowSource> {
    store: &'a Store,
    expected: Expected,
    source: S,
    next_ordinal: u64,
    read_bytes: u64,
    state: State,
}

impl<'a, S: RowSource> ManifestReader<'a, S> {
    pub(crate) fn new(store: &'a Store, expected: Expected, source: S) -> Self {
        Self {
            store,
            expected,
            source,
            next_ordinal: 0,
            read_bytes: 0,
            state: State::Reading,
        }
    }

    pub(crate) fn is_verified(&self) -> bool {
        self.state == State::Verified
    }

    fn fail(&mut self, miss: Miss) -> Result<Lookup<Option<Batch<S::Item>>>, Bypass> {
        self.state = State::Failed(miss);
        Ok(Lookup::Miss(miss))
    }

    /// The next batch, or `Hit(None)` once every row was read and the
    /// witness verified. Items are unverified until then. A header from
    /// another scope is fenced before any SQL.
    pub(crate) fn next_batch(&mut self) -> Result<Lookup<Option<Batch<S::Item>>>, Bypass> {
        let store = self.store;
        store.guarded(Access::Read, || self.next_batch_unguarded())
    }

    fn next_batch_unguarded(&mut self) -> Result<Lookup<Option<Batch<S::Item>>>, Bypass> {
        if self.expected.scope != self.store.scope() {
            return Err(Bypass::Fenced);
        }
        match self.state {
            State::Verified => return Ok(Lookup::Hit(None)),
            State::Failed(miss) => return Ok(Lookup::Miss(miss)),
            State::Reading => {}
        }
        // Copy what the statements bind out of `self`, so `self.fail`
        // stays callable while rows are borrowed.
        let store = self.store;
        let scope = self.expected.scope;
        let namespace = scope.namespace.as_bytes().to_vec();
        let parent = self.expected.parent.clone();
        if self.next_ordinal == self.expected.count {
            let mut reader = store.reader()?;
            let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
            ensure_live(&transaction, scope)?;
            // A parent retired or deleted after its last batch, or one
            // without rows, is no more verified than one lost mid-read.
            if let Some(miss) = ineligible::<S>(&transaction, &namespace, scope, &parent)? {
                return self.fail(miss);
            }
            let trailing: Option<i64> = transaction
                .query_row(
                    S::TRAILING_SQL,
                    params![namespace, epoch(scope), parent, self.expected.count as i64],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| classify(&error))?;
            drop(transaction);
            if trailing.is_some() {
                return self.fail(Miss::Malformed(Malformed::Count));
            }
            let bytes_agree = self
                .expected
                .content_bytes
                .is_none_or(|bytes| bytes == self.read_bytes);
            if bytes_agree && self.source.finish() == self.expected.witness {
                self.state = State::Verified;
                return Ok(Lookup::Hit(None));
            }
            return self.fail(Miss::ChecksumMismatch);
        }
        // A batch's size is known only once its rows are read, so reserve
        // the whole allowance up front and shrink to what was used.
        let Some(mut scratch) = store.scratch.reserve(MAX_ROW_BATCH_BYTES as u64) else {
            return Err(Bypass::ScratchExhausted);
        };
        let mut reader = store.reader()?;
        let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
        ensure_live(&transaction, scope)?;
        // GC between batches is a miss, never a silent gap (spec §9).
        if let Some(miss) = ineligible::<S>(&transaction, &namespace, scope, &parent)? {
            return self.fail(miss);
        }
        let mut statement = transaction
            .prepare_cached(S::ROWS_SQL)
            .map_err(|error| classify(&error))?;
        let mut rows = statement
            .query(params![
                namespace,
                epoch(scope),
                parent,
                self.next_ordinal as i64,
                MAX_ROW_BATCH_ROWS as i64
            ])
            .map_err(|error| classify(&error))?;
        // The witness advances in locals and is committed to `self` only
        // when the batch is handed out, so a bypass mid-batch leaves the
        // reader where the caller is and a retry reads the same rows.
        let mut source = self.source.clone();
        let mut next_ordinal = self.next_ordinal;
        let mut read_bytes = self.read_bytes;
        let mut items = Vec::new();
        let mut bytes = 0usize;
        loop {
            let row = match rows.next() {
                Ok(Some(row)) => row,
                Ok(None) => break,
                Err(error) => return classify_read(&error).and_then(|miss| self.fail(miss)),
            };
            let ordinal = match row.get::<_, i64>(0) {
                Ok(ordinal) => ordinal,
                Err(error) => return classify_read(&error).and_then(|miss| self.fail(miss)),
            };
            if ordinal != next_ordinal as i64 {
                return self.fail(Miss::Malformed(Malformed::Ordinal));
            }
            // A row past the declared count is a count miss whatever else
            // it says, so a decoder never judges rows the header excludes.
            if next_ordinal >= self.expected.count {
                return self.fail(Miss::Malformed(Malformed::Count));
            }
            let size = match S::size(row) {
                Ok(size) => size,
                Err(error) => return classify_read(&error).and_then(|miss| self.fail(miss)),
            };
            if size > MAX_ROW_BATCH_BYTES {
                return self.fail(Miss::Oversized);
            }
            if bytes + size > MAX_ROW_BATCH_BYTES {
                break;
            }
            let item = match source.take(row) {
                Ok(item) => item,
                Err(RowError::Miss(miss)) => return self.fail(miss),
                Err(RowError::Sql(error)) => {
                    return classify_read(&error).and_then(|miss| self.fail(miss));
                }
            };
            bytes += size;
            read_bytes += size as u64;
            items.push(item);
            next_ordinal += 1;
        }
        if items.is_empty() {
            // Rows the header promises are missing.
            return self.fail(Miss::Malformed(Malformed::Count));
        }
        scratch.shrink_to(bytes as u64);
        self.source = source;
        self.next_ordinal = next_ordinal;
        self.read_bytes = read_bytes;
        Ok(Lookup::Hit(Some(Batch { items, scratch })))
    }

    /// [`Self::next_batch`], with a batch handed out as the caller's
    /// public batch type.
    pub(crate) fn next_batch_as<B>(
        &mut self,
        wrap: impl FnOnce(Vec<S::Item>, ScratchLease) -> B,
    ) -> Result<Lookup<Option<B>>, Bypass> {
        Ok(match self.next_batch()? {
            Lookup::Hit(Some(Batch { items, scratch })) => Lookup::Hit(Some(wrap(items, scratch))),
            Lookup::Hit(None) => Lookup::Hit(None),
            Lookup::Miss(miss) => Lookup::Miss(miss),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_errors_from_sql_conversions_are_sql_errors() {
        let error: RowError = rusqlite::types::FromSqlError::InvalidType.into();
        assert!(matches!(error, RowError::Sql(_)));
    }
}
