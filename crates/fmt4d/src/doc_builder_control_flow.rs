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
                parts.push(doc::indent(self.doc_for_node(child)));
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
