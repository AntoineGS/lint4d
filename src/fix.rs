use std::collections::HashMap;

use tree_sitter::Node;

use crate::config::{Config, RuleSeverityOverride};
use crate::engine::suppress::{parse_suppressions, Suppression};
use crate::engine::{parse_file, FileInfo, FileType};
use crate::rules::helpers::node_text;
use crate::rules::naming::{to_camel_case, to_pascal_case, to_upper_snake_case, violates_naming_style};
use crate::rules::scope::{
    collect_file_scope, collect_method_scope, extract_class_name, is_declaration_position,
    is_dot_rhs, is_inside_inherited, is_inside_module_name, is_inside_typeref, Scopes,
};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct TextEdit {
    start_byte: usize,
    end_byte: usize,
    new_text: String,
}

/// Rename map built from naming rule violations.
#[derive(Debug, Default)]
pub struct RenameMap {
    /// File-scoped renames: lowercase old name → new name.
    pub file: HashMap<String, String>,
    /// Method-scoped renames: (proc_start_byte, proc_end_byte, lowercase old name) → new name.
    pub local: HashMap<(usize, usize, String), String>,
}

/// Context for the current procedure during the edit-collection walk.
struct ProcContext {
    start_byte: usize,
    end_byte: usize,
    method_scope: HashMap<String, String>,
    class_fields: Option<HashMap<String, String>>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Fix naming convention violations in a single file.
///
/// Returns `(new_source_bytes, edit_count)` on success.
/// Returns the original source unchanged (with count 0) for `.dpr`/`.dpk` files.
pub fn fix_file(file: &FileInfo, source: &[u8], config: &Config) -> Result<(Vec<u8>, usize), String> {
    if matches!(file.file_type, FileType::Dpr | FileType::Dpk) {
        return Ok((source.to_vec(), 0));
    }

    let source_str = std::str::from_utf8(source).map_err(|e| format!("invalid UTF-8: {e}"))?;
    let _ = source_str; // validates UTF-8

    let (tree, _parse_errors) = parse_file(file, source)?;
    let root = tree.root_node();

    // Pass 1: build rename map
    let suppressions = parse_suppressions(source);
    let rename_map = build_rename_map(root, source, config, &suppressions);

    // Pass 1.5: update scopes with post-rename names
    let mut scopes = collect_file_scope(root, source);
    update_scopes(&mut scopes, &rename_map);

    // Pass 2: collect edits
    let edits = collect_edits(root, source, &rename_map, &scopes, config);

    // Apply edits
    apply_edits(source, edits)
}

// ---------------------------------------------------------------------------
// Pass 1: build rename map
// ---------------------------------------------------------------------------

/// Walk declarations and check naming rules to build the rename map.
pub fn build_rename_map(
    root: Node,
    source: &[u8],
    config: &Config,
    suppressions: &[Suppression],
) -> RenameMap {
    let mut map = RenameMap::default();

    let type_prefix_on = !matches!(
        config.rule_severity("type-prefix"),
        Some(RuleSeverityOverride::Off)
    );
    let intf_prefix_on = !matches!(
        config.rule_severity("interface-prefix"),
        Some(RuleSeverityOverride::Off)
    );
    let const_naming_on = !matches!(
        config.rule_severity("constant-naming"),
        Some(RuleSeverityOverride::Off)
    );
    let local_var_on = !matches!(
        config.rule_severity("local-variable-naming"),
        Some(RuleSeverityOverride::Off)
    );

    walk_declarations(
        root,
        source,
        config,
        suppressions,
        type_prefix_on,
        intf_prefix_on,
        const_naming_on,
        local_var_on,
        &mut map,
    );
    map
}

#[allow(clippy::too_many_arguments)]
fn walk_declarations(
    node: Node,
    source: &[u8],
    config: &Config,
    suppressions: &[Suppression],
    type_prefix_on: bool,
    intf_prefix_on: bool,
    const_naming_on: bool,
    local_var_on: bool,
    map: &mut RenameMap,
) {
    match node.kind() {
        "declType" => {
            if let Some(type_node) = node.child_by_field_name("type") {
                match type_node.kind() {
                    "declClass" if type_prefix_on => {
                        check_type_prefix(node, source, suppressions, map);
                    }
                    "declIntf" if intf_prefix_on => {
                        check_interface_prefix(node, source, suppressions, map);
                    }
                    _ => {}
                }
            }
        }
        "declConst" if const_naming_on => {
            if node.child_by_field_name("type").is_none() {
                check_constant_naming(node, source, config, suppressions, map);
            }
        }
        "defProc" | "lambda" if local_var_on => {
            check_local_var_naming(node, source, config, suppressions, map);
            // Still recurse to find nested procs
        }
        _ => {}
    }

    for child in node.children(&mut node.walk()) {
        walk_declarations(
            child,
            source,
            config,
            suppressions,
            type_prefix_on,
            intf_prefix_on,
            const_naming_on,
            local_var_on,
            map,
        );
    }
}

fn is_suppressed(suppressions: &[Suppression], rule_id: &str, line: usize) -> bool {
    suppressions.iter().any(|s| s.matches(rule_id, line))
}

fn check_type_prefix(
    decl_type: Node,
    source: &[u8],
    suppressions: &[Suppression],
    map: &mut RenameMap,
) {
    let name_node = match decl_type.child_by_field_name("name") {
        Some(n) => n,
        None => return,
    };
    let name = node_text(name_node, source);
    if name.starts_with('T') || name.starts_with('E') {
        return;
    }
    let line = name_node.start_position().row + 1;
    if is_suppressed(suppressions, "type-prefix", line) {
        return;
    }
    map.file.insert(name.to_lowercase(), format!("T{}", name));
}

fn check_interface_prefix(
    decl_type: Node,
    source: &[u8],
    suppressions: &[Suppression],
    map: &mut RenameMap,
) {
    let name_node = match decl_type.child_by_field_name("name") {
        Some(n) => n,
        None => return,
    };
    let name = node_text(name_node, source);
    if name.starts_with('I') {
        return;
    }
    let line = name_node.start_position().row + 1;
    if is_suppressed(suppressions, "interface-prefix", line) {
        return;
    }
    map.file.insert(name.to_lowercase(), format!("I{}", name));
}

fn check_constant_naming(
    decl_const: Node,
    source: &[u8],
    config: &Config,
    suppressions: &[Suppression],
    map: &mut RenameMap,
) {
    let name_node = match decl_const.child_by_field_name("name") {
        Some(n) => n,
        None => return,
    };
    let name = node_text(name_node, source);
    let style = config.constant_style();

    let (conforms, new_name) = if style == "PascalCase" {
        let ok = name.chars().next().is_some_and(|c| c.is_uppercase());
        (ok, to_pascal_case(&name))
    } else {
        let ok = name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        (ok, to_upper_snake_case(&name))
    };

    if conforms {
        return;
    }
    let line = name_node.start_position().row + 1;
    if is_suppressed(suppressions, "constant-naming", line) {
        return;
    }
    map.file.insert(name.to_lowercase(), new_name);
}

fn check_local_var_naming(
    proc_node: Node,
    source: &[u8],
    config: &Config,
    suppressions: &[Suppression],
    map: &mut RenameMap,
) {
    let style = config.local_variable_style();
    let proc_start = proc_node.start_byte();
    let proc_end = proc_node.end_byte();

    for child in proc_node.children(&mut proc_node.walk()) {
        if child.kind() != "declVars" {
            continue;
        }
        for var_child in child.children(&mut child.walk()) {
            if var_child.kind() != "declVar" {
                continue;
            }
            let count = var_child.child_count();
            for i in 0..count {
                let id_node = match var_child.child(i) {
                    Some(c) => c,
                    None => continue,
                };
                if id_node.kind() != "identifier"
                    || var_child.field_name_for_child(i as u32) != Some("name")
                {
                    continue;
                }
                let name = node_text(id_node, source);
                if !violates_naming_style(&name, style) {
                    continue;
                }
                let line = id_node.start_position().row + 1;
                if is_suppressed(suppressions, "local-variable-naming", line) {
                    continue;
                }
                let new_name = if style == "camelCase" {
                    to_camel_case(&name)
                } else {
                    to_pascal_case(&name)
                };
                map.local.insert(
                    (proc_start, proc_end, name.to_lowercase()),
                    new_name,
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pass 1.5: update scope model
// ---------------------------------------------------------------------------

fn update_scopes(scopes: &mut Scopes, rename_map: &RenameMap) {
    // Update file scope entries
    let file_updates: Vec<(String, String)> = rename_map
        .file
        .iter()
        .map(|(old_lower, new_name)| (old_lower.clone(), new_name.clone()))
        .collect();
    for (old_lower, new_name) in &file_updates {
        scopes.file.remove(old_lower);
        scopes.file.insert(new_name.to_lowercase(), new_name.clone());
    }

    // Update class keys if types were renamed
    let class_updates: Vec<(String, String)> = rename_map
        .file
        .iter()
        .filter(|(old_lower, _)| scopes.classes.contains_key(*old_lower))
        .map(|(old_lower, new_name)| (old_lower.clone(), new_name.to_lowercase()))
        .collect();
    for (old_key, new_key) in class_updates {
        if let Some(fields) = scopes.classes.remove(&old_key) {
            scopes.classes.insert(new_key, fields);
        }
    }
}

// ---------------------------------------------------------------------------
// Pass 2: collect edits
// ---------------------------------------------------------------------------

fn collect_edits(
    root: Node,
    source: &[u8],
    rename_map: &RenameMap,
    scopes: &Scopes,
    config: &Config,
) -> Vec<TextEdit> {
    let casing_enabled = !matches!(
        config.rule_severity("identifier-casing"),
        Some(RuleSeverityOverride::Off)
    );
    let mut edits = Vec::new();
    walk_for_edits(root, source, rename_map, scopes, None, casing_enabled, &mut edits);
    edits
}

fn walk_for_edits(
    node: Node,
    source: &[u8],
    rename_map: &RenameMap,
    scopes: &Scopes,
    proc_ctx: Option<&ProcContext>,
    casing_enabled: bool,
    edits: &mut Vec<TextEdit>,
) {
    if node.kind() == "identifier" {
        resolve_and_emit(node, source, rename_map, scopes, proc_ctx, casing_enabled, edits);
        return;
    }

    // Enter a new procedure scope
    if node.kind() == "defProc" || node.kind() == "lambda" {
        // Inherit outer method scope for nested procs (captures outer locals)
        let mut method_scope = match proc_ctx {
            Some(ctx) => ctx.method_scope.clone(),
            None => HashMap::new(),
        };
        collect_method_scope(node, source, &mut method_scope);

        // Apply local renames for THIS procedure AND enclosing procedures.
        // An outer procedure's rename with range (rps, rpe) applies if this
        // procedure's range is contained within it: rps <= ps && rpe >= pe.
        let ps = node.start_byte();
        let pe = node.end_byte();
        for ((rps, rpe, old_lower), new_name) in &rename_map.local {
            if *rps <= ps && *rpe >= pe {
                method_scope.remove(old_lower);
                method_scope.insert(new_name.to_lowercase(), new_name.clone());
            }
        }

        // Determine class context
        let class_name = extract_class_name(node, source);
        let class_fields = class_name
            .as_ref()
            .and_then(|cn| scopes.classes.get(&cn.to_lowercase()))
            .cloned();

        let ctx = ProcContext {
            start_byte: ps,
            end_byte: pe,
            method_scope,
            class_fields,
        };

        for child in node.children(&mut node.walk()) {
            walk_for_edits(child, source, rename_map, scopes, Some(&ctx), casing_enabled, edits);
        }
        return;
    }

    // Default: recurse into children
    for child in node.children(&mut node.walk()) {
        walk_for_edits(child, source, rename_map, scopes, proc_ctx, casing_enabled, edits);
    }
}

fn resolve_and_emit(
    node: Node,
    source: &[u8],
    rename_map: &RenameMap,
    scopes: &Scopes,
    proc_ctx: Option<&ProcContext>,
    casing_enabled: bool,
    edits: &mut Vec<TextEdit>,
) {
    // Always skip these (for both renames and casing fixes)
    if is_dot_rhs(node) || is_inside_inherited(node) || is_inside_module_name(node) {
        return;
    }

    let text = node_text(node, source);
    let lower = text.to_lowercase();

    // Step 1: Check local rename (innermost matching enclosing procedure).
    // A rename keyed to (rps, rpe) applies if this proc context is contained
    // within that range. Prefer the smallest (innermost) containing range.
    if let Some(ctx) = proc_ctx {
        let mut best_match: Option<&str> = None;
        let mut best_range = usize::MAX;
        for ((rps, rpe, key), new_name) in &rename_map.local {
            if *rps <= ctx.start_byte && *rpe >= ctx.end_byte && *key == lower {
                let range_size = rpe - rps;
                if range_size < best_range {
                    best_range = range_size;
                    best_match = Some(new_name.as_str());
                }
            }
        }
        if let Some(new_name) = best_match {
            if text != new_name {
                edits.push(TextEdit {
                    start_byte: node.start_byte(),
                    end_byte: node.end_byte(),
                    new_text: new_name.to_string(),
                });
            }
            return; // local rename found — don't fall through
        }
    }

    // Step 2: Check file-scoped rename
    if let Some(new_name) = rename_map.file.get(&lower) {
        if text != *new_name {
            edits.push(TextEdit {
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                new_text: new_name.clone(),
            });
        }
        return; // file rename found — don't fall through
    }

    // Step 3: Identifier-casing fix (only if enabled)
    if !casing_enabled {
        return;
    }

    // For casing fixes, skip declaration positions and typerefs
    if is_declaration_position(node) || is_inside_typeref(node) {
        return;
    }

    // Look up in scope chain: method → class fields → file
    let declared = proc_ctx
        .and_then(|ctx| ctx.method_scope.get(&lower))
        .or_else(|| {
            proc_ctx.and_then(|ctx| ctx.class_fields.as_ref().and_then(|cf| cf.get(&lower)))
        })
        .or_else(|| scopes.file.get(&lower));

    if let Some(declared_name) = declared {
        if text != *declared_name {
            edits.push(TextEdit {
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                new_text: declared_name.clone(),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Edit application
// ---------------------------------------------------------------------------

fn apply_edits(source: &[u8], mut edits: Vec<TextEdit>) -> Result<(Vec<u8>, usize), String> {
    if edits.is_empty() {
        return Ok((source.to_vec(), 0));
    }

    // Sort descending by start_byte for bottom-up application
    edits.sort_by(|a, b| b.start_byte.cmp(&a.start_byte));

    // Validate no overlapping edits
    for window in edits.windows(2) {
        // window[0] has a HIGHER start_byte than window[1] (descending sort)
        // Overlap: window[1].end_byte > window[0].start_byte
        if window[1].end_byte > window[0].start_byte {
            return Err(format!(
                "overlapping edits detected at byte offsets {}..{} and {}..{} — skipping file",
                window[0].start_byte, window[0].end_byte,
                window[1].start_byte, window[1].end_byte,
            ));
        }
    }

    let count = edits.len();
    let mut result = source.to_vec();
    for edit in &edits {
        result.splice(edit.start_byte..edit.end_byte, edit.new_text.bytes());
    }

    Ok((result, count))
}
