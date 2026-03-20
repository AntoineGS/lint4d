use tree_sitter::{Node, Tree};

use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::rules::helpers::{
    constructor_has_owner_args, is_constructor_call, node_text, statements_free_variable,
    text_references_variable,
};
use crate::rules::{LintContext, Rule, RuleCategory, RuleMeta};

pub struct ResourceLeakUnprotectedRule {
    meta: RuleMeta,
}

impl Default for ResourceLeakUnprotectedRule {
    fn default() -> Self {
        Self::new()
    }
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
        for (j, sibling) in children.iter().enumerate().skip(i + 1) {
            if sibling.kind() == "try" && finally_frees_variable(*sibling, source, &var_name) {
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
        for sibling in children.iter().take(try_idx).skip(i + 1) {
            let stmt = *sibling;
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

/// Check whether the `finally` clause of a `try` node frees the given variable.
///
/// Looks for patterns like `variable.Free` or `FreeAndNil(variable)` in the
/// finally block's source text.
fn finally_frees_variable(try_node: Node, source: &[u8], var_name: &str) -> bool {
    let finally_children: Vec<Node> = try_node
        .children_by_field_name("finally", &mut try_node.walk())
        .collect();

    for child in &finally_children {
        if child.kind() == "statements" {
            return statements_free_variable(*child, source, var_name);
        }
    }

    false
}

// ─── resource-leak-no-try ────────────────────────────────────────────────────

pub struct ResourceLeakNoTryRule {
    meta: RuleMeta,
}

impl Default for ResourceLeakNoTryRule {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceLeakNoTryRule {
    pub fn new() -> Self {
        ResourceLeakNoTryRule {
            meta: RuleMeta {
                id: "resource-leak-no-try",
                name: "Resource Leak: No Try Block",
                category: RuleCategory::ResourceManagement,
                default_severity: Severity::Warning,
                description: "Detects resources created without any try..finally block.",
            },
        }
    }
}

impl Rule for ResourceLeakNoTryRule {
    fn meta(&self) -> &RuleMeta {
        &self.meta
    }

    fn check(&self, _file: &FileInfo, tree: &Tree, source: &[u8], ctx: &mut LintContext) {
        visit_blocks_no_try(tree.root_node(), source, ctx);
    }
}

/// Recursively walk the AST looking for block-like nodes and check for the
/// no-try pattern.
fn visit_blocks_no_try(node: Node, source: &[u8], ctx: &mut LintContext) {
    if node.kind() == "block" {
        check_block_no_try(node, source, ctx);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_blocks_no_try(child, source, ctx);
    }
}

/// Check a block for constructor assignments that have no matching try block
/// (either `try..finally` or `try..except`) that frees the variable.
///
/// Skipped cases:
/// - Owner-managed objects: constructor called with non-nil arguments
/// - Field assignments in methods: fields are freed by the destructor
/// - `Result` assignments: ownership transfers to the caller
fn check_block_no_try(block: Node, source: &[u8], ctx: &mut LintContext) {
    let children: Vec<Node> = block.children(&mut block.walk()).collect();

    for (i, child) in children.iter().enumerate() {
        if child.kind() != "assignment" {
            continue;
        }

        let (var_name, rhs_node) = match extract_constructor_assignment_with_rhs(*child, source) {
            Some(pair) => pair,
            None => continue,
        };

        // Skip owner-managed objects: constructor was called with arguments.
        if constructor_has_owner_args(rhs_node, source) {
            continue;
        }

        // Skip field assignments: fields are owned by the class and freed
        // in the destructor.
        if is_field_name(&var_name) {
            continue;
        }

        // Skip `Result` assignments: a function returning a newly created
        // object transfers ownership to the caller.
        if var_name.eq_ignore_ascii_case("result") {
            continue;
        }

        // Look ahead for any try block (finally or except) that frees this variable.
        let has_protecting_try = children[(i + 1)..].iter().any(|sibling| {
            sibling.kind() == "try" && try_frees_variable(*sibling, source, &var_name)
        });

        if !has_protecting_try {
            let start = child.start_position();
            let end = child.end_position();
            ctx.report(Diagnostic {
                rule_id: "resource-leak-no-try".to_string(),
                severity: Severity::Warning,
                message: format!(
                    "'{}' is created without a try..finally block. \
                     If an exception occurs after construction, the object will leak.",
                    var_name
                ),
                line: start.row + 1,
                column: start.column + 1,
                end_line: end.row + 1,
                end_column: end.column + 1,
                help: Some(
                    "Wrap the usage in a try..finally block and free the object in the finally clause."
                        .to_string(),
                ),
            });
        }
    }
}

/// Check whether a variable name follows the Delphi field naming convention.
///
/// Fields in Delphi conventionally start with 'F' followed by an uppercase
/// letter (e.g., `FDatabase`, `FAdapter`).
fn is_field_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() >= 2 && bytes[0] == b'F' && bytes[1].is_ascii_uppercase()
}

/// Check whether a `try` block (either finally or except) frees the given variable.
fn try_frees_variable(try_node: Node, source: &[u8], var_name: &str) -> bool {
    finally_frees_variable(try_node, source, var_name)
        || except_frees_variable(try_node, source, var_name)
}

/// Check whether the `except` clause of a `try` node frees the given variable.
///
/// Recognises the cleanup-then-reraise pattern:
///   try ... except variable.Free; raise; end;
fn except_frees_variable(try_node: Node, source: &[u8], var_name: &str) -> bool {
    let mut found_except = false;
    let mut cursor = try_node.walk();
    for child in try_node.children(&mut cursor) {
        if child.kind() == "kExcept" {
            found_except = true;
            continue;
        }
        if found_except && child.kind() == "statements" {
            return statements_free_variable(child, source, var_name);
        }
    }
    false
}

/// Like `extract_constructor_assignment` but also returns the RHS node so
/// we can inspect whether the constructor has arguments.
fn extract_constructor_assignment_with_rhs<'a>(
    node: Node<'a>,
    source: &[u8],
) -> Option<(String, Node<'a>)> {
    let lhs = node.child_by_field_name("lhs")?;
    let rhs = node.child_by_field_name("rhs")?;

    if !is_constructor_call(rhs, source) {
        return None;
    }

    Some((node_text(lhs, source), rhs))
}
