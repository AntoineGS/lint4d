use crate::printer::Printer;
use pascal_core::node_kind as K;
use tree_sitter::Node;

impl<'a> Printer<'a> {
    pub(crate) fn print_type_body(&mut self, node: Node) {
        // The opening keyword (class/record/interface) comes first.
        // Then optionally an ancestor list `(...)`, then body children, then kEnd.
        let children: Vec<Node> = node
            .children(&mut node.walk())
            .filter(|c| !c.is_extra())
            .collect();

        // Detect whether this type body has declSection children (visibility
        // sections like private/public). If not, we need to indent the body
        // fields ourselves (e.g. plain records without visibility).
        let has_visibility_sections = children.iter().any(|c| c.kind() == K::DECL_SECTION);
        let mut body_indented = false;
        // Track whether we're still in the ancestor list portion (before
        // the first body child or kEnd).
        let mut in_ancestor_list = false;

        for child in &children {
            match child.kind() {
                K::K_CLASS | K::K_RECORD | K::K_INTERFACE | K::K_OBJECT => {
                    // Emit the keyword on same line as `= class`
                    self.emit_token(child.kind(), &self.node_text(*child), "");
                    self.last_token_kind = child.kind().to_string();
                    in_ancestor_list = true;
                    // DON'T newline yet — ancestor list `(...)` may follow
                }
                // Ancestor list tokens: `(`, `)`, `,`, typeref — keep on same line
                K::OPEN_PAREN if in_ancestor_list => {
                    self.emit_raw("(");
                    self.last_token_kind = K::OPEN_PAREN.to_string();
                }
                K::CLOSE_PAREN if in_ancestor_list => {
                    self.emit_raw(")");
                    self.last_token_kind = K::CLOSE_PAREN.to_string();
                    in_ancestor_list = false;
                    self.ensure_newline();
                }
                K::COMMA if in_ancestor_list => {
                    self.emit_raw(",");
                    self.output.push(' ');
                    self.current_column += 1;
                    self.last_token_kind = K::COMMA.to_string();
                }
                K::TYPEREF if in_ancestor_list => {
                    let text = self.node_text(*child);
                    self.current_column += text.len();
                    self.output.push_str(&text);
                    self.last_token_kind = K::IDENTIFIER.to_string();
                }
                K::K_END => {
                    // End ancestor list if still in it (e.g. empty class with no ancestors)
                    if in_ancestor_list {
                        in_ancestor_list = false;
                        self.ensure_newline();
                    }
                    if body_indented {
                        self.indent.dedent();
                        body_indented = false;
                    }
                    self.ensure_newline();
                    self.emit_indented("end");
                    self.last_token_kind = K::K_END.to_string();
                }
                K::DECL_SECTION => {
                    if in_ancestor_list {
                        in_ancestor_list = false;
                        self.ensure_newline();
                    }
                    self.print_decl_section(*child);
                }
                _ => {
                    if in_ancestor_list {
                        in_ancestor_list = false;
                        self.ensure_newline();
                    }
                    // For records/types without visibility sections, indent body fields
                    if !has_visibility_sections && !body_indented && child.kind() != K::SEMICOLON {
                        self.indent.indent();
                        body_indented = true;
                    }
                    self.print_node(*child);
                }
            }
        }
        if body_indented {
            self.indent.dedent();
        }
    }
}
