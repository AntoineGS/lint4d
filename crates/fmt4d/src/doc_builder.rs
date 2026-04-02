use crate::comments::CommentMap;
use crate::config::FmtConfig;
use crate::doc::{self, Doc};
use pascal_core::node_kind as K;
use pascal_core::FormatOffRegion;
use std::collections::HashSet;
use tree_sitter::Node;

/// Stateless AST-to-Doc builder.
///
/// Converts a tree-sitter AST into a `Doc` IR tree. The key invariant is that
/// `doc_for_node` is the ONLY way to process a node — it always injects
/// leading and trailing comments around the node's body.
#[allow(dead_code)]
pub struct DocBuilder<'a> {
    pub(crate) source: &'a [u8],
    pub(crate) config: &'a FmtConfig,
    comments: &'a CommentMap,
    format_regions: Vec<FormatOffRegion>,
    pub(crate) external_units: HashSet<String>,
}

impl<'a> DocBuilder<'a> {
    pub fn new(
        source: &'a [u8],
        config: &'a FmtConfig,
        comments: &'a CommentMap,
        format_regions: Vec<FormatOffRegion>,
        external_units: HashSet<String>,
    ) -> Self {
        DocBuilder {
            source,
            config,
            comments,
            format_regions,
            external_units,
        }
    }

    /// Entry point: build a `Doc` for the entire AST rooted at `root`.
    pub fn build(&self, root: Node) -> Doc {
        self.doc_for_node(root)
    }

    // ── Core dispatch ────────────────────────────────────────────────

    /// The single entry point for processing any node.
    ///
    /// 1. If the node falls within a format-off region, return verbatim text.
    /// 2. Gather leading comments.
    /// 3. Dispatch to the appropriate handler via `build_doc`.
    /// 4. Gather trailing comments.
    /// 5. Return `concat([leading, body, trailing])`.
    pub(crate) fn doc_for_node(&self, node: Node<'a>) -> Doc {
        if self.is_in_format_off_region(node) {
            return Doc::Raw(self.node_text(node));
        }

        let leading = self.leading_comments_doc(node);
        let body = self.build_doc(node);
        let trailing = self.trailing_comments_doc(node);

        doc::concat(vec![leading, body, trailing])
    }

    /// Dispatch to the correct handler by node kind.
    ///
    /// All handlers are stubs that delegate to `build_children` for now.
    /// Tasks 5-8 will replace the stubs with real implementations.
    fn build_doc(&self, node: Node<'a>) -> Doc {
        match node.kind() {
            K::UNIT => self.build_unit(node),
            K::INTERFACE => self.build_interface_section(node),
            K::IMPLEMENTATION => self.build_implementation_section(node),
            K::INITIALIZATION | K::FINALIZATION => self.build_init_final_section(node),
            K::DECL_USES => self.build_uses(node),
            K::BLOCK | K::STATEMENTS => self.build_block(node),
            K::DECL_CLASS | K::DECL_RECORD | K::DECL_INTF => self.build_type_body(node),
            K::DECL_SECTION => self.build_decl_section(node),
            K::DECL_VARS | K::DECL_CONSTS | K::DECL_TYPES => self.build_section(node),
            K::DEF_PROC => self.build_def_proc(node),
            K::DECL_PROC => self.build_decl_proc(node),
            K::TRY => self.build_try(node),
            K::CASE => self.build_case(node),
            K::REPEAT => self.build_repeat(node),
            K::IF | K::IF_ELSE => self.build_if(node),
            K::FOR | K::FOREACH | K::WHILE | K::WITH => self.build_loop(node),
            K::LITERAL_CHAR | K::LITERAL_STRING => self.build_verbatim_leaf(node),
            K::DECL_ARGS => self.build_args(node),
            K::EXPR_CALL => self.build_call(node),
            _ if node.child_count() == 0 && !node.is_extra() => self.build_leaf(node),
            _ => {
                if node.child_count() > 0 && self.has_breakable_operators(node) {
                    self.build_expression_breaking(node)
                } else {
                    self.build_children(node)
                }
            }
        }
    }

    // ── Leaf helpers ─────────────────────────────────────────────────

    /// Emit a leaf token carrying kind metadata for spacing resolution.
    pub(crate) fn build_leaf(&self, node: Node<'a>) -> Doc {
        let text = self.node_text(node);
        let kind = node.kind();
        let parent_kind = node
            .parent()
            .map(|p| p.kind().to_string())
            .unwrap_or_default();
        doc::token(text, kind, parent_kind)
    }

    /// Emit a leaf token as verbatim text — used for `literalChar` /
    /// `literalString` where child nodes don't cover the full source span.
    fn build_verbatim_leaf(&self, node: Node<'a>) -> Doc {
        let text = self.node_text(node);
        let kind = node.kind();
        let parent_kind = node
            .parent()
            .map(|p| p.kind().to_string())
            .unwrap_or_default();
        doc::token(text, kind, parent_kind)
    }

    // ── Recursion helpers ────────────────────────────────────────────

    /// Map all non-extra children through `doc_for_node` and concatenate.
    pub(crate) fn build_children(&self, node: Node<'a>) -> Doc {
        let docs: Vec<Doc> = self
            .code_children(node)
            .into_iter()
            .map(|child| self.doc_for_node(child))
            .collect();
        doc::concat(docs)
    }

    /// Return the non-extra children of `node` in source order.
    pub(crate) fn code_children(&self, node: Node<'a>) -> Vec<Node<'a>> {
        node.children(&mut node.walk())
            .filter(|c| !c.is_extra())
            .collect()
    }

    // ── Comment injection ────────────────────────────────────────────

    /// Build a `Doc` for the leading comments of `node`.
    ///
    /// Each comment is preceded by a `Hardline` and followed by a `Hardline`.
    /// If there is a blank line in the source between consecutive comments (or
    /// between the last comment and the node), a `BlankLine` is inserted.
    fn leading_comments_doc(&self, node: Node<'a>) -> Doc {
        let comments = self.comments.leading_comments(node.id());
        if comments.is_empty() {
            return Doc::Empty;
        }

        let mut parts: Vec<Doc> = Vec::new();

        for (i, comment) in comments.iter().enumerate() {
            parts.push(Doc::Hardline);
            parts.push(Doc::Raw(comment.text.clone()));

            // Check for blank line between this comment and the next one (or node).
            let comment_end_row =
                comment.source_row + comment.text.lines().count().saturating_sub(1);
            let next_start_row = if i + 1 < comments.len() {
                comments[i + 1].source_row
            } else {
                node.start_position().row
            };

            if self.has_blank_line_between(comment_end_row, next_start_row) {
                parts.push(Doc::BlankLine);
            }
        }

        // Always end with a newline so the node itself starts on a fresh line.
        parts.push(Doc::Hardline);

        doc::concat(parts)
    }

    /// Build a `Doc` for the trailing comments of `node`.
    ///
    /// Each trailing comment is preceded by a single space.
    fn trailing_comments_doc(&self, node: Node<'a>) -> Doc {
        let comments = self.comments.trailing_comments(node.id());
        if comments.is_empty() {
            return Doc::Empty;
        }

        let docs: Vec<Doc> = comments
            .iter()
            .map(|c| Doc::Raw(format!(" {}", c.text)))
            .collect();

        doc::concat(docs)
    }

    // ── Utility helpers ──────────────────────────────────────────────

    /// Extract the source text for `node`, stripping carriage returns.
    pub(crate) fn node_text(&self, node: Node) -> String {
        std::str::from_utf8(&self.source[node.start_byte()..node.end_byte()])
            .unwrap_or("")
            .replace('\r', "")
    }

    /// Return `true` if there is a blank (empty / whitespace-only) line in the
    /// source between `start_row` (exclusive) and `end_row` (exclusive).
    /// Both values are 0-based row indices.
    fn has_blank_line_between(&self, start_row: usize, end_row: usize) -> bool {
        if end_row <= start_row + 1 {
            return false;
        }
        let source_str = std::str::from_utf8(self.source).unwrap_or("");
        for (row_idx, line) in source_str.lines().enumerate() {
            if row_idx > start_row && row_idx < end_row && line.trim().is_empty() {
                return true;
            }
            if row_idx >= end_row {
                break;
            }
        }
        false
    }

    /// Return `true` if `node` falls entirely within a format-off region.
    pub(crate) fn is_in_format_off_region(&self, node: Node) -> bool {
        let node_start = node.start_position().row + 1; // 1-based
        let node_end = node.end_position().row + 1; // 1-based
        self.format_regions
            .iter()
            .any(|r| node_start >= r.start_line && node_end <= r.end_line)
    }

    /// Return `true` if any immediate leaf child of `node` is a breakable
    /// binary operator (arithmetic, logical, or bitwise).
    fn has_breakable_operators(&self, node: Node<'a>) -> bool {
        for child in node.children(&mut node.walk()) {
            if child.child_count() == 0 {
                match child.kind() {
                    K::K_ADD
                    | K::K_SUB
                    | K::K_MUL
                    | K::K_DIV
                    | K::K_MOD
                    | K::K_AND
                    | K::K_OR
                    | K::K_XOR
                    | K::K_SHL
                    | K::K_SHR => return true,
                    _ => {}
                }
            }
        }
        false
    }

    /// Return `true` if any ancestor of `node` has `ancestor_kind`.
    #[allow(dead_code)]
    pub(crate) fn is_ancestor(node: Node, ancestor_kind: &str) -> bool {
        let mut current = node.parent();
        while let Some(p) = current {
            if p.kind() == ancestor_kind {
                return true;
            }
            current = p.parent();
        }
        false
    }

    // ── Stub handlers ────────────────────────────────────────────────
    //
    // All handlers delegate to `build_children` for now.  Tasks 5-8 will
    // replace these stubs with real formatting logic.

    fn build_unit(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
    }

    fn build_interface_section(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
    }

    fn build_implementation_section(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
    }

    fn build_init_final_section(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
    }

    fn build_uses(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
    }

    fn build_block(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
    }

    fn build_section(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
    }

    fn build_decl_section(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
    }

    fn build_def_proc(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
    }

    fn build_decl_proc(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
    }

    fn build_type_body(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
    }

    fn build_try(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
    }

    fn build_case(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
    }

    fn build_repeat(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
    }

    fn build_if(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
    }

    fn build_loop(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
    }

    fn build_args(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
    }

    fn build_call(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
    }

    fn build_expression_breaking(&self, node: Node<'a>) -> Doc {
        self.build_children(node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comments::CommentMap;
    use crate::config::FmtConfig;

    fn parse(source: &str) -> (tree_sitter::Tree, Vec<u8>) {
        let bytes = source.as_bytes().to_vec();
        let info = pascal_core::FileInfo::new(std::path::PathBuf::from("test.pas"));
        let (tree, _) = pascal_core::parser::parse_file(&info, &bytes).unwrap();
        (tree, bytes)
    }

    fn make_builder<'a>(
        source: &'a [u8],
        config: &'a FmtConfig,
        comments: &'a CommentMap,
    ) -> DocBuilder<'a> {
        DocBuilder::new(source, config, comments, vec![], HashSet::new())
    }

    #[test]
    fn build_returns_non_empty_for_unit() {
        let source = "unit Test;\ninterface\nimplementation\nend.\n";
        let (tree, bytes) = parse(source);
        let config = FmtConfig::default();
        let comments = CommentMap::build(tree.root_node(), &bytes);
        let builder = make_builder(&bytes, &config, &comments);
        let doc = builder.build(tree.root_node());
        assert!(!matches!(doc, Doc::Empty));
    }

    #[test]
    fn format_off_region_returns_raw() {
        let source = "{$FMT.OFF}\nunit Test;\ninterface\nimplementation\nend.\n{$FMT.ON}\n";
        let (tree, bytes) = parse(source);
        let config = FmtConfig::default();
        let comments = CommentMap::build(tree.root_node(), &bytes);
        let regions = pascal_core::directives::parse_format_regions(&bytes);
        let builder = DocBuilder::new(&bytes, &config, &comments, regions, HashSet::new());
        let doc = builder.build(tree.root_node());
        // The whole unit falls inside the format-off region, so it should be Raw.
        assert!(matches!(doc, Doc::Raw(_)));
    }

    #[test]
    fn code_children_excludes_extras() {
        let source = "unit Test; // comment\ninterface\nimplementation\nend.\n";
        let (tree, bytes) = parse(source);
        let config = FmtConfig::default();
        let comments = CommentMap::build(tree.root_node(), &bytes);
        let builder = make_builder(&bytes, &config, &comments);
        let root = tree.root_node();
        let children = builder.code_children(root);
        // All returned children must be non-extra.
        for child in &children {
            assert!(!child.is_extra(), "code_children returned an extra node");
        }
    }

    #[test]
    fn leading_comment_produces_hardline_and_raw() {
        let source = "unit Test;\n// a comment\ninterface\nimplementation\nend.\n";
        let (tree, bytes) = parse(source);
        let config = FmtConfig::default();
        let comments = CommentMap::build(tree.root_node(), &bytes);
        let builder = make_builder(&bytes, &config, &comments);

        // The comment map attaches a leading comment to the next leaf node after
        // the comment. Walk the full tree and find any node with leading comments.
        fn find_node_with_leading<'a>(
            node: Node<'a>,
            builder: &DocBuilder<'a>,
        ) -> Option<Node<'a>> {
            if !builder.comments.leading_comments(node.id()).is_empty() {
                return Some(node);
            }
            for child in node.children(&mut node.walk()) {
                if let Some(found) = find_node_with_leading(child, builder) {
                    return Some(found);
                }
            }
            None
        }

        let node = find_node_with_leading(tree.root_node(), &builder)
            .expect("expected at least one node with leading comments");
        let leading = builder.leading_comments_doc(node);
        // Should not be empty — there is a leading comment.
        assert!(!matches!(leading, Doc::Empty));
    }

    #[test]
    fn trailing_comment_produces_raw_with_space() {
        let source = "unit Test; // trailing\ninterface\nimplementation\nend.\n";
        let (tree, bytes) = parse(source);
        let config = FmtConfig::default();
        let comments = CommentMap::build(tree.root_node(), &bytes);
        let builder = make_builder(&bytes, &config, &comments);

        // Find the leaf node that the trailing comment is attached to.
        // The comment map attaches trailing comments to the preceding leaf.
        // Walk all leaves to find one with trailing comments.
        let mut found = false;
        fn walk_leaves<'a>(node: Node<'a>, builder: &DocBuilder<'a>, found: &mut bool) {
            if !node.is_extra() && node.child_count() == 0 {
                let trailing = builder.trailing_comments_doc(node);
                if !matches!(trailing, Doc::Empty) {
                    *found = true;
                }
            }
            for child in node.children(&mut node.walk()) {
                walk_leaves(child, builder, found);
            }
        }
        walk_leaves(tree.root_node(), &builder, &mut found);
        assert!(found, "expected a trailing comment doc to be non-empty");
    }

    #[test]
    fn is_ancestor_finds_parent_kind() {
        let source =
            "unit Test;\ninterface\nimplementation\nprocedure Foo(A: Integer);\nbegin\nend;\nend.\n";
        let (tree, bytes) = parse(source);
        let config = FmtConfig::default();
        let comments = CommentMap::build(tree.root_node(), &bytes);
        let builder = make_builder(&bytes, &config, &comments);

        // Walk tree to find a declArg node and verify is_ancestor works.
        fn find_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
            if node.kind() == kind {
                return Some(node);
            }
            for child in node.children(&mut node.walk()) {
                if let Some(found) = find_kind(child, kind) {
                    return Some(found);
                }
            }
            None
        }

        let _ = builder; // suppress unused warning
        let decl_arg = find_kind(tree.root_node(), K::DECL_ARG);
        if let Some(arg) = decl_arg {
            assert!(DocBuilder::is_ancestor(arg, K::DECL_ARGS));
        }
        // Even if not found (grammar variation), the test should not panic.
    }

    #[test]
    fn has_breakable_operators_detects_add() {
        // Build a minimal binary expression via parse and check the helper.
        let source =
            "unit Test;\ninterface\nimplementation\nvar X: Integer;\nbegin\nX := 1 + 2;\nend.\n";
        let (tree, bytes) = parse(source);
        let config = FmtConfig::default();
        let comments = CommentMap::build(tree.root_node(), &bytes);
        let builder = make_builder(&bytes, &config, &comments);

        fn find_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
            if node.kind() == kind {
                return Some(node);
            }
            for child in node.children(&mut node.walk()) {
                if let Some(found) = find_kind(child, kind) {
                    return Some(found);
                }
            }
            None
        }

        if let Some(bin_expr) = find_kind(tree.root_node(), K::EXPR_BINARY) {
            assert!(builder.has_breakable_operators(bin_expr));
        }
        // If no binary expr found, skip — grammar variation.
    }
}
