//! The snapshot queries `l4d-zf1b.9` needs (spec §6.1 indexes, §8.2): a
//! member by its source identity and the importers of a target, both
//! restricted to one sealed snapshot. Importer hits are dependants to
//! re-validate, not proven importers (D47); hits are candidates: their records
//! are unverified until read, and a target outside the snapshot (a library
//! unit) is simply absent. A snapshot retired or deleted since its header
//! was read is a miss, never an empty answer (spec §9).

use super::Store;
use super::health::Access;
use super::limits::{MAX_INDEX_KEY_BYTES, MAX_INDEX_PAGE};
use super::namespaces::{LIVE, ensure_live, epoch};
use super::outcome::{Bypass, Lookup, Malformed, Miss};
use super::records::{MemberRecordHeader, UNBOUNDED_TARGET_KEY, member_record_header};
use super::snapshots::{SnapshotHeader, SnapshotMember, snapshot_ineligible};
use super::sqlite::{begin, classify_read};
use crate::Digest;
use crate::evidence::path_key;
use crate::path::path_bound;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::path::Path;

/// The member of one snapshot with a source key, through the snapshot's
/// `UNIQUE (snapshot, source_key)` index; the caller has checked the
/// snapshot is sealed in the same transaction. The record ID's and the
/// key's lengths come first, so an over-long one is refused before it is
/// read. `pub` only so `test-support` can re-export it.
pub const MEMBER_QUERY: &str = "SELECT m.ordinal, length(m.record_id), m.record_id, \
     length(m.source_key), m.source_key \
     FROM snapshot_members m CROSS JOIN member_records r \
     WHERE m.namespace_id = ?1 AND m.reset_epoch = ?2 AND m.snapshot_id = ?3 \
     AND m.source_key = ?4 \
     AND r.namespace_id = ?1 AND r.reset_epoch = ?2 AND r.record_id = m.record_id \
     AND r.lifecycle = ?5";

/// Bindings to one target key (`binding_targets_by_target`) whose record is
/// a member of one snapshot (checked sealed by the caller), keyset-paginated
/// over `(record_id, ordinal)`. Retired records are excluded, as
/// `read_snapshot` treats them as a miss. The record ID's length comes
/// first, as in `MEMBER_QUERY`. `CROSS JOIN` pins the order:
/// `binding_targets_by_target` drives and serves the sort. Run once with the
/// target's own key, then with `UNBOUNDED_TARGET_KEY`.
pub const IMPORTERS_QUERY: &str = "SELECT t.dependency, length(t.record_id), t.record_id, \
     t.ordinal, m.ordinal \
     FROM binding_targets t CROSS JOIN snapshot_members m CROSS JOIN member_records r \
     WHERE t.namespace_id = ?1 AND t.reset_epoch = ?2 AND t.target_key = ?4 \
     AND (t.record_id, t.ordinal) > (?5, ?6) \
     AND m.namespace_id = ?1 AND m.reset_epoch = ?2 AND m.record_id = t.record_id \
     AND m.snapshot_id = ?3 \
     AND r.namespace_id = ?1 AND r.reset_epoch = ?2 AND r.record_id = t.record_id \
     AND r.lifecycle = ?8 \
     ORDER BY t.record_id, t.ordinal LIMIT ?7";

/// How a member depends on the queried target (D47). Only `Precise` names
/// the target as the resolver's single outcome; `Ambiguous` lists it among
/// at most 16 candidates; `Unbounded` could depend on any target. All are
/// candidates to re-validate, never proven importers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Dependency {
    Precise,
    Ambiguous,
    Unbounded,
}

/// One binding of a member that may depend on the queried identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ImporterHit {
    pub record_id: Digest,
    pub binding_ordinal: u64,
    /// The importer's position in the snapshot.
    pub member_ordinal: u64,
    pub dependency: Dependency,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImporterPage {
    pub hits: Vec<ImporterHit>,
    /// Pass back as `after` for the next page; `None` on the last page.
    pub next: Option<ImporterHit>,
}

impl Store {
    /// The member of `snapshot` whose canonical source path is `source`
    /// (spec §5 amendment: binding targets resolve this way). Its result is
    /// charged to the scratch while it is built; a member the caller
    /// retains is the caller's to admit.
    pub fn find_member(
        &self,
        snapshot: &SnapshotHeader,
        source: &Path,
    ) -> Result<Lookup<SnapshotMember>, Bypass> {
        self.with_member(snapshot, source, |_, member| Ok(member))
    }

    /// `find_member`, then `open_member_record` of the member's record, in
    /// one read transaction (13f: a restart through a held snapshot looks
    /// up every member this way).
    pub fn find_member_record(
        &self,
        snapshot: &SnapshotHeader,
        source: &Path,
    ) -> Result<Lookup<(SnapshotMember, Lookup<MemberRecordHeader>)>, Bypass> {
        self.with_member(snapshot, source, |transaction, member| {
            let record = member_record_header(transaction, self.scope, member.record_id)?;
            Ok((member, record))
        })
    }

    /// `then` over the member `find_member` finds, in its transaction.
    fn with_member<T>(
        &self,
        snapshot: &SnapshotHeader,
        source: &Path,
        then: impl FnOnce(&rusqlite::Connection, SnapshotMember) -> Result<T, Bypass>,
    ) -> Result<Lookup<T>, Bypass> {
        self.guarded(Access::Read, || {
            if snapshot.scope != self.scope {
                return Err(Bypass::Fenced);
            }
            if over_long(source) {
                return Ok(Lookup::Miss(Miss::Absent));
            }
            // The member and its source key, charged before they are read.
            let Some(_scratch) = self
                .scratch
                .reserve((std::mem::size_of::<SnapshotMember>() + MAX_INDEX_KEY_BYTES) as u64)
            else {
                return Err(Bypass::ScratchExhausted);
            };
            let mut reader = self.reader()?;
            let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
            ensure_live(&transaction, self.scope)?;
            match self.member_in(&transaction, snapshot, source)? {
                Lookup::Hit(member) => then(&transaction, member).map(Lookup::Hit),
                Lookup::Miss(miss) => Ok(Lookup::Miss(miss)),
            }
        })
    }

    /// The member of `snapshot` at `source`, in a transaction that checked
    /// the scope is live.
    fn member_in(
        &self,
        transaction: &rusqlite::Connection,
        snapshot: &SnapshotHeader,
        source: &Path,
    ) -> Result<Lookup<SnapshotMember>, Bypass> {
        if let Some(miss) = snapshot_ineligible(transaction, self.scope, snapshot.id)? {
            return Ok(Lookup::Miss(miss));
        }
        let row = transaction
            .prepare_cached(MEMBER_QUERY)
            .and_then(|mut statement| {
                statement
                    .query_row(
                        params![
                            self.scope.namespace.as_bytes().as_slice(),
                            epoch(self.scope),
                            snapshot.id.as_bytes().as_slice(),
                            path_key(source),
                            LIVE
                        ],
                        |row| {
                            let record = record_id(row, 1)?;
                            let length = row.get::<_, i64>(3)?;
                            let source_key = usize::try_from(length)
                                .is_ok_and(|length| length <= MAX_INDEX_KEY_BYTES)
                                .then(|| row.get::<_, Vec<u8>>(4))
                                .transpose()?;
                            Ok((row.get::<_, i64>(0)?, record, source_key))
                        },
                    )
                    .optional()
            });
        let (ordinal, record, source_key) = match row {
            Ok(Some(row)) => row,
            Ok(None) => return Ok(Lookup::Miss(Miss::Absent)),
            Err(error) => return classify_read(&error).map(Lookup::Miss),
        };
        let (Ok(ordinal), Some(record), Some(source_key)) =
            (u64::try_from(ordinal), record, source_key)
        else {
            return Ok(Lookup::Miss(Miss::Malformed(Malformed::Snapshot)));
        };
        Ok(Lookup::Hit(SnapshotMember {
            ordinal,
            record_id: Digest::from_bytes(record),
            source_key,
        }))
    }

    /// The members of `snapshot` that may depend on the canonical path
    /// `target`, per binding, in two keyset phases (D47). Phase 1 holds the
    /// bindings that name `target` (`Precise`, `Ambiguous`), phase 2 the
    /// `Unbounded` ones (an incomplete or overflowed binding, a dependant of
    /// any target); each is in `(record_id, binding_ordinal)` order. An
    /// `after` hit that is `Unbounded` resumes phase 2, any other phase 1,
    /// and a page that exhausts phase 1 continues into phase 2. A record
    /// with bindings in both phases is a hit in each: callers dedupe by
    /// record. At most `limit` (clamped to 1..=256) per page. Each page
    /// checks the snapshot is still sealed: one retired between pages is a
    /// miss, so a caller never mistakes a truncated set for every dependant.
    /// Its result is charged to the scratch while it is built; a page the
    /// caller retains is the caller's to admit.
    pub fn find_importers(
        &self,
        snapshot: &SnapshotHeader,
        target: &Path,
        after: Option<ImporterHit>,
        limit: usize,
    ) -> Result<Lookup<ImporterPage>, Bypass> {
        self.guarded(Access::Read, || {
            if snapshot.scope != self.scope {
                return Err(Bypass::Fenced);
            }
            if over_long(target) {
                return Ok(Lookup::Miss(Miss::Absent));
            }
            let limit = limit.clamp(1, MAX_INDEX_PAGE);
            // The page, charged before it is allocated (spec §10).
            let Some(_scratch) = self
                .scratch
                .reserve(((limit + 1) * std::mem::size_of::<ImporterHit>()) as u64)
            else {
                return Err(Bypass::ScratchExhausted);
            };
            // No binding ordinal exceeds `i64::MAX`, so a larger (forged) one
            // means "after every binding of this record", never a wrap.
            let cursor = |hit: ImporterHit| {
                (
                    hit.record_id.as_bytes().to_vec(),
                    i64::try_from(hit.binding_ordinal).unwrap_or(i64::MAX),
                )
            };
            let resuming_unbounded =
                after.is_some_and(|hit| hit.dependency == Dependency::Unbounded);
            let start = || (Vec::new(), -1);
            let mut reader = self.reader()?;
            let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
            ensure_live(&transaction, self.scope)?;
            if let Some(miss) = snapshot_ineligible(&transaction, self.scope, snapshot.id)? {
                return Ok(Lookup::Miss(miss));
            }
            let read = |error: rusqlite::Error| classify_read(&error).map(Lookup::Miss);
            let mut hits: Vec<ImporterHit> = Vec::with_capacity(limit + 1);
            let own_key = path_key(target);
            let phases = [(own_key.as_slice(), false), (UNBOUNDED_TARGET_KEY, true)];
            for (key, unbounded) in phases {
                if !unbounded && resuming_unbounded {
                    continue;
                }
                if hits.len() > limit {
                    break;
                }
                let (after_record, after_ordinal) = match after {
                    Some(hit) if (hit.dependency == Dependency::Unbounded) == unbounded => {
                        cursor(hit)
                    }
                    _ => start(),
                };
                let mut statement = match transaction.prepare_cached(IMPORTERS_QUERY) {
                    Ok(statement) => statement,
                    Err(error) => return read(error),
                };
                let rows = statement.query_map(
                    params![
                        self.scope.namespace.as_bytes().as_slice(),
                        epoch(self.scope),
                        snapshot.id.as_bytes().as_slice(),
                        key,
                        after_record,
                        after_ordinal,
                        (limit + 1 - hits.len()) as i64,
                        LIVE
                    ],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            record_id(row, 1)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, i64>(4)?,
                        ))
                    },
                );
                let rows = match rows {
                    Ok(rows) => rows,
                    Err(error) => return read(error),
                };
                for row in rows {
                    let (dependency, record, binding_ordinal, member_ordinal) = match row {
                        Ok(row) => row,
                        Err(error) => return read(error),
                    };
                    let dependency = match (dependency, unbounded) {
                        (1, false) => Dependency::Precise,
                        (2, false) => Dependency::Ambiguous,
                        (3, true) => Dependency::Unbounded,
                        _ => return Ok(Lookup::Miss(Miss::Malformed(Malformed::Snapshot))),
                    };
                    let (Some(record), Ok(binding_ordinal), Ok(member_ordinal)) = (
                        record,
                        u64::try_from(binding_ordinal),
                        u64::try_from(member_ordinal),
                    ) else {
                        return Ok(Lookup::Miss(Miss::Malformed(Malformed::Snapshot)));
                    };
                    hits.push(ImporterHit {
                        record_id: Digest::from_bytes(record),
                        binding_ordinal,
                        member_ordinal,
                        dependency,
                    });
                }
            }
            let next = if hits.len() > limit {
                hits.truncate(limit);
                hits.last().copied()
            } else {
                None
            };
            Ok(Lookup::Hit(ImporterPage { hits, next }))
        })
    }
}

/// Whether `path`'s key would exceed any key the store writes, from its
/// bound, so no key is built for a path that cannot be stored. On Windows the
/// bound overestimates non-ASCII paths, so one near the limit is a safe false
/// miss; the store is bypassed there until `l4d-x2ft`.
fn over_long(path: &Path) -> bool {
    path_bound(path) > MAX_INDEX_KEY_BYTES
}

/// The record ID after its length at `length`, the length read first: an
/// ID of any other length is `None` and never loaded.
fn record_id(row: &rusqlite::Row<'_>, length: usize) -> rusqlite::Result<Option<[u8; 32]>> {
    if row.get::<_, i64>(length)? != 32 {
        return Ok(None);
    }
    Ok(<[u8; 32]>::try_from(row.get::<_, Vec<u8>>(length + 1)?.as_slice()).ok())
}
