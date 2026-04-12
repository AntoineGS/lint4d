use crate::comments::CommentMap;
use crate::config::{FmtConfig, OperatorPosition};
use crate::directive_map::DirectiveMap;
use crate::doc::{self, Doc};
use pascal_core::node_kind as K;
use pascal_core::FormatOffRegion;
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
#[allow(dead_code)]
pub struct DocBuilder<'a> {
    pub(crate) source: &'a [u8],
    pub(crate) config: &'a FmtConfig,
    pub(crate) comments: &'a CommentMap,
    directives: &'a DirectiveMap,
    format_regions: Vec<FormatOffRegion>,
    pub(crate) external_units: HashSet<String>,
}

impl<'a> DocBuilder<'a> {
    pub fn new(
        source: &'a [u8],
        config: &'a FmtConfig,
        comments: &'a CommentMap,
        directives: &'a DirectiveMap,
        format_regions: Vec<FormatOffRegion>,
        external_units: HashSet<String>,
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

    fn build_uses(&self, node: Node<'a>) -> Doc {
        let items = crate::uses::extract_uses_items(node, self.source);
        let indent_str = " ".repeat(self.config.indent_size);
        let formatted = crate::uses::format_uses_items(
            &items,
            &self.config.uses,
            &indent_str,
            &self.external_units,
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
        parts.push(Doc::Raw(formatted));
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

    fn build_block(&self, node: Node<'a>) -> Doc {
        self.build_children_preserving_blank_lines(node)
    }

    pub(crate) fn build_children_preserving_blank_lines(&self, node: Node<'a>) -> Doc {
        let children = self.code_children(node);
        let mut parts = Vec::new();
        let mut body: Vec<Doc> = Vec::new();
        // `statements` nodes (try/finally/repeat bodies) are already inside a
        // block — their parent handles indentation, so we start in block mode
        // but skip the indent wrapper when flushing.
        let is_bare_statements = node.kind() == K::STATEMENTS;
        let mut in_block = is_bare_statements;
        let mut prev_end_row: Option<usize> = None;
        let mut prev_kind = String::new();

        for child in &children {
            let kind = child.kind();

            match kind {
                K::K_BEGIN | K::K_TRY | K::K_REPEAT | K::K_ASM => {
                    // Flush any accumulated body (shouldn't have any before first opener)
                    if !body.is_empty() {
                        let flushed = doc::concat(std::mem::take(&mut body));
                        parts.push(if is_bare_statements {
                            flushed
                        } else {
                            doc::indent(flushed)
                        });
                    }
                    parts.push(Doc::Hardline);
                    parts.push(self.doc_for_node(*child));
                    in_block = true;
                    prev_end_row = Some(child.end_position().row);
                    prev_kind = kind.to_string();
                }
                K::K_END => {
                    // If kEnd has leading comments, they belong inside the body.
                    // Strip trailing Hardline — the handler adds its own before
                    // the keyword, so keeping it would create a blank line.
                    let leading = self.leading_comments_doc(*child);
                    if !matches!(leading, Doc::Empty) {
                        body.push(strip_trailing_hardline(leading));
                    }
                    // Flush body into Indent
                    if !body.is_empty() {
                        let flushed = doc::concat(std::mem::take(&mut body));
                        parts.push(if is_bare_statements {
                            flushed
                        } else {
                            doc::indent(flushed)
                        });
                    }
                    parts.push(Doc::Hardline);
                    // Build the kEnd node WITHOUT its leading comments (already emitted above)
                    let end_body = self.build_doc(*child);
                    let end_trailing = self.trailing_comments_doc(*child);
                    parts.push(doc::concat(vec![end_body, end_trailing]));
                    in_block = false;
                    prev_end_row = Some(child.end_position().row);
                    prev_kind = kind.to_string();
                }
                K::K_EXCEPT | K::K_FINALLY => {
                    // If except/finally has leading comments, they belong inside
                    // the body. Strip trailing Hardline — same reason as kEnd.
                    let leading = self.leading_comments_doc(*child);
                    if !matches!(leading, Doc::Empty) {
                        body.push(strip_trailing_hardline(leading));
                    }
                    // Flush body from try section
                    if !body.is_empty() {
                        let flushed = doc::concat(std::mem::take(&mut body));
                        parts.push(if is_bare_statements {
                            flushed
                        } else {
                            doc::indent(flushed)
                        });
                    }
                    parts.push(Doc::Hardline);
                    // Build except/finally WITHOUT its leading comments (already emitted above)
                    let kw_body = self.build_doc(*child);
                    let kw_trailing = self.trailing_comments_doc(*child);
                    parts.push(doc::concat(vec![kw_body, kw_trailing]));
                    // Start new body for except/finally section
                    in_block = true;
                    prev_end_row = Some(child.end_position().row);
                    prev_kind = kind.to_string();
                }
                K::SEMICOLON if in_block => {
                    // Semicolons inside blocks: emit only — the next child's
                    // Hardline provides the line break (matching the old
                    // printer's idempotent ensure_newline).
                    body.push(self.doc_for_node(*child));
                    prev_end_row = Some(child.end_position().row);
                    prev_kind = kind.to_string();
                }
                K::SEMICOLON => {
                    // Semicolons outside blocks (e.g. after end) — no trailing
                    // Hardline; the parent or next sibling provides it.
                    parts.push(self.doc_for_node(*child));
                    prev_end_row = Some(child.end_position().row);
                    prev_kind = kind.to_string();
                }
                _ => {
                    if in_block {
                        // Check for preserved blank lines between statements
                        let skip_blank = is_block_opener(&prev_kind) || is_block_closer(kind);
                        if !skip_blank && !prev_kind.is_empty() {
                            if let Some(prev_end) = prev_end_row {
                                if self.has_blank_line_between(prev_end, child.start_position().row)
                                {
                                    body.push(Doc::BlankLine);
                                }
                            }
                        }
                        // Build the child doc first, then check if it already
                        // starts with a Hardline (e.g. from leading comments
                        // on a descendant). If so, skip our own Hardline to
                        // match the old printer's idempotent ensure_newline.
                        let child_doc = self.doc_for_node(*child);
                        if !starts_with_hardline(&child_doc) {
                            body.push(Doc::Hardline);
                        }
                        body.push(child_doc);
                    } else {
                        // Preserve blank lines outside blocks
                        let skip_blank = prev_kind.is_empty()
                            || is_block_opener(&prev_kind)
                            || is_block_closer(kind);
                        if !skip_blank {
                            if let Some(prev_end) = prev_end_row {
                                if self.has_blank_line_between(prev_end, child.start_position().row)
                                {
                                    parts.push(Doc::BlankLine);
                                }
                            }
                        }
                        parts.push(self.doc_for_node(*child));
                    }
                    prev_end_row = Some(child.end_position().row);
                    prev_kind = kind.to_string();
                }
            }
        }

        // Flush any remaining body
        if !body.is_empty() {
            let flushed = doc::concat(body);
            parts.push(if is_bare_statements {
                flushed
            } else {
                doc::indent(flushed)
            });
        }

        doc::concat(parts)
    }

    fn build_section(&self, node: Node<'a>) -> Doc {
        let children = self.code_children(node);
        let section_kind = node.kind();
        let use_alignment = self.should_align(section_kind);

        let mut parts = Vec::new();
        let mut body_children: Vec<Node<'a>> = Vec::new();
        let mut keyword_end_row: Option<usize> = None;

        for child in &children {
            match child.kind() {
                K::K_VAR | K::K_CONST | K::K_TYPE => {
                    parts.push(Doc::Hardline);
                    parts.push(self.doc_for_node(*child));
                    keyword_end_row = Some(child.end_position().row);
                }
                _ => {
                    body_children.push(*child);
                }
            }
        }

        if body_children.is_empty() {
            return doc::concat(parts);
        }

        if use_alignment {
            let aligned = self.build_aligned_section(&body_children, section_kind, keyword_end_row);
            parts.push(doc::indent(aligned));
        } else {
            let mut body_parts = Vec::new();
            let mut prev_child_kind = String::new();
            let mut prev_single_line = false;
            let mut prev_end_row: Option<usize> = keyword_end_row;

            for child in &body_children {
                let kind = child.kind().to_string();
                let single_line = child.start_position().row == child.end_position().row;
                let child_doc = self.doc_for_node(*child);
                let source_blank = !prev_child_kind.is_empty()
                    && prev_end_row.is_some_and(|prev_end| {
                        self.has_blank_line_between(prev_end, child.start_position().row)
                    });
                if source_blank
                    || (kind == K::DECL_TYPE
                        && prev_child_kind == K::DECL_TYPE
                        && !(prev_single_line && single_line))
                {
                    body_parts.push(Doc::BlankLine);
                } else if !starts_with_hardline(&child_doc) {
                    body_parts.push(Doc::Hardline);
                }
                body_parts.push(child_doc);
                prev_child_kind = kind;
                prev_single_line = single_line;
                prev_end_row = Some(child.end_position().row);
            }
            if !body_parts.is_empty() {
                parts.push(doc::indent(doc::concat(body_parts)));
            }
        }
        doc::concat(parts)
    }

    fn build_decl_section(&self, node: Node<'a>) -> Doc {
        let children = self.code_children(node);
        let align_fields = self.should_align("fields");
        let align_props = self.should_align("properties");
        let mut parts = Vec::new();
        let mut body_children: Vec<Node<'a>> = Vec::new();
        let mut first = true;
        let mut after_strict = false;
        let mut visibility_end_row: Option<usize> = None;

        for child in &children {
            match child.kind() {
                K::K_PUBLIC | K::K_PRIVATE | K::K_PROTECTED | K::K_PUBLISHED | K::K_STRICT => {
                    let is_strict = child.kind() == K::K_STRICT;

                    if after_strict {
                        parts.push(Doc::Raw(" ".into()));
                        parts.push(Doc::Raw(self.node_text(*child)));
                        parts.push(Doc::Hardline);
                        after_strict = false;
                    } else {
                        if first {
                            parts.push(Doc::Hardline);
                            parts.push(self.doc_for_node(*child));
                            first = false;
                        } else {
                            parts.push(Doc::Raw(" ".into()));
                            parts.push(Doc::Raw(self.node_text(*child)));
                        }
                        if is_strict {
                            after_strict = true;
                        } else {
                            parts.push(Doc::Hardline);
                        }
                    }

                    visibility_end_row = Some(child.end_position().row);
                }
                _ => {
                    body_children.push(*child);
                }
            }
        }

        if body_children.is_empty() {
            return doc::concat(parts);
        }

        if align_fields || align_props {
            // Group consecutive fields and properties for alignment.
            let body = self.build_aligned_decl_section_body(
                &body_children,
                visibility_end_row,
                align_fields,
                align_props,
            );
            parts.push(strip_trailing_hardline(doc::indent(body)));
        } else {
            let mut body_parts: Vec<Doc> = Vec::new();
            let mut prev_end_row = visibility_end_row;

            for child in &body_children {
                let child_doc = self.doc_for_node(*child);
                if let Some(prev_end) = prev_end_row {
                    if self.has_blank_line_between(prev_end, child.start_position().row) {
                        body_parts.push(Doc::BlankLine);
                    } else if !body_parts.is_empty() {
                        let prev_ends = body_parts.last().is_some_and(ends_with_hardline);
                        if !prev_ends && !starts_with_hardline(&child_doc) {
                            body_parts.push(Doc::Hardline);
                        }
                    }
                }
                body_parts.push(child_doc);
                prev_end_row = Some(child.end_position().row);
            }
            if !body_parts.is_empty() {
                let body = doc::indent(doc::concat(body_parts));
                parts.push(strip_trailing_hardline(body));
            }
        }
        doc::concat(parts)
    }

    fn build_def_proc(&self, node: Node<'a>) -> Doc {
        // defProc children: declProc (signature), optional local sections
        // (declVars/declConsts/declTypes), block (begin..end), final ;
        // The old printer just recurses, relying on ensure_newline() being
        // idempotent. We need to suppress trailing Hardlines from declProc
        // before sections/blocks that start with their own Hardline.
        let children = self.code_children(node);
        let mut parts = Vec::new();

        for (i, child) in children.iter().enumerate() {
            let kind = child.kind();

            // Nested DEF_PROC children don't produce their own leading
            // Hardline (unlike sections and blocks), so we insert one.
            if kind == K::DEF_PROC {
                parts.push(Doc::Hardline);
            }

            let doc = self.doc_for_node(*child);

            // Nested procedures must be indented one level relative to
            // their parent procedure so they align with local var bodies.
            let doc = if kind == K::DEF_PROC {
                doc::indent(doc)
            } else {
                doc
            };

            // Check if the next child starts with a Hardline (sections,
            // blocks, and nested procs do). If so, strip trailing Hardline
            // from this child's doc to avoid a blank line.
            let next_starts_with_hardline = children.get(i + 1).is_some_and(|next| {
                matches!(
                    next.kind(),
                    K::DECL_VARS
                        | K::DECL_CONSTS
                        | K::DECL_TYPES
                        | K::BLOCK
                        | K::STATEMENTS
                        | K::DEF_PROC
                )
            });

            if next_starts_with_hardline {
                parts.push(strip_trailing_hardline(doc));
            } else {
                parts.push(doc);
            }
        }
        doc::concat(parts)
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
        let children = self.code_children(node);
        let mut parts: Vec<Doc> = Vec::new();
        let mut seen_open_bracket = false;
        for child in &children {
            if child.kind() == K::OPEN_BRACKET {
                if seen_open_bracket {
                    parts.push(Doc::Hardline);
                }
                seen_open_bracket = true;
            }
            parts.push(self.doc_for_node(*child));
        }
        parts.push(Doc::Hardline);
        doc::concat(parts)
    }

    fn build_decl_proc(&self, node: Node<'a>) -> Doc {
        let children = self.code_children(node);
        let mut parts = Vec::new();
        let mut i = 0;
        while i < children.len() {
            let child = children[i];
            match child.kind() {
                K::SEMICOLON => {
                    let next = children.get(i + 1);
                    parts.push(self.doc_for_node(child));
                    if next.is_some_and(|n| matches!(n.kind(), K::PROC_ATTRIBUTE | K::K_FORWARD)) {
                        parts.push(Doc::Raw(" ".into()));
                    } else {
                        parts.push(Doc::Hardline);
                    }
                }
                _ => {
                    parts.push(self.doc_for_node(child));
                }
            }
            i += 1;
        }
        doc::concat(parts)
    }

    /// Build a declaration that may contain comma-separated identifiers
    /// sharing a type (e.g. `A, B, C: Integer`).
    ///
    /// For `DECL_VAR` nodes the identifiers are always expanded into
    /// separate declarations (one per line).  For `DECL_ARG` / `DECL_FIELD`
    /// the list is wrapped in a Group so commas become break points only
    /// when the line overflows.
    fn build_comma_ident_decl(&self, node: Node<'a>) -> Doc {
        let children = self.code_children(node);

        // No commas → no internal break needed.
        if !children.iter().any(|c| c.kind() == K::COMMA) {
            // Fix alias keyword misparse: if a declVar has a procAttribute
            // child whose first child is kAlias, the parser incorrectly
            // merged two declarations.  Split them onto separate lines.
            if node.kind() == K::DECL_VAR {
                if let Some(doc) = self.build_alias_misparse_split(node, &children) {
                    return doc;
                }
            }
            return self.build_children(node);
        }

        // Split at the colon: identifiers before, type/default after.
        let colon_idx = children.iter().position(|c| c.kind() == K::COLON);
        let (before_colon, from_colon) = match colon_idx {
            Some(idx) => (&children[..idx], &children[idx..]),
            None => (&children[..], &[][..]),
        };

        // ── DECL_VAR: always expand into separate declarations ──────
        if node.kind() == K::DECL_VAR {
            let idents: Vec<Node<'a>> = before_colon
                .iter()
                .copied()
                .filter(|c| c.kind() == K::IDENTIFIER)
                .collect();
            if !idents.is_empty() && !from_colon.is_empty() {
                let suffix_docs: Vec<Doc> =
                    from_colon.iter().map(|c| self.doc_for_node(*c)).collect();
                let suffix = doc::concat(suffix_docs);

                let mut parts = Vec::new();
                for (i, ident) in idents.iter().enumerate() {
                    if i > 0 {
                        parts.push(Doc::Hardline);
                    }
                    parts.push(doc::concat(vec![self.doc_for_node(*ident), suffix.clone()]));
                }
                return doc::concat(parts);
            }
        }

        // ── DECL_ARG / DECL_FIELD: wrap in a Group for optional breaking ─
        // Separate prefix keywords (var/const/out) from identifier list.
        let first_ident = before_colon
            .iter()
            .position(|c| c.kind() == K::IDENTIFIER)
            .unwrap_or(0);
        let prefix = &before_colon[..first_ident];
        let ident_list = &before_colon[first_ident..];

        let prefix_docs: Vec<Doc> = prefix.iter().map(|c| self.doc_for_node(*c)).collect();

        // Split identifiers at commas.
        let groups = Self::split_children_at(ident_list, K::COMMA);
        let group_docs: Vec<Doc> = groups
            .iter()
            .map(|g| {
                let parts: Vec<Doc> = g.iter().map(|n| self.doc_for_node(*n)).collect();
                doc::concat(parts)
            })
            .collect();

        let mut ident_parts = Vec::new();
        for (i, gdoc) in group_docs.into_iter().enumerate() {
            if i > 0 {
                ident_parts.push(doc::token(",", K::COMMA, node.kind()));
                ident_parts.push(Doc::Line);
            }
            ident_parts.push(gdoc);
        }

        let suffix_docs: Vec<Doc> = from_colon.iter().map(|c| self.doc_for_node(*c)).collect();

        doc::group(doc::concat(vec![
            doc::concat(prefix_docs),
            doc::concat(ident_parts),
            doc::concat(suffix_docs),
        ]))
    }

    /// Handle the alias keyword misparse for non-alignment mode.
    ///
    /// When a `declVar` contains a `procAttribute` starting with `kAlias`,
    /// the parser has incorrectly merged two variable declarations.  Render
    /// the original declaration up to the first `;`, then emit a `Hardline`
    /// and render the alias portion as a separate declaration.
    fn build_alias_misparse_split(&self, _node: Node<'a>, children: &[Node<'a>]) -> Option<Doc> {
        let proc_attr_idx = children
            .iter()
            .position(|c| c.kind() == K::PROC_ATTRIBUTE)?;
        let proc_attr = children[proc_attr_idx];
        let attr_children = self.code_children(proc_attr);
        if attr_children.is_empty() || attr_children[0].kind() != "kAlias" {
            return None;
        }

        // Render the main declaration (up to and including the first semicolon).
        let first_semi_idx = children.iter().position(|c| c.kind() == K::SEMICOLON)?;
        let main_parts: Vec<Doc> = children[..=first_semi_idx]
            .iter()
            .map(|c| self.doc_for_node(*c))
            .collect();

        let mut parts = vec![doc::concat(main_parts)];

        // Reconstruct the alias as a separate declaration on the next line.
        let alias_text = self.node_text(attr_children[0]);
        let name_doc = doc::token(alias_text, K::IDENTIFIER, K::DECL_VAR);

        let colon_in_attr = attr_children.iter().position(|c| c.kind() == K::COLON);
        if let Some(ci) = colon_in_attr {
            let mut alias_parts = vec![name_doc];
            for c in &attr_children[ci..] {
                alias_parts.push(self.doc_for_node(*c));
            }
            // Add the semicolon that follows the procAttribute.
            if let Some(semi) = children.get(proc_attr_idx + 1) {
                if semi.kind() == K::SEMICOLON {
                    alias_parts.push(self.doc_for_node(*semi));
                }
            }
            parts.push(Doc::Hardline);
            parts.push(doc::concat(alias_parts));
        }

        Some(doc::concat(parts))
    }

    fn build_args(&self, node: Node<'a>) -> Doc {
        self.build_paren_list(node, K::SEMICOLON)
    }

    fn build_call(&self, node: Node<'a>) -> Doc {
        let children = self.code_children(node);

        // Unwrap exprArgs: the tree-sitter grammar wraps call arguments in an
        // exprArgs node, hiding commas from the delimiter-list splitter.
        // Inline exprArgs's children so each argument gets its own line break.
        let mut flat: Vec<Node<'a>> = Vec::new();
        for child in &children {
            if child.kind() == K::EXPR_ARGS {
                for sub in child.children(&mut child.walk()) {
                    if !sub.is_extra() {
                        flat.push(sub);
                    }
                }
            } else {
                flat.push(*child);
            }
        }

        // Hug a sole bracket-list argument: keep `([` and `])` together by
        // merging the outer call group with the inner bracket group.
        if let Some(hugged) = self.try_build_call_hugging_bracket(&flat) {
            return hugged;
        }

        self.build_delimited_list(&flat, K::COMMA, K::OPEN_PAREN, K::CLOSE_PAREN)
    }

    /// When a call has exactly one argument and that argument is an
    /// `exprBrackets` node, produce a single merged group so that `([` and
    /// `])` stay on the same line:
    ///
    /// ```text
    /// Foo([        ← not   Foo(
    ///   A,         ←         [A, B]
    ///   B          ←       )
    /// ]);
    /// ```
    fn try_build_call_hugging_bracket(&self, flat: &[Node<'a>]) -> Option<Doc> {
        let open_idx = flat.iter().position(|c| c.kind() == K::OPEN_PAREN)?;
        let close_idx = flat.iter().rposition(|c| c.kind() == K::CLOSE_PAREN)?;

        let inner = &flat[open_idx + 1..close_idx];
        if inner.len() != 1 || inner[0].kind() != K::EXPR_BRACKETS {
            return None;
        }
        let bracket_node = inner[0];
        let bracket_children = self.code_children(bracket_node);

        let b_open_idx = bracket_children
            .iter()
            .position(|c| c.kind() == K::OPEN_BRACKET)?;
        let b_close_idx = bracket_children
            .iter()
            .rposition(|c| c.kind() == K::CLOSE_BRACKET)?;
        let b_inner = &bracket_children[b_open_idx + 1..b_close_idx];

        // Build "before" for outer call (everything up to and including `(`)
        let mut before: Vec<Doc> = flat[..=open_idx]
            .iter()
            .map(|c| self.doc_for_node(*c))
            .collect();
        // Immediately append `[` — no softline between `(` and `[`
        before.push(self.doc_for_node(bracket_children[b_open_idx]));

        if b_inner.is_empty() {
            before.push(self.doc_for_node(bracket_children[b_close_idx]));
            before.push(self.doc_for_node(flat[close_idx]));
            for c in &flat[close_idx + 1..] {
                before.push(self.doc_for_node(*c));
            }
            return Some(doc::concat(before));
        }

        let groups = Self::split_children_at(b_inner, K::COMMA);
        let group_docs: Vec<Doc> = groups
            .iter()
            .map(|g| {
                let parts: Vec<Doc> = g.iter().map(|n| self.doc_for_node(*n)).collect();
                doc::concat(parts)
            })
            .collect();

        let mut inner_parts = Vec::new();
        for (i, gdoc) in group_docs.into_iter().enumerate() {
            if i > 0 {
                inner_parts.push(doc::token(",", K::COMMA, ""));
                inner_parts.push(Doc::Line);
            }
            inner_parts.push(gdoc);
        }

        let grouped = doc::group(doc::concat(vec![
            doc::concat(before),
            doc::indent(doc::concat(vec![Doc::Softline, doc::concat(inner_parts)])),
            Doc::Softline,
            self.doc_for_node(bracket_children[b_close_idx]),
            self.doc_for_node(flat[close_idx]),
        ]));

        let mut result = vec![grouped];
        for c in &flat[close_idx + 1..] {
            result.push(self.doc_for_node(*c));
        }
        Some(doc::concat(result))
    }

    fn build_bracket_list(&self, node: Node<'a>) -> Doc {
        let children = self.code_children(node);
        self.build_delimited_list(&children, K::COMMA, K::OPEN_BRACKET, K::CLOSE_BRACKET)
    }

    /// Build a parenthesised list with Group-based line breaking.
    fn build_paren_list(&self, node: Node<'a>, separator: &str) -> Doc {
        let children = self.code_children(node);
        self.build_delimited_list(&children, separator, K::OPEN_PAREN, K::CLOSE_PAREN)
    }

    /// Build a delimited list (parens or brackets) with Group-based line breaking.
    ///
    /// When the content fits on one line, keeps it flat.
    /// When it overflows, puts each separator-delimited item on its own line.
    fn build_delimited_list(
        &self,
        children: &[Node<'a>],
        separator: &str,
        open_kind: &str,
        close_kind: &str,
    ) -> Doc {
        let open_idx = children.iter().position(|c| c.kind() == open_kind);
        let close_idx = children.iter().rposition(|c| c.kind() == close_kind);

        let (open_idx, close_idx) = match (open_idx, close_idx) {
            (Some(o), Some(c)) => (o, c),
            _ => {
                let docs: Vec<Doc> = children.iter().map(|c| self.doc_for_node(*c)).collect();
                return doc::concat(docs);
            }
        };

        let mut before: Vec<Doc> = children[..=open_idx]
            .iter()
            .map(|c| self.doc_for_node(*c))
            .collect();

        let inner = &children[open_idx + 1..close_idx];
        if inner.is_empty() {
            before.push(self.doc_for_node(children[close_idx]));
            for c in &children[close_idx + 1..] {
                before.push(self.doc_for_node(*c));
            }
            return doc::concat(before);
        }

        // Check for // line comments in the inner content — when present the
        // list MUST break because a line comment eats the rest of the line.
        // Skip string literals (delimited by single quotes) so that `//`
        // inside strings like `'http://'` is not mistaken for a comment.
        let inner_start = children[open_idx].end_byte();
        let inner_end = children[close_idx].start_byte();
        let has_line_comments = {
            let bytes = &self.source[inner_start..inner_end];
            let mut in_string = false;
            let mut found = false;
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if in_string {
                        if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                            i += 2; // escaped quote ''
                            continue;
                        }
                        in_string = false;
                    } else {
                        in_string = true;
                    }
                } else if !in_string
                    && bytes[i] == b'/'
                    && i + 1 < bytes.len()
                    && bytes[i + 1] == b'/'
                {
                    found = true;
                    break;
                }
                i += 1;
            }
            found
        };

        // Split into groups while collecting the separator nodes so their
        // trailing comments (e.g. `// & prefix` after a comma) are preserved.
        let mut groups: Vec<Vec<Node<'a>>> = Vec::new();
        let mut sep_nodes: Vec<Node<'a>> = Vec::new();
        let mut current: Vec<Node<'a>> = Vec::new();

        for node in inner {
            if node.kind() == separator {
                if separator == K::SEMICOLON {
                    current.push(*node);
                }
                groups.push(std::mem::take(&mut current));
                if separator != K::SEMICOLON {
                    sep_nodes.push(*node);
                }
            } else {
                current.push(*node);
            }
        }
        if !current.is_empty() {
            groups.push(current);
        }

        let group_docs: Vec<Doc> = groups
            .iter()
            .map(|g| {
                let parts: Vec<Doc> = g.iter().map(|n| self.doc_for_node(*n)).collect();
                doc::concat(parts)
            })
            .collect();

        let mut inner_parts = Vec::new();
        for (i, gdoc) in group_docs.into_iter().enumerate() {
            if i > 0 {
                if separator == K::COMMA {
                    if let Some(sep_node) = sep_nodes.get(i - 1) {
                        inner_parts.push(self.doc_for_node(*sep_node));
                    } else {
                        inner_parts.push(doc::token(",", K::COMMA, ""));
                    }
                }
                if has_line_comments {
                    inner_parts.push(Doc::Hardline);
                } else {
                    inner_parts.push(Doc::Line);
                }
            }
            inner_parts.push(gdoc);
        }

        let body = doc::concat(vec![
            doc::concat(before),
            doc::indent(doc::concat(vec![
                if has_line_comments {
                    Doc::Hardline
                } else {
                    Doc::Softline
                },
                doc::concat(inner_parts),
            ])),
            if has_line_comments {
                Doc::Hardline
            } else {
                Doc::Softline
            },
            self.doc_for_node(children[close_idx]),
        ]);

        let grouped = if has_line_comments {
            body
        } else {
            doc::group(body)
        };

        let mut result = vec![grouped];
        for c in &children[close_idx + 1..] {
            result.push(self.doc_for_node(*c));
        }
        doc::concat(result)
    }

    fn split_children_at<'b>(nodes: &[Node<'b>], separator: &str) -> Vec<Vec<Node<'b>>> {
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

    fn build_expression_breaking(&self, node: Node<'a>) -> Doc {
        let binary_node = if node.kind() == K::EXPR_BINARY {
            node
        } else {
            match node
                .children(&mut node.walk())
                .find(|c| c.kind() == K::EXPR_BINARY)
            {
                Some(bin) => bin,
                None => return self.build_children(node),
            }
        };

        let segments = flatten_binary_chain(binary_node, None);
        if segments.len() <= 1 {
            return self.build_children(node);
        }

        // Determine the chain's operator family.
        let is_add_chain = segments
            .iter()
            .skip(1)
            .all(|s| s.operator.is_none_or(|op| op.kind() == K::K_ADD));
        let is_bool_chain = segments.iter().skip(1).all(|s| {
            s.operator
                .is_none_or(|op| matches!(op.kind(), K::K_OR | K::K_AND))
        });

        let is_multiline = binary_node.start_position().row != binary_node.end_position().row;

        let break_style = if is_bool_chain {
            // or/and chains: expand all (one per line) when they overflow.
            BreakStyle::ExpandAll
        } else if is_add_chain && is_multiline {
            // + chains: greedy pack, but preserve author breaks.
            BreakStyle::PreserveBreaks
        } else {
            // Everything else: greedy pack.
            BreakStyle::GreedyFill
        };

        self.build_binary_chain_doc(&segments, break_style)
    }

    /// Build a flattened binary chain as a Doc IR.
    ///
    /// Operator placement depends on `config.operator_position`:
    /// - `Leading` (default): operator starts the continuation line
    /// - `Trailing`: operator ends the previous line
    ///
    /// Break behaviour depends on `break_style`:
    /// - `GreedyFill`: pack as many operands per line as fit (Fill).
    /// - `PreserveBreaks`: like GreedyFill, but positions where the source
    ///   had a newline use `PreservedLine` (forced break in Fill, joinable
    ///   when a parent `Group` determines the whole expression fits).
    /// - `ExpandAll`: all-or-nothing — either the whole chain fits on one
    ///   line, or every operand gets its own line.
    pub(crate) fn build_binary_chain_doc(
        &self,
        segments: &[BinarySegment],
        break_style: BreakStyle,
    ) -> Doc {
        let trailing = self.config.operator_position == OperatorPosition::Trailing;

        // Build the first operand (no operator).
        let mut first_parts = Vec::new();
        if let Some(seg) = segments.first() {
            for n in &seg.operand {
                first_parts.push(self.doc_for_node(*n));
            }
            // Trailing: attach the *next* segment's operator to the first operand.
            if trailing && segments.len() > 1 {
                if let Some(op) = segments[1].operator {
                    first_parts.push(self.doc_for_node(op));
                }
            }
        }

        if segments.len() <= 1 {
            return doc::concat(first_parts);
        }

        // Build parts: [sep, content, sep, content, ...]
        let mut parts = Vec::new();

        for i in 1..segments.len() {
            let seg = &segments[i];

            let sep = if break_style == BreakStyle::PreserveBreaks
                && has_newline_between(self.source, &segments[i - 1], seg)
            {
                Doc::PreservedLine
            } else {
                Doc::Line
            };
            parts.push(sep);

            // Build content item (operator + operand or operand + operator).
            let mut item = Vec::new();
            if trailing {
                // Trailing: operand first, then next segment's operator (if any).
                for n in &seg.operand {
                    item.push(self.doc_for_node(*n));
                }
                if i + 1 < segments.len() {
                    if let Some(op) = segments[i + 1].operator {
                        item.push(self.doc_for_node(op));
                    }
                }
            } else {
                // Leading: operator first, then operand.
                if let Some(op) = seg.operator {
                    item.push(self.doc_for_node(op));
                }
                for n in &seg.operand {
                    item.push(self.doc_for_node(*n));
                }
            }
            parts.push(doc::concat(item));
        }

        let continuation = if break_style == BreakStyle::ExpandAll {
            // All-or-nothing: Group + Concat with Line separators.
            doc::concat(parts)
        } else {
            // Greedy packing via Fill.
            doc::fill(parts)
        };

        doc::group(doc::concat(vec![
            doc::concat(first_parts),
            doc::indent(continuation),
        ]))
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

fn is_block_opener(kind: &str) -> bool {
    matches!(kind, K::K_BEGIN | K::K_TRY | K::K_REPEAT | K::K_ASM)
}

fn is_block_closer(kind: &str) -> bool {
    matches!(kind, K::K_END | K::K_EXCEPT | K::K_FINALLY)
}

/// A segment of a flattened binary expression chain.
pub(crate) struct BinarySegment<'a> {
    pub operator: Option<Node<'a>>,
    pub operand: Vec<Node<'a>>,
}

pub(crate) fn flatten_binary_chain<'a>(
    node: Node<'a>,
    only_ops: Option<&[&str]>,
) -> Vec<BinarySegment<'a>> {
    let mut segments = Vec::new();
    flatten_binary_chain_inner(node, only_ops, &mut segments);
    segments
}

fn flatten_binary_chain_inner<'a>(
    node: Node<'a>,
    only_ops: Option<&[&str]>,
    segments: &mut Vec<BinarySegment<'a>>,
) {
    let children: Vec<Node<'a>> = node
        .children(&mut node.walk())
        .filter(|c| !c.is_extra())
        .collect();

    if children.len() != 3 {
        segments.push(BinarySegment {
            operator: None,
            operand: vec![node],
        });
        return;
    }

    let left = children[0];
    let op = children[1];
    let right = children[2];

    if let Some(allowed) = only_ops {
        if !allowed.contains(&op.kind()) {
            segments.push(BinarySegment {
                operator: None,
                operand: vec![node],
            });
            return;
        }
    }

    if left.kind() == K::EXPR_BINARY {
        flatten_binary_chain_inner(left, only_ops, segments);
    } else {
        segments.push(BinarySegment {
            operator: None,
            operand: vec![left],
        });
    }

    segments.push(BinarySegment {
        operator: Some(op),
        operand: vec![right],
    });
}

/// Return `true` if the source text between two consecutive binary chain
/// segments contains a newline, indicating the author intentionally broke
/// the expression across lines.
///
/// Checks from the end of the previous operand to the start of the current
/// operand (spanning the operator in between). This covers both leading
/// (`\n  + operand`) and trailing (`operand +\n`) break styles.
fn has_newline_between(source: &[u8], prev: &BinarySegment, curr: &BinarySegment) -> bool {
    let prev_end = prev.operand.last().map(|n| n.end_byte()).unwrap_or(0);
    let curr_start = curr
        .operand
        .first()
        .map(|n| n.start_byte())
        .unwrap_or(prev_end);
    if curr_start <= prev_end {
        return false;
    }
    source[prev_end..curr_start].contains(&b'\n')
}

#[allow(dead_code)]
pub(crate) fn split_at_and_or<'a>(nodes: &[Node<'a>]) -> Vec<BinarySegment<'a>> {
    let mut segments: Vec<BinarySegment<'a>> = Vec::new();
    let mut current_operand: Vec<Node<'a>> = Vec::new();

    for node in nodes {
        if node.kind() == K::K_AND || node.kind() == K::K_OR {
            if !current_operand.is_empty() {
                segments.push(BinarySegment {
                    operator: None,
                    operand: std::mem::take(&mut current_operand),
                });
            }
            segments.push(BinarySegment {
                operator: Some(*node),
                operand: Vec::new(),
            });
        } else if let Some(last_seg) = segments.last_mut() {
            if last_seg.operator.is_some() && last_seg.operand.is_empty() {
                last_seg.operand.push(*node);
            } else {
                current_operand.push(*node);
            }
        } else {
            current_operand.push(*node);
        }
    }

    if !current_operand.is_empty() {
        segments.push(BinarySegment {
            operator: None,
            operand: current_operand,
        });
    }

    segments
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
    ) -> DocBuilder<'a> {
        DocBuilder::new(source, config, comments, directives, vec![], HashSet::new())
    }

    #[test]
    fn build_returns_non_empty_for_unit() {
        let source = "unit Test;\ninterface\nimplementation\nend.\n";
        let (tree, bytes) = parse(source);
        let config = FmtConfig::default();
        let comments = CommentMap::build(tree.root_node(), &bytes);
        let directives = DirectiveMap::build(tree.root_node(), &bytes);
        let builder = make_builder(&bytes, &config, &comments, &directives);
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
        let builder = DocBuilder::new(
            &bytes,
            &config,
            &comments,
            &directives,
            regions,
            HashSet::new(),
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
        let builder = make_builder(&bytes, &config, &comments, &directives);
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
        let builder = make_builder(&bytes, &config, &comments, &directives);

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
        let builder = make_builder(&bytes, &config, &comments, &directives);

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
        let directives = DirectiveMap::empty();
        let builder = make_builder(&bytes, &config, &comments, &directives);

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
        let directives = DirectiveMap::empty();
        let builder = make_builder(&bytes, &config, &comments, &directives);

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
