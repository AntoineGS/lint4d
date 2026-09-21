mod apply;
mod rename_map;
mod types;

pub use rename_map::build_rename_map;
pub use types::RenameMap;

use crate::config::Config;
use crate::engine::suppress::parse_suppressions;
use crate::engine::{FileInfo, FileType, parse_file};
use crate::rules::scope::collect_file_scope;

use apply::{apply_edits, collect_edits};
use rename_map::{build_rename_map_for_rules, update_scopes};
use types::FixConfig;

pub use types::FixEdit;

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

fn collect_file_edits(
    file: &FileInfo,
    source: &[u8],
    config: &Config,
    rules: Option<&[&str]>,
) -> Result<Vec<types::TextEdit>, String> {
    if matches!(file.file_type, FileType::Dpr | FileType::Dpk) {
        return Ok(Vec::new());
    }

    std::str::from_utf8(source).map_err(|e| format!("invalid UTF-8: {e}"))?;

    let (tree, _parse_errors) = parse_file(file, source)?;
    let root = tree.root_node();
    let suppressions = parse_suppressions(source);
    let rename_map = match rules {
        Some(rules) => build_rename_map_for_rules(root, source, config, &suppressions, rules),
        None => build_rename_map(root, source, config, &suppressions),
    };

    let mut scopes = collect_file_scope(root, source);
    update_scopes(&mut scopes, &rename_map);
    let fix_config = match rules {
        Some(rules) => FixConfig::for_rules(config, rules),
        None => FixConfig::from_config(config),
    };
    Ok(collect_edits(
        root,
        source,
        &rename_map,
        &scopes,
        &fix_config,
    ))
}
