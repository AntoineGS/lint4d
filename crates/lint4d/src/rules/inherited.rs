use pascal_core::node_kind as K;
use tree_sitter::{Node, Tree};

use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::rules::field_leak::{get_method_block, parse_def_proc};
use crate::rules::helpers::{first_identifier, node_text};
use crate::rules::{LintContext, Rule, RuleCategory, RuleMeta};

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Collect the meaningful direct-child statements of a `block` node.
///
/// Filters out `kBegin`, `kEnd`, punctuation (`;`), comments (`is_extra`),
/// and error/missing nodes.
fn block_statements(block: Node) -> Vec<Node> {
    super::helpers::effective_children(block)
        .into_iter()
        .filter(|c| {
            c.is_named()
                && !c.is_extra()
                && !c.is_error()
                && !c.is_missing()
                && c.kind() != K::K_BEGIN
                && c.kind() != K::K_END
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
    if node.kind() == K::INHERITED {
        return true;
    }
    // Check the first named, non-extra child of this statement node
    let mut cursor = node.walk();
    let first_child = node
        .children(&mut cursor)
        .find(|c| c.is_named() && !c.is_extra());
    if let Some(child) = first_child {
        if child.kind() == K::INHERITED {
            return true;
        }
    }
    false
}

/// Find the first `inherited` node anywhere inside `node` (recursive).
fn find_inherited(node: Node) -> Option<Node> {
    if node.kind() == K::INHERITED {
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
                description: "Checks that 'inherited' is the first statement in constructors \
                     and the last statement in destructors.",
                enabled_by_default: true,
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
    if node.kind() == K::DEF_PROC {
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
                scope: None,
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
                scope: None,
            });
        }
    }
}

// ─── Reintroduce detection helpers ──────────────────────────────────────────

/// Check if a method is declared with `reintroduce` in the interface section.
///
/// Walks the AST root for `declType` nodes, finds matching class by name, and
/// checks for `kReintroduce` inside a `procAttribute` child of the method.
fn is_reintroduced(root: Node, class_name: &str, method_name: &str, source: &[u8]) -> bool {
    find_reintroduce_recursive(root, class_name, method_name, source)
}

fn find_reintroduce_recursive(
    node: Node,
    class_name: &str,
    method_name: &str,
    source: &[u8],
) -> bool {
    if node.kind() == K::DECL_TYPE {
        // Check if this is the class we're looking for.
        // Use child_by_field_name("name") with first_identifier fallback
        // (some grammar versions don't set the "name" field on declType).
        let name_node = node
            .child_by_field_name("name")
            .or_else(|| first_identifier(node));
        if let Some(name_node) = name_node {
            let name = node_text(name_node, source);
            if name.eq_ignore_ascii_case(class_name) {
                // Find the declClass child
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == K::DECL_CLASS {
                        return class_has_reintroduced_method(child, method_name, source);
                    }
                }
            }
        }
        return false;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if find_reintroduce_recursive(child, class_name, method_name, source) {
            return true;
        }
    }
    false
}

/// Check if a `declClass` node contains a `declProc` for the given method
/// with a `reintroduce` directive.
fn class_has_reintroduced_method(decl_class: Node, method_name: &str, source: &[u8]) -> bool {
    let mut cursor = decl_class.walk();
    for child in decl_class.children(&mut cursor) {
        // declProc can be direct child or inside declSection
        if child.kind() == K::DECL_PROC
            && decl_proc_matches_and_reintroduced(child, method_name, source)
        {
            return true;
        } else if child.kind() == K::DECL_SECTION || child.kind() == K::PP_DECL_SECTION {
            let mut section_cursor = child.walk();
            for section_child in child.children(&mut section_cursor) {
                if section_child.kind() == K::DECL_PROC
                    && decl_proc_matches_and_reintroduced(section_child, method_name, source)
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Check if a `declProc` node matches the method name and has `reintroduce`.
fn decl_proc_matches_and_reintroduced(decl_proc: Node, method_name: &str, source: &[u8]) -> bool {
    // Get the method name from the declProc
    let proc_name = if let Some(name_node) = decl_proc.child_by_field_name("name") {
        node_text(name_node, source)
    } else {
        return false;
    };

    if !proc_name.eq_ignore_ascii_case(method_name) {
        return false;
    }

    // Check for procAttribute > kReintroduce
    let mut cursor = decl_proc.walk();
    for child in decl_proc.children(&mut cursor) {
        if child.kind() == K::PROC_ATTRIBUTE {
            let mut attr_cursor = child.walk();
            for attr_child in child.children(&mut attr_cursor) {
                if attr_child.kind() == K::K_REINTRODUCE {
                    return true;
                }
            }
        }
    }
    false
}

// ─── InheritedCallMissingRule ────────────────────────────────────────────────

pub struct InheritedCallMissingRule {
    meta: RuleMeta,
}

impl Default for InheritedCallMissingRule {
    fn default() -> Self {
        Self::new()
    }
}

impl InheritedCallMissingRule {
    pub fn new() -> Self {
        InheritedCallMissingRule {
            meta: RuleMeta {
                id: "inherited-missing",
                name: "Inherited Call Missing",
                category: RuleCategory::DangerousPattern,
                default_severity: Severity::Hint,
                description: "Checks that constructors and destructors call 'inherited'.",
                enabled_by_default: true,
            },
        }
    }
}

impl Rule for InheritedCallMissingRule {
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
        visit_def_procs_missing(tree.root_node(), tree.root_node(), source, ctx);
    }
}

fn visit_def_procs_missing(node: Node, root: Node, source: &[u8], ctx: &mut LintContext) {
    if node.kind() == K::DEF_PROC {
        check_inherited_missing(node, root, source, ctx);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_def_procs_missing(child, root, source, ctx);
    }
}

fn check_inherited_missing(def_proc: Node, root: Node, source: &[u8], ctx: &mut LintContext) {
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

    // Check if inherited exists anywhere in the block
    if find_inherited(block).is_some() {
        return; // Has inherited — all good
    }

    // Check if the method is reintroduced — if so, suppress
    if is_reintroduced(root, &class_name, &method_name, source) {
        return;
    }

    // Report on the declProc (header) node
    let header = def_proc
        .children(&mut def_proc.walk())
        .find(|c| c.kind() == K::DECL_PROC)
        .unwrap_or(def_proc);
    let start = header.start_position();
    let end = header.end_position();

    let kind = if is_constructor {
        "Constructor"
    } else {
        "Destructor"
    };

    let help = if is_constructor {
        "Add an 'inherited' call to ensure proper parent class initialization"
    } else {
        "Add an 'inherited' call to ensure proper parent class cleanup"
    };

    ctx.report(Diagnostic {
        rule_id: "inherited-missing".to_string(),
        severity: Severity::Hint,
        message: format!(
            "{} {}.{} does not call 'inherited'",
            kind, class_name, method_name
        ),
        line: start.row + 1,
        column: start.column + 1,
        end_line: end.row + 1,
        end_column: end.column + 1,
        help: Some(help.to_string()),
        scope: None,
    });
}
