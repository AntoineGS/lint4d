//! Content-preservation safety net for formatted output (l4d-um8g).
//!
//! fmt4d re-lays-out source text: it changes whitespace, line breaks and
//! indentation, normalises the `uses` keyword's case, reorders the units of
//! a `uses` clause and moves comments relative to separators. None of those
//! change the multiset of the source's non-whitespace tokens, where a token
//! is a word (a run of alphanumeric characters or `_`, compared
//! case-insensitively because Pascal is) or a single other non-whitespace
//! character (compared exactly). A U+FEFF byte-order mark is not content.
//!
//! Two rewrites do change that multiset, and the doc builder records each
//! one it makes as a [`ContentAllowance`]:
//!
//! - expanding `A, B: T = V;` into one declaration per identifier drops the
//!   commas and repeats the `: T = V;` suffix (without its comments);
//! - laying out a `uses` clause rewrites the `,`/`;` separators between its
//!   items.
//!
//! Any other difference means the formatter dropped, duplicated or invented
//! content, and the output must not reach a caller that writes it back.
//!
//! The comparison is linear in the input and output sizes.

use std::collections::HashMap;
use std::fmt::Write as _;

const MAX_REPORTED: usize = 5;

#[derive(Clone, Copy)]
enum Token<'a> {
    Word(&'a str),
    Char(char),
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Calls `visit(token, line)` for every token of `text`, with 1-based lines.
fn for_each_token<'a>(text: &'a str, mut visit: impl FnMut(Token<'a>, usize)) {
    let mut line = 1;
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if c == '\n' {
            line += 1;
        } else if is_word_char(c) {
            let mut end = start + c.len_utf8();
            while let Some(&(index, next)) = chars.peek() {
                if !is_word_char(next) {
                    break;
                }
                end = index + next.len_utf8();
                chars.next();
            }
            visit(Token::Word(&text[start..end]), line);
        } else if !c.is_whitespace() && c != '\u{FEFF}' {
            visit(Token::Char(c), line);
        }
    }
}

fn fold_word(word: &str, buffer: &mut String) {
    buffer.clear();
    buffer.extend(word.chars().flat_map(char::to_lowercase));
}

#[derive(Default)]
struct Counts {
    words: HashMap<String, i64>,
    chars: HashMap<char, i64>,
    buffer: String,
}

impl Counts {
    fn add(&mut self, token: Token<'_>, delta: i64) {
        match token {
            Token::Word(word) => {
                fold_word(word, &mut self.buffer);
                if let Some(count) = self.words.get_mut(self.buffer.as_str()) {
                    *count += delta;
                } else {
                    self.words.insert(self.buffer.clone(), delta);
                }
            }
            Token::Char(c) => *self.chars.entry(c).or_insert(0) += delta,
        }
    }

    /// The surplus of `token` in the input over the output (negative when
    /// the output has more).
    fn surplus(&mut self, token: Token<'_>) -> i64 {
        match token {
            Token::Word(word) => {
                fold_word(word, &mut self.buffer);
                self.words.get(self.buffer.as_str()).copied().unwrap_or(0)
            }
            Token::Char(c) => self.chars.get(&c).copied().unwrap_or(0),
        }
    }

    fn clear(&mut self, token: Token<'_>) {
        match token {
            Token::Word(word) => {
                fold_word(word, &mut self.buffer);
                self.words.remove(self.buffer.as_str());
            }
            Token::Char(c) => {
                self.chars.remove(&c);
            }
        }
    }

    fn is_balanced(&self) -> bool {
        self.words.values().all(|count| *count == 0) && self.chars.values().all(|count| *count == 0)
    }
}

/// Lists the first tokens of `text` whose count differs in the direction
/// `sign` (1: missing from the output, -1: extra in it), words before other
/// characters, each in source order with the line of its first occurrence in
/// `text` (the multiset cannot tell which occurrence went missing).
fn describe(text: &str, counts: &mut Counts, sign: i64, label: &str, summary: &mut String) {
    let mut words = Vec::new();
    let mut chars = Vec::new();
    for_each_token(text, |token, line| {
        let count = counts.surplus(token) * sign;
        if count <= 0 {
            return;
        }
        counts.clear(token);
        let item = match token {
            Token::Word(word) => format!("`{word}`"),
            Token::Char(c) => format!("`{}`", c.escape_debug()),
        };
        let item = if count > 1 {
            format!("{item} x{count} (first at line {line})")
        } else {
            format!("{item} (line {line})")
        };
        match token {
            Token::Word(_) => words.push(item),
            Token::Char(_) => chars.push(item),
        }
    });
    let total = words.len() + chars.len();
    if total == 0 {
        return;
    }
    if !summary.is_empty() {
        summary.push_str("; ");
    }
    let shown: Vec<String> = words.into_iter().chain(chars).take(MAX_REPORTED).collect();
    let _ = write!(summary, "{label} {}", shown.join(", "));
    if total > MAX_REPORTED {
        let _ = write!(summary, " and {} more", total - MAX_REPORTED);
    }
}

/// Content the formatter knowingly drops from or adds to the source (see the
/// module documentation), as texts whose tokens are counted.
#[derive(Debug, Default)]
pub(crate) struct ContentAllowance {
    pub(crate) dropped: String,
    pub(crate) added: String,
}

impl ContentAllowance {
    pub(crate) fn extend(&mut self, other: &ContentAllowance) {
        self.dropped.push(' ');
        self.dropped.push_str(&other.dropped);
        self.added.push(' ');
        self.added.push_str(&other.added);
    }
}

/// Counts the `,` and `;` of `text` outside comments, directives and string
/// literals.
pub(crate) fn code_separators(text: &str) -> (usize, usize) {
    let (mut commas, mut semicolons) = (0, 0);
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let skip = if rest.starts_with("//") {
            rest.find('\n').unwrap_or(rest.len())
        } else if let Some(body) = rest.strip_prefix("(*") {
            body.find("*)").map_or(rest.len(), |end| end + 4)
        } else if c == '{' {
            rest.find('}').map_or(rest.len(), |end| end + 1)
        } else if let Some(body) = rest.strip_prefix('\'') {
            body.find('\'').map_or(rest.len(), |end| end + 2)
        } else {
            match c {
                ',' => commas += 1,
                ';' => semicolons += 1,
                _ => {}
            }
            c.len_utf8()
        };
        rest = &rest[skip..];
    }
    (commas, semicolons)
}

/// Checks that `output` holds exactly the content of `input` but for the
/// `allowance` (see the module documentation). On a difference, returns a
/// short summary naming the first lost tokens with their input lines and the
/// first extra tokens with their output lines.
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
    if counts.is_balanced() {
        return Ok(());
    }
    let mut summary = String::new();
    describe(input, &mut counts, 1, "would drop", &mut summary);
    describe(output, &mut counts, -1, "would add", &mut summary);
    Err(summary)
}

#[cfg(test)]
mod tests {
    use super::{ContentAllowance, check_content_preserved, code_separators};

    fn check(input: &str, output: &str) -> Result<(), String> {
        check_content_preserved(input, output, &ContentAllowance::default())
    }

    #[test]
    fn layout_case_and_order_changes_are_tolerated() {
        let input = "\u{FEFF}unit T;\r\nUSES  B,A; // note\r\n{$IFDEF X}x:=1;{$ENDIF}\r\n";
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
            "would drop `REGION` (line 2), `A` (line 2), `ENDREGION` (line 4), \
             `{` x2 (first at line 2), `$` x2 (first at line 2) and 2 more"
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
        assert_eq!(summary, "would add `c` (line 1), `/` x2 (first at line 1)");
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
        };
        assert_eq!(check_content_preserved(input, output, &allowance), Ok(()));
    }

    #[test]
    fn code_separators_skip_comments_directives_and_strings() {
        assert_eq!(
            code_separators("A, B; // x, y;\n{ a, b; } (* c; *) {$I a,b;} 'p;q', C;"),
            (2, 2)
        );
    }
}
