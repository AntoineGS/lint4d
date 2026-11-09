//! The store directory is not part of any workspace (`l4d-vrw0`).
//!
//! A workspace root may contain the store, as when `$HOME` is opened and the
//! store is `~/.cache/lint4d/analysis`. Its own writes must then not look
//! like workspace changes, so every walk, catalogue, membership check and
//! file event leaves it out. That hides nothing only while the directory
//! holds no Pascal sources or project files: one that does is refused
//! instead of excluded.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// Entries [`check_dedicated`] lists before it gives up on a directory.
pub(crate) const MAX_CHECKED_ENTRIES: usize = 65_536;

/// The excluded store directory: as configured and with its existing
/// ancestors resolved, so a path reached through either form matches.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CacheExclusion {
    directories: Arc<[PathBuf]>,
}

impl CacheExclusion {
    /// Resolves the longest existing ancestor of `directory`: the store may
    /// not have been created yet.
    pub(crate) fn new(directory: &Path) -> Self {
        let lexical = lexically_normal(directory);
        let mut directories = vec![lexical.clone()];
        if let Some(resolved) = resolve_existing_ancestor(&lexical)
            && resolved != lexical
        {
            directories.push(resolved);
        }
        Self {
            directories: directories.into(),
        }
    }

    /// Whether `path` is the directory or under it, in one of its forms.
    pub(crate) fn contains(&self, path: &Path) -> bool {
        self.directories
            .iter()
            .any(|directory| pascal_project::path_identity::path_starts_with(path, directory))
    }

    /// The forms a walk of `root` can meet: those under `root`, or all of
    /// them when `root` itself lies in the directory. The same prefix rule
    /// as [`Self::contains`] (`path_identity::path_starts_with`, which folds
    /// case on a case-insensitive volume), so a catalogue key names the
    /// directory exactly when its walk leaves something out.
    pub(crate) fn met_by_walk_of(&self, root: &Path) -> Vec<PathBuf> {
        if self.contains(root) {
            return self.directories.to_vec();
        }
        self.directories
            .iter()
            .filter(|directory| pascal_project::path_identity::path_starts_with(directory, root))
            .cloned()
            .collect()
    }

    /// The directory as seen through each of `bases` (a workspace root and
    /// its source roots) when a base resolves elsewhere through a symlink:
    /// a walk under `/link` reaches `/real/cache` as `/link/cache`.
    pub(crate) fn seen_from<'a>(&self, bases: impl IntoIterator<Item = &'a Path>) -> Self {
        let mut directories = self.directories.to_vec();
        let resolved = self.directories.last().expect("at least the lexical form");
        for base in bases {
            let Ok(resolved_base) = std::fs::canonicalize(base) else {
                continue;
            };
            let Ok(relative) = resolved.strip_prefix(&resolved_base) else {
                continue;
            };
            let seen = base.join(relative);
            if !directories.contains(&seen) {
                directories.push(seen);
            }
        }
        Self {
            directories: directories.into(),
        }
    }
}

/// `.` and `..` folded away without touching the filesystem.
fn lexically_normal(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other.as_os_str()),
        }
    }
    normal
}

fn resolve_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        if let Ok(resolved) = std::fs::canonicalize(&existing) {
            let mut resolved = pascal_project::path_identity::without_verbatim_prefix(resolved);
            resolved.extend(missing.iter().rev());
            return Some(resolved);
        }
        missing.push(existing.file_name()?.to_os_string());
        if !existing.pop() {
            return None;
        }
    }
}

/// Refuse a store directory holding Pascal sources or project files:
/// excluding it would hide them. An absent directory is accepted (the store
/// creates it). Symlinks are not followed.
pub(crate) fn check_dedicated(directory: &Path) -> Result<(), String> {
    if std::fs::symlink_metadata(directory).is_err() {
        return Ok(());
    }
    let mut listed = 0usize;
    for entry in walkdir::WalkDir::new(directory).follow_links(false) {
        // An entry that cannot be read cannot be shown as a workspace source
        // either: every workspace walk skips unreadable entries the same way.
        let Ok(entry) = entry else {
            continue;
        };
        listed += 1;
        if listed > MAX_CHECKED_ENTRIES {
            return Err(format!(
                "persistentCacheDirectory {} holds more than {MAX_CHECKED_ENTRIES} entries, so it \
                 cannot be checked for Pascal sources; set persistentCacheDirectory to a \
                 directory used only by the cache",
                directory.display()
            ));
        }
        if entry.file_type().is_file() && crate::catalogue::is_retained_file(entry.path()) {
            return Err(sources_message(directory, entry.path()));
        }
    }
    Ok(())
}

/// Why `directory` is refused: it holds `file`, a Pascal source or project
/// file.
pub(crate) fn sources_message(directory: &Path, file: &Path) -> String {
    format!(
        "persistentCacheDirectory {} contains Pascal sources or project files ({}); \
         set persistentCacheDirectory to a directory used only by the cache",
        directory.display(),
        file.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_under_the_directory_is_contained() {
        let exclusion = CacheExclusion::new(Path::new("/home/user/.cache/lint4d/analysis"));
        assert!(exclusion.contains(Path::new("/home/user/.cache/lint4d/analysis")));
        assert!(exclusion.contains(Path::new(
            "/home/user/.cache/lint4d/analysis/analysis-v7.sqlite3-wal"
        )));
        assert!(!exclusion.contains(Path::new("/home/user/.cache/lint4d")));
        assert!(!exclusion.contains(Path::new("/home/user/.cache/lint4d/analysis2")));
        assert!(!exclusion.contains(Path::new("/home/user/Main.pas")));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_ancestors_and_bases_are_resolved() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let real = std::fs::canonicalize(&real).unwrap();

        // Configured through the link, not created yet.
        let exclusion = CacheExclusion::new(&link.join("cache/lint4d/analysis"));
        assert!(exclusion.contains(&real.join("cache/lint4d/analysis/namespaces")));
        assert!(exclusion.contains(&link.join("cache/lint4d/analysis/namespaces")));

        // Configured through the real path, walked through the link.
        let exclusion = CacheExclusion::new(&real.join("cache/lint4d/analysis"));
        assert!(!exclusion.contains(&link.join("cache/lint4d/analysis")));
        let seen = exclusion.seen_from([link.as_path()]);
        assert!(seen.contains(&link.join("cache/lint4d/analysis/analysis-v7.sqlite3")));
        assert!(!seen.contains(&link.join("cache/lint4d")));
    }

    #[test]
    fn a_directory_without_sources_is_dedicated() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(check_dedicated(&temp.path().join("absent")), Ok(()));
        let store = temp.path().join("analysis");
        std::fs::create_dir_all(store.join("namespaces")).unwrap();
        std::fs::write(store.join("analysis-v7.sqlite3"), b"").unwrap();
        std::fs::write(store.join("namespaces/0123.lock"), b"").unwrap();
        assert_eq!(check_dedicated(&store), Ok(()));
    }

    #[test]
    fn a_directory_with_pascal_sources_or_projects_is_refused() {
        for name in ["nested/Unit1.pas", "Project.dproj", "Shared.inc"] {
            let temp = tempfile::tempdir().unwrap();
            let file = temp.path().join(name);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, b"unit Unit1;").unwrap();
            let error = check_dedicated(temp.path()).unwrap_err();
            assert!(error.contains("persistentCacheDirectory"), "{error}");
            assert!(error.contains("used only by the cache"), "{error}");
        }
    }
}
