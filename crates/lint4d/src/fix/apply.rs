use std::collections::HashMap;

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
    let mut edits = Vec::new();
    walk_for_edits(
        root,
        source,
        rename_map,
        scopes,
        None,
        fix_config.casing,
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
    edits: &mut Vec<TextEdit>,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    super::charge_budget(budget, cancel, 1, 0)?;
    if node.kind() == K::IDENTIFIER {
        resolve_and_emit(
            node,
            source,
            rename_map,
            scopes,
            proc_ctx,
            casing_enabled,
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
        collect_method_scope_bounded(node, source, &mut method_scope, budget, cancel)?;

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
                edits,
                budget,
                cancel,
            )?;
        }
        return Ok(());
    }

    // Default: recurse into children
    for child in node.children(&mut node.walk()) {
        walk_for_edits(
            child,
            source,
            rename_map,
            scopes,
            proc_ctx,
            casing_enabled,
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

    // Step 1: Check local rename (innermost matching enclosing procedure).
    // A rename keyed to (rps, rpe) applies if this proc context is contained
    // within that range. Prefer the smallest (innermost) containing range.
    if let Some(ctx) = proc_ctx {
        if let Some(new_name) = ctx.local_renames.get(&lower) {
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

    if let Some(declared_name) = declared {
        if text != *declared_name {
            push_text_edit(
                edits,
                node.start_byte(),
                node.end_byte(),
                declared_name,
                budget,
                cancel,
            )?;
        }
    }
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
        map.iter()
            .map(|(key, _)| key.len())
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
    edits.sort_by(|a, b| b.start_byte.cmp(&a.start_byte));

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

    let count = edits.len();
    let mut result = source.to_vec();
    for edit in &edits {
        result.splice(edit.start_byte..edit.end_byte, edit.new_text.bytes());
    }

    Ok((result, count))
}
