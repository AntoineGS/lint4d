//! Evidence sets: the observations that prove an expanded artifact for one
//! root, one resolver lookup or one discovery scan under one context (spec
//! §6.1, §7), read back in bounded batches under a streamed integrity
//! witness. Subject lookups take the most recent publications first
//! (l4d-e8bl).

use super::Store;
use super::artifacts::{ArtifactWrite, insert_prepared, prepare_artifact};
use super::keys::{
    ArtifactKeyRef, admit_manifest, admit_manifest_by_digest, check_manifest_size, put_manifest,
};
use super::limits::{
    MAX_ARTIFACT_CONTENT_BYTES, MAX_EVIDENCE_CANDIDATES, MAX_EVIDENCE_OBSERVATIONS,
    MAX_INDEX_KEY_BYTES, MAX_ROW_BATCH_BYTES,
};
use super::namespaces::{Scope, ensure_live, epoch};
use super::outcome::{Bypass, Integrity, Lookup, Malformed, Miss, Published, TooLarge};
use super::reader::ScratchLease;
use super::sequence::{Candidates, newest_first, next_publication_seq};
use super::sqlite::{begin, classify, classify_read};
use super::stream::{Expected, ManifestReader, RowError, RowSource};
use crate::evidence::{EvidenceHasher, decode_observation, path_key};
use crate::facts::{encode_facts, encoded_content_bytes};
use crate::{
    CanonicalKey, ContextKey, Digest, DiscoveryScope, EvidencePurpose, EvidenceSubject, KeyKind,
    LocalArtifactKey, LookupKey, Observation,
};
use pascal_core::facts::ArtifactFacts;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

const LIVE: i64 = 1;

#[derive(Debug, Clone, Copy)]
pub struct EvidenceWrite<'a> {
    /// The complete context the observations were made under.
    pub context: &'a ContextKey,
    pub observations: &'a [Observation],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedEvidence {
    pub artifact: Published,
    pub evidence: Published,
}

/// The subject of evidence that concludes no artifact.
#[derive(Debug, Clone, Copy)]
pub enum EvidenceTarget<'a> {
    Binding(&'a LookupKey),
    Discovery(&'a DiscoveryScope),
}

impl<'a> EvidenceTarget<'a> {
    fn purpose(&self) -> EvidencePurpose {
        match self {
            Self::Binding(_) => EvidencePurpose::Binding,
            Self::Discovery(_) => EvidencePurpose::Discovery,
        }
    }

    fn key(&self) -> &'a CanonicalKey {
        match self {
            Self::Binding(key) => key.key(),
            Self::Discovery(scope) => scope.key(),
        }
    }
}

/// A published binding or discovery evidence set: its ID is what member
/// records and snapshots reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedSet {
    pub evidence_id: Digest,
    pub published: Published,
}

/// A live evidence set of one subject. Its observations are unverified
/// until `EvidenceReader::next_batch` reports `Hit(None)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceHeader {
    scope: Scope,
    evidence_id: Digest,
    subject: EvidenceSubject,
    artifact_id: Option<Digest>,
    observation_count: u64,
    content_bytes: u64,
    publication_seq: u64,
}

impl EvidenceHeader {
    pub fn evidence_id(&self) -> Digest {
        self.evidence_id
    }

    pub fn purpose(&self) -> EvidencePurpose {
        self.subject.purpose
    }

    /// The artifact the evidence concludes: present exactly for artifact
    /// evidence.
    pub fn artifact_id(&self) -> Option<Digest> {
        self.artifact_id
    }

    pub fn subject(&self) -> EvidenceSubject {
        self.subject
    }

    pub fn observation_count(&self) -> u64 {
        self.observation_count
    }

    /// When the set was last published, store-wide. A recency hint for
    /// ordering candidates; never evidence of freshness.
    pub fn publication_seq(&self) -> u64 {
        self.publication_seq
    }
}

struct EncodedObservation {
    kind: u8,
    path_key: Vec<u8>,
    payload: Vec<u8>,
}

/// Encode and admit every observation before any transaction (spec §10).
fn encode_observations(
    observations: &[Observation],
) -> Result<(Vec<EncodedObservation>, u64), Bypass> {
    if observations.len() > MAX_EVIDENCE_OBSERVATIONS {
        return Err(Bypass::TooLarge(TooLarge::Observations {
            count: observations.len(),
        }));
    }
    let mut encoded = Vec::with_capacity(observations.len());
    let mut content = 0usize;
    for (ordinal, observation) in observations.iter().enumerate() {
        let path_key = path_key(observation.path());
        if path_key.len() > MAX_INDEX_KEY_BYTES {
            return Err(Bypass::TooLarge(TooLarge::IndexKey {
                ordinal,
                bytes: path_key.len(),
            }));
        }
        let payload = observation.encoded();
        let bytes = payload.len() + path_key.len();
        if bytes > MAX_ROW_BATCH_BYTES {
            return Err(Bypass::TooLarge(TooLarge::Row { ordinal, bytes }));
        }
        content += bytes;
        if content > MAX_ARTIFACT_CONTENT_BYTES {
            return Err(Bypass::TooLarge(TooLarge::Content { bytes: content }));
        }
        encoded.push(EncodedObservation {
            kind: observation.kind(),
            path_key,
            payload,
        });
    }
    Ok((encoded, content as u64))
}

#[derive(Debug, PartialEq, Eq)]
struct StoredEvidence {
    purpose: i64,
    context_key: Vec<u8>,
    subject_key: Vec<u8>,
    artifact_id: Option<Vec<u8>>,
    lifecycle: i64,
    observation_count: i64,
    content_bytes: i64,
}

/// A stored set and its publication sequence, which is not part of its
/// identity.
fn read_evidence_row(
    connection: &Connection,
    scope: Scope,
    evidence_id: Digest,
) -> rusqlite::Result<Option<(StoredEvidence, i64)>> {
    connection
        .query_row(
            "SELECT purpose, context_key, subject_key, artifact_id, lifecycle, \
             observation_count, content_bytes, publication_seq FROM evidence_sets \
             WHERE namespace_id = ?1 AND reset_epoch = ?2 AND evidence_id = ?3",
            params![
                scope.namespace.as_bytes().as_slice(),
                epoch(scope),
                evidence_id.as_bytes().as_slice()
            ],
            |row| {
                Ok((
                    StoredEvidence {
                        purpose: row.get(0)?,
                        context_key: row.get(1)?,
                        subject_key: row.get(2)?,
                        artifact_id: row.get(3)?,
                        lifecycle: row.get(4)?,
                        observation_count: row.get(5)?,
                        content_bytes: row.get(6)?,
                    },
                    row.get(7)?,
                ))
            },
        )
        .optional()
}

/// An encoded, admitted and hashed evidence set, ready for its transaction.
struct PreparedEvidence {
    id: Digest,
    expected: StoredEvidence,
    observations: Vec<EncodedObservation>,
    bytes: u64,
}

fn prepare_evidence(
    subject: EvidenceSubject,
    artifact: Option<Digest>,
    observations: &[Observation],
) -> Result<PreparedEvidence, Bypass> {
    let (observations, bytes) = encode_observations(observations)?;
    let mut hasher = EvidenceHasher::new(&subject, artifact, observations.len());
    for observation in &observations {
        hasher.update(&observation.payload);
    }
    Ok(PreparedEvidence {
        id: hasher.finish(),
        expected: StoredEvidence {
            purpose: i64::from(subject.purpose.tag()),
            context_key: subject.context.as_bytes().to_vec(),
            subject_key: subject.subject.as_bytes().to_vec(),
            artifact_id: artifact.map(|artifact| artifact.as_bytes().to_vec()),
            lifecycle: LIVE,
            observation_count: observations.len() as i64,
            content_bytes: bytes as i64,
        },
        observations,
        bytes,
    })
}

/// Insert a prepared set inside the caller's write transaction. An
/// identical stored set takes the next publication sequence instead, so it
/// becomes the most recent candidate of its subject (l4d-e8bl).
fn insert_evidence(
    transaction: &Connection,
    scope: Scope,
    prepared: &PreparedEvidence,
) -> Result<Published, Bypass> {
    let fail = |error: rusqlite::Error| classify(&error);
    let sequence = next_publication_seq(transaction)?;
    let namespace = scope.namespace.as_bytes().as_slice();
    let reset_epoch = epoch(scope);
    let id = prepared.id.as_bytes().as_slice();
    match read_evidence_row(transaction, scope, prepared.id).map_err(fail)? {
        Some((stored, _)) if stored.lifecycle != LIVE => Err(Bypass::Retiring),
        Some((stored, _)) if stored == prepared.expected => {
            transaction
                .execute(
                    "UPDATE evidence_sets SET publication_seq = ?4 \
                     WHERE namespace_id = ?1 AND reset_epoch = ?2 AND evidence_id = ?3",
                    params![namespace, reset_epoch, id, sequence],
                )
                .map_err(fail)?;
            Ok(Published::AlreadyPresent)
        }
        Some(_) => Err(Bypass::Integrity(Integrity::ConflictingEvidence)),
        None => {
            let expected = &prepared.expected;
            transaction
                .execute(
                    "INSERT INTO evidence_sets (namespace_id, reset_epoch, evidence_id, purpose, \
                     context_key, subject_key, artifact_id, lifecycle, observation_count, \
                     content_bytes, publication_seq) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        namespace,
                        reset_epoch,
                        id,
                        expected.purpose,
                        expected.context_key,
                        expected.subject_key,
                        expected.artifact_id,
                        expected.lifecycle,
                        expected.observation_count,
                        expected.content_bytes,
                        sequence
                    ],
                )
                .map_err(fail)?;
            let mut insert = transaction
                .prepare_cached(
                    "INSERT INTO observations (namespace_id, reset_epoch, evidence_id, \
                     ordinal, kind, path_key, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .map_err(fail)?;
            for (ordinal, observation) in prepared.observations.iter().enumerate() {
                insert
                    .execute(params![
                        namespace,
                        reset_epoch,
                        id,
                        ordinal as i64,
                        i64::from(observation.kind),
                        observation.path_key,
                        observation.payload
                    ])
                    .map_err(fail)?;
            }
            Ok(Published::Inserted)
        }
    }
}

/// `evidence_id, artifact_id, observation_count, content_bytes,
/// publication_seq` as stored.
type CandidateRow = (Vec<u8>, Option<Vec<u8>>, i64, i64, i64);

/// A header from stored columns, or `None` if any is out of range.
fn header_from(
    scope: Scope,
    subject: EvidenceSubject,
    row: CandidateRow,
) -> Option<EvidenceHeader> {
    let (evidence_id, artifact_id, count, bytes, sequence) = row;
    let evidence_id = <[u8; 32]>::try_from(evidence_id.as_slice()).ok()?;
    let artifact_id = match artifact_id {
        None => None,
        Some(id) => Some(Digest::from_bytes(
            <[u8; 32]>::try_from(id.as_slice()).ok()?,
        )),
    };
    if artifact_id.is_some() != (subject.purpose == EvidencePurpose::Artifact) {
        return None;
    }
    let (count, bytes, sequence) = (
        u64::try_from(count).ok()?,
        u64::try_from(bytes).ok()?,
        u64::try_from(sequence).ok()?,
    );
    if count > MAX_EVIDENCE_OBSERVATIONS as u64 || bytes > MAX_ARTIFACT_CONTENT_BYTES as u64 {
        return None;
    }
    Some(EvidenceHeader {
        scope,
        evidence_id: Digest::from_bytes(evidence_id),
        subject,
        artifact_id,
        observation_count: count,
        content_bytes: bytes,
        publication_seq: sequence,
    })
}

impl Store {
    /// Publish an expanded artifact's facts and the evidence that proves
    /// them in one transaction (spec §6.2). Hashing and encoding happen
    /// before it starts and are charged to the scratch allowance.
    pub fn publish_facts_with_evidence(
        &self,
        key: ArtifactKeyRef<'_>,
        facts: &ArtifactFacts,
        evidence: &EvidenceWrite<'_>,
    ) -> Result<PublishedEvidence, Bypass> {
        let Some(root) = key.root() else {
            return Err(Bypass::EvidenceForLocalArtifact);
        };
        check_manifest_size(evidence.context.key())?;
        let fact_bytes = encoded_content_bytes(facts).map_err(Bypass::TooLarge)?;
        let subject = EvidenceSubject {
            purpose: EvidencePurpose::Artifact,
            context: evidence.context.digest(),
            subject: root.digest(),
        };
        let prepared = prepare_evidence(subject, Some(key.digest()), evidence.observations)?;
        let Some(_scratch) = self.scratch.reserve(fact_bytes as u64 + prepared.bytes) else {
            return Err(Bypass::ScratchExhausted);
        };
        let encoded = encode_facts(facts).map_err(Bypass::TooLarge)?;
        let rows = encoded.rows();
        let write = ArtifactWrite {
            key,
            supplemental: &encoded.supplemental,
            rows: &rows,
        };
        let artifact_prepared = prepare_artifact(&write)?;
        let mut writer = self.writer()?;
        let transaction = begin(&mut writer, TransactionBehavior::Immediate)?;
        ensure_live(&transaction, self.scope)?;
        let artifact = insert_prepared(&transaction, self.scope, &write, &artifact_prepared)?;
        put_manifest(&transaction, self.scope, evidence.context.key())?;
        let published = insert_evidence(&transaction, self.scope, &prepared)?;
        fault_point!(self, EvidenceBeforeCommit);
        self.before_commit();
        transaction.commit().map_err(|error| classify(&error))?;
        Ok(PublishedEvidence {
            artifact,
            evidence: published,
        })
    }

    /// Publish binding or discovery evidence with its subject's and
    /// context's manifests in one transaction. Republishing an identical
    /// set makes it its subject's most recent candidate.
    pub fn publish_evidence(
        &self,
        target: EvidenceTarget<'_>,
        evidence: &EvidenceWrite<'_>,
    ) -> Result<PublishedSet, Bypass> {
        check_manifest_size(evidence.context.key())?;
        check_manifest_size(target.key())?;
        let subject = EvidenceSubject {
            purpose: target.purpose(),
            context: evidence.context.digest(),
            subject: target.key().digest(),
        };
        let prepared = prepare_evidence(subject, None, evidence.observations)?;
        let Some(_scratch) = self.scratch.reserve(prepared.bytes) else {
            return Err(Bypass::ScratchExhausted);
        };
        let mut writer = self.writer()?;
        let transaction = begin(&mut writer, TransactionBehavior::Immediate)?;
        ensure_live(&transaction, self.scope)?;
        put_manifest(&transaction, self.scope, evidence.context.key())?;
        put_manifest(&transaction, self.scope, target.key())?;
        let published = insert_evidence(&transaction, self.scope, &prepared)?;
        fault_point!(self, EvidenceBeforeCommit);
        self.before_commit();
        transaction.commit().map_err(|error| classify(&error))?;
        Ok(PublishedSet {
            evidence_id: prepared.id,
            published,
        })
    }

    /// The live artifact evidence of `root` under `context`, most recent
    /// first, at most `MAX_EVIDENCE_CANDIDATES`. Observations are unverified.
    pub fn find_evidence(
        &self,
        context: &ContextKey,
        root: &LocalArtifactKey,
    ) -> Result<Lookup<Vec<EvidenceHeader>>, Bypass> {
        self.find_subject(context, EvidencePurpose::Artifact, root.key())
    }

    /// The live binding or discovery evidence of `target` under `context`,
    /// most recent first, at most `MAX_EVIDENCE_CANDIDATES`.
    pub fn find_subject_evidence(
        &self,
        context: &ContextKey,
        target: EvidenceTarget<'_>,
    ) -> Result<Lookup<Vec<EvidenceHeader>>, Bypass> {
        self.find_subject(context, target.purpose(), target.key())
    }

    fn find_subject(
        &self,
        context: &ContextKey,
        purpose: EvidencePurpose,
        subject_key: &CanonicalKey,
    ) -> Result<Lookup<Vec<EvidenceHeader>>, Bypass> {
        let mut reader = self.reader()?;
        let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
        ensure_live(&transaction, self.scope)?;
        for key in [context.key(), subject_key] {
            if let Lookup::Miss(miss) =
                admit_manifest(&transaction, &self.namespace, self.scope, key)?
            {
                return Ok(Lookup::Miss(miss));
            }
        }
        let subject = EvidenceSubject {
            purpose,
            context: context.digest(),
            subject: subject_key.digest(),
        };
        newest_first(
            &transaction,
            Candidates {
                select: "SELECT evidence_id, artifact_id, observation_count, content_bytes, \
                         publication_seq FROM evidence_sets WHERE namespace_id = ?1 \
                         AND reset_epoch = ?2 AND purpose = ?3 AND context_key = ?4 \
                         AND subject_key = ?5 AND lifecycle = ?6",
                params: params![
                    self.scope.namespace.as_bytes().as_slice(),
                    epoch(self.scope),
                    i64::from(purpose.tag()),
                    subject.context.as_bytes().as_slice(),
                    subject.subject.as_bytes().as_slice(),
                    LIVE
                ],
                limit: MAX_EVIDENCE_CANDIDATES,
                malformed: Malformed::Evidence,
            },
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
            |row: CandidateRow| header_from(self.scope, subject, row),
        )
    }

    /// The live evidence set `id`, as a member record or snapshot references
    /// it. Its context and subject manifests must hash to the stored
    /// digests (`admit_manifest_by_digest`); observations are unverified.
    pub fn open_evidence(&self, id: Digest) -> Result<Lookup<EvidenceHeader>, Bypass> {
        let mut reader = self.reader()?;
        let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
        ensure_live(&transaction, self.scope)?;
        let (stored, sequence) = match read_evidence_row(&transaction, self.scope, id) {
            Ok(Some(row)) => row,
            Ok(None) => return Ok(Lookup::Miss(Miss::Absent)),
            Err(error) => return classify_read(&error).map(Lookup::Miss),
        };
        if stored.lifecycle != LIVE {
            return Ok(Lookup::Miss(Miss::Retired));
        }
        let malformed = Ok(Lookup::Miss(Miss::Malformed(Malformed::Evidence)));
        let Some(purpose) = u8::try_from(stored.purpose)
            .ok()
            .and_then(EvidencePurpose::from_tag)
        else {
            return malformed;
        };
        let (Ok(context), Ok(subject)) = (
            <[u8; 32]>::try_from(stored.context_key.as_slice()),
            <[u8; 32]>::try_from(stored.subject_key.as_slice()),
        ) else {
            return malformed;
        };
        let subject = EvidenceSubject {
            purpose,
            context: Digest::from_bytes(context),
            subject: Digest::from_bytes(subject),
        };
        for (digest, kind) in [
            (subject.context, KeyKind::Context),
            (subject.subject, purpose.subject_kind()),
        ] {
            if let Lookup::Miss(miss) =
                admit_manifest_by_digest(&transaction, self.scope, digest, kind)?
            {
                return Ok(Lookup::Miss(miss));
            }
        }
        let row = (
            id.as_bytes().to_vec(),
            stored.artifact_id,
            stored.observation_count,
            stored.content_bytes,
            sequence,
        );
        match header_from(self.scope, subject, row) {
            Some(header) => Ok(Lookup::Hit(header)),
            None => malformed,
        }
    }

    pub fn read_evidence(&self, header: &EvidenceHeader) -> EvidenceReader<'_> {
        EvidenceReader(ManifestReader::new(
            self,
            Expected {
                scope: header.scope,
                parent: header.evidence_id.as_bytes().to_vec(),
                count: header.observation_count,
                content_bytes: Some(header.content_bytes),
                witness: header.evidence_id,
            },
            ObservationSource(EvidenceHasher::new(
                &header.subject,
                header.artifact_id,
                header.observation_count as usize,
            )),
        ))
    }
}

/// The rows of one evidence set: observations whose `kind` and `path_key`
/// columns must agree with their payload, folded into the evidence ID.
#[derive(Clone)]
struct ObservationSource(EvidenceHasher);

impl RowSource for ObservationSource {
    type Item = Observation;
    const PARENT_SQL: &'static str = "SELECT lifecycle FROM evidence_sets \
         WHERE namespace_id = ?1 AND reset_epoch = ?2 AND evidence_id = ?3";
    const ROWS_SQL: &'static str = "SELECT ordinal, kind, path_key, payload FROM observations \
         WHERE namespace_id = ?1 AND reset_epoch = ?2 AND evidence_id = ?3 \
         AND ordinal >= ?4 ORDER BY ordinal LIMIT ?5";
    const TRAILING_SQL: &'static str = "SELECT 1 FROM observations \
         WHERE namespace_id = ?1 AND reset_epoch = ?2 AND evidence_id = ?3 \
         AND ordinal >= ?4 LIMIT 1";

    fn size(row: &rusqlite::Row<'_>) -> rusqlite::Result<usize> {
        Ok(row.get_ref(2)?.as_blob()?.len() + row.get_ref(3)?.as_blob()?.len())
    }

    fn take(&mut self, row: &rusqlite::Row<'_>) -> Result<Observation, RowError> {
        let kind: i64 = row.get(1)?;
        let stored_path_key = row.get_ref(2)?.as_blob()?;
        let payload = row.get_ref(3)?.as_blob()?;
        let malformed = RowError::Miss(Miss::Malformed(Malformed::Evidence));
        let Ok(observation) = decode_observation(payload) else {
            return Err(malformed);
        };
        if i64::from(observation.kind()) != kind || path_key(observation.path()) != stored_path_key
        {
            return Err(malformed);
        }
        self.0.update(payload);
        Ok(observation)
    }

    fn finish(&self) -> Digest {
        self.0.finish()
    }
}

/// At most 256 observations decoded from at most 1 MiB of rows, charged
/// to the store's scratch budget until dropped.
#[derive(Debug)]
pub struct ObservationBatch {
    observations: Vec<Observation>,
    _scratch: ScratchLease,
}

impl ObservationBatch {
    pub fn observations(&self) -> &[Observation] {
        &self.observations
    }
}

impl PartialEq for ObservationBatch {
    fn eq(&self, other: &Self) -> bool {
        self.observations == other.observations
    }
}

impl Eq for ObservationBatch {}

/// Streams an evidence set's observations in bounded batches, each in its
/// own short read transaction, while hashing the evidence manifest.
pub struct EvidenceReader<'a>(ManifestReader<'a, ObservationSource>);

impl EvidenceReader<'_> {
    /// Whether every observation was read and the manifest digest equals
    /// the evidence ID. Only then may the evidence prove anything.
    pub fn is_verified(&self) -> bool {
        self.0.is_verified()
    }

    /// The next batch, or `Hit(None)` once every observation was read and
    /// verified. A validator may reject the evidence on an unverified
    /// batch, but accepts it only after `Hit(None)`.
    pub fn next_batch(&mut self) -> Result<Lookup<Option<ObservationBatch>>, Bypass> {
        self.0
            .next_batch_as(|observations, scratch| ObservationBatch {
                observations,
                _scratch: scratch,
            })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{ExpandedArtifactKey, Namespace, SourceContent, StoreLimits};
    use pascal_project::{ConditionalContext, ProjectContext, SourceOrigin};

    #[test]
    fn evidence_with_the_same_id_but_other_counts_conflicts() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            directory.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let store = Store::open(
            directory.path(),
            &Namespace::new(Digest::from_bytes([6; 32])),
            StoreLimits::default(),
        )
        .unwrap();
        let root = LocalArtifactKey::new(
            &SourceContent::from_bytes(b"unit A;"),
            &ConditionalContext::default(),
            SourceOrigin::Library,
        );
        let expanded = ExpandedArtifactKey::new(&root, &[]).unwrap();
        let context = ContextKey::new(&ProjectContext::default());
        let key = ArtifactKeyRef::expanded(&expanded, &root).unwrap();
        let facts = ArtifactFacts {
            slot_lengths: vec![7],
            expanded_length: 7,
            ..ArtifactFacts::default()
        };
        let observations = [Observation::Presence {
            path: "/w/a.inc".into(),
            entry: None,
            present: false,
        }];
        let write = EvidenceWrite {
            context: &context,
            observations: &observations,
        };
        store
            .publish_facts_with_evidence(key, &facts, &write)
            .unwrap();
        let writer = store.writer().unwrap();
        writer
            .execute(
                "UPDATE evidence_sets SET content_bytes = content_bytes + 1",
                [],
            )
            .unwrap();
        drop(writer);
        assert_eq!(
            store.publish_facts_with_evidence(key, &facts, &write),
            Err(Bypass::Integrity(Integrity::ConflictingEvidence))
        );
    }
}
