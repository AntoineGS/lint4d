use pascal_core::node_kind as K;
use tree_sitter::Node;

use crate::config::Config;
use crate::engine::suppress::Suppression;
use crate::rules::helpers::node_text;
use crate::rules::naming::{
    to_camel_case, to_pascal_case, to_upper_snake_case, violates_naming_style,
};
use crate::rules::scope::Scopes;

use super::types::{FixConfig, RenameMap};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Walk declarations and check naming rules to build the rename map.
pub fn build_rename_map(
    root: Node,
    source: &[u8],
    config: &Config,
    suppressions: &[Suppression],
) -> RenameMap {
    let mut map = RenameMap::default();
    let fix_config = FixConfig::from_config(config);

    walk_declarations(root, source, config, suppressions, &fix_config, &mut map);
    map
}

// ---------------------------------------------------------------------------
// Declaration walker
// ---------------------------------------------------------------------------

fn walk_declarations(
    node: Node,
    source: &[u8],
    config: &Config,
    suppressions: &[Suppression],
    fix_config: &FixConfig,
    map: &mut RenameMap,
) {
    match node.kind() {
        K::DECL_TYPE => {
            if let Some(type_node) = node.child_by_field_name("type") {
                match type_node.kind() {
                    K::DECL_CLASS if fix_config.type_prefix => {
                        check_type_prefix(node, source, suppressions, map);
                    }
                    K::DECL_INTF if fix_config.intf_prefix => {
                        check_interface_prefix(node, source, suppressions, map);
                    }
                    _ => {}
                }
            }
        }
        K::DECL_CONST if fix_config.const_naming => {
            if node.child_by_field_name("type").is_none() {
                check_constant_naming(node, source, config, suppressions, map);
            }
        }
        K::DEF_PROC | K::LAMBDA if fix_config.local_var => {
            check_local_var_naming(node, source, config, suppressions, map);
            // Still recurse to find nested procs
        }
        _ => {}
    }

    for child in node.children(&mut node.walk()) {
        walk_declarations(child, source, config, suppressions, fix_config, map);
    }
}

// ---------------------------------------------------------------------------
// Suppression helper
// ---------------------------------------------------------------------------

fn is_suppressed(suppressions: &[Suppression], rule_id: &str, line: usize) -> bool {
    suppressions.iter().any(|s| s.matches(rule_id, line))
}

// ---------------------------------------------------------------------------
// Naming checks
// ---------------------------------------------------------------------------

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

    // Check parameters from the header's declArgs.
    if let Some(header) = proc_node.child_by_field_name("header") {
        for header_child in header.children(&mut header.walk()) {
            if header_child.kind() == K::DECL_ARGS {
                for arg in header_child.children(&mut header_child.walk()) {
                    if arg.kind() == K::DECL_ARG {
                        check_decl_names(
                            &arg,
                            source,
                            style,
                            suppressions,
                            proc_start,
                            proc_end,
                            map,
                        );
                    }
                }
            }
        }
    }

    // Check local variable declarations.
    for child in proc_node.children(&mut proc_node.walk()) {
        if child.kind() != K::DECL_VARS {
            continue;
        }
        for var_child in child.children(&mut child.walk()) {
            if var_child.kind() != K::DECL_VAR {
                continue;
            }
            check_decl_names(
                &var_child,
                source,
                style,
                suppressions,
                proc_start,
                proc_end,
                map,
            );
        }
    }
}

/// Check identifier names in a `declVar` or `declArg` node and add
/// violations to the rename map.
fn check_decl_names(
    decl_node: &Node,
    source: &[u8],
    style: &str,
    suppressions: &[Suppression],
    proc_start: usize,
    proc_end: usize,
    map: &mut RenameMap,
) {
    let count = decl_node.child_count();
    for i in 0..count {
        let id_node = match decl_node.child(i) {
            Some(c) => c,
            None => continue,
        };
        if id_node.kind() != K::IDENTIFIER
            || decl_node.field_name_for_child(i as u32) != Some("name")
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
        map.local
            .insert((proc_start, proc_end, name.to_lowercase()), new_name);
    }
}

// ---------------------------------------------------------------------------
// Scope update
// ---------------------------------------------------------------------------

pub(crate) fn update_scopes(scopes: &mut Scopes, rename_map: &RenameMap) {
    // Update file scope entries
    let file_updates: Vec<(String, String)> = rename_map
        .file
        .iter()
        .map(|(old_lower, new_name)| (old_lower.clone(), new_name.clone()))
        .collect();
    for (old_lower, new_name) in &file_updates {
        scopes.file.remove(old_lower);
        scopes
            .file
            .insert(new_name.to_lowercase(), new_name.clone());
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
