use pascal_core::directive_fragment_rewrite::DirectivePatch;
use pascal_core::node_kind as K;
use std::collections::HashMap;
use tree_sitter::Node;

/// A directive synthesized from a `DirectivePatch` returned by the
/// partial-control-flow rewrite pass. Structurally identical to a real
/// `ppDirective` node for attachment purposes, but carries explicit byte
/// offsets and row/col positions instead of referencing a tree-sitter node.
#[derive(Debug, Clone)]
struct VirtualDirective {
    start_byte: usize,
    end_byte: usize,
    text: String,
    row: usize,
}

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
    #[allow(dead_code)]
    pub fn build(root: Node, source: &[u8]) -> Self {
        Self::build_with_patches(root, source, &[])
    }

    /// Like [`DirectiveMap::build`], but also folds `DirectivePatch` records
    /// from the partial-control-flow rewrite pass into the map as virtual
    /// directives.
    pub fn build_with_patches(root: Node, source: &[u8], patches: &[DirectivePatch]) -> Self {
        let mut directives = Vec::new();
        collect_directives(root, source, &mut directives);

        let mut leaves = Vec::new();
        collect_leaves(root, &mut leaves);

        let mut leading: HashMap<usize, Vec<AttachedDirective>> = HashMap::new();
        let mut trailing: HashMap<usize, Vec<AttachedDirective>> = HashMap::new();

        for (dir_node, text) in &directives {
            attach_one(
                &leaves,
                dir_node.start_byte(),
                dir_node.end_byte(),
                dir_node.start_position().row,
                text.clone(),
                &mut leading,
                &mut trailing,
            );
        }

        // Fold virtual directives derived from patches.
        let virtuals = patches_to_virtuals(patches);
        for v in &virtuals {
            attach_one(
                &leaves,
                v.start_byte,
                v.end_byte,
                v.row,
                v.text.clone(),
                &mut leading,
                &mut trailing,
            );
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

/// Attach a single directive (real or virtual) to either a preceding leaf
/// (trailing) or the next leaf (leading). Shared by `build` and
/// `build_with_patches`.
fn attach_one(
    leaves: &[Node<'_>],
    dir_start: usize,
    dir_end: usize,
    dir_row: usize,
    text: String,
    leading: &mut HashMap<usize, Vec<AttachedDirective>>,
    trailing: &mut HashMap<usize, Vec<AttachedDirective>>,
) {
    if let Some(prev) = find_prev_leaf_at(leaves, dir_start) {
        if prev.end_position().row == dir_row {
            let gap = dir_start.saturating_sub(prev.end_byte());
            trailing
                .entry(prev.id())
                .or_default()
                .push(AttachedDirective {
                    text,
                    trailing: true,
                    gap,
                });
            return;
        }
    }
    if let Some(next) = find_next_leaf_at(leaves, dir_end) {
        leading
            .entry(next.id())
            .or_default()
            .push(AttachedDirective {
                text,
                trailing: false,
                gap: 0,
            });
    }
}

fn find_prev_leaf_at<'a>(leaves: &[Node<'a>], target_start: usize) -> Option<Node<'a>> {
    let idx = leaves.partition_point(|leaf| leaf.start_byte() < target_start);
    if idx > 0 { Some(leaves[idx - 1]) } else { None }
}

fn find_next_leaf_at<'a>(leaves: &[Node<'a>], target_end: usize) -> Option<Node<'a>> {
    let idx = leaves.partition_point(|leaf| leaf.start_byte() < target_end);
    leaves.get(idx).copied()
}

fn patches_to_virtuals(patches: &[DirectivePatch]) -> Vec<VirtualDirective> {
    let mut v = Vec::with_capacity(patches.len() * 2);
    for p in patches {
        v.push(VirtualDirective {
            start_byte: p.opening_start,
            end_byte: p.opening_end,
            text: p.opening_text.clone(),
            row: p.opening_row,
        });
        v.push(VirtualDirective {
            start_byte: p.closing_start,
            end_byte: p.closing_end,
            text: p.closing_text.clone(),
            row: p.closing_row,
        });
    }
    v
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
