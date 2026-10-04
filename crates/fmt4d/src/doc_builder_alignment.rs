use crate::doc::{self, AlignCell, Doc};
use crate::doc_builder::DocBuilder;
use crate::doc_builder_decls::SeparatorTrivia;
use pascal_core::node_kind as K;
use tree_sitter::Node;

/// One alignment row of an expanded `A, B: T;` declaration.
pub(crate) struct VarListRow {
    /// Docs emitted above the row (leading comments and directives).
    pub(crate) leading: Vec<Doc>,
    pub(crate) body: RowBody,
}

/// The content of a [`VarListRow`].
pub(crate) enum RowBody {
    Cells(Vec<AlignCell>),
    /// A declaration laid out without alignment: its identifier ends in a
    /// `//` comment, so its `: T;` starts the next line.
    Plain(Doc),
}

impl<'a> DocBuilder<'a> {
    /// Check if alignment is enabled for the given section kind.
    pub(crate) fn should_align(&self, section_kind: &str) -> bool {
        let cfg = &self.config.alignment;
        if !cfg.enabled {
            return false;
        }
        match section_kind {
            K::DECL_CONSTS => cfg.constants,
            K::DECL_VARS => cfg.variables,
            K::DECL_TYPES => cfg.type_aliases,
            "fields" => cfg.fields,
            "properties" => cfg.properties,
            _ => false,
        }
    }

    /// Build a `Doc` for a slice of nodes, optionally stripping the trailing
    /// comment from the last child so it can be promoted into a separate
    /// alignment cell.
    ///
    /// All `decompose_*` and `expand_*` helpers in this module need to render
    /// a contiguous range of children as a single concatenated `Doc`. When the
    /// declaration has a trailing comment that becomes its own alignment cell,
    /// the comment must be stripped from the last node in the range so it
    /// isn't rendered twice. This helper consolidates that idiom.
    fn concat_range_strip_trailing(&self, range: &[Node<'a>], strip_trailing: bool) -> Doc {
        if range.is_empty() {
            return Doc::Empty;
        }
        let last = range.len() - 1;
        let parts: Vec<Doc> = range
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == last && strip_trailing {
                    self.doc_for_node_sans_trailing(*c)
                } else {
                    self.doc_for_node(*c)
                }
            })
            .collect();
        doc::concat(parts)
    }

    /// Decompose a `declConst` node into alignment cells.
    ///
    /// Structure: `identifier [: type] = <initializer> ;`
    /// Cells: [name] [= value;] [trailing_comment?]
    pub(crate) fn decompose_const(&self, node: Node<'a>) -> Option<Vec<AlignCell>> {
        let children = self.code_children(node);
        if children.is_empty() {
            return None;
        }

        // Find the kEq (=) position — the defaultValue node contains kEq + initializer.
        // But in the AST, declConst children are:
        //   [rttiAttributes?] identifier [: type] defaultValue ;
        // where defaultValue = kEq + _initializer
        let eq_idx = children.iter().position(|c| c.kind() == K::DEFAULT_VALUE)?;

        // Name cell: everything before the defaultValue (identifier, optional : type).
        // Use doc_for_node_sans_leading for the first child so that any
        // leading comments are handled at the group level, not inside cells.
        let name_parts: Vec<Doc> = children[..eq_idx]
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == 0 {
                    self.doc_for_node_sans_leading(*c)
                } else {
                    self.doc_for_node(*c)
                }
            })
            .collect();

        // Value cell: defaultValue + semicolon (everything from eq_idx onwards).
        // Use doc_for_node_sans_trailing for the last child so that the
        // trailing comment is handled as a separate alignment cell.
        let trailing_comment = self.trailing_comment_cell(node);
        let has_tail = trailing_comment.is_some();
        let value_doc = self.concat_range_strip_trailing(&children[eq_idx..], has_tail);

        let mut cells = vec![
            doc::align_cell(doc::concat(name_parts), true),
            doc::align_cell(value_doc, has_tail),
        ];

        if let Some(comment_cell) = trailing_comment {
            cells.push(comment_cell);
        }

        Some(cells)
    }

    /// Decompose a `declVar` or `declField` node into alignment cells.
    ///
    /// Structure: `identifier [, identifier]* : type [= <initializer>] ;`
    /// Cells: [name(s)] [: type] [= value;] [trailing_comment?]
    ///
    /// When there's no initializer, the type cell includes the semicolon.
    pub(crate) fn decompose_var_or_field(&self, node: Node<'a>) -> Option<Vec<AlignCell>> {
        let children = self.code_children(node);
        if children.is_empty() {
            return None;
        }

        // Find the colon position.
        let colon_idx = children.iter().position(|c| c.kind() == K::COLON)?;

        // Bail out of alignment when the name list contains a `// line` comment.
        // An aligned cell renders on a single line; folding a `//` comment into
        // a single line would let it swallow every following name (silent
        // declaration deletion). Returning None makes the caller fall back to
        // the multi-line non-aligned renderer, which forces hardlines. A
        // comment trailing the last name lies outside the list's source
        // span, and would swallow the `:`.
        if name_list_has_line_comment(self.source, &children[..colon_idx])
            || colon_idx
                .checked_sub(1)
                .is_some_and(|i| self.has_trailing_line_comment(children[i]))
        {
            return None;
        }

        // Name cell: everything before the colon.
        // Use doc_for_node_sans_leading for the first child so that any
        // leading comments are handled at the group level, not inside cells.
        let name_parts: Vec<Doc> = children[..colon_idx]
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == 0 {
                    self.doc_for_node_sans_leading(*c)
                } else {
                    self.doc_for_node(*c)
                }
            })
            .collect();

        // Find the defaultValue (= initializer) if present.
        let default_idx = children.iter().position(|c| c.kind() == K::DEFAULT_VALUE);

        let trailing_comment = self.trailing_comment_cell(node);
        let has_tail = trailing_comment.is_some();

        if let Some(def_idx) = default_idx {
            // Has initializer: [name] [: type] [= value;] [comment?]
            let type_parts: Vec<Doc> = children[colon_idx..def_idx]
                .iter()
                .map(|c| self.doc_for_node(*c))
                .collect();

            // Strip trailing comment from last child when extracted separately.
            let value_doc = self.concat_range_strip_trailing(&children[def_idx..], has_tail);

            let mut cells = vec![
                doc::align_cell(doc::concat(name_parts), true),
                doc::align_cell(doc::concat(type_parts), true),
                doc::align_cell(value_doc, has_tail),
            ];

            if let Some(comment_cell) = trailing_comment {
                cells.push(comment_cell);
            }

            Some(cells)
        } else {
            // No initializer: [name] [: type;] [comment?]
            // Strip trailing comment from last child when extracted separately.
            let type_doc = self.concat_range_strip_trailing(&children[colon_idx..], has_tail);

            let mut cells = vec![
                doc::align_cell(doc::concat(name_parts), true),
                doc::align_cell(type_doc, has_tail),
            ];

            if let Some(comment_cell) = trailing_comment {
                cells.push(comment_cell);
            }

            Some(cells)
        }
    }

    /// Expand a multi-identifier `declVar` (e.g. `I, J, K: Integer;`) into
    /// one alignment row per identifier. Returns `None` when the node has
    /// no commas (single-identifier declaration).
    ///
    /// Each row comes with the docs to emit above it: the leading comments
    /// and directives of every identifier but the first, whose trivia the
    /// caller lifts above the whole declaration. The trailing comments of the
    /// declaration stay on the last row; the comments after a comma go with
    /// the row of the identifier before it.
    pub(crate) fn expand_comma_var_rows(&self, node: Node<'a>) -> Option<Vec<VarListRow>> {
        let children = self.code_children(node);

        // Only expand when there are commas.
        if !children.iter().any(|c| c.kind() == K::COMMA) {
            return None;
        }

        let colon_idx = children.iter().position(|c| c.kind() == K::COLON)?;

        // Collect just the IDENTIFIER nodes before the colon.
        let idents: Vec<Node<'a>> = children[..colon_idx]
            .iter()
            .copied()
            .filter(|c| c.kind() == K::IDENTIFIER)
            .collect();
        if idents.is_empty() {
            return None;
        }

        // Build the type cell docs (from colon onward). Every row but the
        // last gets a copy without any of the suffix's comments or
        // directives, so they are emitted once, on the last row.
        let trailing_comment = self.trailing_comment_cell(node);
        let has_tail = trailing_comment.is_some();

        let default_idx = children.iter().position(|c| c.kind() == K::DEFAULT_VALUE);

        let build_docs = || {
            if let Some(def_idx) = default_idx {
                let type_parts: Vec<Doc> = children[colon_idx..def_idx]
                    .iter()
                    .map(|c| self.doc_for_node(*c))
                    .collect();
                let value_doc = self.concat_range_strip_trailing(&children[def_idx..], has_tail);
                (doc::concat(type_parts), Some(value_doc))
            } else {
                let type_doc = self.concat_range_strip_trailing(&children[colon_idx..], has_tail);
                (type_doc, None)
            }
        };
        let (type_doc, value_doc) = build_docs();
        let (bare_type_doc, bare_value_doc) = self.without_trivia(build_docs);

        let before_colon = &children[..colon_idx];
        let mut rows = Vec::with_capacity(idents.len());
        // Trivia after a `//` moved from the previous comma: own lines
        // above this row.
        let mut carried: Vec<Doc> = Vec::new();
        for (i, ident) in idents.iter().enumerate() {
            let is_last = i == idents.len() - 1;
            let (name_doc, trivia) = self.var_list_ident(before_colon, *ident, is_last, true);
            let mut leading = Vec::new();
            if !carried.is_empty() {
                let mut lines = Vec::new();
                for item in carried.drain(..) {
                    lines.push(Doc::Hardline);
                    lines.push(item);
                }
                lines.push(Doc::Hardline);
                leading.push(doc::concat(lines));
            }
            if i > 0 {
                leading.extend(
                    [
                        self.leading_comments_doc(*ident),
                        self.leading_directives_doc(*ident),
                    ]
                    .into_iter()
                    .filter(|d| !matches!(d, Doc::Empty)),
                );
            }
            carried = trivia.rest;
            let mut moved = trivia.inline;

            // The last identifier keeps its `//` comment, which would
            // swallow the `:` of an aligned row.
            if is_last && self.has_trailing_line_comment(*ident) {
                let suffix = self.suffix_doc(&children[colon_idx..]);
                let body = RowBody::Plain(doc::concat(vec![name_doc, Doc::Hardline, suffix]));
                rows.push(VarListRow { leading, body });
                continue;
            }

            let (row_type, row_value) = if is_last {
                (type_doc.clone(), value_doc.clone())
            } else {
                (bare_type_doc.clone(), bare_value_doc.clone())
            };
            // The last row's trailing comment cell comes from the node. An
            // earlier row's moved items are laid out the way the next run
            // reads them back as trailing trivia of its `;`: with comments
            // aligned, the items from the first comment on get a cell of
            // their own and the directives before it follow the data;
            // otherwise they all follow the row.
            let comment_cell = if is_last {
                trailing_comment.clone()
            } else if self.config.alignment.comments && trivia.first_comment < moved.len() {
                let comments = moved.split_off(trivia.first_comment);
                let docs = SeparatorTrivia::docs(&comments);
                Some(doc::align_cell(doc::concat(docs), false))
            } else {
                None
            };
            let inline_moved = if is_last || moved.is_empty() {
                None
            } else {
                Some(doc::concat(SeparatorTrivia::docs(&moved)))
            };
            let has_comment_cell = comment_cell.is_some();

            let mut cells = if let Some(val) = row_value {
                let val = match inline_moved {
                    Some(m) => doc::concat(vec![val, m]),
                    None => val,
                };
                vec![
                    doc::align_cell(name_doc, true),
                    doc::align_cell(row_type, true),
                    doc::align_cell(val, has_comment_cell),
                ]
            } else {
                let row_type = match inline_moved {
                    Some(m) => doc::concat(vec![row_type, m]),
                    None => row_type,
                };
                vec![
                    doc::align_cell(name_doc, true),
                    doc::align_cell(row_type, has_comment_cell),
                ]
            };
            cells.extend(comment_cell);

            rows.push(VarListRow {
                leading,
                body: RowBody::Cells(cells),
            });
        }

        Some(rows)
    }

    /// Detect when the tree-sitter parser has incorrectly merged two `declVar`
    /// entries because the second identifier matches the `kAlias` keyword
    /// (FPC `alias:` proc attribute).
    ///
    /// For example, `LookupSql: RawUtf8;\n  Alias: RawUtf8;` is parsed as a
    /// single `declVar` with a `procAttribute` child.  This method splits it
    /// back into two separate alignment rows.
    ///
    /// Returns `None` when the node has no misparse to fix.
    pub(crate) fn expand_alias_misparse(&self, node: Node<'a>) -> Option<Vec<Vec<AlignCell>>> {
        let children = self.code_children(node);

        // Quick check: does the node contain any procAttribute children?
        let has_proc_attr = children.iter().any(|c| c.kind() == K::PROC_ATTRIBUTE);
        if !has_proc_attr {
            return None;
        }

        // Only handle the case where the procAttribute starts with kAlias.
        let proc_attr_idx = children
            .iter()
            .position(|c| c.kind() == K::PROC_ATTRIBUTE)?;
        let proc_attr = children[proc_attr_idx];
        let attr_children = self.code_children(proc_attr);
        if attr_children.is_empty() || attr_children[0].kind() != "kAlias" {
            return None;
        }

        // Build the main declaration row (up to the first semicolon).
        let colon_idx = children.iter().position(|c| c.kind() == K::COLON)?;
        let first_semi_idx = children.iter().position(|c| c.kind() == K::SEMICOLON)?;

        let name_parts: Vec<Doc> = children[..colon_idx]
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == 0 {
                    self.doc_for_node_sans_leading(*c)
                } else {
                    self.doc_for_node(*c)
                }
            })
            .collect();

        let type_parts: Vec<Doc> = children[colon_idx..=first_semi_idx]
            .iter()
            .map(|c| self.doc_for_node(*c))
            .collect();

        let mut rows = vec![vec![
            doc::align_cell(doc::concat(name_parts), true),
            doc::align_cell(doc::concat(type_parts), false),
        ]];

        // Build a row for the alias misparse.
        // procAttribute children: kAlias(':'), :, identifier (the type name from _expr).
        // Reconstruct as: [Alias] [: TypeName;]
        let alias_text = self.node_text(attr_children[0]);
        let name_doc = doc::token(alias_text, K::IDENTIFIER, K::DECL_VAR);

        let colon_in_attr = attr_children.iter().position(|c| c.kind() == K::COLON);
        if let Some(ci) = colon_in_attr {
            let mut type_cell_parts: Vec<Doc> = attr_children[ci..]
                .iter()
                .map(|c| self.doc_for_node(*c))
                .collect();
            // Add the semicolon that follows the procAttribute.
            if let Some(semi) = children.get(proc_attr_idx + 1)
                && semi.kind() == K::SEMICOLON
            {
                type_cell_parts.push(self.doc_for_node(*semi));
            }
            rows.push(vec![
                doc::align_cell(name_doc, true),
                doc::align_cell(doc::concat(type_cell_parts), false),
            ]);
        }

        Some(rows)
    }

    /// Decompose a simple `declType` (type alias) into alignment cells.
    ///
    /// Structure: `identifier = <type_def> ;`
    /// Cells: [name] [= type;] [trailing_comment?]
    ///
    /// Returns `None` for complex types (class, record, interface, enum, etc.)
    /// that shouldn't participate in alignment.
    pub(crate) fn decompose_type_alias(&self, node: Node<'a>) -> Option<Vec<AlignCell>> {
        let children = self.code_children(node);
        if children.is_empty() {
            return None;
        }

        // Check if this is a simple alias by looking at the type field.
        // Complex types have declClass, declRecord, declIntf, declEnum, etc.
        // Single-line forward declarations (e.g. `EFoo = class(TBar);`) are
        // still alignable — only reject multi-line bodies.
        let has_complex_type = children.iter().any(|c| {
            matches!(
                c.kind(),
                K::DECL_CLASS | K::DECL_RECORD | K::DECL_INTF | K::DECL_ENUM
            )
        });
        if has_complex_type && node.start_position().row != node.end_position().row {
            return None;
        }

        // Find the kEq (=) position.
        let eq_idx = children.iter().position(|c| c.kind() == K::K_EQ)?;

        // Name cell: everything before the =.
        // Use doc_for_node_sans_leading for the first child so that any
        // leading comments are handled at the group level, not inside cells.
        let name_parts: Vec<Doc> = children[..eq_idx]
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == 0 {
                    self.doc_for_node_sans_leading(*c)
                } else {
                    self.doc_for_node(*c)
                }
            })
            .collect();

        // Type cell: = and everything after.
        // Strip trailing comment from last child when extracted separately.
        let trailing_comment = self.trailing_comment_cell(node);
        let has_tail = trailing_comment.is_some();
        let type_doc = self.concat_range_strip_trailing(&children[eq_idx..], has_tail);

        let mut cells = vec![
            doc::align_cell(doc::concat(name_parts), true),
            doc::align_cell(type_doc, has_tail),
        ];

        if let Some(comment_cell) = trailing_comment {
            cells.push(comment_cell);
        }

        Some(cells)
    }

    /// Decompose a `declProp` node into alignment cells.
    ///
    /// Structure: `[class] property name [args] : type [read X] [write X] ... ;`
    /// Cells: [property name] [: type] [read X] [write X ...;] [comment?]
    pub(crate) fn decompose_property(&self, node: Node<'a>) -> Option<Vec<AlignCell>> {
        let children = self.code_children(node);
        if children.is_empty() {
            return None;
        }

        // Find the colon.
        let colon_idx = children.iter().position(|c| c.kind() == K::COLON)?;

        // Find kRead and kWrite positions.
        let read_idx = children.iter().position(|c| c.kind() == K::K_READ);
        let write_idx = children.iter().position(|c| c.kind() == K::K_WRITE);

        // Name cell: everything up to and NOT including the colon
        // (includes kProperty, optional kClass, identifier, optional declPropArgs).
        // Use doc_for_node_sans_leading for the first child so that any
        // leading comments are handled at the group level, not inside cells.
        let name_parts: Vec<Doc> = children[..colon_idx]
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == 0 {
                    self.doc_for_node_sans_leading(*c)
                } else {
                    self.doc_for_node(*c)
                }
            })
            .collect();

        let trailing_comment = self.trailing_comment_cell(node);
        let has_tail = trailing_comment.is_some();

        // Determine the boundary after the type (before read/write/other specifiers).
        // The type ends just before the first specifier keyword (kRead, kWrite,
        // kDefault, kNodefault, kStored, kIndex) or the semicolon.
        let first_specifier_idx = children
            .iter()
            .enumerate()
            .skip(colon_idx + 1)
            .find(|(_, c)| {
                matches!(
                    c.kind(),
                    K::K_READ
                        | K::K_WRITE
                        | K::K_DEFAULT
                        | K::K_NODEFAULT
                        | K::K_STORED
                        | K::K_INDEX
                        | K::SEMICOLON
                )
            })
            .map(|(i, _)| i)
            .unwrap_or(children.len());

        // Type cell: colon + type
        let type_parts: Vec<Doc> = children[colon_idx..first_specifier_idx]
            .iter()
            .map(|c| self.doc_for_node(*c))
            .collect();

        let name_doc = doc::concat(name_parts);
        let type_doc = doc::concat(type_parts);

        // Dispatch to the per-branch builder that assembles the variable
        // middle cells; the surrounding name/type cells and the trailing
        // comment cell are appended uniformly here.
        let middle = match (read_idx, write_idx) {
            (Some(ri), Some(wi)) => self.property_cells_read_write(&children, ri, wi, has_tail),
            (Some(ri), None) => self.property_cells_read_only(&children, ri, has_tail),
            (None, Some(wi)) => self.property_cells_write_only(&children, wi, has_tail),
            (None, None) => {
                // No specifier — fold the rest into the type cell.
                let rest_doc =
                    self.concat_range_strip_trailing(&children[first_specifier_idx..], has_tail);
                return Some(self.finish_property_cells(
                    name_doc,
                    doc::concat(vec![type_doc, rest_doc]),
                    Vec::new(),
                    has_tail,
                    trailing_comment,
                ));
            }
        };

        Some(self.finish_property_cells(name_doc, type_doc, middle, has_tail, trailing_comment))
    }

    /// Assemble the read/write cells when the property has both `read` and
    /// `write` specifiers.
    fn property_cells_read_write(
        &self,
        children: &[Node<'a>],
        ri: usize,
        wi: usize,
        has_tail: bool,
    ) -> Vec<AlignCell> {
        let read_parts: Vec<Doc> = children[ri..wi]
            .iter()
            .map(|c| self.doc_for_node(*c))
            .collect();
        let write_doc = self.concat_range_strip_trailing(&children[wi..], has_tail);
        vec![
            doc::align_cell(doc::concat(read_parts), true),
            doc::align_cell(write_doc, has_tail),
        ]
    }

    /// Assemble the read cell when the property has only `read`.
    fn property_cells_read_only(
        &self,
        children: &[Node<'a>],
        ri: usize,
        has_tail: bool,
    ) -> Vec<AlignCell> {
        let read_doc = self.concat_range_strip_trailing(&children[ri..], has_tail);
        vec![doc::align_cell(read_doc, has_tail)]
    }

    /// Assemble the write cell when the property has only `write`. An empty
    /// read cell is emitted so the write column lines up with properties that
    /// have both `read` and `write`.
    fn property_cells_write_only(
        &self,
        children: &[Node<'a>],
        wi: usize,
        has_tail: bool,
    ) -> Vec<AlignCell> {
        let write_doc = self.concat_range_strip_trailing(&children[wi..], has_tail);
        vec![
            doc::align_cell(Doc::Empty, true),
            doc::align_cell(write_doc, has_tail),
        ]
    }

    /// Build the final `[name] [type] [middle...] [comment?]` cell vector
    /// shared by every `decompose_property` branch.
    fn finish_property_cells(
        &self,
        name_doc: Doc,
        type_doc: Doc,
        middle: Vec<AlignCell>,
        has_tail: bool,
        trailing_comment: Option<AlignCell>,
    ) -> Vec<AlignCell> {
        let type_pad = !middle.is_empty() || has_tail;
        let mut cells = Vec::with_capacity(2 + middle.len() + usize::from(has_tail));
        cells.push(doc::align_cell(name_doc, true));
        cells.push(doc::align_cell(type_doc, type_pad));
        cells.extend(middle);
        if let Some(comment_cell) = trailing_comment {
            cells.push(comment_cell);
        }
        cells
    }

    /// Extract the trailing comments for a node as an AlignCell, if present
    /// and comment alignment is enabled.
    ///
    /// CommentMap associates trailing comments with leaf nodes (e.g.
    /// the `;` token), not with parent declaration nodes.  We check the
    /// declaration node first, then fall back to its last code leaf.
    fn trailing_comment_cell(&self, node: Node<'a>) -> Option<AlignCell> {
        if !self.config.alignment.comments || self.trivia_suppressed() {
            return None;
        }

        // Directives after the first comment stay with it, in source order;
        // `doc_for_node_sans_trailing` leaves them out of the data cell.
        let mut items = self.trailing_items_from_first_comment(node);
        if items.is_empty() {
            // Fall back to last leaf descendant (typically `;`).
            let children = self.code_children(node);
            if let Some(last) = children.last() {
                items = self.trailing_items_from_first_comment(*last);
            }
        }
        if items.is_empty() {
            return None;
        }

        Some(doc::align_cell(
            self.trailing_run_doc(&items, Some(1)),
            false,
        ))
    }

    /// Leading comments and directives of a declaration, emitted above its
    /// aligned row.
    ///
    /// Both maps attach to leaves, so when a comment or directive precedes
    /// a declaration it's typically attached to the first leaf child (e.g.
    /// the identifier), not to the declaration node itself. Check both; the
    /// decomposers render that child with `doc_for_node_sans_leading`.
    fn row_leading_docs(&self, decl: Node<'a>) -> Vec<Doc> {
        let first = self.code_children(decl).first().copied();
        let mut comments = self.leading_comments_doc(decl);
        if let (Doc::Empty, Some(first)) = (&comments, first) {
            comments = self.leading_comments_doc(first);
        }
        let mut directives = self.leading_directives_doc(decl);
        if let (Doc::Empty, Some(first)) = (&directives, first) {
            directives = self.leading_directives_doc(first);
        }
        [comments, directives]
            .into_iter()
            .filter(|d| !matches!(d, Doc::Empty))
            .collect()
    }

    /// Build an alignment group from a list of declaration nodes within
    /// a section (const/var/type block).
    ///
    /// Groups declarations by blank-line boundaries. Non-declaration children
    /// (standalone comments, directives) are included as plain docs.
    pub(crate) fn build_aligned_section(
        &self,
        body_children: &[Node<'a>],
        section_kind: &str,
        prev_end_row: Option<usize>,
    ) -> Doc {
        let mut group_items: Vec<Doc> = Vec::new();
        let mut prev_end = prev_end_row;
        let mut prev_child_kind: &'static str = "";
        let mut prev_single_line = false;

        for child in body_children {
            let kind = child.kind();
            let single_line = child.start_position().row == child.end_position().row;

            // Check for blank line.
            let source_blank = !prev_child_kind.is_empty()
                && prev_end
                    .is_some_and(|pe| self.has_blank_line_between(pe, child.start_position().row));

            let needs_blank = source_blank
                || (section_kind == K::DECL_TYPES
                    && kind == K::DECL_TYPE
                    && prev_child_kind == K::DECL_TYPE
                    && !(prev_single_line && single_line));

            if needs_blank {
                group_items.push(Doc::BlankLine);
            }

            // A format-off declaration is emitted verbatim together with its
            // comments and directives; splitting it into cells would rewrite it.
            if self.is_in_format_off_region(*child) {
                group_items.push(self.doc_for_node(*child));
                prev_child_kind = kind;
                prev_single_line = single_line;
                prev_end = Some(child.end_position().row);
                continue;
            }

            // Expand multi-identifier var declarations (e.g. `I, J, K: Integer;`)
            // into one row per identifier before trying normal decomposition.
            if section_kind == K::DECL_VARS
                && kind == K::DECL_VAR
                && let Some(expanded) = self.expand_comma_var_rows(*child)
            {
                // Leading comments/directives of the declaration go above
                // the first row.
                group_items.extend(self.row_leading_docs(*child));

                let trailing_dir = self.trailing_directives_doc(*child);
                let expanded_len = expanded.len();
                for (i, row) in expanded.into_iter().enumerate() {
                    group_items.extend(row.leading);
                    let is_last = i == expanded_len.saturating_sub(1);
                    let mut cells = match row.body {
                        RowBody::Cells(cells) => cells,
                        RowBody::Plain(plain) => {
                            // Laid out as a non-row item, see below.
                            group_items.push(Doc::LineStart(String::new()));
                            group_items.push(plain);
                            if is_last {
                                group_items.push(trailing_dir.clone());
                            }
                            continue;
                        }
                    };
                    if is_last
                        && !matches!(trailing_dir, Doc::Empty)
                        && let Some(last) = cells.last_mut()
                    {
                        last.content =
                            doc::concat(vec![last.content.clone(), trailing_dir.clone()]);
                    }
                    group_items.push(doc::align_row(cells));
                }

                prev_child_kind = kind;
                prev_single_line = single_line;
                prev_end = Some(child.end_position().row);
                continue;
            }

            // Fix alias keyword misparse: `Alias: T;` parsed as a
            // procAttribute on the preceding declVar.
            if section_kind == K::DECL_VARS
                && kind == K::DECL_VAR
                && let Some(expanded) = self.expand_alias_misparse(*child)
            {
                group_items.extend(self.row_leading_docs(*child));

                for cells in expanded {
                    group_items.push(doc::align_row(cells));
                }

                prev_child_kind = kind;
                prev_single_line = single_line;
                prev_end = Some(child.end_position().row);
                continue;
            }

            // Try to decompose as an aligned row.
            let row = match (section_kind, kind) {
                (K::DECL_CONSTS, K::DECL_CONST) => self.decompose_const(*child),
                (K::DECL_VARS, K::DECL_VAR) => self.decompose_var_or_field(*child),
                (K::DECL_TYPES, K::DECL_TYPE) => self.decompose_type_alias(*child),
                ("fields", K::DECL_FIELD) => self.decompose_var_or_field(*child),
                ("properties", K::DECL_PROP) => self.decompose_property(*child),
                _ => None,
            };

            if let Some(cells) = row {
                // Emit leading comments/directives for this declaration
                // as plain docs (they don't participate in alignment but
                // don't break the group either).
                group_items.extend(self.row_leading_docs(*child));
                // Emit trailing directives after the row content if present.
                let trailing_dir = self.trailing_directives_doc(*child);
                if matches!(trailing_dir, Doc::Empty) {
                    group_items.push(doc::align_row(cells));
                } else {
                    // Append trailing directive to the last cell.
                    let mut cells = cells;
                    if let Some(last) = cells.last_mut() {
                        last.content = doc::concat(vec![last.content.clone(), trailing_dir]);
                    }
                    group_items.push(doc::align_row(cells));
                }
            } else {
                // Non-alignable item (complex type, comment, etc.).
                // Emit as plain doc — this also acts as a group boundary
                // for complex types within a type section.
                let child_doc = self.doc_for_node(*child);
                if section_kind == K::DECL_TYPES && kind == K::DECL_TYPE {
                    // Complex type — break alignment group.
                    // Use Hardline (not BlankLine) for the first item to
                    // avoid inserting a spurious blank line after "type".
                    // Skip the Hardline entirely when child_doc already
                    // starts with one (e.g. from a leading comment).
                    if group_items.is_empty() {
                        if !crate::doc_builder::starts_with_hardline(&child_doc) {
                            group_items.push(Doc::Hardline);
                        }
                    } else {
                        group_items.push(Doc::BlankLine);
                    }
                    group_items.push(child_doc);
                    group_items.push(Doc::BlankLine);
                } else {
                    // Start a line: the previous item may end in a `//`
                    // comment, and nothing else breaks the line between
                    // non-row items. A doc that starts with a hardline (a
                    // leading comment or directive) already does.
                    if !crate::doc_builder::starts_with_hardline(&child_doc) {
                        group_items.push(Doc::LineStart(String::new()));
                    }
                    group_items.push(child_doc);
                }
            }

            prev_child_kind = kind;
            prev_single_line = single_line;
            prev_end = Some(child.end_position().row);
        }

        doc::align_group(group_items)
    }

    /// Build the body of a visibility section (declSection) with alignment
    /// for consecutive fields and consecutive properties.
    ///
    /// Non-field/property items (methods, nested types, etc.) are rendered
    /// normally and break alignment runs.
    pub(crate) fn build_aligned_decl_section_body(
        &self,
        body_children: &[Node<'a>],
        visibility_end_row: Option<usize>,
        align_fields: bool,
        align_props: bool,
    ) -> Doc {
        let mut result_parts: Vec<Doc> = Vec::new();
        let mut prev_end_row = visibility_end_row;

        // Group consecutive fields and consecutive properties.
        let mut i = 0;
        while i < body_children.len() {
            let child = body_children[i];
            let kind = child.kind();

            if align_fields && kind == K::DECL_FIELD {
                // Collect consecutive fields.
                let start = i;
                while i < body_children.len() && body_children[i].kind() == K::DECL_FIELD {
                    i += 1;
                }
                let field_group = &body_children[start..i];
                let aligned = self.build_aligned_section(field_group, "fields", prev_end_row);
                result_parts.push(aligned);
                prev_end_row = Some(body_children[i - 1].end_position().row);
            } else if align_props && kind == K::DECL_PROP {
                // Collect consecutive properties.
                let start = i;
                while i < body_children.len() && body_children[i].kind() == K::DECL_PROP {
                    i += 1;
                }
                let prop_group = &body_children[start..i];
                let aligned = self.build_aligned_section(prop_group, "properties", prev_end_row);
                result_parts.push(aligned);
                prev_end_row = Some(body_children[i - 1].end_position().row);
            } else {
                // Non-alignable item — render normally.
                let child_doc = self.doc_for_node(child);
                if let Some(prev_end) = prev_end_row {
                    if self.has_blank_line_between(prev_end, child.start_position().row) {
                        result_parts.push(Doc::BlankLine);
                    } else if !result_parts.is_empty() {
                        let prev_ends = result_parts
                            .last()
                            .is_some_and(crate::doc_builder::ends_with_hardline);
                        if !prev_ends && !crate::doc_builder::starts_with_hardline(&child_doc) {
                            result_parts.push(Doc::Hardline);
                        }
                    }
                }
                result_parts.push(child_doc);
                prev_end_row = Some(child.end_position().row);
                i += 1;
            }
        }

        doc::concat(result_parts)
    }
}

/// Return `true` if the source bytes spanned by `nodes` contain a `//`
/// line-comment outside string literals. Mirrors the helper in
/// `doc_builder_decls.rs`; a shared crate-private utility would be cleaner
/// but the two callsites have intentionally different parent contexts.
fn name_list_has_line_comment(source: &[u8], nodes: &[Node<'_>]) -> bool {
    if nodes.len() < 2 {
        return false;
    }
    let start = nodes.first().unwrap().start_byte();
    let end = nodes.last().unwrap().end_byte();
    if start >= end || end > source.len() {
        return false;
    }
    let bytes = &source[start..end];
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' {
            if in_string && i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                i += 2;
                continue;
            }
            in_string = !in_string;
        } else if !in_string && b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            return true;
        }
        i += 1;
    }
    false
}
