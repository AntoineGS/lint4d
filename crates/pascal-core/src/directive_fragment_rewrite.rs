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
/// Can be either a pair of markers (`{$if*}...{$endif}`) whose markers were
/// blanked to allow parsing, or an opaque block (`{$IF...}{$IFEND}`) whose
/// entire span was blanked because the body is not valid Pascal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectivePatch {
    /// A `{$if*}…{$endif}` pair whose markers were blanked so the body
    /// parses as a standalone fragment. Emitted by `rewrite_partial_control_flow`.
    Markers(MarkersPatch),
    /// A `{$IF…}…{$IFEND}` block whose entire span (markers + body) was
    /// blanked because the body is not valid Pascal. The original bytes are
    /// stored verbatim and re-emitted by the formatter. Emitted by
    /// `rewrite_opaque_if_blocks`.
    OpaqueBlock(OpaqueBlockPatch),
}

/// A directive pair that was rewritten from the source.
///
/// `opening_*` and `closing_*` byte ranges are relative to the **original**
/// source (and also to the rewritten source, since the rewrite preserves byte
/// offsets). `*_text` contains the original directive bytes verbatim so the
/// formatter can re-emit them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkersPatch {
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

/// An opaque directive block whose entire span was blanked.
///
/// Used when a `{$IF…}…{$IFEND}` block's body is not valid Pascal and cannot
/// be parsed even after removing the markers. The entire block is stored as
/// original bytes so the formatter can re-emit it verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpaqueBlockPatch {
    /// Byte of `{` in the opening `{$IF…}`.
    pub start: usize,
    /// Byte after `}` in the closing `{$IFEND}` (or `{$ENDIF}`).
    pub end: usize,
    /// Original bytes `[start..end]` verbatim, including both markers
    /// and the body between them.
    pub text: String,
    pub row: usize,
    pub col: usize,
}

impl DirectivePatch {
    /// Test helper: unwraps to `&MarkersPatch` or panics.
    #[cfg(test)]
    pub fn expect_markers(&self) -> &MarkersPatch {
        match self {
            Self::Markers(m) => m,
            Self::OpaqueBlock(_) => panic!("expected Markers patch, got OpaqueBlock"),
        }
    }

    /// Test helper: unwraps to `&OpaqueBlockPatch` or panics.
    #[cfg(test)]
    pub fn expect_opaque(&self) -> &OpaqueBlockPatch {
        match self {
            Self::OpaqueBlock(o) => o,
            Self::Markers(_) => panic!("expected OpaqueBlock patch, got Markers"),
        }
    }
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
        patches.push(DirectivePatch::Markers(MarkersPatch {
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
        }));
    }

    if patches.is_empty() {
        return (Cow::Borrowed(source), Vec::new());
    }

    let mut rewritten = source.to_vec();
    for p in &patches {
        let DirectivePatch::Markers(m) = p else {
            continue;
        };
        for byte in &mut rewritten[m.opening_start..m.opening_end] {
            *byte = b' ';
        }
        for byte in &mut rewritten[m.closing_start..m.closing_end] {
            *byte = b' ';
        }
    }
    (Cow::Owned(rewritten), patches)
}

/// Scan `source` for `{$IF cond}…{$IFEND}` (or `{$ENDIF}`) blocks whose body
/// is not valid Pascal, blank the entire span to ASCII spaces (preserving
/// newlines), and return the rewritten bytes plus a list of
/// [`DirectivePatch::OpaqueBlock`] records carrying the original spans.
///
/// Returns `Cow::Borrowed(source)` when no patches are found.
///
/// **Only applies to `{$IF}`** — `{$IFDEF}`, `{$IFNDEF}`, and `{$IFOPT}` are
/// left alone. Bucket F is specifically the "non-Pascal content hidden in a
/// dead `{$IF}` branch" pattern.
///
/// Safe to run after `rewrite_partial_control_flow` on the same source:
/// neither rewriter touches bytes the other has already blanked, and both
/// preserve byte offsets.
pub fn rewrite_opaque_if_blocks(source: &[u8]) -> (Cow<'_, [u8]>, Vec<DirectivePatch>) {
    let Some(pairs) = scan_directive_pairs(source) else {
        return (Cow::Borrowed(source), Vec::new());
    };

    let mut patches = Vec::new();
    for pair in pairs {
        if !is_if_opener(source, pair.opening_start) {
            continue;
        }
        let body = &source[pair.opening_end..pair.closing_start];
        if body_is_parseable_pascal(body) {
            continue;
        }
        let (row, col) = row_col_at(source, pair.opening_start);
        patches.push(DirectivePatch::OpaqueBlock(OpaqueBlockPatch {
            start: pair.opening_start,
            end: pair.closing_end,
            text: bytes_to_lossy_string(&source[pair.opening_start..pair.closing_end]),
            row,
            col,
        }));
    }

    if patches.is_empty() {
        return (Cow::Borrowed(source), Vec::new());
    }

    let mut rewritten = source.to_vec();
    for p in &patches {
        let DirectivePatch::OpaqueBlock(ob) = p else {
            continue;
        };
        for byte in &mut rewritten[ob.start..ob.end] {
            if *byte != b'\r' && *byte != b'\n' {
                *byte = b' ';
            }
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

/// Returns true iff `source[opening_start..]` begins with a `{$IF}` directive
/// opener — i.e., `{$IF` followed by a non-alphabetic byte. This distinguishes
/// `{$IF}` from `{$IFDEF}`, `{$IFNDEF}`, `{$IFOPT}`, `{$IFEND}`, and `{$ENDIF}`.
///
/// Case-insensitive on the keyword itself.
fn is_if_opener(source: &[u8], opening_start: usize) -> bool {
    let prefix = &source[opening_start..];
    if prefix.len() < 4 {
        return false;
    }
    if prefix[0] != b'{' || prefix[1] != b'$' {
        return false;
    }
    if !(prefix[2] == b'i' || prefix[2] == b'I') {
        return false;
    }
    if !(prefix[3] == b'f' || prefix[3] == b'F') {
        return false;
    }
    // Character after "if" must not be alphabetic (else it's ifdef, ifend, etc.)
    let next = prefix.get(4).copied().unwrap_or(b' ');
    !next.is_ascii_alphabetic()
}

/// Probes whether `body` parses as valid Pascal when wrapped in a minimal
/// harness. Tries a statement-position harness first; if that fails, tries
/// a declaration-position harness. Returns true if either succeeds.
///
/// Empty or whitespace-only bodies are trivially valid.
///
/// This is the detection heuristic for Bucket F: only bodies that both
/// harnesses reject are considered "non-Pascal content" and blanked by
/// `rewrite_opaque_if_blocks`.
fn body_is_parseable_pascal(body: &[u8]) -> bool {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return true;
    }

    // Statement-position harness: body goes between `begin` and `end.`
    const STMT_HEADER: &[u8] = b"program Harness;\nbegin\n";
    const STMT_FOOTER: &[u8] = b"\nend.\n";

    // Declaration-position harness: body goes in a unit interface.
    const DECL_HEADER: &[u8] = b"unit Harness;\ninterface\n";
    const DECL_FOOTER: &[u8] = b"\nimplementation\nend.\n";

    try_harness(STMT_HEADER, body, STMT_FOOTER) || try_harness(DECL_HEADER, body, DECL_FOOTER)
}

/// Wraps `body` in `header`/`footer`, parses the result, and returns true if
/// the resulting tree has no ERROR or MISSING nodes.
fn try_harness(header: &[u8], body: &[u8], footer: &[u8]) -> bool {
    let mut wrapped = Vec::with_capacity(header.len() + body.len() + footer.len());
    wrapped.extend_from_slice(header);
    wrapped.extend_from_slice(body);
    wrapped.extend_from_slice(footer);

    let parsed = crate::parser::PARSER.with(|p| {
        let mut parser = p.borrow_mut();
        parser.parse(&wrapped, None)
    });

    match parsed {
        Some(tree) => !tree_has_error(tree.root_node()),
        None => false,
    }
}

/// Recursively checks whether `node` or any of its descendants is an ERROR
/// or MISSING node.
fn tree_has_error(node: tree_sitter::Node) -> bool {
    if node.is_error() || node.is_missing() {
        return true;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if tree_has_error(child) {
            return true;
        }
    }
    false
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
        let p = patches[0].expect_markers();
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
        let m = patches[0].expect_markers();
        assert_eq!(m.opening_text, "{$IFNDEF NOSF}");
        assert_eq!(m.closing_text, "{$ENDIF}");
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
        let m = patches[0].expect_markers();
        assert_eq!(m.opening_text, "{$IFDEF A}");
        assert_eq!(m.closing_text, "{$ENDIF}");
        assert!(
            m.closing_start > src.windows(10).position(|w| w == b"{$IFDEF B}").unwrap(),
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
        let m = patches[0].expect_markers();
        assert_eq!(m.opening_row, 1);
        assert_eq!(m.opening_col, 0);
        assert_eq!(m.closing_row, 1);
        // Closing col is where the `{` of `{$ENDIF}` starts on its line.
        assert!(m.closing_col > 0);
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

    #[test]
    fn is_if_opener_matches_if_with_space() {
        assert!(is_if_opener(b"{$IF DEFINED(X)}", 0));
    }

    #[test]
    fn is_if_opener_matches_if_with_tab() {
        assert!(is_if_opener(b"{$IF\tDEFINED(X)}", 0));
    }

    #[test]
    fn is_if_opener_matches_lowercase_if() {
        assert!(is_if_opener(b"{$if DEFINED(X)}", 0));
    }

    #[test]
    fn is_if_opener_matches_bare_if_close() {
        // `{$IF}` with no argument — unusual but legal tokenwise.
        assert!(is_if_opener(b"{$IF}", 0));
    }

    #[test]
    fn is_if_opener_rejects_ifdef() {
        assert!(!is_if_opener(b"{$IFDEF DEBUG}", 0));
    }

    #[test]
    fn is_if_opener_rejects_ifndef() {
        assert!(!is_if_opener(b"{$IFNDEF DEBUG}", 0));
    }

    #[test]
    fn is_if_opener_rejects_ifopt() {
        assert!(!is_if_opener(b"{$IFOPT Q+}", 0));
    }

    #[test]
    fn is_if_opener_rejects_ifend() {
        // `{$IFEND}` is a closer, not an opener. It shouldn't pass this check.
        assert!(!is_if_opener(b"{$IFEND}", 0));
    }

    #[test]
    fn is_if_opener_rejects_endif() {
        assert!(!is_if_opener(b"{$ENDIF}", 0));
    }

    #[test]
    fn is_if_opener_rejects_non_directive() {
        assert!(!is_if_opener(b"{ regular comment }", 0));
    }

    #[test]
    fn is_if_opener_handles_offset_start() {
        let src = b"  {$IF X}";
        assert!(is_if_opener(src, 2));
    }

    #[test]
    fn body_parseable_accepts_empty() {
        assert!(body_is_parseable_pascal(b""));
    }

    #[test]
    fn body_parseable_accepts_whitespace_only() {
        assert!(body_is_parseable_pascal(b"  \n\t\r\n "));
    }

    #[test]
    fn body_parseable_accepts_statement() {
        assert!(body_is_parseable_pascal(b"X := 1;"));
    }

    #[test]
    fn body_parseable_accepts_multiple_statements() {
        assert!(body_is_parseable_pascal(b"X := 1;\nY := 2;\n"));
    }

    #[test]
    fn body_parseable_accepts_const_declaration() {
        assert!(body_is_parseable_pascal(b"const X = 1;"));
    }

    #[test]
    fn body_parseable_accepts_type_declaration() {
        assert!(body_is_parseable_pascal(b"type TFoo = Integer;"));
    }

    #[test]
    fn body_parseable_rejects_french_dev_note() {
        assert!(!body_is_parseable_pascal(
            b"rappel: developper en 32 bits pour plus de stabilite"
        ));
    }

    #[test]
    fn body_parseable_rejects_prose() {
        assert!(!body_is_parseable_pascal(
            b"This is just a note to myself about the build."
        ));
    }

    #[test]
    fn body_parseable_accepts_begin_end_block() {
        assert!(body_is_parseable_pascal(b"begin X := 1; end;"));
    }

    fn opaque_patches_of(source: &[u8]) -> Vec<DirectivePatch> {
        rewrite_opaque_if_blocks(source).1
    }

    fn opaque_rewritten_of(source: &[u8]) -> Vec<u8> {
        rewrite_opaque_if_blocks(source).0.into_owned()
    }

    #[test]
    fn f_u1_opaque_if_with_non_pascal_body_is_patched() {
        // Use the real-world French dev note that is definitely not valid Pascal
        let src = b"unit X;\ninterface\nimplementation\n\
                    {$IF DEFINED(X)}\nrappel: developper en 32 bits pour plus de stabilite\n{$IFEND}\n\
                    end.\n";
        let patches = opaque_patches_of(src);
        assert_eq!(patches.len(), 1, "expected one opaque patch");
        let o = patches[0].expect_opaque();
        assert!(o.text.starts_with("{$IF DEFINED(X)}"));
        assert!(o.text.ends_with("{$IFEND}"));
        assert!(o.text.contains("rappel: developper en 32 bits"));

        // Rewritten bytes must preserve newlines but blank everything else
        // in the opaque span.
        let rewritten = opaque_rewritten_of(src);
        assert_eq!(rewritten.len(), src.len());
        for i in o.start..o.end {
            let b = rewritten[i];
            assert!(
                b == b' ' || b == b'\n' || b == b'\r',
                "rewritten byte at {i} must be space or newline, got {b:?}"
            );
        }
    }

    #[test]
    fn f_u2_valid_if_body_is_not_patched() {
        let src = b"unit X;\ninterface\n{$IF VERSION >= 28}\nconst X = 1;\n{$IFEND}\n\
                    implementation\nend.\n";
        let patches = opaque_patches_of(src);
        assert_eq!(patches.len(), 0, "valid decl body must not be patched");
    }

    #[test]
    fn f_u3_valid_if_statement_body_is_not_patched() {
        // Embedded inside a begin/end so the body position matches the harness.
        let src = b"program P;\nbegin\n{$IF DEBUG}\nWriteLn('x');\n{$IFEND}\nend.\n";
        let patches = opaque_patches_of(src);
        assert_eq!(patches.len(), 0, "valid statement body must not be patched");
    }

    #[test]
    fn f_u4_ifdef_non_pascal_body_is_not_patched() {
        // {$IFDEF} is out of scope for Bucket F — only {$IF} is covered.
        let src = b"unit X;\ninterface\nimplementation\n\
                    {$IFDEF X}\nrappel: developper\n{$ENDIF}\nend.\n";
        let patches = opaque_patches_of(src);
        assert_eq!(patches.len(), 0, "{{$IFDEF}} is out of Bucket F scope");
    }

    #[test]
    fn f_u5_nested_if_inside_opaque_body_balances_to_outer() {
        // Outer {$IF}...{$IFEND} contains an inner {$IF}...{$IFEND} and
        // non-Pascal text. The outer should match and patch the whole outer span.
        let src = b"unit X;\ninterface\nimplementation\n\
                    {$IF A}\nrappel: developper en 32 bits pour plus de stabilite\n{$IF B}\nrappel: corriger le bug de parser\n{$IFEND}\n{$IFEND}\n\
                    end.\n";
        let patches = opaque_patches_of(src);
        assert_eq!(
            patches.len(),
            1,
            "nested {{$IF}} must balance to one outer patch"
        );
        let o = patches[0].expect_opaque();
        assert!(o.text.starts_with("{$IF A}"));
        // The outer text contains both the inner {$IF B} and the final {$IFEND}.
        assert!(o.text.contains("{$IF B}"));
        assert!(o.text.contains("{$IFEND}"));
    }

    #[test]
    fn f_u6_if_with_ifend_closer() {
        let src = b"unit X;\ninterface\nimplementation\n\
                    {$IF A}\nrappel: developper en 32 bits pour plus de stabilite\n{$IFEND}\nend.\n";
        assert_eq!(opaque_patches_of(src).len(), 1);
    }

    #[test]
    fn f_u7_if_with_endif_closer() {
        // Delphi accepts {$ENDIF} as a closer for {$IF} as well as for {$IFDEF}.
        let src = b"unit X;\ninterface\nimplementation\n\
                    {$IF A}\nrappel: developper en 32 bits pour plus de stabilite\n{$ENDIF}\nend.\n";
        assert_eq!(opaque_patches_of(src).len(), 1);
    }

    #[test]
    fn f_u8_empty_body_is_not_patched() {
        let src = b"unit X;\ninterface\nimplementation\n\
                    {$IF DEFINED(X)}{$IFEND}\nend.\n";
        assert_eq!(opaque_patches_of(src).len(), 0);
    }

    #[test]
    fn f_u9_byte_offsets_preserved_after_rewrite() {
        let src = "unit X;\ninterface\nimplementation\n\
                   {$IF X}\nnon-pascal content\n{$IFEND}\nend.\n"
            .as_bytes();
        let rewritten = opaque_rewritten_of(src);
        assert_eq!(rewritten.len(), src.len(), "length preserved");

        // Newlines in the opaque span remain at the same byte offsets.
        for (i, b) in src.iter().enumerate() {
            if *b == b'\n' {
                assert_eq!(rewritten[i], b'\n', "newline at {i} preserved");
            }
            if *b == b'\r' {
                assert_eq!(rewritten[i], b'\r', "CR at {i} preserved");
            }
        }
    }
}
