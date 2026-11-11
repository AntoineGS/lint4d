//! Content-preservation safety net for formatted output (l4d-um8g).
//!
//! fmt4d re-lays-out source text: it changes whitespace, line breaks and
//! indentation, normalises the `uses` keyword's case, reorders the units of
//! a `uses` clause and moves comments relative to separators. None of those
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
//! A U+FEFF byte-order mark at the very start is not content. On top of the
//! multiset, the compiler directives (`{$…}`, `(*$…*)`) must keep their exact
//! order: dropping one `{$ENDIF}` and duplicating another changes the
//! conditional structure without changing the multiset.
//!
//! Two rewrites do change the multiset, and the doc builder records each one
//! it emits as a [`ContentAllowance`]:
//!
//! - expanding `A, B: T = V;` into one declaration per identifier drops the
//!   commas and repeats the `: T = V;` suffix (without its comments);
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
/// after it. The compiler ends the number there, so `1do` is `1` then `do`,
/// which the formatter may separate. (Hexadecimal digits after `$` stay one
/// word with the letters after them.)
fn number_len(rest: &str) -> usize {
    let bytes = rest.as_bytes();
    let digits = |from: usize| {
        bytes[from.min(bytes.len())..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
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
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
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
        } else if c.is_whitespace() {
            (None, c.len_utf8())
        } else {
            (Some(Token::Char(c)), c.len_utf8())
        };
        if let Some(token) = token {
            visit(token, line);
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

/// The directives of `input` in the order the output must hold them: the
/// source order, with each run a [`DirectiveRewrite`] covers replaced by the
/// directives it records for the output. A rewrite applies to the first run
/// of exactly its source directives that starts on or after its line; one
/// that matches no run changes nothing. Each directive is paired with how a
/// summary shows it.
fn expected_directives(input: &str, rewrites: &[DirectiveRewrite]) -> Vec<(String, String)> {
    let shown_at = |token: Token<'_>, line: usize, side: &str| {
        format!("{} ({side} line {line})", shown(token))
    };
    let before = directives(input);
    let mut rewrites: Vec<&DirectiveRewrite> = rewrites.iter().collect();
    rewrites.sort_by_key(|r| r.first_line);
    let mut expected = Vec::new();
    let mut next = 0;
    for rewrite in rewrites {
        let source: Vec<String> = directives(&rewrite.source)
            .into_iter()
            .map(|(key, _, _)| key)
            .collect();
        if source.is_empty() {
            continue;
        }
        let found = (next..before.len()).find(|&start| {
            before[start].2 >= rewrite.first_line
                && before.len() - start >= source.len()
                && before[start..start + source.len()]
                    .iter()
                    .zip(&source)
                    .all(|(b, s)| b.0 == *s)
        });
        let Some(start) = found else {
            continue;
        };
        expected.extend(
            before[next..start]
                .iter()
                .map(|(key, token, line)| (key.clone(), shown_at(*token, *line, "source"))),
        );
        expected.extend(
            directives(&rewrite.output)
                .into_iter()
                .map(|(key, token, _)| {
                    let line = before[start].2;
                    (
                        key,
                        format!("{} (rewritten from source line {line})", shown(token)),
                    )
                }),
        );
        next = start + source.len();
    }
    expected.extend(
        before[next..]
            .iter()
            .map(|(key, token, line)| (key.clone(), shown_at(*token, *line, "source"))),
    );
    expected
}

/// Describes where the ordered directives of `output` first differ from the
/// ones expected from `input` (see [`expected_directives`]), if they do.
fn describe_directive_order(
    input: &str,
    output: &str,
    rewrites: &[DirectiveRewrite],
    summary: &mut String,
) {
    let before = expected_directives(input, rewrites);
    let after = directives(output);
    let first_difference = before
        .iter()
        .zip(&after)
        .position(|(b, a)| b.0 != a.0)
        .or_else(|| (before.len() != after.len()).then(|| before.len().min(after.len())));
    let Some(index) = first_difference else {
        return;
    };
    let expected = before
        .get(index)
        .map_or("nothing".to_string(), |(_, shown)| {
            shown.replace(" (source line ", " (line ")
        });
    let actual = after
        .get(index)
        .map_or("nothing".to_string(), |(_, token, line)| {
            format!("{} (line {line})", shown(*token))
        });
    append(
        summary,
        &format!(
            "would change the order of compiler directives: directive {} is {expected} in the \
             source but {actual} in the output",
            index + 1,
        ),
    );
}

/// Content the formatter knowingly drops from or adds to the source (see the
/// module documentation), as texts whose tokens are counted.
#[derive(Debug, Default)]
pub(crate) struct ContentAllowance {
    pub(crate) dropped: String,
    pub(crate) added: String,
    /// Runs of directives the output holds in another order or repeated.
    pub(crate) directive_rewrites: Vec<DirectiveRewrite>,
}

/// A rewrite that changes the order of directives: the directives of the
/// source text `source`, starting on 1-based line `first_line` of the input,
/// are emitted as the directives of `output`, in that order.
#[derive(Debug, Clone)]
pub(crate) struct DirectiveRewrite {
    pub(crate) first_line: usize,
    pub(crate) source: String,
    pub(crate) output: String,
}

impl ContentAllowance {
    pub(crate) fn extend(&mut self, other: &ContentAllowance) {
        self.dropped.push(' ');
        self.dropped.push_str(&other.dropped);
        self.added.push(' ');
        self.added.push_str(&other.added);
        self.directive_rewrites
            .extend(other.directive_rewrites.iter().cloned());
    }
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
/// `allowance` (see the module documentation). On a difference, returns a
/// short summary naming the first lost tokens with their input lines, the
/// first extra tokens with their output lines, and the first directive out
/// of order.
pub(crate) fn check_content_preserved(
    input: &str,
    output: &str,
    allowance: &ContentAllowance,
) -> Result<(), String> {
    let mut counts = Counts::default();
    for_each_token(input, |token, _| counts.add(token, 1));
    for_each_token(&allowance.dropped, |token, _| counts.add(token, -1));
    for_each_token(&allowance.added, |token, _| counts.add(token, 1));
    for_each_token(output, |token, _| counts.add(token, -1));
    let mut summary = String::new();
    if !counts.is_balanced() {
        describe(input, &mut counts, 1, "would drop", &mut summary);
        describe(output, &mut counts, -1, "would add", &mut summary);
    }
    describe_directive_order(input, output, &allowance.directive_rewrites, &mut summary);
    if summary.is_empty() {
        Ok(())
    } else {
        Err(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::{ContentAllowance, check_content_preserved, code_separators};

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
        let allowance = ContentAllowance {
            dropped: ",".into(),
            added: ": Integer = 1;".into(),
            ..ContentAllowance::default()
        };
        assert_eq!(check_content_preserved(input, output, &allowance), Ok(()));
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
    fn a_recorded_directive_rewrite_allows_exactly_its_order() {
        use super::DirectiveRewrite;
        let input = "{$A}\nuses {$IFDEF Y} Y, {$ENDIF} {$IFDEF X} X, {$ENDIF};\n{$B}\n";
        let output = "{$A}\nuses {$IFDEF X} X, {$ENDIF} {$IFDEF Y} Y, {$ENDIF};\n{$B}\n";
        assert!(check(input, output).is_err());
        let rewrite = |first_line| ContentAllowance {
            directive_rewrites: vec![DirectiveRewrite {
                first_line,
                source: "uses {$IFDEF Y} Y, {$ENDIF} {$IFDEF X} X, {$ENDIF};".into(),
                output: "{$IFDEF X}\nX,\n{$ENDIF}\n{$IFDEF Y}\nY,\n{$ENDIF}".into(),
            }],
            ..ContentAllowance::default()
        };
        assert_eq!(check_content_preserved(input, output, &rewrite(2)), Ok(()));
        // A rewrite anchored after its run does not apply.
        assert!(check_content_preserved(input, output, &rewrite(3)).is_err());
        // It allows no other order change.
        let moved_b = "{$B}\nuses {$IFDEF X} X, {$ENDIF} {$IFDEF Y} Y, {$ENDIF};\n{$A}\n";
        assert!(check_content_preserved(input, moved_b, &rewrite(2)).is_err());
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
