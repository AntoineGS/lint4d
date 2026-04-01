use crate::printer::Printer;
use pascal_core::node_kind as K;
use tree_sitter::Node;

impl<'a> Printer<'a> {
    pub(crate) fn print_unit(&mut self, node: Node) {
        // Children: kUnit, moduleName, `;`, interface, implementation, kEnd, kEndDot
        let mut prev_kind = String::new();
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            let kind = child.kind().to_string();
            // Blank line before interface, implementation, initialization, finalization, and final kEnd
            if (kind == K::INTERFACE
                || kind == K::IMPLEMENTATION
                || kind == K::INITIALIZATION
                || kind == K::FINALIZATION
                || kind == K::K_END)
                && !prev_kind.is_empty()
            {
                self.ensure_newline();
                self.emit_newline();
            }
            self.print_node(child);
            prev_kind = kind;
        }
    }

    pub(crate) fn print_interface_section(&mut self, node: Node) {
        // Children: kInterface, then declUses/declTypes/declVars/declConsts etc.
        let mut after_header = false;
        let mut prev_child_kind = String::new();
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            match child.kind() {
                K::K_INTERFACE => {
                    self.emit_indented("interface");
                    self.last_token_kind = K::K_INTERFACE.to_string();
                    self.ensure_newline();
                    after_header = true;
                }
                _ => {
                    let kind = child.kind().to_string();
                    if after_header {
                        self.emit_newline();
                        after_header = false;
                    } else if !prev_child_kind.is_empty() {
                        // Add blank line between sections (uses→types, types→vars, etc.)
                        let blanks = crate::blank_lines::needs_blank_line_between(
                            &prev_child_kind,
                            &kind,
                            &self.config.blank_lines,
                        );
                        for _ in 0..blanks {
                            self.ensure_newline();
                            self.emit_newline();
                        }
                    }
                    self.print_node(child);
                    prev_child_kind = kind;
                }
            }
        }
    }

    pub(crate) fn print_implementation_section(&mut self, node: Node) {
        // Children: kImplementation, then defProc/declUses/etc.
        let mut after_header = false;
        let mut prev_child_kind = String::new();
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            match child.kind() {
                K::K_IMPLEMENTATION => {
                    self.emit_indented("implementation");
                    self.last_token_kind = K::K_IMPLEMENTATION.to_string();
                    self.ensure_newline();
                    after_header = true;
                }
                _ => {
                    let kind = child.kind().to_string();
                    // Blank line after header or between top-level decls
                    if after_header {
                        self.emit_newline();
                        after_header = false;
                    } else if !prev_child_kind.is_empty() {
                        let blanks = crate::blank_lines::needs_blank_line_between(
                            &prev_child_kind,
                            &kind,
                            &self.config.blank_lines,
                        );
                        for _ in 0..blanks {
                            self.ensure_newline();
                            self.emit_newline();
                        }
                    }
                    self.print_node(child);
                    prev_child_kind = kind;
                }
            }
        }
    }

    pub(crate) fn print_init_final_section(&mut self, node: Node) {
        // Children: kInitialization/kFinalization, then statement(s)
        let mut after_header = false;
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            match child.kind() {
                K::K_INITIALIZATION | K::K_FINALIZATION => {
                    let text = self.node_text(child);
                    self.emit_indented(&text);
                    self.last_token_kind = child.kind().to_string();
                    self.ensure_newline();
                    self.indent.indent();
                    after_header = true;
                }
                _ => {
                    self.print_node(child);
                }
            }
        }
        if after_header {
            self.indent.dedent();
        }
    }
}
