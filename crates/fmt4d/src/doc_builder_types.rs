use crate::doc::{self, Doc};
use crate::doc_builder::{strip_trailing_hardline, DocBuilder};
use pascal_core::node_kind as K;
use tree_sitter::Node;

impl<'a> DocBuilder<'a> {
    pub(crate) fn build_type_body(&self, node: Node<'a>) -> Doc {
        let children = self.code_children(node);
        let has_visibility = children.iter().any(|c| c.kind() == K::DECL_SECTION);
        let has_end = children.iter().any(|c| c.kind() == K::K_END);
        let mut parts = Vec::new();
        let mut body_parts = Vec::new();
        let mut in_ancestor_list = false;
        let mut prev_body_kind = String::new();

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
                    // Only emit Hardline if next child is NOT a DECL_SECTION
                    // (DECL_SECTION provides its own leading Hardline)
                    let next_is_section = children
                        .get(idx + 1)
                        .is_some_and(|n| n.kind() == K::DECL_SECTION);
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
                    // Flush body with indent if no visibility sections
                    if !body_parts.is_empty() {
                        let body_doc = if !has_visibility {
                            doc::indent(doc::concat(std::mem::take(&mut body_parts)))
                        } else {
                            doc::concat(std::mem::take(&mut body_parts))
                        };
                        parts.push(strip_trailing_hardline(body_doc));
                    }
                    parts.push(Doc::Hardline);
                    parts.push(self.doc_for_node(*child));
                }
                K::DECL_SECTION => {
                    if in_ancestor_list {
                        in_ancestor_list = false;
                        // No Hardline needed — build_decl_section starts with
                        // its own leading Hardline.
                    }
                    body_parts.push(self.doc_for_node(*child));
                    prev_body_kind = K::DECL_SECTION.to_string();
                }
                _ => {
                    if in_ancestor_list {
                        in_ancestor_list = false;
                        if has_end && child.kind() != K::DECL_SECTION {
                            parts.push(Doc::Hardline);
                        }
                    }
                    // Add Hardline between body items only if neither the
                    // previous doc ends with one nor the current doc starts
                    // with one (e.g. interface methods from build_decl_proc
                    // already end with Hardline).
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
                    prev_body_kind = child.kind().to_string();
                }
            }
        }
        // Flush remaining body (e.g., if there was no kEnd somehow)
        if !body_parts.is_empty() {
            if !has_visibility {
                parts.push(doc::indent(doc::concat(body_parts)));
            } else {
                parts.push(doc::concat(body_parts));
            }
        }
        doc::concat(parts)
    }
}
