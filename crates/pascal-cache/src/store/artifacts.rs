use super::Store;
use super::keys::{ArtifactKeyRef, admit_manifest, check_manifest_size, put_manifest};
use super::limits::{
    MAX_ARTIFACT_CONTENT_BYTES, MAX_ARTIFACT_ROWS, MAX_INDEX_KEY_BYTES, MAX_ROW_BATCH_BYTES,
    MAX_SUPPLEMENTAL_BYTES,
};
use super::namespaces::{Scope, ensure_live, epoch};
use super::outcome::{Bypass, Integrity, Lookup, Malformed, Miss, Published, TooLarge};
use super::sqlite::{begin, classify, classify_read};
use crate::{Digest, Encoder};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

/// Domain of the whole-artifact fact checksum.
pub const FACT_CHECKSUM_CONTEXT: &str =
    "lint4d persistent analysis cache 2026-10 artifact facts v1";
/// Domain of each row checksum.
pub const ROW_CHECKSUM_CONTEXT: &str = "lint4d persistent analysis cache 2026-10 artifact row v1";

const LIVE: i64 = 1;

/// One fact row as a producer supplies it. The producer (`l4d-zf1b.4`)
/// owns `row_kind` (1-255) and the payload encoding; `index_key` is the
/// binary lookup key (a normalized name) compared bytewise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactRow<'a> {
    pub row_kind: u8,
    pub index_key: Option<&'a [u8]>,
    pub payload: &'a [u8],
}

#[derive(Debug, Clone, Copy)]
pub struct ArtifactWrite<'a> {
    pub key: ArtifactKeyRef<'a>,
    /// Header data owned by the producer (counts per kind, coverage).
    pub supplemental: &'a [u8],
    pub rows: &'a [ArtifactRow<'a>],
}

/// An admitted artifact header. Its counts and rows are unverified until
/// [`Store::read_artifact`] has streamed and checked every row and the reader
/// reports `is_verified()`; its supplemental bytes are available only then,
/// through `ArtifactReader::supplemental`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactHeader {
    pub(crate) scope: Scope,
    pub(crate) artifact_id: Digest,
    pub(crate) row_count: u64,
    pub(crate) content_bytes: u64,
    pub(crate) fact_checksum: Digest,
    pub(crate) supplemental: Vec<u8>,
}

impl ArtifactHeader {
    pub fn artifact_id(&self) -> Digest {
        self.artifact_id
    }

    pub fn row_count(&self) -> u64 {
        self.row_count
    }

    /// Bytes of payloads and index keys over all rows.
    pub fn content_bytes(&self) -> u64 {
        self.content_bytes
    }
}

/// Feed the canonical encoding of one row to `sink` in pieces, borrowing the
/// payload and key: nothing is materialized. Its checksum is stored with the
/// row, and the fact checksum covers it, so the artifact and position are
/// bound into both. Field order and framing are those of [`Encoder`].
fn stream_row_encoding(
    artifact_id: Digest,
    ordinal: u64,
    row_kind: u8,
    index_key: Option<&[u8]>,
    payload: &[u8],
    sink: &mut impl FnMut(&[u8]),
) {
    sink(artifact_id.as_bytes());
    sink(&ordinal.to_le_bytes());
    sink(&[row_kind]);
    match index_key {
        None => sink(&[0]),
        Some(key) => {
            sink(&[1]);
            sink(&(key.len() as u64).to_le_bytes());
            sink(key);
        }
    }
    sink(&(payload.len() as u64).to_le_bytes());
    sink(payload);
}

/// Hash one row into `fact` and return the row's own checksum, streaming
/// the encoding straight from the borrowed slices.
pub(crate) fn hash_row(
    fact: &mut FactHasher,
    artifact_id: Digest,
    ordinal: u64,
    row_kind: u8,
    index_key: Option<&[u8]>,
    payload: &[u8],
) -> Digest {
    let mut row = blake3::Hasher::new_derive_key(ROW_CHECKSUM_CONTEXT);
    stream_row_encoding(
        artifact_id,
        ordinal,
        row_kind,
        index_key,
        payload,
        &mut |piece| {
            row.update(piece);
            fact.0.update(piece);
        },
    );
    Digest::from_bytes(*row.finalize().as_bytes())
}

/// The fact checksum: the header fields, then every row in ordinal order.
#[derive(Clone)]
pub(crate) struct FactHasher(blake3::Hasher);

impl FactHasher {
    pub(crate) fn new(artifact_id: Digest, row_count: u64, supplemental: &[u8]) -> Self {
        let mut encoder = Encoder::new();
        encoder.digest(&artifact_id);
        encoder.u64(row_count);
        encoder.bytes(supplemental);
        let mut hasher = blake3::Hasher::new_derive_key(FACT_CHECKSUM_CONTEXT);
        hasher.update(encoder.as_bytes());
        Self(hasher)
    }

    pub(crate) fn finish(&self) -> Digest {
        Digest::from_bytes(*self.0.finalize().as_bytes())
    }
}

/// Check every admission limit before any work: an oversized artifact is
/// simply not persisted (spec §6.2, §10).
fn admit_write(write: &ArtifactWrite<'_>) -> Result<u64, Bypass> {
    check_manifest_size(write.key.key())?;
    if let Some(root) = write.key.root() {
        check_manifest_size(root)?;
    }
    let too_large = |limit| Err(Bypass::TooLarge(limit));
    if write.supplemental.len() > MAX_SUPPLEMENTAL_BYTES {
        return too_large(TooLarge::Supplemental {
            bytes: write.supplemental.len(),
        });
    }
    if write.rows.len() > MAX_ARTIFACT_ROWS {
        return too_large(TooLarge::Rows {
            count: write.rows.len(),
        });
    }
    let mut content = 0usize;
    for (ordinal, row) in write.rows.iter().enumerate() {
        if row.row_kind == 0 {
            return Err(Bypass::InvalidRowKind { ordinal });
        }
        let key_bytes = row.index_key.map_or(0, <[u8]>::len);
        if key_bytes > MAX_INDEX_KEY_BYTES {
            return too_large(TooLarge::IndexKey {
                ordinal,
                bytes: key_bytes,
            });
        }
        let bytes = row.payload.len() + key_bytes;
        if bytes > MAX_ROW_BATCH_BYTES {
            return too_large(TooLarge::Row { ordinal, bytes });
        }
        content += bytes;
        if content > MAX_ARTIFACT_CONTENT_BYTES {
            return too_large(TooLarge::Content { bytes: content });
        }
    }
    Ok(content as u64)
}

#[derive(Debug, PartialEq, Eq)]
struct StoredHeader {
    kind: i64,
    root_key: Option<Vec<u8>>,
    lifecycle: i64,
    row_count: i64,
    content_bytes: i64,
    fact_checksum: Vec<u8>,
    supplemental: Vec<u8>,
}

fn read_header(
    connection: &Connection,
    scope: Scope,
    artifact_id: Digest,
) -> rusqlite::Result<Option<StoredHeader>> {
    connection
        .query_row(
            "SELECT kind, root_key, lifecycle, row_count, content_bytes, fact_checksum, supplemental \
             FROM artifacts WHERE namespace_id = ?1 AND reset_epoch = ?2 AND artifact_id = ?3",
            params![
                scope.namespace.as_bytes().as_slice(),
                epoch(scope),
                artifact_id.as_bytes().as_slice()
            ],
            |row| {
                Ok(StoredHeader {
                    kind: row.get(0)?,
                    root_key: row.get(1)?,
                    lifecycle: row.get(2)?,
                    row_count: row.get(3)?,
                    content_bytes: row.get(4)?,
                    fact_checksum: row.get(5)?,
                    supplemental: row.get(6)?,
                })
            },
        )
        .optional()
}

impl Store {
    /// Publish an immutable artifact: key manifests, header and rows in one
    /// transaction. Hashing happens before the transaction starts.
    pub fn publish_artifact(&self, write: &ArtifactWrite<'_>) -> Result<Published, Bypass> {
        let content_bytes = admit_write(write)?;
        let artifact_id = write.key.digest();
        let mut hasher = FactHasher::new(artifact_id, write.rows.len() as u64, write.supplemental);
        let mut checksums = Vec::with_capacity(write.rows.len());
        for (ordinal, row) in write.rows.iter().enumerate() {
            checksums.push(hash_row(
                &mut hasher,
                artifact_id,
                ordinal as u64,
                row.row_kind,
                row.index_key,
                row.payload,
            ));
        }
        let expected = StoredHeader {
            kind: i64::from(write.key.key().kind().tag()),
            root_key: write
                .key
                .root()
                .map(|root| root.digest().as_bytes().to_vec()),
            lifecycle: LIVE,
            row_count: write.rows.len() as i64,
            content_bytes: content_bytes as i64,
            fact_checksum: hasher.finish().as_bytes().to_vec(),
            supplemental: write.supplemental.to_vec(),
        };

        let fail = |error: rusqlite::Error| classify(&error);
        let mut writer = self.writer()?;
        let transaction = begin(&mut writer, TransactionBehavior::Immediate)?;
        ensure_live(&transaction, self.scope)?;
        put_manifest(&transaction, self.scope, write.key.key())?;
        if let Some(root) = write.key.root() {
            put_manifest(&transaction, self.scope, root)?;
        }
        if let Some(stored) = read_header(&transaction, self.scope, artifact_id).map_err(fail)? {
            return if stored.lifecycle != LIVE {
                Err(Bypass::Retiring)
            } else if stored == expected {
                Ok(Published::AlreadyPresent)
            } else {
                Err(Bypass::Integrity(Integrity::ConflictingArtifact))
            };
        }
        let namespace = self.scope.namespace.as_bytes().as_slice();
        let reset_epoch = epoch(self.scope);
        transaction
            .execute(
                "INSERT INTO artifacts (namespace_id, reset_epoch, artifact_id, kind, root_key, \
                 lifecycle, row_count, content_bytes, fact_checksum, supplemental) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    namespace,
                    reset_epoch,
                    artifact_id.as_bytes().as_slice(),
                    expected.kind,
                    expected.root_key,
                    expected.lifecycle,
                    expected.row_count,
                    expected.content_bytes,
                    expected.fact_checksum,
                    expected.supplemental
                ],
            )
            .map_err(fail)?;
        {
            let mut insert = transaction
                .prepare_cached(
                    "INSERT INTO artifact_rows (namespace_id, reset_epoch, artifact_id, ordinal, \
                     row_kind, index_key, payload, row_checksum) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )
                .map_err(fail)?;
            for (ordinal, (row, checksum)) in write.rows.iter().zip(&checksums).enumerate() {
                insert
                    .execute(params![
                        namespace,
                        reset_epoch,
                        artifact_id.as_bytes().as_slice(),
                        ordinal as i64,
                        i64::from(row.row_kind),
                        row.index_key,
                        row.payload,
                        checksum.as_bytes().as_slice()
                    ])
                    .map_err(fail)?;
            }
        }
        self.before_commit();
        transaction.commit().map_err(fail)?;
        Ok(Published::Inserted)
    }

    /// Admit the artifact's key manifests (and its root's) and its header.
    pub fn open_artifact(&self, key: ArtifactKeyRef<'_>) -> Result<Lookup<ArtifactHeader>, Bypass> {
        let mut reader = self.reader()?;
        let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
        ensure_live(&transaction, self.scope)?;
        for manifest in std::iter::once(key.key()).chain(key.root()) {
            if let Lookup::Miss(miss) =
                admit_manifest(&transaction, &self.namespace, self.scope, manifest)?
            {
                return Ok(Lookup::Miss(miss));
            }
        }
        let stored = match read_header(&transaction, self.scope, key.digest()) {
            Ok(Some(stored)) => stored,
            Ok(None) => return Ok(Lookup::Miss(Miss::Absent)),
            Err(error) => return classify_read(&error).map(Lookup::Miss),
        };
        if stored.lifecycle != LIVE {
            return Ok(Lookup::Miss(Miss::Retired));
        }
        let malformed = Lookup::Miss(Miss::Malformed(Malformed::Header));
        let expected_root = key.root().map(|root| root.digest().as_bytes().to_vec());
        let (Ok(row_count), Ok(content_bytes), Ok(fact_checksum)) = (
            u64::try_from(stored.row_count),
            u64::try_from(stored.content_bytes),
            <[u8; 32]>::try_from(stored.fact_checksum.as_slice()),
        ) else {
            return Ok(malformed);
        };
        if stored.kind != i64::from(key.key().kind().tag())
            || stored.root_key != expected_root
            || row_count > MAX_ARTIFACT_ROWS as u64
            || content_bytes > MAX_ARTIFACT_CONTENT_BYTES as u64
            || stored.supplemental.len() > MAX_SUPPLEMENTAL_BYTES
        {
            return Ok(malformed);
        }
        Ok(Lookup::Hit(ArtifactHeader {
            scope: self.scope,
            artifact_id: key.digest(),
            row_count,
            content_bytes,
            fact_checksum: Digest::from_bytes(fact_checksum),
            supplemental: stored.supplemental,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoder_checksums(
        id: Digest,
        ordinal: u64,
        kind: u8,
        key: Option<&[u8]>,
        payload: &[u8],
    ) -> (Digest, Digest) {
        let mut encoder = Encoder::new();
        encoder.digest(&id);
        encoder.u64(ordinal);
        encoder.u8(kind);
        encoder.option(key, |encoder, key| encoder.bytes(key));
        encoder.bytes(payload);
        let mut reference = FactHasher::new(id, 1, b"s");
        reference.0.update(encoder.as_bytes());
        (
            Digest::derive(ROW_CHECKSUM_CONTEXT, encoder.as_bytes()),
            reference.finish(),
        )
    }

    #[test]
    fn streamed_checksums_equal_the_encoder_framing() {
        let id = Digest::from_bytes([7; 32]);
        let big = vec![9u8; 5000];
        type Case<'a> = (u64, u8, Option<&'a [u8]>, &'a [u8]);
        let cases: [Case<'_>; 5] = [
            (0, 1, None, b""),
            (1, 2, Some(b""), b"payload"),
            (300, 255, Some(b"name"), b""),
            (u64::from(u32::MAX) + 1, 0, Some(&big), &big),
            (5, 3, None, b"x"),
        ];
        for (ordinal, kind, key, payload) in cases {
            let mut fact = FactHasher::new(id, 1, b"s");
            let checksum = hash_row(&mut fact, id, ordinal, kind, key, payload);
            let (expected_row, expected_fact) = encoder_checksums(id, ordinal, kind, key, payload);
            assert_eq!(checksum, expected_row, "row {ordinal}");
            assert_eq!(fact.finish(), expected_fact, "fact {ordinal}");
        }
    }
}
