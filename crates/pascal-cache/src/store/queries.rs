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
use super::namespaces::{LIVE, ensure_live, epoch, scope_ref};
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

/// The source keys of one snapshot's live members whose live artifact has a
/// row of kind `?5` matching the key filter `{filter}` (`?6` the needle),
/// at most `?8` (l4d-c8l2). `CROSS JOIN` pins the order: the scope's rows of
/// the kind drive through `artifact_rows_by_index_key`, then the artifact,
/// its records (`member_records_by_artifact`) and their membership
/// (`snapshot_members_by_record`); the partial artifact index is named, as
/// the planner otherwise walks every live record per row. `pub` only so
/// `test-support` can re-export it.
pub fn members_with_rows_query(filter: &str) -> String {
    format!(
        "SELECT DISTINCT length(m.source_key), m.source_key \
         FROM artifact_rows r CROSS JOIN artifacts a \
         CROSS JOIN member_records mr INDEXED BY member_records_by_artifact \
         CROSS JOIN snapshot_members m \
         WHERE r.scope_ref = ?4 AND r.row_kind = ?5 AND r.index_key IS NOT NULL AND {filter} \
         AND a.artifact_ref = r.artifact_ref AND a.namespace_id = ?1 AND a.reset_epoch = ?2 \
         AND a.lifecycle = ?7 \
         AND mr.namespace_id = ?1 AND mr.reset_epoch = ?2 AND mr.artifact_id = a.artifact_id \
         AND mr.artifact_id IS NOT NULL AND mr.lifecycle = ?7 \
         AND m.namespace_id = ?1 AND m.reset_epoch = ?2 AND m.record_id = mr.record_id \
         AND m.snapshot_id = ?3 \
         LIMIT ?8"
    )
}

/// A key holding the needle bytes, or any key that is not ASCII: the
/// needle is matched bytewise, so a caller matching case-insensitively
/// passes it ASCII-lowercased against ASCII-lowercased keys and keeps every
/// key it could not compare that way.
/// A scan covers one page of keys, `?9 <= key < ?10`, so a caller can
/// stop between pages.
pub const CONTAINING_FILTER: &str = "r.index_key >= ?9 AND r.index_key < ?10 \
     AND (instr(r.index_key, ?6) > 0 \
     OR length(CAST(r.index_key AS TEXT)) <> length(r.index_key))";
pub const EQUAL_FILTER: &str = "r.index_key = ?6 AND ?9 IS NOT NULL AND ?10 IS NOT NULL";

/// The source keys of one snapshot's live members whose live record has a
/// live artifact (l4d-c8l2): the members whose stored rows a
/// `find_members_with_rows` answer covers. `CROSS JOIN` pins the order:
/// the snapshot's members by their primary key, their records, then the
/// artifact by its unique key. `pub` only so `test-support` can re-export it.
pub const MEMBERS_WITH_ARTIFACTS_QUERY: &str = "SELECT length(m.source_key), m.source_key \
     FROM snapshot_members m CROSS JOIN member_records mr CROSS JOIN artifacts a \
     WHERE m.namespace_id = ?1 AND m.reset_epoch = ?2 AND m.snapshot_id = ?3 \
     AND mr.namespace_id = ?1 AND mr.reset_epoch = ?2 AND mr.record_id = m.record_id \
     AND mr.lifecycle = ?4 AND mr.artifact_id IS NOT NULL \
     AND a.namespace_id = ?1 AND a.reset_epoch = ?2 AND a.artifact_id = mr.artifact_id \
     AND a.lifecycle = ?4";

/// The source keys of one snapshot's live members whose live member
/// evidence observed the canonical path `?4` (l4d-c8l2): an include, or any
/// other payload, their stored facts were extracted from. `CROSS JOIN` pins
/// the order: the observations of the path (`observation_content_by_path`),
/// the evidence sets holding them, the records naming those as member
/// evidence (the partial `member_records_by_evidence`, named), and their
/// membership. `pub` only so `test-support` can re-export it.
pub const MEMBERS_OBSERVING_QUERY: &str = "SELECT DISTINCT length(m.source_key), m.source_key \
     FROM observation_content o CROSS JOIN evidence_observations eo \
     CROSS JOIN evidence_sets e \
     CROSS JOIN member_records mr INDEXED BY member_records_by_evidence \
     CROSS JOIN snapshot_members m \
     WHERE o.namespace_id = ?1 AND o.reset_epoch = ?2 AND o.path_key = ?4 \
     AND eo.observation = o.observation \
     AND e.evidence_ref = eo.evidence_ref AND e.lifecycle = ?5 \
     AND mr.namespace_id = ?1 AND mr.reset_epoch = ?2 AND mr.member_evidence = e.evidence_id \
     AND mr.member_evidence IS NOT NULL AND mr.lifecycle = ?5 \
     AND m.namespace_id = ?1 AND m.reset_epoch = ?2 AND m.record_id = mr.record_id \
     AND m.snapshot_id = ?3";

/// What `find_members_with_rows` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MembersWithRows {
    /// The matching members' source keys, in no particular order.
    Found(Vec<Vec<u8>>),
    /// More than the limit match.
    OverLimit,
    /// The caller's `stop` asked to end the scan.
    Stopped,
}

/// One page of a row scan: the needle, then `low <= key < high`.
type KeyPage<'a> = (&'a [u8], Vec<u8>, Vec<u8>);

/// How often the member listings check `stop`, in rows.
const STOP_EVERY_ROWS: usize = 1024;

/// Which index keys `find_members_with_rows` keeps.
#[derive(Debug, Clone, Copy)]
pub enum IndexKeyMatch<'a> {
    /// Keys containing these bytes, and every key that is not ASCII.
    Containing(&'a [u8]),
    /// Keys equal to one of these.
    Equal(&'a [&'a [u8]]),
}

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

    /// The canonical source keys (`path_key`) of the live members of
    /// `snapshot` whose live artifact holds a row of `row_kind` whose index
    /// key `matching` keeps (l4d-c8l2). A hit selects a candidate to
    /// re-validate: the artifact and record are unverified until read (spec
    /// §6.1, §7), and a member absent from the answer is proven nothing.
    /// A `Containing` scan runs in pages of keys by their first byte and
    /// asks `stop` before each page; `Equal` asks before each key. Each kept
    /// key is charged to the scratch as it is read.
    pub fn find_members_with_rows(
        &self,
        snapshot: &SnapshotHeader,
        row_kind: u8,
        matching: IndexKeyMatch<'_>,
        limit: usize,
        stop: &dyn Fn() -> bool,
    ) -> Result<Lookup<MembersWithRows>, Bypass> {
        self.guarded(Access::Read, || {
            if snapshot.scope != self.scope {
                return Err(Bypass::Fenced);
            }
            let mut reader = self.reader()?;
            let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
            ensure_live(&transaction, self.scope)?;
            if let Some(miss) = snapshot_ineligible(&transaction, self.scope, snapshot.id)? {
                return Ok(Lookup::Miss(miss));
            }
            let Some(scope_ref) = scope_ref(&transaction, self.scope)? else {
                return Ok(Lookup::Hit(MembersWithRows::Found(Vec::new())));
            };
            let read = |error: rusqlite::Error| classify_read(&error).map(Lookup::Miss);
            // (needle, low, high): `low <= key < high`; the last page's high
            // bound is past any stored key.
            let past_every_key = vec![0xff_u8; MAX_INDEX_KEY_BYTES + 1];
            let (filter, pages): (&str, Vec<KeyPage<'_>>) = match matching {
                IndexKeyMatch::Containing(needle) => (
                    CONTAINING_FILTER,
                    (0..=u8::MAX)
                        .map(|first| {
                            let low = if first == 0 { Vec::new() } else { vec![first] };
                            let high = match first.checked_add(1) {
                                Some(next) => vec![next],
                                None => past_every_key.clone(),
                            };
                            (needle, low, high)
                        })
                        .collect(),
                ),
                IndexKeyMatch::Equal(keys) => (
                    EQUAL_FILTER,
                    keys.iter()
                        .map(|key| (*key, Vec::new(), Vec::new()))
                        .collect(),
                ),
            };
            let query = members_with_rows_query(filter);
            let mut found = std::collections::HashSet::<Vec<u8>>::new();
            let mut leases = Vec::new();
            for (needle, low, high) in pages {
                if stop() {
                    return Ok(Lookup::Hit(MembersWithRows::Stopped));
                }
                if needle.len() > MAX_INDEX_KEY_BYTES {
                    continue;
                }
                let mut statement = match transaction.prepare_cached(&query) {
                    Ok(statement) => statement,
                    Err(error) => return read(error),
                };
                let rows = statement.query_map(
                    params![
                        self.scope.namespace.as_bytes().as_slice(),
                        epoch(self.scope),
                        snapshot.id.as_bytes().as_slice(),
                        scope_ref,
                        i64::from(row_kind),
                        needle,
                        LIVE,
                        i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX),
                        low,
                        high
                    ],
                    source_key,
                );
                let rows = match rows {
                    Ok(rows) => rows,
                    Err(error) => return read(error),
                };
                for row in rows {
                    let source = match row {
                        Ok(Some(source)) => source,
                        Ok(None) => return Ok(Lookup::Miss(Miss::Malformed(Malformed::Snapshot))),
                        Err(error) => return read(error),
                    };
                    if found.contains(&source) {
                        continue;
                    }
                    if found.len() >= limit {
                        return Ok(Lookup::Hit(MembersWithRows::OverLimit));
                    }
                    let Some(lease) = self.scratch.reserve(source.len() as u64) else {
                        return Err(Bypass::ScratchExhausted);
                    };
                    leases.push(lease);
                    found.insert(source);
                }
            }
            Ok(Lookup::Hit(MembersWithRows::Found(
                found.into_iter().collect(),
            )))
        })
    }

    /// The canonical source keys of the live members of `snapshot` whose
    /// live record has a live artifact: those whose stored rows
    /// `find_members_with_rows` searched (l4d-c8l2). A member without one
    /// (an artifact-less refusal or over-size record) has no stored index.
    /// `None` when `stop` asked to end the listing (checked every 1,024
    /// rows). Each key is charged to the scratch as it is read.
    pub fn find_members_with_artifacts(
        &self,
        snapshot: &SnapshotHeader,
        stop: &dyn Fn() -> bool,
    ) -> Result<Lookup<Option<Vec<Vec<u8>>>>, Bypass> {
        self.members_of(snapshot, MEMBERS_WITH_ARTIFACTS_QUERY, None, stop)
    }

    /// The canonical source keys of the live members of `snapshot` whose
    /// live member evidence observed `path` (l4d-c8l2): the members whose
    /// stored facts a change to `path`, an include for instance, may make
    /// stale. `None` when `stop` asked to end the listing.
    pub fn find_members_observing(
        &self,
        snapshot: &SnapshotHeader,
        path: &Path,
        stop: &dyn Fn() -> bool,
    ) -> Result<Lookup<Option<Vec<Vec<u8>>>>, Bypass> {
        if over_long(path) {
            return Ok(Lookup::Hit(Some(Vec::new())));
        }
        self.members_of(
            snapshot,
            MEMBERS_OBSERVING_QUERY,
            Some(path_key(path)),
            stop,
        )
    }

    /// The source keys `query` lists for `snapshot`, its fourth parameter
    /// `key` (else `LIVE`) and its fifth `LIVE`.
    fn members_of(
        &self,
        snapshot: &SnapshotHeader,
        query: &str,
        key: Option<Vec<u8>>,
        stop: &dyn Fn() -> bool,
    ) -> Result<Lookup<Option<Vec<Vec<u8>>>>, Bypass> {
        self.guarded(Access::Read, || {
            if snapshot.scope != self.scope {
                return Err(Bypass::Fenced);
            }
            let mut reader = self.reader()?;
            let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
            ensure_live(&transaction, self.scope)?;
            if let Some(miss) = snapshot_ineligible(&transaction, self.scope, snapshot.id)? {
                return Ok(Lookup::Miss(miss));
            }
            let read = |error: rusqlite::Error| classify_read(&error).map(Lookup::Miss);
            let mut statement = match transaction.prepare_cached(query) {
                Ok(statement) => statement,
                Err(error) => return read(error),
            };
            let namespace = self.scope.namespace.as_bytes().as_slice();
            let id = snapshot.id.as_bytes().as_slice();
            let rows = match &key {
                Some(key) => statement.query_map(
                    params![namespace, epoch(self.scope), id, key.as_slice(), LIVE],
                    source_key,
                ),
                None => {
                    statement.query_map(params![namespace, epoch(self.scope), id, LIVE], source_key)
                }
            };
            let rows = match rows {
                Ok(rows) => rows,
                Err(error) => return read(error),
            };
            let mut found = Vec::new();
            let mut leases = Vec::new();
            for (at, row) in rows.enumerate() {
                if at % STOP_EVERY_ROWS == 0 && stop() {
                    return Ok(Lookup::Hit(None));
                }
                let source = match row {
                    Ok(Some(source)) => source,
                    Ok(None) => return Ok(Lookup::Miss(Miss::Malformed(Malformed::Snapshot))),
                    Err(error) => return read(error),
                };
                let Some(lease) = self.scratch.reserve(source.len() as u64) else {
                    return Err(Bypass::ScratchExhausted);
                };
                leases.push(lease);
                found.push(source);
            }
            Ok(Lookup::Hit(Some(found)))
        })
    }
}

/// A listed source key after its length: one over any stored key's bound
/// is `None` and never loaded.
fn source_key(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<Vec<u8>>> {
    let length = row.get::<_, i64>(0)?;
    usize::try_from(length)
        .is_ok_and(|length| length <= MAX_INDEX_KEY_BYTES)
        .then(|| row.get::<_, Vec<u8>>(1))
        .transpose()
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
