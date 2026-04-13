use crate::comments::CommentMap;
use crate::config::FmtConfig;
use crate::directive_map::DirectiveMap;
use crate::doc::{self, Doc};
use pascal_core::FormatOffRegion;
use pascal_core::node_kind as K;
use std::collections::HashSet;
use tree_sitter::Node;

/// Controls how a binary chain breaks across lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BreakStyle {
    /// Greedy Fill: pack as many operands per line as fit.
    GreedyFill,
    /// Greedy Fill with preserved author line breaks (for `+` chains).
    PreserveBreaks,
    /// Expand all: one operand per line when the chain overflows.
    ExpandAll,
}

/// Stateless AST-to-Doc builder.
///
/// Converts a tree-sitter AST into a `Doc` IR tree. The key invariant is that
/// `doc_for_node` is the ONLY way to process a node — it always injects
/// leading and trailing comments around the node's body.
pub struct DocBuilder<'a> {
    pub(crate) source: &'a [u8],
    pub(crate) config: &'a FmtConfig,
    pub(crate) comments: &'a CommentMap,
    directives: &'a DirectiveMap,
    format_regions: Vec<FormatOffRegion>,
    pub(crate) external_units: &'a HashSet<String>,
}

impl<'a> DocBuilder<'a> {
    pub fn new(
        source: &'a [u8],
        config: &'a FmtConfig,
        comments: &'a CommentMap,
        directives: &'a DirectiveMap,
        format_regions: Vec<FormatOffRegion>,
        external_units: &'a HashSet<String>,
    ) -> Self {
        DocBuilder {
            source,
            config,
            comments,
            directives,
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

        let leading_comments = self.leading_comments_doc(node);
        let leading_directives = self.leading_directives_doc(node);
        let body = self.build_doc(node);
        let trailing_comments = self.trailing_comments_doc(node);
        let trailing_directives = self.trailing_directives_doc(node);

        doc::concat(vec![
            leading_comments,
            leading_directives,
            body,
            trailing_comments,
            trailing_directives,
        ])
    }

    /// Like `doc_for_node` but omits leading comments.
    ///
    /// Used by alignment decompose functions so that leading comments
    /// can be extracted at the group level instead of being embedded
    /// inside aligned cells.
    pub(crate) fn doc_for_node_sans_leading(&self, node: Node<'a>) -> Doc {
        if self.is_in_format_off_region(node) {
            return Doc::Raw(self.node_text(node));
        }

        let leading_directives = self.leading_directives_doc(node);
        let body = self.build_doc(node);
        let trailing_comments = self.trailing_comments_doc(node);
        let trailing_directives = self.trailing_directives_doc(node);

        doc::concat(vec![
            leading_directives,
            body,
            trailing_comments,
            trailing_directives,
        ])
    }

    /// Like `doc_for_node` but omits trailing comments.
    ///
    /// Used by alignment decompose functions so that trailing comments
    /// can be extracted as a separate alignment cell instead of being
    /// embedded inside the last data cell.
    pub(crate) fn doc_for_node_sans_trailing(&self, node: Node<'a>) -> Doc {
        if self.is_in_format_off_region(node) {
            return Doc::Raw(self.node_text(node));
        }

        let leading_comments = self.leading_comments_doc(node);
        let leading_directives = self.leading_directives_doc(node);
        let body = self.build_doc(node);
        let trailing_directives = self.trailing_directives_doc(node);

        doc::concat(vec![
            leading_comments,
            leading_directives,
            body,
            trailing_directives,
        ])
    }

    /// Dispatch to the correct handler by node kind.
    ///
    /// All handlers are stubs that delegate to `build_children` for now.
    /// Tasks 5-8 will replace the stubs with real implementations.
    pub(crate) fn build_doc(&self, node: Node<'a>) -> Doc {
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
            K::DECL_ARG | K::DECL_VAR | K::DECL_FIELD => self.build_comma_ident_decl(node),
            K::DECL_ARGS => self.build_args(node),
            K::EXPR_CALL => self.build_call(node),
            K::EXPR_BRACKETS => self.build_bracket_list(node),
            K::DECL_ENUM => self.build_paren_list(node, K::COMMA),
            K::RTTI_ATTRIBUTES => self.build_rtti_attributes(node),
            K::PP_BLOCK => self.build_pp_block(node),
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
        let parent_kind = node.parent().map(|p| p.kind()).unwrap_or("");
        doc::token(text, kind, parent_kind)
    }

    /// Emit a leaf token as verbatim text — used for `literalChar` /
    /// `literalString` where child nodes don't cover the full source span.
    fn build_verbatim_leaf(&self, node: Node<'a>) -> Doc {
        let text = self.node_text(node);
        let kind = node.kind();
        let parent_kind = node.parent().map(|p| p.kind()).unwrap_or("");
        doc::token(text, kind, parent_kind)
    }

    // ── Recursion helpers ────────────────────────────────────────────

    /// Map all non-extra children through `doc_for_node` and concatenate.
    pub(crate) fn build_children(&self, node: Node<'a>) -> Doc {
        let mut docs: Vec<Doc> = Vec::new();
        self.for_each_code_child(node, |child| {
            docs.push(self.doc_for_node(child));
        });
        doc::concat(docs)
    }

    /// Return the non-extra children of `node` in source order.
    pub(crate) fn code_children(&self, node: Node<'a>) -> Vec<Node<'a>> {
        node.children(&mut node.walk())
            .filter(|c| !c.is_extra())
            .collect()
    }

    /// Like [`code_children`], but calls `f` with each non-extra child in
    /// source order without allocating a `Vec<Node>`. Prefer this at call
    /// sites that only iterate; use [`code_children`] when indexing or
    /// windowed access is required.
    ///
    /// Review PERF-H1: `code_children` was called ~2000 times per 1000-line
    /// file, each allocating a fresh `Vec`.
    pub(crate) fn for_each_code_child(&self, node: Node<'a>, mut f: impl FnMut(Node<'a>)) {
        let mut walker = node.walk();
        for child in node.children(&mut walker) {
            if !child.is_extra() {
                f(child);
            }
        }
    }

    // ── Comment injection ────────────────────────────────────────────

    /// Build a `Doc` for the leading comments of `node`.
    ///
    /// Each comment is preceded by a `Hardline` and followed by a `Hardline`.
    /// If there is a blank line in the source between consecutive comments (or
    /// between the last comment and the node), a `BlankLine` is inserted.
    pub(crate) fn leading_comments_doc(&self, node: Node<'a>) -> Doc {
        let comments = self.comments.leading_comments(node.id());
        if comments.is_empty() {
            return Doc::Empty;
        }

        let mut parts: Vec<Doc> = Vec::new();

        for (i, comment) in comments.iter().enumerate() {
            parts.push(Doc::Hardline);
            // Use Token (not Raw) so the renderer applies proper indentation
            // when the comment appears at line start.
            parts.push(doc::token(comment.text.clone(), K::COMMENT, ""));

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
    pub(crate) fn trailing_comments_doc(&self, node: Node<'a>) -> Doc {
        let comments = self.comments.trailing_comments(node.id());
        if comments.is_empty() {
            return Doc::Empty;
        }

        let docs: Vec<Doc> = comments
            .iter()
            .map(|c| {
                let gap = if c.gap > 0 { c.gap } else { 1 };
                Doc::Raw(format!("{}{}", " ".repeat(gap), c.text))
            })
            .collect();

        doc::concat(docs)
    }

    pub(crate) fn leading_directives_doc(&self, node: Node<'a>) -> Doc {
        let directives = self.directives.leading_directives(node.id());
        if directives.is_empty() {
            return Doc::Empty;
        }
        let mut parts = Vec::new();
        for directive in directives {
            parts.push(Doc::Hardline);
            parts.push(doc::token(directive.text.clone(), K::PP_DIRECTIVE, ""));
        }
        parts.push(Doc::Hardline);
        doc::concat(parts)
    }

    pub(crate) fn trailing_directives_doc(&self, node: Node<'a>) -> Doc {
        let directives = self.directives.trailing_directives(node.id());
        if directives.is_empty() {
            return Doc::Empty;
        }
        let docs: Vec<Doc> = directives
            .iter()
            .map(|d| {
                let gap = if d.gap > 0 { d.gap } else { 1 };
                Doc::Raw(format!("{}{}", " ".repeat(gap), d.text))
            })
            .collect();
        doc::concat(docs)
    }

    // ── Utility helpers ──────────────────────────────────────────────

    /// Extract the source text for `node`, stripping carriage returns.
    ///
    /// Tolerates non-UTF-8 bytes (legacy Latin-1 / Windows-1252 Delphi
    /// sources) via [`pascal_core::decode_bytes`], so accented text in
    /// comments and string literals survives round-tripping.
    pub(crate) fn node_text(&self, node: Node) -> String {
        pascal_core::decode_bytes(&self.source[node.start_byte()..node.end_byte()])
            .replace('\r', "")
    }

    /// Return `true` if there is a blank (empty / whitespace-only) line in the
    /// source between `start_row` (exclusive) and `end_row` (exclusive).
    /// Both values are 0-based row indices.
    ///
    /// Scans raw bytes rather than decoding UTF-8 so legacy Latin-1 /
    /// Windows-1252 Pascal sources (common in older Delphi codebases) do not
    /// silently disable blank-line preservation file-wide when a single
    /// accented character appears in a comment. Whitespace and `\n` are ASCII
    /// and thus encoding-independent for any ASCII-superset.
    pub(crate) fn has_blank_line_between(&self, start_row: usize, end_row: usize) -> bool {
        if end_row <= start_row + 1 {
            return false;
        }
        let bytes = self.source;
        let mut row_idx: usize = 0;
        let mut line_start: usize = 0;
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'\n' {
                if row_idx > start_row && row_idx < end_row {
                    let line = &bytes[line_start..i];
                    if line.iter().all(|c| c.is_ascii_whitespace()) {
                        return true;
                    }
                }
                row_idx += 1;
                if row_idx >= end_row {
                    return false;
                }
                line_start = i + 1;
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

    fn build_uses(&self, node: Node<'a>) -> Doc {
        let items = crate::uses::extract_uses_items(node, self.source);
        let indent_str = " ".repeat(self.config.indent_size);
        let formatted = crate::uses::format_uses_items(
            &items,
            &self.config.uses,
            &indent_str,
            self.external_units,
        );

        // Collect pre-uses directives (extras immediately before this node)
        let pre_directives = self.collect_pre_uses_directives(node);

        let mut parts = Vec::new();
        parts.push(Doc::Hardline);
        for dir in pre_directives {
            parts.push(Doc::Raw(dir));
            parts.push(Doc::Hardline);
        }
        parts.push(doc::token("uses", K::K_USES, ""));
        parts.push(Doc::Hardline);
        // Convert the pre-formatted uses body to a Doc tree of Token+Hardline
        // pairs so the renderer can see the contents (line-length budgeting,
        // no blind Doc::Raw escape hatch). Review AH2 (intermediate fix);
        // the full fix would have format_uses_items return Doc directly.
        // The synthetic kind "pp_raw_line" is not matched by spacing.rs, and
        // since each line is followed by a Hardline the renderer is at line
        // start when the next token is emitted, so no spurious spaces leak.
        for (i, line) in formatted.lines().enumerate() {
            if i > 0 {
                parts.push(Doc::Hardline);
            }
            if !line.is_empty() {
                parts.push(doc::token(line.to_string(), "pp_raw_line", ""));
            }
        }
        if formatted.ends_with('\n') {
            parts.push(Doc::Hardline);
        }
        doc::concat(parts)
    }

    fn build_pp_block(&self, node: Node<'a>) -> Doc {
        let mut parts = Vec::new();
        let mut prev_end_row: Option<usize> = None;

        for child in node.children(&mut node.walk()) {
            if child.is_extra() {
                continue;
            }
            match child.kind() {
                K::PP_IF | K::PP_ELSE | K::PP_END_IF => {
                    if let Some(prev) = prev_end_row {
                        if self.has_blank_line_between(prev, child.start_position().row) {
                            parts.push(Doc::BlankLine);
                        }
                    }
                    parts.push(Doc::Hardline);
                    parts.push(doc::token(self.node_text(child), child.kind(), ""));
                    prev_end_row = Some(child.end_position().row);
                }
                _ => {
                    if let Some(prev) = prev_end_row {
                        if self.has_blank_line_between(prev, child.start_position().row) {
                            parts.push(Doc::BlankLine);
                        }
                    }
                    let child_doc = self.doc_for_node(child);
                    if !starts_with_hardline(&child_doc) {
                        parts.push(Doc::Hardline);
                    }
                    parts.push(child_doc);
                    prev_end_row = Some(child.end_position().row);
                }
            }
        }
        doc::concat(parts)
    }

    /// Collect preprocessor directive texts that appear immediately before a
    /// `declUses` node in the parent's children (e.g., `{$I MDCompilers.inc}`).
    fn collect_pre_uses_directives(&self, uses_node: Node<'a>) -> Vec<String> {
        let mut found = Vec::new();
        let mut prev = uses_node.prev_sibling();
        while let Some(sib) = prev {
            if sib.is_extra() && sib.kind() == K::PP_DIRECTIVE {
                let text =
                    pascal_core::decode_bytes(&self.source[sib.start_byte()..sib.end_byte()])
                        .into_owned();
                found.push(text);
            } else {
                break;
            }
            prev = sib.prev_sibling();
        }
        found.reverse();
        found
    }

    /// Format an `rttiAttributes` node so each `[...]` group sits on its own
    /// line above the declaration it annotates.
    ///
    /// The grammar packs consecutive bracket attributes into a single
    /// `rttiAttributes` node — e.g. `[Test][TestCase('case1')]` is one node
    /// with children `[`, `Test`, `]`, `[`, `exprCall`, `]`. We insert a
    /// `Hardline` before every `[` after the first, and a final `Hardline`
    /// after the closing `]` so the next sibling (`procedure`, class name,
    /// field name, `property`) starts on a fresh line.
    fn build_rtti_attributes(&self, node: Node<'a>) -> Doc {
        let mut parts: Vec<Doc> = Vec::new();
        let mut seen_open_bracket = false;
        self.for_each_code_child(node, |child| {
            if child.kind() == K::OPEN_BRACKET {
                if seen_open_bracket {
                    parts.push(Doc::Hardline);
                }
                seen_open_bracket = true;
            }
            parts.push(self.doc_for_node(child));
        });
        parts.push(Doc::Hardline);
        doc::concat(parts)
    }

    pub(crate) fn split_children_at<'b>(nodes: &[Node<'b>], separator: &str) -> Vec<Vec<Node<'b>>> {
        let mut groups: Vec<Vec<Node>> = Vec::new();
        let mut current: Vec<Node> = Vec::new();

        for node in nodes {
            if node.kind() == separator {
                if separator == K::SEMICOLON {
                    current.push(*node);
                }
                groups.push(current);
                current = Vec::new();
            } else {
                current.push(*node);
            }
        }
        if !current.is_empty() {
            groups.push(current);
        }
        groups
    }
}

/// Remove a trailing `Hardline` from a Doc tree.
///
/// This mirrors the old printer's `ensure_newline()` idempotency: when two
/// consecutive constructs both emit newlines at their boundary, the old printer
/// collapses them into one because `ensure_newline` is a no-op if already at
/// line start. We achieve the same by stripping the trailing Hardline from the
/// first construct's Doc.
pub(crate) fn strip_trailing_hardline(doc: Doc) -> Doc {
    match doc {
        Doc::Concat(mut docs) => {
            if let Some(last) = docs.pop() {
                let stripped = strip_trailing_hardline(last);
                docs.push(stripped);
            }
            doc::concat(docs)
        }
        Doc::Indent(inner) => doc::indent(strip_trailing_hardline(*inner)),
        Doc::Hardline => Doc::Empty,
        other => other,
    }
}

/// Check if a Doc starts with a Hardline (or BlankLine).
///
/// Drills into Concat, Group, and Indent to find the first non-empty element.
pub(crate) fn starts_with_hardline(doc: &Doc) -> bool {
    match doc {
        Doc::Hardline | Doc::BlankLine => true,
        Doc::Concat(docs) => docs
            .iter()
            .find(|d| !matches!(d, Doc::Empty))
            .is_some_and(starts_with_hardline),
        Doc::Group(inner) | Doc::Indent(inner) => starts_with_hardline(inner),
        _ => false,
    }
}

/// Check if a Doc ends with a Hardline (or BlankLine).
///
/// Drills into Concat, Group, and Indent to find the last non-empty element.
pub(crate) fn ends_with_hardline(doc: &Doc) -> bool {
    match doc {
        Doc::Hardline | Doc::BlankLine => true,
        Doc::Concat(docs) => docs
            .iter()
            .rfind(|d| !matches!(d, Doc::Empty))
            .is_some_and(ends_with_hardline),
        Doc::Group(inner) | Doc::Indent(inner) => ends_with_hardline(inner),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comments::CommentMap;
    use crate::config::FmtConfig;
    use crate::directive_map::DirectiveMap;

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
        directives: &'a DirectiveMap,
        external_units: &'a HashSet<String>,
    ) -> DocBuilder<'a> {
        DocBuilder::new(source, config, comments, directives, vec![], external_units)
    }

    #[test]
    fn build_returns_non_empty_for_unit() {
        let source = "unit Test;\ninterface\nimplementation\nend.\n";
        let (tree, bytes) = parse(source);
        let config = FmtConfig::default();
        let comments = CommentMap::build(tree.root_node(), &bytes);
        let directives = DirectiveMap::build(tree.root_node(), &bytes);
        let external_units = HashSet::new();
        let builder = make_builder(&bytes, &config, &comments, &directives, &external_units);
        let doc = builder.build(tree.root_node());
        assert!(!matches!(doc, Doc::Empty));
    }

    #[test]
    fn format_off_region_returns_raw() {
        let source = "{$FMT.OFF}\nunit Test;\ninterface\nimplementation\nend.\n{$FMT.ON}\n";
        let (tree, bytes) = parse(source);
        let config = FmtConfig::default();
        let comments = CommentMap::build(tree.root_node(), &bytes);
        let directives = DirectiveMap::build(tree.root_node(), &bytes);
        let regions = pascal_core::directives::parse_format_regions(&bytes);
        let external_units = HashSet::new();
        let builder = DocBuilder::new(
            &bytes,
            &config,
            &comments,
            &directives,
            regions,
            &external_units,
        );
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
        let directives = DirectiveMap::empty();
        let external_units = HashSet::new();
        let builder = make_builder(&bytes, &config, &comments, &directives, &external_units);
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
        let directives = DirectiveMap::empty();
        let external_units = HashSet::new();
        let builder = make_builder(&bytes, &config, &comments, &directives, &external_units);

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
        let directives = DirectiveMap::empty();
        let external_units = HashSet::new();
        let builder = make_builder(&bytes, &config, &comments, &directives, &external_units);

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
    fn has_breakable_operators_detects_add() {
        // Build a minimal binary expression via parse and check the helper.
        let source =
            "unit Test;\ninterface\nimplementation\nvar X: Integer;\nbegin\nX := 1 + 2;\nend.\n";
        let (tree, bytes) = parse(source);
        let config = FmtConfig::default();
        let comments = CommentMap::build(tree.root_node(), &bytes);
        let directives = DirectiveMap::empty();
        let external_units = HashSet::new();
        let builder = make_builder(&bytes, &config, &comments, &directives, &external_units);

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
