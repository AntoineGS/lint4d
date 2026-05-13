use crate::doc::{self, Doc};
use crate::doc_builder::DocBuilder;
use pascal_core::node_kind as K;
use tree_sitter::Node;

impl<'a> DocBuilder<'a> {
    pub(crate) fn build_try(&self, node: Node<'a>) -> Doc {
        // try..except/finally..end — same structure as block but with
        // try/except/finally/end as structural keywords.
        self.build_children_preserving_blank_lines(node)
    }

    pub(crate) fn build_case(&self, node: Node<'a>) -> Doc {
        let mut parts = Vec::new();
        let mut after_else = false;
        let mut else_body: Vec<Doc> = Vec::new();
        let mut prev_was_case_branch = false;

        self.for_each_code_child(node, |child| match child.kind() {
            K::K_CASE => {
                parts.push(self.doc_for_node(child));
            }
            K::K_OF => {
                parts.push(self.doc_for_node(child));
                parts.push(Doc::Hardline);
                prev_was_case_branch = false;
            }
            K::CASE_CASE => {
                if prev_was_case_branch {
                    parts.push(Doc::Hardline);
                }
                parts.push(doc::indent(self.build_case_arm(child)));
                prev_was_case_branch = true;
            }
            K::K_ELSE => {
                // Flush any accumulated body before `else`
                if !else_body.is_empty() {
                    parts.push(doc::indent(doc::concat(std::mem::take(&mut else_body))));
                }
                // `else` aligns with `case`, not with branches
                parts.push(Doc::Hardline);
                parts.push(self.doc_for_node(child));
                after_else = true;
                prev_was_case_branch = false;
            }
            K::K_END => {
                // Flush else body before end
                if !else_body.is_empty() {
                    parts.push(doc::indent(doc::concat(std::mem::take(&mut else_body))));
                }
                after_else = false;
                parts.push(Doc::Hardline);
                parts.push(self.doc_for_node(child));
                prev_was_case_branch = false;
            }
            K::SEMICOLON if after_else => {
                // Semicolons in else body: emit without preceding Hardline
                // so they stay attached to the previous statement.
                else_body.push(self.doc_for_node(child));
            }
            _ if after_else => {
                // Statements after `else` are indented.
                // Skip Hardline if the child already starts with one
                // (e.g. from leading comments) to avoid a blank line.
                let child_doc = self.doc_for_node(child);
                if !crate::doc_builder::starts_with_hardline(&child_doc) {
                    else_body.push(Doc::Hardline);
                }
                else_body.push(child_doc);
            }
            _ => {
                parts.push(self.doc_for_node(child));
            }
        });
        // Flush any remaining else body
        if !else_body.is_empty() {
            parts.push(doc::indent(doc::concat(else_body)));
        }
        doc::concat(parts)
    }

    /// Build a single `caseCase` arm.
    ///
    /// Falls through to default rendering unless a `//` line comment lives
    /// between the case label and the arm body — in that case, joining
    /// label/body on one line lets the comment swallow the body (silent
    /// statement deletion). Force a hardline between them.
    fn build_case_arm(&self, node: Node<'a>) -> Doc {
        let children = self.code_children(node);
        let Some(label_idx) = children.iter().position(|c| c.kind() == K::CASE_LABEL) else {
            return self.doc_for_node(node);
        };
        let body_idx = label_idx + 1;
        if body_idx >= children.len() {
            return self.doc_for_node(node);
        }
        let label = children[label_idx];
        let body = children[body_idx];

        let label_end = label.end_byte();
        let body_start = body.start_byte();
        if label_end >= body_start || !has_line_comment_in_span(self.source, label_end, body_start)
        {
            return self.doc_for_node(node);
        }

        // doc_for_node on the label picks up its trailing `// comment`
        // (attached to the colon leaf); we then force a hardline before
        // emitting the body so the comment terminates correctly. Any
        // trailing children of the arm (e.g. the `;` separator that lives
        // *inside* caseCase per the grammar) are appended after the body
        // so we don't silently drop them.
        let label_doc = self.doc_for_node(label);
        let body_doc = self.doc_for_node(body);

        let mut indented = Vec::new();
        if !crate::doc_builder::starts_with_hardline(&body_doc) {
            indented.push(Doc::Hardline);
        }
        indented.push(body_doc);
        for tail in &children[body_idx + 1..] {
            indented.push(self.doc_for_node(*tail));
        }

        let leading = self.leading_comments_doc(node);
        let trailing = self.trailing_comments_doc(node);

        doc::concat(vec![
            leading,
            label_doc,
            doc::indent(doc::concat(indented)),
            trailing,
        ])
    }

    pub(crate) fn build_repeat(&self, node: Node<'a>) -> Doc {
        let mut parts = Vec::new();
        let mut body: Vec<Doc> = Vec::new();
        let mut in_body = false;

        self.for_each_code_child(node, |child| match child.kind() {
            K::K_REPEAT => {
                parts.push(self.doc_for_node(child));
                in_body = true;
            }
            K::K_UNTIL => {
                // Flush body
                if !body.is_empty() {
                    parts.push(doc::indent(doc::concat(std::mem::take(&mut body))));
                }
                parts.push(Doc::Hardline);
                parts.push(self.doc_for_node(child));
                in_body = false;
            }
            K::SEMICOLON if in_body => {
                body.push(self.doc_for_node(child));
            }
            _ if in_body => {
                let child_doc = self.doc_for_node(child);
                if !crate::doc_builder::starts_with_hardline(&child_doc) {
                    body.push(Doc::Hardline);
                }
                body.push(child_doc);
            }
            _ => {
                parts.push(self.doc_for_node(child));
            }
        });
        if !body.is_empty() {
            parts.push(doc::indent(doc::concat(body)));
        }
        doc::concat(parts)
    }

    pub(crate) fn build_if(&self, node: Node<'a>) -> Doc {
        let children = self.code_children(node);
        let mut parts = Vec::new();
        let mut i = 0;

        while i < children.len() {
            let child = children[i];
            match child.kind() {
                K::K_THEN | K::K_DO => {
                    parts.push(self.doc_for_node(child));
                    // Single statement after then/do → indented on next line
                    if let Some(next) = children.get(i + 1) {
                        if next.kind() != K::BLOCK && next.kind() != K::K_ELSE {
                            let next_doc = self.doc_for_node(*next);
                            let mut inner = Vec::new();
                            if !crate::doc_builder::starts_with_hardline(&next_doc) {
                                inner.push(Doc::Hardline);
                            }
                            inner.push(next_doc);
                            parts.push(doc::indent(doc::concat(inner)));
                            i += 2;
                            continue;
                        }
                    }
                }
                K::K_ELSE => {
                    parts.push(Doc::Hardline);
                    parts.push(self.doc_for_node(child));
                    // Single statement after else → indented on next line
                    if let Some(next) = children.get(i + 1) {
                        if next.kind() != K::BLOCK
                            && next.kind() != K::IF
                            && next.kind() != K::IF_ELSE
                        {
                            let next_doc = self.doc_for_node(*next);
                            let mut inner = Vec::new();
                            if !crate::doc_builder::starts_with_hardline(&next_doc) {
                                inner.push(Doc::Hardline);
                            }
                            inner.push(next_doc);
                            parts.push(doc::indent(doc::concat(inner)));
                            i += 2;
                            continue;
                        }
                    }
                }
                _ => {
                    parts.push(self.doc_for_node(child));
                }
            }
            i += 1;
        }
        doc::concat(parts)
    }

    pub(crate) fn build_loop(&self, node: Node<'a>) -> Doc {
        let children = self.code_children(node);
        let mut parts = Vec::new();
        let mut i = 0;

        while i < children.len() {
            let child = children[i];
            match child.kind() {
                K::K_DO => {
                    parts.push(self.doc_for_node(child));
                    if let Some(next) = children.get(i + 1) {
                        if next.kind() != K::BLOCK {
                            let next_doc = self.doc_for_node(*next);
                            let mut inner = Vec::new();
                            if !crate::doc_builder::starts_with_hardline(&next_doc) {
                                inner.push(Doc::Hardline);
                            }
                            inner.push(next_doc);
                            parts.push(doc::indent(doc::concat(inner)));
                            i += 2;
                            continue;
                        }
                    }
                }
                _ => {
                    parts.push(self.doc_for_node(child));
                }
            }
            i += 1;
        }
        doc::concat(parts)
    }
}

/// Return `true` if the source bytes from `start` (inclusive) to `end`
/// (exclusive) contain a `//` line comment outside of Pascal string
/// literals.
fn has_line_comment_in_span(source: &[u8], start: usize, end: usize) -> bool {
    if start >= end || end > source.len() {
        return false;
    }
    let bytes = &source[start..end];
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' {
            if in_string && i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                i += 2;
                continue;
            }
            in_string = !in_string;
        } else if !in_string && b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            return true;
        }
        i += 1;
    }
    false
}
