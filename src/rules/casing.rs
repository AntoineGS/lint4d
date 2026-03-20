use std::collections::HashMap;

use tree_sitter::{Node, Tree};

use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::rules::helpers::node_text;
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
// Data structures for scopes
// ---------------------------------------------------------------------------

/// File-level scope: maps lowercase identifier → declared casing.
type FileScope = HashMap<String, String>;

/// Class fields: maps lowercase class name → (lowercase field name → declared casing).
type ClassFields = HashMap<String, HashMap<String, String>>;

/// All scopes collected during Pass 1.
struct Scopes {
    /// File-level declarations (types, constants, global vars, standalone procs).
    file: FileScope,
    /// Per-class field declarations.
    classes: ClassFields,
}

// ---------------------------------------------------------------------------
// Pass 1: collect declarations
// ---------------------------------------------------------------------------

fn collect_file_scope(root: Node, source: &[u8]) -> Scopes {
    let mut scopes = Scopes {
        file: HashMap::new(),
        classes: HashMap::new(),
    };
    collect_node(root, source, &mut scopes);
    scopes
}

fn collect_node(node: Node, source: &[u8], scopes: &mut Scopes) {
    match node.kind() {
        "declType" => {
            // Collect the type name itself.
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, source);
                scopes.file.insert(name.to_lowercase(), name.clone());

                // If the type is a class/record, collect its fields.
                if let Some(type_node) = node.child_by_field_name("type") {
                    if type_node.kind() == "declClass" || type_node.kind() == "declRecord" {
                        let class_key = name.to_lowercase();
                        let fields = scopes.classes.entry(class_key).or_default();
                        collect_class_fields(type_node, source, fields);
                    }
                }
            }
            // Don't recurse further — class body is handled above.
            return;
        }
        "declConst" => {
            // Only untyped constants (typed ones have a "type" field).
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, source);
                scopes.file.insert(name.to_lowercase(), name);
            }
            return;
        }
        "declVar" => {
            // Only collect file-level vars (not inside defProc/lambda).
            if !is_inside_proc(node) {
                collect_decl_var_names(node, source, &mut scopes.file);
            }
            return;
        }
        "defProc" | "lambda" => {
            // For method implementations, don't add their name to file scope
            // (the declProc inside the interface section already did that, or it's
            // a class method). But do recurse to find nested lambdas etc.
            // We DON'T collect params/locals here — they are collected per-method
            // in Pass 2's check_proc.
            for child in node.children(&mut node.walk()) {
                collect_node(child, source, scopes);
            }
            return;
        }
        "declProc" => {
            // Standalone procedure declaration (not class-qualified).
            // A class-qualified one has a genericDot name: `TFoo.DoWork`.
            // We add the simple name to file scope.
            if let Some(name_node) = node.child_by_field_name("name") {
                if name_node.kind() == "identifier" {
                    // Simple name — file-level procedure
                    let name = node_text(name_node, source);
                    scopes.file.insert(name.to_lowercase(), name);
                }
                // genericDot means class method — skip for file scope.
            }
            return;
        }
        _ => {}
    }

    // Default: recurse into children.
    for child in node.children(&mut node.walk()) {
        collect_node(child, source, scopes);
    }
}

/// Collect all field names from a `declClass` or `declRecord` node.
fn collect_class_fields(class_node: Node, source: &[u8], fields: &mut HashMap<String, String>) {
    for child in class_node.children(&mut class_node.walk()) {
        if child.kind() == "declSection" {
            for item in child.children(&mut child.walk()) {
                if item.kind() == "declField" {
                    // A field can declare multiple names: `A, B: Integer`
                    collect_decl_field_names(item, source, fields);
                }
            }
        }
    }
}

/// Collect all identifier names from a `declField` node.
fn collect_decl_field_names(decl_field: Node, source: &[u8], map: &mut HashMap<String, String>) {
    let count = decl_field.child_count();
    for i in 0..count {
        let child = match decl_field.child(i) {
            Some(c) => c,
            None => continue,
        };
        let field_name = decl_field.field_name_for_child(i as u32);
        if child.kind() == "identifier" && field_name == Some("name") {
            let name = node_text(child, source);
            map.insert(name.to_lowercase(), name);
        }
    }
}

/// Collect all identifier names from a `declVar` node.
fn collect_decl_var_names(decl_var: Node, source: &[u8], map: &mut HashMap<String, String>) {
    let count = decl_var.child_count();
    for i in 0..count {
        let child = match decl_var.child(i) {
            Some(c) => c,
            None => continue,
        };
        let field_name = decl_var.field_name_for_child(i as u32);
        if child.kind() == "identifier" && field_name == Some("name") {
            let name = node_text(child, source);
            map.insert(name.to_lowercase(), name);
        }
    }
}

/// Collect all parameter names from a `declArg` node.
fn collect_decl_arg_names(decl_arg: Node, source: &[u8], map: &mut HashMap<String, String>) {
    let count = decl_arg.child_count();
    for i in 0..count {
        let child = match decl_arg.child(i) {
            Some(c) => c,
            None => continue,
        };
        let field_name = decl_arg.field_name_for_child(i as u32);
        if child.kind() == "identifier" && field_name == Some("name") {
            let name = node_text(child, source);
            map.insert(name.to_lowercase(), name);
        }
    }
}

// ---------------------------------------------------------------------------
// Scope helpers
// ---------------------------------------------------------------------------

/// Returns true if the node has any ancestor that is `defProc` or `lambda`.
fn is_inside_proc(node: Node) -> bool {
    let mut current = node.parent();
    while let Some(p) = current {
        match p.kind() {
            "defProc" | "lambda" => return true,
            _ => {}
        }
        current = p.parent();
    }
    false
}

/// Extract the class name from a `defProc` node.
/// `defProc` > `declProc` > (field=name) `genericDot` > (field=lhs) identifier
fn extract_class_name(def_proc: Node, source: &[u8]) -> Option<String> {
    let header = def_proc.child_by_field_name("header")?;
    let name_node = header.child_by_field_name("name")?;
    if name_node.kind() == "genericDot" {
        let lhs = name_node.child_by_field_name("lhs")?;
        Some(node_text(lhs, source))
    } else {
        None
    }
}

/// Returns true if this identifier is the RHS of an `exprDot` expression.
/// We only skip if the identifier is NOT the lhs field of the exprDot parent.
fn is_dot_rhs(node: Node) -> bool {
    let parent = match node.parent() {
        Some(p) => p,
        None => return false,
    };
    if parent.kind() != "exprDot" {
        return false;
    }
    // Check if this node is the lhs field of the exprDot.
    // If not lhs, it's the rhs — skip it.
    match parent.child_by_field_name("lhs") {
        Some(lhs) => lhs.id() != node.id(),
        None => true, // no lhs found — treat as rhs to be safe
    }
}

/// Returns true if the identifier is inside an `inherited` node.
fn is_inside_inherited(node: Node) -> bool {
    match node.parent() {
        Some(p) => p.kind() == "inherited",
        None => false,
    }
}

/// Returns true if the identifier is inside a `typeref` node.
fn is_inside_typeref(node: Node) -> bool {
    match node.parent() {
        Some(p) => p.kind() == "typeref",
        None => false,
    }
}

/// Returns true if the identifier is inside a `moduleName` node.
fn is_inside_module_name(node: Node) -> bool {
    match node.parent() {
        Some(p) => p.kind() == "moduleName",
        None => false,
    }
}

/// Returns true if the identifier is in a declaration position.
/// These are the positions where the identifier IS the declaration — not a usage.
fn is_declaration_position(node: Node) -> bool {
    let parent = match node.parent() {
        Some(p) => p,
        None => return false,
    };
    match parent.kind() {
        // Direct parent is a declaration node.
        "declVar" | "declConst" | "declType" | "declField" | "declArg" => {
            // Only skip if this identifier is the NAME field, not the type.
            match parent.child_by_field_name("name") {
                Some(name_node) => name_node.id() == node.id(),
                None => false,
            }
        }
        // Inside the name part of a procedure declaration.
        "declProc" => {
            match parent.child_by_field_name("name") {
                Some(name_node) => {
                    if name_node.kind() == "identifier" {
                        name_node.id() == node.id()
                    } else if name_node.kind() == "genericDot" {
                        // Both the lhs and rhs of the genericDot are declaration positions.
                        // lhs = class name, rhs = method name.
                        true
                    } else {
                        false
                    }
                }
                None => false,
            }
        }
        // Inside a genericDot that is the name of a declProc.
        "genericDot" => {
            let grandparent = match parent.parent() {
                Some(gp) => gp,
                None => return false,
            };
            if grandparent.kind() == "declProc" {
                match grandparent.child_by_field_name("name") {
                    Some(name_node) => name_node.id() == parent.id(),
                    None => false,
                }
            } else {
                false
            }
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Pass 2: check usages
// ---------------------------------------------------------------------------

fn check_usages(root: Node, source: &[u8], scopes: &Scopes, ctx: &mut LintContext) {
    // Walk the implementation section, visiting each defProc individually.
    for child in root.children(&mut root.walk()) {
        if child.kind() == "unit" {
            check_unit_usages(child, source, scopes, ctx);
        }
    }
}

fn check_unit_usages(unit: Node, source: &[u8], scopes: &Scopes, ctx: &mut LintContext) {
    for child in unit.children(&mut unit.walk()) {
        if child.kind() == "implementation" {
            for impl_child in child.children(&mut child.walk()) {
                if impl_child.kind() == "defProc" {
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

/// Collect parameters and local vars for a `defProc` or `lambda`.
fn collect_method_scope(proc_node: Node, source: &[u8], map: &mut HashMap<String, String>) {
    // Parameters are in the header's declProc > declArgs.
    if let Some(header) = proc_node.child_by_field_name("header") {
        collect_params_from_header(header, source, map);
    }
    // Local vars are in direct `declVars` children of defProc/lambda.
    for child in proc_node.children(&mut proc_node.walk()) {
        if child.kind() == "declVars" {
            for var_child in child.children(&mut child.walk()) {
                if var_child.kind() == "declVar" {
                    collect_decl_var_names(var_child, source, map);
                }
            }
        }
    }
}

/// Collect parameter names from a `declProc` header node.
fn collect_params_from_header(header: Node, source: &[u8], map: &mut HashMap<String, String>) {
    for child in header.children(&mut header.walk()) {
        if child.kind() == "declArgs" {
            for arg_child in child.children(&mut child.walk()) {
                if arg_child.kind() == "declArg" {
                    collect_decl_arg_names(arg_child, source, map);
                }
            }
        }
    }
}

/// Walk all nodes under `node`, checking identifier usages.
/// For nested `defProc`/`lambda`, recurse with updated scopes.
fn walk_and_check<'a>(
    node: Node<'a>,
    source: &[u8],
    scopes: &Scopes,
    method_scope: &HashMap<String, String>,
    class_fields: Option<&HashMap<String, String>>,
    ctx: &mut LintContext,
) {
    if node.kind() == "identifier" {
        check_identifier_usage(node, source, scopes, method_scope, class_fields, ctx);
        return;
    }

    // For nested lambdas/defProcs, build a fresh method scope that merges
    // outer method scope with the nested one.
    if node.kind() == "defProc" || node.kind() == "lambda" {
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
            });
        }
    }
}
