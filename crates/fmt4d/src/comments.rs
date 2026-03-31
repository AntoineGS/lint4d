use std::collections::HashMap;
use tree_sitter::Node;

/// A comment attached to a code node.
#[derive(Debug, Clone)]
pub struct AttachedComment {
    /// The comment text (including delimiters like `//`, `{}`).
    pub text: String,
    /// True if the comment appeared on the same line as code (trailing).
    pub trailing: bool,
    /// The source row (0-based) where the comment starts.
    pub source_row: usize,
}

/// Maps node IDs to their attached comments.
#[derive(Debug)]
pub struct CommentMap {
    /// Leading comments keyed by the node ID they precede.
    leading: HashMap<usize, Vec<AttachedComment>>,
    /// Trailing comments keyed by the node ID they follow.
    trailing: HashMap<usize, Vec<AttachedComment>>,
}

impl CommentMap {
    /// Scan all comment nodes in the tree and attach them to nearby code nodes.
    pub fn build(root: Node, source: &[u8]) -> Self {
        let mut comments = Vec::new();
        collect_comments(root, source, &mut comments);

        let mut leading: HashMap<usize, Vec<AttachedComment>> = HashMap::new();
        let mut trailing: HashMap<usize, Vec<AttachedComment>> = HashMap::new();

        for (comment_node, text) in &comments {
            let comment_line = comment_node.start_position().row;

            // Check if there is a non-comment token on the same line before
            // the comment — that makes it a trailing comment.
            if let Some(prev) = find_prev_leaf(root, *comment_node) {
                if prev.end_position().row == comment_line {
                    trailing
                        .entry(prev.id())
                        .or_default()
                        .push(AttachedComment {
                            text: text.clone(),
                            trailing: true,
                            source_row: comment_line,
                        });
                    continue;
                }
            }

            // Otherwise it is a leading comment for the next non-comment node.
            if let Some(next) = find_next_code_node(root, *comment_node) {
                leading.entry(next.id()).or_default().push(AttachedComment {
                    text: text.clone(),
                    trailing: false,
                    source_row: comment_line,
                });
            }
        }

        CommentMap { leading, trailing }
    }

    /// Get leading comments for a node (comments on lines above it).
    pub fn leading_comments(&self, node_id: usize) -> &[AttachedComment] {
        self.leading.get(&node_id).map_or(&[], |v| v.as_slice())
    }

    /// Get trailing comments for a node (comments on same line after it).
    pub fn trailing_comments(&self, node_id: usize) -> &[AttachedComment] {
        self.trailing.get(&node_id).map_or(&[], |v| v.as_slice())
    }
}

/// Collect all `comment` (extra) nodes from the tree.
fn collect_comments<'a>(node: Node<'a>, source: &[u8], out: &mut Vec<(Node<'a>, String)>) {
    if node.is_extra() && node.kind() == "comment" {
        let text = std::str::from_utf8(&source[node.start_byte()..node.end_byte()])
            .unwrap_or("")
            .to_string();
        out.push((node, text));
        return;
    }
    for child in node.children(&mut node.walk()) {
        collect_comments(child, source, out);
    }
}

/// Find the previous leaf node (non-extra) before `target` in tree order.
fn find_prev_leaf<'a>(root: Node<'a>, target: Node<'a>) -> Option<Node<'a>> {
    let mut leaves = Vec::new();
    collect_leaves(root, &mut leaves);
    let target_start = target.start_byte();
    let mut best: Option<Node<'a>> = None;
    for leaf in leaves {
        if leaf.start_byte() < target_start {
            best = Some(leaf);
        } else {
            break;
        }
    }
    best
}

/// Find the next non-extra, non-comment node after `target` in tree order.
fn find_next_code_node<'a>(root: Node<'a>, target: Node<'a>) -> Option<Node<'a>> {
    let mut leaves = Vec::new();
    collect_leaves(root, &mut leaves);
    let target_end = target.end_byte();
    leaves
        .into_iter()
        .find(|leaf| leaf.start_byte() >= target_end)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> (tree_sitter::Tree, Vec<u8>) {
        let bytes = source.as_bytes().to_vec();
        let info = pascal_core::FileInfo::new(std::path::PathBuf::from("test.pas"));
        let (tree, _) = pascal_core::parser::parse_file(&info, &bytes).unwrap();
        (tree, bytes)
    }

    #[test]
    fn trailing_comment_attached_to_prev_token() {
        let source = "unit Test; // this is a unit\ninterface\nimplementation\nend.\n";
        let (tree, bytes) = parse(source);
        let map = CommentMap::build(tree.root_node(), &bytes);

        // The comment should be trailing on the `;` after `Test`
        let mut found_trailing = false;
        for (_, comments) in &map.trailing {
            for c in comments {
                if c.text.contains("this is a unit") {
                    found_trailing = true;
                    assert!(c.trailing);
                }
            }
        }
        assert!(found_trailing, "trailing comment not found");
    }

    #[test]
    fn leading_comment_attached_to_next_node() {
        let source = "unit Test;\n// interface comment\ninterface\nimplementation\nend.\n";
        let (tree, bytes) = parse(source);
        let map = CommentMap::build(tree.root_node(), &bytes);

        let mut found_leading = false;
        for (_, comments) in &map.leading {
            for c in comments {
                if c.text.contains("interface comment") {
                    found_leading = true;
                    assert!(!c.trailing);
                }
            }
        }
        assert!(found_leading, "leading comment not found");
    }

    #[test]
    fn no_comments_returns_empty() {
        let source = "unit Test;\ninterface\nimplementation\nend.\n";
        let (tree, bytes) = parse(source);
        let map = CommentMap::build(tree.root_node(), &bytes);
        assert!(map.leading.is_empty());
        assert!(map.trailing.is_empty());
    }
}
