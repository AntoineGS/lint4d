mod apply;
mod rename_map;
mod types;

pub use rename_map::build_rename_map;
pub use types::RenameMap;

use crate::config::Config;
use crate::engine::suppress::parse_suppressions;
use crate::engine::{FileInfo, FileType, parse_file};
use std::sync::atomic::{AtomicBool, Ordering};

use apply::apply_edits;
use types::FixConfig;

pub use types::FixEdit;

/// Work accounting supplied by a caller that needs a bounded naming-fix
/// traversal. The legacy [`fix_file`] and [`fix_file_edits`] APIs remain
/// unbounded for CLI compatibility; the bounded companion charges every
/// parser-derived traversal and materialized replacement to the caller's
/// request-wide budget.
pub trait FixWorkBudget {
    /// Charge visited semantic/tree work.
    fn charge_work(&mut self, amount: usize) -> Result<(), String>;

    /// Charge bytes inspected or materialized.
    fn charge_bytes(&mut self, amount: usize) -> Result<(), String>;
}

/// Fix naming convention violations in a single file.
///
/// Returns `(new_source_bytes, edit_count)` on success.
/// Returns the original source unchanged (with count 0) for `.dpr`/`.dpk` files.
pub fn fix_file(
    file: &FileInfo,
    source: &[u8],
    config: &Config,
) -> Result<(Vec<u8>, usize), String> {
    if matches!(file.file_type, FileType::Dpr | FileType::Dpk) {
        return Ok((source.to_vec(), 0));
    }

    let edits = collect_file_edits(file, source, config, None)?;
    apply_edits(source, edits)
}

/// Return the parser-derived edits for a selected finite set of naming rules.
///
/// The edits are relative to `source` and are intentionally not converted to
/// LSP ranges here. This keeps the established fix builder as the single
/// source of replacement text and lets callers prove the combined edit before
/// applying it. An empty rule list produces no edits.
pub fn fix_file_edits(
    file: &FileInfo,
    source: &[u8],
    config: &Config,
    rules: &[&str],
) -> Result<Vec<FixEdit>, String> {
    let edits = collect_file_edits(file, source, config, Some(rules))?;
    Ok(edits
        .into_iter()
        .map(|edit| FixEdit {
            start_byte: edit.start_byte,
            end_byte: edit.end_byte,
            new_text: edit.new_text,
        })
        .collect())
}

/// Return parser-derived edits while polling cancellation and charging a
/// caller-owned work/byte budget throughout collection.
///
/// No edits are returned after a budget or cancellation failure. The budget is
/// intentionally a trait so an embedding service can share one request-wide
/// counter with its navigation and workspace proof phases.
pub fn fix_file_edits_bounded<B: FixWorkBudget>(
    file: &FileInfo,
    source: &[u8],
    config: &Config,
    rules: &[&str],
    budget: &mut B,
    cancel: &AtomicBool,
) -> Result<Vec<FixEdit>, String> {
    let mut budget: Option<&mut dyn FixWorkBudget> = Some(budget);
    let edits =
        collect_file_edits_bounded(file, source, config, Some(rules), &mut budget, Some(cancel))?;
    Ok(edits
        .into_iter()
        .map(|edit| FixEdit {
            start_byte: edit.start_byte,
            end_byte: edit.end_byte,
            new_text: edit.new_text,
        })
        .collect())
}

fn collect_file_edits(
    file: &FileInfo,
    source: &[u8],
    config: &Config,
    rules: Option<&[&str]>,
) -> Result<Vec<types::TextEdit>, String> {
    let mut budget = None;
    collect_file_edits_bounded(file, source, config, rules, &mut budget, None)
}

fn collect_file_edits_bounded(
    file: &FileInfo,
    source: &[u8],
    config: &Config,
    rules: Option<&[&str]>,
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<types::TextEdit>, String> {
    if matches!(file.file_type, FileType::Dpr | FileType::Dpk) {
        return Ok(Vec::new());
    }

    charge_budget(budget, cancel, 1, source.len())?;
    std::str::from_utf8(source).map_err(|e| format!("invalid UTF-8: {e}"))?;

    let (tree, _parse_errors) = parse_file(file, source)?;
    charge_budget(budget, cancel, 1, 0)?;
    let root = tree.root_node();
    let suppressions = parse_suppressions(source);
    let rename_map = match rules {
        Some(rules) => rename_map::build_rename_map_for_rules_bounded(
            root,
            source,
            config,
            &suppressions,
            rules,
            budget,
            cancel,
        )?,
        None => rename_map::build_rename_map_bounded(
            root,
            source,
            config,
            &suppressions,
            budget,
            cancel,
        )?,
    };

    let mut scopes = crate::rules::scope::collect_file_scope_bounded(root, source, budget, cancel)?;
    rename_map::update_scopes_bounded(&mut scopes, &rename_map, budget, cancel)?;
    let fix_config = match rules {
        Some(rules) => FixConfig::for_rules(config, rules),
        None => FixConfig::from_config(config),
    };
    apply::collect_edits_bounded(
        root,
        source,
        &rename_map,
        &scopes,
        &fix_config,
        budget,
        cancel,
    )
}

pub(crate) fn charge_budget(
    budget: &mut Option<&mut dyn FixWorkBudget>,
    cancel: Option<&AtomicBool>,
    work: usize,
    bytes: usize,
) -> Result<(), String> {
    if cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
        return Err("request cancelled".to_string());
    }
    if let Some(budget) = budget.as_deref_mut() {
        budget.charge_work(work)?;
        budget.charge_bytes(bytes)?;
    }
    if cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
        return Err("request cancelled".to_string());
    }
    Ok(())
}
