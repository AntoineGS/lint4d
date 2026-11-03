//! Snapshots (spec §6.3 and §9, amendments 2026-10-27). A builder adds
//! member records in short batches whose first write compares and
//! increments the build revision. A seal streams the manifest digest, then
//! compares and sets once. Readers get at most 8 sealed snapshots of a
//! context, newest publication first, as hints, and stream their members
//! under a witness that recomputes the sealed digest. Nothing here says a
//! snapshot is current or verified.

use super::Store;
use super::budget::WriteEstimate;
use super::health::Access;
use super::keys::{admit_manifest, check_manifest_size, put_manifest};
use super::limits::{
    MAX_INDEX_KEY_BYTES, MAX_ROW_BATCH_BYTES, MAX_ROW_BATCH_ROWS, MAX_SNAPSHOT_CANDIDATES,
    MAX_SNAPSHOT_MEMBERS,
};
use super::namespaces::{LIVE, RETIRED as RECORD_RETIRED, Scope, ensure_live, epoch};
use super::nonce::nonce;
use super::outcome::{Bypass, Lookup, Malformed, Miss, SealRefusal, TooLarge};
use super::reader::ScratchLease;
use super::records::require_evidence;
use super::sequence::{Candidates, newest_first, next_publication_seq};
use super::sqlite::{begin, classify};
use super::stream::{Expected, ManifestReader, RowError, RowSource, ineligible};
use crate::manifest::manifest_encoder;
use crate::{
    ContextKey, Digest, EvidencePurpose, KeyKind, MANIFEST_DIGEST_CONTEXT, MemberError,
    SnapshotCoverage, decode_snapshot_coverage,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

/// A snapshot's `state`.
const BUILDING: i64 = 1;
const SEALED: i64 = 2;
const RETIRED: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SnapshotId([u8; 16]);

impl SnapshotId {
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// The right to add members to one building snapshot and seal it. A copy
/// shares the token: whichever writes first advances the revision and
/// fences every other copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotBuilder {
    scope: Scope,
    id: SnapshotId,
    token: [u8; 16],
    revision: u64,
    context: Digest,
}

impl SnapshotBuilder {
    pub fn id(&self) -> SnapshotId {
        self.id
    }

    /// Committed batches so far.
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

/// What a seal records besides the members; both are covered by the
/// sealed digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotSeal {
    pub discovery: Option<Digest>,
    pub coverage: SnapshotCoverage,
}

/// A sealed snapshot: structurally finished history, not current or
/// complete analysis (spec §6.3). Its members are unverified until
/// [`Store::read_snapshot`] has streamed them all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotHeader {
    pub(crate) scope: Scope,
    pub(crate) id: SnapshotId,
    context: Digest,
    member_count: u64,
    discovery: Option<Digest>,
    coverage: SnapshotCoverage,
    sealed_digest: Digest,
    publication_seq: u64,
}

impl SnapshotHeader {
    pub fn id(&self) -> SnapshotId {
        self.id
    }

    pub fn context(&self) -> Digest {
        self.context
    }

    pub fn member_count(&self) -> u64 {
        self.member_count
    }

    pub fn discovery_evidence(&self) -> Option<Digest> {
        self.discovery
    }

    pub fn coverage(&self) -> SnapshotCoverage {
        self.coverage
    }

    /// When it was sealed, store-wide: the candidate order, never freshness.
    pub fn publication_seq(&self) -> u64 {
        self.publication_seq
    }
}

/// One member: a record and the copy of its canonical source path key that
/// resolves binding targets within the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotMember {
    pub ordinal: u64,
    pub record_id: Digest,
    pub source_key: Vec<u8>,
}

/// The sealed digest: context, discovery evidence, coverage, count, then
/// every `(record ID, source key)` in ordinal order.
#[derive(Clone)]
struct SnapshotHasher(blake3::Hasher);

impl SnapshotHasher {
    fn new(
        context: Digest,
        discovery: Option<Digest>,
        coverage: &SnapshotCoverage,
        count: u64,
    ) -> Self {
        let mut encoder = manifest_encoder(KeyKind::Snapshot);
        encoder.field(1);
        encoder.digest(&context);
        encoder.field(2);
        encoder.option(discovery.as_ref(), |encoder, discovery| {
            encoder.digest(discovery)
        });
        encoder.field(3);
        encoder.encode(coverage);
        encoder.field(4);
        encoder.u64(count);
        let mut hasher = blake3::Hasher::new_derive_key(MANIFEST_DIGEST_CONTEXT);
        hasher.update(encoder.as_bytes());
        Self(hasher)
    }

    fn update(&mut self, record: &[u8], source_key: &[u8]) {
        self.0.update(record);
        self.0.update(&(source_key.len() as u64).to_le_bytes());
        self.0.update(source_key);
    }

    fn finish(&self) -> Digest {
        Digest::from_bytes(*self.0.finalize().as_bytes())
    }
}

fn has_extended_code(error: &rusqlite::Error, code: i32) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(failure, _) if failure.extended_code == code)
}

/// The stored member count while `builder` still holds its build: same
/// token, same revision, still building.
fn held_count(connection: &Connection, builder: &SnapshotBuilder) -> rusqlite::Result<Option<u64>> {
    connection
        .query_row(
            "SELECT member_count FROM snapshots WHERE namespace_id = ?1 AND reset_epoch = ?2 \
             AND snapshot_id = ?3 AND builder_token = ?4 AND build_revision = ?5 AND state = ?6",
            params![
                builder.scope.namespace.as_bytes().as_slice(),
                epoch(builder.scope),
                builder.id.0.as_slice(),
                builder.token.as_slice(),
                builder.revision as i64,
                BUILDING
            ],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map(|count| count.map(|count| count as u64))
}

fn record_lifecycle(
    connection: &Connection,
    scope: Scope,
    record: Digest,
) -> rusqlite::Result<Option<i64>> {
    connection
        .query_row(
            "SELECT lifecycle FROM member_records \
             WHERE namespace_id = ?1 AND reset_epoch = ?2 AND record_id = ?3",
            params![
                scope.namespace.as_bytes().as_slice(),
                epoch(scope),
                record.as_bytes().as_slice()
            ],
            |row| row.get(0),
        )
        .optional()
}

/// `snapshot_id, member_count, discovery_evidence, coverage, sealed_digest,
/// publication_seq` as stored.
type SnapshotRow = (
    Vec<u8>,
    i64,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<i64>,
);

fn header_from(scope: Scope, context: Digest, row: SnapshotRow) -> Option<SnapshotHeader> {
    let (id, count, discovery, coverage, sealed_digest, sequence) = row;
    let id = SnapshotId(<[u8; 16]>::try_from(id.as_slice()).ok()?);
    let member_count = u64::try_from(count)
        .ok()
        .filter(|count| *count <= MAX_SNAPSHOT_MEMBERS as u64)?;
    let discovery = match discovery {
        None => None,
        Some(digest) => Some(Digest::from_bytes(
            <[u8; 32]>::try_from(digest.as_slice()).ok()?,
        )),
    };
    let coverage = decode_snapshot_coverage(&coverage?).ok()?;
    let sealed_digest = Digest::from_bytes(<[u8; 32]>::try_from(sealed_digest?.as_slice()).ok()?);
    let publication_seq = u64::try_from(sequence?).ok()?;
    Some(SnapshotHeader {
        scope,
        id,
        context,
        member_count,
        discovery,
        coverage,
        sealed_digest,
        publication_seq,
    })
}

impl Store {
    /// Insert a `building` snapshot of `context` with a fresh ID and builder
    /// token at revision 0 (spec §6.3 amendment, item 2).
    pub fn begin_snapshot(&self, context: &ContextKey) -> Result<SnapshotBuilder, Bypass> {
        self.guarded(Access::Write, || {
            check_manifest_size(context.key())?;
            let id = SnapshotId(nonce());
            let token = nonce();
            let fail = |error: rusqlite::Error| classify(&error);
            let mut writer = self.writer()?;
            let transaction = begin(&mut writer, TransactionBehavior::Immediate)?;
            ensure_live(&transaction, self.scope)?;
            self.admit_write(
                &transaction,
                WriteEstimate {
                    bytes: context.key().manifest().len() as u64 + 128,
                    rows: 2,
                },
            )?;
            put_manifest(&transaction, self.scope, context.key())?;
            transaction
                .execute(
                    "INSERT INTO snapshots (namespace_id, reset_epoch, snapshot_id, context_key, \
                 state, builder_token, build_revision, member_count) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0)",
                    params![
                        self.scope.namespace.as_bytes().as_slice(),
                        epoch(self.scope),
                        id.0.as_slice(),
                        context.digest().as_bytes().as_slice(),
                        BUILDING,
                        token.as_slice()
                    ],
                )
                .map_err(fail)?;
            super::leases::ensure_row(
                &transaction,
                self.scope,
                &self.lease,
                self.clock.unix_seconds(),
            )?;
            transaction
                .execute(
                    "INSERT INTO lease_holds (namespace_id, reset_epoch, lease_id, snapshot_id) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        self.scope.namespace.as_bytes().as_slice(),
                        epoch(self.scope),
                        self.lease.id.as_slice(),
                        id.0.as_slice()
                    ],
                )
                .map_err(fail)?;
            fault_point!(self, BeginBeforeCommit);
            self.before_commit();
            transaction.commit().map_err(fail)?;
            Ok(SnapshotBuilder {
                scope: self.scope,
                id,
                token,
                revision: 0,
                context: context.digest(),
            })
        })
    }

    /// Append `records` (at most 256) in one short transaction whose first
    /// write compares and increments the build revision under the token and
    /// `building` state; losing it writes nothing (spec §6.3 amendment,
    /// item 3). Each member copies its record's source key, so a missing or
    /// retired record, or a source identity already in the snapshot, fails
    /// the whole batch.
    pub fn add_members(
        &self,
        builder: &mut SnapshotBuilder,
        records: &[Digest],
    ) -> Result<(), Bypass> {
        self.guarded(Access::Write, || {
            if builder.scope != self.scope {
                return Err(Bypass::Fenced);
            }
            if records.len() > MAX_ROW_BATCH_ROWS {
                return Err(Bypass::TooLarge(TooLarge::Rows {
                    count: records.len(),
                }));
            }
            let fail = |error: rusqlite::Error| classify(&error);
            let namespace = self.scope.namespace.as_bytes().as_slice();
            let reset_epoch = epoch(self.scope);
            let id = builder.id.0.as_slice();
            let mut writer = self.writer()?;
            let transaction = begin(&mut writer, TransactionBehavior::Immediate)?;
            ensure_live(&transaction, self.scope)?;
            self.admit_write(
                &transaction,
                WriteEstimate {
                    bytes: (records.len() as u64 * (32 + MAX_INDEX_KEY_BYTES as u64))
                        .min(MAX_ROW_BATCH_BYTES as u64),
                    rows: records.len() as u64,
                },
            )?;
            let start: Option<i64> = transaction
                .query_row(
                    "UPDATE snapshots SET build_revision = build_revision + 1, \
                 member_count = member_count + ?7 \
                 WHERE namespace_id = ?1 AND reset_epoch = ?2 AND snapshot_id = ?3 \
                 AND builder_token = ?4 AND build_revision = ?5 AND state = ?6 \
                 RETURNING member_count - ?7",
                    params![
                        namespace,
                        reset_epoch,
                        id,
                        builder.token.as_slice(),
                        builder.revision as i64,
                        BUILDING,
                        records.len() as i64
                    ],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| {
                    if has_extended_code(&error, rusqlite::ffi::SQLITE_CONSTRAINT_CHECK) {
                        Bypass::TooLarge(TooLarge::Members {
                            added: records.len(),
                        })
                    } else {
                        classify(&error)
                    }
                })?;
            let Some(start) = start else {
                return Err(Bypass::BuilderFenced);
            };
            let mut insert = transaction
            .prepare_cached(
                "INSERT INTO snapshot_members (namespace_id, reset_epoch, snapshot_id, ordinal, \
                 record_id, source_key) \
                 SELECT ?1, ?2, ?3, ?4, record_id, source_key FROM member_records \
                 WHERE namespace_id = ?1 AND reset_epoch = ?2 AND record_id = ?5 \
                 AND lifecycle = ?6",
            )
            .map_err(fail)?;
            for (offset, record) in records.iter().enumerate() {
                let inserted = insert
                    .execute(params![
                        namespace,
                        reset_epoch,
                        id,
                        start + offset as i64,
                        record.as_bytes().as_slice(),
                        LIVE
                    ])
                    .map_err(|error| {
                        if has_extended_code(&error, rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE) {
                            Bypass::DuplicateMember
                        } else {
                            classify(&error)
                        }
                    })?;
                if inserted == 0 {
                    return Err(
                        match record_lifecycle(&transaction, self.scope, *record).map_err(fail)? {
                            None => Bypass::MissingReference,
                            Some(_) => Bypass::Retiring,
                        },
                    );
                }
            }
            drop(insert);
            let copied: i64 = transaction
                .query_row(
                    "SELECT coalesce(sum(length(source_key)), 0) FROM snapshot_members \
                 WHERE namespace_id = ?1 AND reset_epoch = ?2 AND snapshot_id = ?3 \
                 AND ordinal >= ?4",
                    params![namespace, reset_epoch, id, start],
                    |row| row.get(0),
                )
                .map_err(fail)?;
            let bytes = copied as usize + 32 * records.len();
            if bytes > MAX_ROW_BATCH_BYTES {
                return Err(Bypass::TooLarge(TooLarge::Batch { bytes }));
            }
            fault_point!(self, MembersBeforeCommit);
            self.before_commit();
            transaction.commit().map_err(fail)?;
            builder.revision += 1;
            Ok(())
        })
    }

    /// Seal `builder`'s snapshot (spec §6.3 amendment, item 4). The digest
    /// streams in bounded read transactions outside any write; then one
    /// short transaction compares token, revision and state, checks the
    /// count and that no member's record is retired, and stores the digest
    /// and the next publication sequence. Any change in between is a
    /// refusal, never a partial seal.
    ///
    /// The seal checks the members' records, not what those records
    /// reference. That relies on the reachability invariant (Decision 11):
    /// a live member record's artifact and evidence stay live, because
    /// publication refuses records over retired references and
    /// `l4d-zf1b.10` retires top-down (snapshots and builds, then records
    /// no live snapshot or build lists, then artifacts and evidence no live
    /// record references). Retiring a referenced artifact or evidence set
    /// while its records are live breaks that contract. `l4d-zf1b.10`
    /// enforces that order: `Store::retire_step` (`gc.rs`) re-checks every
    /// reference inside its write transaction.
    ///
    /// Discovery evidence must be discovery evidence of the snapshot's own
    /// context, or the seal is refused as `DiscoveryEvidence`.
    ///
    /// The builder is consumed: clone it before sealing to retry after a
    /// transient bypass, or to `retire_build` it after a refusal.
    pub fn seal_snapshot(
        &self,
        builder: SnapshotBuilder,
        seal: &SnapshotSeal,
    ) -> Result<SnapshotHeader, Bypass> {
        self.guarded(Access::Write, || {
            if builder.scope != self.scope {
                return Err(Bypass::Fenced);
            }
            let fail = |error: rusqlite::Error| classify(&error);
            let blob_fail =
                |error: rusqlite::types::FromSqlError| classify(&rusqlite::Error::from(error));
            let namespace = self.scope.namespace.as_bytes().as_slice();
            let reset_epoch = epoch(self.scope);
            let id = builder.id.0.as_slice();
            let mut hasher: Option<SnapshotHasher> = None;
            let mut stored_count = 0u64;
            let mut count = 0u64;
            // Each batch is bounded by rows, not bytes: its rows are hashed as
            // they are read and never retained.
            loop {
                let mut reader = self.reader()?;
                let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
                ensure_live(&transaction, self.scope)?;
                let Some(member_count) = held_count(&transaction, &builder).map_err(fail)? else {
                    return Err(Bypass::BuilderFenced);
                };
                if hasher.is_none() {
                    stored_count = member_count;
                    hasher = Some(SnapshotHasher::new(
                        builder.context,
                        seal.discovery,
                        &seal.coverage,
                        member_count,
                    ));
                }
                let witness = hasher.as_mut().expect("set by the first batch");
                let mut statement = transaction
                    .prepare_cached(
                        "SELECT ordinal, record_id, source_key FROM snapshot_members \
                     WHERE namespace_id = ?1 AND reset_epoch = ?2 AND snapshot_id = ?3 \
                     AND ordinal >= ?4 ORDER BY ordinal LIMIT ?5",
                    )
                    .map_err(fail)?;
                let mut rows = statement
                    .query(params![
                        namespace,
                        reset_epoch,
                        id,
                        count as i64,
                        MAX_ROW_BATCH_ROWS as i64
                    ])
                    .map_err(fail)?;
                let mut read = 0usize;
                while let Some(row) = rows.next().map_err(fail)? {
                    let ordinal: i64 = row.get(0).map_err(fail)?;
                    let record = row.get_ref(1).map_err(fail)?.as_blob().map_err(blob_fail)?;
                    let source_key = row.get_ref(2).map_err(fail)?.as_blob().map_err(blob_fail)?;
                    if ordinal != count as i64 {
                        return Err(Bypass::SealRefused(SealRefusal::CountMismatch));
                    }
                    if record.len() != 32 {
                        return Err(Bypass::Corrupt);
                    }
                    witness.update(record, source_key);
                    count += 1;
                    read += 1;
                }
                if read < MAX_ROW_BATCH_ROWS {
                    break;
                }
            }
            if count != stored_count {
                return Err(Bypass::SealRefused(SealRefusal::CountMismatch));
            }
            let digest = hasher.expect("set by the first batch").finish();
            fault_point!(self, SealDigestStreamed);
            let mut writer = self.writer()?;
            let transaction = begin(&mut writer, TransactionBehavior::Immediate)?;
            ensure_live(&transaction, self.scope)?;
            self.admit_write(
                &transaction,
                WriteEstimate {
                    bytes: 128,
                    rows: 1,
                },
            )?;
            let Some(member_count) = held_count(&transaction, &builder).map_err(fail)? else {
                return Err(Bypass::BuilderFenced);
            };
            if member_count != count {
                return Err(Bypass::SealRefused(SealRefusal::CountMismatch));
            }
            let retired: Option<i64> = transaction
                .query_row(
                    "SELECT 1 FROM snapshot_members m LEFT JOIN member_records r \
                 ON r.namespace_id = m.namespace_id AND r.reset_epoch = m.reset_epoch \
                 AND r.record_id = m.record_id \
                 WHERE m.namespace_id = ?1 AND m.reset_epoch = ?2 AND m.snapshot_id = ?3 \
                 AND (r.lifecycle IS NULL OR r.lifecycle <> ?4) LIMIT 1",
                    params![namespace, reset_epoch, id, LIVE],
                    |row| row.get(0),
                )
                .optional()
                .map_err(fail)?;
            if retired.is_some() {
                return Err(Bypass::SealRefused(SealRefusal::RetiredRecord));
            }
            if let Some(discovery) = seal.discovery {
                require_evidence(
                    &transaction,
                    self.scope,
                    discovery,
                    EvidencePurpose::Discovery,
                    None,
                )
                .map_err(|bypass| match bypass {
                    Bypass::InvalidRecord(MemberError::EvidencePurpose) => {
                        Bypass::SealRefused(SealRefusal::DiscoveryEvidence)
                    }
                    bypass => bypass,
                })?;
                let context: Vec<u8> = transaction
                    .query_row(
                        "SELECT context_key FROM evidence_sets \
                     WHERE namespace_id = ?1 AND reset_epoch = ?2 AND evidence_id = ?3",
                        params![namespace, reset_epoch, discovery.as_bytes().as_slice()],
                        |row| row.get(0),
                    )
                    .map_err(fail)?;
                if context != builder.context.as_bytes() {
                    return Err(Bypass::SealRefused(SealRefusal::DiscoveryEvidence));
                }
            }
            let sequence = next_publication_seq(&transaction)?;
            let changed = transaction
                .execute(
                    "UPDATE snapshots SET state = ?7, sealed_digest = ?8, coverage = ?9, \
                 discovery_evidence = ?10, publication_seq = ?11 \
                 WHERE namespace_id = ?1 AND reset_epoch = ?2 AND snapshot_id = ?3 \
                 AND builder_token = ?4 AND build_revision = ?5 AND state = ?6",
                    params![
                        namespace,
                        reset_epoch,
                        id,
                        builder.token.as_slice(),
                        builder.revision as i64,
                        BUILDING,
                        SEALED,
                        digest.as_bytes().as_slice(),
                        seal.coverage.encoded(),
                        seal.discovery
                            .map(|discovery| discovery.as_bytes().to_vec()),
                        sequence
                    ],
                )
                .map_err(fail)?;
            if changed != 1 {
                return Err(Bypass::BuilderFenced);
            }
            transaction
                .execute(
                    "DELETE FROM lease_holds WHERE namespace_id = ?1 AND reset_epoch = ?2 \
                     AND snapshot_id = ?3",
                    params![namespace, reset_epoch, id],
                )
                .map_err(fail)?;
            fault_point!(self, SealBeforeCommit);
            self.before_commit();
            transaction.commit().map_err(fail)?;
            drop(writer);
            fault_point!(self, SealCommitted);
            Ok(SnapshotHeader {
                scope: self.scope,
                id: builder.id,
                context: builder.context,
                member_count: count,
                discovery: seal.discovery,
                coverage: seal.coverage,
                sealed_digest: digest,
                publication_seq: sequence as u64,
            })
        })
    }

    /// The builder retires its own unsealed build (token-checked). `false`
    /// if the build was not building under this token any more.
    pub fn retire_build(&self, builder: &SnapshotBuilder) -> Result<bool, Bypass> {
        self.guarded(Access::Maintenance, || {
            if builder.scope != self.scope {
                return Err(Bypass::Fenced);
            }
            self.retire_where(builder.id, Some(builder.token))
        })
    }

    /// Maintenance retires a building or sealed snapshot (no token). Read
    /// eligibility ends at once; rows are deleted later (`l4d-zf1b.10`).
    pub fn retire_snapshot(&self, id: SnapshotId) -> Result<bool, Bypass> {
        self.guarded(Access::Maintenance, || self.retire_where(id, None))
    }

    fn retire_where(&self, id: SnapshotId, token: Option<[u8; 16]>) -> Result<bool, Bypass> {
        let fail = |error: rusqlite::Error| classify(&error);
        let namespace = self.scope.namespace.as_bytes().as_slice();
        let reset_epoch = epoch(self.scope);
        let mut writer = self.writer()?;
        let transaction = begin(&mut writer, TransactionBehavior::Immediate)?;
        ensure_live(&transaction, self.scope)?;
        let changed = match token {
            Some(token) => transaction.execute(
                "UPDATE snapshots SET state = ?4 WHERE namespace_id = ?1 AND reset_epoch = ?2 \
                 AND snapshot_id = ?3 AND builder_token = ?5 AND state = ?6",
                params![
                    namespace,
                    reset_epoch,
                    id.0.as_slice(),
                    RETIRED,
                    token.as_slice(),
                    BUILDING
                ],
            ),
            None => transaction.execute(
                "UPDATE snapshots SET state = ?4 WHERE namespace_id = ?1 AND reset_epoch = ?2 \
                 AND snapshot_id = ?3 AND state IN (?5, ?6)",
                params![
                    namespace,
                    reset_epoch,
                    id.0.as_slice(),
                    RETIRED,
                    BUILDING,
                    SEALED
                ],
            ),
        }
        .map_err(fail)?;
        if changed == 1 {
            transaction
                .execute(
                    "DELETE FROM lease_holds WHERE namespace_id = ?1 AND reset_epoch = ?2 \
                     AND snapshot_id = ?3",
                    params![namespace, reset_epoch, id.0.as_slice()],
                )
                .map_err(fail)?;
        }
        fault_point!(self, RetireBeforeCommit);
        self.before_commit();
        transaction.commit().map_err(fail)?;
        Ok(changed == 1)
    }

    /// Maintenance retires a member record (no token): no build can add it
    /// and no seal of a build listing it succeeds. `l4d-zf1b.10` calls this
    /// only for records no live snapshot or build lists (Decision 11).
    pub fn retire_member_record(&self, id: Digest) -> Result<bool, Bypass> {
        self.guarded(Access::Maintenance, || {
            let fail = |error: rusqlite::Error| classify(&error);
            let mut writer = self.writer()?;
            let transaction = begin(&mut writer, TransactionBehavior::Immediate)?;
            ensure_live(&transaction, self.scope)?;
            let changed = transaction
                .execute(
                    "UPDATE member_records SET lifecycle = ?5 WHERE namespace_id = ?1 \
                 AND reset_epoch = ?2 AND record_id = ?3 AND lifecycle = ?4",
                    params![
                        self.scope.namespace.as_bytes().as_slice(),
                        epoch(self.scope),
                        id.as_bytes().as_slice(),
                        LIVE,
                        RECORD_RETIRED
                    ],
                )
                .map_err(fail)?;
            self.before_commit();
            transaction.commit().map_err(fail)?;
            Ok(changed == 1)
        })
    }

    /// At most `MAX_SNAPSHOT_CANDIDATES` sealed snapshots of `context`,
    /// newest publication first. Hints only: a session validates each
    /// candidate's members and evidence before using any (spec §9). The
    /// candidates are charged to the scratch while they are built.
    pub fn find_snapshots(
        &self,
        context: &ContextKey,
    ) -> Result<Lookup<Vec<SnapshotHeader>>, Bypass> {
        self.guarded(Access::Read, || {
            // The candidates, charged before they are allocated (spec §10).
            let Some(_scratch) = self
                .scratch
                .reserve((MAX_SNAPSHOT_CANDIDATES * std::mem::size_of::<SnapshotHeader>()) as u64)
            else {
                return Err(Bypass::ScratchExhausted);
            };
            let mut reader = self.reader()?;
            let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
            ensure_live(&transaction, self.scope)?;
            if let Lookup::Miss(miss) =
                admit_manifest(&transaction, &self.namespace, self.scope, context.key())?
            {
                return Ok(Lookup::Miss(miss));
            }
            newest_first(
                &transaction,
                Candidates {
                    select: "SELECT snapshot_id, member_count, discovery_evidence, coverage, \
                         sealed_digest, publication_seq FROM snapshots WHERE namespace_id = ?1 \
                         AND reset_epoch = ?2 AND context_key = ?3 AND state = ?4",
                    params: params![
                        self.scope.namespace.as_bytes().as_slice(),
                        epoch(self.scope),
                        context.digest().as_bytes().as_slice(),
                        SEALED
                    ],
                    limit: MAX_SNAPSHOT_CANDIDATES,
                    malformed: Malformed::Snapshot,
                },
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
                |row: SnapshotRow| header_from(self.scope, context.digest(), row),
            )
        })
    }

    pub fn read_snapshot(&self, header: &SnapshotHeader) -> SnapshotReader<'_> {
        SnapshotReader(ManifestReader::new(
            self,
            Expected {
                scope: header.scope,
                parent: header.id.0.to_vec(),
                count: header.member_count,
                content_bytes: None,
                witness: header.sealed_digest,
            },
            MemberSource(SnapshotHasher::new(
                header.context,
                header.discovery,
                &header.coverage,
                header.member_count,
            )),
        ))
    }
}

/// Why `snapshot` can no longer be read in `transaction` (no longer sealed,
/// or gone); `None` while it is sealed. The queries run the reader's check.
pub(crate) fn snapshot_ineligible(
    transaction: &Connection,
    scope: Scope,
    snapshot: SnapshotId,
) -> Result<Option<Miss>, Bypass> {
    ineligible::<MemberSource>(
        transaction,
        scope.namespace.as_bytes(),
        scope,
        snapshot.as_bytes(),
    )
}

/// The rows of one snapshot: members whose record is still live, folded
/// into the sealed digest.
#[derive(Clone)]
struct MemberSource(SnapshotHasher);

impl RowSource for MemberSource {
    type Item = SnapshotMember;
    const PARENT_SQL: &'static str = "SELECT CASE state WHEN 2 THEN 1 ELSE 2 END FROM snapshots \
         WHERE namespace_id = ?1 AND reset_epoch = ?2 AND snapshot_id = ?3";
    const ROWS_SQL: &'static str = "SELECT m.ordinal, m.record_id, m.source_key, r.lifecycle \
         FROM snapshot_members m LEFT JOIN member_records r \
         ON r.namespace_id = m.namespace_id AND r.reset_epoch = m.reset_epoch \
         AND r.record_id = m.record_id \
         WHERE m.namespace_id = ?1 AND m.reset_epoch = ?2 AND m.snapshot_id = ?3 \
         AND m.ordinal >= ?4 ORDER BY m.ordinal LIMIT ?5";
    const TRAILING_SQL: &'static str = "SELECT 1 FROM snapshot_members \
         WHERE namespace_id = ?1 AND reset_epoch = ?2 AND snapshot_id = ?3 \
         AND ordinal >= ?4 LIMIT 1";

    fn size(row: &rusqlite::Row<'_>) -> rusqlite::Result<usize> {
        Ok(row.get_ref(1)?.as_blob()?.len() + row.get_ref(2)?.as_blob()?.len())
    }

    fn take(&mut self, row: &rusqlite::Row<'_>) -> Result<SnapshotMember, RowError> {
        let ordinal: i64 = row.get(0)?;
        let record = row.get_ref(1)?.as_blob()?;
        let source_key = row.get_ref(2)?.as_blob()?;
        let lifecycle: Option<i64> = row.get(3)?;
        match lifecycle {
            None => return Err(RowError::Miss(Miss::Vanished)),
            Some(LIVE) => {}
            Some(_) => return Err(RowError::Miss(Miss::Retired)),
        }
        let Ok(record_id) = <[u8; 32]>::try_from(record) else {
            return Err(RowError::Miss(Miss::Malformed(Malformed::Snapshot)));
        };
        self.0.update(record, source_key);
        Ok(SnapshotMember {
            ordinal: ordinal as u64,
            record_id: Digest::from_bytes(record_id),
            source_key: source_key.to_vec(),
        })
    }

    fn finish(&self) -> Digest {
        self.0.finish()
    }
}

/// At most 256 members from at most 1 MiB of rows, charged to the store's
/// scratch budget until dropped.
#[derive(Debug)]
pub struct MemberBatch {
    members: Vec<SnapshotMember>,
    _scratch: ScratchLease,
}

impl MemberBatch {
    pub fn members(&self) -> &[SnapshotMember] {
        &self.members
    }
}

/// Streams a sealed snapshot's members in bounded batches while recomputing
/// its sealed digest. A record retired or removed between batches is a miss.
pub struct SnapshotReader<'a>(ManifestReader<'a, MemberSource>);

impl SnapshotReader<'_> {
    pub fn is_verified(&self) -> bool {
        self.0.is_verified()
    }

    pub fn next_batch(&mut self) -> Result<Lookup<Option<MemberBatch>>, Bypass> {
        self.0.next_batch_as(|members, scratch| MemberBatch {
            members,
            _scratch: scratch,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CoverageStatus;

    #[test]
    fn the_sealed_digest_is_the_digest_of_the_snapshot_manifest() {
        let coverage = SnapshotCoverage {
            discovery: CoverageStatus::Complete,
            membership: CoverageStatus::Complete,
        };
        let context = Digest::from_bytes([1; 32]);
        let mut hasher = SnapshotHasher::new(context, None, &coverage, 1);
        hasher.update(&[2; 32], b"key");
        let mut manifest = manifest_encoder(KeyKind::Snapshot);
        manifest.field(1);
        manifest.digest(&context);
        manifest.field(2);
        manifest.u8(0);
        manifest.field(3);
        manifest.encode(&coverage);
        manifest.field(4);
        manifest.u64(1);
        manifest.digest(&Digest::from_bytes([2; 32]));
        manifest.bytes(b"key");
        assert_eq!(
            hasher.finish(),
            Digest::derive(MANIFEST_DIGEST_CONTEXT, manifest.as_bytes())
        );
    }
}
