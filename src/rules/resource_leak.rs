use tree_sitter::{Node, Tree};

use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::rules::{LintContext, Rule, RuleCategory, RuleMeta};

pub struct ResourceLeakUnprotectedRule {
    meta: RuleMeta,
}

impl ResourceLeakUnprotectedRule {
    pub fn new() -> Self {
        ResourceLeakUnprotectedRule {
            meta: RuleMeta {
                id: "resource-leak-unprotected",
                name: "Resource Leak: Unprotected",
                category: RuleCategory::ResourceManagement,
                default_severity: Severity::Error,
                description: "Detects resources created without try..finally protection.",
            },
        }
    }
}

impl Rule for ResourceLeakUnprotectedRule {
    fn meta(&self) -> &RuleMeta {
        &self.meta
    }

    fn check(&self, _file: &FileInfo, tree: &Tree, source: &[u8], ctx: &mut LintContext) {
        visit_blocks(tree.root_node(), source, ctx);
    }
}

/// Recursively walk the AST looking for block-like nodes that contain
/// sequential statements (e.g., `begin..end` blocks).
fn visit_blocks(node: Node, source: &[u8], ctx: &mut LintContext) {
    if node.kind() == "block" {
        check_block_for_leaks(node, source, ctx);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_blocks(child, source, ctx);
    }
}

/// Check a `block` node for the pattern:
///   assignment (constructor)  ->  statement(s)  ->  try..finally
///
/// If we find statements between the constructor assignment and the `try`
/// node whose `finally` clause frees the same variable, flag them.
fn check_block_for_leaks(block: Node, source: &[u8], ctx: &mut LintContext) {
    let children: Vec<Node> = block.children(&mut block.walk()).collect();

    for (i, child) in children.iter().enumerate() {
        // Step 1: Is this an assignment whose RHS is a constructor call?
        if child.kind() != "assignment" {
            continue;
        }

        let var_name = match extract_constructor_assignment(*child, source) {
            Some(name) => name,
            None => continue,
        };

        // Step 2: Look ahead for a `try` node whose `finally` frees this variable.
        let mut try_index = None;
        for j in (i + 1)..children.len() {
            if children[j].kind() == "try" && finally_frees_variable(children[j], source, &var_name)
            {
                try_index = Some(j);
                break;
            }
        }

        let try_idx = match try_index {
            Some(idx) => idx,
            None => continue,
        };

        // Step 3: Flag any statements between the assignment and the try
        // that reference the variable.
        for j in (i + 1)..try_idx {
            let stmt = children[j];
            let stmt_text = node_text(stmt, source);
            if text_references_variable(&stmt_text, &var_name) {
                let start = stmt.start_position();
                let end = stmt.end_position();
                ctx.report(Diagnostic {
                    rule_id: "resource-leak-unprotected".to_string(),
                    severity: Severity::Error,
                    message: format!(
                        "Statement uses '{}' between constructor and try..finally block. \
                         If this line raises an exception, the object will leak.",
                        var_name
                    ),
                    line: start.row + 1,
                    column: start.column + 1,
                    end_line: end.row + 1,
                    end_column: end.column + 1,
                    help: Some(
                        "Move this statement inside the try block, immediately after \
                         the constructor assignment."
                            .to_string(),
                    ),
                });
            }
        }
    }
}

/// If `node` is an assignment whose RHS looks like a constructor call
/// (`TFoo.Create` or `TFoo.Create(...)`), return the LHS variable name.
fn extract_constructor_assignment<'a>(node: Node<'a>, source: &'a [u8]) -> Option<String> {
    // node.kind() == "assignment"
    // Fields: lhs (identifier), operator (kAssign), rhs (exprDot or exprCall)
    let lhs = node.child_by_field_name("lhs")?;
    let rhs = node.child_by_field_name("rhs")?;

    if !is_constructor_call(rhs, source) {
        return None;
    }

    Some(node_text(lhs, source))
}

/// Check whether a node represents a constructor call pattern.
///
/// Matches:
/// - `exprDot` where rhs text starts with "Create" (e.g., `TFoo.Create`)
/// - `exprCall` whose entity is an `exprDot` matching the above
fn is_constructor_call(node: Node, source: &[u8]) -> bool {
    match node.kind() {
        "exprDot" => {
            let rhs = match node.child_by_field_name("rhs") {
                Some(r) => r,
                None => return false,
            };
            let rhs_text = node_text(rhs, source);
            rhs_text.eq_ignore_ascii_case("create")
                || rhs_text
                    .to_lowercase()
                    .starts_with("create")
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

/// Check whether the `finally` clause of a `try` node frees the given variable.
///
/// Looks for patterns like `variable.Free` or `FreeAndNil(variable)` in the
/// finally block's source text.
fn finally_frees_variable(try_node: Node, source: &[u8], var_name: &str) -> bool {
    let finally_children: Vec<Node> = try_node
        .children_by_field_name("finally", &mut try_node.walk())
        .collect();

    // Find the statements node(s) in the finally field (skip the kFinally keyword).
    for child in &finally_children {
        if child.kind() == "statements" {
            let text = node_text(*child, source).to_lowercase();
            let var_lower = var_name.to_lowercase();

            // Check for `variable.Free` or `variable.Destroy`
            let dot_free = format!("{}.free", var_lower);
            let dot_destroy = format!("{}.destroy", var_lower);
            if text.contains(&dot_free) || text.contains(&dot_destroy) {
                return true;
            }

            // Check for `FreeAndNil(variable)`
            let free_and_nil = format!("freeandnil({})", var_lower);
            if text.contains(&free_and_nil) {
                return true;
            }
        }
    }

    false
}

/// Check whether a source text references a variable name as a standalone word.
fn text_references_variable(text: &str, var_name: &str) -> bool {
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

/// Extract the UTF-8 text of a node from the source bytes.
fn node_text(node: Node, source: &[u8]) -> String {
    std::str::from_utf8(&source[node.start_byte()..node.end_byte()])
        .unwrap_or("")
        .to_string()
}
