use crate::doc::{self, AlignCell, Doc};
use crate::doc_builder::DocBuilder;
use pascal_core::node_kind as K;
use tree_sitter::Node;

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
        let value_range = &children[eq_idx..];
        let value_last = value_range.len().saturating_sub(1);
        let value_parts: Vec<Doc> = value_range
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == value_last && trailing_comment.is_some() {
                    self.doc_for_node_sans_trailing(*c)
                } else {
                    self.doc_for_node(*c)
                }
            })
            .collect();

        let mut cells = vec![
            doc::align_cell(doc::concat(name_parts), true),
            doc::align_cell(doc::concat(value_parts), trailing_comment.is_some()),
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
            let value_range = &children[def_idx..];
            let value_last = value_range.len().saturating_sub(1);
            let value_parts: Vec<Doc> = value_range
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    if i == value_last && has_tail {
                        self.doc_for_node_sans_trailing(*c)
                    } else {
                        self.doc_for_node(*c)
                    }
                })
                .collect();

            let mut cells = vec![
                doc::align_cell(doc::concat(name_parts), true),
                doc::align_cell(doc::concat(type_parts), true),
                doc::align_cell(doc::concat(value_parts), has_tail),
            ];

            if let Some(comment_cell) = trailing_comment {
                cells.push(comment_cell);
            }

            Some(cells)
        } else {
            // No initializer: [name] [: type;] [comment?]
            // Strip trailing comment from last child when extracted separately.
            let type_range = &children[colon_idx..];
            let type_last = type_range.len().saturating_sub(1);
            let type_parts: Vec<Doc> = type_range
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    if i == type_last && has_tail {
                        self.doc_for_node_sans_trailing(*c)
                    } else {
                        self.doc_for_node(*c)
                    }
                })
                .collect();

            let mut cells = vec![
                doc::align_cell(doc::concat(name_parts), true),
                doc::align_cell(doc::concat(type_parts), has_tail),
            ];

            if let Some(comment_cell) = trailing_comment {
                cells.push(comment_cell);
            }

            Some(cells)
        }
    }

    /// Expand a multi-identifier `declVar` (e.g. `I, J, K: Integer;`) into
    /// one alignment row per identifier.  Returns `None` when the node has
    /// no commas (single-identifier declaration).
    pub(crate) fn expand_comma_var_rows(&self, node: Node<'a>) -> Option<Vec<Vec<AlignCell>>> {
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

        // Build the type cell docs (from colon onward) — shared by all rows.
        let trailing_comment = self.trailing_comment_cell(node);
        let has_tail = trailing_comment.is_some();

        let default_idx = children.iter().position(|c| c.kind() == K::DEFAULT_VALUE);

        let (type_doc, value_doc) = if let Some(def_idx) = default_idx {
            let type_parts: Vec<Doc> = children[colon_idx..def_idx]
                .iter()
                .map(|c| self.doc_for_node(*c))
                .collect();
            let value_range = &children[def_idx..];
            let value_last = value_range.len().saturating_sub(1);
            let value_parts: Vec<Doc> = value_range
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    if i == value_last && has_tail {
                        self.doc_for_node_sans_trailing(*c)
                    } else {
                        self.doc_for_node(*c)
                    }
                })
                .collect();
            (doc::concat(type_parts), Some(doc::concat(value_parts)))
        } else {
            let type_range = &children[colon_idx..];
            let type_last = type_range.len().saturating_sub(1);
            let type_parts: Vec<Doc> = type_range
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    if i == type_last && has_tail {
                        self.doc_for_node_sans_trailing(*c)
                    } else {
                        self.doc_for_node(*c)
                    }
                })
                .collect();
            (doc::concat(type_parts), None)
        };

        let mut rows = Vec::with_capacity(idents.len());
        for (i, ident) in idents.iter().enumerate() {
            let name_doc = self.doc_for_node_sans_leading(*ident);

            let is_last = i == idents.len() - 1;

            let mut cells = if let Some(ref val) = value_doc {
                vec![
                    doc::align_cell(name_doc, true),
                    doc::align_cell(type_doc.clone(), true),
                    doc::align_cell(val.clone(), is_last && has_tail),
                ]
            } else {
                vec![
                    doc::align_cell(name_doc, true),
                    doc::align_cell(type_doc.clone(), is_last && has_tail),
                ]
            };

            // Trailing comment only on the last row.
            if is_last {
                if let Some(ref comment_cell) = trailing_comment {
                    cells.push(comment_cell.clone());
                }
            }

            rows.push(cells);
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
            if let Some(semi) = children.get(proc_attr_idx + 1) {
                if semi.kind() == K::SEMICOLON {
                    type_cell_parts.push(self.doc_for_node(*semi));
                }
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
        let type_range = &children[eq_idx..];
        let type_last = type_range.len().saturating_sub(1);
        let type_parts: Vec<Doc> = type_range
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == type_last && trailing_comment.is_some() {
                    self.doc_for_node_sans_trailing(*c)
                } else {
                    self.doc_for_node(*c)
                }
            })
            .collect();

        let mut cells = vec![
            doc::align_cell(doc::concat(name_parts), true),
            doc::align_cell(doc::concat(type_parts), trailing_comment.is_some()),
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

        if let Some(ri) = read_idx {
            if let Some(wi) = write_idx {
                // Has both read and write.
                let read_parts: Vec<Doc> = children[ri..wi]
                    .iter()
                    .map(|c| self.doc_for_node(*c))
                    .collect();
                // Write cell: strip trailing comment from last child.
                let write_range = &children[wi..];
                let write_last = write_range.len().saturating_sub(1);
                let write_parts: Vec<Doc> = write_range
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        if i == write_last && has_tail {
                            self.doc_for_node_sans_trailing(*c)
                        } else {
                            self.doc_for_node(*c)
                        }
                    })
                    .collect();

                let mut cells = vec![
                    doc::align_cell(doc::concat(name_parts), true),
                    doc::align_cell(doc::concat(type_parts), true),
                    doc::align_cell(doc::concat(read_parts), true),
                    doc::align_cell(doc::concat(write_parts), has_tail),
                ];

                if let Some(comment_cell) = trailing_comment {
                    cells.push(comment_cell);
                }

                Some(cells)
            } else {
                // Has read but no write — strip trailing comment from last child.
                let read_range = &children[ri..];
                let read_last = read_range.len().saturating_sub(1);
                let read_parts: Vec<Doc> = read_range
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        if i == read_last && has_tail {
                            self.doc_for_node_sans_trailing(*c)
                        } else {
                            self.doc_for_node(*c)
                        }
                    })
                    .collect();

                let mut cells = vec![
                    doc::align_cell(doc::concat(name_parts), true),
                    doc::align_cell(doc::concat(type_parts), true),
                    doc::align_cell(doc::concat(read_parts), has_tail),
                ];

                if let Some(comment_cell) = trailing_comment {
                    cells.push(comment_cell);
                }

                Some(cells)
            }
        } else {
            // No read specifier — just name : type [rest];
            // Strip trailing comment from last child.
            let rest_range = &children[first_specifier_idx..];
            let rest_last = rest_range.len().saturating_sub(1);
            let rest_parts: Vec<Doc> = rest_range
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    if i == rest_last && has_tail {
                        self.doc_for_node_sans_trailing(*c)
                    } else {
                        self.doc_for_node(*c)
                    }
                })
                .collect();

            let mut cells = vec![
                doc::align_cell(doc::concat(name_parts), true),
                doc::align_cell(
                    doc::concat(vec![doc::concat(type_parts), doc::concat(rest_parts)]),
                    has_tail,
                ),
            ];

            if let Some(comment_cell) = trailing_comment {
                cells.push(comment_cell);
            }

            Some(cells)
        }
    }

    /// Extract the trailing comment for a node as an AlignCell, if present
    /// and comment alignment is enabled.
    ///
    /// CommentMap associates trailing comments with leaf nodes (e.g.
    /// the `;` token), not with parent declaration nodes.  We check the
    /// declaration node first, then fall back to its last code leaf.
    fn trailing_comment_cell(&self, node: Node<'a>) -> Option<AlignCell> {
        if !self.config.alignment.comments {
            return None;
        }

        let mut comments = self.comments.trailing_comments(node.id());
        if comments.is_empty() {
            // Fall back to last leaf descendant (typically `;`).
            let children = self.code_children(node);
            if let Some(last) = children.last() {
                comments = self.comments.trailing_comments(last.id());
            }
        }
        if comments.is_empty() {
            return None;
        }

        let docs: Vec<Doc> = comments
            .iter()
            .map(|c| Doc::Raw(format!(" {}", c.text)))
            .collect();

        Some(doc::align_cell(doc::concat(docs), false))
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
        let mut prev_child_kind = String::new();
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

            // Expand multi-identifier var declarations (e.g. `I, J, K: Integer;`)
            // into one row per identifier before trying normal decomposition.
            if section_kind == K::DECL_VARS && kind == K::DECL_VAR {
                if let Some(expanded) = self.expand_comma_var_rows(*child) {
                    // Leading comments/directives only on first row.
                    let mut leading = self.leading_comments_doc(*child);
                    if matches!(leading, Doc::Empty) {
                        let ch = self.code_children(*child);
                        if let Some(first) = ch.first() {
                            leading = self.leading_comments_doc(*first);
                        }
                    }
                    if !matches!(leading, Doc::Empty) {
                        group_items.push(leading);
                    }
                    let leading_dir = self.leading_directives_doc(*child);
                    if !matches!(leading_dir, Doc::Empty) {
                        group_items.push(leading_dir);
                    }

                    let trailing_dir = self.trailing_directives_doc(*child);
                    let expanded_len = expanded.len();
                    for (i, cells) in expanded.into_iter().enumerate() {
                        let is_last = i == expanded_len.saturating_sub(1);
                        if !is_last || matches!(trailing_dir, Doc::Empty) {
                            group_items.push(doc::align_row(cells));
                        } else {
                            let mut cells = cells;
                            if let Some(last) = cells.last_mut() {
                                last.content =
                                    doc::concat(vec![last.content.clone(), trailing_dir.clone()]);
                            }
                            group_items.push(doc::align_row(cells));
                        }
                    }

                    prev_child_kind = kind.to_string();
                    prev_single_line = single_line;
                    prev_end = Some(child.end_position().row);
                    continue;
                }
            }

            // Fix alias keyword misparse: `Alias: T;` parsed as a
            // procAttribute on the preceding declVar.
            if section_kind == K::DECL_VARS && kind == K::DECL_VAR {
                if let Some(expanded) = self.expand_alias_misparse(*child) {
                    let mut leading = self.leading_comments_doc(*child);
                    if matches!(leading, Doc::Empty) {
                        let ch = self.code_children(*child);
                        if let Some(first) = ch.first() {
                            leading = self.leading_comments_doc(*first);
                        }
                    }
                    if !matches!(leading, Doc::Empty) {
                        group_items.push(leading);
                    }
                    let leading_dir = self.leading_directives_doc(*child);
                    if !matches!(leading_dir, Doc::Empty) {
                        group_items.push(leading_dir);
                    }

                    for cells in expanded {
                        group_items.push(doc::align_row(cells));
                    }

                    prev_child_kind = kind.to_string();
                    prev_single_line = single_line;
                    prev_end = Some(child.end_position().row);
                    continue;
                }
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
                //
                // CommentMap associates comments with leaves, so when a
                // comment precedes a declaration it's typically attached
                // to the first leaf child (e.g. the identifier), not to
                // the declaration node itself.  Check both.
                let mut leading = self.leading_comments_doc(*child);
                if matches!(leading, Doc::Empty) {
                    let children = self.code_children(*child);
                    if let Some(first) = children.first() {
                        leading = self.leading_comments_doc(*first);
                    }
                }
                if !matches!(leading, Doc::Empty) {
                    group_items.push(leading);
                }
                let leading_dir = self.leading_directives_doc(*child);
                if !matches!(leading_dir, Doc::Empty) {
                    group_items.push(leading_dir);
                }
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
                    group_items.push(child_doc);
                }
            }

            prev_child_kind = kind.to_string();
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
