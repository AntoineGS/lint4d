//! Canonical durable keys for the persistent analysis cache.
//!
//! Inputs are encoded with canonical encoding v1 (see [`Encoder`]) into
//! manifests that are retained and compared exactly; BLAKE3 digests of those
//! manifests only narrow a lookup.

mod artifact_key;
mod context_key;
mod digest;
mod encoding;
mod facts;
mod member_key;
mod path;
mod project;
mod source;
mod store;

pub use artifact_key::{
    ExpandedArtifactKey, IncludeOccurrence, IncludeOutcome, KeyError, LocalArtifactKey,
};
pub use context_key::ContextKey;
pub use digest::{Digest, MANIFEST_DIGEST_CONTEXT, SOURCE_DIGEST_CONTEXT};
pub use encoding::{CanonicalEncode, ENCODING_VERSION, Encoder};
pub use facts::{
    DECLARATION_ROW, EncodedFacts, EncodedRow, FactDecodeError, FactDecoder, IMPORT_ROW,
    INCLUDE_ROW, POSTING_ROW, SEGMENT_ROW, encode_facts, encoded_content_bytes,
};
pub use member_key::{ArtifactRef, MemberInputs, MemberKey};
pub use source::{SourceContent, SourceIdentity};

#[cfg(test)]
mod build_fingerprint;
mod manifest;
mod namespace;

pub use manifest::{
    CanonicalKey, KeyKind, KeyRecord, MANIFEST_HEADER_LEN, MANIFEST_MAGIC, ManifestError,
    ManifestHeader, Mismatch,
};
pub use namespace::{BUILD_FINGERPRINT, Namespace};
pub use store::{
    APPLICATION_ID, ArtifactHeader, ArtifactKeyRef, ArtifactReader, ArtifactRow, ArtifactWrite,
    Bypass, CONTAINER_VERSION, DATABASE_FILE, FACT_CHECKSUM_CONTEXT, IndexHit, IndexPage,
    Integrity, LimitsError, LocationIssue, Lookup, MAX_ARTIFACT_CONTENT_BYTES, MAX_ARTIFACT_ROWS,
    MAX_INDEX_KEY_BYTES, MAX_INDEX_PAGE, MAX_MANIFEST_BYTES, MAX_OPEN_WAIT, MAX_PAGE_CACHE_BYTES,
    MAX_READERS, MAX_ROW_BATCH_BYTES, MAX_ROW_BATCH_ROWS, MAX_SCRATCH_BYTES,
    MAX_SUPPLEMENTAL_BYTES, MAX_WAIT, MIN_CONNECTION_PAGE_CACHE_BYTES, MIN_SQLITE_VERSION_NUMBER,
    Malformed, Miss, Published, ROW_CHECKSUM_CONTEXT, Reclaimed, RetireReport, RootMismatch,
    RowBatch, SQLITE_LENGTH_LIMIT, Scope, Store, StoreLimits, StoreLocation, StoredRow, TooLarge,
    default_directory,
};

#[cfg(feature = "test-support")]
pub use store::HeldConnection;
#[cfg(feature = "test-support")]
pub use store::INDEX_QUERY_FOR_TEST;
