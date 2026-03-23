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
pub fn statements_free_variable(statements: Node, source: &[u8], var_name: &str) -> bool {
    let text = node_text(statements, source).to_lowercase();
    let var_lower = var_name.to_lowercase();

    // Check for `variable.Free` / `variable.Free()` / `variable.Destroy` / `variable.Destroy()`
    let dot_free = format!("{}.free", var_lower);
    let dot_destroy = format!("{}.destroy", var_lower);
    if text.contains(&dot_free) || text.contains(&dot_destroy) {
        return true;
    }

    // Check for `FreeAndNil(variable)` — tolerate optional whitespace inside parens
    let free_and_nil = format!("freeandnil({})", var_lower);
    let free_and_nil_sp = format!("freeandnil( {}", var_lower);
    if text.contains(&free_and_nil) || text.contains(&free_and_nil_sp) {
        return true;
    }

    false
}

/// Check whether a source text references a variable name as a standalone word.
pub fn text_references_variable(text: &str, var_name: &str) -> bool {
    let lower = text.to_lowercase();
    let var_lower = var_name.to_lowercase();
    let var_bytes = var_lower.as_bytes();
    let text_bytes = lower.as_bytes();
    let len = var_bytes.len();

    let mut i = 0;
    while i + len <= text_bytes.len() {
        if &text_bytes[i..i + len] == var_bytes {
            let preceded_by_ident =
                i > 0 && (text_bytes[i - 1].is_ascii_alphanumeric() || text_bytes[i - 1] == b'_');
            let followed_by_ident = i + len < text_bytes.len()
                && (text_bytes[i + len].is_ascii_alphanumeric() || text_bytes[i + len] == b'_');

            if !preceded_by_ident && !followed_by_ident {
                return true;
            }
        }
        i += 1;
    }

    false
}

/// Check whether arbitrary text contains a free/destroy call for the variable.
///
/// Similar to `statements_free_variable` but works on `&str` instead of a
/// tree-sitter `Node`.
pub fn text_frees_variable(text: &str, var_name: &str) -> bool {
    let lower = text.to_lowercase();
    let var_lower = var_name.to_lowercase();
    let dot_free = format!("{}.free", var_lower);
    let dot_destroy = format!("{}.destroy", var_lower);
    if lower.contains(&dot_free) || lower.contains(&dot_destroy) {
        return true;
    }
    let free_and_nil = format!("freeandnil({})", var_lower);
    let free_and_nil_sp = format!("freeandnil( {}", var_lower);
    lower.contains(&free_and_nil) || lower.contains(&free_and_nil_sp)
}
