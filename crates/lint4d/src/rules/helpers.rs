use std::collections::HashMap;

use pascal_core::node_kind as K;
use tree_sitter::Node;

/// Extract the UTF-8 text of a node from the source bytes.
pub fn node_text(node: Node, source: &[u8]) -> String {
    std::str::from_utf8(&source[node.start_byte()..node.end_byte()])
        .unwrap_or("")
        .to_string()
}

/// Check whether a node represents a constructor call pattern.
///
/// Matches only `TFoo.Create` (exact match, case-insensitive).
/// Does NOT match factory methods like `TFoo.CreateRunner` or
/// `TFoo.CreateInstance`, which are typically class functions returning
/// interfaces or other managed types.
pub fn is_constructor_call(node: Node, source: &[u8]) -> bool {
    match node.kind() {
        K::EXPR_DOT => {
            let rhs = match node.child_by_field_name("rhs") {
                Some(r) => r,
                None => return false,
            };
            let rhs_text = node_text(rhs, source);
            rhs_text.eq_ignore_ascii_case("create")
        }
        K::EXPR_CALL => {
            let entity = match node.child_by_field_name("entity") {
                Some(e) => e,
                None => return false,
            };
            is_constructor_call(entity, source)
        }
        _ => false,
    }
}

/// Returns `true` when the constructor call has at least one real argument
/// (i.e., the call is `TFoo.Create(something)` and `something` is not empty
/// or `nil`).  A bare `TFoo.Create` or `TFoo.Create()` returns `false`.
pub fn constructor_has_owner_args(rhs: Node, source: &[u8]) -> bool {
    // Only `exprCall` nodes have arguments; a bare `exprDot` has none.
    if rhs.kind() != K::EXPR_CALL {
        return false;
    }

    // The text of the full call, e.g. "TButton.Create(Self)"
    let call_text = node_text(rhs, source);

    // Find the opening parenthesis to extract the argument list.
    if let Some(paren_pos) = call_text.find('(') {
        let after_paren = call_text[paren_pos + 1..].trim();
        // Empty call `Create()` or call with only `)`.
        if after_paren.is_empty() || after_paren == ")" {
            return false;
        }
        // Strip closing paren and whitespace.
        let args = after_paren.trim_end_matches(')').trim();
        // `nil` by itself is not an owner.
        if args.eq_ignore_ascii_case("nil") || args.is_empty() {
            return false;
        }
        return true;
    }

    false
}

/// Check whether a `statements` node contains a free/destroy call for the variable.
///
/// Delegates to `ast_frees_variable` for AST-based detection that ignores
/// matches inside comments and string literals.
pub fn statements_free_variable(statements: Node, source: &[u8], var_name: &str) -> bool {
    ast_frees_variable(statements, source, var_name)
}

/// AST-based check: does any descendant of `node` free the given variable?
///
/// Looks for:
/// - `variable.Free` / `variable.Destroy` (exprDot, optionally wrapped in exprCall)
/// - `FreeAndNil(variable)` (exprCall with identifier entity)
///
/// Unlike the text-based functions, this ignores matches inside comments and
/// string literals (tree-sitter excludes them from the AST).
pub fn ast_frees_variable(node: Node, source: &[u8], var_name: &str) -> bool {
    match node.kind() {
        K::EXPR_CALL => {
            if let Some(entity) = node.child_by_field_name("entity") {
                // Check for FreeAndNil(variable)
                let entity_text = node_text(entity, source);
                if entity_text.eq_ignore_ascii_case("freeandnil") {
                    if let Some(args) = node.child_by_field_name("args") {
                        let mut cursor = args.walk();
                        for child in args.children(&mut cursor) {
                            if child.kind() == K::IDENTIFIER {
                                let arg_text = node_text(child, source);
                                if arg_text.eq_ignore_ascii_case(var_name) {
                                    return true;
                                }
                            }
                        }
                    }
                }
                // Check for variable.Free() / variable.Destroy()
                if is_free_or_destroy_call(entity, source, var_name) {
                    return true;
                }
            }
        }
        K::EXPR_DOT => {
            // Check for variable.Free / variable.Destroy (statement form without parens)
            if is_free_or_destroy_call(node, source, var_name) {
                return true;
            }
        }
        _ => {}
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if ast_frees_variable(child, source, var_name) {
            return true;
        }
    }

    false
}

/// Check if an exprDot node is `variable.Free` or `variable.Destroy`.
fn is_free_or_destroy_call(dot_node: Node, source: &[u8], var_name: &str) -> bool {
    if dot_node.kind() != K::EXPR_DOT {
        return false;
    }
    let lhs = match dot_node.child_by_field_name("lhs") {
        Some(l) => l,
        None => return false,
    };
    let rhs = match dot_node.child_by_field_name("rhs") {
        Some(r) => r,
        None => return false,
    };
    let lhs_text = node_text(lhs, source);
    let rhs_text = node_text(rhs, source);

    lhs_text.eq_ignore_ascii_case(var_name)
        && (rhs_text.eq_ignore_ascii_case("free") || rhs_text.eq_ignore_ascii_case("destroy"))
}

/// AST-based check: does any descendant of `node` reference the given variable?
///
/// Walks the AST for `identifier` nodes matching the variable name (case-insensitive).
/// Tree-sitter excludes comments and string literals from identifier nodes.
pub fn ast_references_variable(node: Node, source: &[u8], var_name: &str) -> bool {
    if node.kind() == K::IDENTIFIER {
        let text = node_text(node, source);
        if text.eq_ignore_ascii_case(var_name) {
            return true;
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if ast_references_variable(child, source, var_name) {
            return true;
        }
    }

    false
}

/// DCU-enhanced owner detection: checks if the constructor's first parameter
/// type descends from TComponent AND the call site passes a non-nil value.
pub fn constructor_is_owner_managed(
    rhs: Node,
    source: &[u8],
    class_name: Option<&str>,
    project: &crate::dcu::ProjectContext,
    uses: &[String],
) -> bool {
    use crate::dcu::TypeRef;

    if rhs.kind() != K::EXPR_CALL {
        return false;
    }

    if !call_has_non_nil_args(rhs, source) {
        return false;
    }

    let Some(cn) = class_name else { return false };
    let Some(ctor) = project.get_constructor(cn, uses) else {
        // Constructor not found in DCU — conservatively treat as owner-managed
        return true;
    };
    let Some(first_param) = ctor.params.first() else {
        // No parameters — not owner-managed
        return false;
    };
    match &first_param.type_ref {
        TypeRef::Resolved(param_type) => {
            // If we resolved the type but can't trace the full ancestry,
            // assume NOT owner-managed: better to flag a potential leak
            // (false positive) than to miss a real one (false negative).
            project
                .descends_from(param_type, "TComponent", uses)
                .unwrap_or(false)
        }
        TypeRef::Unresolved(_) => true, // Can't resolve type at all -> conservative
    }
}

/// Extract the unit name from a parsed AST.
///
/// The tree-sitter-pascal grammar places the unit declaration as a direct child
/// of the root node with kind `"unit"` (or `"program"`/`"library"` for other file
/// types). The name itself is stored in a `moduleName` child of that node.
///
/// Returns `None` if no unit/program/library declaration is found.
pub fn extract_unit_name(root: Node, source: &[u8]) -> Option<String> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            K::UNIT | K::PROGRAM | K::LIBRARY => {
                let mut inner = child.walk();
                for grandchild in child.children(&mut inner) {
                    if grandchild.kind() == K::MODULE_NAME {
                        return Some(node_text(grandchild, source));
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Extract all unit names from `declUses` nodes in the AST.
///
/// Collects unit names from both `interface` and `implementation` uses clauses.
pub fn extract_uses_clauses(root: Node, source: &[u8]) -> Vec<String> {
    let mut units = Vec::new();
    collect_uses_recursive(root, source, &mut units);
    units
}

fn collect_uses_recursive(node: Node, source: &[u8], units: &mut Vec<String>) {
    if node.kind() == K::DECL_USES {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == K::MODULE_NAME {
                units.push(node_text(child, source));
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_uses_recursive(child, source, units);
    }
}

/// AST-based check: does the exprCall have at least one non-nil argument?
fn call_has_non_nil_args(call_node: Node, source: &[u8]) -> bool {
    // Look for the args field on exprCall
    if let Some(args) = call_node.child_by_field_name("args") {
        let mut cursor = args.walk();
        for child in args.children(&mut cursor) {
            // Skip punctuation and whitespace nodes
            match child.kind() {
                K::K_OPEN | K::K_CLOSE | K::K_COMMA | K::OPEN_PAREN | K::CLOSE_PAREN | K::COMMA => {
                    continue
                }
                _ => {}
            }
            let text = node_text(child, source);
            let trimmed = text.trim();
            if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("nil") {
                return true;
            }
        }
    }
    false
}

/// Build a mapping of variable name (lowercase) -> declared type name
/// from the `var` section of a defProc node.
pub fn build_var_type_map(proc_node: Node, source: &[u8]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut proc_cursor = proc_node.walk();
    for child in proc_node.children(&mut proc_cursor) {
        if child.kind() != K::DECL_VARS {
            continue;
        }
        let mut vars_cursor = child.walk();
        for decl_var in child.children(&mut vars_cursor) {
            if decl_var.kind() != K::DECL_VAR {
                continue;
            }
            let type_name = match extract_type_from_decl_var(decl_var, source) {
                Some(t) => t,
                None => continue,
            };
            let mut dv_cursor = decl_var.walk();
            for dv_child in decl_var.children(&mut dv_cursor) {
                if dv_child.kind() == K::IDENTIFIER {
                    let var_name = node_text(dv_child, source);
                    map.insert(var_name.to_lowercase(), type_name.clone());
                }
            }
        }
    }
    map
}

/// Extract the type name from a `declArg` AST node.
///
/// Walks `declArg -> type -> typeref -> identifier` to get the type name.
pub fn extract_type_from_decl_arg(decl_arg: Node, source: &[u8]) -> Option<String> {
    let type_node = decl_arg.child_by_field_name("type")?;
    let mut type_cursor = type_node.walk();
    for type_child in type_node.children(&mut type_cursor) {
        if type_child.kind() == K::TYPEREF {
            let mut ref_cursor = type_child.walk();
            for ref_child in type_child.children(&mut ref_cursor) {
                if ref_child.kind() == K::IDENTIFIER {
                    return Some(node_text(ref_child, source));
                }
            }
        }
    }
    None
}

/// Check if a `declArg` node has an `out` modifier.
pub fn has_out_modifier(decl_arg: Node) -> bool {
    let mut cursor = decl_arg.walk();
    for child in decl_arg.children(&mut cursor) {
        if child.kind() == K::K_OUT {
            return true;
        }
    }
    false
}

/// Return the first `identifier` child that isn't a comment/extra node.
pub fn first_identifier(node: Node) -> Option<Node> {
    let count = node.child_count();
    for i in 0..count {
        let child = node.child(i)?;
        if child.kind() == K::IDENTIFIER && !child.is_extra() {
            return Some(child);
        }
    }
    None
}

/// Collect field names from class declarations in the AST (same file).
///
/// Returns a map from lowercase class name to its field names.
pub fn collect_ast_class_fields(root: Node, source: &[u8]) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    collect_ast_class_fields_recursive(root, source, &mut map);
    map
}

fn collect_ast_class_fields_recursive(
    node: Node,
    source: &[u8],
    out: &mut HashMap<String, Vec<String>>,
) {
    if node.kind() == K::DECL_TYPE {
        if let Some((class_name, fields)) = parse_class_fields(node, source) {
            out.insert(class_name.to_lowercase(), fields);
            return;
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_ast_class_fields_recursive(child, source, out);
    }
}

/// Parse a `declType` node: if it declares a class, return (class_name, field_names).
pub fn parse_class_fields(node: Node, source: &[u8]) -> Option<(String, Vec<String>)> {
    let name_node = match node.child_by_field_name("name") {
        Some(n) => n,
        None => first_identifier(node)?,
    };
    let name = node_text(name_node, source);

    let mut cursor = node.walk();
    let decl_class = node
        .children(&mut cursor)
        .find(|c| c.kind() == K::DECL_CLASS)?;

    let mut fields = Vec::new();
    let mut class_cursor = decl_class.walk();
    for section in decl_class.children(&mut class_cursor) {
        if section.kind() == K::DECL_SECTION {
            let mut section_cursor = section.walk();
            for item in section.children(&mut section_cursor) {
                if item.kind() == K::DECL_FIELD {
                    if let Some(id_node) = first_identifier(item) {
                        fields.push(node_text(id_node, source));
                    }
                }
            }
        }
    }
    Some((name, fields))
}

/// Convert a byte offset to 1-based (line, column).
pub fn byte_offset_to_line_col(source: &[u8], offset: usize) -> (usize, usize) {
    let offset = offset.min(source.len());
    let mut line = 1;
    let mut last_newline = 0;
    for (i, &b) in source[..offset].iter().enumerate() {
        if b == b'\n' {
            line += 1;
            last_newline = i + 1;
        }
    }
    let col = offset - last_newline + 1;
    (line, col)
}

/// Extract the type name from a `declVar` AST node.
///
/// Walks `declVar -> type -> typeref -> identifier` to get the type name.
pub fn extract_type_from_decl_var(decl_var: Node, source: &[u8]) -> Option<String> {
    let mut cursor = decl_var.walk();
    for child in decl_var.children(&mut cursor) {
        if child.kind() == K::TYPE {
            let mut type_cursor = child.walk();
            for type_child in child.children(&mut type_cursor) {
                if type_child.kind() == K::TYPEREF {
                    let mut ref_cursor = type_child.walk();
                    for ref_child in type_child.children(&mut ref_cursor) {
                        if ref_child.kind() == K::IDENTIFIER {
                            return Some(node_text(ref_child, source));
                        }
                    }
                }
            }
        }
    }
    None
}
