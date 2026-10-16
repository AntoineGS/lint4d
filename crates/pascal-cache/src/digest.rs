use std::fmt::{self, Write};

/// Domain of manifest digests (see `CanonicalKey`).
pub const MANIFEST_DIGEST_CONTEXT: &str =
    "lint4d persistent analysis cache 2026-10 key manifest v1";
/// Domain of source payload digests (see `SourceContent`).
pub const SOURCE_DIGEST_CONTEXT: &str = "lint4d persistent analysis cache 2026-10 source bytes v1";

/// A BLAKE3 256-bit digest.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest([u8; 32]);

impl Digest {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        let mut text = String::with_capacity(64);
        for byte in self.0 {
            let _ = write!(text, "{byte:02x}");
        }
        text
    }

    pub fn from_hex(text: &str) -> Option<Self> {
        if text.len() != 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let mut bytes = [0; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
        }
        Some(Self(bytes))
    }

    // Unused outside tests until the key manifest digest is added; remove then.
    #[allow(dead_code)]
    pub(crate) fn derive(context: &'static str, material: &[u8]) -> Self {
        Self(blake3::derive_key(context, material))
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Digest({})", self.to_hex())
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_rejects_malformed_text() {
        let digest = Digest::from_bytes(std::array::from_fn(|index| index as u8));
        let hex = digest.to_hex();
        assert_eq!(
            hex,
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
        );
        assert_eq!(Digest::from_hex(&hex), Some(digest));
        assert_eq!(format!("{digest}"), hex);
        assert_eq!(format!("{digest:?}"), format!("Digest({hex})"));
        assert_eq!(Digest::from_hex(&hex[..62]), None);
        assert_eq!(Digest::from_hex(&format!("{}zz", &hex[..62])), None);
        assert_eq!(Digest::from_hex(&format!("+f{}", &hex[2..])), None);
    }

    #[test]
    fn derivation_is_blake3_derive_key_and_domain_separated() {
        let manifest = Digest::derive(MANIFEST_DIGEST_CONTEXT, b"payload");
        let source = Digest::derive(SOURCE_DIGEST_CONTEXT, b"payload");
        assert_eq!(
            *manifest.as_bytes(),
            blake3::derive_key(MANIFEST_DIGEST_CONTEXT, b"payload")
        );
        assert_ne!(manifest, source);
    }
}
