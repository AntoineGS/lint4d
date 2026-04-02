use crate::doc::{self, Doc};
use crate::doc_builder::DocBuilder;
use pascal_core::node_kind as K;
use tree_sitter::Node;

impl<'a> DocBuilder<'a> {
    pub(crate) fn build_type_body(&self, node: Node<'a>) -> Doc {
        let children = self.code_children(node);
        let has_visibility = children.iter().any(|c| c.kind() == K::DECL_SECTION);
        let mut parts = Vec::new();
        let mut body_parts = Vec::new();
        let mut in_ancestor_list = false;

        for child in &children {
            match child.kind() {
                K::K_CLASS | K::K_RECORD | K::K_INTERFACE | K::K_OBJECT => {
                    parts.push(self.doc_for_node(*child));
                    in_ancestor_list = true;
                }
                K::OPEN_PAREN if in_ancestor_list => {
                    parts.push(Doc::Raw("(".into()));
                }
                K::CLOSE_PAREN if in_ancestor_list => {
                    parts.push(Doc::Raw(")".into()));
                    in_ancestor_list = false;
                    parts.push(Doc::Hardline);
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
                        parts.push(Doc::Hardline);
                    }
                    // Flush body with indent if no visibility sections
                    if !body_parts.is_empty() {
                        if !has_visibility {
                            parts.push(doc::indent(doc::concat(std::mem::take(&mut body_parts))));
                        } else {
                            parts.push(doc::concat(std::mem::take(&mut body_parts)));
                        }
                    }
                    parts.push(Doc::Hardline);
                    parts.push(self.doc_for_node(*child));
                }
                K::DECL_SECTION => {
                    if in_ancestor_list {
                        in_ancestor_list = false;
                        parts.push(Doc::Hardline);
                    }
                    body_parts.push(self.doc_for_node(*child));
                }
                _ => {
                    if in_ancestor_list {
                        in_ancestor_list = false;
                        parts.push(Doc::Hardline);
                    }
                    body_parts.push(self.doc_for_node(*child));
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
