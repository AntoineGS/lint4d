use std::collections::HashMap;

use tree_sitter::Node;

use crate::rules::helpers::node_text;
use crate::rules::scope::{
    collect_method_scope, extract_class_name, is_declaration_position, is_dot_rhs,
    is_inside_inherited, is_inside_module_name, is_inside_typeref, Scopes,
};

use super::types::{FixConfig, ProcContext, RenameMap, TextEdit};

// ---------------------------------------------------------------------------
// Collect edits
// ---------------------------------------------------------------------------

pub(crate) fn collect_edits(
    root: Node,
    source: &[u8],
    rename_map: &RenameMap,
    scopes: &Scopes,
    fix_config: &FixConfig,
) -> Vec<TextEdit> {
    let mut edits = Vec::new();
    walk_for_edits(
        root,
        source,
        rename_map,
        scopes,
        None,
        fix_config.casing,
        &mut edits,
    );
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
        resolve_and_emit(
            node,
            source,
            rename_map,
            scopes,
            proc_ctx,
            casing_enabled,
            edits,
        );
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
            walk_for_edits(
                child,
                source,
                rename_map,
                scopes,
                Some(&ctx),
                casing_enabled,
                edits,
            );
        }
        return;
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
        );
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

    // Look up in scope chain: method -> class fields -> file
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
