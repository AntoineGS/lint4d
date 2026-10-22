use crate::manifest::manifest_encoder;
use crate::{CanonicalKey, Digest, KeyKind};
use std::sync::OnceLock;

/// The build fingerprint of this binary (spec §4, 2026-10-15 amendment).
pub const BUILD_FINGERPRINT: Digest =
    Digest::from_bytes(include!(concat!(env!("OUT_DIR"), "/build_fingerprint.rs")));

/// The scope that every key is published and admitted in. Its identity is the
/// build fingerprint, so any change to the analysis inputs of the build
/// selects a new namespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Namespace {
    key: CanonicalKey,
    build_fingerprint: Digest,
}

impl Namespace {
    pub fn new(build_fingerprint: Digest) -> Self {
        let mut encoder = manifest_encoder(KeyKind::Namespace);
        encoder.field(1);
        encoder.digest(&build_fingerprint);
        Self {
            key: CanonicalKey::finish(KeyKind::Namespace, encoder),
            build_fingerprint,
        }
    }

    pub fn current() -> &'static Self {
        static CURRENT: OnceLock<Namespace> = OnceLock::new();
        CURRENT.get_or_init(|| Self::new(BUILD_FINGERPRINT))
    }

    pub fn id(&self) -> Digest {
        self.key.digest()
    }

    pub fn manifest(&self) -> &[u8] {
        self.key.manifest()
    }

    pub fn build_fingerprint(&self) -> Digest {
        self.build_fingerprint
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MANIFEST_DIGEST_CONTEXT;

    #[test]
    fn namespace_manifest_is_the_build_fingerprint() {
        let namespace = Namespace::new(Digest::from_bytes([0x11; 32]));
        let mut expected = vec![b'L', b'4', b'D', b'C', 1, 0, 0, 0, 1, 1, 0];
        expected.extend([0x11; 32]);
        assert_eq!(namespace.manifest(), expected);
        assert_eq!(
            *namespace.id().as_bytes(),
            blake3::derive_key(MANIFEST_DIGEST_CONTEXT, &expected)
        );
        assert_eq!(
            namespace.build_fingerprint(),
            Digest::from_bytes([0x11; 32])
        );
        assert_ne!(
            namespace.id(),
            Namespace::new(Digest::from_bytes([0x12; 32])).id()
        );
    }

    #[test]
    fn current_namespace_uses_this_build() {
        assert_eq!(Namespace::current().build_fingerprint(), BUILD_FINGERPRINT);
        assert!(std::ptr::eq(Namespace::current(), Namespace::current()));
    }

    #[test]
    fn build_fingerprint_matches_the_workspace_it_was_built_from() {
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let digest = crate::build_fingerprint::fingerprint(
            &workspace,
            env!("PASCAL_CACHE_RUSTC_VERSION"),
            env!("PASCAL_CACHE_TARGET"),
        )
        .unwrap();
        assert_eq!(Digest::from_bytes(digest), BUILD_FINGERPRINT);
    }
}
