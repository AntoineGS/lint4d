use std::collections::HashMap;

use pascal_core::node_kind as K;
use tree_sitter::Node;

use crate::rules::helpers::node_text;

// ---------------------------------------------------------------------------
// Data structures for scopes
// ---------------------------------------------------------------------------

/// File-level scope: maps lowercase identifier → declared casing.
pub type FileScope = HashMap<String, String>;

/// Class fields: maps lowercase class name → (lowercase field name → declared casing).
pub type ClassFields = HashMap<String, HashMap<String, String>>;

/// All scopes collected during Pass 1.
pub struct Scopes {
    /// File-level declarations (types, constants, global vars, standalone procs).
    pub file: FileScope,
    /// Per-class field declarations.
    pub classes: ClassFields,
}

// ---------------------------------------------------------------------------
// Pass 1: collect declarations
// ---------------------------------------------------------------------------

pub fn collect_file_scope(root: Node, source: &[u8]) -> Scopes {
    let mut scopes = Scopes {
        file: HashMap::new(),
        classes: HashMap::new(),
    };
    collect_node(root, source, &mut scopes);
    scopes
}

fn collect_node(node: Node, source: &[u8], scopes: &mut Scopes) {
    match node.kind() {
        K::DECL_TYPE => {
            // Collect the type name itself.
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, source);
                scopes.file.insert(name.to_lowercase(), name.clone());

                // If the type is a class/record, collect its fields.
                if let Some(type_node) = node.child_by_field_name("type") {
                    if type_node.kind() == K::DECL_CLASS || type_node.kind() == K::DECL_RECORD {
                        let class_key = name.to_lowercase();
                        let fields = scopes.classes.entry(class_key).or_default();
                        collect_class_fields(type_node, source, fields);
                    }
                }
            }
            // Don't recurse further — class body is handled above.
            return;
        }
        K::DECL_CONST => {
            // Only untyped constants (typed ones have a "type" field).
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, source);
                scopes.file.insert(name.to_lowercase(), name);
            }
            return;
        }
        K::DECL_VAR => {
            // Only collect file-level vars (not inside defProc/lambda).
            if !is_inside_proc(node) {
                collect_decl_var_names(node, source, &mut scopes.file);
            }
            return;
        }
        K::DEF_PROC | K::LAMBDA => {
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
        K::DECL_PROC => {
            // Standalone procedure declaration (not class-qualified).
            // A class-qualified one has a genericDot name: `TFoo.DoWork`.
            // We add the simple name to file scope.
            if let Some(name_node) = node.child_by_field_name("name") {
                if name_node.kind() == K::IDENTIFIER {
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
pub fn collect_class_fields(class_node: Node, source: &[u8], fields: &mut HashMap<String, String>) {
    for child in class_node.children(&mut class_node.walk()) {
        if child.kind() == K::DECL_SECTION {
            for item in child.children(&mut child.walk()) {
                if item.kind() == K::DECL_FIELD {
                    // A field can declare multiple names: `A, B: Integer`
                    collect_decl_field_names(item, source, fields);
                }
            }
        }
    }
}

/// Collect all identifier names from a `declField` node.
pub fn collect_decl_field_names(
    decl_field: Node,
    source: &[u8],
    map: &mut HashMap<String, String>,
) {
    let count = decl_field.child_count();
    for i in 0..count {
        let child = match decl_field.child(i) {
            Some(c) => c,
            None => continue,
        };
        let field_name = decl_field.field_name_for_child(i as u32);
        if child.kind() == K::IDENTIFIER && field_name == Some("name") {
            let name = node_text(child, source);
            map.insert(name.to_lowercase(), name);
        }
    }
}

/// Collect all identifier names from a `declVar` node.
pub fn collect_decl_var_names(decl_var: Node, source: &[u8], map: &mut HashMap<String, String>) {
    let count = decl_var.child_count();
    for i in 0..count {
        let child = match decl_var.child(i) {
            Some(c) => c,
            None => continue,
        };
        let field_name = decl_var.field_name_for_child(i as u32);
        if child.kind() == K::IDENTIFIER && field_name == Some("name") {
            let name = node_text(child, source);
            map.insert(name.to_lowercase(), name);
        }
    }
}

/// Collect all parameter names from a `declArg` node.
pub fn collect_decl_arg_names(decl_arg: Node, source: &[u8], map: &mut HashMap<String, String>) {
    let count = decl_arg.child_count();
    for i in 0..count {
        let child = match decl_arg.child(i) {
            Some(c) => c,
            None => continue,
        };
        let field_name = decl_arg.field_name_for_child(i as u32);
        if child.kind() == K::IDENTIFIER && field_name == Some("name") {
            let name = node_text(child, source);
            map.insert(name.to_lowercase(), name);
        }
    }
}

// ---------------------------------------------------------------------------
// Method scope functions
// ---------------------------------------------------------------------------

/// Collect parameters and local vars for a `defProc` or `lambda`.
pub fn collect_method_scope(proc_node: Node, source: &[u8], map: &mut HashMap<String, String>) {
    // Parameters are in the header's declProc > declArgs.
    if let Some(header) = proc_node.child_by_field_name("header") {
        collect_params_from_header(header, source, map);
    }
    // Local vars are in direct `declVars` children of defProc/lambda.
    for child in proc_node.children(&mut proc_node.walk()) {
        if child.kind() == K::DECL_VARS {
            for var_child in child.children(&mut child.walk()) {
                if var_child.kind() == K::DECL_VAR {
                    collect_decl_var_names(var_child, source, map);
                }
            }
        }
    }
}

/// Collect parameter names from a `declProc` header node.
pub fn collect_params_from_header(header: Node, source: &[u8], map: &mut HashMap<String, String>) {
    for child in header.children(&mut header.walk()) {
        if child.kind() == K::DECL_ARGS {
            for arg_child in child.children(&mut child.walk()) {
                if arg_child.kind() == K::DECL_ARG {
                    collect_decl_arg_names(arg_child, source, map);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Scope helpers
// ---------------------------------------------------------------------------

/// Returns true if the node has any ancestor that is `defProc` or `lambda`.
pub fn is_inside_proc(node: Node) -> bool {
    let mut current = node.parent();
    while let Some(p) = current {
        match p.kind() {
            K::DEF_PROC | K::LAMBDA => return true,
            _ => {}
        }
        current = p.parent();
    }
    false
}

/// Extract the class name from a `defProc` node.
/// `defProc` > `declProc` > (field=name) `genericDot` > (field=lhs) identifier
pub fn extract_class_name(def_proc: Node, source: &[u8]) -> Option<String> {
    let header = def_proc.child_by_field_name("header")?;
    let name_node = header.child_by_field_name("name")?;
    if name_node.kind() == K::GENERIC_DOT {
        let lhs = name_node.child_by_field_name("lhs")?;
        Some(node_text(lhs, source))
    } else {
        None
    }
}

/// Returns true if this identifier is the RHS of an `exprDot` expression.
/// We only skip if the identifier is NOT the lhs field of the exprDot parent.
pub fn is_dot_rhs(node: Node) -> bool {
    let parent = match node.parent() {
        Some(p) => p,
        None => return false,
    };
    if parent.kind() != K::EXPR_DOT {
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
pub fn is_inside_inherited(node: Node) -> bool {
    match node.parent() {
        Some(p) => p.kind() == K::INHERITED,
        None => false,
    }
}

/// Returns true if the identifier is inside a `typeref` node.
pub fn is_inside_typeref(node: Node) -> bool {
    match node.parent() {
        Some(p) => p.kind() == K::TYPEREF,
        None => false,
    }
}

/// Returns true if the identifier is inside a `moduleName` node.
pub fn is_inside_module_name(node: Node) -> bool {
    match node.parent() {
        Some(p) => p.kind() == K::MODULE_NAME,
        None => false,
    }
}

/// Returns true if the identifier is in a declaration position.
/// These are the positions where the identifier IS the declaration — not a usage.
pub fn is_declaration_position(node: Node) -> bool {
    let parent = match node.parent() {
        Some(p) => p,
        None => return false,
    };
    match parent.kind() {
        // Direct parent is a declaration node.
        K::DECL_VAR | K::DECL_CONST | K::DECL_TYPE | K::DECL_FIELD | K::DECL_ARG => {
            // Only skip if this identifier is the NAME field, not the type.
            match parent.child_by_field_name("name") {
                Some(name_node) => name_node.id() == node.id(),
                None => false,
            }
        }
        // Inside the name part of a procedure declaration.
        K::DECL_PROC => {
            match parent.child_by_field_name("name") {
                Some(name_node) => {
                    if name_node.kind() == K::IDENTIFIER {
                        name_node.id() == node.id()
                    } else if name_node.kind() == K::GENERIC_DOT {
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
        K::GENERIC_DOT => {
            let grandparent = match parent.parent() {
                Some(gp) => gp,
                None => return false,
            };
            if grandparent.kind() == K::DECL_PROC {
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
