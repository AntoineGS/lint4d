//! Content-preservation safety net for formatted output (l4d-um8g).
//!
//! fmt4d re-lays-out source text: it changes whitespace, line breaks and
//! indentation, normalises the `uses` keyword's case, reorders the units of
//! a `uses` clause without directives and moves comments relative to
//! separators. None of those
//! change the multiset of the source's tokens, where a token is
//!
//! - a comment (`// …` to the end of its line, `{ … }`, `(* … *)`, compiler
//!   directives included) or a string literal (`'…'` with `''` escapes, or a
//!   `#13` / `#$0D` character literal), compared exactly: code moved onto a
//!   `//` comment line changes that comment's text, and a string keeps its
//!   case and whitespace. Only line-ending `\r`s and the trailing whitespace
//!   of a `//` comment are not compared;
//! - a word (a run of alphanumeric characters or `_`) outside comments and
//!   strings, compared case-insensitively because Pascal is;
//! - any other non-whitespace character, compared exactly.
//!
//! A U+FEFF byte-order mark at the very start is not content, nor is a C0
//! control character (#1..#31) in code, a blank to the compiler. On top of the
//! multiset, the compiler directives (`{$…}`, `(*$…*)`) must keep their exact
//! order: dropping one `{$ENDIF}` and duplicating another changes the
//! conditional structure without changing the multiset. And the check is
//! positional: the directives split the input and the output into regions
//! (before the first, between two consecutive ones, after the last), and
//! each region of the output must hold the multiset of its input region,
//! so no code, comment or string crosses a directive (a unit sorted into a
//! `{$WARNINGS OFF}` stretch changes what the switch covers).
//!
//! Two rewrites do change the multiset, and the doc builder records each one
//! it emits as an [`AllowanceEntry`], applied in the region of the node:
//!
//! - expanding `A, B: T = V;` into one declaration per identifier drops the
//!   commas and repeats the `: T = V;` suffix (without its comments); the
//!   repeated tokens must come from that suffix in the input, and the
//!   suffix must hold no directive;
//! - laying out a `uses` clause rewrites the `,`/`;` separators between its
//!   items.
//!
//! Any other difference means the formatter dropped, duplicated, moved or
//! invented content, and the output must not reach a caller that writes it
//! back.
//!
//! The comparison is linear in the input and output sizes.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;

const MAX_REPORTED: usize = 5;
const MAX_SHOWN_CHARS: usize = 40;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Token<'a> {
    Word(&'a str),
    Char(char),
    /// A comment or compiler directive, as written.
    Comment(&'a str),
    /// A string or character literal, as written.
    Str(&'a str),
}

impl Token<'_> {
    fn is_directive(&self) -> bool {
        matches!(self, Token::Comment(text) if text.starts_with("{$") || text.starts_with("(*$"))
    }
}

/// A C0 control character (#1..#31), which the compiler, like the grammar,
/// reads as a blank outside comments and string literals.
fn is_code_blank_control(c: char) -> bool {
    matches!(c, '\u{1}'..='\u{1f}')
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The byte length of the string literal at the start of `rest` (which
/// starts with `'`): up to its closing quote, `''` being an escaped quote,
/// or up to the end of the line when it is unterminated.
fn quoted_len(rest: &str) -> usize {
    let bytes = rest.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' if bytes.get(i + 1) == Some(&b'\'') => i += 2,
            b'\'' => return i + 1,
            b'\n' | b'\r' => return i,
            _ => i += 1,
        }
    }
    bytes.len()
}

/// The byte length of the `#13` / `#$0D` character literal at the start of
/// `rest` (which starts with `#`), or 0 when `#` starts none.
fn char_literal_len(rest: &str) -> usize {
    let bytes = rest.as_bytes();
    let (start, digit): (usize, fn(&u8) -> bool) = if bytes.get(1) == Some(&b'$') {
        (2, u8::is_ascii_hexdigit)
    } else {
        (1, u8::is_ascii_digit)
    };
    let digits = bytes[start.min(bytes.len())..]
        .iter()
        .take_while(|b| digit(b))
        .count();
    if digits == 0 { 0 } else { start + digits }
}

/// The byte length of the decimal number at the start of `rest` (which
/// starts with a digit): its digits, and an exponent letter with the digits
/// after it, each run possibly holding `_` digit separators (`1_000`,
/// accepted by dcc 37.0). The compiler ends the number there, so `1do` is
/// `1` then `do`, which the formatter may separate. (Hexadecimal digits
/// after `$` stay one word with the letters after them.)
fn number_len(rest: &str) -> usize {
    let bytes = rest.as_bytes();
    let digits = |from: usize| {
        bytes[from.min(bytes.len())..]
            .iter()
            .take_while(|b| b.is_ascii_digit() || **b == b'_')
            .count()
    };
    let len = digits(0);
    match bytes.get(len) {
        Some(b'e' | b'E') => match bytes.get(len + 1) {
            Some(b) if b.is_ascii_digit() => len + 1 + digits(len + 1),
            Some(b'+' | b'-') => len + 1,
            _ => len,
        },
        _ => len,
    }
}

/// Calls `visit(token, line)` for every token of `text`, with 1-based lines
/// of the token's start.
fn for_each_token<'a>(text: &'a str, mut visit: impl FnMut(Token<'a>, usize)) {
    for_each_token_at(text, |token, line, _| visit(token, line));
}

/// Like [`for_each_token`], also passing the token's byte offset in `text`.
fn for_each_token_at<'a>(text: &'a str, mut visit: impl FnMut(Token<'a>, usize, usize)) {
    let base = if text.starts_with('\u{FEFF}') {
        '\u{FEFF}'.len_utf8()
    } else {
        0
    };
    let text = &text[base..];
    let mut line = 1;
    let mut offset = 0;
    while let Some(c) = text[offset..].chars().next() {
        let rest = &text[offset..];
        let (token, len) = if rest.starts_with("//") {
            let len = rest.find('\n').unwrap_or(rest.len());
            (Some(Token::Comment(rest[..len].trim_end())), len)
        } else if let Some(body) = rest.strip_prefix("(*") {
            let len = body.find("*)").map_or(rest.len(), |end| end + 4);
            (Some(Token::Comment(&rest[..len])), len)
        } else if c == '{' {
            let len = rest.find('}').map_or(rest.len(), |end| end + 1);
            (Some(Token::Comment(&rest[..len])), len)
        } else if c == '\'' {
            let len = quoted_len(rest);
            (Some(Token::Str(&rest[..len])), len)
        } else if c == '#' && char_literal_len(rest) > 0 {
            let len = char_literal_len(rest);
            (Some(Token::Str(&rest[..len])), len)
        } else if c.is_ascii_digit() && !text[..offset].ends_with('$') {
            let len = number_len(rest);
            (Some(Token::Word(&rest[..len])), len)
        } else if is_word_char(c) {
            let len = rest
                .char_indices()
                .find(|(_, next)| !is_word_char(*next))
                .map_or(rest.len(), |(index, _)| index);
            (Some(Token::Word(&rest[..len])), len)
        } else if c.is_whitespace() || is_code_blank_control(c) {
            (None, c.len_utf8())
        } else {
            (Some(Token::Char(c)), c.len_utf8())
        };
        if let Some(token) = token {
            visit(token, line, base + offset);
        }
        line += rest[..len].matches('\n').count();
        offset += len;
    }
}

/// Writes the comparison key of `token` into `key`: a kind tag, then the
/// folded word, the character, or the comment/string text without `\r`.
fn token_key(token: Token<'_>, key: &mut String) {
    key.clear();
    match token {
        Token::Word(word) => {
            key.push('w');
            key.extend(word.chars().flat_map(char::to_lowercase));
        }
        Token::Char(c) => {
            key.push('c');
            key.push(c);
        }
        Token::Comment(text) => {
            key.push('m');
            key.extend(text.chars().filter(|&c| c != '\r'));
        }
        Token::Str(text) => {
            key.push('s');
            key.push_str(text);
        }
    }
}

fn shown(token: Token<'_>) -> String {
    let text: Cow<'_, str> = match token {
        Token::Word(word) => word.into(),
        Token::Char(c) => c.escape_debug().to_string().into(),
        Token::Comment(text) | Token::Str(text) => text.into(),
    };
    let one_line = text.replace(['\r', '\n'], " ");
    if one_line.chars().count() > MAX_SHOWN_CHARS {
        let cut: String = one_line.chars().take(MAX_SHOWN_CHARS).collect();
        format!("`{cut}…`")
    } else {
        format!("`{one_line}`")
    }
}

#[derive(Default)]
struct Counts {
    counts: HashMap<String, i64>,
    key: String,
}

impl Counts {
    fn add(&mut self, token: Token<'_>, delta: i64) {
        token_key(token, &mut self.key);
        if let Some(count) = self.counts.get_mut(self.key.as_str()) {
            *count += delta;
        } else {
            self.counts.insert(self.key.clone(), delta);
        }
    }

    /// The surplus of `token` in the input over the output (negative when
    /// the output has more).
    fn surplus(&mut self, token: Token<'_>) -> i64 {
        token_key(token, &mut self.key);
        self.counts.get(self.key.as_str()).copied().unwrap_or(0)
    }

    fn clear(&mut self, token: Token<'_>) {
        token_key(token, &mut self.key);
        self.counts.remove(self.key.as_str());
    }

    fn is_balanced(&self) -> bool {
        self.counts.values().all(|count| *count == 0)
    }
}

fn append(summary: &mut String, part: &str) {
    if !summary.is_empty() {
        summary.push_str("; ");
    }
    summary.push_str(part);
}

/// Lists the first tokens of `text` whose count differs in the direction
/// `sign` (1: missing from the output, -1: extra in it): words, then
/// comments and strings, then other characters, each in source order with
/// the line of its first occurrence in `text` (the multiset cannot tell
/// which occurrence went missing).
fn describe(text: &str, counts: &mut Counts, sign: i64, label: &str, summary: &mut String) {
    let mut groups: [Vec<String>; 3] = Default::default();
    for_each_token(text, |token, line| {
        let count = counts.surplus(token) * sign;
        if count <= 0 {
            return;
        }
        counts.clear(token);
        let item = shown(token);
        let item = if count > 1 {
            format!("{item} x{count} (first at line {line})")
        } else {
            format!("{item} (line {line})")
        };
        let group = match token {
            Token::Word(_) => 0,
            Token::Comment(_) | Token::Str(_) => 1,
            Token::Char(_) => 2,
        };
        groups[group].push(item);
    });
    let total: usize = groups.iter().map(Vec::len).sum();
    if total == 0 {
        return;
    }
    let items: Vec<String> = groups.into_iter().flatten().take(MAX_REPORTED).collect();
    let mut part = format!("{label} {}", items.join(", "));
    if total > MAX_REPORTED {
        let _ = write!(part, " and {} more", total - MAX_REPORTED);
    }
    append(summary, &part);
}

fn directives(text: &str) -> Vec<(String, Token<'_>, usize)> {
    let mut found = Vec::new();
    for_each_token(text, |token, line| {
        if token.is_directive() {
            let mut key = String::new();
            token_key(token, &mut key);
            found.push((key, token, line));
        }
    });
    found
}

/// Describes where the ordered directives of `input` and `output` first
/// differ, if they do; returns whether they do.
fn describe_directive_order(input: &str, output: &str, summary: &mut String) -> bool {
    let before = directives(input);
    let after = directives(output);
    let first_difference = before
        .iter()
        .zip(&after)
        .position(|(b, a)| b.0 != a.0)
        .or_else(|| (before.len() != after.len()).then(|| before.len().min(after.len())));
    let Some(index) = first_difference else {
        return false;
    };
    let side = |list: &[(String, Token<'_>, usize)]| {
        list.get(index)
            .map_or("nothing".to_string(), |(_, token, line)| {
                format!("{} (line {line})", shown(*token))
            })
    };
    append(
        summary,
        &format!(
            "would change the order of compiler directives: directive {} is {} in the source \
             but {} in the output",
            index + 1,
            side(&before),
            side(&after)
        ),
    );
    true
}

/// Content the formatter knowingly drops from or adds to the source (see the
/// module documentation).
#[derive(Debug, Default)]
pub(crate) struct ContentAllowance {
    pub(crate) entries: Vec<AllowanceEntry>,
}

/// One rewrite's change to the content: the tokens of `dropped` and of
/// `added` (texts whose tokens are counted), in the region of the input
/// (between two directives) holding byte offset `at`.
#[derive(Debug, Clone, Default)]
pub(crate) struct AllowanceEntry {
    pub(crate) at: usize,
    pub(crate) dropped: String,
    pub(crate) added: String,
    /// The input bytes the added tokens repeat, and how many times: every
    /// added token must occur there that often, and the span must hold no
    /// directive, else the entry does not apply (code guarded by a
    /// directive must not be copied outside it).
    pub(crate) repeats: Option<(std::ops::Range<usize>, usize)>,
}

impl ContentAllowance {
    pub(crate) fn extend(&mut self, other: &ContentAllowance) {
        self.entries.extend(other.entries.iter().cloned());
    }
}

/// Whether `entry` may apply to `input`: what it adds comes from the span it
/// repeats, which holds no directive.
fn entry_applies(input: &str, entry: &AllowanceEntry) -> bool {
    let Some((span, copies)) = &entry.repeats else {
        return true;
    };
    let Some(source) = input.get(span.clone()) else {
        return false;
    };
    let mut available = Counts::default();
    let mut directive = false;
    for_each_token(source, |token, _| {
        directive |= token.is_directive();
        available.add(token, *copies as i64);
    });
    if directive {
        return false;
    }
    let mut fits = true;
    for_each_token(&entry.added, |token, _| {
        available.add(token, -1);
        fits &= available.surplus(token) >= 0;
    });
    fits
}

/// Counts the `,` and `;` of `text` outside comments, directives and string
/// literals (lexed as the check lexes them, `''` escapes included).
pub(crate) fn code_separators(text: &str) -> (usize, usize) {
    let (mut commas, mut semicolons) = (0, 0);
    for_each_token(text, |token, _| match token {
        Token::Char(',') => commas += 1,
        Token::Char(';') => semicolons += 1,
        _ => {}
    });
    (commas, semicolons)
}

/// Checks that `output` holds exactly the content of `input` but for the
/// `allowance` (see the module documentation), in the same regions between
/// compiler directives. On a difference, returns a short summary naming the
/// first lost tokens with their input lines, the first extra tokens with
/// their output lines, the first directive out of order, or the first token
/// moved across a directive.
pub(crate) fn check_content_preserved(
    input: &str,
    output: &str,
    allowance: &ContentAllowance,
) -> Result<(), String> {
    let mut counts = Counts::default();
    let mut regions = RegionCounts::default();
    let mut input_directives = Vec::new();
    for_each_token_at(input, |token, _, offset| {
        counts.add(token, 1);
        if token.is_directive() {
            input_directives.push(offset);
        } else {
            regions.add(input_directives.len(), token, 1);
        }
    });
    for entry in allowance.entries.iter().filter(|e| entry_applies(input, e)) {
        let region = input_directives.partition_point(|&d| d < entry.at);
        for_each_token(&entry.dropped, |token, _| {
            counts.add(token, -1);
            regions.add(region, token, -1);
        });
        for_each_token(&entry.added, |token, _| {
            counts.add(token, 1);
            regions.add(region, token, 1);
        });
    }
    let mut output_region = 0;
    for_each_token(output, |token, _| {
        counts.add(token, -1);
        if token.is_directive() {
            output_region += 1;
        } else {
            regions.add(output_region, token, -1);
        }
    });
    let mut summary = String::new();
    if !counts.is_balanced() {
        describe(input, &mut counts, 1, "would drop", &mut summary);
        describe(output, &mut counts, -1, "would add", &mut summary);
    }
    let reordered = describe_directive_order(input, output, &mut summary);
    if summary.is_empty() && !reordered && !regions.is_balanced() {
        describe_move(input, output, &mut regions, &mut summary);
    }
    if summary.is_empty() {
        Ok(())
    } else {
        Err(summary)
    }
}

/// Token counts per region: the code, comments and strings between two
/// consecutive compiler directives (or before the first, or after the
/// last), numbered in order.
#[derive(Default)]
struct RegionCounts {
    counts: HashMap<(usize, String), i64>,
    key: String,
}

impl RegionCounts {
    fn add(&mut self, region: usize, token: Token<'_>, delta: i64) {
        token_key(token, &mut self.key);
        *self
            .counts
            .entry((region, std::mem::take(&mut self.key)))
            .or_insert(0) += delta;
    }

    fn surplus(&mut self, region: usize, token: Token<'_>) -> i64 {
        token_key(token, &mut self.key);
        self.counts
            .get(&(region, std::mem::take(&mut self.key)))
            .copied()
            .unwrap_or(0)
    }

    fn is_balanced(&self) -> bool {
        self.counts.values().all(|count| *count == 0)
    }
}

/// Names the first token of `input` missing from its region in `output`
/// (else the first token of `output` extra in its region): with the same
/// tokens and the same directives overall, it crossed a directive.
fn describe_move(input: &str, output: &str, regions: &mut RegionCounts, summary: &mut String) {
    for (text, sign) in [(input, 1), (output, -1)] {
        let mut region = 0;
        let mut found = None;
        for_each_token(text, |token, line| {
            if found.is_some() {
                return;
            }
            if token.is_directive() {
                region += 1;
            } else if regions.surplus(region, token) * sign > 0 {
                found = Some((shown(token), line));
            }
        });
        if let Some((token, line)) = found {
            let side = if sign > 0 { "source" } else { "output" };
            append(
                summary,
                &format!("would move {token} ({side} line {line}) across a compiler directive"),
            );
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AllowanceEntry, ContentAllowance, check_content_preserved, code_separators};

    fn check(input: &str, output: &str) -> Result<(), String> {
        check_content_preserved(input, output, &ContentAllowance::default())
    }

    #[test]
    fn layout_case_and_order_changes_are_tolerated() {
        let input = "\u{FEFF}unit T;\r\nUSES  B,A; // note  \r\n{$IFDEF X}x:=1;{$ENDIF}\r\n";
        let output = "unit T;\n\nuses\n  A, // note\n  B;\n{$IFDEF X}\nx := 1;\n{$ENDIF}\n";
        assert_eq!(check(input, output), Ok(()));
    }

    #[test]
    fn lost_words_are_reported_with_their_input_lines() {
        let input = "unit T;\n{$REGION 'A'}\ntype X = Integer;\n{$ENDREGION}\n";
        let output = "unit T;\ntype X = Integer;\n";
        let summary = check(input, output).unwrap_err();
        assert_eq!(
            summary,
            "would drop `{$REGION 'A'}` (line 2), `{$ENDREGION}` (line 4); \
             would change the order of compiler directives: directive 1 is \
             `{$REGION 'A'}` (line 2) in the source but nothing in the output"
        );
    }

    #[test]
    fn lost_punctuation_is_refused() {
        let summary = check("a := 1;\nb := 2;\n", "a := 1\nb := 2;\n").unwrap_err();
        assert_eq!(summary, "would drop `;` (line 1)");
    }

    #[test]
    fn duplicated_content_is_refused() {
        let summary = check("x; // c\n", "x; // c\n// c\n").unwrap_err();
        assert_eq!(summary, "would add `// c` (line 1)");
    }

    #[test]
    fn non_ascii_words_compare_case_insensitively() {
        assert_eq!(check("Éa := 1;", "éA := 1;"), Ok(()));
        assert!(check("// café", "// caf").is_err());
    }

    #[test]
    fn allowance_covers_a_var_list_expansion() {
        let input = "var A, B: Integer = 1; // n\n";
        let output = "var\n  A: Integer = 1;\n  B: Integer = 1; // n\n";
        assert!(check(input, output).is_err());
        let suffix = input.find(':').unwrap()..input.find(';').unwrap() + 1;
        let allowance = |added: &str| ContentAllowance {
            entries: vec![AllowanceEntry {
                at: 0,
                dropped: ",".into(),
                added: added.into(),
                repeats: Some((suffix.clone(), 1)),
            }],
        };
        assert_eq!(
            check_content_preserved(input, output, &allowance(": Integer = 1;")),
            Ok(())
        );
        // Added tokens must come from the repeated span.
        assert!(check_content_preserved(input, output, &allowance(": Integer = 2;")).is_err());
    }

    #[test]
    fn code_separators_skip_comments_directives_and_strings() {
        assert_eq!(
            code_separators("A, B; // x, y;\n{ a, b; } (* c; *) {$I a,b;} 'p;q', C;"),
            (2, 2)
        );
        assert_eq!(code_separators("S := 'it''s; a, b'#13';', T;"), (1, 1));
    }

    #[test]
    fn a_dropped_and_a_duplicated_directive_are_refused() {
        let input = "{$IFDEF X}a;{$ENDIF}\n{$IFDEF Y}b;{$ENDIF}\n";
        let output = "{$IFDEF X}a;\n{$IFDEF Y}b;{$ENDIF}{$ENDIF}\n";
        assert_eq!(
            check(input, output).unwrap_err(),
            "would change the order of compiler directives: directive 2 is \
             `{$ENDIF}` (line 1) in the source but `{$IFDEF Y}` (line 2) in the output"
        );
    }

    #[test]
    fn code_moved_into_a_line_comment_is_refused() {
        assert_eq!(
            check("if A // c\nthen B;\n", "if A // c then\nB;\n").unwrap_err(),
            "would drop `then` (line 2), `// c` (line 1); would add `// c then` (line 1)"
        );
        assert!(check("X := A // c\n;\n", "X := A // c;\n").is_err());
    }

    #[test]
    fn code_moved_across_a_directive_is_refused() {
        // Review of de1f3c6: the multiset and the directive order were
        // intact, but units changed sides of the switches.
        let input =
            "uses Windows, {$WARN UNIT_PLATFORM OFF} FileCtrl, {$WARN UNIT_PLATFORM ON} Classes;";
        let output =
            "uses Classes, {$WARN UNIT_PLATFORM OFF} FileCtrl, {$WARN UNIT_PLATFORM ON} Windows;";
        assert!(check(input, output).is_err());
        let input = "uses SysUtils, Forms, {$WARNINGS OFF}OleAuto{$WARNINGS ON}, mscorlib_tlb;";
        let output = "uses {$WARNINGS OFF}Forms, mscorlib_tlb, OleAuto, {$WARNINGS ON}SysUtils;";
        assert!(check(input, output).is_err());
        // A comment changing sides of a directive is refused too.
        assert!(
            check(
                "{$IFDEF X} // c\nA;\n{$ENDIF}",
                "// c\n{$IFDEF X}\nA;\n{$ENDIF}"
            )
            .is_err()
        );
        // Layout within each region is fine.
        assert_eq!(
            check(
                "uses A,{$IFDEF X}B,{$ENDIF}C;",
                "uses\n  A,\n  {$IFDEF X}\n  B,\n  {$ENDIF}\n  C;"
            ),
            Ok(())
        );
    }

    #[test]
    fn a_define_reordered_in_a_uses_clause_is_refused() {
        let input = "uses Zed, {$DEFINE FOO} Alpha, {$UNDEF FOO} Mid;";
        let output = "uses Alpha, {$UNDEF FOO} Mid, Zed {$DEFINE FOO};";
        assert!(check(input, output).is_err());
    }

    #[test]
    fn a_var_list_allowance_does_not_carry_code_across_a_directive() {
        // The repeated `absolute X` came from inside `{$IFDEF Q}`.
        let input = "var A, B: Integer {$IFDEF Q} absolute X {$ENDIF};";
        let output = "var A: Integer absolute X;\n  B: Integer {$IFDEF Q} absolute X {$ENDIF};";
        let suffix = input.find(':').unwrap()..input.rfind(';').unwrap() + 1;
        let allowance = ContentAllowance {
            entries: vec![AllowanceEntry {
                at: 0,
                dropped: ",".into(),
                added: ": Integer absolute X;".into(),
                repeats: Some((suffix, 1)),
            }],
        };
        assert!(check_content_preserved(input, output, &allowance).is_err());
    }

    #[test]
    fn control_characters_in_code_are_blanks_but_exact_in_comments_and_strings() {
        // The compiler reads C0 controls outside comments and strings as
        // blanks: dropping one from code is not content loss.
        assert_eq!(check("X := 1; \u{16}\nY := 2;", "X := 1;\nY := 2;"), Ok(()));
        assert_eq!(check("A :=\u{1c}B;", "A := B;"), Ok(()));
        assert!(check("X := 1; // a\u{16}b\n", "X := 1; // ab\n").is_err());
        assert!(check("X := 1; { a\u{16}b }", "X := 1; { ab }").is_err());
        assert!(check("X := 'a\u{16}b';", "X := 'ab';").is_err());
    }

    #[test]
    fn digit_separators_are_part_of_the_number() {
        // dcc 37.0 accepts `1_000`, `%1010_1010`, `1e1_0`, `1_e1`.
        assert_eq!(check("X := 1_000;", "X := 1_000;"), Ok(()));
        assert!(check("X := 1_000;", "X := 1 _000;").is_err());
        assert!(check("X := %1010_1010;", "X := %1010 _1010;").is_err());
        assert!(check("X := 1e1_0;", "X := 1e1 _0;").is_err());
        assert_eq!(check("X := 1_000do", "X := 1_000 do"), Ok(()));
    }

    #[test]
    fn a_number_glued_to_a_keyword_may_be_separated() {
        // `1do` is the number `1` then `do` to the compiler.
        assert_eq!(
            check("for I := 0 to 1do X;", "for I := 0 to 1 do X;"),
            Ok(())
        );
        assert_eq!(check("if A = 1then B;", "if A = 1 then B;"), Ok(()));
        assert_eq!(check("X := 1e5;", "X := 1e5;"), Ok(()));
        assert!(check("X := 1e5;", "X := 1 e5;").is_err());
        assert!(check("X := $1F;", "X := $1 F;").is_err());
        assert!(check("X := 12;", "X := 1 2;").is_err());
    }

    #[test]
    fn string_case_change_is_refused() {
        assert!(check("S := 'Hello';", "S := 'hello';").is_err());
    }

    #[test]
    fn string_whitespace_change_is_refused() {
        assert!(check("S := 'a  b';", "S := 'a b';").is_err());
        assert!(check("S := 'it''s';", "S := 'it'' s';").is_err());
        assert_eq!(
            check("S := 'a'#13#$0A'b';", "S := 'a' #13 #$0A 'b';"),
            Ok(())
        );
    }

    #[test]
    fn byte_order_mark_is_ignored_only_at_the_start() {
        assert_eq!(check("\u{FEFF}S := 'a';", "S := 'a';"), Ok(()));
        assert!(check("S := 'a\u{FEFF}';", "S := 'a';").is_err());
    }
}
