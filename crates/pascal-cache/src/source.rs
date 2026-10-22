use crate::{CanonicalEncode, Digest, Encoder, SOURCE_DIGEST_CONTEXT};
use pascal_core::SourceEncoding;
use std::path::PathBuf;

/// The authorized original payload of a source and the decoder contract it
/// selects. Analysis ranges are in decoded UTF-8 coordinates, so the decoder
/// is part of the identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceContent {
    digest: Digest,
    length: u64,
    encoding: SourceEncoding,
}

impl SourceContent {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            digest: Digest::derive(SOURCE_DIGEST_CONTEXT, bytes),
            length: bytes.len() as u64,
            encoding: pascal_core::detect_encoding(bytes),
        }
    }

    pub fn digest(&self) -> Digest {
        self.digest
    }

    pub fn length(&self) -> u64 {
        self.length
    }

    pub fn encoding(&self) -> SourceEncoding {
        self.encoding
    }
}

impl CanonicalEncode for SourceEncoding {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.variant(match self {
            Self::Utf8Bom => 1,
            Self::Utf8 => 2,
            Self::Latin1 => 3,
        });
    }
}

impl CanonicalEncode for SourceContent {
    fn encode(&self, encoder: &mut Encoder) {
        let Self {
            digest,
            length,
            encoding,
        } = self;
        encoder.field(1);
        encoder.digest(digest);
        encoder.field(2);
        encoder.u64(*length);
        encoder.field(3);
        encoder.encode(encoding);
    }
}

/// How one source is known: its resolved identity, the path it was reached
/// through, the reference as written, and the client URI. Each is kept
/// separately and never derived from another.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceIdentity {
    pub canonical: PathBuf,
    pub logical: PathBuf,
    pub spelling: Option<String>,
    pub uri: Option<String>,
}

impl CanonicalEncode for SourceIdentity {
    fn encode(&self, encoder: &mut Encoder) {
        let Self {
            canonical,
            logical,
            spelling,
            uri,
        } = self;
        encoder.field(1);
        encoder.encode(canonical.as_path());
        encoder.field(2);
        encoder.encode(logical.as_path());
        encoder.field(3);
        encoder.option(spelling.as_deref(), |encoder, spelling| {
            encoder.str(spelling)
        });
        encoder.field(4);
        encoder.option(uri.as_deref(), |encoder, uri| encoder.str(uri));
    }
}
