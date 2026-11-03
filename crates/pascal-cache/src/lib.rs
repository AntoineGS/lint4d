//! Canonical durable keys for the persistent analysis cache.
//!
//! Inputs are encoded with canonical encoding v1 (see [`Encoder`]) into
//! manifests that are retained and compared exactly; BLAKE3 digests of those
//! manifests only narrow a lookup.

mod artifact_key;
mod context_key;
mod decode;
mod digest;
mod encoding;
mod evidence;
mod facts;
mod lookup_key;
mod member_key;
mod path;
mod project;
mod record;
mod source;
mod store;

pub use artifact_key::{
    ExpandedArtifactKey, IncludeOccurrence, IncludeOutcome, KeyError, LocalArtifactKey,
    SessionDependence,
};
pub use context_key::ContextKey;
pub use digest::{Digest, MANIFEST_DIGEST_CONTEXT, RecordListDigest, SOURCE_DIGEST_CONTEXT};
pub use encoding::{CanonicalEncode, ENCODING_VERSION, Encoder};
pub use evidence::{
    EvidencePurpose, EvidenceSubject, LISTING_DIGEST_CONTEXT, OBSERVATIONS_DIGEST_CONTEXT,
    Observation, ObservationError, decode_observation, evidence_id, listing_digest,
    observations_digest, path_key,
};
pub use facts::{
    DECLARATION_ROW, EncodedFacts, EncodedRow, FactDecodeError, FactDecoder, IMPORT_ROW,
    INCLUDE_ROW, POSTING_ROW, SEGMENT_ROW, encode_facts, encoded_content_bytes,
};
pub use lookup_key::{
    DiscoveryInputs, DiscoveryScope, LookupInputs, LookupKey, LookupKind, ScannedDirectory,
};
pub use member_key::{ArtifactRef, MemberInputs, MemberKey};
pub use record::{
    Binding, BindingKind, BindingOutcome, CoverageStatus, IncompleteReason,
    MAX_AMBIGUOUS_CANDIDATES, MemberCoverage, MemberError, MemberHeader, MemberRecord,
    RecordDecodeError, SnapshotCoverage, decode_binding, decode_member_header,
    decode_snapshot_coverage,
};
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
pub use store::admin;
pub use store::{
    APPLICATION_ID, ArtifactHeader, ArtifactKeyRef, ArtifactReader, ArtifactRow, ArtifactWrite,
    BindingBatch, Bypass, CONNECTION_OVERHEAD_BYTES, CONTAINER_VERSION, Checkpointed,
    DATABASE_FILE, EvidenceHeader, EvidenceReader, EvidenceTarget, EvidenceWrite,
    FACT_CHECKSUM_CONTEXT, Hint, HintStats, ImporterHit, ImporterPage, IndexHit, IndexPage,
    Integrity, JOURNAL_SIZE_LIMIT_BYTES, LEASE_EXPIRY, LEASE_RENEWAL, LOW_SPACE_MARGIN_BYTES,
    LimitsError, LocationIssue, Lookup, MAX_ARTIFACT_CONTENT_BYTES, MAX_ARTIFACT_ROWS,
    MAX_EVIDENCE_CANDIDATES, MAX_EVIDENCE_OBSERVATIONS, MAX_HEALTH_BACKOFF, MAX_INDEX_KEY_BYTES,
    MAX_INDEX_PAGE, MAX_LEASE_HOLDS, MAX_MANIFEST_BYTES, MAX_MEMBER_BINDINGS, MAX_OPEN_WAIT,
    MAX_PAGE_CACHE_BYTES, MAX_PENDING_HINTS, MAX_READERS, MAX_ROW_BATCH_BYTES, MAX_ROW_BATCH_ROWS,
    MAX_SCRATCH_BYTES, MAX_SNAPSHOT_CANDIDATES, MAX_SNAPSHOT_MEMBERS, MAX_SUPPLEMENTAL_BYTES,
    MAX_WAIT, MIN_BUDGET_TO_WAL_RATIO, MIN_CONNECTION_PAGE_CACHE_BYTES, MIN_HEALTH_BACKOFF,
    MIN_SQLITE_VERSION_NUMBER, MIN_WAL_THRESHOLD_BYTES, Maintenance, MaintenanceLimits,
    MaintenanceLimitsError, Malformed, MemberBatch, MemberRecordHeader, MemberRecordReader, Miss,
    ObservationBatch, Phase, PolicyError, PolicyState, Pressure, Published, PublishedEvidence,
    PublishedRecord, PublishedSet, ROW_CHECKSUM_CONTEXT, ROW_OVERHEAD_BYTES, Reclaimed,
    RetireReport, RetireStep, RootMismatch, RowBatch, SQLITE_LENGTH_LIMIT, Scope, SealRefusal,
    SnapshotBuilder, SnapshotHeader, SnapshotId, SnapshotMember, SnapshotReader, SnapshotSeal,
    StepBudget, StepReport, Store, StoreLimits, StoreLocation, StorePolicy, StoredRow, TooLarge,
    Unhealthy, Usage, default_directory,
};
pub use store::{LOCK_WAIT_THRESHOLD, LockWaits, lock_waits};

#[cfg(feature = "test-support")]
pub use store::FaultPoint;
#[cfg(feature = "test-support")]
pub use store::INDEX_QUERY_FOR_TEST;
#[cfg(feature = "test-support")]
pub use store::store_operations_for_test;
#[cfg(feature = "test-support")]
pub use store::{HeldConnection, MaintenanceLockHold, ScratchHold};
#[cfg(feature = "test-support")]
pub use store::{IMPORTERS_QUERY_FOR_TEST, MEMBER_QUERY_FOR_TEST};
