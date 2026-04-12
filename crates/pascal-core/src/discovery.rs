use crate::types::{FileInfo, FileType};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::path::PathBuf;
use walkdir::WalkDir;

/// Discover Delphi source files (`.pas`, `.dpr`, `.dpk`) from the given paths.
///
/// Each entry in `paths` may be:
/// - A single file with a recognised extension — included directly.
/// - A directory — walked recursively, filtered by extension.
///
/// Files whose path (relative to the first matching directory root, or absolute)
/// matches any pattern in `exclude_patterns` are omitted.
/// Results are returned sorted by path for deterministic output.
pub fn discover_files(
    paths: &[PathBuf],
    exclude_patterns: &[String],
) -> Result<Vec<FileInfo>, String> {
    let excludes = build_glob_set(exclude_patterns)?;

    let mut results: Vec<FileInfo> = Vec::new();

    for path in paths {
        // Reject symlinks passed directly as positional args. Same reason as
        // the walkdir case below. Use symlink_metadata so we don't follow.
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if meta.file_type().is_symlink() {
                eprintln!("pascal-core: skipping symlink {}", path.display());
                continue;
            }
        }
        if path.is_file() {
            if let Some(ft) = file_type_for_path(path) {
                if !is_excluded(path, path.parent().unwrap_or(path), &excludes) {
                    results.push(FileInfo {
                        path: path.clone(),
                        file_type: ft,
                    });
                }
            }
        } else if path.is_dir() {
            let base = path.as_path();
            for entry in WalkDir::new(path)
                .follow_links(false)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                let entry_path = entry.path();
                // Reject symlinks outright: following them enables arbitrary-file
                // overwrite via `fs::write(path, ...)` in --write mode.
                // See review SEC-CRIT-1.
                if entry.file_type().is_symlink() {
                    continue;
                }
                if !entry.file_type().is_file() {
                    continue;
                }
                if let Some(ft) = file_type_for_path(entry_path) {
                    if !is_excluded(entry_path, base, &excludes) {
                        results.push(FileInfo {
                            path: entry_path.to_path_buf(),
                            file_type: ft,
                        });
                    }
                }
            }
        }
    }

    results.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(results)
}

fn file_type_for_path(path: &std::path::Path) -> Option<FileType> {
    path.extension()
        .and_then(|e| e.to_str())
        .and_then(FileType::from_extension)
}

fn build_glob_set(patterns: &[String]) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob =
            Glob::new(pattern).map_err(|e| format!("invalid glob pattern {:?}: {}", pattern, e))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|e| format!("failed to build glob set: {}", e))
}

/// Check whether `file_path` matches any exclusion pattern.
///
/// Matching is attempted against:
/// 1. The path relative to `base` (so `generated/**` matches `generated/Foo.pas`).
/// 2. The absolute path string (fallback).
fn is_excluded(file_path: &std::path::Path, base: &std::path::Path, excludes: &GlobSet) -> bool {
    if excludes.is_empty() {
        return false;
    }
    // Try relative path first.
    if let Ok(rel) = file_path.strip_prefix(base) {
        if excludes.is_match(rel) {
            return true;
        }
    }
    // Fallback: absolute path.
    excludes.is_match(file_path)
}
