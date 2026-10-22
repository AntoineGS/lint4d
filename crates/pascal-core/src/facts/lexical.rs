//! The lexical identifier scan the workspace candidate prefilter uses.

use std::ops::Range;

/// An identifier byte of the lexical scan: ASCII letters, digits and `_`.
/// Every other byte, including all non-ASCII bytes, separates runs.
pub fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The maximal runs of identifier bytes in `bytes`, in order.
pub fn identifier_runs(bytes: &[u8]) -> IdentifierRuns<'_> {
    IdentifierRuns { bytes, at: 0 }
}

#[derive(Debug, Clone)]
pub struct IdentifierRuns<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Iterator for IdentifierRuns<'_> {
    type Item = Range<usize>;

    fn next(&mut self) -> Option<Range<usize>> {
        while self.at < self.bytes.len() && !is_identifier_byte(self.bytes[self.at]) {
            self.at += 1;
        }
        if self.at == self.bytes.len() {
            return None;
        }
        let start = self.at;
        while self.at < self.bytes.len() && is_identifier_byte(self.bytes[self.at]) {
            self.at += 1;
        }
        Some(start..self.at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_split_on_every_non_identifier_byte() {
        let runs: Vec<_> = identifier_runs(b"a_1 +Foo(\xe9bar)\xc3\xa9x 12ab").collect();
        assert_eq!(runs, vec![0..3, 5..8, 10..13, 16..17, 18..22]);
    }

    #[test]
    fn separators_alone_have_no_runs() {
        assert_eq!(identifier_runs(b"").count(), 0);
        assert_eq!(identifier_runs(b" .;{}$&'\r\n").count(), 0);
        assert!(is_identifier_byte(b'_') && is_identifier_byte(b'7') && !is_identifier_byte(0xe9));
    }
}
