//! Host path identity shared by project discovery, the resolver and the
//! server: whether two spellings name the same file, where a component walk
//! starts, and the plain form of `canonicalize` results.

use std::path::{Component, Path, PathBuf};

/// Whether the volume holding `path` ignores ASCII letter case. Windows
/// volumes do; other hosts are treated as case-sensitive until volumes are
/// probed (backlog TASK-74 for macOS).
pub fn is_case_insensitive(_path: &Path) -> bool {
    cfg!(windows)
}

/// Compares path components, ignoring ASCII case when `case_insensitive`.
pub fn components_equal(left: Component<'_>, right: Component<'_>, case_insensitive: bool) -> bool {
    if case_insensitive {
        left.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
    } else {
        left.as_os_str() == right.as_os_str()
    }
}

/// Whether `left` and `right` name the same path on this host, component by
/// component. Both paths should already be absolute and lexically normal.
pub fn paths_equal(left: &Path, right: &Path) -> bool {
    let case_insensitive = is_case_insensitive(left);
    left.components().count() == right.components().count()
        && left
            .components()
            .zip(right.components())
            .all(|(left, right)| components_equal(left, right, case_insensitive))
}

/// Whether `path` is `root` or lies under it on this host.
pub fn path_starts_with(path: &Path, root: &Path) -> bool {
    let case_insensitive = is_case_insensitive(root);
    let mut path_components = path.components();
    root.components().all(|root| {
        path_components
            .next()
            .is_some_and(|path| components_equal(path, root, case_insensitive))
    })
}

/// A string key for `path` that is equal for every spelling of it on this
/// host: lowercased on case-insensitive volumes.
pub fn path_key(path: &Path) -> String {
    let key = path.to_string_lossy();
    if is_case_insensitive(path) {
        key.to_ascii_lowercase()
    } else {
        key.into_owned()
    }
}

/// The directory a component-by-component walk of `absolute` starts from:
/// its prefix and root (`C:\`, `\\server\share\`, `/`). Starting from the
/// bare separator instead would drop a Windows drive or share.
pub fn walk_root(absolute: &Path) -> PathBuf {
    let mut root = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => root.push(component.as_os_str()),
            _ => break,
        }
    }
    if root.as_os_str().is_empty() {
        root.push(std::path::MAIN_SEPARATOR_STR);
    }
    root
}

/// The spelling on disk of the existing file `path` on a case-insensitive
/// volume, when it differs from `path`: other letter case, or 8.3 short
/// names (`RUNNER~1`) for long ones. Symlinked components keep their own
/// name; they are never replaced by their target's.
pub fn on_disk_spelling(path: &Path) -> Option<PathBuf> {
    if !is_case_insensitive(path) {
        return None;
    }
    // Windows reports the on-disk spelling from an open handle; a letter-case
    // difference alone needs nothing more. Volumes that do not report it
    // (macOS, backlog TASK-74) need the directory walk below.
    let actual = without_verbatim_prefix(std::fs::canonicalize(path).ok()?);
    if actual == path {
        return None;
    }
    if paths_equal(&actual, path) {
        return Some(actual);
    }
    // Short names, symlinks or junctions: walk the listings, which map short
    // names to long ones but keep symlinked components as named.
    let mut warnings = Vec::new();
    match crate::resolve_existing_path_status(path, &mut warnings, "path") {
        crate::ExistingPathStatus::Found(walked) if walked != path => Some(walked),
        _ => None,
    }
}

/// Turns a verbatim Windows path from `canonicalize` (`\\?\C:\x`,
/// `\\?\UNC\server\share\x`) back into its plain form, which the rest of the
/// path handling and every user-facing path use. Other paths are unchanged.
pub fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        let bytes = rest.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return PathBuf::from(rest);
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_drive_and_unc_paths_become_plain() {
        assert_eq!(
            without_verbatim_prefix(PathBuf::from(r"\\?\C:\Users\runneradmin\x.pas")),
            PathBuf::from(r"C:\Users\runneradmin\x.pas")
        );
        assert_eq!(
            without_verbatim_prefix(PathBuf::from(r"\\?\UNC\server\share\x.pas")),
            PathBuf::from(r"\\server\share\x.pas")
        );
        assert_eq!(
            without_verbatim_prefix(PathBuf::from(r"\\?\Volume{1234}\x.pas")),
            PathBuf::from(r"\\?\Volume{1234}\x.pas")
        );
        assert_eq!(
            without_verbatim_prefix(PathBuf::from("/tmp/x.pas")),
            PathBuf::from("/tmp/x.pas")
        );
    }

    #[test]
    fn walk_root_keeps_the_prefix_and_root() {
        let absolute = std::env::temp_dir().join("unit.pas");
        let root = walk_root(&absolute);
        assert!(absolute.starts_with(&root));
        assert!(root.has_root());
        assert!(
            root.components()
                .all(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
        );
    }

    #[cfg(windows)]
    #[test]
    fn walk_root_keeps_the_drive() {
        assert_eq!(
            walk_root(Path::new(r"C:\Users\x.pas")),
            PathBuf::from(r"C:\")
        );
        assert_eq!(
            walk_root(Path::new(r"\\server\share\x.pas")),
            PathBuf::from(r"\\server\share\")
        );
    }

    #[test]
    fn on_disk_spelling_corrects_letter_case_only_on_case_insensitive_volumes() {
        let temp = tempfile::tempdir().unwrap();
        let actual =
            without_verbatim_prefix(std::fs::canonicalize(temp.path()).unwrap()).join("Body.inc");
        std::fs::write(&actual, "").unwrap();
        let requested = actual.with_file_name("BODY.INC");
        let expected = is_case_insensitive(&actual).then(|| actual.clone());
        assert_eq!(on_disk_spelling(&requested), expected);
        assert_eq!(on_disk_spelling(&actual), None);
    }

    #[test]
    fn identity_follows_the_volume_case_rule() {
        let left = std::env::temp_dir().join("Unit1.pas");
        let right = std::env::temp_dir().join("unit1.pas");
        assert_eq!(paths_equal(&left, &right), is_case_insensitive(&left));
        assert_eq!(
            path_key(&left) == path_key(&right),
            is_case_insensitive(&left)
        );
        assert!(path_starts_with(&right, &std::env::temp_dir()));
        assert!(!path_starts_with(&std::env::temp_dir(), &right));
        assert!(paths_equal(&left, &left));
    }
}
