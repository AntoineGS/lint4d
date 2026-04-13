use pascal_core::node_kind as K;
use std::collections::HashMap;
use tree_sitter::Node;

/// A directive attached to a code node.
#[derive(Debug, Clone)]
pub struct AttachedDirective {
    /// The directive text (e.g. `{$R *.dfm}`, `{$HINTS OFF}`).
    pub text: String,
    /// True if the directive appeared on the same line as code (trailing).
    #[allow(dead_code)] // Phase F follow-up: used for trailing-directive placement.
    pub trailing: bool,
    /// Gap in bytes between preceding token and this directive (trailing only).
    pub gap: usize,
}

/// Maps node IDs to their attached standalone directives.
///
/// Mirrors `CommentMap` but for `ppDirective` extra nodes that are not
/// part of a structured `ppBlock` or `ppUsesBlock`.
#[derive(Debug)]
pub struct DirectiveMap {
    leading: HashMap<usize, Vec<AttachedDirective>>,
    trailing: HashMap<usize, Vec<AttachedDirective>>,
}

impl DirectiveMap {
    /// Scan all `ppDirective` extra nodes and attach them to nearby code nodes.
    pub fn build(root: Node, source: &[u8]) -> Self {
        let mut directives = Vec::new();
        collect_directives(root, source, &mut directives);

        let mut leaves = Vec::new();
        collect_leaves(root, &mut leaves);

        let mut leading: HashMap<usize, Vec<AttachedDirective>> = HashMap::new();
        let mut trailing: HashMap<usize, Vec<AttachedDirective>> = HashMap::new();

        for (dir_node, text) in &directives {
            let dir_line = dir_node.start_position().row;

            // Check if there is a non-extra token on the same line before
            // the directive — that makes it trailing.
            if let Some(prev) = find_prev_leaf(&leaves, *dir_node) {
                if prev.end_position().row == dir_line {
                    let gap = dir_node.start_byte().saturating_sub(prev.end_byte());
                    trailing
                        .entry(prev.id())
                        .or_default()
                        .push(AttachedDirective {
                            text: text.clone(),
                            trailing: true,
                            gap,
                        });
                    continue;
                }
            }

            // Otherwise it is a leading directive for the next code node.
            if let Some(next) = find_next_leaf(&leaves, *dir_node) {
                leading
                    .entry(next.id())
                    .or_default()
                    .push(AttachedDirective {
                        text: text.clone(),
                        trailing: false,
                        gap: 0,
                    });
            }
        }

        DirectiveMap { leading, trailing }
    }

    /// Get leading directives for a node.
    pub fn leading_directives(&self, node_id: usize) -> &[AttachedDirective] {
        self.leading.get(&node_id).map_or(&[], |v| v.as_slice())
    }

    /// Get trailing directives for a node.
    pub fn trailing_directives(&self, node_id: usize) -> &[AttachedDirective] {
        self.trailing.get(&node_id).map_or(&[], |v| v.as_slice())
    }

    /// Returns an empty DirectiveMap (no directives attached).
    #[allow(dead_code)] // Phase F follow-up: wire into builder fall-back paths.
    pub fn empty() -> Self {
        DirectiveMap {
            leading: HashMap::new(),
            trailing: HashMap::new(),
        }
    }
}

/// Collect all `ppDirective` extra nodes from the tree.
fn collect_directives<'a>(node: Node<'a>, source: &[u8], out: &mut Vec<(Node<'a>, String)>) {
    if node.is_extra() && node.kind() == K::PP_DIRECTIVE {
        let text = pascal_core::decode_bytes(&source[node.start_byte()..node.end_byte()])
            .replace('\r', "");
        out.push((node, text));
        return;
    }
    for child in node.children(&mut node.walk()) {
        collect_directives(child, source, out);
    }
}

/// Find the previous leaf node (non-extra) before `target`.
fn find_prev_leaf<'a>(leaves: &[Node<'a>], target: Node<'a>) -> Option<Node<'a>> {
    let target_start = target.start_byte();
    let idx = leaves.partition_point(|leaf| leaf.start_byte() < target_start);
    if idx > 0 { Some(leaves[idx - 1]) } else { None }
}

/// Find the next non-extra node after `target`.
fn find_next_leaf<'a>(leaves: &[Node<'a>], target: Node<'a>) -> Option<Node<'a>> {
    let target_end = target.end_byte();
    let idx = leaves.partition_point(|leaf| leaf.start_byte() < target_end);
    leaves.get(idx).copied()
}

/// Collect all leaf nodes (child_count == 0, not extra) in source order.
fn collect_leaves<'a>(node: Node<'a>, out: &mut Vec<Node<'a>>) {
    if node.is_extra() {
        return;
    }
    if node.child_count() == 0 {
        out.push(node);
        return;
    }
    for child in node.children(&mut node.walk()) {
        collect_leaves(child, out);
    }
}
