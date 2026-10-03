//! Declaration builders — `var` / `const` / `type` sections, class member sections,
//! procedure declarations and definitions, and comma-separated identifier decls.
//! Extracted from doc_builder.rs during Phase F decomposition (review AH1 / BP-M5).

use crate::doc::{self, Doc};
use crate::doc_builder::{
    DocBuilder, ends_with_hardline, starts_with_hardline, strip_trailing_hardline,
};
use pascal_core::node_kind as K;
use tree_sitter::Node;

impl<'a> DocBuilder<'a> {
    pub(crate) fn build_section(&self, node: Node<'a>) -> Doc {
        let section_kind = node.kind();
        let use_alignment = self.should_align(section_kind);

        let mut parts = Vec::new();
        let mut body_children: Vec<Node<'a>> = Vec::new();
        let mut keyword_end_row: Option<usize> = None;
        // The grammar parses `class var` / `class const` as
        //   declVars: optional(kClass), choice(kVar, kThreadvar), repeat(declVar)
        // so the kClass token is a sibling of kVar. Treat it as a keyword
        // prefix, not as a body declaration — otherwise it renders on its
        // own line above the field names, producing invalid Pascal.
        let mut pending_class_prefix: Option<Node<'a>> = None;

        self.for_each_code_child(node, |child| match child.kind() {
            K::K_CLASS => {
                pending_class_prefix = Some(child);
            }
            K::K_VAR | K::K_CONST | K::K_TYPE => {
                parts.push(Doc::Hardline);
                if let Some(class_node) = pending_class_prefix.take() {
                    parts.push(self.doc_for_node(class_node));
                    parts.push(Doc::Raw(" ".into()));
                }
                parts.push(self.doc_for_node(child));
                keyword_end_row = Some(child.end_position().row);
            }
            _ => {
                body_children.push(child);
            }
        });

        if body_children.is_empty() {
            return doc::concat(parts);
        }

        if use_alignment {
            let aligned = self.build_aligned_section(&body_children, section_kind, keyword_end_row);
            parts.push(doc::indent(aligned));
        } else {
            let mut body_parts = Vec::new();
            let mut prev_child_kind: &'static str = "";
            let mut prev_single_line = false;
            let mut prev_end_row: Option<usize> = keyword_end_row;

            for child in &body_children {
                let kind = child.kind();
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

    pub(crate) fn build_decl_section(&self, node: Node<'a>) -> Doc {
        let align_fields = self.should_align("fields");
        let align_props = self.should_align("properties");
        let mut parts = Vec::new();
        let mut body_children: Vec<Node<'a>> = Vec::new();
        let mut first = true;
        let mut after_strict = false;
        let mut visibility_end_row: Option<usize> = None;

        self.for_each_code_child(node, |child| match child.kind() {
            K::K_PUBLIC | K::K_PRIVATE | K::K_PROTECTED | K::K_PUBLISHED | K::K_STRICT => {
                let is_strict = child.kind() == K::K_STRICT;

                if after_strict {
                    parts.push(Doc::Raw(" ".into()));
                    parts.push(Doc::Raw(self.node_text(child)));
                    parts.push(Doc::Hardline);
                    after_strict = false;
                } else {
                    if first {
                        parts.push(Doc::Hardline);
                        parts.push(self.doc_for_node(child));
                        first = false;
                    } else {
                        parts.push(Doc::Raw(" ".into()));
                        parts.push(Doc::Raw(self.node_text(child)));
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
                body_children.push(child);
            }
        });

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

    /// Build a `ppDeclSection` node — a visibility specifier wrapped in a
    /// preprocessor directive pair, e.g.:
    ///
    /// ```pascal
    /// {$IFDEF UNITTEST}
    /// published
    /// {$ENDIF}
    ///   property Bar: integer read FBar;
    /// ```
    ///
    /// The ppIf/ppEndIf tokens are emitted verbatim at the visibility level,
    /// and the body (fields, properties, etc.) is indented beneath.
    pub(crate) fn build_pp_decl_section(&self, node: Node<'a>) -> Doc {
        let align_fields = self.should_align("fields");
        let align_props = self.should_align("properties");
        let mut parts = Vec::new();
        let mut body_children: Vec<Node<'a>> = Vec::new();
        let mut visibility_end_row: Option<usize> = None;
        let mut after_strict = false;

        self.for_each_code_child(node, |child| match child.kind() {
            K::PP_IF => {
                parts.push(Doc::Hardline);
                parts.push(Doc::Raw(self.node_text(child)));
            }
            K::K_STRICT => {
                parts.push(Doc::Hardline);
                parts.push(self.doc_for_node(child));
                after_strict = true;
                visibility_end_row = Some(child.end_position().row);
            }
            K::K_PUBLIC | K::K_PRIVATE | K::K_PROTECTED | K::K_PUBLISHED => {
                if after_strict {
                    parts.push(Doc::Raw(" ".into()));
                    parts.push(Doc::Raw(self.node_text(child)));
                    after_strict = false;
                } else {
                    parts.push(Doc::Hardline);
                    parts.push(self.doc_for_node(child));
                }
                visibility_end_row = Some(child.end_position().row);
            }
            K::PP_END_IF => {
                parts.push(Doc::Hardline);
                parts.push(Doc::Raw(self.node_text(child)));
            }
            _ => {
                body_children.push(child);
            }
        });

        if body_children.is_empty() {
            return doc::concat(parts);
        }

        if align_fields || align_props {
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

    pub(crate) fn build_def_proc(&self, node: Node<'a>) -> Doc {
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

    pub(crate) fn build_decl_proc(&self, node: Node<'a>) -> Doc {
        let children = self.code_children(node);
        let mut parts = Vec::new();
        let mut i = 0;
        let mut prev_had_line_comment = false;
        while i < children.len() {
            let child = children[i];
            if prev_had_line_comment {
                parts.push(Doc::Hardline);
            }
            match child.kind() {
                K::SEMICOLON => {
                    let next = children.get(i + 1);
                    let has_trailing_line_comment = self.has_trailing_line_comment(child);
                    parts.push(self.doc_for_node(child));
                    if next.is_some_and(|n| matches!(n.kind(), K::PROC_ATTRIBUTE | K::K_FORWARD))
                        && !has_trailing_line_comment
                    {
                        parts.push(Doc::Raw(" ".into()));
                    } else if next.is_none() || !has_trailing_line_comment {
                        parts.push(Doc::Hardline);
                    }
                }
                _ => {
                    parts.push(self.doc_for_node(child));
                }
            }
            prev_had_line_comment = self.has_trailing_line_comment(child);
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
    pub(crate) fn build_comma_ident_decl(&self, node: Node<'a>) -> Doc {
        let children = self.code_children(node);

        // No commas → no internal break needed.
        if !children.iter().any(|c| c.kind() == K::COMMA) {
            // Fix alias keyword misparse: if a declVar has a procAttribute
            // child whose first child is kAlias, the parser incorrectly
            // merged two declarations.  Split them onto separate lines.
            if node.kind() == K::DECL_VAR
                && let Some(doc) = self.build_alias_misparse_split(node, &children)
            {
                return doc;
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
                let suffix = self.suffix_doc(from_colon, true);
                let bare_suffix = self.suffix_doc(from_colon, false);

                let mut parts = Vec::new();
                for (i, ident) in idents.iter().enumerate() {
                    if i > 0 {
                        parts.push(Doc::Hardline);
                    }
                    let is_last = i == idents.len() - 1;
                    let (ident_doc, moved) =
                        self.var_list_ident(before_colon, *ident, is_last, false);
                    parts.push(ident_doc);
                    parts.push(if is_last {
                        suffix.clone()
                    } else {
                        bare_suffix.clone()
                    });
                    parts.extend(moved);
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
        let commas: Vec<Node<'a>> = ident_list
            .iter()
            .copied()
            .filter(|c| c.kind() == K::COMMA)
            .collect();
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
                // Comments after a comma stay after it; a `//` comment
                // forces the next identifier onto a new line.
                let comma = commas.get(i - 1).copied();
                let comments = comma.map(|c| self.comma_trivia(c)).unwrap_or_default();
                let ends_line = comma.is_some_and(|c| self.has_trailing_line_comment(c));
                ident_parts.extend(comments);
                ident_parts.push(if ends_line { Doc::Hardline } else { Doc::Line });
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

    /// The `: Type;` part of a `declVar` (children from the colon on). With
    /// `with_trivia` false the last child loses its trailing comments and
    /// directives, for the declarations that precede the last one.
    fn suffix_doc(&self, from_colon: &[Node<'a>], with_trivia: bool) -> Doc {
        let last = from_colon.len().saturating_sub(1);
        let docs: Vec<Doc> = from_colon
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if with_trivia || i != last {
                    self.doc_for_node(*c)
                } else {
                    doc::concat(vec![
                        self.leading_comments_doc(*c),
                        self.leading_directives_doc(*c),
                        self.doc_for_node_bare(*c),
                    ])
                }
            })
            .collect();
        doc::concat(docs)
    }

    /// Comments and directives after a comma of an identifier list, as
    /// same-line docs.
    pub(crate) fn comma_trivia(&self, comma: Node<'a>) -> Vec<Doc> {
        let leading = self.comments.leading_comments(comma.id());
        let trailing = self.comments.trailing_comments(comma.id());
        let directives = self.directives.trailing_directives(comma.id());
        leading
            .iter()
            .map(|c| (c.span.start, 1, c.text.as_str()))
            .chain(
                trailing
                    .iter()
                    .map(|c| (c.span.start, c.gap, c.text.as_str())),
            )
            .chain(
                directives
                    .iter()
                    .map(|d| (d.span.start, d.gap, d.text.as_str())),
            )
            .map(|(_, gap, text)| Doc::Raw(format!("{}{text}", " ".repeat(gap.max(1)))))
            .collect()
    }

    /// An identifier of an expanded `A, B: T;` list, and the same-line docs
    /// to put after its declaration's `;`: the comments after the following
    /// comma and, when its own trailing comments end in a `//` comment that
    /// would swallow the `:`, those too. `sans_leading` leaves the
    /// identifier's leading trivia to the caller.
    pub(crate) fn var_list_ident(
        &self,
        before_colon: &[Node<'a>],
        ident: Node<'a>,
        is_last: bool,
        sans_leading: bool,
    ) -> (Doc, Vec<Doc>) {
        let comma = before_colon
            .iter()
            .position(|c| c.id() == ident.id())
            .and_then(|i| before_colon.get(i + 1))
            .filter(|c| c.kind() == K::COMMA);
        let move_own = !is_last && self.has_trailing_line_comment(ident);
        let ident_doc = match (sans_leading, move_own) {
            (false, false) => self.doc_for_node(ident),
            (false, true) => self.doc_for_node_sans_trailing(ident),
            (true, false) => self.doc_for_node_sans_leading(ident),
            (true, true) => doc::concat(vec![
                self.doc_for_node_bare(ident),
                self.trailing_directives_doc(ident),
            ]),
        };
        let mut moved = Vec::new();
        if move_own {
            let comments = self.comments.trailing_comments(ident.id());
            moved.extend(
                comments
                    .iter()
                    .map(|c| Doc::Raw(format!("{}{}", " ".repeat(c.gap.max(1)), c.text))),
            );
        }
        if let Some(comma) = comma {
            moved.extend(self.comma_trivia(*comma));
        }
        (ident_doc, moved)
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
            if let Some(semi) = children.get(proc_attr_idx + 1)
                && semi.kind() == K::SEMICOLON
            {
                alias_parts.push(self.doc_for_node(*semi));
            }
            parts.push(Doc::Hardline);
            parts.push(doc::concat(alias_parts));
        }

        Some(doc::concat(parts))
    }
}
