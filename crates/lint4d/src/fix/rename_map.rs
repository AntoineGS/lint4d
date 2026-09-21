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
use super::{FixWorkBudget, charge_budget, charge_owned_budget};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;

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
    let mut budget = None;
    build_rename_map_bounded(root, source, config, suppressions, &mut budget, None)
        .expect("unbounded naming fix traversal cannot exhaust a budget")
}

pub(crate) fn build_rename_map_bounded(
    root: Node,
    source: &[u8],
    config: &Config,
    suppressions: &[Suppression],
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<RenameMap, String> {
    charge_owned_budget(budget, cancel, std::mem::size_of::<RenameMap>())?;
    let mut map = RenameMap::default();
    let fix_config = FixConfig::from_config(config);
    walk_declarations(
        root,
        source,
        config,
        suppressions,
        &fix_config,
        &mut map,
        budget,
        cancel,
    )?;
    Ok(map)
}

pub(crate) fn build_rename_map_for_rules_bounded(
    root: Node,
    source: &[u8],
    config: &Config,
    suppressions: &[Suppression],
    rules: &[&str],
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<RenameMap, String> {
    charge_owned_budget(budget, cancel, std::mem::size_of::<RenameMap>())?;
    let mut map = RenameMap::default();
    let fix_config = FixConfig::for_rules(config, rules);
    walk_declarations(
        root,
        source,
        config,
        suppressions,
        &fix_config,
        &mut map,
        budget,
        cancel,
    )?;
    Ok(map)
}

// ---------------------------------------------------------------------------
// Declaration walker
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn walk_declarations(
    node: Node,
    source: &[u8],
    config: &Config,
    suppressions: &[Suppression],
    fix_config: &FixConfig,
    map: &mut RenameMap,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    super::charge_budget(budget, cancel, 1, 0)?;
    match node.kind() {
        K::DECL_TYPE => {
            if let Some(type_node) = node.child_by_field_name("type") {
                match type_node.kind() {
                    K::DECL_CLASS if fix_config.type_prefix => {
                        check_type_prefix(node, source, suppressions, map, budget, cancel)?;
                    }
                    K::DECL_INTF if fix_config.intf_prefix => {
                        check_interface_prefix(node, source, suppressions, map, budget, cancel)?;
                    }
                    _ => {}
                }
            }
        }
        K::DECL_CONST if fix_config.const_naming => {
            if node.child_by_field_name("type").is_none() {
                check_constant_naming(node, source, config, suppressions, map, budget, cancel)?;
            }
        }
        K::DEF_PROC | K::LAMBDA if fix_config.local_var => {
            check_local_var_naming(node, source, config, suppressions, map, budget, cancel)?;
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
            fix_config,
            map,
            budget,
            cancel,
        )?;
    }
    Ok(())
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
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    let name_node = match decl_type.child_by_field_name("name") {
        Some(n) => n,
        None => return Ok(()),
    };
    let name_bytes = name_node.end_byte().saturating_sub(name_node.start_byte());
    charge_owned_budget(budget, cancel, decoded_text_capacity(name_bytes))?;
    let name = node_text(name_node, source);
    if name.starts_with('T') || name.starts_with('E') {
        return Ok(());
    }
    let line = name_node.start_position().row + 1;
    if is_suppressed(suppressions, "type-prefix", line) {
        return Ok(());
    }
    let new_name_bytes = name.len().saturating_add(1);
    charge_string_map_insert(
        &map.file,
        budget,
        cancel,
        lowercase_utf8_len(&name),
        new_name_bytes,
    )?;
    let new_name = format!("T{}", name);
    map.file.insert(name.to_lowercase(), new_name);
    Ok(())
}

fn check_interface_prefix(
    decl_type: Node,
    source: &[u8],
    suppressions: &[Suppression],
    map: &mut RenameMap,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    let name_node = match decl_type.child_by_field_name("name") {
        Some(n) => n,
        None => return Ok(()),
    };
    let name_bytes = name_node.end_byte().saturating_sub(name_node.start_byte());
    charge_owned_budget(budget, cancel, decoded_text_capacity(name_bytes))?;
    let name = node_text(name_node, source);
    if name.starts_with('I') {
        return Ok(());
    }
    let line = name_node.start_position().row + 1;
    if is_suppressed(suppressions, "interface-prefix", line) {
        return Ok(());
    }
    let new_name_bytes = name.len().saturating_add(1);
    charge_string_map_insert(
        &map.file,
        budget,
        cancel,
        lowercase_utf8_len(&name),
        new_name_bytes,
    )?;
    let new_name = format!("I{}", name);
    map.file.insert(name.to_lowercase(), new_name);
    Ok(())
}

fn check_constant_naming(
    decl_const: Node,
    source: &[u8],
    config: &Config,
    suppressions: &[Suppression],
    map: &mut RenameMap,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    let name_node = match decl_const.child_by_field_name("name") {
        Some(n) => n,
        None => return Ok(()),
    };
    let name_bytes = name_node.end_byte().saturating_sub(name_node.start_byte());
    charge_owned_budget(budget, cancel, decoded_text_capacity(name_bytes))?;
    let name = node_text(name_node, source);
    let style = config.constant_style();

    let conforms = if style == "PascalCase" {
        let ok = name.chars().next().is_some_and(|c| c.is_uppercase());
        ok
    } else {
        let ok = name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        ok
    };

    if conforms {
        return Ok(());
    }
    let line = name_node.start_position().row + 1;
    if is_suppressed(suppressions, "constant-naming", line) {
        return Ok(());
    }
    charge_string_map_insert(
        &map.file,
        budget,
        cancel,
        lowercase_utf8_len(&name),
        max_generated_name_bytes(name.len()),
    )?;
    charge_owned_budget(budget, cancel, naming_workspace_bytes(name.len()))?;
    let new_name = if style == "PascalCase" {
        to_pascal_case(&name)
    } else {
        to_upper_snake_case(&name)
    };
    map.file.insert(name.to_lowercase(), new_name);
    Ok(())
}

fn check_local_var_naming(
    proc_node: Node,
    source: &[u8],
    config: &Config,
    suppressions: &[Suppression],
    map: &mut RenameMap,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
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
                            budget,
                            cancel,
                        )?;
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
                budget,
                cancel,
            )?;
        }
    }
    Ok(())
}

/// Check identifier names in a `declVar` or `declArg` node and add
/// violations to the rename map.
#[allow(clippy::too_many_arguments)]
fn check_decl_names(
    decl_node: &Node,
    source: &[u8],
    style: &str,
    suppressions: &[Suppression],
    proc_start: usize,
    proc_end: usize,
    map: &mut RenameMap,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    let count = decl_node.child_count();
    for i in 0..count {
        let id_node = match decl_node.child(i) {
            Some(c) => c,
            None => continue,
        };
        charge_budget(budget, cancel, 1, 0)?;
        if id_node.kind() != K::IDENTIFIER
            || decl_node.field_name_for_child(i as u32) != Some("name")
        {
            continue;
        }
        let name_bytes = id_node.end_byte().saturating_sub(id_node.start_byte());
        charge_owned_budget(budget, cancel, decoded_text_capacity(name_bytes))?;
        let name = node_text(id_node, source);
        if !violates_naming_style(&name, style) {
            continue;
        }
        let line = id_node.start_position().row + 1;
        if is_suppressed(suppressions, "local-variable-naming", line) {
            continue;
        }
        charge_local_map_insert(
            &map.local,
            budget,
            cancel,
            lowercase_utf8_len(&name).saturating_add(std::mem::size_of::<usize>() * 2),
            max_generated_name_bytes(name.len()),
        )?;
        charge_owned_budget(budget, cancel, naming_workspace_bytes(name.len()))?;
        let new_name = if style == "camelCase" {
            to_camel_case(&name)
        } else {
            to_pascal_case(&name)
        };
        map.local
            .insert((proc_start, proc_end, name.to_lowercase()), new_name);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Scope update
// ---------------------------------------------------------------------------

pub(crate) fn update_scopes_bounded(
    scopes: &mut Scopes,
    rename_map: &RenameMap,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    // Update file scope entries
    charge_owned_budget(
        budget,
        cancel,
        rename_map
            .file
            .len()
            .saturating_mul(std::mem::size_of::<(String, String)>()),
    )?;
    let mut file_updates = Vec::with_capacity(rename_map.file.len());
    for (old_lower, new_name) in &rename_map.file {
        charge_budget(budget, cancel, 1, 0)?;
        charge_owned_budget(budget, cancel, old_lower.len() + new_name.len())?;
        file_updates.push((old_lower.clone(), new_name.clone()));
    }
    for (old_lower, new_name) in &file_updates {
        super::charge_budget(budget, cancel, 1, old_lower.len() + new_name.len())?;
        scopes.file.remove(old_lower);
        scopes
            .file
            .insert(new_name.to_lowercase(), new_name.clone());
    }

    // Update class keys if types were renamed
    charge_owned_budget(
        budget,
        cancel,
        rename_map
            .file
            .len()
            .saturating_mul(std::mem::size_of::<(String, String)>()),
    )?;
    let mut class_updates = Vec::with_capacity(rename_map.file.len());
    for (old_lower, new_name) in &rename_map.file {
        charge_budget(budget, cancel, 1, 0)?;
        if scopes.classes.contains_key(old_lower) {
            charge_owned_budget(
                budget,
                cancel,
                old_lower
                    .len()
                    .saturating_add(new_name.len())
                    .saturating_add(lowercase_utf8_len(new_name)),
            )?;
            class_updates.push((old_lower.clone(), new_name.to_lowercase()));
        }
    }
    for (old_key, new_key) in class_updates {
        super::charge_budget(budget, cancel, 1, old_key.len() + new_key.len())?;
        if let Some(fields) = scopes.classes.remove(&old_key) {
            scopes.classes.insert(new_key, fields);
        }
    }
    Ok(())
}

fn charge_string_map_insert(
    map: &HashMap<String, String>,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
    key_bytes: usize,
    value_bytes: usize,
) -> Result<(), String> {
    charge_owned_budget(
        budget,
        cancel,
        std::mem::size_of::<(String, String)>()
            .saturating_add(key_bytes)
            .saturating_add(value_bytes)
            .saturating_add(hash_map_growth_bytes::<String, String>(map)),
    )
}

fn charge_local_map_insert(
    map: &HashMap<(usize, usize, String), String>,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
    key_bytes: usize,
    value_bytes: usize,
) -> Result<(), String> {
    charge_owned_budget(
        budget,
        cancel,
        std::mem::size_of::<((usize, usize, String), String)>()
            .saturating_add(key_bytes)
            .saturating_add(value_bytes)
            .saturating_add(hash_map_growth_bytes::<(usize, usize, String), String>(map)),
    )
}

fn hash_map_growth_bytes<K, V>(map: &HashMap<K, V>) -> usize {
    if map.len() == map.capacity() {
        map.capacity()
            .max(1)
            .saturating_mul(2)
            .saturating_mul(std::mem::size_of::<(K, V)>() + std::mem::size_of::<usize>())
    } else {
        0
    }
}

fn decoded_text_capacity(bytes: usize) -> usize {
    bytes.saturating_mul(2)
}

fn lowercase_utf8_len(text: &str) -> usize {
    text.chars()
        .flat_map(|character| character.to_lowercase())
        .map(char::len_utf8)
        .sum()
}

fn max_generated_name_bytes(input_bytes: usize) -> usize {
    input_bytes.saturating_mul(2).max(1)
}

fn naming_workspace_bytes(input_bytes: usize) -> usize {
    input_bytes.saturating_mul(8)
}
