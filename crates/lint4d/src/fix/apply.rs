use std::collections::{HashMap, HashSet};

use pascal_core::node_kind as K;
use tree_sitter::Node;

use crate::rules::helpers::node_text;
use crate::rules::scope::{
    Scopes, collect_method_scope_bounded, is_declaration_position, is_dot_rhs, is_inside_inherited,
    is_inside_module_name, is_inside_typeref,
};
use std::sync::atomic::AtomicBool;

use super::types::{FixConfig, ProcContext, RenameMap, TextEdit};
use super::{FixWorkBudget, charge_budget, charge_owned_budget};

// ---------------------------------------------------------------------------
// Collect edits
// ---------------------------------------------------------------------------

pub(crate) fn collect_edits_bounded(
    root: Node,
    source: &[u8],
    rename_map: &RenameMap,
    scopes: &Scopes,
    fix_config: &FixConfig,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<TextEdit>, String> {
    collect_edits_with_shadow_barriers(
        root, source, rename_map, scopes, fix_config, false, budget, cancel,
    )
}

pub(crate) fn collect_edits_restricted(
    root: Node,
    source: &[u8],
    rename_map: &RenameMap,
    scopes: &Scopes,
    fix_config: &FixConfig,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<TextEdit>, String> {
    collect_edits_with_shadow_barriers(
        root, source, rename_map, scopes, fix_config, true, budget, cancel,
    )
}

#[allow(clippy::too_many_arguments)]
fn collect_edits_with_shadow_barriers(
    root: Node,
    source: &[u8],
    rename_map: &RenameMap,
    scopes: &Scopes,
    fix_config: &FixConfig,
    shadow_barriers: bool,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<TextEdit>, String> {
    let mut edits = Vec::new();
    walk_for_edits(
        root,
        source,
        rename_map,
        scopes,
        None,
        fix_config.casing,
        shadow_barriers,
        false,
        &mut edits,
        budget,
        cancel,
    )?;
    Ok(edits)
}

#[allow(clippy::too_many_arguments)]
fn walk_for_edits(
    node: Node,
    source: &[u8],
    rename_map: &RenameMap,
    scopes: &Scopes,
    proc_ctx: Option<&ProcContext<'_>>,
    casing_enabled: bool,
    shadow_barriers: bool,
    inside_with_body: bool,
    edits: &mut Vec<TextEdit>,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    super::charge_budget(budget, cancel, 1, 0)?;
    if node.kind() == K::IDENTIFIER {
        if shadow_barriers && inside_with_body {
            return Ok(());
        }
        resolve_and_emit(
            node,
            source,
            rename_map,
            scopes,
            proc_ctx,
            casing_enabled,
            shadow_barriers,
            edits,
            budget,
            cancel,
        )?;
        return Ok(());
    }

    // Enter a new procedure scope
    if node.kind() == K::DEF_PROC || node.kind() == K::LAMBDA {
        // Inherit outer method scope for nested procs (captures outer locals)
        let mut method_scope = match proc_ctx {
            Some(ctx) => {
                charge_string_string_map_clone(&ctx.method_scope, budget, cancel)?;
                ctx.method_scope.clone()
            }
            None => HashMap::new(),
        };
        let mut local_renames = match proc_ctx {
            Some(ctx) => {
                charge_string_string_map_clone(&ctx.local_renames, budget, cancel)?;
                ctx.local_renames.clone()
            }
            None => HashMap::new(),
        };
        let mut local_rename_ranges = match proc_ctx {
            Some(ctx) => {
                charge_string_usize_map_clone(&ctx.local_rename_ranges, budget, cancel)?;
                ctx.local_rename_ranges.clone()
            }
            None => HashMap::new(),
        };
        let mut blocked_local_names = if shadow_barriers {
            match proc_ctx {
                Some(ctx) => {
                    charge_string_hash_set_clone(&ctx.blocked_local_names, budget, cancel)?;
                    ctx.blocked_local_names.clone()
                }
                None => HashSet::new(),
            }
        } else {
            HashSet::new()
        };
        let mut blocked_local_renames = if shadow_barriers {
            match proc_ctx {
                Some(ctx) => {
                    charge_blocked_rename_set_clone(&ctx.blocked_local_renames, budget, cancel)?;
                    ctx.blocked_local_renames.clone()
                }
                None => HashSet::new(),
            }
        } else {
            HashSet::new()
        };
        let mut local_declaration_renames = HashSet::new();
        let mut restricted_shadow_names = HashSet::new();
        if shadow_barriers {
            let mut direct_scope = HashMap::new();
            collect_method_scope_bounded(node, source, &mut direct_scope, budget, cancel)?;
            if node.kind() == K::LAMBDA {
                collect_lambda_argument_scope(node, source, &mut direct_scope, budget, cancel)?;
            }
            let ambiguous_declarations =
                collect_restricted_declaration_shadows(node, source, budget, cancel)?;

            // A direct local/parameter declaration shadows inherited names.
            // Keep its spelling available for ordinary case normalization.
            for (name, declared_name) in &direct_scope {
                insert_string_hash_set(&mut restricted_shadow_names, name, budget, cancel)?;
                blocked_local_names.remove(name);
                charge_hash_map_insert(
                    &method_scope,
                    budget,
                    cancel,
                    name.len(),
                    declared_name.len(),
                )?;
                method_scope.insert(name.clone(), declared_name.clone());
            }

            // Local constants, types, and routines are lexical shadows too,
            // but this walker cannot safely resolve their unqualified uses.
            // Suppress matching edits rather than borrowing an outer local's
            // rename or casing.
            for name in ambiguous_declarations {
                insert_string_hash_set(&mut restricted_shadow_names, &name, budget, cancel)?;
                insert_string_hash_set(&mut blocked_local_names, &name, budget, cancel)?;
            }

            // Remove inherited spelling-based candidates here. An own
            // candidate for this routine is considered below; blocked
            // ancestor candidates are carried to all descendant routines.
            local_renames.retain(|name, _| !restricted_shadow_names.contains(name));
            local_rename_ranges.retain(|name, _| !restricted_shadow_names.contains(name));
        } else {
            collect_method_scope_bounded(node, source, &mut method_scope, budget, cancel)?;
        }

        // Apply local renames for THIS procedure AND enclosing procedures.
        // An outer procedure's rename with range (rps, rpe) applies if this
        // procedure's range is contained within it: rps <= ps && rpe >= pe.
        let ps = node.start_byte();
        let pe = node.end_byte();
        charge_owned_budget(
            budget,
            cancel,
            rename_map
                .local
                .len()
                .saturating_mul(std::mem::size_of::<(&(usize, usize, String), &String)>()),
        )?;
        let mut containing_renames = Vec::with_capacity(rename_map.local.len());
        for entry in &rename_map.local {
            charge_budget(budget, cancel, 1, 0)?;
            if entry.0.0 <= ps && entry.0.1 >= pe {
                containing_renames.push(entry);
            }
        }
        charge_budget(
            budget,
            cancel,
            comparison_sort_work(containing_renames.len()),
            0,
        )?;
        containing_renames.sort_by(|left, right| {
            (left.0.1 - left.0.0, left.0.0, left.0.1, &left.0.2, left.1).cmp(&(
                right.0.1 - right.0.0,
                right.0.0,
                right.0.1,
                &right.0.2,
                right.1,
            ))
        });
        for ((rps, rpe, old_lower), new_name) in containing_renames {
            super::charge_budget(budget, cancel, 1, old_lower.len() + new_name.len())?;
            let rename_key = if shadow_barriers {
                charge_owned_budget(budget, cancel, old_lower.len())?;
                Some((*rps, *rpe, old_lower.clone()))
            } else {
                None
            };
            if rename_key
                .as_ref()
                .is_some_and(|key| blocked_local_renames.contains(key))
            {
                continue;
            }
            if shadow_barriers
                && (*rps, *rpe) != (ps, pe)
                && restricted_shadow_names.contains(old_lower)
            {
                insert_blocked_rename(
                    &mut blocked_local_renames,
                    rename_key.expect("restricted mode materializes local rename keys"),
                    budget,
                    cancel,
                )?;
                continue;
            }
            if shadow_barriers && (*rps, *rpe) == (ps, pe) {
                insert_string_hash_set(&mut local_declaration_renames, old_lower, budget, cancel)?;
            }
            method_scope.remove(old_lower);
            charge_hash_map_insert(
                &method_scope,
                budget,
                cancel,
                lowercase_utf8_len(new_name),
                new_name.len(),
            )?;
            method_scope.insert(new_name.to_lowercase(), new_name.clone());
            let range_size = rpe - rps;
            let replace = local_rename_ranges
                .get(old_lower)
                .is_none_or(|existing| range_size < *existing);
            if replace {
                charge_hash_map_insert(
                    &local_renames,
                    budget,
                    cancel,
                    old_lower.len(),
                    new_name.len(),
                )?;
                charge_hash_map_insert(
                    &local_rename_ranges,
                    budget,
                    cancel,
                    old_lower.len(),
                    std::mem::size_of::<usize>(),
                )?;
                local_renames.insert(old_lower.clone(), new_name.clone());
                local_rename_ranges.insert(old_lower.clone(), range_size);
            }
        }

        // Determine class context
        let class_name = if let Some(name_node) = class_name_node(node) {
            let name_bytes = name_node.end_byte().saturating_sub(name_node.start_byte());
            charge_owned_budget(budget, cancel, decoded_text_capacity(name_bytes))?;
            Some(node_text(name_node, source))
        } else {
            None
        };
        let class_fields = if let Some(class_name) = class_name.as_ref() {
            let key_bytes = lowercase_utf8_len(class_name);
            charge_owned_budget(budget, cancel, key_bytes)?;
            scopes.classes.get(&class_name.to_lowercase())
        } else {
            None
        };

        let ctx = ProcContext {
            method_scope,
            local_renames,
            local_rename_ranges,
            blocked_local_names,
            blocked_local_renames,
            local_declaration_renames,
            class_fields,
        };

        for child in node.children(&mut node.walk()) {
            walk_for_edits(
                child,
                source,
                rename_map,
                scopes,
                Some(&ctx),
                casing_enabled,
                shadow_barriers,
                inside_with_body,
                edits,
                budget,
                cancel,
            )?;
        }
        return Ok(());
    }

    // Default: recurse into children
    let with_body = (shadow_barriers && node.kind() == K::WITH)
        .then(|| node.child_by_field_name("body").map(|body| body.id()))
        .flatten();
    for child in node.children(&mut node.walk()) {
        walk_for_edits(
            child,
            source,
            rename_map,
            scopes,
            proc_ctx,
            casing_enabled,
            shadow_barriers,
            inside_with_body || with_body == Some(child.id()),
            edits,
            budget,
            cancel,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn resolve_and_emit(
    node: Node,
    source: &[u8],
    rename_map: &RenameMap,
    scopes: &Scopes,
    proc_ctx: Option<&ProcContext>,
    casing_enabled: bool,
    shadow_barriers: bool,
    edits: &mut Vec<TextEdit>,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    // Always skip these (for both renames and casing fixes)
    if is_dot_rhs(node) || is_inside_inherited(node) || is_inside_module_name(node) {
        return Ok(());
    }

    let text_bytes = node.end_byte().saturating_sub(node.start_byte());
    charge_owned_budget(budget, cancel, decoded_text_capacity(text_bytes))?;
    let text = node_text(node, source);
    charge_owned_budget(budget, cancel, lowercase_utf8_len(&text))?;
    let lower = text.to_lowercase();

    if shadow_barriers {
        if is_inside_typeref(node) {
            return Ok(());
        }
        let own_local_declaration = node.parent().is_some_and(|parent| {
            matches!(parent.kind(), K::DECL_VAR | K::DECL_ARG)
                && proc_ctx.is_some_and(|ctx| ctx.local_declaration_renames.contains(&lower))
        });
        if is_declaration_position(node) && !own_local_declaration {
            return Ok(());
        }
        if proc_ctx.is_some_and(|ctx| ctx.blocked_local_names.contains(&lower)) {
            return Ok(());
        }
    }

    // Step 1: Check local rename (innermost matching enclosing procedure).
    // A rename keyed to (rps, rpe) applies if this proc context is contained
    // within that range. Prefer the smallest (innermost) containing range.
    if let Some(ctx) = proc_ctx
        && let Some(new_name) = ctx.local_renames.get(&lower)
    {
        if text != *new_name {
            push_text_edit(
                edits,
                node.start_byte(),
                node.end_byte(),
                new_name,
                budget,
                cancel,
            )?;
        }
        return Ok(()); // local rename found — don't fall through
    }

    // Step 2: Check file-scoped rename
    if let Some(new_name) = rename_map.file.get(&lower) {
        if text != *new_name {
            push_text_edit(
                edits,
                node.start_byte(),
                node.end_byte(),
                new_name,
                budget,
                cancel,
            )?;
        }
        return Ok(()); // file rename found — don't fall through
    }

    // Step 3: Identifier-casing fix (only if enabled)
    if !casing_enabled {
        return Ok(());
    }

    // For casing fixes, skip declaration positions and typerefs
    if is_declaration_position(node) || is_inside_typeref(node) {
        return Ok(());
    }

    // Look up in scope chain: method -> class fields -> file
    let declared = proc_ctx
        .and_then(|ctx| ctx.method_scope.get(&lower))
        .or_else(|| {
            proc_ctx.and_then(|ctx| ctx.class_fields.as_ref().and_then(|cf| cf.get(&lower)))
        })
        .or_else(|| scopes.file.get(&lower));

    if let Some(declared_name) = declared
        && text != *declared_name
    {
        push_text_edit(
            edits,
            node.start_byte(),
            node.end_byte(),
            declared_name,
            budget,
            cancel,
        )?;
    }
    Ok(())
}

fn collect_lambda_argument_scope(
    lambda: Node,
    source: &[u8],
    scope: &mut HashMap<String, String>,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    let Some(args) = lambda.child_by_field_name("args") else {
        return Ok(());
    };
    for arg in args.children(&mut args.walk()) {
        charge_budget(budget, cancel, 1, 0)?;
        if arg.kind() != K::DECL_ARG {
            continue;
        }
        for index in 0..arg.child_count() {
            charge_budget(budget, cancel, 1, 0)?;
            let Some(name) = arg.child(index) else {
                continue;
            };
            if name.kind() == K::IDENTIFIER
                && arg.field_name_for_child(index as u32) == Some("name")
            {
                insert_scope_identifier(name, source, scope, budget, cancel)?;
            }
        }
    }
    Ok(())
}

fn insert_scope_identifier(
    node: Node,
    source: &[u8],
    scope: &mut HashMap<String, String>,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    let text_bytes = node.end_byte().saturating_sub(node.start_byte());
    charge_owned_budget(budget, cancel, decoded_text_capacity(text_bytes))?;
    let name = node_text(node, source);
    let lower_len = lowercase_utf8_len(&name);
    charge_owned_budget(budget, cancel, lower_len)?;
    charge_hash_map_insert(scope, budget, cancel, lower_len, name.len())?;
    scope.insert(name.to_lowercase(), name);
    Ok(())
}

fn collect_restricted_declaration_shadows(
    proc_node: Node,
    source: &[u8],
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<HashSet<String>, String> {
    let mut names = HashSet::new();
    if let Some(local) = proc_node.child_by_field_name("local") {
        collect_local_declaration_shadows(local, source, &mut names, budget, cancel)?;
    }
    Ok(names)
}

fn collect_local_declaration_shadows(
    node: Node,
    source: &[u8],
    names: &mut HashSet<String>,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    charge_budget(budget, cancel, 1, 0)?;
    match node.kind() {
        K::DEF_PROC => {
            if let Some(header) = node.child_by_field_name("header")
                && let Some(name) = header.child_by_field_name("name")
            {
                collect_identifier_names(name, source, names, budget, cancel)?;
            }
            return Ok(());
        }
        K::LAMBDA => return Ok(()),
        K::DECL_CONST | K::DECL_TYPE | K::DECL_PROC => {
            if let Some(name) = node.child_by_field_name("name") {
                collect_identifier_names(name, source, names, budget, cancel)?;
            }
            return Ok(());
        }
        _ => {}
    }

    for child in node.children(&mut node.walk()) {
        collect_local_declaration_shadows(child, source, names, budget, cancel)?;
    }
    Ok(())
}

fn collect_identifier_names(
    node: Node,
    source: &[u8],
    names: &mut HashSet<String>,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    charge_budget(budget, cancel, 1, 0)?;
    if node.kind() == K::IDENTIFIER {
        let text_bytes = node.end_byte().saturating_sub(node.start_byte());
        charge_owned_budget(budget, cancel, decoded_text_capacity(text_bytes))?;
        let name = node_text(node, source);
        let lower_len = lowercase_utf8_len(&name);
        charge_owned_budget(budget, cancel, lower_len)?;
        insert_string_hash_set(names, &name.to_lowercase(), budget, cancel)?;
        return Ok(());
    }
    for child in node.children(&mut node.walk()) {
        collect_identifier_names(child, source, names, budget, cancel)?;
    }
    Ok(())
}

fn charge_string_hash_set_clone(
    set: &HashSet<String>,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    charge_budget(budget, cancel, set.len(), 0)?;
    charge_owned_budget(
        budget,
        cancel,
        set.capacity()
            .saturating_mul(std::mem::size_of::<(String, usize)>())
            .saturating_add(set.iter().map(String::len).sum::<usize>()),
    )
}

fn charge_blocked_rename_set_clone(
    set: &HashSet<(usize, usize, String)>,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    charge_budget(budget, cancel, set.len(), 0)?;
    charge_owned_budget(
        budget,
        cancel,
        set.capacity()
            .saturating_mul(std::mem::size_of::<((usize, usize, String), usize)>())
            .saturating_add(set.iter().map(|(_, _, name)| name.len()).sum::<usize>()),
    )
}

fn insert_string_hash_set(
    set: &mut HashSet<String>,
    value: &str,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    charge_budget(budget, cancel, 1, 0)?;
    if set.contains(value) {
        return Ok(());
    }
    let growth = if set.len() == set.capacity() {
        set.capacity()
            .max(1)
            .saturating_mul(2)
            .saturating_mul(std::mem::size_of::<(String, usize)>())
    } else {
        0
    };
    charge_owned_budget(
        budget,
        cancel,
        std::mem::size_of::<String>()
            .saturating_add(value.len())
            .saturating_add(growth),
    )?;
    set.insert(value.to_owned());
    Ok(())
}

fn insert_blocked_rename(
    set: &mut HashSet<(usize, usize, String)>,
    value: (usize, usize, String),
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    charge_budget(budget, cancel, 1, 0)?;
    if set.contains(&value) {
        return Ok(());
    }
    let growth = if set.len() == set.capacity() {
        set.capacity()
            .max(1)
            .saturating_mul(2)
            .saturating_mul(std::mem::size_of::<((usize, usize, String), usize)>())
    } else {
        0
    };
    charge_owned_budget(
        budget,
        cancel,
        std::mem::size_of::<(usize, usize, String)>()
            .saturating_add(value.2.len())
            .saturating_add(growth),
    )?;
    set.insert(value);
    Ok(())
}

fn hash_map_storage_bytes<K, V>(map: &HashMap<K, V>) -> usize {
    map.capacity()
        .saturating_mul(std::mem::size_of::<(K, V)>() + std::mem::size_of::<usize>())
}

fn charge_hash_map_clone<K, V>(
    map: &HashMap<K, V>,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    charge_budget(budget, cancel, map.len(), 0)?;
    charge_owned_budget(budget, cancel, hash_map_storage_bytes(map))
}

fn charge_string_string_map_clone(
    map: &HashMap<String, String>,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    charge_hash_map_clone(map, budget, cancel)?;
    charge_owned_budget(
        budget,
        cancel,
        map.iter()
            .map(|(key, value)| key.len().saturating_add(value.len()))
            .sum(),
    )
}

fn charge_string_usize_map_clone(
    map: &HashMap<String, usize>,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    charge_hash_map_clone(map, budget, cancel)?;
    charge_owned_budget(
        budget,
        cancel,
        map.keys()
            .map(|key| key.len())
            .sum::<usize>()
            .saturating_add(map.len().saturating_mul(std::mem::size_of::<usize>())),
    )
}

fn charge_hash_map_insert<K, V>(
    map: &HashMap<K, V>,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
    key_bytes: usize,
    value_bytes: usize,
) -> Result<(), String> {
    let growth = if map.len() == map.capacity() {
        map.capacity()
            .max(1)
            .saturating_mul(2)
            .saturating_mul(std::mem::size_of::<(K, V)>() + std::mem::size_of::<usize>())
    } else {
        0
    };
    charge_owned_budget(
        budget,
        cancel,
        std::mem::size_of::<(K, V)>()
            .saturating_add(key_bytes)
            .saturating_add(value_bytes)
            .saturating_add(growth),
    )
}

fn push_text_edit(
    edits: &mut Vec<TextEdit>,
    start_byte: usize,
    end_byte: usize,
    new_text: &str,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    charge_budget(budget, cancel, 1, 0)?;
    if edits.len() == edits.capacity() {
        let old_capacity = edits.capacity();
        let new_capacity = old_capacity.max(1).saturating_mul(2);
        let additional = new_capacity.saturating_sub(old_capacity);
        charge_owned_budget(
            budget,
            cancel,
            old_capacity
                .saturating_add(new_capacity)
                .saturating_mul(std::mem::size_of::<TextEdit>()),
        )?;
        edits
            .try_reserve_exact(additional)
            .map_err(|error| format!("could not reserve bounded fix edit: {error}"))?;
    }
    charge_owned_budget(budget, cancel, new_text.len())?;
    edits.push(TextEdit {
        start_byte,
        end_byte,
        new_text: new_text.to_owned(),
    });
    Ok(())
}

fn comparison_sort_work(length: usize) -> usize {
    if length < 2 {
        return 0;
    }
    let mut remaining = length;
    let mut depth = 0;
    while remaining > 1 {
        depth += 1;
        remaining = remaining.saturating_add(1) / 2;
    }
    length.saturating_mul(depth)
}

fn class_name_node(def_proc: Node) -> Option<Node> {
    let header = def_proc.child_by_field_name("header")?;
    let name_node = header.child_by_field_name("name")?;
    (name_node.kind() == K::GENERIC_DOT).then(|| name_node.child_by_field_name("lhs"))?
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

// ---------------------------------------------------------------------------
// Edit application
// ---------------------------------------------------------------------------

pub(crate) fn apply_edits(
    source: &[u8],
    mut edits: Vec<TextEdit>,
) -> Result<(Vec<u8>, usize), String> {
    if edits.is_empty() {
        return Ok((source.to_vec(), 0));
    }

    // Sort descending by start_byte for bottom-up application
    edits.sort_by_key(|e| std::cmp::Reverse(e.start_byte));

    // Validate no overlapping edits
    for window in edits.windows(2) {
        // window[0] has a HIGHER start_byte than window[1] (descending sort)
        // Overlap: window[1].end_byte > window[0].start_byte
        if window[1].end_byte > window[0].start_byte {
            return Err(format!(
                "overlapping edits detected at byte offsets {}..{} and {}..{} — skipping file",
                window[0].start_byte, window[0].end_byte, window[1].start_byte, window[1].end_byte,
            ));
        }
    }

    for edit in &edits {
        if edit.start_byte > edit.end_byte {
            return Err(format!(
                "invalid edit range {}..{} — start exceeds end",
                edit.start_byte, edit.end_byte,
            ));
        }
        if edit.end_byte > source.len() {
            return Err(format!(
                "edit range {}..{} exceeds source length {}",
                edit.start_byte,
                edit.end_byte,
                source.len(),
            ));
        }
    }

    let count = edits.len();
    let output_len = edits.iter().try_fold(source.len(), |length, edit| {
        length
            .checked_sub(edit.end_byte - edit.start_byte)
            .and_then(|length| length.checked_add(edit.new_text.len()))
            .ok_or_else(|| "edited source length overflow".to_string())
    })?;
    let mut result = Vec::new();
    result
        .try_reserve_exact(output_len)
        .map_err(|error| format!("could not reserve edited source: {error}"))?;

    // The edits are sorted descending, so reverse iteration constructs the
    // result in source order without repeatedly moving the untouched suffix.
    let mut cursor = 0;
    for edit in edits.iter().rev() {
        result.extend_from_slice(&source[cursor..edit.start_byte]);
        result.extend_from_slice(edit.new_text.as_bytes());
        cursor = edit.end_byte;
    }
    result.extend_from_slice(&source[cursor..]);
    debug_assert_eq!(result.len(), output_len);

    Ok((result, count))
}

#[cfg(test)]
mod tests {
    use super::{TextEdit, apply_edits};

    fn edit(start_byte: usize, end_byte: usize, new_text: &str) -> TextEdit {
        TextEdit {
            start_byte,
            end_byte,
            new_text: new_text.to_owned(),
        }
    }

    #[test]
    fn applying_no_edits_copies_source_and_reports_zero() {
        assert_eq!(
            apply_edits(b"source", Vec::new()).unwrap(),
            (b"source".to_vec(), 0)
        );
    }

    #[test]
    fn applies_unsorted_adjacent_edits_and_boundary_insertion() {
        let edits = vec![
            edit(4, 6, ""),
            edit(3, 3, "X"),
            edit(0, 2, "A"),
            edit(2, 3, "B"),
        ];

        assert_eq!(
            apply_edits(b"abcdef", edits).unwrap(),
            (b"ABXd".to_vec(), 4)
        );
    }

    #[test]
    fn applies_replacements_that_grow_and_shrink_the_source() {
        let edits = vec![edit(4, 5, ""), edit(1, 2, "long")];

        assert_eq!(
            apply_edits(b"012345", edits).unwrap(),
            (b"0long235".to_vec(), 2)
        );
    }

    #[test]
    fn same_offset_insertions_keep_bottom_up_splice_order() {
        let edits = vec![edit(1, 1, "X"), edit(1, 1, "Y")];

        assert_eq!(apply_edits(b"ab", edits).unwrap(), (b"aYXb".to_vec(), 2));
    }

    #[test]
    fn replacement_then_equal_offset_insertions_keep_stable_splice_order() {
        let edits = vec![edit(1, 2, "R"), edit(1, 1, "X"), edit(1, 1, "Y")];

        assert_eq!(
            apply_edits(b"abcd", edits).unwrap(),
            (b"aYXRcd".to_vec(), 3)
        );
    }

    #[test]
    fn insertion_before_equal_offset_replacement_keeps_overlap_error_offsets() {
        let edits = vec![edit(1, 1, "X"), edit(1, 2, "R")];

        assert_eq!(
            apply_edits(b"abcd", edits).unwrap_err(),
            "overlapping edits detected at byte offsets 1..1 and 1..2 — skipping file"
        );
    }

    #[test]
    fn overlapping_edits_keep_the_existing_error_and_offset_order() {
        let edits = vec![edit(2, 4, "Y"), edit(3, 5, "X")];

        let error = apply_edits(b"abcdef", edits).unwrap_err();
        assert_eq!(
            error,
            "overlapping edits detected at byte offsets 3..5 and 2..4 — skipping file"
        );
    }

    #[test]
    fn rejects_reversed_edit_range_without_panicking() {
        let outcome = std::panic::catch_unwind(|| apply_edits(b"abc", vec![edit(2, 1, "x")]));

        assert!(outcome.is_ok(), "invalid edit range panicked");
        assert!(
            outcome
                .unwrap()
                .unwrap_err()
                .contains("invalid edit range 2..1")
        );
    }

    #[test]
    fn rejects_edit_past_source_end_without_panicking() {
        let outcome = std::panic::catch_unwind(|| apply_edits(b"abc", vec![edit(2, 4, "x")]));

        assert!(outcome.is_ok(), "out-of-bounds edit range panicked");
        assert!(
            outcome
                .unwrap()
                .unwrap_err()
                .contains("edit range 2..4 exceeds source length 3")
        );
    }

    #[test]
    fn applies_ten_thousand_edits_to_a_five_mib_source() {
        let source = vec![b'a'; 5 * 1024 * 1024];
        let edit_count = 10_000;
        let edits = (0..edit_count)
            .map(|index| {
                let start_byte = index * 512;
                edit(start_byte, start_byte + 1, "bb")
            })
            .collect();
        let mut expected = Vec::with_capacity(source.len() + edit_count);
        let mut cursor = 0;
        for index in 0..edit_count {
            let start_byte = index * 512;
            expected.extend_from_slice(&source[cursor..start_byte]);
            expected.extend_from_slice(b"bb");
            cursor = start_byte + 1;
        }
        expected.extend_from_slice(&source[cursor..]);

        assert_eq!(apply_edits(&source, edits).unwrap(), (expected, edit_count));
    }
}
