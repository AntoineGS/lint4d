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
        let children = self.code_children(node);
        let mut parts = Vec::new();

        for child in &children {
            match child.kind() {
                K::K_CASE => {
                    parts.push(self.doc_for_node(*child));
                }
                K::K_OF => {
                    parts.push(self.doc_for_node(*child));
                    parts.push(Doc::Hardline);
                }
                K::CASE_CASE => {
                    parts.push(doc::indent(self.doc_for_node(*child)));
                }
                K::K_ELSE => {
                    // `else` aligns with `case`, not with branches
                    parts.push(self.doc_for_node(*child));
                    parts.push(Doc::Hardline);
                }
                K::K_END => {
                    parts.push(Doc::Hardline);
                    parts.push(self.doc_for_node(*child));
                }
                _ => {
                    parts.push(self.doc_for_node(*child));
                }
            }
        }
        doc::concat(parts)
    }

    pub(crate) fn build_repeat(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
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
                            parts.push(doc::indent(doc::concat(vec![
                                Doc::Hardline,
                                self.doc_for_node(*next),
                            ])));
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
                            parts.push(doc::indent(doc::concat(vec![
                                Doc::Hardline,
                                self.doc_for_node(*next),
                            ])));
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
                            parts.push(doc::indent(doc::concat(vec![
                                Doc::Hardline,
                                self.doc_for_node(*next),
                            ])));
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
