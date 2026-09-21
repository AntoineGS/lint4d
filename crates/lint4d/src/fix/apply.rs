use std::collections::HashMap;

use pascal_core::node_kind as K;
use tree_sitter::Node;

use crate::rules::helpers::node_text;
use crate::rules::scope::{
    Scopes, collect_method_scope, extract_class_name, is_declaration_position, is_dot_rhs,
    is_inside_inherited, is_inside_module_name, is_inside_typeref,
};
use std::sync::atomic::AtomicBool;

use super::FixWorkBudget;
use super::types::{FixConfig, ProcContext, RenameMap, TextEdit};

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
    proc_ctx: Option<&ProcContext>,
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
            Some(ctx) => ctx.method_scope.clone(),
            None => HashMap::new(),
        };
        let mut local_renames = match proc_ctx {
            Some(ctx) => ctx.local_renames.clone(),
            None => HashMap::new(),
        };
        let mut local_rename_ranges = match proc_ctx {
            Some(ctx) => ctx.local_rename_ranges.clone(),
            None => HashMap::new(),
        };
        collect_method_scope(node, source, &mut method_scope);

        // Apply local renames for THIS procedure AND enclosing procedures.
        // An outer procedure's rename with range (rps, rpe) applies if this
        // procedure's range is contained within it: rps <= ps && rpe >= pe.
        let ps = node.start_byte();
        let pe = node.end_byte();
        let mut containing_renames = rename_map
            .local
            .iter()
            .filter(|((rps, rpe, _), _)| *rps <= ps && *rpe >= pe)
            .collect::<Vec<_>>();
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
            method_scope.insert(new_name.to_lowercase(), new_name.clone());
            let range_size = rpe - rps;
            let replace = local_rename_ranges
                .get(old_lower)
                .is_none_or(|existing| range_size < *existing);
            if replace {
                local_renames.insert(old_lower.clone(), new_name.clone());
                local_rename_ranges.insert(old_lower.clone(), range_size);
            }
        }

        // Determine class context
        let class_name = extract_class_name(node, source);
        let class_fields = class_name
            .as_ref()
            .and_then(|cn| scopes.classes.get(&cn.to_lowercase()))
            .cloned();

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

    let text = node_text(node, source);
    let lower = text.to_lowercase();

    // Step 1: Check local rename (innermost matching enclosing procedure).
    // A rename keyed to (rps, rpe) applies if this proc context is contained
    // within that range. Prefer the smallest (innermost) containing range.
    if let Some(ctx) = proc_ctx {
        if let Some(new_name) = ctx.local_renames.get(&lower) {
            if text != *new_name {
                super::charge_budget(budget, cancel, 1, new_name.len())?;
                edits.push(TextEdit {
                    start_byte: node.start_byte(),
                    end_byte: node.end_byte(),
                    new_text: new_name.to_string(),
                });
            }
            return Ok(()); // local rename found — don't fall through
        }
    }

    // Step 2: Check file-scoped rename
    if let Some(new_name) = rename_map.file.get(&lower) {
        if text != *new_name {
            super::charge_budget(budget, cancel, 1, new_name.len())?;
            edits.push(TextEdit {
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                new_text: new_name.clone(),
            });
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
            super::charge_budget(budget, cancel, 1, declared_name.len())?;
            edits.push(TextEdit {
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                new_text: declared_name.clone(),
            });
        }
    }
    Ok(())
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
