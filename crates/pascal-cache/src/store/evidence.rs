//! Evidence sets: the observations that prove an expanded artifact for one
//! root under one context (spec §6.1, §7), published with the artifact and
//! read back in bounded batches under a streamed integrity witness.

use super::Store;
use super::artifacts::{ArtifactWrite, insert_prepared, prepare_artifact};
use super::keys::{ArtifactKeyRef, admit_manifest, check_manifest_size, put_manifest};
use super::limits::{
    MAX_ARTIFACT_CONTENT_BYTES, MAX_EVIDENCE_CANDIDATES, MAX_EVIDENCE_OBSERVATIONS,
    MAX_INDEX_KEY_BYTES, MAX_ROW_BATCH_BYTES,
};
use super::namespaces::{Scope, ensure_live, epoch};
use super::outcome::{Bypass, Integrity, Lookup, Malformed, Miss, Published, TooLarge};
use super::reader::ScratchLease;
use super::sqlite::{begin, classify, classify_read};
use super::stream::{Batch, Expected, ManifestReader, RowError, RowSource};
use crate::evidence::{EvidenceHasher, decode_observation, path_key};
use crate::facts::{encode_facts, encoded_content_bytes};
use crate::{ContextKey, Digest, EvidenceSubject, LocalArtifactKey, Observation};
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

/// A live evidence set of one subject. Its observations are unverified
/// until `EvidenceReader::next_batch` reports `Hit(None)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceHeader {
    scope: Scope,
    evidence_id: Digest,
    subject: EvidenceSubject,
    artifact_id: Digest,
    observation_count: u64,
    content_bytes: u64,
}

impl EvidenceHeader {
    pub fn evidence_id(&self) -> Digest {
        self.evidence_id
    }

    /// The artifact the evidence concludes.
    pub fn artifact_id(&self) -> Digest {
        self.artifact_id
    }

    pub fn subject(&self) -> EvidenceSubject {
        self.subject
    }

    pub fn observation_count(&self) -> u64 {
        self.observation_count
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

/// `evidence_id, artifact_id, observation_count, content_bytes` as stored.
type CandidateRow = (Vec<u8>, Vec<u8>, i64, i64);

#[derive(Debug, PartialEq, Eq)]
struct StoredEvidence {
    context_key: Vec<u8>,
    root_key: Vec<u8>,
    artifact_id: Vec<u8>,
    lifecycle: i64,
    observation_count: i64,
    content_bytes: i64,
}

fn read_evidence_row(
    connection: &Connection,
    scope: Scope,
    evidence_id: Digest,
) -> rusqlite::Result<Option<StoredEvidence>> {
    connection
        .query_row(
            "SELECT context_key, root_key, artifact_id, lifecycle, observation_count, content_bytes \
             FROM evidence_sets WHERE namespace_id = ?1 AND reset_epoch = ?2 AND evidence_id = ?3",
            params![
                scope.namespace.as_bytes().as_slice(),
                epoch(scope),
                evidence_id.as_bytes().as_slice()
            ],
            |row| {
                Ok(StoredEvidence {
                    context_key: row.get(0)?,
                    root_key: row.get(1)?,
                    artifact_id: row.get(2)?,
                    lifecycle: row.get(3)?,
                    observation_count: row.get(4)?,
                    content_bytes: row.get(5)?,
                })
            },
        )
        .optional()
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
        let (observations, observation_bytes) = encode_observations(evidence.observations)?;
        let Some(_scratch) = self.scratch.reserve(fact_bytes as u64 + observation_bytes) else {
            return Err(Bypass::ScratchExhausted);
        };
        let encoded = encode_facts(facts).map_err(Bypass::TooLarge)?;
        let rows = encoded.rows();
        let write = ArtifactWrite {
            key,
            supplemental: &encoded.supplemental,
            rows: &rows,
        };
        let prepared = prepare_artifact(&write)?;
        let subject = EvidenceSubject {
            context: evidence.context.digest(),
            root: root.digest(),
        };
        let mut hasher = EvidenceHasher::new(&subject, key.digest(), observations.len());
        for observation in &observations {
            hasher.update(&observation.payload);
        }
        let evidence_id = hasher.finish();
        let expected = StoredEvidence {
            context_key: subject.context.as_bytes().to_vec(),
            root_key: subject.root.as_bytes().to_vec(),
            artifact_id: key.digest().as_bytes().to_vec(),
            lifecycle: LIVE,
            observation_count: observations.len() as i64,
            content_bytes: observation_bytes as i64,
        };

        let fail = |error: rusqlite::Error| classify(&error);
        let mut writer = self.writer()?;
        let transaction = begin(&mut writer, TransactionBehavior::Immediate)?;
        ensure_live(&transaction, self.scope)?;
        let artifact = insert_prepared(&transaction, self.scope, &write, &prepared)?;
        put_manifest(&transaction, self.scope, evidence.context.key())?;
        let published =
            match read_evidence_row(&transaction, self.scope, evidence_id).map_err(fail)? {
                Some(stored) if stored.lifecycle != LIVE => return Err(Bypass::Retiring),
                Some(stored) if stored == expected => Published::AlreadyPresent,
                Some(_) => return Err(Bypass::Integrity(Integrity::ConflictingEvidence)),
                None => {
                    let namespace = self.scope.namespace.as_bytes().as_slice();
                    let reset_epoch = epoch(self.scope);
                    transaction
                        .execute(
                            "INSERT INTO evidence_sets (namespace_id, reset_epoch, evidence_id, \
                         context_key, root_key, artifact_id, lifecycle, observation_count, \
                         content_bytes) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                            params![
                                namespace,
                                reset_epoch,
                                evidence_id.as_bytes().as_slice(),
                                expected.context_key,
                                expected.root_key,
                                expected.artifact_id,
                                expected.lifecycle,
                                expected.observation_count,
                                expected.content_bytes
                            ],
                        )
                        .map_err(fail)?;
                    let mut insert = transaction
                        .prepare_cached(
                            "INSERT INTO observations (namespace_id, reset_epoch, evidence_id, \
                         ordinal, kind, path_key, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        )
                        .map_err(fail)?;
                    for (ordinal, observation) in observations.iter().enumerate() {
                        insert
                            .execute(params![
                                namespace,
                                reset_epoch,
                                evidence_id.as_bytes().as_slice(),
                                ordinal as i64,
                                i64::from(observation.kind),
                                observation.path_key,
                                observation.payload
                            ])
                            .map_err(fail)?;
                    }
                    Published::Inserted
                }
            };
        self.before_commit();
        transaction.commit().map_err(fail)?;
        Ok(PublishedEvidence {
            artifact,
            evidence: published,
        })
    }

    /// The live evidence sets of `root` under `context`, at most
    /// `MAX_EVIDENCE_CANDIDATES`. Their observations are unverified.
    pub fn find_evidence(
        &self,
        context: &ContextKey,
        root: &LocalArtifactKey,
    ) -> Result<Lookup<Vec<EvidenceHeader>>, Bypass> {
        let mut reader = self.reader()?;
        let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
        ensure_live(&transaction, self.scope)?;
        for key in [context.key(), root.key()] {
            if let Lookup::Miss(miss) =
                admit_manifest(&transaction, &self.namespace, self.scope, key)?
            {
                return Ok(Lookup::Miss(miss));
            }
        }
        let subject = EvidenceSubject {
            context: context.digest(),
            root: root.digest(),
        };
        let rows: rusqlite::Result<Vec<CandidateRow>> = transaction
            .prepare_cached(
                "SELECT evidence_id, artifact_id, observation_count, content_bytes \
                 FROM evidence_sets WHERE namespace_id = ?1 AND reset_epoch = ?2 \
                 AND context_key = ?3 AND root_key = ?4 AND lifecycle = ?5 \
                 ORDER BY evidence_id LIMIT ?6",
            )
            .and_then(|mut statement| {
                statement
                    .query_map(
                        params![
                            self.scope.namespace.as_bytes().as_slice(),
                            epoch(self.scope),
                            subject.context.as_bytes().as_slice(),
                            subject.root.as_bytes().as_slice(),
                            LIVE,
                            MAX_EVIDENCE_CANDIDATES as i64
                        ],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )?
                    .collect()
            });
        let rows = match rows {
            Ok(rows) => rows,
            Err(error) => return classify_read(&error).map(Lookup::Miss),
        };
        let mut headers = Vec::with_capacity(rows.len());
        for (evidence_id, artifact_id, count, bytes) in rows {
            let (Ok(evidence_id), Ok(artifact_id), Ok(count), Ok(bytes)) = (
                <[u8; 32]>::try_from(evidence_id.as_slice()),
                <[u8; 32]>::try_from(artifact_id.as_slice()),
                u64::try_from(count),
                u64::try_from(bytes),
            ) else {
                return Ok(Lookup::Miss(Miss::Malformed(Malformed::Evidence)));
            };
            if count > MAX_EVIDENCE_OBSERVATIONS as u64 || bytes > MAX_ARTIFACT_CONTENT_BYTES as u64
            {
                return Ok(Lookup::Miss(Miss::Malformed(Malformed::Evidence)));
            }
            headers.push(EvidenceHeader {
                scope: self.scope,
                evidence_id: Digest::from_bytes(evidence_id),
                subject,
                artifact_id: Digest::from_bytes(artifact_id),
                observation_count: count,
                content_bytes: bytes,
            });
        }
        if headers.is_empty() {
            return Ok(Lookup::Miss(Miss::Absent));
        }
        Ok(Lookup::Hit(headers))
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
        Ok(match self.0.next_batch()? {
            Lookup::Hit(Some(Batch { items, scratch })) => Lookup::Hit(Some(ObservationBatch {
                observations: items,
                _scratch: scratch,
            })),
            Lookup::Hit(None) => Lookup::Hit(None),
            Lookup::Miss(miss) => Lookup::Miss(miss),
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
