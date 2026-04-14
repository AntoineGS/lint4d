//! Source-level preprocessing pass that detects `{$if*}...{$endif}` directive
//! pairs wrapping a **partial control-flow header** (body ending in `then`,
//! `else`, or `do`) and rewrites the directive markers to ASCII spaces so
//! tree-sitter-pascal can parse the remaining content as a normal statement.
//!
//! The erased spans are recorded as [`DirectivePatch`] records; `fmt4d` later
//! re-injects them into `DirectiveMap` as virtual directives so the formatter
//! emits the original directive text in the output.
//!
//! See `.full-review/bucket-c-design.md` Part 3 for the full design rationale.

use std::borrow::Cow;

/// A directive pair that was rewritten from the source.
///
/// `opening_*` and `closing_*` byte ranges are relative to the **original**
/// source (and also to the rewritten source, since the rewrite preserves byte
/// offsets). `*_text` contains the original directive bytes verbatim so the
/// formatter can re-emit them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectivePatch {
    pub opening_start: usize,
    pub opening_end: usize,
    pub opening_text: String,
    pub opening_row: usize,
    pub opening_col: usize,
    pub closing_start: usize,
    pub closing_end: usize,
    pub closing_text: String,
    pub closing_row: usize,
    pub closing_col: usize,
}

/// Scan `source` for partial-control-flow directive pairs, rewrite the
/// directive markers to ASCII spaces, and return the rewritten bytes plus
/// the list of patches.
///
/// Returns `Cow::Borrowed(source)` when no patches are found — the common
/// case — so files without partial-control-flow directives pay zero
/// allocation cost.
///
/// Safe to run multiple times on the same input: the output of a rewrite has
/// no `{$if*}` markers left to match, so a second pass produces zero
/// additional patches.
pub fn rewrite_partial_control_flow(source: &[u8]) -> (Cow<'_, [u8]>, Vec<DirectivePatch>) {
    let Some(openings) = scan_directive_pairs(source) else {
        return (Cow::Borrowed(source), Vec::new());
    };

    let mut patches = Vec::new();
    for pair in openings {
        if pair.has_else_branch {
            continue;
        }
        let inner = &source[pair.opening_end..pair.closing_start];
        if !body_ends_in_continuation_keyword(inner) {
            continue;
        }
        let (opening_row, opening_col) = row_col_at(source, pair.opening_start);
        let (closing_row, closing_col) = row_col_at(source, pair.closing_start);
        patches.push(DirectivePatch {
            opening_start: pair.opening_start,
            opening_end: pair.opening_end,
            opening_text: bytes_to_lossy_string(&source[pair.opening_start..pair.opening_end]),
            opening_row,
            opening_col,
            closing_start: pair.closing_start,
            closing_end: pair.closing_end,
            closing_text: bytes_to_lossy_string(&source[pair.closing_start..pair.closing_end]),
            closing_row,
            closing_col,
        });
    }

    if patches.is_empty() {
        return (Cow::Borrowed(source), Vec::new());
    }

    let mut rewritten = source.to_vec();
    for p in &patches {
        for byte in &mut rewritten[p.opening_start..p.opening_end] {
            *byte = b' ';
        }
        for byte in &mut rewritten[p.closing_start..p.closing_end] {
            *byte = b' ';
        }
    }
    (Cow::Owned(rewritten), patches)
}

/// One `{$if*}...{$endif}` pair discovered by the scan.
struct Pair {
    opening_start: usize,
    opening_end: usize,   // exclusive, byte after `}`
    closing_start: usize, // byte of `{`
    closing_end: usize,   // exclusive, byte after `}`
    has_else_branch: bool,
}

/// Walk `source` with a mini-lexer that skips string literals and comments,
/// matching every `{$if*}` opener to its balanced `{$endif}` / `{$ifend}`
/// closer. Returns `None` if the source has no `{$if*}` directives at all
/// — the Cow::Borrowed fast path.
fn scan_directive_pairs(source: &[u8]) -> Option<Vec<Pair>> {
    let mut pairs = Vec::new();
    let mut cursor = 0usize;
    let mut has_any = false;

    while cursor < source.len() {
        cursor = skip_lexical_noise(source, cursor);
        if cursor >= source.len() {
            break;
        }
        if source[cursor] != b'{' || cursor + 1 >= source.len() || source[cursor + 1] != b'$' {
            cursor += 1;
            continue;
        }
        // cursor is at `{$`. Classify the directive keyword.
        let kind = directive_keyword_kind(source, cursor + 2);
        match kind {
            DirectiveKind::If => {
                has_any = true;
                let opening_start = cursor;
                let opening_end = find_close_brace(source, cursor)?;
                match scan_to_endif(source, opening_end) {
                    Some((closing_start, closing_end, has_else)) => {
                        pairs.push(Pair {
                            opening_start,
                            opening_end,
                            closing_start,
                            closing_end,
                            has_else_branch: has_else,
                        });
                        cursor = closing_end;
                    }
                    None => {
                        cursor = opening_end;
                    }
                }
            }
            _ => {
                cursor = find_close_brace(source, cursor).unwrap_or(cursor + 1);
            }
        }
    }

    if has_any { Some(pairs) } else { None }
}

#[derive(PartialEq, Eq)]
enum DirectiveKind {
    If,
    Else,
    Endif,
    Other,
}

fn directive_keyword_kind(source: &[u8], start: usize) -> DirectiveKind {
    let rest = &source[start..];
    let end = rest
        .iter()
        .position(|&b| !b.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    let kw: String = rest[..end]
        .iter()
        .map(|&b| b.to_ascii_lowercase() as char)
        .collect();
    match kw.as_str() {
        "if" | "ifdef" | "ifndef" => DirectiveKind::If,
        "else" | "elseif" => DirectiveKind::Else,
        "endif" | "ifend" => DirectiveKind::Endif,
        _ => DirectiveKind::Other,
    }
}

/// Given that `opening_end` is just after the `}` of a `{$if*}`, walk forward
/// tracking nested depth until the matching `{$endif}` / `{$ifend}`. Returns
/// `(closing_start, closing_end, saw_else_at_depth_1)`.
fn scan_to_endif(source: &[u8], opening_end: usize) -> Option<(usize, usize, bool)> {
    let mut cursor = opening_end;
    let mut depth = 1usize;
    let mut saw_else = false;
    while cursor < source.len() {
        cursor = skip_lexical_noise(source, cursor);
        if cursor >= source.len() {
            return None;
        }
        if source[cursor] != b'{' || cursor + 1 >= source.len() || source[cursor + 1] != b'$' {
            cursor += 1;
            continue;
        }
        let kind = directive_keyword_kind(source, cursor + 2);
        match kind {
            DirectiveKind::If => {
                depth += 1;
                cursor = find_close_brace(source, cursor)?;
            }
            DirectiveKind::Else if depth == 1 => {
                saw_else = true;
                cursor = find_close_brace(source, cursor)?;
            }
            DirectiveKind::Else => {
                cursor = find_close_brace(source, cursor)?;
            }
            DirectiveKind::Endif => {
                depth -= 1;
                let closing_start = cursor;
                let closing_end = find_close_brace(source, cursor)?;
                if depth == 0 {
                    return Some((closing_start, closing_end, saw_else));
                }
                cursor = closing_end;
            }
            DirectiveKind::Other => {
                cursor = find_close_brace(source, cursor).unwrap_or(cursor + 1);
            }
        }
    }
    None
}

/// Starting at a `{`, find the byte just after the matching `}`. Directives
/// are single-line in Delphi practice but may contain nested content — for
/// our purposes we scan to the **next** `}` which is the directive terminator.
fn find_close_brace(source: &[u8], start: usize) -> Option<usize> {
    debug_assert_eq!(source[start], b'{');
    let mut cursor = start + 1;
    while cursor < source.len() {
        if source[cursor] == b'}' {
            return Some(cursor + 1);
        }
        cursor += 1;
    }
    None
}

/// Advance `cursor` past any whitespace, comment, or string literal,
/// returning the position of the next lexically-significant byte (or
/// `source.len()` if we fall off the end). Directives `{$...}` are NOT
/// treated as comments — they are lexically significant.
fn skip_lexical_noise(source: &[u8], mut cursor: usize) -> usize {
    loop {
        if cursor >= source.len() {
            return cursor;
        }
        match source[cursor] {
            b'\'' => {
                cursor = skip_string_literal(source, cursor);
            }
            b'/' if cursor + 1 < source.len() && source[cursor + 1] == b'/' => {
                cursor = skip_line_comment(source, cursor);
            }
            b'(' if cursor + 1 < source.len() && source[cursor + 1] == b'*' => {
                cursor = skip_paren_star_comment(source, cursor);
            }
            b'{' if cursor + 1 < source.len() && source[cursor + 1] != b'$' => {
                cursor = skip_brace_comment(source, cursor);
            }
            _ => return cursor,
        }
    }
}

fn skip_string_literal(source: &[u8], start: usize) -> usize {
    debug_assert_eq!(source[start], b'\'');
    let mut cursor = start + 1;
    while cursor < source.len() {
        if source[cursor] == b'\'' {
            if cursor + 1 < source.len() && source[cursor + 1] == b'\'' {
                // Doubled quote escape — stay inside the literal.
                cursor += 2;
                continue;
            }
            return cursor + 1;
        }
        if source[cursor] == b'\n' {
            // Unterminated literal — bail to end-of-line so we don't hang.
            return cursor;
        }
        cursor += 1;
    }
    cursor
}

fn skip_line_comment(source: &[u8], start: usize) -> usize {
    let mut cursor = start + 2;
    while cursor < source.len() && source[cursor] != b'\n' {
        cursor += 1;
    }
    cursor
}

fn skip_paren_star_comment(source: &[u8], start: usize) -> usize {
    // Start scanning from `start + 1` (the `*` of the opener) so that the
    // degenerate empty comment `(*)` — where the `*` is shared between
    // opener and closer — is recognized. This mirrors the backward-scan
    // semantics in `find_matching_paren_star`.
    let mut cursor = start + 1;
    while cursor + 1 < source.len() {
        if source[cursor] == b'*' && source[cursor + 1] == b')' {
            return cursor + 2;
        }
        cursor += 1;
    }
    source.len()
}

fn skip_brace_comment(source: &[u8], start: usize) -> usize {
    debug_assert_eq!(source[start], b'{');
    let mut cursor = start + 1;
    while cursor < source.len() {
        if source[cursor] == b'}' {
            return cursor + 1;
        }
        cursor += 1;
    }
    source.len()
}

/// `true` if the last significant token in `body` is `then`, `else`, or `do`
/// (case-insensitive, word-boundary).
fn body_ends_in_continuation_keyword(body: &[u8]) -> bool {
    // Scan backward, skipping trailing whitespace and Delphi comments.
    let mut end = body.len();
    loop {
        end = trim_trailing_whitespace(body, end);
        let before = trim_trailing_comment(body, end);
        if before == end {
            break;
        }
        end = before;
    }
    if end == 0 {
        return false;
    }
    // Find the start of the last word.
    let mut start = end;
    while start > 0 && body[start - 1].is_ascii_alphabetic() {
        start -= 1;
    }
    if start == end {
        return false;
    }
    // Word boundary: char before `start` must NOT be alphanumeric / underscore.
    if start > 0 {
        let b = body[start - 1];
        if b.is_ascii_alphanumeric() || b == b'_' {
            return false;
        }
    }
    let word: String = body[start..end]
        .iter()
        .map(|&b| b.to_ascii_lowercase() as char)
        .collect();
    matches!(word.as_str(), "then" | "else" | "do")
}

fn trim_trailing_whitespace(body: &[u8], mut end: usize) -> usize {
    while end > 0 && matches!(body[end - 1], b' ' | b'\t' | b'\n' | b'\r') {
        end -= 1;
    }
    end
}

fn trim_trailing_comment(body: &[u8], end: usize) -> usize {
    if end < 2 {
        return end;
    }
    // `{ ... }` trailing comment
    if body[end - 1] == b'}' {
        if let Some(open) = find_matching_open_brace_comment(body, end - 1) {
            return open;
        }
    }
    // `(* ... *)` trailing comment
    if end >= 2 && body[end - 2] == b'*' && body[end - 1] == b')' {
        if let Some(open) = find_matching_paren_star(body, end - 2) {
            return open;
        }
    }
    end
}

fn find_matching_open_brace_comment(body: &[u8], close_pos: usize) -> Option<usize> {
    // Walk backward from close_pos looking for an unmatched `{` that is NOT
    // part of `{$...}`. Conservative — only handles simple trailing comments.
    let mut cursor = close_pos;
    while cursor > 0 {
        cursor -= 1;
        if body[cursor] == b'{' {
            if cursor + 1 < body.len() && body[cursor + 1] == b'$' {
                // Directive, not a comment — give up.
                return None;
            }
            return Some(cursor);
        }
    }
    None
}

fn find_matching_paren_star(body: &[u8], star_paren_pos: usize) -> Option<usize> {
    // Walk backward looking for `(*`.
    let mut cursor = star_paren_pos + 1;
    while cursor >= 2 {
        cursor -= 1;
        if body[cursor - 1] == b'(' && body[cursor] == b'*' {
            return Some(cursor - 1);
        }
    }
    None
}

fn row_col_at(source: &[u8], byte_offset: usize) -> (usize, usize) {
    let mut row = 0usize;
    let mut col = 0usize;
    for &b in &source[..byte_offset.min(source.len())] {
        if b == b'\n' {
            row += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (row, col)
}

fn bytes_to_lossy_string(bytes: &[u8]) -> String {
    // Delphi source may be Latin-1. Use lossy UTF-8 conversion for the
    // directive text preservation path — the original bytes are also stored
    // in source, so this is only used for debug/display purposes in
    // `DirectivePatch::*_text`.
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ascii(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    fn patches_of(source: &[u8]) -> Vec<DirectivePatch> {
        rewrite_partial_control_flow(source).1
    }

    fn rewritten_of(source: &[u8]) -> Vec<u8> {
        rewrite_partial_control_flow(source).0.into_owned()
    }

    #[test]
    fn rewrite_single_if_then() {
        let src = ascii(
            "procedure P;\n\
             begin\n\
             {$IFNDEF DEBUG} if cond then {$ENDIF}\n  \
               DoThing;\n\
             end;\n",
        );
        let (rewritten, patches) = rewrite_partial_control_flow(&src);
        assert_eq!(patches.len(), 1, "expected one patch");
        let p = &patches[0];
        assert_eq!(p.opening_text, "{$IFNDEF DEBUG}");
        assert_eq!(p.closing_text, "{$ENDIF}");
        // Rewritten bytes must be the same length and must have spaces
        // where the directive markers were.
        assert_eq!(rewritten.len(), src.len());
        let rewritten_bytes = rewritten.into_owned();
        for i in p.opening_start..p.opening_end {
            assert_eq!(
                rewritten_bytes[i], b' ',
                "opening marker byte at {i} must be space"
            );
        }
        for i in p.closing_start..p.closing_end {
            assert_eq!(
                rewritten_bytes[i], b' ',
                "closing marker byte at {i} must be space"
            );
        }
        // Content between markers (` if cond then `) is preserved.
        let inner_start = p.opening_end;
        let inner_end = p.closing_start;
        assert_eq!(
            &rewritten_bytes[inner_start..inner_end],
            &src[inner_start..inner_end],
            "content between markers must be untouched"
        );
    }

    #[test]
    fn rewrite_if_then_else() {
        let src = ascii(
            "procedure P;\n\
             begin\n\
             {$IFNDEF NOSF}\n\
               if cond then\n\
                 X := 1\n\
               else\n\
             {$ENDIF}\n  \
                 Y := true;\n\
             end;\n",
        );
        let patches = patches_of(&src);
        assert_eq!(patches.len(), 1, "expected one patch");
        assert_eq!(patches[0].opening_text, "{$IFNDEF NOSF}");
        assert_eq!(patches[0].closing_text, "{$ENDIF}");
    }

    #[test]
    fn rewrite_trailing_do() {
        let src = ascii("{$IFNDEF X} while cond do {$ENDIF}\n  DoThing;\n");
        assert_eq!(patches_of(&src).len(), 1);
    }

    #[test]
    fn rewrite_skip_when_else_branch_present() {
        // Directive with a top-level {$else} is not a partial control-flow
        // pattern — it's a conditional whose branches we don't mess with.
        let src = ascii("{$IFDEF A} if a then {$ELSE} if b then {$ENDIF}\n  DoThing;\n");
        assert_eq!(
            patches_of(&src).len(),
            0,
            "directives with else branches must not be rewritten"
        );
    }

    #[test]
    fn rewrite_skip_when_content_is_full_statement() {
        // {$IFDEF} x := 1; {$ENDIF} — the content is a complete assignment,
        // not a partial control-flow header. Must not rewrite.
        let src = ascii("{$IFDEF A} x := 1; {$ENDIF}\n");
        assert_eq!(patches_of(&src).len(), 0);
    }

    #[test]
    fn rewrite_skip_when_content_has_no_trailing_keyword() {
        // Content ends in identifier, not in then/else/do.
        let src = ascii("{$IFDEF A} x {$ENDIF}\n");
        assert_eq!(patches_of(&src).len(), 0);
    }

    #[test]
    fn rewrite_skip_inside_string_literal() {
        let src = ascii("s := '{$IFDEF foo} if x then {$ENDIF}';\n");
        assert_eq!(
            patches_of(&src).len(),
            0,
            "directives inside string literals must not match"
        );
    }

    #[test]
    fn rewrite_skip_inside_double_quoted_escape() {
        // Delphi uses doubled single-quote as escape: 'it''s'
        let src = ascii("s := 'it''s {$IFDEF foo} if x then {$ENDIF}';\n");
        assert_eq!(patches_of(&src).len(), 0);
    }

    #[test]
    fn rewrite_skip_inside_line_comment() {
        let src = ascii("// {$IFDEF foo} if x then {$ENDIF}\n");
        assert_eq!(patches_of(&src).len(), 0);
    }

    #[test]
    fn rewrite_skip_inside_brace_comment() {
        let src = ascii("{ {$IFDEF foo} if x then {$ENDIF} }\n");
        assert_eq!(patches_of(&src).len(), 0);
    }

    #[test]
    fn rewrite_skip_inside_paren_star_comment() {
        let src = ascii("(* {$IFDEF foo} if x then {$ENDIF} *)\n");
        assert_eq!(patches_of(&src).len(), 0);
    }

    #[test]
    fn rewrite_nested_directives_tracks_depth() {
        // Outer {$IFDEF A} ... {$IFDEF B} ... {$ENDIF} ... {$ENDIF}
        // The outer body ends in `then`. It should match as one patch.
        let src =
            ascii("{$IFDEF A} if cond then {$IFDEF B} x {$ENDIF} then {$ENDIF}\n  DoThing;\n");
        let patches = patches_of(&src);
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].opening_text, "{$IFDEF A}");
        assert_eq!(patches[0].closing_text, "{$ENDIF}");
        assert!(
            patches[0].closing_start > src.windows(10).position(|w| w == b"{$IFDEF B}").unwrap(),
            "closing must be the OUTER {{$ENDIF}}"
        );
    }

    #[test]
    fn rewrite_byte_offset_preservation() {
        let src = ascii("{$IFNDEF X} if c then {$ENDIF}\n  stmt;\n");
        let (rewritten, _) = rewrite_partial_control_flow(&src);
        assert_eq!(
            rewritten.len(),
            src.len(),
            "rewrite must preserve total length"
        );
    }

    #[test]
    fn rewrite_idempotent() {
        let src = ascii("{$IFNDEF X} if c then {$ENDIF}\n  stmt;\n");
        let once = rewritten_of(&src);
        let twice = rewritten_of(&once);
        assert_eq!(once, twice, "second pass must produce zero new patches");
        let (_, patches2) = rewrite_partial_control_flow(&once);
        assert_eq!(patches2.len(), 0, "second pass must yield no patches");
    }

    #[test]
    fn rewrite_borrowed_fast_path_for_clean_source() {
        let src = ascii("procedure P;\nbegin\n  x := 1;\nend;\n");
        let (cow, patches) = rewrite_partial_control_flow(&src);
        assert_eq!(patches.len(), 0);
        assert!(
            matches!(cow, Cow::Borrowed(_)),
            "clean source must not allocate"
        );
    }

    #[test]
    fn rewrite_records_row_and_column() {
        let src = ascii(
            "line0\n\
             {$IFNDEF X} if c then {$ENDIF}\n  stmt;\n",
        );
        let patches = patches_of(&src);
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].opening_row, 1);
        assert_eq!(patches[0].opening_col, 0);
        assert_eq!(patches[0].closing_row, 1);
        // Closing col is where the `{` of `{$ENDIF}` starts on its line.
        assert!(patches[0].closing_col > 0);
    }

    #[test]
    fn rewrite_case_insensitive_keywords() {
        let src = ascii("{$ifndef DEBUG} if c THEN {$endif}\n  stmt;\n");
        assert_eq!(patches_of(&src).len(), 1);
    }

    #[test]
    fn rewrite_trailing_empty_paren_star_comment() {
        // `if cond then(*)` — the (*) is an empty trailing comment that
        // must be trimmed so `then` is recognized as the last significant
        // token. Without the find_matching_paren_star off-by-one fix this
        // returns zero patches.
        let src = ascii("{$IFDEF A} if cond then(*){$ENDIF}\n  stmt;\n");
        assert_eq!(patches_of(&src).len(), 1);
    }
}
