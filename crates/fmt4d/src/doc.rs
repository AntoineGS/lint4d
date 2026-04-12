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
    IfBreak { broken: Box<Doc>, flat: Box<Doc> },

    /// Greedy line-filling for binary chains.
    ///
    /// Parts alternate: `[sep_0, content_0, sep_1, content_1, ...]`
    /// where separators are `Line` (greedy decision) or `Hardline` (forced
    /// break).  The renderer processes pairs greedily: if `sep + content`
    /// fits on the current line, render sep flat (space); otherwise render
    /// sep as break (newline + indent).
    Fill(Vec<Doc>),

    /// A group of rows that should be column-aligned.
    /// The renderer pre-calculates column widths across all rows
    /// before rendering with padding.
    AlignGroup(Vec<Doc>),

    /// A single row within an AlignGroup. Each cell is rendered
    /// normally but padded to the group's resolved column width.
    AlignRow(Vec<AlignCell>),

    /// Identity element — produces no output.
    Empty,
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

/// Join docs with a separator between each pair.
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
