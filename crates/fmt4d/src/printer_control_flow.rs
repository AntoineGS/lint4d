use crate::printer::Printer;
use pascal_core::node_kind as K;
use tree_sitter::Node;

impl<'a> Printer<'a> {
    pub(crate) fn print_try(&mut self, node: Node) {
        self.emit_leading_comments(node);
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            self.print_node(child);
        }
    }

    pub(crate) fn print_case(&mut self, node: Node) {
        self.emit_leading_comments(node);
        let mut after_of = false;
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            match child.kind() {
                K::K_CASE => {
                    self.emit_indented("case");
                    self.last_token_kind = K::K_CASE.to_string();
                }
                K::K_OF => {
                    self.output.push(' ');
                    self.current_column += 1;
                    self.output.push_str("of");
                    self.current_column += "of".len();
                    self.last_token_kind = K::K_OF.to_string();
                    self.ensure_newline();
                    self.indent.indent();
                    after_of = true;
                }
                K::CASE_CASE => {
                    self.print_case_branch(child);
                }
                K::K_ELSE => {
                    // `else` aligns with `case`, not with the branches
                    self.indent.dedent();
                    self.emit_leading_comments(child);
                    self.ensure_newline();
                    self.emit_indented("else");
                    self.last_token_kind = K::K_ELSE.to_string();
                    self.ensure_newline();
                    self.indent.indent();
                    // after_of stays true — kEnd will do the final dedent
                }
                K::K_END => {
                    if after_of {
                        self.indent.dedent();
                        after_of = false;
                    }
                    self.emit_leading_comments(child);
                    self.ensure_newline();
                    self.emit_indented("end");
                    self.last_token_kind = K::K_END.to_string();
                }
                _ => {
                    self.print_node(child);
                }
            }
        }
        if after_of {
            self.indent.dedent();
        }
    }

    pub(crate) fn print_case_branch(&mut self, node: Node) {
        self.emit_leading_comments(node);
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            self.print_node(child);
        }
    }

    pub(crate) fn print_repeat(&mut self, node: Node) {
        self.emit_leading_comments(node);
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            self.print_node(child);
        }
    }

    pub(crate) fn print_if(&mut self, node: Node) {
        self.emit_leading_comments(node);

        // Measure the entire if node. If it overflows, delegate to the
        // breaking path which splits the condition at and/or operators.
        let (width, _, _) = self.measure_node(
            node,
            &self.last_token_kind.clone(),
            &self.last_token_parent_kind.clone(),
        );
        if self.current_column + width > self.max_line_length {
            self.print_if_breaking(node);
            return;
        }

        // ── Short path: everything fits on one line ─────────────
        let children: Vec<Node> = node
            .children(&mut node.walk())
            .filter(|c| !c.is_extra())
            .collect();

        let mut i = 0;
        while i < children.len() {
            let child = children[i];
            match child.kind() {
                K::K_THEN | K::K_DO => {
                    // Emit the keyword on the same line
                    self.print_node(child);
                    // If the NEXT child is a single statement (not begin..end),
                    // put it on its own indented line.
                    if let Some(next) = children.get(i + 1) {
                        if next.kind() != K::BLOCK && next.kind() != K::K_ELSE {
                            self.ensure_newline();
                            self.indent.indent();
                            self.print_node(*next);
                            self.indent.dedent();
                            i += 2;
                            continue;
                        }
                    }
                }
                K::K_ELSE => {
                    // `else` aligns with `if` — no extra indent
                    self.ensure_newline();
                    self.emit_indented("else");
                    self.last_token_kind = K::K_ELSE.to_string();
                    // If next child is a single statement (not begin..end or another if),
                    // put it on its own indented line.
                    if let Some(next) = children.get(i + 1) {
                        if next.kind() != K::BLOCK
                            && next.kind() != K::IF
                            && next.kind() != K::IF_ELSE
                        {
                            self.ensure_newline();
                            self.indent.indent();
                            self.print_node(*next);
                            self.indent.dedent();
                            i += 2;
                            continue;
                        }
                    }
                }
                _ => {
                    self.print_node(child);
                }
            }
            i += 1;
        }
    }

    pub(crate) fn print_loop(&mut self, node: Node) {
        self.emit_leading_comments(node);
        let children: Vec<Node> = node
            .children(&mut node.walk())
            .filter(|c| !c.is_extra())
            .collect();
        let mut i = 0;
        while i < children.len() {
            let child = children[i];
            match child.kind() {
                K::K_DO => {
                    // Emit `do` on the same line
                    self.print_node(child);
                    // If next child is a single statement (not begin..end),
                    // put it on its own indented line.
                    if let Some(next) = children.get(i + 1) {
                        if next.kind() != K::BLOCK {
                            self.ensure_newline();
                            self.indent.indent();
                            self.print_node(*next);
                            self.indent.dedent();
                            i += 2;
                            continue;
                        }
                    }
                }
                _ => {
                    self.print_node(child);
                }
            }
            i += 1;
        }
    }
}
