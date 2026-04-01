use crate::printer::{is_generic_parent, is_keyword_needing_space_after, Printer};
use crate::spacing;
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
        // 1. No previous token → no space.
        if prev_kind.is_empty() {
            return false;
        }
        // 2. No space before `)`, `]`, `.`
        if kind == K::CLOSE_PAREN || kind == K::CLOSE_BRACKET || kind == K::DOT {
            return false;
        }
        // 3. No space after `(`, `[`, `.`
        if prev_kind == K::OPEN_PAREN || prev_kind == K::OPEN_BRACKET || prev_kind == K::DOT {
            return false;
        }
        // 4. No space before `;`
        if kind == K::SEMICOLON {
            return false;
        }
        // 5. No space before `,`
        if kind == K::COMMA {
            return false;
        }
        // 6. No space before `[` in subscript context
        if kind == K::OPEN_BRACKET && parent_kind == K::EXPR_SUBSCRIPT {
            return false;
        }
        // 7. No space before `(` in call/args context
        if kind == K::OPEN_PAREN && (parent_kind == K::EXPR_CALL || parent_kind == K::DECL_ARGS) {
            return false;
        }
        // 8. No spaces inside generic angle brackets
        if (kind == K::K_LT || kind == K::LESS_THAN)
            && (parent_kind == K::TYPEREF_TPL
                || parent_kind == K::GENERIC_TPL
                || parent_kind == K::GENERIC_ARGS
                || parent_kind == K::TYPEREF_ARGS
                || parent_kind == K::EXPR_TPL)
        {
            return false;
        }
        if (kind == K::K_GT || kind == K::GREATER_THAN)
            && (parent_kind == K::TYPEREF_TPL
                || parent_kind == K::GENERIC_TPL
                || parent_kind == K::GENERIC_ARGS
                || parent_kind == K::TYPEREF_ARGS
                || parent_kind == K::EXPR_TPL)
        {
            return false;
        }
        // 9. No space after `<` in generic context
        if prev_kind == K::K_LT && is_generic_parent(prev_parent_kind) {
            return false;
        }
        // 10. No space before `:`
        if kind == K::COLON {
            return false;
        }
        // 11. No space around `..`
        if kind == K::DOTDOT || prev_kind == K::DOTDOT {
            return false;
        }
        // 12. No space before/after `kDot` or `.`
        if kind == K::K_DOT {
            return false;
        }
        if prev_kind == K::K_DOT || prev_kind == K::DOT {
            return false;
        }
        // 13. spacing::space_before
        if spacing::space_before(kind) {
            return true;
        }
        // 14. spacing::space_after
        if spacing::space_after(prev_kind) {
            return true;
        }
        // 15. keyword needing space after
        if is_keyword_needing_space_after(prev_kind) {
            return true;
        }
        // 16. Default: space between two identifiers/keywords/literals
        if !prev_kind.is_empty()
            && prev_kind != K::SEMICOLON
            && prev_kind != K::OPEN_PAREN
            && prev_kind != K::OPEN_BRACKET
        {
            return true;
        }
        // 17. Otherwise
        false
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
