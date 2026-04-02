use crate::doc::{self, Doc};
use crate::doc_builder::DocBuilder;
use pascal_core::node_kind as K;
use tree_sitter::Node;

impl<'a> DocBuilder<'a> {
    pub(crate) fn build_unit(&self, node: Node) -> Doc {
        let children = self.code_children(node);
        let mut parts = Vec::new();
        let mut prev_kind = String::new();

        for child in &children {
            let kind = child.kind().to_string();
            if (kind == K::INTERFACE
                || kind == K::IMPLEMENTATION
                || kind == K::INITIALIZATION
                || kind == K::FINALIZATION
                || kind == K::K_END)
                && !prev_kind.is_empty()
            {
                parts.push(Doc::BlankLine);
            }
            parts.push(self.doc_for_node(*child));
            prev_kind = kind;
        }
        doc::concat(parts)
    }

    pub(crate) fn build_interface_section(&self, node: Node) -> Doc {
        let children = self.code_children(node);
        let mut parts = Vec::new();
        let mut after_header = false;
        let mut prev_child_kind = String::new();

        for child in &children {
            match child.kind() {
                K::K_INTERFACE => {
                    parts.push(self.doc_for_node(*child));
                    parts.push(Doc::Hardline);
                    after_header = true;
                }
                _ => {
                    let kind = child.kind().to_string();
                    if after_header {
                        parts.push(Doc::Hardline);
                        after_header = false;
                    } else if !prev_child_kind.is_empty() {
                        let blanks = crate::blank_lines::needs_blank_line_between(
                            &prev_child_kind,
                            &kind,
                            &self.config.blank_lines,
                        );
                        for _ in 0..blanks {
                            parts.push(Doc::BlankLine);
                        }
                    }
                    parts.push(self.doc_for_node(*child));
                    prev_child_kind = kind;
                }
            }
        }
        doc::concat(parts)
    }

    pub(crate) fn build_implementation_section(&self, node: Node) -> Doc {
        let children = self.code_children(node);
        let mut parts = Vec::new();
        let mut after_header = false;
        let mut prev_child_kind = String::new();

        for child in &children {
            match child.kind() {
                K::K_IMPLEMENTATION => {
                    parts.push(self.doc_for_node(*child));
                    parts.push(Doc::Hardline);
                    after_header = true;
                }
                _ => {
                    let kind = child.kind().to_string();
                    if after_header {
                        parts.push(Doc::Hardline);
                        after_header = false;
                    } else if !prev_child_kind.is_empty() {
                        let blanks = crate::blank_lines::needs_blank_line_between(
                            &prev_child_kind,
                            &kind,
                            &self.config.blank_lines,
                        );
                        for _ in 0..blanks {
                            parts.push(Doc::BlankLine);
                        }
                    }
                    parts.push(self.doc_for_node(*child));
                    prev_child_kind = kind;
                }
            }
        }
        doc::concat(parts)
    }

    pub(crate) fn build_init_final_section(&self, node: Node) -> Doc {
        let children = self.code_children(node);
        let mut parts = Vec::new();

        for child in &children {
            match child.kind() {
                K::K_INITIALIZATION | K::K_FINALIZATION => {
                    parts.push(self.doc_for_node(*child));
                    parts.push(Doc::Hardline);
                }
                _ => {
                    parts.push(doc::indent(self.doc_for_node(*child)));
                }
            }
        }
        doc::concat(parts)
    }
}
