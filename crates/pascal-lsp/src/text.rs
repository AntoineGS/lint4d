use lsp_types::Position;

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

/// Convert a zero-based LSP position (UTF-16 characters) to a UTF-8 byte
/// offset. Positions in the middle of a UTF-16 surrogate pair, in a line
/// ending, or beyond the line's content are rejected.
pub fn position_to_offset(source: &str, position: Position) -> Option<usize> {
    let line_number = usize::try_from(position.line).ok()?;
    let character = usize::try_from(position.character).ok()?;
    let line = lines(source).get(line_number).copied()?;
    offset_for_utf16(line, character)
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
