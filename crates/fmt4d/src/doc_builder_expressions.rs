//! Expression formatting — binary chains, calls, args, delimited lists.
//! Extracted from doc_builder.rs during Phase F decomposition (review AH1 / BP-M5).

use crate::config::OperatorPosition;
use crate::doc::{self, Doc};
use crate::doc_builder::{BreakStyle, DocBuilder};
use pascal_core::node_kind as K;
use tree_sitter::Node;

impl<'a> DocBuilder<'a> {
    pub(crate) fn build_args(&self, node: Node<'a>) -> Doc {
        self.build_paren_list(node, K::SEMICOLON)
    }

    pub(crate) fn build_call(&self, node: Node<'a>) -> Doc {
        // Unwrap exprArgs: the tree-sitter grammar wraps call arguments in an
        // exprArgs node, hiding commas from the delimiter-list splitter.
        // Inline exprArgs's children so each argument gets its own line break.
        let mut flat: Vec<Node<'a>> = Vec::new();
        self.for_each_code_child(node, |child| {
            if child.kind() == K::EXPR_ARGS {
                for sub in child.children(&mut child.walk()) {
                    if !sub.is_extra() {
                        flat.push(sub);
                    }
                }
            } else {
                flat.push(child);
            }
        });

        // Hug a sole bracket-list argument: keep `([` and `])` together by
        // merging the outer call group with the inner bracket group.
        if let Some(hugged) = self.try_build_call_hugging_bracket(&flat) {
            return hugged;
        }

        self.build_delimited_list(&flat, K::COMMA, K::OPEN_PAREN, K::CLOSE_PAREN)
    }

    /// When a call has exactly one argument and that argument is an
    /// `exprBrackets` node, produce a single merged group so that `([` and
    /// `])` stay on the same line:
    ///
    /// ```text
    /// Foo([        ← not   Foo(
    ///   A,         ←         [A, B]
    ///   B          ←       )
    /// ]);
    /// ```
    fn try_build_call_hugging_bracket(&self, flat: &[Node<'a>]) -> Option<Doc> {
        let open_idx = flat.iter().position(|c| c.kind() == K::OPEN_PAREN)?;
        let close_idx = flat.iter().rposition(|c| c.kind() == K::CLOSE_PAREN)?;

        let inner = &flat[open_idx + 1..close_idx];
        if inner.len() != 1 || inner[0].kind() != K::EXPR_BRACKETS {
            return None;
        }
        let bracket_node = inner[0];
        let bracket_children = self.code_children(bracket_node);

        let b_open_idx = bracket_children
            .iter()
            .position(|c| c.kind() == K::OPEN_BRACKET)?;
        let b_close_idx = bracket_children
            .iter()
            .rposition(|c| c.kind() == K::CLOSE_BRACKET)?;
        let b_inner = &bracket_children[b_open_idx + 1..b_close_idx];

        // Build "before" for outer call (everything up to and including `(`)
        let mut before: Vec<Doc> = flat[..=open_idx]
            .iter()
            .map(|c| self.doc_for_node(*c))
            .collect();
        // Immediately append `[` — no softline between `(` and `[`
        before.push(self.doc_for_node(bracket_children[b_open_idx]));

        if b_inner.is_empty() {
            before.push(self.doc_for_node(bracket_children[b_close_idx]));
            before.push(self.doc_for_node(flat[close_idx]));
            for c in &flat[close_idx + 1..] {
                before.push(self.doc_for_node(*c));
            }
            return Some(doc::concat(before));
        }

        let groups = Self::split_children_at(b_inner, K::COMMA);
        let group_docs: Vec<Doc> = groups
            .iter()
            .map(|g| {
                let parts: Vec<Doc> = g.iter().map(|n| self.doc_for_node(*n)).collect();
                doc::concat(parts)
            })
            .collect();

        let mut inner_parts = Vec::new();
        for (i, gdoc) in group_docs.into_iter().enumerate() {
            if i > 0 {
                inner_parts.push(doc::token(",", K::COMMA, ""));
                inner_parts.push(Doc::Line);
            }
            inner_parts.push(gdoc);
        }

        let grouped = doc::group(doc::concat(vec![
            doc::concat(before),
            doc::indent(doc::concat(vec![Doc::Softline, doc::concat(inner_parts)])),
            Doc::Softline,
            self.doc_for_node(bracket_children[b_close_idx]),
            self.doc_for_node(flat[close_idx]),
        ]));

        let mut result = vec![grouped];
        for c in &flat[close_idx + 1..] {
            result.push(self.doc_for_node(*c));
        }
        Some(doc::concat(result))
    }

    pub(crate) fn build_bracket_list(&self, node: Node<'a>) -> Doc {
        let children = self.code_children(node);
        self.build_delimited_list(&children, K::COMMA, K::OPEN_BRACKET, K::CLOSE_BRACKET)
    }

    /// Build a parenthesised list with Group-based line breaking.
    pub(crate) fn build_paren_list(&self, node: Node<'a>, separator: &str) -> Doc {
        let children = self.code_children(node);
        self.build_delimited_list(&children, separator, K::OPEN_PAREN, K::CLOSE_PAREN)
    }

    /// Build a delimited list (parens or brackets) with Group-based line breaking.
    ///
    /// When the content fits on one line, keeps it flat.
    /// When it overflows, puts each separator-delimited item on its own line.
    pub(crate) fn build_delimited_list(
        &self,
        children: &[Node<'a>],
        separator: &str,
        open_kind: &str,
        close_kind: &str,
    ) -> Doc {
        let open_idx = children.iter().position(|c| c.kind() == open_kind);
        let close_idx = children.iter().rposition(|c| c.kind() == close_kind);

        let (open_idx, close_idx) = match (open_idx, close_idx) {
            (Some(o), Some(c)) => (o, c),
            _ => {
                let docs: Vec<Doc> = children.iter().map(|c| self.doc_for_node(*c)).collect();
                return doc::concat(docs);
            }
        };

        let mut before: Vec<Doc> = children[..=open_idx]
            .iter()
            .map(|c| self.doc_for_node(*c))
            .collect();

        let inner = &children[open_idx + 1..close_idx];
        if inner.is_empty() {
            before.push(self.doc_for_node(children[close_idx]));
            for c in &children[close_idx + 1..] {
                before.push(self.doc_for_node(*c));
            }
            return doc::concat(before);
        }

        // Check for // line comments in the inner content — when present the
        // list MUST break because a line comment eats the rest of the line.
        // Skip string literals (delimited by single quotes) so that `//`
        // inside strings like `'http://'` is not mistaken for a comment.
        let inner_start = children[open_idx].end_byte();
        let inner_end = children[close_idx].start_byte();
        // Guard against tree-sitter error-recovery producing inverted
        // or out-of-range byte offsets (review SEC-H5). Fall back to a
        // plain children walk rather than panic on a slice index.
        if inner_start > inner_end || inner_end > self.source.len() {
            let docs: Vec<Doc> = children.iter().map(|c| self.doc_for_node(*c)).collect();
            return doc::concat(docs);
        }
        let has_line_comments = {
            let bytes = &self.source[inner_start..inner_end];
            let mut in_string = false;
            let mut found = false;
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if in_string {
                        if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                            i += 2; // escaped quote ''
                            continue;
                        }
                        in_string = false;
                    } else {
                        in_string = true;
                    }
                } else if !in_string
                    && bytes[i] == b'/'
                    && i + 1 < bytes.len()
                    && bytes[i + 1] == b'/'
                {
                    found = true;
                    break;
                }
                i += 1;
            }
            found
        };

        // Split into groups while collecting the separator nodes so their
        // trailing comments (e.g. `// & prefix` after a comma) are preserved.
        let mut groups: Vec<Vec<Node<'a>>> = Vec::new();
        let mut sep_nodes: Vec<Node<'a>> = Vec::new();
        let mut current: Vec<Node<'a>> = Vec::new();

        for node in inner {
            if node.kind() == separator {
                if separator == K::SEMICOLON {
                    current.push(*node);
                }
                groups.push(std::mem::take(&mut current));
                if separator != K::SEMICOLON {
                    sep_nodes.push(*node);
                }
            } else {
                current.push(*node);
            }
        }
        if !current.is_empty() {
            groups.push(current);
        }

        let group_docs: Vec<Doc> = groups
            .iter()
            .map(|g| {
                let parts: Vec<Doc> = g.iter().map(|n| self.doc_for_node(*n)).collect();
                doc::concat(parts)
            })
            .collect();

        let mut inner_parts = Vec::new();
        for (i, gdoc) in group_docs.into_iter().enumerate() {
            if i > 0 {
                if separator == K::COMMA {
                    if let Some(sep_node) = sep_nodes.get(i - 1) {
                        inner_parts.push(self.doc_for_node(*sep_node));
                    } else {
                        inner_parts.push(doc::token(",", K::COMMA, ""));
                    }
                }
                if has_line_comments {
                    inner_parts.push(Doc::Hardline);
                } else {
                    inner_parts.push(Doc::Line);
                }
            }
            inner_parts.push(gdoc);
        }

        let body = doc::concat(vec![
            doc::concat(before),
            doc::indent(doc::concat(vec![
                if has_line_comments {
                    Doc::Hardline
                } else {
                    Doc::Softline
                },
                doc::concat(inner_parts),
            ])),
            if has_line_comments {
                Doc::Hardline
            } else {
                Doc::Softline
            },
            self.doc_for_node(children[close_idx]),
        ]);

        let grouped = if has_line_comments {
            body
        } else {
            doc::group(body)
        };

        let mut result = vec![grouped];
        for c in &children[close_idx + 1..] {
            result.push(self.doc_for_node(*c));
        }
        doc::concat(result)
    }

    pub(crate) fn build_expression_breaking(&self, node: Node<'a>) -> Doc {
        let binary_node = if node.kind() == K::EXPR_BINARY {
            node
        } else {
            let mut cursor = node.walk();
            let found = node
                .children(&mut cursor)
                .find(|c| c.kind() == K::EXPR_BINARY);
            match found {
                Some(bin) => bin,
                None => return self.build_children(node),
            }
        };

        let segments = flatten_binary_chain(binary_node, None);
        if segments.len() <= 1 {
            return self.build_children(node);
        }

        // Determine the chain's operator family.
        let is_add_chain = segments
            .iter()
            .skip(1)
            .all(|s| s.operator.is_none_or(|op| op.kind() == K::K_ADD));
        let is_bool_chain = segments.iter().skip(1).all(|s| {
            s.operator
                .is_none_or(|op| matches!(op.kind(), K::K_OR | K::K_AND))
        });

        let is_multiline = binary_node.start_position().row != binary_node.end_position().row;

        let break_style = if is_bool_chain {
            // or/and chains: expand all (one per line) when they overflow.
            BreakStyle::ExpandAll
        } else if is_add_chain && is_multiline {
            // + chains: greedy pack, but preserve author breaks.
            BreakStyle::PreserveBreaks
        } else {
            // Everything else: greedy pack.
            BreakStyle::GreedyFill
        };

        self.build_binary_chain_doc(&segments, break_style)
    }

    /// Build a flattened binary chain as a Doc IR.
    ///
    /// Operator placement depends on `config.operator_position`:
    /// - `Leading` (default): operator starts the continuation line
    /// - `Trailing`: operator ends the previous line
    ///
    /// Break behaviour depends on `break_style`:
    /// - `GreedyFill`: pack as many operands per line as fit (Fill).
    /// - `PreserveBreaks`: like GreedyFill, but positions where the source
    ///   had a newline use `PreservedLine` (forced break in Fill, joinable
    ///   when a parent `Group` determines the whole expression fits).
    /// - `ExpandAll`: all-or-nothing — either the whole chain fits on one
    ///   line, or every operand gets its own line.
    pub(crate) fn build_binary_chain_doc(
        &self,
        segments: &[BinarySegment],
        break_style: BreakStyle,
    ) -> Doc {
        let trailing = self.config.operator_position == OperatorPosition::Trailing;

        // Build the first operand (no operator). Insert Hardline before any
        // sibling that follows a node carrying a trailing `//` line comment.
        let mut first_parts = Vec::new();
        let mut prev_had_line_comment = false;
        if let Some(seg) = segments.first() {
            for n in &seg.operand {
                if prev_had_line_comment {
                    first_parts.push(Doc::Hardline);
                }
                first_parts.push(self.doc_for_node(*n));
                prev_had_line_comment = self.has_trailing_line_comment(*n);
            }
            // Trailing: attach the *next* segment's operator to the first operand.
            if trailing && segments.len() > 1 {
                if let Some(op) = segments[1].operator {
                    if prev_had_line_comment {
                        first_parts.push(Doc::Hardline);
                    }
                    first_parts.push(self.doc_for_node(op));
                }
            }
        }

        if segments.len() <= 1 {
            return doc::concat(first_parts);
        }

        // Build parts: [sep, content, sep, content, ...]
        let mut parts = Vec::new();

        for i in 1..segments.len() {
            let seg = &segments[i];

            // If the content preceding this separator ends with a `//` line
            // comment, force a hard newline. A Line/PreservedLine in flat
            // mode degrades to a space, which would slide the following
            // operand onto the same physical line as the `//` and silently
            // make it part of the comment (invalid Pascal).
            let prev_ends_with_line_comment = if trailing {
                // Trailing mode: the previous item ended with op[i]
                // (segments[i].operator, attached to the prior segment's
                // content). The trailing comment lives on that operator.
                segments[i]
                    .operator
                    .is_some_and(|op| self.has_trailing_line_comment(op))
            } else {
                // Leading mode: the previous item ended with the last node
                // of segments[i-1].operand.
                segments[i - 1]
                    .operand
                    .last()
                    .is_some_and(|n| self.has_trailing_line_comment(*n))
            };

            let sep = if prev_ends_with_line_comment {
                Doc::Hardline
            } else if break_style == BreakStyle::PreserveBreaks
                && has_newline_between(self.source, &segments[i - 1], seg)
            {
                Doc::PreservedLine
            } else {
                Doc::Line
            };
            parts.push(sep);

            // Build content item (operator + operand or operand + operator).
            // Whenever a node in this item carries a trailing `//` line
            // comment, the following sibling must start on a new physical
            // line — anything else would be swallowed by the comment.
            let mut item = Vec::new();
            if trailing {
                // Trailing: operand first, then next segment's operator (if any).
                let mut prev_had_line_comment = false;
                for n in &seg.operand {
                    if prev_had_line_comment {
                        item.push(Doc::Hardline);
                    }
                    item.push(self.doc_for_node(*n));
                    prev_had_line_comment = self.has_trailing_line_comment(*n);
                }
                if i + 1 < segments.len() {
                    if let Some(op) = segments[i + 1].operator {
                        if prev_had_line_comment {
                            item.push(Doc::Hardline);
                        }
                        item.push(self.doc_for_node(op));
                    }
                }
            } else {
                // Leading: operator first, then operand.
                let mut prev_had_line_comment = false;
                if let Some(op) = seg.operator {
                    item.push(self.doc_for_node(op));
                    prev_had_line_comment = self.has_trailing_line_comment(op);
                }
                for n in &seg.operand {
                    if prev_had_line_comment {
                        item.push(Doc::Hardline);
                    }
                    item.push(self.doc_for_node(*n));
                    prev_had_line_comment = self.has_trailing_line_comment(*n);
                }
            }
            parts.push(doc::concat(item));
        }

        let continuation = if break_style == BreakStyle::ExpandAll {
            // All-or-nothing: Group + Concat with Line separators.
            doc::concat(parts)
        } else {
            // Greedy packing via Fill.
            doc::fill(parts)
        };

        doc::group(doc::concat(vec![
            doc::concat(first_parts),
            doc::indent(continuation),
        ]))
    }
}

/// A segment of a flattened binary expression chain.
pub(crate) struct BinarySegment<'a> {
    pub operator: Option<Node<'a>>,
    pub operand: Vec<Node<'a>>,
}

pub(crate) fn flatten_binary_chain<'a>(
    node: Node<'a>,
    only_ops: Option<&[&str]>,
) -> Vec<BinarySegment<'a>> {
    let mut segments = Vec::new();
    flatten_binary_chain_inner(node, only_ops, &mut segments);
    segments
}

fn flatten_binary_chain_inner<'a>(
    start: Node<'a>,
    only_ops: Option<&[&str]>,
    segments: &mut Vec<BinarySegment<'a>>,
) {
    // Walk the left spine iteratively: a binary chain `a op b op c op d`
    // parses as `((a op b) op c) op d`, so `left` descends through the
    // EXPR_BINARY spine. Previously this was recursive and would
    // stack-overflow on ~5k-depth chains (review SEC-H1).
    //
    // `pending` accumulates (operator, right-operand) pairs on the way
    // down. When we reach the leaf (or an early break point — malformed
    // shape / operator outside `only_ops`), we emit the leftmost operand
    // and unwind the pending stack in reverse to emit each (op, right)
    // segment in source order. Early-break paths MUST drain `pending` to
    // avoid losing outer segments.
    let mut pending: Vec<(Node<'a>, Node<'a>)> = Vec::new();
    let mut node = start;

    // Helper: emit `leftmost` as the first operand, then drain pending
    // in reverse to emit each (op, right) segment in source order.
    fn emit_leaf_and_drain<'a>(
        leftmost: Node<'a>,
        pending: &mut Vec<(Node<'a>, Node<'a>)>,
        segments: &mut Vec<BinarySegment<'a>>,
    ) {
        segments.push(BinarySegment {
            operator: None,
            operand: vec![leftmost],
        });
        while let Some((op, right)) = pending.pop() {
            segments.push(BinarySegment {
                operator: Some(op),
                operand: vec![right],
            });
        }
    }

    loop {
        let children: Vec<Node<'a>> = node
            .children(&mut node.walk())
            .filter(|c| !c.is_extra())
            .collect();
        if children.len() != 3 {
            // Not a binary shape — emit the whole subtree as one operand,
            // then drain any pending outer segments.
            emit_leaf_and_drain(node, &mut pending, segments);
            break;
        }
        let left = children[0];
        let op = children[1];
        let right = children[2];
        if let Some(allowed) = only_ops {
            if !allowed.contains(&op.kind()) {
                // Operator filtered out — emit the whole subtree as one
                // operand, then drain any pending outer segments.
                emit_leaf_and_drain(node, &mut pending, segments);
                break;
            }
        }
        pending.push((op, right));
        if left.kind() == K::EXPR_BINARY {
            node = left;
            continue;
        }
        // Leaf reached: emit `left` as the first operand, then unwind
        // the pending stack in reverse to emit each (op, right) segment
        // in source order.
        emit_leaf_and_drain(left, &mut pending, segments);
        break;
    }
}

/// Return `true` if the source text between two consecutive binary chain
/// segments contains a newline, indicating the author intentionally broke
/// the expression across lines.
///
/// Checks from the end of the previous operand to the start of the current
/// operand (spanning the operator in between). This covers both leading
/// (`\n  + operand`) and trailing (`operand +\n`) break styles.
fn has_newline_between(source: &[u8], prev: &BinarySegment, curr: &BinarySegment) -> bool {
    let prev_end = prev.operand.last().map(|n| n.end_byte()).unwrap_or(0);
    let curr_start = curr
        .operand
        .first()
        .map(|n| n.start_byte())
        .unwrap_or(prev_end);
    if curr_start <= prev_end {
        return false;
    }
    source[prev_end..curr_start].contains(&b'\n')
}
