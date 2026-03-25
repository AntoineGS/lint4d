use std::collections::HashSet;

use tree_sitter::Node;

use crate::engine::FileInfo;
use crate::rules::helpers::{
    extract_unit_name, extract_uses_clauses, is_constructor_call, node_text,
};

/// Registry of factory functions discovered by pre-pass source analysis.
///
/// A factory function is one whose body ultimately returns a newly-constructed
/// object (directly via `Result := TFoo.Create` or indirectly via calling
/// another registered factory).
pub struct SourceContext {
    /// (unit_name_lowercase, function_name_lowercase) pairs.
    factory_functions: HashSet<(String, String)>,
}

/// Metadata extracted from a single function/procedure definition.
struct FuncMeta {
    /// Lowercased unit name where the function lives.
    unit: String,
    /// Lowercased function name (unqualified).
    name: String,
    /// Units referenced via `uses` clauses (original casing).
    uses: Vec<String>,
    /// True if any `Result := TFoo.Create` appears in the body.
    has_direct_constructor: bool,
    /// Lowercased names extracted from `Result := SomeFunc(...)` assignments
    /// that are NOT constructor calls. These are candidate indirect factories.
    result_callees: Vec<String>,
}

impl SourceContext {
    /// Build the factory registry from pre-read source files.
    ///
    /// Each entry is `(&FileInfo, &[u8])` — the file metadata and its source bytes.
    /// Files are parsed, function metadata extracted, and a fixed-point algorithm
    /// resolves direct and indirect factories. ASTs are discarded after extraction.
    pub fn build(files: &[(&FileInfo, &[u8])]) -> Self {
        let mut all_metas: Vec<FuncMeta> = Vec::new();

        // Phase 1: Parse each file and extract function metadata.
        for &(file_info, source) in files {
            let (tree, _) = match crate::engine::parse_file(file_info, source) {
                Ok(result) => result,
                Err(_) => continue,
            };
            let root = tree.root_node();
            let unit_name = extract_unit_name(root, source)
                .unwrap_or_default()
                .to_lowercase();
            let uses = extract_uses_clauses(root, source);

            collect_func_metas(root, source, &unit_name, &uses, &mut all_metas);
        }

        // Phase 2: Seed direct factories.
        let mut factory_set: HashSet<(String, String)> = HashSet::new();
        for meta in &all_metas {
            if meta.has_direct_constructor {
                factory_set.insert((meta.unit.clone(), meta.name.clone()));
            }
        }

        // Phase 3: Fixed-point expansion for indirect factories.
        // A function `F` in unit `U` is an indirect factory if it has
        // `Result := SomeFunc(...)` and SomeFunc is a registered factory
        // (resolved via U's own unit or its uses clauses).
        for _iteration in 0..20 {
            let mut changed = false;
            for meta in &all_metas {
                // Skip if already registered.
                if factory_set.contains(&(meta.unit.clone(), meta.name.clone())) {
                    continue;
                }
                for callee in &meta.result_callees {
                    // Check current unit first.
                    if factory_set.contains(&(meta.unit.clone(), callee.clone())) {
                        factory_set.insert((meta.unit.clone(), meta.name.clone()));
                        changed = true;
                        break;
                    }
                    // Check uses clauses in reverse order (Delphi semantics).
                    let mut found = false;
                    for u in meta.uses.iter().rev() {
                        if factory_set.contains(&(u.to_lowercase(), callee.clone())) {
                            factory_set.insert((meta.unit.clone(), meta.name.clone()));
                            changed = true;
                            found = true;
                            break;
                        }
                    }
                    if found {
                        break;
                    }
                }
            }
            if !changed {
                break;
            }
        }

        SourceContext {
            factory_functions: factory_set,
        }
    }

    /// Check if a function in a given unit is a registered factory.
    pub fn is_factory(&self, unit_name: &str, function_name: &str) -> bool {
        self.factory_functions
            .contains(&(unit_name.to_lowercase(), function_name.to_lowercase()))
    }
}

/// Recursively collect `FuncMeta` entries from all `defProc` nodes in the AST.
fn collect_func_metas(
    node: Node,
    source: &[u8],
    unit_name: &str,
    uses: &[String],
    out: &mut Vec<FuncMeta>,
) {
    if node.kind() == "defProc" {
        if let Some(meta) = extract_func_meta(node, source, unit_name, uses) {
            out.push(meta);
        }
        // Also recurse into nested procs inside defProc.
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect_func_metas(child, source, unit_name, uses, out);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_func_metas(child, source, unit_name, uses, out);
    }
}

/// Extract metadata from a single `defProc` node.
fn extract_func_meta(
    def_proc: Node,
    source: &[u8],
    unit_name: &str,
    uses: &[String],
) -> Option<FuncMeta> {
    let proc_name = extract_proc_name(def_proc, source)?;

    // Find the block (body) of the procedure.
    let block = def_proc
        .children(&mut def_proc.walk())
        .find(|c| c.kind() == "block")?;

    let mut has_direct_constructor = false;
    let mut result_callees: Vec<String> = Vec::new();

    // Scan all assignments in the block for `Result := ...` patterns.
    scan_assignments(
        block,
        source,
        &mut has_direct_constructor,
        &mut result_callees,
    );

    Some(FuncMeta {
        unit: unit_name.to_string(),
        name: proc_name.to_lowercase(),
        uses: uses.to_vec(),
        has_direct_constructor,
        result_callees,
    })
}

/// Extract the unqualified function name from a `defProc` node.
///
/// For `TFoo.Bar` returns `Bar`. For standalone `Baz` returns `Baz`.
fn extract_proc_name(def_proc: Node, source: &[u8]) -> Option<String> {
    // Look for declProc child first.
    let decl_proc = def_proc
        .children(&mut def_proc.walk())
        .find(|c| c.kind() == "declProc")?;

    // Try `name` field first.
    if let Some(name_node) = decl_proc.child_by_field_name("name") {
        return Some(node_text(name_node, source));
    }

    // Fall back: look for genericDot or exprDot (qualified name like TFoo.Bar).
    let mut cursor = decl_proc.walk();
    for child in decl_proc.children(&mut cursor) {
        match child.kind() {
            "genericDot" | "exprDot" => {
                // The last identifier in the dot expression is the method name.
                let idents: Vec<Node> = child
                    .children(&mut child.walk())
                    .filter(|c| c.kind() == "identifier")
                    .collect();
                if let Some(last) = idents.last() {
                    return Some(node_text(*last, source));
                }
            }
            "identifier" => {
                return Some(node_text(child, source));
            }
            _ => {}
        }
    }

    None
}

/// Recursively scan a node for `Result := ...` assignment patterns.
fn scan_assignments(
    node: Node,
    source: &[u8],
    has_direct_constructor: &mut bool,
    result_callees: &mut Vec<String>,
) {
    if node.kind() == "assignment" {
        if let Some(lhs) = node.child_by_field_name("lhs") {
            let lhs_text = node_text(lhs, source);
            if lhs_text.eq_ignore_ascii_case("result") {
                if let Some(rhs) = node.child_by_field_name("rhs") {
                    if is_constructor_call(rhs, source) {
                        *has_direct_constructor = true;
                    } else if let Some(callee) = extract_callee_name(rhs, source) {
                        result_callees.push(callee.to_lowercase());
                    }
                }
            }
        }
    }

    // Recurse into children (to find assignments in nested blocks, if/else, etc.)
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        scan_assignments(child, source, has_direct_constructor, result_callees);
    }
}

/// Extract the simple function name from a RHS expression.
///
/// - `identifier` -> the name itself
/// - `exprCall` wrapping `identifier` -> the identifier text
/// - `exprDot` with two identifiers -> the RHS identifier (dotted name resolution)
/// - Anything else -> None (too complex)
fn extract_callee_name(node: Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => Some(node_text(node, source)),
        "exprCall" => {
            let entity = node.child_by_field_name("entity")?;
            extract_callee_name(entity, source)
        }
        "exprDot" => {
            let lhs = node.child_by_field_name("lhs")?;
            let rhs = node.child_by_field_name("rhs")?;
            if lhs.kind() == "identifier" && rhs.kind() == "identifier" {
                // e.g. Unit.Func — return just the function name for resolution
                Some(node_text(rhs, source))
            } else {
                None
            }
        }
        _ => None,
    }
}
