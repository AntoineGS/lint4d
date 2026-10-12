//! Per-routine control-flow digests for coding agents (l4d-7mw.1).

pub mod model;
pub mod render;

pub use model::*;

/// 1-based line lookup over source bytes. CRLF and LF both end at `\n`.
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(source: &[u8]) -> Self {
        let mut starts = vec![0];
        starts.extend(
            source
                .iter()
                .enumerate()
                .filter(|(_, byte)| **byte == b'\n')
                .map(|(index, _)| index + 1),
        );
        Self { starts }
    }

    pub fn line(&self, byte: usize) -> usize {
        match self.starts.binary_search(&byte) {
            Ok(index) => index + 1,
            Err(index) => index,
        }
    }
}

/// Collapse every whitespace run (including CR and LF) to one space.
pub fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cut `text` to `max` characters followed by `…`.
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_index_is_one_based_for_lf_and_crlf() {
        let lf = LineIndex::new(b"a\nbc\nd");
        assert_eq!(
            (lf.line(0), lf.line(2), lf.line(3), lf.line(5)),
            (1, 2, 2, 3)
        );
        let crlf = LineIndex::new(b"a\r\nbc\r\nd");
        assert_eq!((crlf.line(0), crlf.line(3), crlf.line(7)), (1, 2, 3));
    }

    #[test]
    fn truncate_counts_characters() {
        assert_eq!(truncate("été", 3), "été");
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(normalize(" a\r\n  b\t c "), "a b c");
    }
}
