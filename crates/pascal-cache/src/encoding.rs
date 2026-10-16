//! Canonical encoding v1.
//!
//! - Integers are fixed-width little-endian; `bool` is one byte, 0 or 1.
//! - Lengths and counts are `u64`; byte strings and UTF-8 text are a length
//!   followed by the raw bytes, with no Unicode normalization.
//! - Struct fields are each preceded by a `u16` tag, numbered from 1 in
//!   declaration order. Enum variants are a `u8` tag, numbered from 1,
//!   followed by their fields.
//! - `Option` is 0 for `None`, or 1 followed by the value.
//! - Sequences are a count followed by the items in their semantic order.
//! - Maps and sets are a count followed by entries sorted by the bytes of
//!   their encoded keys.
//! - Paths are a native tag (1 Unix bytes, 2 Windows UTF-16 code units)
//!   followed by the raw units; spelling is never normalized.

use crate::Digest;

pub const ENCODING_VERSION: u32 = 1;

pub trait CanonicalEncode {
    fn encode(&self, encoder: &mut Encoder);
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub fn bool(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    pub fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub fn length(&mut self, length: usize) {
        self.u64(u64::try_from(length).expect("lengths fit in u64"));
    }

    pub fn bytes(&mut self, value: &[u8]) {
        self.length(value.len());
        self.bytes.extend_from_slice(value);
    }

    pub fn str(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    pub fn digest(&mut self, value: &Digest) {
        self.bytes.extend_from_slice(value.as_bytes());
    }

    pub fn field(&mut self, tag: u16) {
        self.u16(tag);
    }

    pub fn variant(&mut self, tag: u8) {
        self.u8(tag);
    }

    pub fn option<T: ?Sized>(&mut self, value: Option<&T>, encode: impl FnOnce(&mut Self, &T)) {
        match value {
            None => self.u8(0),
            Some(value) => {
                self.u8(1);
                encode(self, value);
            }
        }
    }

    pub fn seq<I>(&mut self, items: I, mut encode: impl FnMut(&mut Self, I::Item))
    where
        I: IntoIterator,
        I::IntoIter: ExactSizeIterator,
    {
        let items = items.into_iter();
        self.length(items.len());
        for item in items {
            encode(self, item);
        }
    }

    pub fn map<I, K, V>(
        &mut self,
        entries: I,
        mut key: impl FnMut(&mut Self, K),
        mut value: impl FnMut(&mut Self, V),
    ) where
        I: IntoIterator<Item = (K, V)>,
    {
        let mut encoded = entries
            .into_iter()
            .map(|(entry_key, entry_value)| {
                let mut key_encoder = Self::new();
                key(&mut key_encoder, entry_key);
                let mut value_encoder = Self::new();
                value(&mut value_encoder, entry_value);
                (key_encoder.bytes, value_encoder.bytes)
            })
            .collect::<Vec<_>>();
        encoded.sort();
        debug_assert!(
            encoded.windows(2).all(|pair| pair[0].0 != pair[1].0),
            "map keys must encode distinctly"
        );
        self.length(encoded.len());
        for (key_bytes, value_bytes) in encoded {
            self.bytes.extend_from_slice(&key_bytes);
            self.bytes.extend_from_slice(&value_bytes);
        }
    }

    pub fn encode<T: CanonicalEncode + ?Sized>(&mut self, value: &T) {
        value.encode(self);
    }
}

impl CanonicalEncode for Digest {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.digest(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives_use_fixed_little_endian_and_length_prefixes() {
        let mut encoder = Encoder::new();
        encoder.u8(0xab);
        encoder.bool(true);
        encoder.u16(0x0102);
        encoder.u32(7);
        encoder.u64(0x0102_0304_0506_0708);
        encoder.i64(-2);
        encoder.str("ab");
        assert_eq!(
            encoder.as_bytes(),
            [
                0xab, 1, 2, 1, 7, 0, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1, 0xfe, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 2, 0, 0, 0, 0, 0, 0, 0, b'a', b'b',
            ]
        );
    }

    #[test]
    fn options_are_explicit() {
        let mut none = Encoder::new();
        none.option(None::<&u8>, |encoder, value| encoder.u8(*value));
        let mut some = Encoder::new();
        some.option(Some(&5_u8), |encoder, value| encoder.u8(*value));
        assert_eq!(none.as_bytes(), [0]);
        assert_eq!(some.as_bytes(), [1, 5]);
    }

    #[test]
    fn sequences_keep_their_order() {
        let encode = |items: [u8; 3]| {
            let mut encoder = Encoder::new();
            encoder.seq(items, |encoder, item| encoder.u8(item));
            encoder.into_bytes()
        };
        assert_eq!(encode([3, 1, 2]), [3, 0, 0, 0, 0, 0, 0, 0, 3, 1, 2]);
        assert_ne!(encode([3, 1, 2]), encode([2, 1, 3]));
    }

    #[test]
    fn maps_are_ordered_by_encoded_key_bytes() {
        let encode = |entries: Vec<(&str, u8)>| {
            let mut encoder = Encoder::new();
            encoder.map(
                entries,
                |encoder, key| encoder.str(key),
                |encoder, value| encoder.u8(value),
            );
            encoder.into_bytes()
        };
        let forward = encode(vec![("b", 2), ("aa", 1), ("a", 3)]);
        let backward = encode(vec![("a", 3), ("aa", 1), ("b", 2)]);
        assert_eq!(forward, backward);
        // The length prefix sorts first: "a" < "b" < "aa".
        assert_eq!(
            forward,
            [
                3, 0, 0, 0, 0, 0, 0, 0, //
                1, 0, 0, 0, 0, 0, 0, 0, b'a', 3, //
                1, 0, 0, 0, 0, 0, 0, 0, b'b', 2, //
                2, 0, 0, 0, 0, 0, 0, 0, b'a', b'a', 1,
            ]
        );
    }

    #[test]
    fn field_and_variant_tags_are_written_verbatim() {
        let mut encoder = Encoder::new();
        encoder.field(0x0201);
        encoder.variant(3);
        encoder.digest(&Digest::from_bytes([9; 32]));
        let mut expected = vec![1, 2, 3];
        expected.extend([9; 32]);
        assert_eq!(encoder.as_bytes(), expected);
    }
}
