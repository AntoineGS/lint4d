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

fn lines<'a>(source: &'a str) -> Vec<Line<'a>> {
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

fn utf16_before(source: &str, offset: usize) -> usize {
    source[..offset].encode_utf16().count()
}

/// Convert a UTF-8 byte offset to a zero-based LSP position measured in
/// UTF-16 code units. Offsets in the middle of a UTF-8 scalar or CRLF pair are
/// rejected.
pub fn offset_to_position(source: &str, offset: usize) -> Option<Position> {
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
/// Keeping line boundaries and per-line UTF-16 prefixes avoids rescanning the
/// complete prefix of the source for every span.
#[derive(Debug, Clone)]
pub(crate) struct PositionIndex {
    source_len: usize,
    lines: Vec<IndexedLine>,
}

#[cfg(test)]
thread_local! {
    static POSITION_INDEX_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Debug, Clone)]
struct IndexedLine {
    start: usize,
    end: usize,
    break_start: usize,
    break_end: usize,
    byte_offsets: Vec<usize>,
    utf16_offsets: Vec<usize>,
}

impl PositionIndex {
    pub(crate) fn new(source: &str) -> Self {
        #[cfg(test)]
        POSITION_INDEX_BUILDS.with(|builds| builds.set(builds.get().saturating_add(1)));
        Self::build(source, None).expect("uncancelled position index construction cannot fail")
    }

    pub(crate) fn new_with_cancel(source: &str, cancel: &AtomicBool) -> Result<Self, ()> {
        Self::build(source, Some(cancel))
    }

    fn build(source: &str, cancel: Option<&AtomicBool>) -> Result<Self, ()> {
        let mut lines = Vec::new();
        let bytes = source.as_bytes();
        let mut start = 0;
        let mut index = 0;

        while index < bytes.len() {
            if is_cancelled(cancel) {
                return Err(());
            }
            let break_len = match bytes[index] {
                b'\n' => 1,
                b'\r' if bytes.get(index + 1) == Some(&b'\n') => 2,
                b'\r' => 1,
                _ => {
                    index += 1;
                    continue;
                }
            };
            lines.push(build_line(
                source,
                start,
                index,
                index,
                index + break_len,
                cancel,
            )?);
            start = index + break_len;
            index += break_len;
        }

        lines.push(build_line(
            source,
            start,
            source.len(),
            source.len(),
            source.len(),
            cancel,
        )?);
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
            let offset_index = line.byte_offsets.binary_search(&relative).ok()?;
            return Some(Position {
                line: u32::try_from(line_index).ok()?,
                character: u32::try_from(line.utf16_offsets[offset_index]).ok()?,
            });
        }
        if line.break_start < offset && offset < line.break_end {
            return None;
        }
        None
    }
}

fn build_line(
    source: &str,
    start: usize,
    end: usize,
    break_start: usize,
    break_end: usize,
    cancel: Option<&AtomicBool>,
) -> Result<IndexedLine, ()> {
    let mut byte_offsets = vec![0];
    let mut utf16_offsets = vec![0];
    let mut utf16 = 0;
    for (relative, character) in source[start..end].char_indices() {
        if is_cancelled(cancel) {
            return Err(());
        }
        utf16 += character.len_utf16();
        byte_offsets.push(relative + character.len_utf8());
        utf16_offsets.push(utf16);
    }
    Ok(IndexedLine {
        start,
        end,
        break_start,
        break_end,
        byte_offsets,
        utf16_offsets,
    })
}

fn is_cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::{POSITION_INDEX_BUILDS, PositionIndex, position_to_offset};
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

    #[test]
    fn one_shot_position_to_offset_does_not_build_a_full_position_index() {
        POSITION_INDEX_BUILDS.with(|builds| builds.set(0));
        assert_eq!(
            position_to_offset("prefix\n😀 value", Position::new(1, 3)),
            Some(12)
        );
        assert_eq!(POSITION_INDEX_BUILDS.with(std::cell::Cell::get), 0);
    }
}
