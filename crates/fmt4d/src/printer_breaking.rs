use crate::printer::Printer;
use tree_sitter::Node;

impl<'a> Printer<'a> {
    /// Emit a `declArgs` parameter list with line breaks at `;` separators.
    ///
    /// Groups parameters by `;`-separated chunks, then greedily packs them
    /// onto lines respecting `max_line_length`.
    pub(crate) fn print_args_breaking(&mut self, node: Node) {
        let children: Vec<Node> = node
            .children(&mut node.walk())
            .filter(|c| !c.is_extra())
            .collect();

        // Find the positions of `(` and `)` tokens.
        let open_idx = children.iter().position(|c| c.kind() == "(");
        let close_idx = children.iter().rposition(|c| c.kind() == ")");

        let (open_idx, close_idx) = match (open_idx, close_idx) {
            (Some(o), Some(c)) => (o, c),
            _ => {
                // Fallback: no parens found, just recurse normally.
                self.recurse_children(node);
                return;
            }
        };

        // Emit everything up to and including `(`.
        for child in &children[..=open_idx] {
            self.print_node(*child);
        }

        // Collect the inner children (between `(` and `)`).
        let inner = &children[open_idx + 1..close_idx];

        if inner.is_empty() {
            // Empty param list — just emit `)`.
            self.print_node(children[close_idx]);
            // Emit anything after `)` (unlikely but safe).
            for child in &children[close_idx + 1..] {
                self.print_node(*child);
            }
            return;
        }

        // Split inner children into groups separated by `;`.
        // The `;` stays with the group it terminates.
        let groups = split_at_semicolons(inner);

        // Continuation indent = current indent + one extra indent_size.
        let cont_indent = self.indent.current().len() + self.config.indent_size;
        let cont_indent_str = " ".repeat(cont_indent);

        // Track whether we're at the start of a (possibly continuation) line
        // within the param list.
        let mut first_group = true;

        for group in &groups {
            // Measure this group's width.
            let (width, _last_kind, _last_parent) = self.measure_group(group);

            // For the first group it follows directly after `(`, so no extra
            // space is needed. For subsequent groups, the preceding `;` has
            // already been emitted (with a trailing space from print_leaf).
            // We need to check if the group fits on the current line.

            if first_group {
                // First group always starts right after `(`.
                // Check if it fits; if not, break to continuation line.
                if self.current_column + width > self.max_line_length {
                    self.output.push('\n');
                    self.output.push_str(&cont_indent_str);
                    self.at_line_start = false;
                    self.current_column = cont_indent;
                }
                for child in group {
                    self.print_node(*child);
                }
                first_group = false;
            } else {
                // Subsequent group: the `;` and space from the previous group
                // have already been emitted. Check if this group fits.
                if self.current_column + width > self.max_line_length {
                    // The space after `;` was already pushed by print_leaf.
                    // Remove it and replace with newline + continuation indent.
                    if self.output.ends_with(' ') {
                        self.output.pop();
                        self.current_column -= 1;
                    }
                    self.output.push('\n');
                    self.output.push_str(&cont_indent_str);
                    self.at_line_start = false;
                    self.current_column = cont_indent;

                    // After a line break, the measurement assumed prev_kind = ";"
                    // which means no space before the next token. But now we're
                    // at the start of a continuation line and the spacing is
                    // handled by the indent. We need to make sure print_node
                    // doesn't add a spurious space. Since at_line_start is false
                    // and last_token_kind is ";", needs_space_before will return
                    // false for most tokens after ";". That's correct because the
                    // indent already provides the positioning.
                }
                for child in group {
                    self.print_node(*child);
                }
            }
        }

        // Emit the closing `)`.
        self.print_node(children[close_idx]);

        // Emit anything after `)` (unlikely but safe).
        for child in &children[close_idx + 1..] {
            self.print_node(*child);
        }
    }
}

/// Split a slice of nodes into groups separated by `;` tokens.
/// The `;` node stays at the end of the group it terminates.
fn split_at_semicolons<'a>(nodes: &[Node<'a>]) -> Vec<Vec<Node<'a>>> {
    let mut groups: Vec<Vec<Node>> = Vec::new();
    let mut current_group: Vec<Node> = Vec::new();

    for node in nodes {
        current_group.push(*node);
        if node.kind() == ";" {
            groups.push(current_group);
            current_group = Vec::new();
        }
    }

    // Last group (after the last `;` or if there's no `;`).
    if !current_group.is_empty() {
        groups.push(current_group);
    }

    groups
}
