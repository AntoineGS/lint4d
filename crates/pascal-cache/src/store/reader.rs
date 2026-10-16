use super::Store;
use super::artifacts::{ArtifactHeader, FactHasher, hash_row};
use super::limits::{MAX_ROW_BATCH_BYTES, MAX_ROW_BATCH_ROWS};
use super::namespaces::{ensure_live, epoch};
use super::outcome::{Bypass, Lookup, Malformed, Miss};
use super::sqlite::{begin, classify, classify_read};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::sync::{Arc, Mutex, PoisonError};

/// The store's aggregate transient decode allowance (spec §10).
#[derive(Debug)]
pub(crate) struct ScratchBudget {
    limit: u64,
    used: Mutex<u64>,
}

/// Bytes reserved from the scratch budget until dropped.
#[derive(Debug)]
pub(crate) struct ScratchLease {
    budget: Arc<ScratchBudget>,
    bytes: u64,
}

impl ScratchBudget {
    pub(crate) fn new(limit: u64) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: Mutex::new(0),
        })
    }

    pub(crate) fn used(&self) -> u64 {
        *self.used.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn reserve(self: &Arc<Self>, bytes: u64) -> Option<ScratchLease> {
        let mut used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
        if *used + bytes > self.limit {
            return None;
        }
        *used += bytes;
        Some(ScratchLease {
            budget: Arc::clone(self),
            bytes,
        })
    }
}

impl ScratchLease {
    fn shrink_to(&mut self, bytes: u64) {
        let mut used = self
            .budget
            .used
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *used -= self.bytes - bytes.min(self.bytes);
        self.bytes = bytes.min(self.bytes);
    }
}

impl Drop for ScratchLease {
    fn drop(&mut self) {
        *self
            .budget
            .used
            .lock()
            .unwrap_or_else(PoisonError::into_inner) -= self.bytes;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRow {
    pub ordinal: u64,
    pub row_kind: u8,
    pub index_key: Option<Vec<u8>>,
    pub payload: Vec<u8>,
}

/// At most 256 rows and 1 MiB, charged to the store's scratch budget until
/// dropped.
#[derive(Debug)]
pub struct RowBatch {
    rows: Vec<StoredRow>,
    _scratch: ScratchLease,
}

impl RowBatch {
    pub fn rows(&self) -> &[StoredRow] {
        &self.rows
    }
}

impl PartialEq for RowBatch {
    fn eq(&self, other: &Self) -> bool {
        self.rows == other.rows
    }
}

impl Eq for RowBatch {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Reading,
    Verified,
    Failed(Miss),
}

/// Streams an artifact's rows in bounded batches, each in its own short
/// read transaction, while building its integrity witness.
pub struct ArtifactReader<'a> {
    store: &'a Store,
    header: ArtifactHeader,
    next_ordinal: u64,
    content_bytes: u64,
    hasher: FactHasher,
    state: State,
}

impl Store {
    pub fn read_artifact(&self, header: &ArtifactHeader) -> ArtifactReader<'_> {
        ArtifactReader {
            store: self,
            hasher: FactHasher::new(header.artifact_id, header.row_count, &header.supplemental),
            header: header.clone(),
            next_ordinal: 0,
            content_bytes: 0,
            state: State::Reading,
        }
    }

    /// Scratch bytes currently held by row batches.
    pub fn scratch_in_use(&self) -> u64 {
        self.scratch.used()
    }
}

impl ArtifactReader<'_> {
    /// Whether every row was read and the counts, sizes and fact checksum
    /// agree with the header. Only then may the rows exclude anything.
    pub fn is_verified(&self) -> bool {
        self.state == State::Verified
    }

    /// The artifact's supplemental bytes, once the whole read verified.
    /// Before that they are as unverified as the rows.
    pub fn supplemental(&self) -> Option<&[u8]> {
        self.is_verified()
            .then_some(self.header.supplemental.as_slice())
    }

    fn fail(&mut self, miss: Miss) -> Result<Lookup<Option<RowBatch>>, Bypass> {
        self.state = State::Failed(miss);
        Ok(Lookup::Miss(miss))
    }

    /// A row at or past the header's count would be indexed yet never
    /// yielded by a verified read, so it makes the artifact malformed.
    fn trailing_rows(&self) -> Result<Option<Miss>, Bypass> {
        let scope = self.header.scope;
        let mut reader = self.store.reader()?;
        let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
        ensure_live(&transaction, scope)?;
        let found: Option<i64> = transaction
            .query_row(
                "SELECT 1 FROM artifact_rows \
                 WHERE namespace_id = ?1 AND reset_epoch = ?2 AND artifact_id = ?3 AND ordinal >= ?4 \
                 LIMIT 1",
                params![
                    scope.namespace.as_bytes().as_slice(),
                    epoch(scope),
                    self.header.artifact_id.as_bytes().as_slice(),
                    self.header.row_count as i64
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| classify(&error))?;
        Ok(found.map(|_| Miss::Malformed(Malformed::Count)))
    }

    /// The next batch, or `Hit(None)` once every row was read and verified.
    /// Rows are unverified until then. A header from another store (another
    /// scope) is fenced before any SQL.
    pub fn next_batch(&mut self) -> Result<Lookup<Option<RowBatch>>, Bypass> {
        if self.header.scope != self.store.scope() {
            return Err(Bypass::Fenced);
        }
        match self.state {
            State::Verified => return Ok(Lookup::Hit(None)),
            State::Failed(miss) => return Ok(Lookup::Miss(miss)),
            State::Reading => {}
        }
        if self.next_ordinal == self.header.row_count {
            if let Some(miss) = self.trailing_rows()? {
                return self.fail(miss);
            }
            if self.content_bytes == self.header.content_bytes
                && self.hasher.finish() == self.header.fact_checksum
            {
                self.state = State::Verified;
                return Ok(Lookup::Hit(None));
            }
            return self.fail(Miss::ChecksumMismatch);
        }
        // Copy the `&'a Store` out of `self`, so the leased connection does
        // not borrow `self` and `self.fail` stays callable below.
        let store = self.store;
        // A batch's size is known only once its rows are read, so reserve
        // the whole batch allowance up front and shrink to what was used.
        let Some(mut scratch) = store.scratch.reserve(MAX_ROW_BATCH_BYTES as u64) else {
            return Err(Bypass::ScratchExhausted);
        };
        let scope = self.header.scope;
        let artifact_id = self.header.artifact_id;
        let mut reader = store.reader()?;
        let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
        ensure_live(&transaction, scope)?;
        // GC between batches is a miss, never a silent gap (spec §9).
        let lifecycle: Option<i64> = transaction
            .query_row(
                "SELECT lifecycle FROM artifacts \
                 WHERE namespace_id = ?1 AND reset_epoch = ?2 AND artifact_id = ?3",
                params![
                    scope.namespace.as_bytes().as_slice(),
                    epoch(scope),
                    artifact_id.as_bytes().as_slice()
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| classify(&error))?;
        match lifecycle {
            None => return self.fail(Miss::Vanished),
            Some(1) => {}
            Some(_) => return self.fail(Miss::Retired),
        }
        let mut statement = transaction
            .prepare_cached(
                "SELECT ordinal, row_kind, index_key, payload, row_checksum FROM artifact_rows \
                 WHERE namespace_id = ?1 AND reset_epoch = ?2 AND artifact_id = ?3 AND ordinal >= ?4 \
                 ORDER BY ordinal LIMIT ?5",
            )
            .map_err(|error| classify(&error))?;
        let mut rows = statement
            .query(params![
                scope.namespace.as_bytes().as_slice(),
                epoch(scope),
                artifact_id.as_bytes().as_slice(),
                self.next_ordinal as i64,
                MAX_ROW_BATCH_ROWS as i64
            ])
            .map_err(|error| classify(&error))?;
        // The witness advances in locals and is committed to `self` only
        // when the batch is handed out, so a bypass mid-batch leaves it
        // where the caller is and a retry re-reads the same rows.
        let mut hasher = self.hasher.clone();
        let mut next_ordinal = self.next_ordinal;
        let mut content_bytes = self.content_bytes;
        let mut batch = Vec::new();
        let mut bytes = 0usize;
        let fault = store.take_row_fault();
        loop {
            if fault == Some(batch.len()) {
                return Err(Bypass::Sqlite {
                    extended_code: rusqlite::ffi::SQLITE_IOERR,
                });
            }
            let row = match rows.next() {
                Ok(Some(row)) => row,
                Ok(None) => break,
                Err(error) => return classify_read(&error).and_then(|miss| self.fail(miss)),
            };
            // Borrow the values in place, so an oversized row is refused
            // before anything is copied.
            let decoded = (|| -> rusqlite::Result<_> {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get_ref(2)?.as_blob_or_null()?,
                    row.get_ref(3)?.as_blob()?,
                    row.get_ref(4)?.as_blob()?,
                ))
            })();
            let (ordinal, row_kind, index_key, payload, checksum) = match decoded {
                Ok(values) => values,
                Err(error) => return classify_read(&error).and_then(|miss| self.fail(miss)),
            };
            if ordinal != next_ordinal as i64 {
                return self.fail(Miss::Malformed(Malformed::Ordinal));
            }
            let Ok(row_kind) = u8::try_from(row_kind) else {
                return self.fail(Miss::Malformed(Malformed::Row));
            };
            let size = payload.len() + index_key.map_or(0, <[u8]>::len);
            if size > MAX_ROW_BATCH_BYTES {
                return self.fail(Miss::Oversized);
            }
            if bytes + size > MAX_ROW_BATCH_BYTES {
                break;
            }
            let computed = hash_row(
                &mut hasher,
                artifact_id,
                next_ordinal,
                row_kind,
                index_key,
                payload,
            );
            if computed.as_bytes().as_slice() != checksum {
                return self.fail(Miss::ChecksumMismatch);
            }
            bytes += size;
            content_bytes += size as u64;
            batch.push(StoredRow {
                ordinal: next_ordinal,
                row_kind,
                index_key: index_key.map(<[u8]>::to_vec),
                payload: payload.to_vec(),
            });
            next_ordinal += 1;
            if next_ordinal > self.header.row_count {
                return self.fail(Miss::Malformed(Malformed::Count));
            }
        }
        if batch.is_empty() {
            // Rows the header promises are missing.
            return self.fail(Miss::Malformed(Malformed::Count));
        }
        scratch.shrink_to(bytes as u64);
        self.hasher = hasher;
        self.next_ordinal = next_ordinal;
        self.content_bytes = content_bytes;
        Ok(Lookup::Hit(Some(RowBatch {
            rows: batch,
            _scratch: scratch,
        })))
    }
}
