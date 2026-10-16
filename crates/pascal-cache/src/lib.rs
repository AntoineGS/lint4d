//! Canonical durable keys for the persistent analysis cache.
//!
//! Inputs are encoded with canonical encoding v1 (see [`Encoder`]) into
//! manifests that are retained and compared exactly; BLAKE3 digests of those
//! manifests only narrow a lookup.

mod artifact_key;
mod context_key;
mod digest;
mod encoding;
mod member_key;
mod path;
mod project;
mod source;

pub use artifact_key::{
    ExpandedArtifactKey, IncludeOccurrence, IncludeOutcome, KeyError, LocalArtifactKey,
};
pub use context_key::ContextKey;
pub use digest::{Digest, MANIFEST_DIGEST_CONTEXT, SOURCE_DIGEST_CONTEXT};
pub use encoding::{CanonicalEncode, ENCODING_VERSION, Encoder};
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
