use std::collections::HashSet;

use tree_sitter::{Node, Tree};

use crate::dcu::ProjectContext;
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

    fn check(
        &self,
        _file: &FileInfo,
        tree: &Tree,
        source: &[u8],
        _config: &crate::config::Config,
        ctx: &mut LintContext,
    ) {
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

    fn check(
        &self,
        _file: &FileInfo,
        tree: &Tree,
        source: &[u8],
        _config: &crate::config::Config,
        ctx: &mut LintContext,
    ) {
        let uses = extract_uses_clauses(tree.root_node(), source);
        visit_blocks_no_try(tree.root_node(), source, None, &uses, ctx);
    }

    fn check_with_context(
        &self,
        file: &FileInfo,
        tree: &Tree,
        source: &[u8],
        config: &crate::config::Config,
        project: &ProjectContext,
        ctx: &mut LintContext,
    ) {
        if project.unit_count() == 0 {
            // No DCU data loaded — fall back to heuristic-only path.
            self.check(file, tree, source, config, ctx);
            return;
        }
        let uses = extract_uses_clauses(tree.root_node(), source);
        visit_blocks_no_try(tree.root_node(), source, Some(project), &uses, ctx);
    }
}

/// Recursively walk the AST looking for block-like nodes and check for the
/// no-try pattern.
fn visit_blocks_no_try(
    node: Node,
    source: &[u8],
    project: Option<&ProjectContext>,
    uses: &[String],
    ctx: &mut LintContext,
) {
    if node.kind() == "block" {
        check_block_no_try(node, source, project, uses, ctx);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_blocks_no_try(child, source, project, uses, ctx);
    }
}

/// Check a block for constructor assignments that have no matching try block
/// (either `try..finally` or `try..except`) that frees the variable.
///
/// Skipped cases:
/// - Owner-managed objects: constructor called with non-nil arguments
/// - Field assignments in methods: fields are freed by the destructor
/// - `Result` assignments: ownership transfers to the caller
/// - Reference-counted objects: assigned to interface-typed variables
fn check_block_no_try(
    block: Node,
    source: &[u8],
    project: Option<&ProjectContext>,
    uses: &[String],
    ctx: &mut LintContext,
) {
    let children: Vec<Node> = block.children(&mut block.walk()).collect();
    let interface_vars = collect_interface_vars(block, source, project, uses);

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

        // Skip reference-counted objects: if the variable itself is
        // interface-typed, or is later assigned to an interface-typed
        // variable, reference counting manages the object's lifetime.
        // However, some classes (e.g. TNoRefCountObject) implement
        // IInterface with stub _AddRef/_Release that do NOT free the
        // object, so those must still be flagged.
        let ctor_class = extract_constructor_class_name(rhs_node, source);
        let non_refcounting = ctor_class
            .as_deref()
            .is_some_and(|cn| is_non_refcounting_class(cn, project, uses));

        if !non_refcounting
            && (interface_vars.contains(&var_name)
                || assigned_to_interface_var(&children, i, &var_name, &interface_vars, source))
        {
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

// ─── Interface / reference-counting helpers ──────────────────────────────────

/// Collect the names of all local variables whose declared type is an
/// interface type.
///
/// When a `ProjectContext` is available, uses DCU metadata to definitively
/// determine whether a type is an interface.  Falls back to the Delphi naming
/// convention (`I` + uppercase) when no DCU data is present.
fn collect_interface_vars(
    block: Node,
    source: &[u8],
    project: Option<&ProjectContext>,
    uses: &[String],
) -> HashSet<String> {
    let mut result = HashSet::new();

    // The parent of a block inside a procedure is the defProc/lambda node.
    let proc_node = match block.parent() {
        Some(p) if p.kind() == "defProc" || p.kind() == "lambda" => p,
        _ => return result,
    };

    let mut proc_cursor = proc_node.walk();
    for child in proc_node.children(&mut proc_cursor) {
        if child.kind() != "declVars" {
            continue;
        }
        let mut vars_cursor = child.walk();
        for decl_var in child.children(&mut vars_cursor) {
            if decl_var.kind() != "declVar" {
                continue;
            }
            let type_name = match extract_decl_var_type(decl_var, source) {
                Some(tn) => tn,
                None => continue,
            };
            if !is_interface_type(&type_name, project, uses) {
                continue;
            }
            // Collect all identifier names in this declVar (handles `a, b: IFoo`).
            let mut dv_cursor = decl_var.walk();
            for dv_child in decl_var.children(&mut dv_cursor) {
                if dv_child.kind() == "identifier" {
                    result.insert(node_text(dv_child, source));
                }
            }
        }
    }

    result
}

/// Determine whether a type name refers to an interface type.
///
/// Uses `ProjectContext` DCU metadata when available for a definitive answer.
/// Falls back to the Delphi naming convention (`I` + uppercase letter) when
/// the type is not found in any loaded DCU.
fn is_interface_type(
    type_name: &str,
    project: Option<&ProjectContext>,
    uses: &[String],
) -> bool {
    if let Some(proj) = project {
        if let Some(is_intf) = proj.is_interface_type(type_name, uses) {
            return is_intf;
        }
        // Type not found in any DCU — fall through to heuristic.
    }
    is_interface_type_by_name(type_name)
}

/// Heuristic: check whether a type name follows the Delphi interface naming
/// convention (`I` + uppercase letter), excluding common non-interface types.
fn is_interface_type_by_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'I' || !bytes[1].is_ascii_uppercase() {
        return false;
    }
    !matches!(
        name,
        "Integer" | "Int8" | "Int16" | "Int32" | "Int64" | "IDispatch"
    )
}

/// Extract the class name from a constructor call's RHS node.
///
/// For `TFoo.Create` (exprDot) returns `"TFoo"`.
/// For `TFoo.Create(args)` (exprCall wrapping exprDot) returns `"TFoo"`.
fn extract_constructor_class_name(rhs_node: Node, source: &[u8]) -> Option<String> {
    let dot_node = match rhs_node.kind() {
        "exprDot" => rhs_node,
        "exprCall" => rhs_node.child_by_field_name("entity")?,
        _ => return None,
    };
    if dot_node.kind() != "exprDot" {
        return None;
    }
    let lhs = dot_node.child_by_field_name("lhs")?;
    Some(node_text(lhs, source))
}

/// Known base classes whose `_AddRef`/`_Release` return -1, meaning
/// they do NOT perform reference counting despite implementing IInterface.
const NON_REFCOUNTING_CLASSES: &[&str] = &["TNoRefCountObject"];

/// Check whether the constructor's class is known to not support
/// reference counting.
///
/// When a `ProjectContext` is available, walks the class's parent chain
/// via DCU metadata to check for `TNoRefCountObject` ancestry.  Falls
/// back to a direct name match against the known list.
fn is_non_refcounting_class(
    class_name: &str,
    project: Option<&ProjectContext>,
    uses: &[String],
) -> bool {
    // Direct name match (works with or without DCU).
    if NON_REFCOUNTING_CLASSES
        .iter()
        .any(|&c| c.eq_ignore_ascii_case(class_name))
    {
        return true;
    }

    // DCU ancestry check: does the class descend from a non-ref-counting base?
    if let Some(proj) = project {
        for &ancestor in NON_REFCOUNTING_CLASSES {
            if let Some(true) = proj.descends_from(class_name, ancestor, uses) {
                return true;
            }
        }
    }

    false
}

/// Extract the type name string from a `declVar` node.
///
/// Expected structure: `declVar -> type -> typeref -> identifier`.
fn extract_decl_var_type(decl_var: Node, source: &[u8]) -> Option<String> {
    let mut cursor = decl_var.walk();
    for child in decl_var.children(&mut cursor) {
        if child.kind() == "type" {
            let mut type_cursor = child.walk();
            for type_child in child.children(&mut type_cursor) {
                if type_child.kind() == "typeref" {
                    let mut ref_cursor = type_child.walk();
                    for ref_child in type_child.children(&mut ref_cursor) {
                        if ref_child.kind() == "identifier" {
                            return Some(node_text(ref_child, source));
                        }
                    }
                }
            }
        }
    }
    None
}

/// Check whether the constructor-assigned variable is later assigned to an
/// interface-typed variable in the same block, which means the interface
/// reference counting will manage the object's lifetime.
fn assigned_to_interface_var(
    children: &[Node],
    start_idx: usize,
    var_name: &str,
    interface_vars: &HashSet<String>,
    source: &[u8],
) -> bool {
    if interface_vars.is_empty() {
        return false;
    }
    let var_lower = var_name.to_lowercase();
    for sibling in children.iter().skip(start_idx + 1) {
        if sibling.kind() != "assignment" {
            continue;
        }
        let lhs = match sibling.child_by_field_name("lhs") {
            Some(l) => l,
            None => continue,
        };
        let rhs = match sibling.child_by_field_name("rhs") {
            Some(r) => r,
            None => continue,
        };
        let lhs_text = node_text(lhs, source);
        let rhs_text = node_text(rhs, source);
        // RHS must reference the constructor variable, LHS must be an interface var.
        if rhs_text.to_lowercase() == var_lower && interface_vars.contains(&lhs_text) {
            return true;
        }
    }
    false
}

/// Extract all unit names from `declUses` nodes in the AST.
///
/// Collects unit names from both `interface` and `implementation` uses clauses.
fn extract_uses_clauses(root: Node, source: &[u8]) -> Vec<String> {
    let mut units = Vec::new();
    collect_uses_recursive(root, source, &mut units);
    units
}

fn collect_uses_recursive(node: Node, source: &[u8], units: &mut Vec<String>) {
    if node.kind() == "declUses" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "moduleName" {
                units.push(node_text(child, source));
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_uses_recursive(child, source, units);
    }
}

// ─── Common helpers ──────────────────────────────────────────────────────────

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
