use std::collections::HashMap;

use pascal_core::node_kind as K;
use tree_sitter::{Node, Tree};

use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::rules::helpers::node_text;
use crate::rules::scope::{
    Scopes, collect_file_scope, collect_method_scope, extract_class_name, is_declaration_position,
    is_dot_rhs, is_inside_inherited, is_inside_module_name, is_inside_typeref,
};
use crate::rules::{LintContext, Rule, RuleCategory, RuleMeta};

// ---------------------------------------------------------------------------
// IdentifierCasingRule
// ---------------------------------------------------------------------------

/// Enforces that every usage of an identifier matches the exact casing of its
/// declaration. Uses a two-pass approach with a three-level scope model:
///
/// - File scope: type names, constants, global vars, standalone procedures
/// - Class scope: fields, keyed by class name (lowercase)
/// - Method scope: parameters and local variables
pub struct IdentifierCasingRule {
    meta: RuleMeta,
}

impl Default for IdentifierCasingRule {
    fn default() -> Self {
        Self::new()
    }
}

impl IdentifierCasingRule {
    pub fn new() -> Self {
        IdentifierCasingRule {
            meta: RuleMeta {
                id: "identifier-casing",
                name: "Identifier Casing",
                category: RuleCategory::NamingConvention,
                default_severity: Severity::Hint,
                description: "Enforces that every usage of an identifier matches the exact casing of its declaration.",
                enabled_by_default: true,
            },
        }
    }
}

impl Rule for IdentifierCasingRule {
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
        let root = tree.root_node();
        let scope = collect_file_scope(root, source);
        check_usages(root, source, &scope, ctx);
    }
}

// ---------------------------------------------------------------------------
// Pass 2: check usages
// ---------------------------------------------------------------------------

fn check_usages(root: Node, source: &[u8], scopes: &Scopes, ctx: &mut LintContext) {
    // Walk the implementation section, visiting each defProc individually.
    // Handles unit, program, and library root nodes.
    for child in root.children(&mut root.walk()) {
        match child.kind() {
            K::UNIT | K::PROGRAM | K::LIBRARY => {
                check_unit_usages(child, source, scopes, ctx);
            }
            _ => {}
        }
    }
}

fn check_unit_usages(unit: Node, source: &[u8], scopes: &Scopes, ctx: &mut LintContext) {
    for child in unit.children(&mut unit.walk()) {
        if child.kind() == K::IMPLEMENTATION {
            for impl_child in super::helpers::effective_children(child) {
                if impl_child.kind() == K::DEF_PROC {
                    check_proc_usages(impl_child, source, scopes, ctx);
                }
            }
        }
    }
}

/// Check all identifier usages inside a single `defProc`.
fn check_proc_usages(def_proc: Node, source: &[u8], scopes: &Scopes, ctx: &mut LintContext) {
    // Build the method-level scope: params + local vars.
    let mut method_scope: HashMap<String, String> = HashMap::new();
    collect_method_scope(def_proc, source, &mut method_scope);

    // Determine which class this method belongs to (if any).
    let class_name = extract_class_name(def_proc, source);
    let class_fields: Option<&HashMap<String, String>> = class_name
        .as_ref()
        .and_then(|cn| scopes.classes.get(&cn.to_lowercase()));

    // Walk the body and local declarations, checking usages.
    walk_and_check(def_proc, source, scopes, &method_scope, class_fields, ctx);
}

/// Walk all nodes under `node`, checking identifier usages.
/// For nested `defProc`/`lambda`, recurse with updated scopes.
fn walk_and_check(
    node: Node<'_>,
    source: &[u8],
    scopes: &Scopes,
    method_scope: &HashMap<String, String>,
    class_fields: Option<&HashMap<String, String>>,
    ctx: &mut LintContext,
) {
    if node.kind() == K::IDENTIFIER {
        check_identifier_usage(node, source, scopes, method_scope, class_fields, ctx);
        return;
    }

    // For nested lambdas/defProcs, build a fresh method scope that merges
    // outer method scope with the nested one.
    if node.kind() == K::DEF_PROC || node.kind() == K::LAMBDA {
        // Collect nested method scope (params + locals of the nested proc).
        let mut nested_method_scope = method_scope.clone();
        collect_method_scope(node, source, &mut nested_method_scope);
        // Nested lambdas inherit parent class context.
        for child in node.children(&mut node.walk()) {
            walk_and_check(
                child,
                source,
                scopes,
                &nested_method_scope,
                class_fields,
                ctx,
            );
        }
        return;
    }

    for child in node.children(&mut node.walk()) {
        walk_and_check(child, source, scopes, method_scope, class_fields, ctx);
    }
}

/// Check a single identifier node against the known scopes.
fn check_identifier_usage(
    node: Node,
    source: &[u8],
    scopes: &Scopes,
    method_scope: &HashMap<String, String>,
    class_fields: Option<&HashMap<String, String>>,
    ctx: &mut LintContext,
) {
    // Skip if this is a declaration position.
    if is_declaration_position(node) {
        return;
    }
    // Skip RHS of dot access (e.g. `.Free`, `.Create` — method calls on objects).
    if is_dot_rhs(node) {
        return;
    }
    // Skip identifiers inside `inherited` calls.
    if is_inside_inherited(node) {
        return;
    }
    // Skip type annotations.
    if is_inside_typeref(node) {
        return;
    }
    // Skip module name identifiers.
    if is_inside_module_name(node) {
        return;
    }

    let used = node_text(node, source);
    let used_lower = used.to_lowercase();

    // Look up in scope chain: method → class fields → file.
    let declared = method_scope
        .get(&used_lower)
        .or_else(|| class_fields.and_then(|cf| cf.get(&used_lower)))
        .or_else(|| scopes.file.get(&used_lower));

    if let Some(declared_name) = declared {
        if *declared_name != used {
            let start = node.start_position();
            let end = node.end_position();
            ctx.report(Diagnostic {
                rule_id: "identifier-casing".to_string(),
                severity: Severity::Hint,
                message: format!(
                    "Identifier '{}' was declared as '{}' but used with different casing.",
                    used, declared_name
                ),
                line: start.row + 1,
                column: start.column + 1,
                end_line: end.row + 1,
                end_column: end.column + 1,
                help: Some(format!("Rename usage to '{}'.", declared_name)),
                scope: None,
            });
        }
    }
}
