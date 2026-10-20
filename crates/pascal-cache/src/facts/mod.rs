//! Row encoding of [`ArtifactFacts`] for `artifact_rows` (spec §5, §6.1
//! amendment 2026-10-16). Names are index keys compared bytewise; every
//! other field is a positional payload. Decoding validates every DTO
//! invariant and charges each fact before it is retained.

mod decode;
mod encode;

pub use decode::{FactDecodeError, FactDecoder};
pub use encode::{EncodedFacts, EncodedRow, encode_facts, encoded_content_bytes};

pub const DECLARATION_ROW: u8 = 1;
pub const IMPORT_ROW: u8 = 2;
pub const POSTING_ROW: u8 = 3;
pub const INCLUDE_ROW: u8 = 4;
pub const SEGMENT_ROW: u8 = 5;

/// Bytes of one posting occurrence: slot, start, end and activity.
const OCCURRENCE_BYTES: usize = 13;
/// Bytes of a `u64` length prefix.
const LENGTH_BYTES: usize = 8;
