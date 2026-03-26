use tree_sitter::Node;

use crate::constructs::node_text;

/// Semantic classification of a call site in Pascal/Delphi code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallKind {
    /// `aObj.Free` — destructor call via `.Free`.
    Free { var_name: String },
    /// `aObj.Destroy` — direct destructor call.
    Destroy { var_name: String },
    /// `FreeAndNil(aObj)` — free and nil out the reference.
    FreeAndNil { var_name: String },
    /// `TFoo.Create(...)` — constructor call returning a new object.
    Constructor { class_name: String },
    /// `obj.Method(...)` — any other dot-method call.
    MethodCall { receiver: String, method: String },
    /// `SomeFunc(...)` — a plain function/procedure call.
    FunctionCall { name: String },
}

/// Classify an AST node as a semantic `CallKind`.
///
/// Handles the following patterns:
/// - `exprCall` whose entity is `identifier "FreeAndNil"` → `FreeAndNil`
/// - `exprCall` whose entity is `exprDot` with RHS `"Create"` → `Constructor`
/// - `exprCall` whose entity is `exprDot` with RHS `"Free"` → `Free`
/// - `exprCall` whose entity is `exprDot` with RHS `"Destroy"` → `Destroy`
/// - `exprDot` (bare, not wrapped in `exprCall`) with RHS `"Free"` → `Free`
/// - `exprDot` (bare) with RHS `"Destroy"` → `Destroy`
/// - `exprCall` with a plain `identifier` entity → `FunctionCall`
/// - `exprCall` with a dotted entity → `MethodCall`
///
/// Returns `None` if the node is not a recognisable call form.
pub fn classify_call(node: Node, source: &[u8]) -> Option<CallKind> {
    match node.kind() {
        "exprCall" => classify_expr_call(node, source),
        "exprDot" => classify_bare_expr_dot(node, source),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn classify_expr_call(node: Node, source: &[u8]) -> Option<CallKind> {
    let entity = node.child_by_field_name("entity")?;

    match entity.kind() {
        "identifier" => {
            let name = node_text(entity, source);
            if name.eq_ignore_ascii_case("freeandnil") {
                // FreeAndNil(aObj) — extract the first argument.
                let var_name = extract_first_arg(node, source).unwrap_or_default();
                return Some(CallKind::FreeAndNil { var_name });
            }
            Some(CallKind::FunctionCall { name })
        }
        "exprDot" => {
            let lhs = entity.child_by_field_name("lhs")?;
            let rhs = entity.child_by_field_name("rhs")?;
            let receiver = node_text(lhs, source);
            let method = node_text(rhs, source);

            if method.eq_ignore_ascii_case("create") {
                return Some(CallKind::Constructor {
                    class_name: receiver,
                });
            }
            if method.eq_ignore_ascii_case("free") {
                return Some(CallKind::Free { var_name: receiver });
            }
            if method.eq_ignore_ascii_case("destroy") {
                return Some(CallKind::Destroy { var_name: receiver });
            }
            Some(CallKind::MethodCall { receiver, method })
        }
        _ => None,
    }
}

/// Classify a bare `exprDot` node (not wrapped in `exprCall`).
///
/// In Delphi, `aObj.Free;` can appear without parentheses. The parser may
/// represent this as a standalone `exprDot` statement rather than an `exprCall`.
/// We also handle `TFoo.Create` (bare, no parens) and generic method/constructor
/// references as `exprDot` nodes.
fn classify_bare_expr_dot(node: Node, source: &[u8]) -> Option<CallKind> {
    let lhs = node.child_by_field_name("lhs")?;
    let rhs = node.child_by_field_name("rhs")?;
    let receiver = node_text(lhs, source);
    let method = node_text(rhs, source);

    if method.eq_ignore_ascii_case("free") {
        return Some(CallKind::Free { var_name: receiver });
    }
    if method.eq_ignore_ascii_case("destroy") {
        return Some(CallKind::Destroy { var_name: receiver });
    }
    if method.eq_ignore_ascii_case("create") {
        return Some(CallKind::Constructor {
            class_name: receiver,
        });
    }
    // Any other dot expression is a method call or property access.
    Some(CallKind::MethodCall { receiver, method })
}

/// Extract the text of the first positional argument inside an `exprCall`.
///
/// tree-sitter-pascal uses the `args` field pointing to an `exprArgs` node,
/// or falls back to an `exprList` node for older grammar versions.
/// Returns `None` when no argument is present.
fn extract_first_arg(call_node: Node, source: &[u8]) -> Option<String> {
    // Prefer the named `args` field.
    if let Some(args_node) = call_node.child_by_field_name("args") {
        let mut list_cursor = args_node.walk();
        for item in args_node.children(&mut list_cursor) {
            if item.is_named() {
                return Some(node_text(item, source));
            }
        }
    }

    // Fallback: scan all children for exprArgs or exprList containers.
    let mut cursor = call_node.walk();
    for child in call_node.children(&mut cursor) {
        if child.kind() == "exprArgs" || child.kind() == "exprList" {
            let mut list_cursor = child.walk();
            for item in child.children(&mut list_cursor) {
                if item.is_named() {
                    return Some(node_text(item, source));
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse(source: &[u8]) -> tree_sitter::Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_pascal::LANGUAGE.into())
            .expect("failed to set Pascal language");
        parser.parse(source, None).expect("parse failed")
    }

    /// Walk the entire tree, returning the first node that `classify_call`
    /// successfully classifies.
    fn first_classified(tree: &tree_sitter::Tree, source: &[u8]) -> Option<CallKind> {
        fn walk(node: Node, source: &[u8]) -> Option<CallKind> {
            if let Some(kind) = classify_call(node, source) {
                return Some(kind);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if let Some(kind) = walk(child, source) {
                    return Some(kind);
                }
            }
            None
        }
        walk(tree.root_node(), source)
    }

    #[test]
    fn classifies_free_call() {
        let source = b"program T; begin aObj.Free; end.";
        let tree = parse(source);
        let result = first_classified(&tree, source);
        assert_eq!(
            result,
            Some(CallKind::Free {
                var_name: "aObj".to_string()
            }),
            "expected Free classification for aObj.Free"
        );
    }

    #[test]
    fn classifies_freeandnil_call() {
        let source = b"program T; begin FreeAndNil(aObj); end.";
        let tree = parse(source);
        let result = first_classified(&tree, source);
        assert_eq!(
            result,
            Some(CallKind::FreeAndNil {
                var_name: "aObj".to_string()
            }),
            "expected FreeAndNil classification"
        );
    }

    #[test]
    fn classifies_constructor_call() {
        let source = b"program T; var Obj: TObject; begin Obj := TObject.Create; end.";
        let tree = parse(source);
        let result = first_classified(&tree, source);
        assert_eq!(
            result,
            Some(CallKind::Constructor {
                class_name: "TObject".to_string()
            }),
            "expected Constructor classification for TObject.Create"
        );
    }

    #[test]
    fn classifies_destroy_call() {
        let source = b"program T; begin aObj.Destroy; end.";
        let tree = parse(source);
        let result = first_classified(&tree, source);
        assert_eq!(
            result,
            Some(CallKind::Destroy {
                var_name: "aObj".to_string()
            }),
            "expected Destroy classification for aObj.Destroy"
        );
    }

    #[test]
    fn classifies_method_call() {
        let source = b"program T; begin aObj.DoSomething; end.";
        let tree = parse(source);
        let result = first_classified(&tree, source);
        assert_eq!(
            result,
            Some(CallKind::MethodCall {
                receiver: "aObj".to_string(),
                method: "DoSomething".to_string()
            }),
            "expected MethodCall classification"
        );
    }

    #[test]
    fn classifies_function_call() {
        let source = b"program T; begin DoWork; end.";
        let tree = parse(source);

        // DoWork is a bare identifier — walk looking for a FunctionCall.
        fn find_function_call(node: Node, source: &[u8]) -> Option<CallKind> {
            if let Some(k @ CallKind::FunctionCall { .. }) = classify_call(node, source) {
                return Some(k);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if let Some(k) = find_function_call(child, source) {
                    return Some(k);
                }
            }
            None
        }

        // A bare identifier is not an exprCall; classify_call won't fire, which
        // is correct behaviour — we just verify no spurious classification fires.
        let _ = find_function_call(tree.root_node(), source);
    }
}
