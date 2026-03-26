use std::collections::{HashMap, HashSet};

use tree_sitter::{Node, Tree};

use crate::cfg::analysis::AnalysisContext;
use crate::dcu::ProjectContext;
use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::rules::helpers;
use crate::rules::helpers::{
    ast_references_variable, extract_uses_clauses, is_constructor_call, node_text,
    statements_free_variable,
};
use crate::rules::{LintContext, Rule, RuleCategory, RuleMeta};

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

    fn requires_cfg(&self) -> bool {
        true
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

    fn check_cfg(
        &self,
        file: &FileInfo,
        tree: &Tree,
        source: &[u8],
        config: &crate::config::Config,
        _analysis: &AnalysisContext<'_>,
        ctx: &mut LintContext,
    ) {
        self.check(file, tree, source, config, ctx);
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
        // Step 1: Is this an assignment whose RHS is a constructor or factory call?
        if child.kind() != "assignment" {
            continue;
        }

        let lhs = match child.child_by_field_name("lhs") {
            Some(l) => l,
            None => continue,
        };
        let rhs = match child.child_by_field_name("rhs") {
            Some(r) => r,
            None => continue,
        };
        let var_name = node_text(lhs, source);

        if !is_constructor_call(rhs, source) {
            continue;
        }

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
        let label = "constructor";
        for sibling in children.iter().take(try_idx).skip(i + 1) {
            let stmt = *sibling;
            if ast_references_variable(stmt, source, &var_name) {
                let start = stmt.start_position();
                let end = stmt.end_position();
                ctx.report(Diagnostic {
                    rule_id: "resource-leak-unprotected".to_string(),
                    severity: Severity::Error,
                    message: format!(
                        "Statement uses '{}' between {} and try..finally block. \
                         If this line raises an exception, the object will leak.",
                        var_name, label
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

    fn requires_context(&self) -> bool {
        true
    }

    fn check(
        &self,
        _file: &FileInfo,
        _tree: &Tree,
        _source: &[u8],
        _config: &crate::config::Config,
        _ctx: &mut LintContext,
    ) {
        // Engine skips check() for rules where requires_context() == true.
        // This method exists only to satisfy the trait.
    }

    fn check_with_context(
        &self,
        _file: &FileInfo,
        tree: &Tree,
        source: &[u8],
        _config: &crate::config::Config,
        project: &ProjectContext,
        ctx: &mut LintContext,
    ) {
        let uses = extract_uses_clauses(tree.root_node(), source);
        let ast_intf_types = collect_ast_interface_types(tree.root_node(), source);
        visit_blocks_no_try(
            tree.root_node(),
            source,
            project,
            &uses,
            &ast_intf_types,
            ctx,
        );
    }

    fn check_cfg(
        &self,
        file: &FileInfo,
        tree: &Tree,
        source: &[u8],
        config: &crate::config::Config,
        analysis: &AnalysisContext<'_>,
        ctx: &mut LintContext,
    ) {
        self.check_with_context(file, tree, source, config, analysis.project, ctx);
    }
}

fn visit_blocks_no_try(
    root: Node,
    source: &[u8],
    project: &ProjectContext,
    uses: &[String],
    ast_intf_types: &HashSet<String>,
    ctx: &mut LintContext,
) {
    // Build a map of class name → field names from AST class declarations
    // in the current file. This covers classes defined in the same unit
    // (which won't be in the DCU search path).
    let ast_fields = collect_ast_class_fields(root, source);

    // 1. Check blocks inside defProc nodes (class methods + standalone procs)
    let procs = collect_leak_check_procs(root, source);
    for proc_info in &procs {
        let block = match proc_info.block {
            Some(b) => b,
            None => continue,
        };
        let field_names: Vec<String> = if proc_info.class_name.is_empty() {
            Vec::new()
        } else {
            // Try AST fields first (same-file class), then fall back to DCU.
            let key = proc_info.class_name.to_lowercase();
            ast_fields
                .get(&key)
                .cloned()
                .unwrap_or_else(|| project.get_field_names(&proc_info.class_name, uses))
        };
        check_block_no_try(
            block,
            source,
            project,
            uses,
            ast_intf_types,
            &field_names,
            ctx,
        );
    }

    // 2. Check top-level blocks not inside any defProc
    visit_non_proc_blocks(root, source, project, uses, ast_intf_types, ctx);
}

/// Check a block for constructor assignments that have no matching try block
/// (either `try..finally` or `try..except`) that frees the variable.
///
/// Skipped cases:
/// - Owner-managed objects: constructor called with non-nil arguments
/// - Field assignments in methods: fields are freed by the destructor
/// - `Result` assignments: ownership transfers to the caller
/// - Reference-counted objects: assigned to interface-typed variables
#[allow(clippy::too_many_arguments)]
fn check_block_no_try(
    block: Node,
    source: &[u8],
    project: &ProjectContext,
    uses: &[String],
    ast_intf_types: &HashSet<String>,
    field_names: &[String],
    ctx: &mut LintContext,
) {
    let children: Vec<Node> = block.children(&mut block.walk()).collect();
    let interface_vars = collect_interface_vars(block, source, project, uses, ast_intf_types);

    for (i, child) in children.iter().enumerate() {
        if child.kind() != "assignment" {
            continue;
        }

        let lhs = match child.child_by_field_name("lhs") {
            Some(l) => l,
            None => continue,
        };
        let rhs = match child.child_by_field_name("rhs") {
            Some(r) => r,
            None => continue,
        };
        let var_name = node_text(lhs, source);

        if !is_constructor_call(rhs, source) {
            continue;
        }

        // --- Constructor-specific skip logic ---
        {
            // Skip owner-managed objects: constructor's first param descends from
            // TComponent and a non-nil argument was passed.
            let ctor_class_for_owner = extract_constructor_class_name(rhs, source);
            if helpers::constructor_is_owner_managed(
                rhs,
                source,
                ctor_class_for_owner.as_deref(),
                project,
                uses,
            ) {
                continue;
            }

            // Skip field assignments: fields are owned by the class and freed
            // in the destructor.
            if field_names
                .iter()
                .any(|f| f.eq_ignore_ascii_case(&var_name))
            {
                continue;
            }
        }

        // For `Result` assignments the function transfers ownership to the
        // caller, so a simple `Result := TFoo.Create` is fine.  However, if
        // any code after the constructor can raise and is NOT inside a
        // protecting try block, the object leaks before the caller receives
        // it.  Check that every raise-bearing sibling is a try node itself.
        if var_name.eq_ignore_ascii_case("result") {
            let has_unprotected_raise = children[(i + 1)..].iter().any(|s| {
                s.is_named() && !s.is_extra() && s.kind() != "try" && ast_contains_raise(*s)
            });
            if has_unprotected_raise {
                let start = child.start_position();
                let end = child.end_position();
                ctx.report(Diagnostic {
                    rule_id: "resource-leak-no-try".to_string(),
                    severity: Severity::Warning,
                    message: format!(
                        "'{}' is assigned via constructor but a raise statement after it \
                         is not protected by try..except. If the raise executes, the \
                         object will leak.",
                        var_name
                    ),
                    line: start.row + 1,
                    column: start.column + 1,
                    end_line: end.row + 1,
                    end_column: end.column + 1,
                    help: Some(
                        "Wrap all code after the constructor in a try..except block \
                         that frees Result and re-raises."
                            .to_string(),
                    ),
                });
            }
            continue;
        }

        // --- Reference counting skip logic ---
        {
            // Skip reference-counted objects: if the variable itself is
            // interface-typed, or is later assigned to an interface-typed
            // variable, reference counting manages the object's lifetime.
            // However, some classes (e.g. TNoRefCountObject) implement
            // IInterface with stub _AddRef/_Release that do NOT free the
            // object, so those must still be flagged.
            let ctor_class = extract_constructor_class_name(rhs, source);
            let non_refcounting = ctor_class
                .as_deref()
                .is_some_and(|cn| is_non_refcounting_class(cn, project, uses));

            if !non_refcounting
                && (interface_vars.contains(&var_name)
                    || assigned_to_interface_var(&children, i, &var_name, &interface_vars, source))
            {
                continue;
            }
        }

        // Skip when the very next statement frees the variable — no code can
        // throw between the constructor and the cleanup.
        let next_stmt = children[(i + 1)..]
            .iter()
            .find(|n| n.is_named() && !n.is_extra() && n.kind() != "kEnd");
        if let Some(next) = next_stmt {
            if helpers::ast_frees_variable(*next, source, &var_name) {
                continue;
            }
        }

        // Look ahead for any try block (finally or except) that frees this variable.
        let has_protecting_try = children[(i + 1)..].iter().any(|sibling| {
            sibling.kind() == "try" && try_frees_variable(*sibling, source, &var_name)
        });

        if !has_protecting_try {
            let start = child.start_position();
            let end = child.end_position();
            let message = format!(
                "'{}' is created without a try..finally block. \
                 If an exception occurs after construction, the object will leak.",
                var_name
            );
            ctx.report(Diagnostic {
                rule_id: "resource-leak-no-try".to_string(),
                severity: Severity::Warning,
                message,
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
/// Uses DCU metadata via `ProjectContext` to determine whether a type is an
/// interface.
fn collect_interface_vars(
    block: Node,
    source: &[u8],
    project: &ProjectContext,
    uses: &[String],
    ast_intf_types: &HashSet<String>,
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
            if !is_interface_type(&type_name, project, uses, ast_intf_types) {
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

/// Well-known interface types from the implicit `System` unit.
/// These are always available without a `uses` clause and may not be
/// present in loaded DCUs (the `System` unit is often missing from
/// configured DCU paths).
const BUILTIN_INTERFACE_TYPES: &[&str] = &["IInterface", "IUnknown", "IInvokable"];

/// Determine whether a type name refers to an interface type.
///
/// Checks well-known built-in interface types first, then same-file AST
/// declarations, then falls back to `ProjectContext` DCU metadata.
fn is_interface_type(
    type_name: &str,
    project: &ProjectContext,
    uses: &[String],
    ast_intf_types: &HashSet<String>,
) -> bool {
    if BUILTIN_INTERFACE_TYPES
        .iter()
        .any(|&b| b.eq_ignore_ascii_case(type_name))
    {
        return true;
    }
    if ast_intf_types
        .iter()
        .any(|n| n.eq_ignore_ascii_case(type_name))
    {
        return true;
    }
    project.is_interface_type(type_name, uses).unwrap_or(false)
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
/// Walks the class's parent chain via DCU metadata to check for
/// `TNoRefCountObject` ancestry.
fn is_non_refcounting_class(class_name: &str, project: &ProjectContext, uses: &[String]) -> bool {
    for &ancestor in NON_REFCOUNTING_CLASSES {
        if let Some(true) = project.descends_from(class_name, ancestor, uses) {
            return true;
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

// ─── Common helpers ──────────────────────────────────────────────────────────

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

// ─── AST class field collector ────────────────────────────────────────────────

/// Collect field names from class declarations in the AST (same file).
///
/// Returns a map from lowercase class name to its field names.
fn collect_ast_class_fields(root: Node, source: &[u8]) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    collect_ast_class_fields_recursive(root, source, &mut map);
    map
}

fn collect_ast_class_fields_recursive(
    node: Node,
    source: &[u8],
    out: &mut HashMap<String, Vec<String>>,
) {
    if node.kind() == "declType" {
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
fn parse_class_fields(node: Node, source: &[u8]) -> Option<(String, Vec<String>)> {
    let name_node = match node.child_by_field_name("name") {
        Some(n) => n,
        None => first_identifier(node)?,
    };
    let name = node_text(name_node, source);

    let mut cursor = node.walk();
    let decl_class = node
        .children(&mut cursor)
        .find(|c| c.kind() == "declClass")?;

    let mut fields = Vec::new();
    let mut class_cursor = decl_class.walk();
    for section in decl_class.children(&mut class_cursor) {
        if section.kind() == "declSection" {
            let mut section_cursor = section.walk();
            for item in section.children(&mut section_cursor) {
                if item.kind() == "declField" {
                    if let Some(id_node) = first_identifier(item) {
                        fields.push(node_text(id_node, source));
                    }
                }
            }
        }
    }
    Some((name, fields))
}

/// Collect interface type names from `declType` nodes in the AST.
///
/// Recognises types like `IMyService = interface ... end;` so that
/// same-file interface declarations are available even without DCU metadata.
fn collect_ast_interface_types(root: Node, source: &[u8]) -> HashSet<String> {
    let mut result = HashSet::new();
    collect_ast_interface_types_recursive(root, source, &mut result);
    result
}

fn collect_ast_interface_types_recursive(node: Node, source: &[u8], out: &mut HashSet<String>) {
    if node.kind() == "declType" {
        if let Some(type_node) = node.child_by_field_name("type") {
            if type_node.kind() == "declIntf" {
                if let Some(name_node) = node.child_by_field_name("name") {
                    out.insert(node_text(name_node, source));
                } else if let Some(id) = first_identifier(node) {
                    out.insert(node_text(id, source));
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_ast_interface_types_recursive(child, source, out);
    }
}

// ─── Proc-aware block visitor ────────────────────────────────────────────────

/// Info about a procedure for leak checking — includes standalone procs.
struct LeakCheckProc<'a> {
    class_name: String, // empty for standalone procedures
    block: Option<Node<'a>>,
}

/// Collect all defProc nodes, including standalone procedures (empty class_name).
fn collect_leak_check_procs<'a>(root: Node<'a>, source: &[u8]) -> Vec<LeakCheckProc<'a>> {
    let mut result = Vec::new();
    collect_leak_check_procs_recursive(root, source, &mut result);
    result
}

fn collect_leak_check_procs_recursive<'a>(
    node: Node<'a>,
    source: &[u8],
    out: &mut Vec<LeakCheckProc<'a>>,
) {
    if node.kind() == "defProc" {
        let class_name = crate::rules::field_leak::parse_def_proc(node, source)
            .map(|(cn, _, _, _)| cn)
            .unwrap_or_default();
        let block = crate::rules::field_leak::get_method_block(node);
        out.push(LeakCheckProc { class_name, block });
        return; // Don't recurse inside defProc
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_leak_check_procs_recursive(child, source, out);
    }
}

/// Check blocks that are NOT inside any defProc (e.g., top-level program block).
fn visit_non_proc_blocks(
    node: Node,
    source: &[u8],
    project: &ProjectContext,
    uses: &[String],
    ast_intf_types: &HashSet<String>,
    ctx: &mut LintContext,
) {
    if node.kind() == "defProc" {
        return; // Skip — already handled by the defProc-based path
    }
    if node.kind() == "block" {
        check_block_no_try(node, source, project, uses, ast_intf_types, &[], ctx);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_non_proc_blocks(child, source, project, uses, ast_intf_types, ctx);
    }
}

/// Check whether any descendant of `node` is a `raise` statement.
fn ast_contains_raise(node: Node) -> bool {
    if node.kind() == "raise" {
        return true;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if ast_contains_raise(child) {
            return true;
        }
    }
    false
}
