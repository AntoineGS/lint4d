//! Member records (spec §6.1 and §6.3, amendments 2026-10-27): encoded and
//! hashed outside any transaction, published in one bounded transaction
//! only over live references, and read back under a witness that recomputes
//! the record ID.

use super::Store;
use super::limits::{
    MAX_ARTIFACT_CONTENT_BYTES, MAX_INDEX_KEY_BYTES, MAX_MANIFEST_BYTES, MAX_MEMBER_BINDINGS,
    MAX_ROW_BATCH_BYTES,
};
use super::namespaces::{LIVE, Scope, ensure_live, epoch};
use super::outcome::{Bypass, Integrity, Lookup, Malformed, Miss, Published, TooLarge};
use super::reader::ScratchLease;
use super::sqlite::{begin, classify, classify_read};
use super::stream::{Expected, ManifestReader, RowError, RowSource};
use crate::evidence::path_key;
use crate::record::RecordHasher;
use crate::{
    Binding, Digest, EvidencePurpose, MemberError, MemberHeader, MemberRecord, decode_binding,
    decode_member_header,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedRecord {
    pub record_id: Digest,
    pub published: Published,
}

struct EncodedBinding {
    kind: u8,
    outcome: u8,
    target_key: Option<Vec<u8>>,
    evidence: Option<Digest>,
    payload: Vec<u8>,
}

struct EncodedRecord {
    id: Digest,
    header: Vec<u8>,
    source_key: Vec<u8>,
    bindings: Vec<EncodedBinding>,
    /// Bytes of the binding rows (payloads and target keys).
    content_bytes: u64,
}

/// Encode, admit and hash a validated record before any transaction
/// (spec §9, §10).
fn encode_record(record: &MemberRecord) -> Result<EncodedRecord, Bypass> {
    let too_large = |limit| Err(Bypass::TooLarge(limit));
    if record.bindings.len() > MAX_MEMBER_BINDINGS {
        return too_large(TooLarge::Bindings {
            count: record.bindings.len(),
        });
    }
    let header = record.header.encoded();
    if header.len() > MAX_MANIFEST_BYTES {
        return too_large(TooLarge::Manifest {
            bytes: header.len(),
        });
    }
    let source_key = path_key(&record.header.source.canonical);
    if source_key.len() > MAX_INDEX_KEY_BYTES {
        return too_large(TooLarge::SourceKey {
            bytes: source_key.len(),
        });
    }
    let mut hasher = RecordHasher::new(&header, record.bindings.len());
    let mut bindings = Vec::with_capacity(record.bindings.len());
    let mut content = 0usize;
    for (ordinal, binding) in record.bindings.iter().enumerate() {
        let target_key = binding
            .outcome
            .target()
            .map(|target| path_key(&target.canonical));
        let key_bytes = target_key.as_ref().map_or(0, Vec::len);
        if key_bytes > MAX_INDEX_KEY_BYTES {
            return too_large(TooLarge::IndexKey {
                ordinal,
                bytes: key_bytes,
            });
        }
        let payload = binding.encoded();
        let bytes = payload.len() + key_bytes;
        if bytes > MAX_ROW_BATCH_BYTES {
            return too_large(TooLarge::Row { ordinal, bytes });
        }
        content += bytes;
        if content > MAX_ARTIFACT_CONTENT_BYTES {
            return too_large(TooLarge::Content { bytes: content });
        }
        hasher.update(&payload);
        bindings.push(EncodedBinding {
            kind: binding.kind.tag(),
            outcome: binding.outcome.tag(),
            target_key,
            evidence: binding.evidence,
            payload,
        });
    }
    Ok(EncodedRecord {
        id: hasher.finish(),
        header,
        source_key,
        bindings,
        content_bytes: content as u64,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct StoredRecord {
    lifecycle: i64,
    header: Vec<u8>,
    source_key: Vec<u8>,
    artifact_id: Option<Vec<u8>>,
    member_evidence: Option<Vec<u8>>,
    binding_count: i64,
    content_bytes: i64,
}

fn digest_column(digest: Option<Digest>) -> Option<Vec<u8>> {
    digest.map(|digest| digest.as_bytes().to_vec())
}

fn expected_row(record: &MemberRecord, encoded: &EncodedRecord) -> StoredRecord {
    StoredRecord {
        lifecycle: LIVE,
        header: encoded.header.clone(),
        source_key: encoded.source_key.clone(),
        artifact_id: digest_column(record.header.artifact),
        member_evidence: digest_column(record.header.member_evidence),
        binding_count: encoded.bindings.len() as i64,
        content_bytes: encoded.content_bytes as i64,
    }
}

fn read_record_row(
    connection: &Connection,
    scope: Scope,
    id: Digest,
) -> rusqlite::Result<Option<StoredRecord>> {
    connection
        .query_row(
            "SELECT lifecycle, header, source_key, artifact_id, member_evidence, binding_count, \
             content_bytes FROM member_records \
             WHERE namespace_id = ?1 AND reset_epoch = ?2 AND record_id = ?3",
            params![
                scope.namespace.as_bytes().as_slice(),
                epoch(scope),
                id.as_bytes().as_slice()
            ],
            |row| {
                Ok(StoredRecord {
                    lifecycle: row.get(0)?,
                    header: row.get(1)?,
                    source_key: row.get(2)?,
                    artifact_id: row.get(3)?,
                    member_evidence: row.get(4)?,
                    binding_count: row.get(5)?,
                    content_bytes: row.get(6)?,
                })
            },
        )
        .optional()
}

/// A live artifact in the caller's transaction (spec §6.1: new references
/// require live records).
pub(crate) fn require_artifact(
    connection: &Connection,
    scope: Scope,
    id: Digest,
) -> Result<(), Bypass> {
    let lifecycle: Option<i64> = connection
        .query_row(
            "SELECT lifecycle FROM artifacts \
             WHERE namespace_id = ?1 AND reset_epoch = ?2 AND artifact_id = ?3",
            params![
                scope.namespace.as_bytes().as_slice(),
                epoch(scope),
                id.as_bytes().as_slice()
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| classify(&error))?;
    match lifecycle {
        None => Err(Bypass::MissingReference),
        Some(LIVE) => Ok(()),
        Some(_) => Err(Bypass::Retiring),
    }
}

/// A live evidence set of `purpose` in the caller's transaction; artifact
/// evidence must conclude `artifact`.
pub(crate) fn require_evidence(
    connection: &Connection,
    scope: Scope,
    id: Digest,
    purpose: EvidencePurpose,
    artifact: Option<Digest>,
) -> Result<(), Bypass> {
    let stored: Option<(i64, i64, Option<Vec<u8>>)> = connection
        .query_row(
            "SELECT lifecycle, purpose, artifact_id FROM evidence_sets \
             WHERE namespace_id = ?1 AND reset_epoch = ?2 AND evidence_id = ?3",
            params![
                scope.namespace.as_bytes().as_slice(),
                epoch(scope),
                id.as_bytes().as_slice()
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| classify(&error))?;
    match stored {
        None => Err(Bypass::MissingReference),
        Some((lifecycle, _, _)) if lifecycle != LIVE => Err(Bypass::Retiring),
        Some((_, stored_purpose, stored_artifact))
            if stored_purpose != i64::from(purpose.tag())
                || (purpose == EvidencePurpose::Artifact
                    && stored_artifact != digest_column(artifact)) =>
        {
            Err(Bypass::InvalidRecord(MemberError::EvidencePurpose))
        }
        Some(_) => Ok(()),
    }
}

fn insert_record(
    transaction: &Connection,
    scope: Scope,
    encoded: &EncodedRecord,
    expected: &StoredRecord,
) -> Result<(), Bypass> {
    let fail = |error: rusqlite::Error| classify(&error);
    let namespace = scope.namespace.as_bytes().as_slice();
    let reset_epoch = epoch(scope);
    let id = encoded.id.as_bytes().as_slice();
    transaction
        .execute(
            "INSERT INTO member_records (namespace_id, reset_epoch, record_id, lifecycle, header, \
             source_key, artifact_id, member_evidence, binding_count, content_bytes) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                namespace,
                reset_epoch,
                id,
                expected.lifecycle,
                expected.header,
                expected.source_key,
                expected.artifact_id,
                expected.member_evidence,
                expected.binding_count,
                expected.content_bytes
            ],
        )
        .map_err(fail)?;
    let mut insert = transaction
        .prepare_cached(
            "INSERT INTO member_bindings (namespace_id, reset_epoch, record_id, ordinal, kind, \
             outcome, target_key, evidence_id, payload) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )
        .map_err(fail)?;
    for (ordinal, binding) in encoded.bindings.iter().enumerate() {
        insert
            .execute(params![
                namespace,
                reset_epoch,
                id,
                ordinal as i64,
                i64::from(binding.kind),
                i64::from(binding.outcome),
                binding.target_key,
                digest_column(binding.evidence),
                binding.payload
            ])
            .map_err(fail)?;
    }
    Ok(())
}

/// An admitted member record. Its member header and binding rows are
/// unverified until [`Store::read_member_record`] has streamed every row
/// and the reader reports `is_verified()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRecordHeader {
    scope: Scope,
    record_id: Digest,
    member: MemberHeader,
    encoded_header: Vec<u8>,
    binding_count: u64,
    content_bytes: u64,
}

impl MemberRecordHeader {
    pub fn record_id(&self) -> Digest {
        self.record_id
    }

    pub fn member(&self) -> &MemberHeader {
        &self.member
    }

    pub fn binding_count(&self) -> u64 {
        self.binding_count
    }
}

impl Store {
    /// Publish `record` and its bindings in one transaction, only if every
    /// artifact and evidence set it references is live in this scope (spec
    /// §6.3 amendment, item 1). An identical record converges; a different
    /// record under the same ID is an integrity failure.
    pub fn publish_member_record(&self, record: &MemberRecord) -> Result<PublishedRecord, Bypass> {
        record.validate().map_err(Bypass::InvalidRecord)?;
        let encoded = encode_record(record)?;
        let Some(_scratch) = self
            .scratch
            .reserve(encoded.header.len() as u64 + encoded.content_bytes)
        else {
            return Err(Bypass::ScratchExhausted);
        };
        let expected = expected_row(record, &encoded);
        let fail = |error: rusqlite::Error| classify(&error);
        let mut writer = self.writer()?;
        let transaction = begin(&mut writer, TransactionBehavior::Immediate)?;
        ensure_live(&transaction, self.scope)?;
        if let Some(artifact) = record.header.artifact {
            require_artifact(&transaction, self.scope, artifact)?;
        }
        if let Some(evidence) = record.header.member_evidence {
            require_evidence(
                &transaction,
                self.scope,
                evidence,
                EvidencePurpose::Artifact,
                record.header.artifact,
            )?;
        }
        let mut checked = BTreeSet::new();
        for evidence in record
            .bindings
            .iter()
            .filter_map(|binding| binding.evidence)
        {
            if checked.insert(evidence) {
                require_evidence(
                    &transaction,
                    self.scope,
                    evidence,
                    EvidencePurpose::Binding,
                    None,
                )?;
            }
        }
        let published = match read_record_row(&transaction, self.scope, encoded.id).map_err(fail)? {
            Some(stored) if stored.lifecycle != LIVE => return Err(Bypass::Retiring),
            // Only the record row is compared: the ID is the digest of the
            // header and every binding, so the same ID means the same
            // bindings, and stored rows that disagree fail every read.
            Some(stored) if stored == expected => Published::AlreadyPresent,
            Some(_) => return Err(Bypass::Integrity(Integrity::ConflictingRecord)),
            None => {
                insert_record(&transaction, self.scope, &encoded, &expected)?;
                Published::Inserted
            }
        };
        fault_point!(self, RecordBeforeCommit);
        self.before_commit();
        transaction.commit().map_err(fail)?;
        Ok(PublishedRecord {
            record_id: encoded.id,
            published,
        })
    }

    /// Admit the live record `id`: its stored header decodes and agrees with
    /// its index columns. Its content is unverified until read.
    pub fn open_member_record(&self, id: Digest) -> Result<Lookup<MemberRecordHeader>, Bypass> {
        let mut reader = self.reader()?;
        let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
        ensure_live(&transaction, self.scope)?;
        let stored = match read_record_row(&transaction, self.scope, id) {
            Ok(Some(stored)) => stored,
            Ok(None) => return Ok(Lookup::Miss(Miss::Absent)),
            Err(error) => return classify_read(&error).map(Lookup::Miss),
        };
        if stored.lifecycle != LIVE {
            return Ok(Lookup::Miss(Miss::Retired));
        }
        let malformed = Ok(Lookup::Miss(Miss::Malformed(Malformed::Member)));
        let (Ok(binding_count), Ok(content_bytes)) = (
            u64::try_from(stored.binding_count),
            u64::try_from(stored.content_bytes),
        ) else {
            return malformed;
        };
        if binding_count > MAX_MEMBER_BINDINGS as u64
            || content_bytes > MAX_ARTIFACT_CONTENT_BYTES as u64
            || stored.header.len() > MAX_MANIFEST_BYTES
        {
            return malformed;
        }
        let Ok(member) = decode_member_header(&stored.header) else {
            return malformed;
        };
        if path_key(&member.source.canonical) != stored.source_key
            || digest_column(member.artifact) != stored.artifact_id
            || digest_column(member.member_evidence) != stored.member_evidence
        {
            return malformed;
        }
        Ok(Lookup::Hit(MemberRecordHeader {
            scope: self.scope,
            record_id: id,
            member,
            encoded_header: stored.header,
            binding_count,
            content_bytes,
        }))
    }

    pub fn read_member_record(&self, header: &MemberRecordHeader) -> MemberRecordReader<'_> {
        MemberRecordReader(ManifestReader::new(
            self,
            Expected {
                scope: header.scope,
                parent: header.record_id.as_bytes().to_vec(),
                count: header.binding_count,
                content_bytes: Some(header.content_bytes),
                witness: header.record_id,
            },
            BindingSource {
                hasher: RecordHasher::new(&header.encoded_header, header.binding_count as usize),
                previous: None,
            },
        ))
    }
}

/// The rows of one record: bindings whose `kind`, `outcome`, `target_key`
/// and `evidence_id` columns agree with their payload, in strict
/// `(kind, occurrence)` order, folded into the record ID.
#[derive(Clone)]
struct BindingSource {
    hasher: RecordHasher,
    previous: Option<(u8, u64)>,
}

impl RowSource for BindingSource {
    type Item = Binding;
    const PARENT_SQL: &'static str = "SELECT lifecycle FROM member_records \
         WHERE namespace_id = ?1 AND reset_epoch = ?2 AND record_id = ?3";
    const ROWS_SQL: &'static str = "SELECT ordinal, kind, outcome, target_key, evidence_id, \
         payload FROM member_bindings \
         WHERE namespace_id = ?1 AND reset_epoch = ?2 AND record_id = ?3 \
         AND ordinal >= ?4 ORDER BY ordinal LIMIT ?5";
    const TRAILING_SQL: &'static str = "SELECT 1 FROM member_bindings \
         WHERE namespace_id = ?1 AND reset_epoch = ?2 AND record_id = ?3 \
         AND ordinal >= ?4 LIMIT 1";

    fn size(row: &rusqlite::Row<'_>) -> rusqlite::Result<usize> {
        let target = row.get_ref(3)?.as_blob_or_null()?.map_or(0, <[u8]>::len);
        Ok(row.get_ref(5)?.as_blob()?.len() + target)
    }

    fn take(&mut self, row: &rusqlite::Row<'_>) -> Result<Binding, RowError> {
        let kind: i64 = row.get(1)?;
        let outcome: i64 = row.get(2)?;
        let target_key = row.get_ref(3)?.as_blob_or_null()?;
        let evidence = row.get_ref(4)?.as_blob_or_null()?;
        let payload = row.get_ref(5)?.as_blob()?;
        let malformed = RowError::Miss(Miss::Malformed(Malformed::Member));
        let Ok(binding) = decode_binding(payload) else {
            return Err(malformed);
        };
        let key = binding.order_key();
        let stored_target = binding
            .outcome
            .target()
            .map(|target| path_key(&target.canonical));
        if i64::from(binding.kind.tag()) != kind
            || i64::from(binding.outcome.tag()) != outcome
            || stored_target.as_deref() != target_key
            || binding
                .evidence
                .as_ref()
                .map(|digest| digest.as_bytes().as_slice())
                != evidence
            || self.previous.is_some_and(|previous| previous >= key)
        {
            return Err(malformed);
        }
        self.previous = Some(key);
        self.hasher.update(payload);
        Ok(binding)
    }

    fn finish(&self) -> Digest {
        self.hasher.finish()
    }
}

/// At most 256 bindings decoded from at most 1 MiB of rows, charged to the
/// store's scratch budget until dropped.
#[derive(Debug)]
pub struct BindingBatch {
    bindings: Vec<Binding>,
    _scratch: ScratchLease,
}

impl BindingBatch {
    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }
}

/// Streams a record's bindings in bounded batches while recomputing its ID.
pub struct MemberRecordReader<'a>(ManifestReader<'a, BindingSource>);

impl MemberRecordReader<'_> {
    /// Whether every binding was read and the record ID recomputed. Only
    /// then may the record or its header be used.
    pub fn is_verified(&self) -> bool {
        self.0.is_verified()
    }

    pub fn next_batch(&mut self) -> Result<Lookup<Option<BindingBatch>>, Bypass> {
        self.0.next_batch_as(|bindings, scratch| BindingBatch {
            bindings,
            _scratch: scratch,
        })
    }
}
