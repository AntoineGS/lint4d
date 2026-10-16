use crate::{Digest, ENCODING_VERSION, Encoder, MANIFEST_DIGEST_CONTEXT, Namespace};
use std::sync::Arc;

pub const MANIFEST_MAGIC: [u8; 4] = *b"L4DC";
pub const MANIFEST_HEADER_LEN: usize = 9;

/// What a manifest describes. The tag is the ninth header byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyKind {
    Namespace,
    LocalArtifact,
    ExpandedArtifact,
    Context,
    Member,
}

impl KeyKind {
    pub const fn tag(self) -> u8 {
        match self {
            Self::Namespace => 1,
            Self::LocalArtifact => 2,
            Self::ExpandedArtifact => 3,
            Self::Context => 4,
            Self::Member => 5,
        }
    }

    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::Namespace),
            2 => Some(Self::LocalArtifact),
            3 => Some(Self::ExpandedArtifact),
            4 => Some(Self::Context),
            5 => Some(Self::Member),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestHeader {
    pub encoding_version: u32,
    pub kind: KeyKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestError {
    Truncated,
    BadMagic,
    UnsupportedEncoding(u32),
    UnknownKind(u8),
}

impl ManifestHeader {
    pub fn parse(manifest: &[u8]) -> Result<Self, ManifestError> {
        let header = manifest
            .get(..MANIFEST_HEADER_LEN)
            .ok_or(ManifestError::Truncated)?;
        if header[..4] != MANIFEST_MAGIC {
            return Err(ManifestError::BadMagic);
        }
        let encoding_version = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        if encoding_version != ENCODING_VERSION {
            return Err(ManifestError::UnsupportedEncoding(encoding_version));
        }
        let kind = KeyKind::from_tag(header[8]).ok_or(ManifestError::UnknownKind(header[8]))?;
        Ok(Self {
            encoding_version,
            kind,
        })
    }
}

pub(crate) fn manifest_encoder(kind: KeyKind) -> Encoder {
    let mut encoder = Encoder::new();
    for byte in MANIFEST_MAGIC {
        encoder.u8(byte);
    }
    encoder.u32(ENCODING_VERSION);
    encoder.u8(kind.tag());
    encoder
}

/// A retained canonical manifest and its digest. The digest narrows a
/// lookup; only [`CanonicalKey::admit`] establishes that a stored record is
/// this key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanonicalKey {
    kind: KeyKind,
    manifest: Arc<[u8]>,
    digest: Digest,
}

/// A key as stored: the namespace it was published in, its digest and its
/// manifest bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyRecord<'a> {
    pub namespace: Digest,
    pub digest: Digest,
    pub manifest: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mismatch {
    NamespaceDiffers,
    Malformed(ManifestError),
    KindDiffers { expected: KeyKind, found: KeyKind },
    DigestDiffers,
    ManifestDiffers,
}

impl CanonicalKey {
    pub(crate) fn finish(kind: KeyKind, encoder: Encoder) -> Self {
        let manifest: Arc<[u8]> = encoder.into_bytes().into();
        debug_assert_eq!(
            ManifestHeader::parse(&manifest).map(|header| header.kind),
            Ok(kind)
        );
        let digest = Digest::derive(MANIFEST_DIGEST_CONTEXT, &manifest);
        Self {
            kind,
            manifest,
            digest,
        }
    }

    pub fn kind(&self) -> KeyKind {
        self.kind
    }

    pub fn digest(&self) -> Digest {
        self.digest
    }

    pub fn manifest(&self) -> &[u8] {
        &self.manifest
    }

    /// Accept `record` only if it was published in `namespace` and carries
    /// exactly this key's manifest.
    pub fn admit(&self, namespace: &Namespace, record: &KeyRecord<'_>) -> Result<(), Mismatch> {
        if record.namespace != namespace.id() {
            return Err(Mismatch::NamespaceDiffers);
        }
        let header = ManifestHeader::parse(record.manifest).map_err(Mismatch::Malformed)?;
        if header.kind != self.kind {
            return Err(Mismatch::KindDiffers {
                expected: self.kind,
                found: header.kind,
            });
        }
        if record.digest != self.digest {
            return Err(Mismatch::DigestDiffers);
        }
        if *record.manifest != *self.manifest {
            return Err(Mismatch::ManifestDiffers);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context_key(text: &str) -> CanonicalKey {
        let mut encoder = manifest_encoder(KeyKind::Context);
        encoder.field(1);
        encoder.str(text);
        CanonicalKey::finish(KeyKind::Context, encoder)
    }

    #[test]
    fn headers_name_the_encoding_and_kind() {
        let key = context_key("x");
        assert_eq!(
            &key.manifest()[..MANIFEST_HEADER_LEN],
            [b'L', b'4', b'D', b'C', 1, 0, 0, 0, 4]
        );
        assert_eq!(
            ManifestHeader::parse(key.manifest()),
            Ok(ManifestHeader {
                encoding_version: 1,
                kind: KeyKind::Context
            })
        );
        assert_eq!(
            *key.digest().as_bytes(),
            blake3::derive_key(MANIFEST_DIGEST_CONTEXT, key.manifest())
        );
    }

    #[test]
    fn malformed_headers_are_rejected() {
        assert_eq!(
            ManifestHeader::parse(b"L4DC"),
            Err(ManifestError::Truncated)
        );
        assert_eq!(
            ManifestHeader::parse(&[b'X', b'4', b'D', b'C', 1, 0, 0, 0, 4]),
            Err(ManifestError::BadMagic)
        );
        assert_eq!(
            ManifestHeader::parse(&[b'L', b'4', b'D', b'C', 2, 0, 0, 0, 4]),
            Err(ManifestError::UnsupportedEncoding(2))
        );
        assert_eq!(
            ManifestHeader::parse(&[b'L', b'4', b'D', b'C', 1, 0, 0, 0, 9]),
            Err(ManifestError::UnknownKind(9))
        );
        for kind in [
            KeyKind::Namespace,
            KeyKind::LocalArtifact,
            KeyKind::ExpandedArtifact,
            KeyKind::Context,
            KeyKind::Member,
        ] {
            assert_eq!(KeyKind::from_tag(kind.tag()), Some(kind));
        }
        assert_eq!(KeyKind::from_tag(0), None);
    }

    #[test]
    fn admission_requires_namespace_kind_digest_and_exact_manifest() {
        let namespace = Namespace::new(Digest::from_bytes([1; 32]));
        let other_namespace = Namespace::new(Digest::from_bytes([2; 32]));
        let key = context_key("x");
        let record = |namespace: &Namespace, digest: Digest, manifest: &'static [u8]| KeyRecord {
            namespace: namespace.id(),
            digest,
            manifest,
        };
        let manifest: &'static [u8] = key.manifest().to_vec().leak();

        assert_eq!(
            key.admit(&namespace, &record(&namespace, key.digest(), manifest)),
            Ok(())
        );
        assert_eq!(
            key.admit(
                &namespace,
                &record(&other_namespace, key.digest(), manifest)
            ),
            Err(Mismatch::NamespaceDiffers)
        );

        let mut newer = manifest.to_vec();
        newer[4] = 2;
        assert_eq!(
            key.admit(&namespace, &record(&namespace, key.digest(), newer.leak())),
            Err(Mismatch::Malformed(ManifestError::UnsupportedEncoding(2)))
        );

        let mut other_kind = manifest.to_vec();
        other_kind[8] = KeyKind::Member.tag();
        assert_eq!(
            key.admit(
                &namespace,
                &record(&namespace, key.digest(), other_kind.leak())
            ),
            Err(Mismatch::KindDiffers {
                expected: KeyKind::Context,
                found: KeyKind::Member
            })
        );

        let other = context_key("y");
        let other_manifest: &'static [u8] = other.manifest().to_vec().leak();
        assert_eq!(
            key.admit(
                &namespace,
                &record(&namespace, other.digest(), other_manifest)
            ),
            Err(Mismatch::DigestDiffers)
        );
        // A stored digest that matches while the manifest does not (a collision
        // or a corrupted row) is still rejected.
        assert_eq!(
            key.admit(
                &namespace,
                &record(&namespace, key.digest(), other_manifest)
            ),
            Err(Mismatch::ManifestDiffers)
        );
    }
}
