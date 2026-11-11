/// Intermediate representation for formatted output.
///
/// The AST-to-Doc builder produces a tree of `Doc` nodes. A separate renderer
/// walks this tree to produce the final string, handling spacing, indentation,
/// and line-break decisions.
#[derive(Debug, Clone)]
pub enum Doc {
    /// A source token — carries kind metadata for spacing resolution.
    ///
    /// `kind` and `parent_kind` reference `&'static str` constants from
    /// `pascal_core::node_kind` (all grammar node kinds are compile-time
    /// constants), avoiding per-token heap allocation.
    Token {
        text: String,
        kind: &'static str,
        parent_kind: &'static str,
    },

    /// Pre-formatted text (comments, verbatim regions) — no spacing logic.
    Raw(String),

    /// Start a line for verbatim text that starts a line in the source: a
    /// newline unless already at line start, then this source indentation.
    LineStart(String),

    /// Always a newline.
    Hardline,

    /// Preserved blank line from source (hardline + hardline).
    BlankLine,

    /// In flat mode: space. In break mode: newline + current indent.
    Line,

    /// In flat mode: nothing. In break mode: newline + current indent.
    Softline,

    /// Renders identically to `Line` (space in flat, newline in break),
    /// but the `Fill` handler treats it as a forced break — preserving
    /// the author's intentional line break while still allowing a parent
    /// `Group` to join everything onto one line when it fits.
    PreservedLine,

    /// Sequence of docs.
    Concat(Vec<Doc>),

    /// Adds one indent level to its contents.
    Indent(Box<Doc>),

    /// Tries to fit contents on one line (flat mode).
    /// If it doesn't fit, all Line/Softline inside switch to break mode.
    Group(Box<Doc>),

    /// Different content depending on enclosing group's break mode.
    #[allow(dead_code)] // Phase F follow-up: builder integration pending.
    IfBreak { broken: Box<Doc>, flat: Box<Doc> },

    /// Greedy line-filling for binary chains.
    ///
    /// Parts alternate: `[sep_0, content_0, sep_1, content_1, ...]`
    /// where separators are `Line` (greedy decision) or `Hardline` (forced
    /// break).  The renderer processes pairs greedily: if `sep + content`
    /// fits on the current line, render sep flat (space); otherwise render
    /// sep as break (newline + indent).
    Fill(Vec<Doc>),

    /// Renderer-internal: the not-yet-rendered tail of a `Fill`, drained
    /// through an iterator so each pair is taken in O(1). Builders never
    /// produce this variant.
    #[doc(hidden)]
    FillRest(std::vec::IntoIter<Doc>),

    /// A group of rows that should be column-aligned.
    /// The renderer pre-calculates column widths across all rows
    /// before rendering with padding.
    AlignGroup(Vec<Doc>),

    /// A single row within an AlignGroup. Each cell is rendered
    /// normally but padded to the group's resolved column width.
    AlignRow(Vec<AlignCell>),

    /// Identity element — produces no output.
    Empty,

    /// Produces no output, like `Empty`: marks that the content-check
    /// allowance recorded for the node of this id applies, because the doc
    /// holding the mark is emitted.
    ContentMark(usize),
}

/// A single cell in an alignment row.
#[derive(Debug, Clone)]
pub struct AlignCell {
    /// The content of this cell.
    pub content: Doc,
    /// If true, this cell is right-padded to the column width.
    /// If false, rendered as-is (typically the last cell).
    pub pad: bool,
}

/// Convenience: concatenate a list of Docs, filtering out Empty.
pub fn concat(docs: Vec<Doc>) -> Doc {
    let filtered: Vec<Doc> = docs
        .into_iter()
        .filter(|d| !matches!(d, Doc::Empty))
        .collect();
    match filtered.len() {
        0 => Doc::Empty,
        1 => filtered.into_iter().next().unwrap(),
        _ => Doc::Concat(filtered),
    }
}

pub fn group(doc: Doc) -> Doc {
    Doc::Group(Box::new(doc))
}

pub fn indent(doc: Doc) -> Doc {
    Doc::Indent(Box::new(doc))
}

#[allow(dead_code)] // Phase F follow-up: builder integration pending.
pub fn if_break(broken: Doc, flat: Doc) -> Doc {
    Doc::IfBreak {
        broken: Box::new(broken),
        flat: Box::new(flat),
    }
}

pub fn fill(docs: Vec<Doc>) -> Doc {
    Doc::Fill(docs)
}

pub fn align_group(rows: Vec<Doc>) -> Doc {
    Doc::AlignGroup(rows)
}

pub fn align_row(cells: Vec<AlignCell>) -> Doc {
    Doc::AlignRow(cells)
}

pub fn align_cell(content: Doc, pad: bool) -> AlignCell {
    AlignCell { content, pad }
}

pub fn token(text: impl Into<String>, kind: &'static str, parent_kind: &'static str) -> Doc {
    Doc::Token {
        text: text.into(),
        kind,
        parent_kind,
    }
}

/// The text `doc` emits, its pieces separated by spaces: the same tokens as
/// the rendered output, whatever its layout. Of an `IfBreak`, the broken
/// branch.
pub fn text_of(doc: &Doc) -> String {
    text_joined(doc, ' ')
}

/// The text of `doc`'s tokens and raw pieces, one per line, so that a `//`
/// comment in one piece does not run into the next.
pub fn text_lines_of(doc: &Doc) -> String {
    text_joined(doc, '\n')
}

fn text_joined(doc: &Doc, separator: char) -> String {
    fn collect(doc: &Doc, separator: char, out: &mut String) {
        match doc {
            Doc::Token { text, .. } | Doc::Raw(text) | Doc::LineStart(text) => {
                out.push(separator);
                out.push_str(text);
            }
            Doc::Concat(docs) | Doc::Fill(docs) | Doc::AlignGroup(docs) => {
                docs.iter().for_each(|d| collect(d, separator, out));
            }
            Doc::Indent(inner) | Doc::Group(inner) => collect(inner, separator, out),
            Doc::IfBreak { broken, .. } => collect(broken, separator, out),
            Doc::FillRest(rest) => rest
                .as_slice()
                .iter()
                .for_each(|d| collect(d, separator, out)),
            Doc::AlignRow(cells) => cells
                .iter()
                .for_each(|c| collect(&c.content, separator, out)),
            Doc::Hardline
            | Doc::BlankLine
            | Doc::Line
            | Doc::Softline
            | Doc::PreservedLine
            | Doc::ContentMark(_)
            | Doc::Empty => {}
        }
    }
    let mut out = String::new();
    collect(doc, separator, &mut out);
    out
}

/// Whether `doc` always renders on more than one line: a forced line break
/// between two of its texts, or text spanning lines (a multi-line comment or
/// string). Breaks a group may or may not take do not count, so the answer
/// does not depend on the line width; nor do breaks before its first text
/// or after its last, or comments and directives laid out on their own
/// lines.
pub fn has_forced_break(doc: &Doc) -> bool {
    /// Walks `doc` in order; `state` is 0 before any text, 1 after text,
    /// 2 after text then a break. Returns true on text in state 2.
    fn walk(doc: &Doc, state: &mut u8) -> bool {
        match doc {
            Doc::Hardline | Doc::BlankLine | Doc::LineStart(_) => {
                if *state == 1 {
                    *state = 2;
                }
                false
            }
            // A comment or directive on its own line (leading trivia) is not
            // the declaration's own text.
            Doc::Token { kind, .. }
                if *kind == pascal_core::node_kind::COMMENT
                    || *kind == pascal_core::node_kind::PP_DIRECTIVE =>
            {
                false
            }
            Doc::Token { text, .. } | Doc::Raw(text) => {
                if text.trim().is_empty() {
                    return false;
                }
                if *state == 2 || text.trim().contains('\n') {
                    return true;
                }
                *state = if text.ends_with('\n') { 2 } else { 1 };
                false
            }
            Doc::Concat(docs) | Doc::Fill(docs) | Doc::AlignGroup(docs) => {
                docs.iter().any(|d| walk(d, state))
            }
            Doc::Indent(inner) | Doc::Group(inner) => walk(inner, state),
            Doc::IfBreak { flat, .. } => walk(flat, state),
            Doc::FillRest(rest) => rest.as_slice().iter().any(|d| walk(d, state)),
            Doc::AlignRow(cells) => cells.iter().any(|c| walk(&c.content, state)),
            Doc::Line | Doc::Softline | Doc::PreservedLine | Doc::ContentMark(_) | Doc::Empty => {
                false
            }
        }
    }
    walk(doc, &mut 0)
}

/// The ids of every [`Doc::ContentMark`] in `doc`.
pub fn content_marks(doc: &Doc) -> std::collections::HashSet<usize> {
    let mut marks = std::collections::HashSet::new();
    let mut stack = vec![doc];
    while let Some(doc) = stack.pop() {
        match doc {
            Doc::ContentMark(id) => {
                marks.insert(*id);
            }
            Doc::Concat(docs) | Doc::Fill(docs) | Doc::AlignGroup(docs) => stack.extend(docs),
            Doc::FillRest(rest) => stack.extend(rest.as_slice()),
            Doc::Indent(inner) | Doc::Group(inner) => stack.push(inner),
            Doc::IfBreak { broken, flat } => stack.extend([&**broken, &**flat]),
            Doc::AlignRow(cells) => stack.extend(cells.iter().map(|c| &c.content)),
            Doc::Token { .. }
            | Doc::Raw(_)
            | Doc::LineStart(_)
            | Doc::Hardline
            | Doc::BlankLine
            | Doc::Line
            | Doc::Softline
            | Doc::PreservedLine
            | Doc::Empty => {}
        }
    }
    marks
}

/// Join docs with a separator between each pair.
#[allow(dead_code)] // Phase F follow-up: builder integration pending.
pub fn join(docs: Vec<Doc>, separator: Doc) -> Doc {
    let mut parts = Vec::with_capacity(docs.len() * 2);
    for (i, doc) in docs.into_iter().enumerate() {
        if i > 0 {
            parts.push(separator.clone());
        }
        parts.push(doc);
    }
    concat(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concat_filters_empty() {
        let doc = concat(vec![Doc::Empty, Doc::Raw("a".into()), Doc::Empty]);
        assert!(matches!(doc, Doc::Raw(ref s) if s == "a"));
    }

    #[test]
    fn concat_empty_vec() {
        let doc = concat(vec![]);
        assert!(matches!(doc, Doc::Empty));
    }

    #[test]
    fn concat_single_element() {
        let doc = concat(vec![Doc::Hardline]);
        assert!(matches!(doc, Doc::Hardline));
    }

    #[test]
    fn join_with_separator() {
        let doc = join(
            vec![
                Doc::Raw("a".into()),
                Doc::Raw("b".into()),
                Doc::Raw("c".into()),
            ],
            Doc::Line,
        );
        match doc {
            Doc::Concat(parts) => assert_eq!(parts.len(), 5), // a, Line, b, Line, c
            _ => panic!("expected Concat"),
        }
    }
}
