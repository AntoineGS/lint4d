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

        // Name cell: everything before the defaultValue (identifier, optional : type)
        let name_parts: Vec<Doc> = children[..eq_idx]
            .iter()
            .map(|c| self.doc_for_node(*c))
            .collect();

        // Value cell: defaultValue + semicolon (everything from eq_idx onwards)
        let value_parts: Vec<Doc> = children[eq_idx..]
            .iter()
            .map(|c| self.doc_for_node(*c))
            .collect();

        let trailing_comment = self.trailing_comment_cell(node);

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
        let name_parts: Vec<Doc> = children[..colon_idx]
            .iter()
            .map(|c| self.doc_for_node(*c))
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

            let value_parts: Vec<Doc> = children[def_idx..]
                .iter()
                .map(|c| self.doc_for_node(*c))
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
            let type_parts: Vec<Doc> = children[colon_idx..]
                .iter()
                .map(|c| self.doc_for_node(*c))
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
        let has_complex_type = children.iter().any(|c| {
            matches!(
                c.kind(),
                K::DECL_CLASS | K::DECL_RECORD | K::DECL_INTF | K::DECL_ENUM
            )
        });
        if has_complex_type {
            return None;
        }

        // Find the kEq (=) position.
        let eq_idx = children.iter().position(|c| c.kind() == K::K_EQ)?;

        // Name cell: everything before the =
        let name_parts: Vec<Doc> = children[..eq_idx]
            .iter()
            .map(|c| self.doc_for_node(*c))
            .collect();

        // Type cell: = and everything after
        let type_parts: Vec<Doc> = children[eq_idx..]
            .iter()
            .map(|c| self.doc_for_node(*c))
            .collect();

        let trailing_comment = self.trailing_comment_cell(node);

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
        // (includes kProperty, optional kClass, identifier, optional declPropArgs)
        let name_parts: Vec<Doc> = children[..colon_idx]
            .iter()
            .map(|c| self.doc_for_node(*c))
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
                // Read cell: read getter
                let read_parts: Vec<Doc> = children[ri..wi]
                    .iter()
                    .map(|c| self.doc_for_node(*c))
                    .collect();
                // Write cell: write setter + remaining specifiers + ;
                let write_parts: Vec<Doc> = children[wi..]
                    .iter()
                    .map(|c| self.doc_for_node(*c))
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
                // Has read but no write.
                let read_parts: Vec<Doc> = children[ri..]
                    .iter()
                    .map(|c| self.doc_for_node(*c))
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
            let rest_parts: Vec<Doc> = children[first_specifier_idx..]
                .iter()
                .map(|c| self.doc_for_node(*c))
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
    fn trailing_comment_cell(&self, node: Node<'a>) -> Option<AlignCell> {
        if !self.config.alignment.comments {
            return None;
        }

        let comments = self.comments.trailing_comments(node.id());
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
                let leading = self.leading_comments_doc(*child);
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
                    group_items.push(Doc::BlankLine);
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
