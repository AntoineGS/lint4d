use crate::doc::{self, Doc};
use crate::doc_builder::DocBuilder;
use pascal_core::node_kind as K;
use tree_sitter::Node;

impl DocBuilder<'_> {
    pub(crate) fn build_unit(&self, node: Node) -> Doc {
        let mut parts = Vec::new();
        let mut prev_kind: &'static str = "";

        self.for_each_code_child(node, |child| {
            let kind = child.kind();
            if (kind == K::INTERFACE
                || kind == K::IMPLEMENTATION
                || kind == K::INITIALIZATION
                || kind == K::FINALIZATION
                || kind == K::K_END)
                && !prev_kind.is_empty()
            {
                parts.push(Doc::BlankLine);
            }
            parts.push(self.doc_for_node(child));
            prev_kind = kind;
        });
        doc::concat(parts)
    }

    pub(crate) fn build_interface_section(&self, node: Node) -> Doc {
        let mut parts = Vec::new();
        let mut after_header = false;
        let mut prev_child_kind: &'static str = "";
        let mut prev_single_line = false;

        self.for_each_code_child(node, |child| match child.kind() {
            K::K_INTERFACE => {
                parts.push(self.doc_for_node(child));
                parts.push(Doc::Hardline);
                after_header = true;
            }
            _ => {
                let kind = child.kind();
                let single_line = child.start_position().row == child.end_position().row;
                let child_doc = self.doc_for_node(child);
                if after_header {
                    parts.push(Doc::Hardline);
                    after_header = false;
                } else if !prev_child_kind.is_empty() {
                    let blanks = crate::blank_lines::needs_blank_line_between(
                        prev_child_kind,
                        kind,
                        &self.config.blank_lines,
                    );
                    if blanks > 0 && !(prev_single_line && single_line) {
                        for _ in 0..blanks {
                            parts.push(Doc::BlankLine);
                        }
                    } else {
                        let prev_ends = parts
                            .last()
                            .is_some_and(crate::doc_builder::ends_with_hardline);
                        if !prev_ends && !crate::doc_builder::starts_with_hardline(&child_doc) {
                            parts.push(Doc::Hardline);
                        }
                    }
                }
                parts.push(child_doc);
                prev_child_kind = kind;
                prev_single_line = single_line;
            }
        });
        doc::concat(parts)
    }

    pub(crate) fn build_implementation_section(&self, node: Node) -> Doc {
        let mut parts = Vec::new();
        let mut after_header = false;
        let mut prev_child_kind: &'static str = "";
        let mut prev_single_line = false;

        self.for_each_code_child(node, |child| match child.kind() {
            K::K_IMPLEMENTATION => {
                parts.push(self.doc_for_node(child));
                parts.push(Doc::Hardline);
                after_header = true;
            }
            _ => {
                let kind = child.kind();
                let single_line = child.start_position().row == child.end_position().row;
                let child_doc = self.doc_for_node(child);
                if after_header {
                    parts.push(Doc::Hardline);
                    after_header = false;
                } else if !prev_child_kind.is_empty() {
                    let blanks = crate::blank_lines::needs_blank_line_between(
                        prev_child_kind,
                        kind,
                        &self.config.blank_lines,
                    );
                    if blanks > 0 && !(prev_single_line && single_line) {
                        for _ in 0..blanks {
                            parts.push(Doc::BlankLine);
                        }
                    } else {
                        let prev_ends = parts
                            .last()
                            .is_some_and(crate::doc_builder::ends_with_hardline);
                        if !prev_ends && !crate::doc_builder::starts_with_hardline(&child_doc) {
                            parts.push(Doc::Hardline);
                        }
                    }
                }
                parts.push(child_doc);
                prev_child_kind = kind;
                prev_single_line = single_line;
            }
        });
        doc::concat(parts)
    }

    pub(crate) fn build_init_final_section(&self, node: Node) -> Doc {
        let mut parts = Vec::new();
        let mut body_parts: Vec<Doc> = Vec::new();
        let mut prev_end_row: Option<usize> = None;

        self.for_each_code_child(node, |child| match child.kind() {
            K::K_INITIALIZATION | K::K_FINALIZATION => {
                // Flush accumulated body
                if !body_parts.is_empty() {
                    parts.push(doc::indent(doc::concat(std::mem::take(&mut body_parts))));
                    prev_end_row = None;
                }
                parts.push(self.doc_for_node(child));
                parts.push(Doc::Hardline);
            }
            K::SEMICOLON => {
                // Semicolons: just emit into body
                body_parts.push(self.doc_for_node(child));
                prev_end_row = Some(child.end_position().row);
            }
            _ => {
                // Check for blank line preservation
                if let Some(prev_end) = prev_end_row {
                    if self.has_blank_line_between(prev_end, child.start_position().row) {
                        body_parts.push(Doc::BlankLine);
                    }
                }
                // Add Hardline before each statement (except the first)
                if !body_parts.is_empty() {
                    body_parts.push(Doc::Hardline);
                }
                body_parts.push(self.doc_for_node(child));
                prev_end_row = Some(child.end_position().row);
            }
        });
        // Flush remaining body
        if !body_parts.is_empty() {
            parts.push(doc::indent(doc::concat(body_parts)));
        }
        doc::concat(parts)
    }
}
