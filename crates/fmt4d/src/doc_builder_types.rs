use crate::doc::{self, Doc};
use crate::doc_builder::{DocBuilder, strip_trailing_hardline};
use pascal_core::node_kind as K;
use tree_sitter::Node;

impl<'a> DocBuilder<'a> {
    pub(crate) fn build_type_body(&self, node: Node<'a>) -> Doc {
        let children = self.code_children(node);
        let has_visibility = children
            .iter()
            .any(|c| c.kind() == K::DECL_SECTION || c.kind() == K::PP_DECL_SECTION);
        let has_end = children.iter().any(|c| c.kind() == K::K_END);
        let align_fields = self.should_align("fields");
        let align_props = self.should_align("properties");
        let mut parts = Vec::new();
        let mut body_children: Vec<Node<'a>> = Vec::new();
        let mut in_ancestor_list = false;
        let mut prev_body_end_row: Option<usize> = None;

        for (idx, child) in children.iter().enumerate() {
            match child.kind() {
                K::K_CLASS
                | K::K_RECORD
                | K::K_INTERFACE
                | K::K_OBJECT
                | K::K_PACKED
                | K::K_ABSTRACT
                | K::K_SEALED => {
                    parts.push(self.doc_for_node(*child));
                    in_ancestor_list = true;
                }
                K::OPEN_PAREN if in_ancestor_list => {
                    parts.push(Doc::Raw("(".into()));
                }
                K::CLOSE_PAREN if in_ancestor_list => {
                    parts.push(Doc::Raw(")".into()));
                    in_ancestor_list = false;
                    let next_is_section = children.get(idx + 1).is_some_and(|n| {
                        n.kind() == K::DECL_SECTION || n.kind() == K::PP_DECL_SECTION
                    });
                    if has_end && !next_is_section {
                        parts.push(Doc::Hardline);
                    }
                }
                K::COMMA if in_ancestor_list => {
                    parts.push(Doc::Raw(", ".into()));
                }
                K::TYPEREF if in_ancestor_list => {
                    parts.push(Doc::Raw(self.node_text(*child)));
                }
                K::K_END => {
                    if in_ancestor_list {
                        in_ancestor_list = false;
                    }
                    // Flush body
                    if !body_children.is_empty() {
                        let body_doc = if !has_visibility && (align_fields || align_props) {
                            let aligned = self.build_aligned_decl_section_body(
                                &body_children,
                                prev_body_end_row,
                                align_fields,
                                align_props,
                            );
                            doc::indent(aligned)
                        } else if !has_visibility {
                            let body_parts = self.build_type_body_parts(&body_children);
                            doc::indent(doc::concat(body_parts))
                        } else {
                            let body_parts = self.build_type_body_parts(&body_children);
                            doc::concat(body_parts)
                        };
                        parts.push(strip_trailing_hardline(body_doc));
                        body_children.clear();
                    }
                    parts.push(Doc::Hardline);
                    parts.push(self.doc_for_node(*child));
                }
                K::DECL_SECTION | K::PP_DECL_SECTION => {
                    if in_ancestor_list {
                        in_ancestor_list = false;
                    }
                    if let Some(prev_end) = prev_body_end_row {
                        if self.has_blank_line_between(prev_end, child.start_position().row) {
                            body_children.push(*child); // will be handled as-is
                        } else {
                            body_children.push(*child);
                        }
                    } else {
                        body_children.push(*child);
                    }
                    prev_body_end_row = Some(child.end_position().row);
                }
                _ => {
                    if in_ancestor_list {
                        in_ancestor_list = false;
                        let is_section =
                            child.kind() == K::DECL_SECTION || child.kind() == K::PP_DECL_SECTION;
                        if has_end && !is_section {
                            parts.push(Doc::Hardline);
                        }
                    }
                    body_children.push(*child);
                    prev_body_end_row = Some(child.end_position().row);
                }
            }
        }
        // Flush remaining body
        if !body_children.is_empty() {
            if !has_visibility {
                if align_fields || align_props {
                    let aligned = self.build_aligned_decl_section_body(
                        &body_children,
                        None,
                        align_fields,
                        align_props,
                    );
                    parts.push(doc::indent(aligned));
                } else {
                    let body_parts = self.build_type_body_parts(&body_children);
                    parts.push(doc::indent(doc::concat(body_parts)));
                }
            } else {
                let body_parts = self.build_type_body_parts(&body_children);
                parts.push(doc::concat(body_parts));
            }
        }
        doc::concat(parts)
    }

    /// Build body parts from collected children using the original
    /// non-aligned logic (Hardline separation, blank line preservation).
    fn build_type_body_parts(&self, body_children: &[Node<'a>]) -> Vec<Doc> {
        let mut body_parts = Vec::new();
        let mut prev_body_kind: &'static str = "";
        let mut prev_body_end_row: Option<usize> = None;

        for child in body_children {
            if child.kind() == K::DECL_SECTION || child.kind() == K::PP_DECL_SECTION {
                if let Some(prev_end) = prev_body_end_row {
                    if self.has_blank_line_between(prev_end, child.start_position().row) {
                        body_parts.push(Doc::BlankLine);
                    }
                }
                body_parts.push(self.doc_for_node(*child));
                prev_body_kind = child.kind();
                prev_body_end_row = Some(child.end_position().row);
                continue;
            }

            let child_doc = self.doc_for_node(*child);
            let prev_ends = body_parts
                .last()
                .is_some_and(crate::doc_builder::ends_with_hardline);
            if !prev_body_kind.is_empty()
                && !prev_ends
                && !crate::doc_builder::starts_with_hardline(&child_doc)
            {
                body_parts.push(Doc::Hardline);
            }
            body_parts.push(child_doc);
            prev_body_kind = child.kind();
            prev_body_end_row = Some(child.end_position().row);
        }
        body_parts
    }
}
