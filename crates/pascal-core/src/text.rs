//! Encoding-tolerant text decoding for legacy Pascal sources.
//!
//! Delphi codebases often predate UTF-8 and are stored in Windows-1252 or
//! ISO-8859-1 (Latin-1), especially when they contain comments/strings with
//! accented characters (French, German, Spanish, etc.). Any tool that calls
//! [`std::str::from_utf8`] directly on such a source will get back `Err` and
//! typically silently degrades (returning `""`, dropping comments, disabling
//! blank-line preservation, ignoring `{$FMT.OFF}` directives, …).
//!
//! This module provides:
//! - [`decode_bytes`]: a lossless decoder that is fast for UTF-8 and falls
//!   back to byte-wise Latin-1 for any other input.
//! - [`SourceEncoding`] / [`detect_encoding`] / [`encode_as`]: helpers for
//!   round-tripping legacy files so a formatter can read them, process them
//!   as UTF-8 internally, and write them back in the **original** on-disk
//!   encoding — no silent encoding upgrades, no churn in version control.

use std::borrow::Cow;

/// Byte-order mark for UTF-8 (`EF BB BF`).
pub const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Decode a byte slice as text, tolerating legacy 8-bit encodings.
///
/// Behaviour:
/// - If `bytes` is valid UTF-8, returns a `Cow::Borrowed` view — zero-copy.
/// - Otherwise, decodes each byte as a Latin-1 / ISO-8859-1 codepoint
///   (byte value → Unicode scalar value with the same numeric value).
///
/// Why Latin-1 fallback: every byte 0x00..=0xFF maps injectively to a valid
/// Unicode scalar, so the decode is lossless for any input. For Windows-1252
/// sources this preserves the characters in 0x00..=0x7F and 0xA0..=0xFF
/// correctly; the eight rare characters in 0x80..=0x9F (€, ‚, ƒ, „, …, etc.)
/// will render as C1 control codes rather than their Windows-1252 glyphs,
/// which we accept as a trade-off against pulling in a full encoding library.
///
/// This matches the philosophy of existing defensive call sites that use
/// `std::str::from_utf8(...).unwrap_or("")` but is lossless instead of
/// silently data-destructive.
pub fn decode_bytes(bytes: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Cow::Borrowed(s),
        Err(_) => Cow::Owned(bytes.iter().map(|&b| b as char).collect()),
    }
}

/// On-disk encoding of a source file, as detected by [`detect_encoding`].
///
/// Only three variants because that's all that matters for preservation:
/// - [`SourceEncoding::Utf8Bom`] — UTF-8 with a BOM prefix
/// - [`SourceEncoding::Utf8`] — UTF-8 without a BOM
/// - [`SourceEncoding::Latin1`] — any other 8-bit encoding (ISO-8859-1,
///   Windows-1252, …). See [`encode_as`] for the round-trip guarantees this
///   gives for legacy Delphi files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceEncoding {
    /// UTF-8 with a leading `EF BB BF` byte-order mark.
    Utf8Bom,
    /// UTF-8 without a byte-order mark.
    Utf8,
    /// Non-UTF-8 8-bit encoding — decoded/encoded via a 1:1 byte-to-codepoint
    /// mapping (ISO-8859-1 semantics). This also round-trips Windows-1252
    /// bytes losslessly because every byte 0x00..=0xFF maps to a distinct
    /// Unicode codepoint, though characters in 0x80..=0x9F will land on C1
    /// control codepoints rather than their Windows-1252 glyphs — acceptable
    /// for a formatter that treats text as opaque.
    Latin1,
}

/// Classify `source` by its byte pattern.
///
/// Used by tools that need to preserve the original encoding when writing
/// formatted output back to disk. A file that decodes cleanly as UTF-8 is
/// reported as UTF-8 (with or without BOM); anything else falls back to
/// [`SourceEncoding::Latin1`].
pub fn detect_encoding(source: &[u8]) -> SourceEncoding {
    if source.starts_with(UTF8_BOM) {
        // BOM-prefixed: check the body for UTF-8 validity. If the body is
        // not valid UTF-8, the file is malformed — treat it as Latin-1 and
        // preserve bytes rather than corrupting them.
        if std::str::from_utf8(&source[UTF8_BOM.len()..]).is_ok() {
            return SourceEncoding::Utf8Bom;
        }
        return SourceEncoding::Latin1;
    }
    if std::str::from_utf8(source).is_ok() {
        SourceEncoding::Utf8
    } else {
        SourceEncoding::Latin1
    }
}

/// Re-encode a UTF-8 `text` to bytes in the requested `encoding`.
///
/// Round-trip contract when paired with [`decode_bytes`]:
/// - UTF-8 / UTF-8+BOM input → UTF-8 text → UTF-8 / UTF-8+BOM bytes ✓
/// - Latin-1 input (every byte 0x00..=0xFF) → UTF-8 string with codepoints
///   U+0000..=U+00FF → original bytes ✓
/// - Windows-1252 input where all bytes are in 0x00..=0x7F ∪ 0xA0..=0xFF →
///   identical round-trip
/// - Windows-1252 bytes in 0x80..=0x9F decoded as Latin-1 land on C1 control
///   codepoints; encoding them back produces the same bytes ✓
///
/// Characters outside U+0000..=U+00FF that can't fit in an 8-bit target are
/// replaced with `?` to keep the output valid. This should not happen for a
/// formatter that only preserves user text, but protects us if it ever does.
pub fn encode_as(text: &str, encoding: SourceEncoding) -> Vec<u8> {
    match encoding {
        SourceEncoding::Utf8 => text.as_bytes().to_vec(),
        SourceEncoding::Utf8Bom => {
            // If `text` already starts with U+FEFF (the BOM as a character),
            // encode it verbatim — otherwise prepend the raw UTF-8 BOM.
            let mut out = Vec::with_capacity(UTF8_BOM.len() + text.len());
            if !text.starts_with('\u{FEFF}') {
                out.extend_from_slice(UTF8_BOM);
            }
            out.extend_from_slice(text.as_bytes());
            out
        }
        SourceEncoding::Latin1 => {
            let mut out = Vec::with_capacity(text.len());
            for c in text.chars() {
                let cp = c as u32;
                if cp <= 0xFF {
                    out.push(cp as u8);
                } else {
                    // Character can't be represented in a single byte.
                    // Fall back to '?' — data loss, but only for characters
                    // the formatter could only have introduced on its own
                    // (it faithfully preserves user text).
                    out.push(b'?');
                }
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_borrowed() {
        let bytes = b"hello world";
        let decoded = decode_bytes(bytes);
        assert_eq!(decoded, "hello world");
        assert!(matches!(decoded, Cow::Borrowed(_)));
    }

    #[test]
    fn valid_utf8_with_multibyte_is_borrowed() {
        // "café" as UTF-8: 0x63 0x61 0x66 0xC3 0xA9
        let bytes = b"caf\xC3\xA9";
        let decoded = decode_bytes(bytes);
        assert_eq!(decoded, "café");
        assert!(matches!(decoded, Cow::Borrowed(_)));
    }

    #[test]
    fn latin1_is_decoded_losslessly() {
        // "café" as Latin-1: 0x63 0x61 0x66 0xE9  (single byte for é)
        let bytes = b"caf\xE9";
        let decoded = decode_bytes(bytes);
        assert_eq!(decoded, "café");
        assert!(matches!(decoded, Cow::Owned(_)));
    }

    #[test]
    fn latin1_accented_comment() {
        // `// Vérifier` in Latin-1: `// V` + 0xE9 + `rifier`
        let bytes = b"// V\xE9rifier";
        let decoded = decode_bytes(bytes);
        assert_eq!(decoded, "// Vérifier");
    }

    #[test]
    fn isolated_high_byte_is_not_lost() {
        // A single 0xE9 byte is invalid UTF-8 but decodes to U+00E9 (é).
        let bytes = &[0xE9u8];
        let decoded = decode_bytes(bytes);
        assert_eq!(decoded, "é");
        assert_eq!(decoded.chars().count(), 1);
    }

    #[test]
    fn empty_is_borrowed() {
        let decoded = decode_bytes(b"");
        assert_eq!(decoded, "");
        assert!(matches!(decoded, Cow::Borrowed(_)));
    }

    #[test]
    fn all_high_bytes_roundtrip_to_same_codepoints() {
        // Every byte 0x80..=0xFF is invalid as a lone UTF-8 byte, but
        // Latin-1 fallback maps each to the same numeric codepoint.
        let bytes: Vec<u8> = (0x80u8..=0xFFu8).collect();
        let decoded = decode_bytes(&bytes);
        let chars: Vec<char> = decoded.chars().collect();
        assert_eq!(chars.len(), bytes.len());
        for (i, &b) in bytes.iter().enumerate() {
            assert_eq!(chars[i] as u32, b as u32);
        }
    }

    // ── Encoding detection ───────────────────────────────────────────

    #[test]
    fn detect_ascii_as_utf8() {
        assert_eq!(detect_encoding(b"plain ascii"), SourceEncoding::Utf8);
    }

    #[test]
    fn detect_valid_utf8_multibyte_as_utf8() {
        // "café" as UTF-8.
        assert_eq!(detect_encoding(b"caf\xC3\xA9"), SourceEncoding::Utf8);
    }

    #[test]
    fn detect_utf8_bom() {
        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend_from_slice(b"hello");
        assert_eq!(detect_encoding(&bytes), SourceEncoding::Utf8Bom);
    }

    #[test]
    fn detect_latin1_from_lone_high_byte() {
        // 0xE9 is not valid as a lone UTF-8 continuation byte.
        assert_eq!(detect_encoding(b"caf\xE9"), SourceEncoding::Latin1);
    }

    #[test]
    fn detect_bom_prefixed_but_invalid_body_falls_back_to_latin1() {
        // File "claims" UTF-8 via BOM but the body isn't valid UTF-8 —
        // treat the whole thing as Latin-1 so bytes are preserved.
        let mut bytes = UTF8_BOM.to_vec();
        bytes.push(0xE9);
        assert_eq!(detect_encoding(&bytes), SourceEncoding::Latin1);
    }

    #[test]
    fn detect_empty_as_utf8() {
        assert_eq!(detect_encoding(b""), SourceEncoding::Utf8);
    }

    // ── encode_as round-trip ─────────────────────────────────────────

    #[test]
    fn encode_utf8_ascii_roundtrip() {
        let src = b"hello, world";
        let decoded = decode_bytes(src);
        let encoded = encode_as(&decoded, SourceEncoding::Utf8);
        assert_eq!(encoded, src);
    }

    #[test]
    fn encode_utf8_multibyte_roundtrip() {
        let src = "café naïve".as_bytes();
        let decoded = decode_bytes(src);
        let encoded = encode_as(&decoded, SourceEncoding::Utf8);
        assert_eq!(encoded, src);
    }

    #[test]
    fn encode_utf8_bom_prepends_bom_when_missing() {
        let decoded = "hi";
        let encoded = encode_as(decoded, SourceEncoding::Utf8Bom);
        assert!(encoded.starts_with(UTF8_BOM));
        assert_eq!(&encoded[UTF8_BOM.len()..], b"hi");
    }

    #[test]
    fn encode_utf8_bom_does_not_double_bom() {
        let decoded = "\u{FEFF}hi";
        let encoded = encode_as(decoded, SourceEncoding::Utf8Bom);
        // Exactly one BOM.
        assert_eq!(&encoded[..UTF8_BOM.len()], UTF8_BOM);
        assert_eq!(&encoded[UTF8_BOM.len()..], b"hi");
    }

    #[test]
    fn encode_latin1_roundtrip_ascii() {
        let src: &[u8] = b"hello";
        let decoded = decode_bytes(src);
        let encoded = encode_as(&decoded, SourceEncoding::Latin1);
        assert_eq!(encoded, src);
    }

    #[test]
    fn encode_latin1_roundtrip_accented() {
        // "café" in Latin-1: c a f 0xE9
        let src: &[u8] = b"caf\xE9";
        let decoded = decode_bytes(src);
        let encoded = encode_as(&decoded, SourceEncoding::Latin1);
        assert_eq!(encoded, src);
    }

    #[test]
    fn encode_latin1_roundtrip_full_high_byte_range() {
        // Every byte 0x00..=0xFF must round-trip through Latin-1.
        let src: Vec<u8> = (0u8..=255u8).collect();
        let decoded = decode_bytes(&src);
        let encoded = encode_as(&decoded, SourceEncoding::Latin1);
        assert_eq!(encoded, src);
    }

    #[test]
    fn encode_latin1_replaces_non_representable() {
        // A character outside U+0000..=U+00FF can't fit in one Latin-1 byte.
        let text = "A \u{2603} B"; // snowman
        let encoded = encode_as(text, SourceEncoding::Latin1);
        assert_eq!(encoded, b"A ? B");
    }

    #[test]
    fn encode_latin1_roundtrip_windows1252_quote() {
        // Windows-1252 byte 0x92 (right single quotation mark `'`) is
        // decoded as U+0092 (C1 control) by decode_bytes. The roundtrip
        // still preserves the exact byte — that's what legacy Delphi
        // files need.
        let src: &[u8] = b"it\x92s";
        let decoded = decode_bytes(src);
        let encoded = encode_as(&decoded, SourceEncoding::Latin1);
        assert_eq!(encoded, src);
    }
}
