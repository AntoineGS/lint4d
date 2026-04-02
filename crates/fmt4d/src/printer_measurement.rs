use crate::printer::Printer;
use pascal_core::node_kind as K;
use tree_sitter::Node;

impl<'a> Printer<'a> {
    /// Measure the single-line width a subtree would produce.
    /// Returns `(width, last_kind, last_parent_kind)` so callers can chain measurements.
    pub(crate) fn measure_node(
        &self,
        node: Node,
        prev_kind: &str,
        prev_parent_kind: &str,
    ) -> (usize, String, String) {
        // If in format-off region, use approximate width from raw text.
        if self.is_in_format_off_region(node) {
            let text = self.node_text(node);
            let approx = text.lines().map(|l| l.len()).max().unwrap_or(0);
            return (approx, prev_kind.to_string(), prev_parent_kind.to_string());
        }

        let kind = node.kind();

        // Verbatim leaf nodes (literalChar / literalString) — treat as single token.
        if kind == K::LITERAL_CHAR || kind == K::LITERAL_STRING {
            let text = self.node_text(node);
            let parent_kind = node
                .parent()
                .map(|p| p.kind().to_string())
                .unwrap_or_default();
            let space = if self.would_need_space(prev_kind, prev_parent_kind, kind, &parent_kind) {
                1
            } else {
                0
            };
            return (space + text.len(), kind.to_string(), parent_kind);
        }

        // Plain leaf node.
        if node.child_count() == 0 && !node.is_extra() {
            return self.measure_leaf(node, prev_kind, prev_parent_kind);
        }

        // Internal node — recurse into non-extra children.
        let mut total = 0usize;
        let mut cur_kind = prev_kind.to_string();
        let mut cur_parent = prev_parent_kind.to_string();
        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            let (w, k, p) = self.measure_node(child, &cur_kind, &cur_parent);
            total += w;
            cur_kind = k;
            cur_parent = p;
        }
        (total, cur_kind, cur_parent)
    }

    /// Measure a single leaf token.
    pub(crate) fn measure_leaf(
        &self,
        node: Node,
        prev_kind: &str,
        prev_parent_kind: &str,
    ) -> (usize, String, String) {
        let kind = node.kind();
        let text = self.node_text(node);
        let parent_kind = node
            .parent()
            .map(|p| p.kind().to_string())
            .unwrap_or_default();
        let space = if self.would_need_space(prev_kind, prev_parent_kind, kind, &parent_kind) {
            1
        } else {
            0
        };
        (space + text.len(), kind.to_string(), parent_kind)
    }

    /// Pure spacing check — mirrors `needs_space_before` but takes explicit
    /// parameters instead of reading `self.last_token_kind`.
    pub(crate) fn would_need_space(
        &self,
        prev_kind: &str,
        prev_parent_kind: &str,
        kind: &str,
        parent_kind: &str,
    ) -> bool {
        crate::spacing::would_need_space(prev_kind, prev_parent_kind, kind, parent_kind)
    }

    /// Measure the combined single-line width of a slice of nodes.
    /// Starts from `self.last_token_kind` / `self.last_token_parent_kind`.
    pub(crate) fn measure_group(&self, nodes: &[Node]) -> (usize, String, String) {
        let mut total = 0usize;
        let mut cur_kind = self.last_token_kind.clone();
        let mut cur_parent = self.last_token_parent_kind.clone();
        for node in nodes {
            let (w, k, p) = self.measure_node(*node, &cur_kind, &cur_parent);
            total += w;
            cur_kind = k;
            cur_parent = p;
        }
        (total, cur_kind, cur_parent)
    }
}
