use crate::comments::CommentMap;
use crate::config::FmtConfig;
use crate::directive_map::DirectiveMap;
use crate::doc::{self, Doc};
use pascal_core::FormatOffRegion;
use pascal_core::node_kind as K;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
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

/// Rows of every newline-terminated line that is empty or whitespace-only.
fn blank_line_rows(source: &[u8]) -> Vec<u32> {
    let mut rows = Vec::new();
    let mut line_start = 0;
    for (row, line_end) in source
        .iter()
        .enumerate()
        .filter_map(|(i, &b)| (b == b'\n').then_some(i))
        .enumerate()
    {
        if source[line_start..line_end]
            .iter()
            .all(|c| c.is_ascii_whitespace())
        {
            rows.push(row as u32);
        }
        line_start = line_end + 1;
    }
    rows
}

/// A trailing comment or directive, as laid out by
/// [`DocBuilder::trailing_run_doc`].
pub(crate) struct TrailingItem<'m> {
    pub(crate) span: Range<usize>,
    pub(crate) text: &'m str,
    /// Bytes between the code token and the item in the source.
    pub(crate) gap: usize,
    pub(crate) is_comment: bool,
}

impl TrailingItem<'_> {
    pub(crate) fn is_line_comment(&self) -> bool {
        self.is_comment && self.text.trim_start().starts_with("//")
    }
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
    pub(crate) directives: &'a DirectiveMap,
    format_regions: Vec<FormatOffRegion>,
    pub(crate) external_units: &'a HashSet<String>,
    /// Ascending 0-based rows of newline-terminated, whitespace-only lines.
    blank_line_rows: Vec<u32>,
    /// Parent kind of every node, by node id, filled by [`Self::build`].
    /// `Node::parent()` is O(depth) and a long binary chain is that deep.
    parent_kinds: RefCell<HashMap<usize, &'static str>>,
    /// Set while building a copy of a node whose comments and directives
    /// are emitted with another copy (see [`Self::without_trivia`]).
    trivia_suppressed: Cell<bool>,
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
            blank_line_rows: blank_line_rows(source),
            parent_kinds: RefCell::new(HashMap::new()),
            trivia_suppressed: Cell::new(false),
        }
    }

    /// Entry point: build a `Doc` for the entire AST rooted at `root`.
    pub fn build(&self, root: Node<'a>) -> Doc {
        self.index_parent_kinds(root);
        let body = self.doc_for_node(root);
        if self.is_in_format_off_region(root) {
            // The verbatim root text already spans the end-of-file trivia.
            return body;
        }
        doc::concat(vec![body, self.eof_trivia_doc(root)])
    }

    /// Record the parent kind of every node under `root` in one walk.
    fn index_parent_kinds(&self, root: Node<'a>) {
        let mut kinds = self.parent_kinds.borrow_mut();
        kinds.clear();
        let mut cursor = root.walk();
        let mut parents: Vec<&'static str> = Vec::new();
        'walk: loop {
            let node = cursor.node();
            kinds.insert(node.id(), parents.last().copied().unwrap_or(""));
            if cursor.goto_first_child() {
                parents.push(node.kind());
                continue;
            }
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    break 'walk;
                }
                parents.pop();
            }
        }
    }

    /// Kind of `node`'s parent: from the index when built, else `parent()`.
    fn parent_kind(&self, node: Node<'a>) -> &'static str {
        match self.parent_kinds.borrow().get(&node.id()) {
            Some(kind) => kind,
            None => node.parent().map(|p| p.kind()).unwrap_or(""),
        }
    }

    /// Comments and directives after the file's last leaf, laid out as in
    /// the source (see [`Self::trivia_run_doc`]). Items inside a
    /// `{$FMT.OFF}` region are emitted verbatim instead.
    fn eof_trivia_doc(&self, root: Node<'a>) -> Doc {
        let mut trivia = self.trivia_items(
            self.comments.eof_comments(),
            self.directives.eof_directives(),
        );
        if trivia.is_empty() {
            return Doc::Empty;
        }
        let last = last_leaf(root);
        // Without a code leaf, the first item starts the output.
        let mut prev_end = (last.id() != root.id()).then(|| {
            self.comments
                .trailing_comments(last.id())
                .iter()
                .map(|c| c.span.end)
                .chain(
                    self.directives
                        .trailing_directives(last.id())
                        .iter()
                        .map(|d| d.span.end),
                )
                .fold(last.end_byte(), usize::max)
        });
        let mut parts = Vec::new();
        while !trivia.is_empty() {
            let off = trivia
                .iter()
                .position(|(span, _)| self.in_format_off_region(span.start));
            let run: Vec<_> = trivia.drain(..off.unwrap_or(trivia.len())).collect();
            let (run_doc, end) = self.trivia_run_doc(run, prev_end);
            parts.push(run_doc);
            prev_end = end;
            if off.is_none() {
                break;
            }
            let verbatim = trivia
                .iter()
                .position(|(span, _)| !self.in_format_off_region(span.start))
                .unwrap_or(trivia.len());
            let region: Vec<_> = trivia.drain(..verbatim).collect();
            let (first, end) = (region[0].0.start, region[region.len() - 1].0.end);
            // The region began before the file's last token: keep the gap
            // after it too.
            let start = match prev_end {
                Some(prev) if self.in_format_off_region(prev.saturating_sub(1)) => prev,
                _ => first,
            };
            parts.push(self.verbatim_doc(start, end));
            prev_end = Some(end);
        }
        doc::concat(parts)
    }

    /// Whether the line holding source byte `offset` is inside a
    /// `{$FMT.OFF}` region.
    fn in_format_off_region(&self, offset: usize) -> bool {
        let line = 1 + self.source[..offset.min(self.source.len())]
            .iter()
            .filter(|&&b| b == b'\n')
            .count();
        self.format_regions
            .iter()
            .any(|r| line >= r.start_line && line <= r.end_line)
    }

    /// Source text between two byte offsets, emitted as written. Text that
    /// starts a line in the source starts one in the output, with its
    /// source indentation.
    fn verbatim_doc(&self, start: usize, end: usize) -> Doc {
        let line_start = self.source[..start]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1);
        let prefix = &self.source[line_start..start];
        let line = if start > 0 && prefix.iter().all(|&b| b == b' ' || b == b'\t') {
            Doc::LineStart(String::from_utf8_lossy(prefix).into_owned())
        } else {
            Doc::Empty
        };
        let text = pascal_core::decode_bytes(&self.source[start..end]).replace('\r', "");
        doc::concat(vec![line, Doc::Raw(text)])
    }

    /// Pair attached comments and directives with their docs, in source order.
    fn trivia_items(
        &self,
        comments: &[crate::comments::AttachedComment],
        directives: &[crate::directive_map::AttachedDirective],
    ) -> Vec<(Range<usize>, Doc)> {
        let mut trivia: Vec<(Range<usize>, Doc)> = comments
            .iter()
            .map(|c| (c.span.clone(), doc::token(c.text.clone(), K::COMMENT, "")))
            .chain(directives.iter().map(|d| {
                (
                    d.span.clone(),
                    doc::token(d.text.clone(), K::PP_DIRECTIVE, ""),
                )
            }))
            .collect();
        trivia.sort_by_key(|(span, _)| span.start);
        trivia
    }

    /// Emit standalone trivia items separated as in the source: an item on
    /// the same line as the previous one keeps its spacing, a blank line
    /// before an item is kept, and any other item starts a new line.
    /// `prev_end` is where the source before the first item ends; `None`
    /// leaves the first item's placement to the caller. Also returns where
    /// the last item ends.
    fn trivia_run_doc(
        &self,
        trivia: Vec<(Range<usize>, Doc)>,
        mut prev_end: Option<usize>,
    ) -> (Doc, Option<usize>) {
        let mut parts = Vec::new();
        for (span, item) in trivia {
            if let Some(end) = prev_end {
                let gap = &self.source[end.min(span.start)..span.start];
                match gap.iter().filter(|&&b| b == b'\n').count() {
                    0 => parts.push(Doc::Raw(" ".repeat(gap.len().max(1)))),
                    1 => parts.push(Doc::LineStart(String::new())),
                    _ => {
                        parts.push(Doc::LineStart(String::new()));
                        parts.push(Doc::Hardline);
                    }
                }
            }
            parts.push(item);
            prev_end = Some(span.end);
        }
        (doc::concat(parts), prev_end)
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
            return self.format_off_doc(node, true, true);
        }

        let leading_comments = self.leading_comments_doc(node);
        let leading_directives = self.leading_directives_doc(node);
        let body = self.build_doc(node);
        let trailing = self.trailing_trivia_doc(node);

        doc::concat(vec![leading_comments, leading_directives, body, trailing])
    }

    /// Like `doc_for_node` but omits leading comments and directives.
    ///
    /// Used by alignment decompose functions so that leading comments and
    /// directives can be extracted at the group level instead of being
    /// embedded inside aligned cells, where their line breaks would land
    /// after the row has already started its line.
    pub(crate) fn doc_for_node_sans_leading(&self, node: Node<'a>) -> Doc {
        if self.is_in_format_off_region(node) {
            return self.format_off_doc(node, false, true);
        }

        let body = self.build_doc(node);
        let trailing = self.trailing_trivia_doc(node);

        doc::concat(vec![body, trailing])
    }

    /// Like `doc_for_node` but omits trailing comments, and the trailing
    /// directives after the first of them, which go with the comments.
    ///
    /// Used by alignment decompose functions so that trailing comments
    /// can be extracted as a separate alignment cell instead of being
    /// embedded inside the last data cell.
    pub(crate) fn doc_for_node_sans_trailing(&self, node: Node<'a>) -> Doc {
        if self.is_in_format_off_region(node) {
            return self.format_off_doc(node, true, false);
        }

        let leading_comments = self.leading_comments_doc(node);
        let leading_directives = self.leading_directives_doc(node);
        let body = self.build_doc(node);
        let trailing_directives = self.trailing_directives_before_comments_doc(node);

        doc::concat(vec![
            leading_comments,
            leading_directives,
            body,
            trailing_directives,
        ])
    }

    /// Only the node's own body: no leading or trailing comments or
    /// directives. For the copies of a shared suffix whose trivia belongs to
    /// the last copy alone.
    pub(crate) fn doc_for_node_bare(&self, node: Node<'a>) -> Doc {
        if self.is_in_format_off_region(node) {
            return self.format_off_doc(node, false, false);
        }
        self.build_doc(node)
    }

    pub(crate) fn trivia_suppressed(&self) -> bool {
        self.trivia_suppressed.get()
    }

    /// Run `build` with every comment and directive left out, for a copy of
    /// nodes whose trivia another copy emits.
    pub(crate) fn without_trivia<T>(&self, build: impl FnOnce() -> T) -> T {
        let outer = self.trivia_suppressed.replace(true);
        let result = build();
        self.trivia_suppressed.set(outer);
        result
    }

    /// Emit a node inside a format-off region as its source text.
    ///
    /// Comments and directives are attached to leaves, so the ones leading
    /// the node's first leaf (such as the `{$FMT.OFF}` itself) and trailing
    /// its last leaf lie outside the node's text; descending would have
    /// emitted them, so the slice is widened to cover them. Leading
    /// comments and directives on the node itself are left out when
    /// `with_leading` is false, trailing comments when `with_trailing` is
    /// false, mirroring the `sans_*` variants. Text that starts a line in the source starts one
    /// in the output too, with its source indentation.
    fn format_off_doc(&self, node: Node<'a>, with_leading: bool, with_trailing: bool) -> Doc {
        let first = first_leaf(node);
        let last = last_leaf(node);
        let (leading_comments, leading_directives) = if with_leading || first.id() != node.id() {
            (
                self.comments.leading_comments(first.id()),
                self.directives.leading_directives(first.id()),
            )
        } else {
            (&[][..], &[][..])
        };
        let trailing_comments = if with_trailing || last.id() != node.id() {
            self.comments.trailing_comments(last.id())
        } else {
            &[]
        };
        let start = leading_comments
            .iter()
            .map(|c| c.span.start)
            .chain(leading_directives.iter().map(|d| d.span.start))
            .fold(node.start_byte(), usize::min);
        let end = trailing_comments
            .iter()
            .map(|c| c.span.end)
            .chain(
                self.directives
                    .trailing_directives(last.id())
                    .iter()
                    .map(|d| d.span.end),
            )
            .fold(node.end_byte(), usize::max);

        self.verbatim_doc(start, end)
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
            K::PP_DECL_SECTION => self.build_pp_decl_section(node),
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
            K::ARR_INITIALIZER => self.build_paren_list(node, K::COMMA),
            K::RTTI_ATTRIBUTES => self.build_rtti_attributes(node),
            K::PP_BLOCK => self.build_pp_block(node),
            K::PP_FRAGMENT_EXPR | K::PP_FRAGMENT_STMT => {
                // Render the fragment span verbatim: the external scanner consumed
                // the whole {$ifdef ...}...{$endif} directive pair (plus any
                // trailing identifier-chain continuation) as one opaque token, so
                // we emit the node's original source bytes unchanged. No
                // structural sub-parsing — fmt4d does not interpret the branches.
                let text = self.node_text(node);
                let kind = node.kind();
                let parent_kind = self.parent_kind(node);
                doc::token(text, kind, parent_kind)
            }
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
        let parent_kind = self.parent_kind(node);
        doc::token(text, kind, parent_kind)
    }

    /// Emit a leaf token as verbatim text — used for `literalChar` /
    /// `literalString` where child nodes don't cover the full source span.
    fn build_verbatim_leaf(&self, node: Node<'a>) -> Doc {
        let text = self.node_text(node);
        let kind = node.kind();
        let parent_kind = self.parent_kind(node);
        doc::token(text, kind, parent_kind)
    }

    // ── Recursion helpers ────────────────────────────────────────────

    /// Map all non-extra children through `doc_for_node` and concatenate.
    pub(crate) fn build_children(&self, node: Node<'a>) -> Doc {
        let mut docs: Vec<Doc> = Vec::new();
        let mut prev_had_line_comment = false;
        self.for_each_code_child(node, |child| {
            if prev_had_line_comment {
                // Previous sibling ended with a `//` line comment. The next
                // child must start on a new line — anything else would be
                // swallowed by the comment (e.g. RHS of `x := // ...\n RHS`).
                docs.push(Doc::Hardline);
            }
            docs.push(self.doc_for_node(child));
            prev_had_line_comment = self.has_trailing_line_comment(child);
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
        if comments.is_empty() || self.trivia_suppressed.get() {
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

    /// Trailing comments and directives of `node`, in source order. Empty
    /// while trivia is suppressed.
    pub(crate) fn trailing_items(&self, node: Node<'a>) -> Vec<TrailingItem<'a>> {
        if self.trivia_suppressed.get() {
            return Vec::new();
        }
        let comments: &'a CommentMap = self.comments;
        let directives: &'a DirectiveMap = self.directives;
        let mut items: Vec<TrailingItem<'a>> = comments
            .trailing_comments(node.id())
            .iter()
            .map(|c| TrailingItem {
                span: c.span.clone(),
                text: &c.text,
                gap: c.gap,
                is_comment: true,
            })
            .chain(
                directives
                    .trailing_directives(node.id())
                    .iter()
                    .map(|d| TrailingItem {
                        span: d.span.clone(),
                        text: &d.text,
                        gap: d.gap,
                        is_comment: false,
                    }),
            )
            .collect();
        items.sort_by_key(|item| item.span.start);
        items
    }

    /// Lay out trailing items on the line they follow. The first item is
    /// separated by `first_gap`, or by its source distance from the code
    /// token when `None`; each later item keeps its source distance from
    /// the previous item, so spacing is stable across runs. Anything after
    /// a `//` comment starts a new line, since the comment would swallow it.
    pub(crate) fn trailing_run_doc(
        &self,
        items: &[TrailingItem<'a>],
        first_gap: Option<usize>,
    ) -> Doc {
        let mut parts = Vec::new();
        let mut prev: Option<&TrailingItem<'a>> = None;
        for item in items {
            // Gap and text go in one Raw: the renderer forgets the previous
            // token after a whitespace-ending Raw, and the next token would
            // lose its space.
            let gap = match prev {
                None => first_gap.unwrap_or(item.gap).max(1),
                Some(p) if p.is_line_comment() => {
                    parts.push(Doc::Hardline);
                    0
                }
                Some(p) => {
                    let between = &self.source[p.span.end.min(item.span.start)..item.span.start];
                    if between.contains(&b'\n') {
                        1
                    } else {
                        between.len().max(1)
                    }
                }
            };
            parts.push(Doc::Raw(format!("{}{}", " ".repeat(gap), item.text)));
            prev = Some(item);
        }
        doc::concat(parts)
    }

    /// Trailing comments and directives of `node`, in source order.
    pub(crate) fn trailing_trivia_doc(&self, node: Node<'a>) -> Doc {
        self.trailing_run_doc(&self.trailing_items(node), None)
    }

    /// Build a `Doc` for the trailing comments of `node`.
    pub(crate) fn trailing_comments_doc(&self, node: Node<'a>) -> Doc {
        let mut items = self.trailing_items(node);
        items.retain(|item| item.is_comment);
        self.trailing_run_doc(&items, None)
    }

    /// The trailing directives of `node` that precede its first trailing
    /// comment; the rest are emitted together with the comments.
    pub(crate) fn trailing_directives_before_comments_doc(&self, node: Node<'a>) -> Doc {
        let mut items = self.trailing_items(node);
        let first_comment = items.iter().position(|item| item.is_comment);
        items.truncate(first_comment.unwrap_or(items.len()));
        self.trailing_run_doc(&items, None)
    }

    /// The trailing items of `node` from its first trailing comment on.
    pub(crate) fn trailing_items_from_first_comment(
        &self,
        node: Node<'a>,
    ) -> Vec<TrailingItem<'a>> {
        let mut items = self.trailing_items(node);
        let first_comment = items
            .iter()
            .position(|item| item.is_comment)
            .unwrap_or(items.len());
        items.drain(..first_comment);
        items
    }

    /// Return `true` if `node` has at least one trailing comment and the
    /// LAST of them is a `//` line comment. The next sibling MUST start on
    /// a new line — otherwise the comment swallows it (a `//` runs to end-
    /// of-line). Used by sibling-emitting helpers to insert a Hardline
    /// only where a structural break isn't already provided.
    pub(crate) fn has_trailing_line_comment(&self, node: Node<'a>) -> bool {
        // Comments are attached to leaf nodes (via `CommentMap`), but in
        // binary chains we ask about wrapper nodes like `kAdd` whose only
        // child is the actual `+` leaf. Descend to the last non-extra leaf
        // before consulting the comment map.
        let leaf = last_leaf(node);
        !self.trivia_suppressed.get()
            && self
                .comments
                .trailing_comments(leaf.id())
                .last()
                .is_some_and(|c| c.text.trim_start().starts_with("//"))
    }

    pub(crate) fn leading_directives_doc(&self, node: Node<'a>) -> Doc {
        let directives = self.directives.leading_directives(node.id());
        if directives.is_empty() || self.trivia_suppressed.get() {
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
        let mut items = self.trailing_items(node);
        items.retain(|item| !item.is_comment);
        self.trailing_run_doc(&items, None)
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
        let first_after_start = self
            .blank_line_rows
            .partition_point(|&row| row as usize <= start_row);
        self.blank_line_rows
            .get(first_after_start)
            .is_some_and(|&row| (row as usize) < end_row)
    }

    /// Return `true` if `node` falls entirely within a format-off region.
    pub(crate) fn is_in_format_off_region(&self, node: Node) -> bool {
        let node_start = node.start_position().row + 1; // 1-based
        let node_end = node.end_position().row + 1; // 1-based
        self.format_regions
            .iter()
            .any(|r| node_start >= r.start_line && node_end <= r.end_line)
    }

    /// Return `true` if any immediate child of `node` is a breakable
    /// binary operator (arithmetic, logical, or bitwise). Operator nodes
    /// are identified by kind, regardless of whether tree-sitter-pascal
    /// emits them as leaves or single-child wrappers around the literal
    /// token — the operand subtrees never share these kinds.
    fn has_breakable_operators(&self, node: Node<'a>) -> bool {
        for child in node.children(&mut node.walk()) {
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
        false
    }

    fn build_uses(&self, node: Node<'a>) -> Doc {
        let clause =
            crate::uses::extract_uses_items(node, self.source, self.comments, self.directives);
        let indent_str = " ".repeat(self.config.indent_size);
        let lines = crate::uses::layout_uses_items(
            &clause.items,
            &clause.after,
            &self.config.uses,
            &indent_str,
            self.external_units,
        );

        let mut parts = Vec::new();
        parts.push(Doc::Hardline);
        let keyword = first_leaf(node);
        parts.push(self.uses_leading_trivia_doc(keyword));
        parts.push(doc::token("uses", K::K_USES, ""));
        parts.push(self.trailing_comments_doc(keyword));
        parts.push(Doc::Hardline);
        // Emit the laid-out uses body as Token+Hardline pairs so the renderer
        // can see the contents (line-length budgeting, no blind Doc::Raw
        // escape hatch). Review AH2 (intermediate fix); the full fix would
        // have the layout return Doc directly.
        // The synthetic kind "pp_raw_line" is not matched by spacing.rs, and
        // since each line is followed by a Hardline the renderer is at line
        // start when the next token is emitted, so no spurious spaces leak.
        // A line holding a multi-line comment stays one token, so the
        // renderer protects its inner lines.
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                parts.push(Doc::Hardline);
            }
            if !line.is_empty() {
                parts.push(doc::token(line.clone(), "pp_raw_line", ""));
            }
        }
        if !lines.is_empty() {
            parts.push(Doc::Hardline);
        }
        doc::concat(parts)
    }

    /// Comments and directives leading the `uses` keyword, laid out as in
    /// the source, then a line break. `build_uses` does not descend into
    /// the clause, so nothing else emits them.
    fn uses_leading_trivia_doc(&self, keyword: Node<'a>) -> Doc {
        let trivia = self.trivia_items(
            self.comments.leading_comments(keyword.id()),
            self.directives.leading_directives(keyword.id()),
        );
        let (run, last_end) = self.trivia_run_doc(trivia, None);
        let Some(last_end) = last_end else {
            return Doc::Empty;
        };
        let gap = &self.source[last_end.min(keyword.start_byte())..keyword.start_byte()];
        let line_break = if gap.iter().filter(|&&b| b == b'\n').count() >= 2 {
            Doc::BlankLine
        } else {
            Doc::Hardline
        };
        doc::concat(vec![run, line_break])
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
                    if let Some(prev) = prev_end_row
                        && self.has_blank_line_between(prev, child.start_position().row)
                    {
                        parts.push(Doc::BlankLine);
                    }
                    parts.push(Doc::Hardline);
                    parts.push(doc::token(self.node_text(child), child.kind(), ""));
                    prev_end_row = Some(child.end_position().row);
                }
                _ => {
                    if let Some(prev) = prev_end_row
                        && self.has_blank_line_between(prev, child.start_position().row)
                    {
                        parts.push(Doc::BlankLine);
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

/// Descend through `node`'s leftmost non-extra children until we reach a
/// leaf, where [`CommentMap`] attaches leading comments.
pub(crate) fn first_leaf(node: Node<'_>) -> Node<'_> {
    let mut current = node;
    loop {
        let first_child = current
            .children(&mut current.walk())
            .find(|c| !c.is_extra());
        match first_child {
            Some(child) => current = child,
            None => return current,
        }
    }
}

/// Descend through `node`'s rightmost non-extra children until we reach a
/// leaf (a node with no code children). Used by `has_trailing_line_comment`
/// because tree-sitter-pascal sometimes wraps operator tokens in a
/// single-child node — comments are attached to the inner leaf, not the
/// wrapper, by [`CommentMap`].
pub(crate) fn last_leaf(node: Node<'_>) -> Node<'_> {
    let mut current = node;
    loop {
        let last_child = current
            .children(&mut current.walk())
            .filter(|c| !c.is_extra())
            .last();
        match last_child {
            Some(child) => current = child,
            None => return current,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comments::CommentMap;
    use crate::config::FmtConfig;
    use crate::directive_map::DirectiveMap;

    fn blank_line_between_by_scan(bytes: &[u8], start_row: usize, end_row: usize) -> bool {
        let mut lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
        lines.pop(); // text after the last newline is not a terminated line
        lines.iter().enumerate().any(|(row, line)| {
            row > start_row && row < end_row && line.iter().all(|c| c.is_ascii_whitespace())
        })
    }

    #[test]
    fn blank_line_index_matches_line_scan() {
        let sources: [&[u8]; 5] = [
            b"a\n\nb\n  \t\nc\n",
            b"a\r\n\r\nb\r\n\r\n",
            b"a\nb\n\n\n\nc\n   ",
            b"\n\n\n",
            b"caf\xe9\n\nx\n",
        ];
        for source in sources {
            let (tree, unit_bytes) = parse("unit T;\ninterface\nimplementation\nend.\n");
            let config = FmtConfig::default();
            let comments = CommentMap::build(tree.root_node(), &unit_bytes);
            let directives = DirectiveMap::build(tree.root_node(), &unit_bytes);
            let external_units = HashSet::new();
            let builder = make_builder(source, &config, &comments, &directives, &external_units);
            for start in 0..8 {
                for end in 0..10 {
                    assert_eq!(
                        builder.has_blank_line_between(start, end),
                        blank_line_between_by_scan(source, start, end),
                        "{source:?} rows {start}..{end}"
                    );
                }
            }
        }
    }

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
        // The whole unit falls inside the format-off region, so it should be
        // Raw, followed by the `{$FMT.ON}` after `end.`.
        let Doc::Concat(parts) = &doc else {
            panic!("expected the unit and its end-of-file trivia: {doc:?}");
        };
        assert!(matches!(parts.first(), Some(Doc::Raw(text)) if text.ends_with("end.")));
        assert_eq!(format!("{doc:?}").matches("{$FMT.ON}").count(), 1);
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
