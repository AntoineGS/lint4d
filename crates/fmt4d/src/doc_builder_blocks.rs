//! Block builders — `begin..end`, `try..except/finally`, `repeat..until`, `asm..end`
//! and the blank-line preserving statement walker.
//! Extracted from doc_builder.rs during Phase F decomposition (review AH1 / BP-M5).

use crate::doc::{self, Doc};
use crate::doc_builder::{
    DocBuilder, first_leaf, last_leaf, starts_with_hardline, strip_trailing_hardline,
};
use pascal_core::node_kind as K;
use tree_sitter::Node;

impl<'a> DocBuilder<'a> {
    pub(crate) fn build_block(&self, node: Node<'a>) -> Doc {
        self.build_children_preserving_blank_lines(node)
    }

    /// An `asm … end` block. The grammar keeps only the identifiers and
    /// reserved words of an `asmBody` as nodes, so the body is copied as
    /// written, from its first to its last line, with the comments and
    /// directives around it up to `end`; `asm` and `end` start their own
    /// lines.
    pub(crate) fn build_asm(&self, node: Node<'a>) -> Doc {
        // `asm` starts its line, also as a routine body after a section.
        let mut parts = vec![Doc::Hardline];
        let mut body_end: Option<usize> = None;
        for child in self.code_children(node) {
            match child.kind() {
                K::ASM_BODY => {
                    let first = first_leaf(child);
                    let last = last_leaf(child);
                    let start = self
                        .comments
                        .leading_comments(first.id())
                        .iter()
                        .map(|c| c.span.start)
                        .chain(
                            self.directives
                                .leading_directives(first.id())
                                .iter()
                                .map(|d| d.span.start),
                        )
                        .fold(child.start_byte(), usize::min);
                    let end = self
                        .trailing_items(last)
                        .iter()
                        .map(|item| item.span.end)
                        .fold(child.end_byte(), usize::max);
                    parts.push(Doc::Hardline);
                    parts.push(self.verbatim_doc(start, end));
                    body_end = Some(end);
                }
                K::K_END => {
                    match body_end {
                        // Trivia between the body and `end` stays with the
                        // verbatim body.
                        Some(body_end) => {
                            let leading = self
                                .comments
                                .leading_comments(child.id())
                                .iter()
                                .map(|c| c.span.end)
                                .chain(
                                    self.directives
                                        .leading_directives(child.id())
                                        .iter()
                                        .map(|d| d.span.end),
                                )
                                .max();
                            if let Some(leading_end) = leading {
                                parts.push(self.verbatim_doc(body_end, leading_end));
                            }
                            parts.push(Doc::Hardline);
                            parts.push(self.build_doc(child));
                            parts.push(self.trailing_trivia_doc(child));
                        }
                        None => {
                            let end = self.doc_for_node(child);
                            if !starts_with_hardline(&end) {
                                parts.push(Doc::Hardline);
                            }
                            parts.push(end);
                        }
                    }
                }
                _ => parts.push(self.doc_for_node(child)),
            }
        }
        doc::concat(parts)
    }

    pub(crate) fn build_children_preserving_blank_lines(&self, node: Node<'a>) -> Doc {
        let children = self.code_children(node);
        let mut parts = Vec::new();
        let mut body: Vec<Doc> = Vec::new();
        // `statements` nodes (try/finally/repeat bodies) are already inside a
        // block — their parent handles indentation, so we start in block mode
        // but skip the indent wrapper when flushing.
        let is_bare_statements = node.kind() == K::STATEMENTS;
        let mut in_block = is_bare_statements;
        let mut prev_end_row: Option<usize> = None;
        let mut prev_kind: &'static str = "";

        for child in &children {
            let kind = child.kind();

            match kind {
                K::K_BEGIN | K::K_TRY | K::K_REPEAT | K::K_ASM => {
                    // Flush any accumulated body (shouldn't have any before first opener)
                    if !body.is_empty() {
                        let flushed = doc::concat(std::mem::take(&mut body));
                        parts.push(if is_bare_statements {
                            flushed
                        } else {
                            doc::indent(flushed)
                        });
                    }
                    parts.push(Doc::Hardline);
                    parts.push(self.doc_for_node(*child));
                    in_block = true;
                    prev_end_row = Some(child.end_position().row);
                    prev_kind = kind;
                }
                K::K_END => {
                    // If kEnd has leading comments or directives, they belong inside the body.
                    // Strip trailing Hardline — the handler adds its own before
                    // the keyword, so keeping it would create a blank line.
                    let leading = self.leading_trivia_doc(*child);
                    if !matches!(leading, Doc::Empty) {
                        body.push(strip_trailing_hardline(leading));
                    }
                    // Flush body into Indent
                    if !body.is_empty() {
                        let flushed = doc::concat(std::mem::take(&mut body));
                        parts.push(if is_bare_statements {
                            flushed
                        } else {
                            doc::indent(flushed)
                        });
                    }
                    parts.push(Doc::Hardline);
                    // Build the kEnd node WITHOUT its leading trivia (already emitted above)
                    let end_body = self.build_doc(*child);
                    let end_trailing = self.trailing_trivia_doc(*child);
                    parts.push(doc::concat(vec![end_body, end_trailing]));
                    in_block = false;
                    prev_end_row = Some(child.end_position().row);
                    prev_kind = kind;
                }
                K::K_EXCEPT | K::K_FINALLY => {
                    // If except/finally has leading comments or directives, they belong inside
                    // the body. Strip trailing Hardline — same reason as kEnd.
                    let leading = self.leading_trivia_doc(*child);
                    if !matches!(leading, Doc::Empty) {
                        body.push(strip_trailing_hardline(leading));
                    }
                    // Flush body from try section
                    if !body.is_empty() {
                        let flushed = doc::concat(std::mem::take(&mut body));
                        parts.push(if is_bare_statements {
                            flushed
                        } else {
                            doc::indent(flushed)
                        });
                    }
                    parts.push(Doc::Hardline);
                    // Build except/finally WITHOUT its leading trivia (already emitted above)
                    let kw_body = self.build_doc(*child);
                    let kw_trailing = self.trailing_trivia_doc(*child);
                    parts.push(doc::concat(vec![kw_body, kw_trailing]));
                    // Start new body for except/finally section
                    in_block = true;
                    prev_end_row = Some(child.end_position().row);
                    prev_kind = kind;
                }
                K::SEMICOLON if in_block => {
                    // Semicolons inside blocks: emit only — the next child's
                    // Hardline provides the line break (matching the old
                    // printer's idempotent ensure_newline).
                    body.push(self.doc_for_node(*child));
                    prev_end_row = Some(child.end_position().row);
                    prev_kind = kind;
                }
                K::SEMICOLON => {
                    // Semicolons outside blocks (e.g. after end) — no trailing
                    // Hardline; the parent or next sibling provides it.
                    parts.push(self.doc_for_node(*child));
                    prev_end_row = Some(child.end_position().row);
                    prev_kind = kind;
                }
                _ => {
                    if in_block {
                        // Check for preserved blank lines between statements
                        let skip_blank = is_block_opener(prev_kind) || is_block_closer(kind);
                        if !skip_blank
                            && !prev_kind.is_empty()
                            && let Some(prev_end) = prev_end_row
                            && self.has_blank_line_between(prev_end, child.start_position().row)
                        {
                            body.push(Doc::BlankLine);
                        }
                        // Build the child doc first, then check if it already
                        // starts with a Hardline (e.g. from leading comments
                        // on a descendant). If so, skip our own Hardline to
                        // match the old printer's idempotent ensure_newline.
                        let child_doc = self.doc_for_node(*child);
                        if !starts_with_hardline(&child_doc) {
                            body.push(Doc::Hardline);
                        }
                        body.push(child_doc);
                    } else {
                        // Preserve blank lines outside blocks
                        let skip_blank = prev_kind.is_empty()
                            || is_block_opener(prev_kind)
                            || is_block_closer(kind);
                        if !skip_blank
                            && let Some(prev_end) = prev_end_row
                            && self.has_blank_line_between(prev_end, child.start_position().row)
                        {
                            parts.push(Doc::BlankLine);
                        }
                        parts.push(self.doc_for_node(*child));
                    }
                    prev_end_row = Some(child.end_position().row);
                    prev_kind = kind;
                }
            }
        }

        // Flush any remaining body
        if !body.is_empty() {
            let flushed = doc::concat(body);
            parts.push(if is_bare_statements {
                flushed
            } else {
                doc::indent(flushed)
            });
        }

        doc::concat(parts)
    }
}

fn is_block_opener(kind: &str) -> bool {
    matches!(kind, K::K_BEGIN | K::K_TRY | K::K_REPEAT | K::K_ASM)
}

fn is_block_closer(kind: &str) -> bool {
    matches!(kind, K::K_END | K::K_EXCEPT | K::K_FINALLY)
}
