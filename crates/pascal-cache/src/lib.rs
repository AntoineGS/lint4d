//! Canonical durable keys for the persistent analysis cache.
//!
//! Inputs are encoded with canonical encoding v1 (see [`Encoder`]) into
//! manifests that are retained and compared exactly; BLAKE3 digests of those
//! manifests only narrow a lookup.

mod context_key;
mod digest;
mod encoding;
mod path;
mod project;

pub use context_key::ContextKey;
pub use digest::{Digest, MANIFEST_DIGEST_CONTEXT, SOURCE_DIGEST_CONTEXT};
pub use encoding::{CanonicalEncode, ENCODING_VERSION, Encoder};

#[cfg(test)]
mod build_fingerprint;
mod manifest;
mod namespace;

pub use manifest::{
    CanonicalKey, KeyKind, KeyRecord, MANIFEST_HEADER_LEN, MANIFEST_MAGIC, ManifestError,
    ManifestHeader, Mismatch,
};
pub use namespace::{BUILD_FINGERPRINT, Namespace};
