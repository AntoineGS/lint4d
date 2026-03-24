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
        "exprDot" => {
            let rhs = match node.child_by_field_name("rhs") {
                Some(r) => r,
                None => return false,
            };
            let rhs_text = node_text(rhs, source);
            rhs_text.eq_ignore_ascii_case("create")
        }
        "exprCall" => {
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
    if rhs.kind() != "exprCall" {
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
        "exprCall" => {
            if let Some(entity) = node.child_by_field_name("entity") {
                // Check for FreeAndNil(variable)
                let entity_text = node_text(entity, source);
                if entity_text.eq_ignore_ascii_case("freeandnil") {
                    if let Some(args) = node.child_by_field_name("args") {
                        let mut cursor = args.walk();
                        for child in args.children(&mut cursor) {
                            if child.kind() == "identifier" {
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
        "exprDot" => {
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
    if dot_node.kind() != "exprDot" {
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
    if node.kind() == "identifier" {
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

    if rhs.kind() != "exprCall" {
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

/// AST-based check: does the exprCall have at least one non-nil argument?
fn call_has_non_nil_args(call_node: Node, source: &[u8]) -> bool {
    // Look for the args field on exprCall
    if let Some(args) = call_node.child_by_field_name("args") {
        let mut cursor = args.walk();
        for child in args.children(&mut cursor) {
            // Skip punctuation and whitespace nodes
            match child.kind() {
                "kOpen" | "kClose" | "kComma" | "(" | ")" | "," => continue,
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

