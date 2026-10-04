use lsp_types::Position;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy)]
struct Line<'a> {
    source: &'a str,
    start: usize,
    end: usize,
    break_start: usize,
    break_end: usize,
}

impl<'a> Line<'a> {
    fn text(self) -> &'a str {
        &self.source[self.start..self.end]
    }

    fn utf16_len(self) -> usize {
        self.text().encode_utf16().count()
    }
}

fn lines(source: &str) -> Vec<Line<'_>> {
    let mut result = Vec::new();
    let mut start = 0;
    let bytes = source.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        let break_len = match bytes[index] {
            b'\n' => 1,
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => 2,
            b'\r' => 1,
            _ => {
                index += 1;
                continue;
            }
        };

        let break_start = index;
        let end = index;
        result.push(Line {
            source,
            start,
            end,
            break_start,
            break_end: index + break_len,
        });
        start = index + break_len;
        index += break_len;
    }

    result.push(Line {
        source,
        start,
        end: source.len(),
        break_start: source.len(),
        break_end: source.len(),
    });
    result
}

/// Convert a zero-based LSP position (UTF-16 characters) to a UTF-8 byte
/// offset. Positions in the middle of a UTF-16 surrogate pair, in a line
/// ending, or beyond the line's content are rejected.
pub fn position_to_offset(source: &str, position: Position) -> Option<usize> {
    let line_number = usize::try_from(position.line).ok()?;
    let character = usize::try_from(position.character).ok()?;
    let line = lines(source).get(line_number).copied()?;
    offset_for_utf16(line, character)
}

/// Convert a zero-based LSP range endpoint to a UTF-8 byte offset, clamping a
/// character position past the end of an existing line to that line's end.
/// Positions in the middle of a UTF-16 surrogate pair or in a missing line are
/// still rejected.
pub(crate) fn position_to_offset_clamped(source: &str, position: Position) -> Option<usize> {
    let line_number = usize::try_from(position.line).ok()?;
    let character = usize::try_from(position.character).ok()?;
    let line = lines(source).get(line_number).copied()?;
    offset_for_utf16_clamped(line, character)
}

fn offset_for_utf16(line: Line<'_>, character: usize) -> Option<usize> {
    if character > line.utf16_len() {
        return None;
    }

    let mut units = 0;
    for (relative, ch) in line.text().char_indices() {
        if units == character {
            return Some(line.start + relative);
        }
        let next = units + ch.len_utf16();
        if character < next {
            // Do not produce a byte offset between the two UTF-16 code units
            // of a supplementary scalar value.
            return None;
        }
        units = next;
    }

    (units == character).then_some(line.end)
}

fn offset_for_utf16_clamped(line: Line<'_>, character: usize) -> Option<usize> {
    if character > line.utf16_len() {
        return Some(line.end);
    }
    offset_for_utf16(line, character)
}

fn utf16_before(source: &str, offset: usize) -> usize {
    source[..offset].encode_utf16().count()
}

/// Convert a UTF-8 byte offset to a zero-based LSP position measured in
/// UTF-16 code units. Offsets in the middle of a UTF-8 scalar or CRLF pair are
/// rejected.
pub fn offset_to_position(source: &str, offset: usize) -> Option<Position> {
    #[cfg(test)]
    SOURCE_SCAN_CONVERSIONS.with(|scans| scans.set(scans.get().saturating_add(1)));
    if offset > source.len() || !source.is_char_boundary(offset) {
        return None;
    }

    for (line_number, line) in lines(source).into_iter().enumerate() {
        if line.start <= offset && offset <= line.end {
            return Some(Position {
                line: u32::try_from(line_number).ok()?,
                character: u32::try_from(utf16_before(
                    &source[line.start..line.end],
                    offset - line.start,
                ))
                .ok()?,
            });
        }
        if line.break_start < offset && offset < line.break_end {
            return None;
        }
    }

    None
}

/// Cached byte and UTF-16 offsets for one source document.
///
/// Symbol requests convert many declaration spans from one immutable source.
/// Keeping line boundaries avoids rescanning the complete prefix of the
/// source for every span. Byte and UTF-16 offsets agree on ASCII text, so
/// only lines with non-ASCII scalars keep a list of corrections.
#[derive(Debug, Clone)]
pub(crate) struct PositionIndex {
    source_len: usize,
    lines: Vec<IndexedLine>,
}

#[cfg(test)]
thread_local! {
    static POSITION_INDEX_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Conversions that rescan the source prefix through [`offset_to_position`].
    pub(crate) static SOURCE_SCAN_CONVERSIONS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[derive(Debug, Clone)]
struct IndexedLine {
    start: usize,
    end: usize,
    /// One entry per non-ASCII scalar on the line, in order. Empty for ASCII
    /// lines.
    corrections: Box<[Correction]>,
}

/// A non-ASCII scalar: where it ends, relative to the line start, and how
/// many more UTF-8 bytes than UTF-16 units the line has used up to there.
#[derive(Debug, Clone, Copy)]
struct Correction {
    byte_end: usize,
    excess: usize,
}

/// Cancellation is checked once per this many scanned bytes.
const CANCEL_CHECK_BYTES: usize = 4096;

impl PositionIndex {
    pub(crate) fn owned_bytes_upper_bound_with_cancel(
        source: &str,
        cancel: &AtomicBool,
    ) -> Result<usize, ()> {
        let mut line_count = 1usize;
        let mut non_ascii_count = 0usize;
        let bytes = source.as_bytes();
        let mut index = 0usize;
        while index < bytes.len() {
            if index.is_multiple_of(CANCEL_CHECK_BYTES) && is_cancelled(Some(cancel)) {
                return Err(());
            }
            match bytes[index] {
                b'\n' => line_count = line_count.saturating_add(1),
                b'\r' if bytes.get(index + 1) != Some(&b'\n') => {
                    line_count = line_count.saturating_add(1);
                }
                byte if !byte.is_ascii() && source.is_char_boundary(index) => {
                    non_ascii_count = non_ascii_count.saturating_add(1);
                }
                _ => {}
            }
            index += 1;
        }
        // The line vector may grow to twice its final length while building.
        Ok(line_count
            .saturating_mul(2 * std::mem::size_of::<IndexedLine>())
            .saturating_add(non_ascii_count.saturating_mul(std::mem::size_of::<Correction>()))
            .saturating_add(2 * std::mem::size_of::<usize>()))
    }

    pub(crate) fn new(source: &str) -> Self {
        #[cfg(test)]
        POSITION_INDEX_BUILDS.with(|builds| builds.set(builds.get().saturating_add(1)));
        Self::build(source, None).expect("uncancelled position index construction cannot fail")
    }

    pub(crate) fn new_with_cancel(source: &str, cancel: &AtomicBool) -> Result<Self, ()> {
        Self::build(source, Some(cancel))
    }

    fn build(source: &str, cancel: Option<&AtomicBool>) -> Result<Self, ()> {
        if is_cancelled(cancel) {
            return Err(());
        }
        let bytes = source.as_bytes();
        let mut lines = Vec::new();
        let mut corrections = Vec::new();
        let mut excess = 0usize;
        let mut start = 0;
        let mut index = 0;
        let mut next_check = CANCEL_CHECK_BYTES;

        while index < bytes.len() {
            if index >= next_check {
                if is_cancelled(cancel) {
                    return Err(());
                }
                next_check = index.saturating_add(CANCEL_CHECK_BYTES);
            }
            let break_len = match bytes[index] {
                b'\n' => 1,
                b'\r' if bytes.get(index + 1) == Some(&b'\n') => 2,
                b'\r' => 1,
                byte if !byte.is_ascii() => {
                    let character = source[index..].chars().next().ok_or(())?;
                    excess += character.len_utf8() - character.len_utf16();
                    index += character.len_utf8();
                    corrections.push(Correction {
                        byte_end: index - start,
                        excess,
                    });
                    continue;
                }
                _ => {
                    index += 1;
                    continue;
                }
            };
            lines.push(IndexedLine {
                start,
                end: index,
                corrections: std::mem::take(&mut corrections).into_boxed_slice(),
            });
            excess = 0;
            start = index + break_len;
            index += break_len;
        }

        lines.push(IndexedLine {
            start,
            end: source.len(),
            corrections: corrections.into_boxed_slice(),
        });
        lines.shrink_to_fit();
        Ok(Self {
            source_len: source.len(),
            lines,
        })
    }

    pub(crate) fn offset_to_position(&self, source: &str, offset: usize) -> Option<Position> {
        if source.len() != self.source_len
            || offset > self.source_len
            || !source.is_char_boundary(offset)
        {
            return None;
        }

        let line_index = self
            .lines
            .partition_point(|line| line.start <= offset)
            .saturating_sub(1);
        let line = self.lines.get(line_index)?;
        if line.start <= offset && offset <= line.end {
            let relative = offset - line.start;
            let corrected = line
                .corrections
                .partition_point(|correction| correction.byte_end <= relative);
            let excess = corrected
                .checked_sub(1)
                .map_or(0, |last| line.corrections[last].excess);
            return Some(Position {
                line: u32::try_from(line_index).ok()?,
                character: u32::try_from(relative - excess).ok()?,
            });
        }
        // The offset is inside a line break, such as between CR and LF.
        None
    }

    pub(crate) fn position_to_offset(&self, source: &str, position: Position) -> Option<usize> {
        if source.len() != self.source_len {
            return None;
        }
        let line_index = usize::try_from(position.line).ok()?;
        let character = usize::try_from(position.character).ok()?;
        let line = self.lines.get(line_index)?;
        let corrected = line
            .corrections
            .partition_point(|correction| correction.byte_end - correction.excess <= character);
        let excess = corrected
            .checked_sub(1)
            .map_or(0, |last| line.corrections[last].excess);
        // Between corrections the line is ASCII, so the offset follows the
        // character; one that lands inside a scalar, including between the
        // two units of a surrogate pair, is not a position.
        let offset = line.start.checked_add(character)?.checked_add(excess)?;
        (offset <= line.end && source.is_char_boundary(offset)).then_some(offset)
    }
}

#[cfg(test)]
impl PositionIndex {
    fn heap_bytes(&self) -> usize {
        self.lines.capacity() * std::mem::size_of::<IndexedLine>()
            + self
                .lines
                .iter()
                .map(|line| line.corrections.len() * std::mem::size_of::<Correction>())
                .sum::<usize>()
    }
}

fn is_cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::{
        POSITION_INDEX_BUILDS, PositionIndex, position_to_offset, position_to_offset_clamped,
    };
    use lsp_types::Position;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn position_index_handles_utf16_boundaries_and_cancellation() {
        let source = "😀\r\nAlpha é";
        let index = PositionIndex::new(source);
        assert_eq!(
            index.offset_to_position(source, 4),
            Some(Position::new(0, 2))
        );
        assert_eq!(
            index.offset_to_position(source, 12),
            Some(Position::new(1, 6))
        );
        assert_eq!(position_to_offset(source, Position::new(1, 6)), Some(12));
        assert_eq!(
            position_to_offset(source, Position::new(0, 1)),
            None,
            "a surrogate-pair interior must not become a byte offset"
        );

        let cancelled = AtomicBool::new(true);
        assert!(PositionIndex::new_with_cancel(source, &cancelled).is_err());
    }

    /// The per-character index this module used before the sparse one,
    /// kept as the oracle for `sparse_index_agrees_with_per_character_index`.
    struct OldPositionIndex {
        lines: Vec<OldLine>,
    }

    struct OldLine {
        start: usize,
        end: usize,
        break_start: usize,
        break_end: usize,
        byte_offsets: Vec<usize>,
        utf16_offsets: Vec<usize>,
    }

    impl OldPositionIndex {
        fn new(source: &str) -> Self {
            let bytes = source.as_bytes();
            let mut lines = Vec::new();
            let mut start = 0;
            let mut index = 0;
            let line = |start: usize, end: usize, break_end: usize| {
                let mut byte_offsets = vec![0];
                let mut utf16_offsets = vec![0];
                let mut utf16 = 0;
                for (relative, character) in source[start..end].char_indices() {
                    utf16 += character.len_utf16();
                    byte_offsets.push(relative + character.len_utf8());
                    utf16_offsets.push(utf16);
                }
                OldLine {
                    start,
                    end,
                    break_start: end,
                    break_end,
                    byte_offsets,
                    utf16_offsets,
                }
            };
            while index < bytes.len() {
                let break_len = match bytes[index] {
                    b'\n' => 1,
                    b'\r' if bytes.get(index + 1) == Some(&b'\n') => 2,
                    b'\r' => 1,
                    _ => {
                        index += 1;
                        continue;
                    }
                };
                lines.push(line(start, index, index + break_len));
                start = index + break_len;
                index += break_len;
            }
            lines.push(line(start, source.len(), source.len()));
            Self { lines }
        }

        fn offset_to_position(&self, source: &str, offset: usize) -> Option<Position> {
            if offset > source.len() || !source.is_char_boundary(offset) {
                return None;
            }
            let line_index = self
                .lines
                .partition_point(|line| line.start <= offset)
                .saturating_sub(1);
            let line = self.lines.get(line_index)?;
            if line.start <= offset && offset <= line.end {
                let offset_index = line
                    .byte_offsets
                    .binary_search(&(offset - line.start))
                    .ok()?;
                return Some(Position::new(
                    u32::try_from(line_index).ok()?,
                    u32::try_from(line.utf16_offsets[offset_index]).ok()?,
                ));
            }
            let _ = (line.break_start, line.break_end);
            None
        }

        fn position_to_offset(&self, position: Position) -> Option<usize> {
            let line = self.lines.get(position.line as usize)?;
            let offset_index = line
                .utf16_offsets
                .binary_search(&(position.character as usize))
                .ok()?;
            Some(line.start + line.byte_offsets[offset_index])
        }
    }

    /// Deterministic mixed-content sources: ASCII, LF, CR, CRLF, two- and
    /// three-byte scalars, and supplementary scalars (UTF-16 pairs).
    fn mixed_sources() -> impl Iterator<Item = String> {
        const PIECES: [&str; 9] = ["a", "Zq", "\n", "\r", "\r\n", "é", "中", "😀", " "];
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        (0..400).map(move |round| {
            let len = round % 40;
            let mut source = String::new();
            for _ in 0..len {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                source.push_str(PIECES[(state % PIECES.len() as u64) as usize]);
            }
            source
        })
    }

    #[test]
    fn sparse_index_agrees_with_per_character_index() {
        for source in mixed_sources() {
            let old = OldPositionIndex::new(&source);
            let new = PositionIndex::new(&source);
            for offset in 0..=source.len() + 1 {
                assert_eq!(
                    new.offset_to_position(&source, offset),
                    old.offset_to_position(&source, offset),
                    "offset {offset} in {source:?}"
                );
            }
            let widest = source.encode_utf16().count() + 2;
            for line in 0..=old.lines.len() as u32 {
                for character in 0..=widest as u32 {
                    let position = Position::new(line, character);
                    assert_eq!(
                        new.position_to_offset(&source, position),
                        old.position_to_offset(position),
                        "{position:?} in {source:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn ascii_lines_store_no_per_character_offsets() {
        let line = format!("{}\n", "x".repeat(63));
        let source = line.repeat(16 * 1024);
        let index = PositionIndex::new(&source);
        assert!(
            index.heap_bytes() < source.len(),
            "{} index bytes for {} ASCII source bytes",
            index.heap_bytes(),
            source.len()
        );
    }

    #[test]
    fn one_shot_position_to_offset_does_not_build_a_full_position_index() {
        POSITION_INDEX_BUILDS.with(|builds| builds.set(0));
        assert_eq!(
            position_to_offset("prefix\n😀 value", Position::new(1, 3)),
            Some(12)
        );
        assert_eq!(POSITION_INDEX_BUILDS.with(std::cell::Cell::get), 0);
    }

    #[test]
    fn clamped_range_endpoints_preserve_utf16_and_crlf_boundaries() {
        let source = "😀abc\r\nxy\n";
        assert_eq!(
            position_to_offset_clamped(source, Position::new(0, 10)),
            Some(7)
        );
        assert_eq!(
            position_to_offset_clamped(source, Position::new(1, 10)),
            Some(11)
        );
        assert_eq!(
            position_to_offset_clamped(source, Position::new(0, 1)),
            None,
            "a surrogate-pair interior must remain invalid"
        );
        assert_eq!(
            position_to_offset_clamped(source, Position::new(2, 10)),
            Some(source.len())
        );
    }
}
