//! The snapshot queries `l4d-zf1b.9` needs (spec §6.1 indexes, §8.2): a
//! member by its source identity and the importers of a target, both
//! restricted to one sealed snapshot. Hits are candidates: their records
//! are unverified until read, and a target outside the snapshot (a library
//! unit) is simply absent.

use super::Store;
use super::limits::MAX_INDEX_PAGE;
use super::namespaces::{ensure_live, epoch};
use super::outcome::{Bypass, Lookup, Malformed, Miss};
use super::snapshots::{SnapshotHeader, SnapshotMember};
use super::sqlite::{begin, classify, classify_read};
use crate::Digest;
use crate::evidence::path_key;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::path::Path;

/// The member of one sealed snapshot with a source key, through the
/// snapshot's `UNIQUE (snapshot, source_key)` index. `pub` only so
/// `test-support` can re-export it.
pub const MEMBER_QUERY: &str = "SELECT m.ordinal, m.record_id, m.source_key \
     FROM snapshot_members m CROSS JOIN snapshots s \
     WHERE m.namespace_id = ?1 AND m.reset_epoch = ?2 AND m.snapshot_id = ?3 \
     AND m.source_key = ?4 \
     AND s.namespace_id = ?1 AND s.reset_epoch = ?2 AND s.snapshot_id = ?3 AND s.state = 2";

/// Precise bindings to a target key whose record is a member of one sealed
/// snapshot, keyset-paginated over `(record_id, ordinal)`. `CROSS JOIN`
/// pins the order: `member_bindings_by_target` drives and serves the sort.
pub const IMPORTERS_QUERY: &str = "SELECT b.record_id, b.ordinal, m.ordinal \
     FROM member_bindings b CROSS JOIN snapshot_members m CROSS JOIN snapshots s \
     WHERE b.namespace_id = ?1 AND b.reset_epoch = ?2 AND b.target_key = ?4 \
     AND (b.record_id, b.ordinal) > (?5, ?6) \
     AND m.namespace_id = ?1 AND m.reset_epoch = ?2 AND m.record_id = b.record_id \
     AND m.snapshot_id = ?3 \
     AND s.namespace_id = ?1 AND s.reset_epoch = ?2 AND s.snapshot_id = ?3 AND s.state = 2 \
     ORDER BY b.record_id, b.ordinal LIMIT ?7";

/// One binding of a member that targets the queried identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ImporterHit {
    pub record_id: Digest,
    pub binding_ordinal: u64,
    /// The importer's position in the snapshot.
    pub member_ordinal: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImporterPage {
    pub hits: Vec<ImporterHit>,
    /// Pass back as `after` for the next page; `None` on the last page.
    pub next: Option<ImporterHit>,
}

impl Store {
    /// The member of `snapshot` whose canonical source path is `source`
    /// (spec §5 amendment: binding targets resolve this way).
    pub fn find_member(
        &self,
        snapshot: &SnapshotHeader,
        source: &Path,
    ) -> Result<Lookup<SnapshotMember>, Bypass> {
        if snapshot.scope != self.scope {
            return Err(Bypass::Fenced);
        }
        let mut reader = self.reader()?;
        let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
        ensure_live(&transaction, self.scope)?;
        let row = transaction
            .prepare_cached(MEMBER_QUERY)
            .and_then(|mut statement| {
                statement
                    .query_row(
                        params![
                            self.scope.namespace.as_bytes().as_slice(),
                            epoch(self.scope),
                            snapshot.id.as_bytes().as_slice(),
                            path_key(source)
                        ],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, Vec<u8>>(1)?,
                                row.get::<_, Vec<u8>>(2)?,
                            ))
                        },
                    )
                    .optional()
            });
        let (ordinal, record, source_key) = match row {
            Ok(Some(row)) => row,
            Ok(None) => return Ok(Lookup::Miss(Miss::Absent)),
            Err(error) => return classify_read(&error).map(Lookup::Miss),
        };
        let (Ok(ordinal), Ok(record)) = (
            u64::try_from(ordinal),
            <[u8; 32]>::try_from(record.as_slice()),
        ) else {
            return Ok(Lookup::Miss(Miss::Malformed(Malformed::Snapshot)));
        };
        Ok(Lookup::Hit(SnapshotMember {
            ordinal,
            record_id: Digest::from_bytes(record),
            source_key,
        }))
    }

    /// The members of `snapshot` with a precise binding to the canonical
    /// path `target`, in `(record_id, binding_ordinal)` order after `after`,
    /// at most `limit` (clamped to 1..=256).
    pub fn find_importers(
        &self,
        snapshot: &SnapshotHeader,
        target: &Path,
        after: Option<ImporterHit>,
        limit: usize,
    ) -> Result<ImporterPage, Bypass> {
        if snapshot.scope != self.scope {
            return Err(Bypass::Fenced);
        }
        let limit = limit.clamp(1, MAX_INDEX_PAGE);
        let fail = |error: rusqlite::Error| classify(&error);
        let (after_record, after_ordinal) = match after {
            Some(hit) => (
                hit.record_id.as_bytes().to_vec(),
                hit.binding_ordinal as i64,
            ),
            None => (Vec::new(), -1),
        };
        let mut reader = self.reader()?;
        let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
        ensure_live(&transaction, self.scope)?;
        let mut statement = transaction.prepare_cached(IMPORTERS_QUERY).map_err(fail)?;
        let rows = statement
            .query_map(
                params![
                    self.scope.namespace.as_bytes().as_slice(),
                    epoch(self.scope),
                    snapshot.id.as_bytes().as_slice(),
                    path_key(target),
                    after_record,
                    after_ordinal,
                    (limit + 1) as i64
                ],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .map_err(fail)?;
        let mut hits = Vec::with_capacity(limit + 1);
        for row in rows {
            let (record, binding_ordinal, member_ordinal) = row.map_err(fail)?;
            let record_id = <[u8; 32]>::try_from(record.as_slice())
                .map(Digest::from_bytes)
                .map_err(|_| Bypass::Corrupt)?;
            hits.push(ImporterHit {
                record_id,
                binding_ordinal: u64::try_from(binding_ordinal).map_err(|_| Bypass::Corrupt)?,
                member_ordinal: u64::try_from(member_ordinal).map_err(|_| Bypass::Corrupt)?,
            });
        }
        let next = if hits.len() > limit {
            hits.truncate(limit);
            hits.last().copied()
        } else {
            None
        };
        Ok(ImporterPage { hits, next })
    }
}
