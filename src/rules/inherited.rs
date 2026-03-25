use tree_sitter::{Node, Tree};

use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::rules::field_leak::{get_method_block, parse_def_proc};
use crate::rules::helpers::node_text;
use crate::rules::{LintContext, Rule, RuleCategory, RuleMeta};

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Return the first `identifier` child that isn't a comment/extra node.
fn first_identifier(node: Node) -> Option<Node> {
    let count = node.child_count();
    for i in 0..count {
        let child = node.child(i)?;
        if child.kind() == "identifier" && !child.is_extra() {
            return Some(child);
        }
    }
    None
}

/// Collect the meaningful direct-child statements of a `block` node.
///
/// Filters out `kBegin`, `kEnd`, punctuation (`;`), comments (`is_extra`),
/// and error/missing nodes.
fn block_statements(block: Node) -> Vec<Node> {
    let mut cursor = block.walk();
    block
        .children(&mut cursor)
        .filter(|c| {
            c.is_named()
                && !c.is_extra()
                && !c.is_error()
                && !c.is_missing()
                && c.kind() != "kBegin"
                && c.kind() != "kEnd"
        })
        .collect()
}

/// Return true when `node` is *directly* an inherited call — i.e. the node
/// itself is `inherited`, or its immediate first meaningful child is
/// `inherited` (covers `inherited Create`, `inherited Destroy`, etc.).
///
/// Intentionally does NOT recurse into control-flow nodes (if/try/for/while)
/// so that `inherited` nested inside a branch is NOT considered a direct call.
fn statement_is_direct_inherited(node: Node) -> bool {
    if node.kind() == "inherited" {
        return true;
    }
    // Check the first named, non-extra child of this statement node
    let mut cursor = node.walk();
    let first_child = node
        .children(&mut cursor)
        .find(|c| c.is_named() && !c.is_extra());
    if let Some(child) = first_child {
        if child.kind() == "inherited" {
            return true;
        }
    }
    false
}

/// Find the first `inherited` node anywhere inside `node` (recursive).
fn find_inherited(node: Node) -> Option<Node> {
    if node.kind() == "inherited" {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = find_inherited(child) {
            return Some(found);
        }
    }
    None
}

// ─── InheritedCallOrderRule ─────────────────────────────────────────────────

pub struct InheritedCallOrderRule {
    meta: RuleMeta,
}

impl Default for InheritedCallOrderRule {
    fn default() -> Self {
        Self::new()
    }
}

impl InheritedCallOrderRule {
    pub fn new() -> Self {
        InheritedCallOrderRule {
            meta: RuleMeta {
                id: "inherited-order",
                name: "Inherited Call Order",
                category: RuleCategory::DangerousPattern,
                default_severity: Severity::Hint,
                description:
                    "Checks that 'inherited' is the first statement in constructors \
                     and the last statement in destructors.",
            },
        }
    }
}

impl Rule for InheritedCallOrderRule {
    fn meta(&self) -> &RuleMeta {
        &self.meta
    }

    fn check(
        &self,
        _file: &FileInfo,
        tree: &Tree,
        source: &[u8],
        _config: &crate::config::Config,
        ctx: &mut LintContext,
    ) {
        visit_def_procs_order(tree.root_node(), source, ctx);
    }
}

fn visit_def_procs_order(node: Node, source: &[u8], ctx: &mut LintContext) {
    if node.kind() == "defProc" {
        check_inherited_order(node, source, ctx);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_def_procs_order(child, source, ctx);
    }
}

fn check_inherited_order(def_proc: Node, source: &[u8], ctx: &mut LintContext) {
    let Some((class_name, method_name, is_constructor, is_destructor)) =
        parse_def_proc(def_proc, source)
    else {
        return;
    };

    if !is_constructor && !is_destructor {
        return;
    }

    let Some(block) = get_method_block(def_proc) else {
        return;
    };

    let stmts = block_statements(block);
    if stmts.is_empty() {
        return; // Empty block — inherited-missing handles this
    }

    // Find the inherited node anywhere in the block
    let Some(inherited_node) = find_inherited(block) else {
        return; // No inherited — inherited-missing handles this
    };

    if is_constructor {
        // First statement must directly be inherited (not nested in control flow)
        if !statement_is_direct_inherited(stmts[0]) {
            let start = inherited_node.start_position();
            let end = inherited_node.end_position();
            ctx.report(Diagnostic {
                rule_id: "inherited-order".to_string(),
                severity: Severity::Hint,
                message: format!(
                    "'inherited' should be the first statement in constructor {}.{}",
                    class_name, method_name
                ),
                line: start.row + 1,
                column: start.column + 1,
                end_line: end.row + 1,
                end_column: end.column + 1,
                help: Some(
                    "Move the 'inherited' call to the beginning of the constructor \
                     to ensure the parent class is fully initialized before \
                     accessing its members"
                        .to_string(),
                ),
            });
        }
    } else {
        // Destructor: last statement must directly be inherited (not nested in control flow)
        if !statement_is_direct_inherited(stmts[stmts.len() - 1]) {
            let start = inherited_node.start_position();
            let end = inherited_node.end_position();
            ctx.report(Diagnostic {
                rule_id: "inherited-order".to_string(),
                severity: Severity::Hint,
                message: format!(
                    "'inherited' should be the last statement in destructor {}.{}",
                    class_name, method_name
                ),
                line: start.row + 1,
                column: start.column + 1,
                end_line: end.row + 1,
                end_column: end.column + 1,
                help: Some(
                    "Move the 'inherited' call to the end of the destructor \
                     to ensure your cleanup runs before the parent class is destroyed"
                        .to_string(),
                ),
            });
        }
    }
}

// Keep node_text in scope so it's available for Task 5 (inherited-missing)
// when it imports from this module; suppress the dead-code warning in the
// meantime.
#[allow(dead_code)]
fn _use_node_text_and_first_identifier() {
    // These are used by inherited-missing (Task 5).
    let _ = node_text as fn(_, _) -> _;
    let _ = first_identifier as fn(_) -> _;
}
