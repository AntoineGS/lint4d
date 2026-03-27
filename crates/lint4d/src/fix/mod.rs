mod apply;
mod rename_map;
mod types;

pub use rename_map::build_rename_map;
pub use types::RenameMap;

use crate::config::Config;
use crate::engine::suppress::parse_suppressions;
use crate::engine::{parse_file, FileInfo, FileType};
use crate::rules::scope::collect_file_scope;

use apply::{apply_edits, collect_edits};
use rename_map::update_scopes;
use types::FixConfig;

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

    let source_str = std::str::from_utf8(source).map_err(|e| format!("invalid UTF-8: {e}"))?;
    let _ = source_str; // validates UTF-8

    let (tree, _parse_errors) = parse_file(file, source)?;
    let root = tree.root_node();

    let fix_config = FixConfig::from_config(config);

    // Pass 1: build rename map
    let suppressions = parse_suppressions(source);
    let rename_map = build_rename_map(root, source, config, &suppressions);

    // Pass 1.5: update scopes with post-rename names
    let mut scopes = collect_file_scope(root, source);
    update_scopes(&mut scopes, &rename_map);

    // Pass 2: collect edits
    let edits = collect_edits(root, source, &rename_map, &scopes, &fix_config);

    // Apply edits
    apply_edits(source, edits)
}
