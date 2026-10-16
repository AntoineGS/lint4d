//! Canonical durable keys for the persistent analysis cache.
//!
//! Inputs are encoded with canonical encoding v1 (see [`Encoder`]) into
//! manifests that are retained and compared exactly; BLAKE3 digests of those
//! manifests only narrow a lookup.

mod digest;
mod encoding;
mod path;

pub use digest::{Digest, MANIFEST_DIGEST_CONTEXT, SOURCE_DIGEST_CONTEXT};
pub use encoding::{CanonicalEncode, ENCODING_VERSION, Encoder};
