use crate::printer::Printer;
use pascal_core::node_kind as K;
use tree_sitter::Node;

/// A segment of a flattened binary expression chain.
///
/// The first segment has `operator = None`. Subsequent segments start with
/// the `kAnd` / `kOr` operator node, followed by the operand nodes.
struct ConditionSegment<'a> {
    /// The `kAnd` or `kOr` operator node (absent for the first segment).
    operator: Option<Node<'a>>,
    /// The operand nodes that follow the operator (or all nodes for segment 0).
    operand: Vec<Node<'a>>,
}

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
        let open_idx = children.iter().position(|c| c.kind() == K::OPEN_PAREN);
        let close_idx = children.iter().rposition(|c| c.kind() == K::CLOSE_PAREN);

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

    /// Emit an `if` / `ifElse` node, breaking the condition at `kAnd` / `kOr`
    /// operators when it overflows `max_line_length`.
    pub(crate) fn print_if_breaking(&mut self, node: Node) {
        let children: Vec<Node> = node
            .children(&mut node.walk())
            .filter(|c| !c.is_extra())
            .collect();

        let mut i = 0;
        while i < children.len() {
            let child = children[i];
            match child.kind() {
                K::K_IF => {
                    // Emit "if" normally.
                    self.print_node(child);

                    // Collect all children between kIf and kThen/kDo — these
                    // are the condition nodes.
                    let cond_start = i + 1;
                    let cond_end = children[cond_start..]
                        .iter()
                        .position(|c| c.kind() == K::K_THEN || c.kind() == K::K_DO)
                        .map(|p| cond_start + p)
                        .unwrap_or(children.len());

                    let condition_nodes = &children[cond_start..cond_end];
                    self.emit_condition_with_breaks(condition_nodes);

                    // Skip past the condition nodes; the loop will handle
                    // kThen/kDo on the next iteration.
                    i = cond_end;
                    continue;
                }
                K::K_THEN | K::K_DO => {
                    // Emit the keyword on the same line (or after condition).
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
                    // `else` aligns with `if` — no extra indent.
                    self.ensure_newline();
                    self.emit_indented("else");
                    self.last_token_kind = K::K_ELSE.to_string();
                    // If next child is a single statement (not begin..end or
                    // another if), put it on its own indented line.
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

    /// Break a condition expression at top-level `kAnd` / `kOr` operators.
    ///
    /// Flattens the left-recursive `exprBinary` chain and emits each segment
    /// on a new continuation-indented line when it would overflow.
    fn emit_condition_with_breaks(&mut self, nodes: &[Node]) {
        // If there's a single exprBinary node, flatten its chain to find
        // top-level and/or operators.  Otherwise treat the nodes as-is.
        let segments = if nodes.len() == 1 && nodes[0].kind() == K::EXPR_BINARY {
            flatten_binary_chain(nodes[0])
        } else {
            // Multiple condition nodes at the same level — look for kAnd/kOr
            // among them directly.
            split_at_and_or(nodes)
        };

        // If flattening found no break points (no top-level kAnd/kOr), just
        // emit all nodes normally.
        if segments.len() <= 1 {
            for node in nodes {
                self.print_node(*node);
            }
            return;
        }

        // Continuation indent = current indent + one indent_size.
        let cont_indent = self.indent.current().len() + self.config.indent_size;
        let cont_indent_str = " ".repeat(cont_indent);

        for (idx, seg) in segments.iter().enumerate() {
            if idx == 0 {
                // First segment: emit normally right after "if ".
                for n in &seg.operand {
                    self.print_node(*n);
                }
            } else {
                // Subsequent segments: measure operator + operand width.
                let op_node = seg.operator.unwrap();
                let mut width = 0usize;
                let mut cur_kind = self.last_token_kind.clone();
                let mut cur_parent = self.last_token_parent_kind.clone();

                // Measure the operator.
                let (w, k, p) = self.measure_node(op_node, &cur_kind, &cur_parent);
                width += w;
                cur_kind = k;
                cur_parent = p;

                // Measure each operand node.
                for n in &seg.operand {
                    let (w, k, p) = self.measure_node(*n, &cur_kind, &cur_parent);
                    width += w;
                    cur_kind = k;
                    cur_parent = p;
                }

                if self.current_column + width > self.max_line_length {
                    // Break before the operator: newline + continuation indent.
                    self.output.push('\n');
                    self.output.push_str(&cont_indent_str);
                    self.at_line_start = false;
                    self.current_column = cont_indent;
                }

                // Emit operator + operand nodes.
                self.print_node(op_node);
                for n in &seg.operand {
                    self.print_node(*n);
                }
            }
        }
    }

    /// Emit an expression with line breaks before binary operators when the
    /// expression would overflow `max_line_length`.
    ///
    /// Flattens the left-recursive `exprBinary` chain and emits each segment
    /// on a new continuation-indented line when it would overflow. The operator
    /// leads the continuation line (break BEFORE the operator).
    ///
    /// Unlike `emit_condition_with_breaks`, this applies to ALL binary operators
    /// (arithmetic, logical, bitwise), not just `kAnd`/`kOr`.
    pub(crate) fn print_expression_breaking(&mut self, node: Node) {
        // Flatten the binary chain at any operator level.
        let segments = if node.kind() == K::EXPR_BINARY {
            flatten_binary_chain_any_op(node)
        } else {
            // Not a direct exprBinary — try to find one inside.
            // Look for an exprBinary child.
            let binary_child = node
                .children(&mut node.walk())
                .find(|c| c.kind() == K::EXPR_BINARY);
            if let Some(bin) = binary_child {
                flatten_binary_chain_any_op(bin)
            } else {
                // No breakable chain found — just recurse normally.
                self.recurse_children(node);
                return;
            }
        };

        // If flattening found no break points (single segment), just recurse.
        if segments.len() <= 1 {
            self.recurse_children(node);
            return;
        }

        // Continuation indent = current indent + one indent_size.
        let cont_indent = self.indent.current().len() + self.config.indent_size;
        let cont_indent_str = " ".repeat(cont_indent);

        // If node is not exprBinary itself, we need to emit the parts before and
        // after the binary expression. But for the common case (assignment),
        // the node passed in is the exprBinary directly (called from the catch-all).
        // We just emit the segments with breaks.
        for (idx, seg) in segments.iter().enumerate() {
            if idx == 0 {
                // First segment: emit normally.
                for n in &seg.operand {
                    self.print_node(*n);
                }
            } else {
                // Subsequent segments: measure operator + operand width.
                let op_node = seg.operator.unwrap();
                let mut width = 0usize;
                let mut cur_kind = self.last_token_kind.clone();
                let mut cur_parent = self.last_token_parent_kind.clone();

                // Measure the operator.
                let (w, k, p) = self.measure_node(op_node, &cur_kind, &cur_parent);
                width += w;
                cur_kind = k;
                cur_parent = p;

                // Measure each operand node.
                for n in &seg.operand {
                    let (w, k, p) = self.measure_node(*n, &cur_kind, &cur_parent);
                    width += w;
                    cur_kind = k;
                    cur_parent = p;
                }

                if self.current_column + width > self.max_line_length {
                    // Break before the operator: newline + continuation indent.
                    self.output.push('\n');
                    self.output.push_str(&cont_indent_str);
                    self.at_line_start = false;
                    self.current_column = cont_indent;
                }

                // Emit operator + operand nodes.
                self.print_node(op_node);
                for n in &seg.operand {
                    self.print_node(*n);
                }
            }
        }
    }

    /// Emit an `exprCall` function call with line breaks at `,` separators
    /// when the call would overflow `max_line_length`.
    ///
    /// Groups arguments by `,`-separated chunks, then greedily packs them
    /// onto lines respecting `max_line_length`. The commas are emitted by
    /// this method (not taken from the AST), so spacing is handled correctly.
    pub(crate) fn print_call_args_breaking(&mut self, call_node: Node) {
        let children: Vec<Node> = call_node
            .children(&mut call_node.walk())
            .filter(|c| !c.is_extra())
            .collect();

        // Find the positions of `(` and `)` tokens.
        let open_idx = children.iter().position(|c| c.kind() == K::OPEN_PAREN);
        let close_idx = children.iter().rposition(|c| c.kind() == K::CLOSE_PAREN);

        let (open_idx, close_idx) = match (open_idx, close_idx) {
            (Some(o), Some(c)) => (o, c),
            _ => {
                // Fallback: no parens found, just recurse normally.
                self.recurse_children(call_node);
                return;
            }
        };

        // Emit everything up to and including `(` (the entity / function name).
        for child in &children[..=open_idx] {
            self.print_node(*child);
        }

        // Collect the inner children (between `(` and `)`), excluding the `,` separators.
        let inner = &children[open_idx + 1..close_idx];

        if inner.is_empty() {
            // Empty argument list — just emit `)`.
            self.print_node(children[close_idx]);
            for child in &children[close_idx + 1..] {
                self.print_node(*child);
            }
            return;
        }

        // Split inner children into argument groups, excluding the `,` nodes.
        let groups = split_at_commas(inner);

        // Continuation indent = current indent + one extra indent_size.
        let cont_indent = self.indent.current().len() + self.config.indent_size;
        let cont_indent_str = " ".repeat(cont_indent);

        let mut first_group = true;

        for group in &groups {
            let (width, _last_kind, _last_parent) = self.measure_group(group);

            if first_group {
                // First group starts right after `(`.
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
                // Subsequent group: we need to emit `, ` or `,\n<indent>`.
                // Check if `, ` + group fits on the current line.
                if self.current_column + 2 + width <= self.max_line_length {
                    // Fits on same line: emit `, ` then group.
                    self.output.push_str(", ");
                    self.current_column += 2;
                } else {
                    // Doesn't fit: emit `,\n<continuation indent>`.
                    self.output.push(',');
                    self.output.push('\n');
                    self.output.push_str(&cont_indent_str);
                    self.at_line_start = false;
                    self.current_column = cont_indent;
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

/// Split a slice of nodes into groups separated by `,` tokens.
/// The `,` nodes are excluded from all groups (the caller emits them).
fn split_at_commas<'a>(nodes: &[Node<'a>]) -> Vec<Vec<Node<'a>>> {
    let mut groups: Vec<Vec<Node>> = Vec::new();
    let mut current_group: Vec<Node> = Vec::new();

    for node in nodes {
        if node.kind() == K::COMMA {
            groups.push(current_group);
            current_group = Vec::new();
        } else {
            current_group.push(*node);
        }
    }

    // Last group (after the last `,` or if there's no `,`).
    if !current_group.is_empty() {
        groups.push(current_group);
    }

    groups
}

/// Split a slice of nodes into groups separated by `;` tokens.
/// The `;` node stays at the end of the group it terminates.
fn split_at_semicolons<'a>(nodes: &[Node<'a>]) -> Vec<Vec<Node<'a>>> {
    let mut groups: Vec<Vec<Node>> = Vec::new();
    let mut current_group: Vec<Node> = Vec::new();

    for node in nodes {
        current_group.push(*node);
        if node.kind() == K::SEMICOLON {
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

/// Flatten a left-recursive `exprBinary` chain into [`ConditionSegment`]s,
/// breaking at ANY binary operator (arithmetic, logical, bitwise).
///
/// Like `flatten_binary_chain` but treats all operator kinds as break-points,
/// not just `kAnd`/`kOr`. Used for assignment expression breaking.
fn flatten_binary_chain_any_op<'a>(node: Node<'a>) -> Vec<ConditionSegment<'a>> {
    let mut segments: Vec<ConditionSegment<'a>> = Vec::new();
    flatten_binary_chain_any_op_inner(node, &mut segments);
    segments
}

fn flatten_binary_chain_any_op_inner<'a>(node: Node<'a>, segments: &mut Vec<ConditionSegment<'a>>) {
    debug_assert_eq!(node.kind(), K::EXPR_BINARY);

    let children: Vec<Node<'a>> = node
        .children(&mut node.walk())
        .filter(|c| !c.is_extra())
        .collect();

    // An exprBinary has 3 children: [left, operator, right].
    if children.len() != 3 {
        // Unexpected shape — treat entire node as one atomic segment.
        segments.push(ConditionSegment {
            operator: None,
            operand: vec![node],
        });
        return;
    }

    let left = children[0];
    let op = children[1];
    let right = children[2];

    // Recurse into the left child if it's another bare exprBinary
    // (not wrapped in parens). We always recurse for any operator type.
    if left.kind() == K::EXPR_BINARY {
        flatten_binary_chain_any_op_inner(left, segments);
    } else {
        segments.push(ConditionSegment {
            operator: None,
            operand: vec![left],
        });
    }

    // The operator + right operand form a new segment.
    segments.push(ConditionSegment {
        operator: Some(op),
        operand: vec![right],
    });
}

/// Flatten a left-recursive `exprBinary` chain into [`ConditionSegment`]s.
///
/// The tree-sitter grammar produces left-associative binary expressions:
///
/// ```text
/// exprBinary("A and B and C")
///   exprBinary("A and B")
///     A
///     kAnd
///     B
///   kAnd
///   C
/// ```
///
/// This function walks the left spine, collecting operand + operator pairs.
/// Only `kAnd` / `kOr` operators at the `exprBinary` level (not inside
/// `exprParens`) are treated as break-points.
fn flatten_binary_chain<'a>(node: Node<'a>) -> Vec<ConditionSegment<'a>> {
    let mut segments: Vec<ConditionSegment<'a>> = Vec::new();
    flatten_binary_chain_inner(node, &mut segments);
    segments
}

fn flatten_binary_chain_inner<'a>(node: Node<'a>, segments: &mut Vec<ConditionSegment<'a>>) {
    debug_assert_eq!(node.kind(), K::EXPR_BINARY);

    let children: Vec<Node<'a>> = node
        .children(&mut node.walk())
        .filter(|c| !c.is_extra())
        .collect();

    // An exprBinary has 3 children: [left, operator, right].
    // The operator must be kAnd or kOr to be a breakable point.
    if children.len() != 3 {
        // Unexpected shape — treat entire node as one atomic segment.
        segments.push(ConditionSegment {
            operator: None,
            operand: vec![node],
        });
        return;
    }

    let left = children[0];
    let op = children[1];
    let right = children[2];

    if op.kind() != K::K_AND && op.kind() != K::K_OR {
        // Not a logical operator — treat entire node as atomic.
        segments.push(ConditionSegment {
            operator: None,
            operand: vec![node],
        });
        return;
    }

    // Recurse into the left child if it's another bare exprBinary
    // (not wrapped in parens).
    if left.kind() == K::EXPR_BINARY {
        flatten_binary_chain_inner(left, segments);
    } else {
        segments.push(ConditionSegment {
            operator: None,
            operand: vec![left],
        });
    }

    // The operator + right operand form a new segment.
    segments.push(ConditionSegment {
        operator: Some(op),
        operand: vec![right],
    });
}

/// Split a flat slice of nodes at `kAnd` / `kOr` tokens.
///
/// Used when the condition between `kIf` and `kThen` consists of multiple
/// sibling nodes (rather than a single `exprBinary` wrapper).
fn split_at_and_or<'a>(nodes: &[Node<'a>]) -> Vec<ConditionSegment<'a>> {
    let mut segments: Vec<ConditionSegment<'a>> = Vec::new();
    let mut current_operand: Vec<Node<'a>> = Vec::new();

    for node in nodes {
        if node.kind() == K::K_AND || node.kind() == K::K_OR {
            // Flush current operand as a segment, then start a new one
            // with this operator.
            if !current_operand.is_empty() {
                segments.push(ConditionSegment {
                    operator: None,
                    operand: std::mem::take(&mut current_operand),
                });
            }
            // The next operand will be collected in the following iterations.
            // We create a partial segment with the operator and empty operand
            // for now — the operand will be filled by subsequent nodes.
            segments.push(ConditionSegment {
                operator: Some(*node),
                operand: Vec::new(),
            });
        } else if let Some(last_seg) = segments.last_mut() {
            if last_seg.operator.is_some() && last_seg.operand.is_empty() {
                // We're filling in the operand for a segment that has an
                // operator but no operand yet.
                last_seg.operand.push(*node);
            } else {
                current_operand.push(*node);
            }
        } else {
            current_operand.push(*node);
        }
    }

    // Flush any remaining operand.
    if !current_operand.is_empty() {
        segments.push(ConditionSegment {
            operator: None,
            operand: current_operand,
        });
    }

    segments
}
