//! Host path identity shared by project discovery, the resolver and the
//! server: whether two spellings name the same file, where a component walk
//! starts, and the plain form of `canonicalize` results.

use std::path::{Component, Path, PathBuf};

/// Whether the volume holding `path` ignores ASCII letter case. Windows
/// volumes do. On macOS the volume is asked, since APFS and HFS+ volumes can
/// be formatted either way; a path that does not exist yet takes the answer
/// of its nearest existing ancestor. Other hosts are case-sensitive.
pub fn is_case_insensitive(path: &Path) -> bool {
    #[cfg(any(test, feature = "test-support"))]
    if TEST_CASE_INSENSITIVE.with(std::cell::Cell::get) {
        return true;
    }
    #[cfg(windows)]
    {
        let _ = path;
        true
    }
    #[cfg(target_os = "macos")]
    {
        volume_case::is_case_insensitive(path)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = path;
        false
    }
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static TEST_CASE_INSENSITIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test support: runs `f` with every volume treated as case-insensitive on
/// this thread, so identity rules can be tested on any host. The files
/// themselves keep their volume's behavior.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn with_case_insensitive_volumes<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_CASE_INSENSITIVE.with(|forced| forced.set(self.0));
        }
    }
    let _restore = Restore(TEST_CASE_INSENSITIVE.with(|forced| forced.replace(true)));
    f()
}

#[cfg(target_os = "macos")]
mod volume_case {
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    // Identity checks ask about the same paths over and over; the answer is
    // cached per path (not per directory, which would give a mount point its
    // parent volume's answer). The cache is dropped when it fills up.
    const MAX_CACHED_PATHS: usize = 4096;

    static CACHE: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();

    pub(super) fn is_case_insensitive(path: &Path) -> bool {
        let cache = CACHE.get_or_init(Default::default);
        if let Some(&answer) = cache.lock().ok().as_ref().and_then(|map| map.get(path)) {
            return answer;
        }
        let answer = probe(path);
        if let Ok(mut map) = cache.lock() {
            if map.len() >= MAX_CACHED_PATHS {
                map.clear();
            }
            map.insert(path.to_path_buf(), answer);
        }
        answer
    }

    /// Asks the volume of `path`, or of its nearest existing ancestor.
    /// Volumes that cannot answer are taken as case-insensitive, the macOS
    /// default.
    fn probe(path: &Path) -> bool {
        for ancestor in path.ancestors() {
            let ancestor = if ancestor.as_os_str().is_empty() {
                Path::new(".")
            } else {
                ancestor
            };
            let Ok(name) = CString::new(ancestor.as_os_str().as_bytes()) else {
                return true;
            };
            // SAFETY: `name` is a NUL-terminated path that outlives the call.
            let sensitive = unsafe { libc::pathconf(name.as_ptr(), libc::_PC_CASE_SENSITIVE) };
            if sensitive >= 0 {
                return sensitive == 0;
            }
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ENOENT | libc::ENOTDIR) => continue,
                _ => return true,
            }
        }
        true
    }
}

/// Test support: a directory on a case-sensitive volume for tests that need
/// case-distinct entries such as `Foo` and `FOO`. This is the temporary
/// directory when its volume is case-sensitive, else
/// `LINT4D_CASE_SENSITIVE_TMPDIR` (macOS CI mounts a case-sensitive volume
/// there). `None` means the test cannot run on this host; under CI that is
/// a failure instead, so a missing volume does not skip tests silently.
#[doc(hidden)]
pub fn case_sensitive_test_dir() -> Option<PathBuf> {
    let candidates = std::env::var_os("LINT4D_CASE_SENSITIVE_TMPDIR")
        .map(PathBuf::from)
        .into_iter()
        .chain(std::iter::once(std::env::temp_dir()));
    for candidate in candidates {
        if candidate.is_dir() && !is_case_insensitive(&candidate) {
            return Some(candidate);
        }
    }
    assert!(
        std::env::var_os("CI").is_none(),
        "no case-sensitive volume for this test: set LINT4D_CASE_SENSITIVE_TMPDIR"
    );
    None
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
    // `canonicalize` reports the on-disk spelling on Windows (from an open
    // handle) and on macOS (realpath); a letter-case difference alone needs
    // nothing more.
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

    // The rule must agree with the volume itself, for files that exist and
    // for paths that do not exist yet; on macOS CI this covers the default
    // case-insensitive volume and a mounted case-sensitive one.
    #[test]
    fn case_rule_matches_what_the_volume_does() {
        let volumes = std::env::var_os("LINT4D_CASE_SENSITIVE_TMPDIR")
            .map(PathBuf::from)
            .into_iter()
            .chain(std::iter::once(std::env::temp_dir()));
        for volume in volumes {
            let temp = tempfile::tempdir_in(&volume).unwrap();
            let file = temp.path().join("Probe.txt");
            std::fs::write(&file, "").unwrap();
            let folds = temp.path().join("PROBE.TXT").exists();
            assert_eq!(is_case_insensitive(&file), folds, "{}", volume.display());
            assert_eq!(
                is_case_insensitive(&temp.path().join("missing/Unit.pas")),
                folds,
                "{}",
                volume.display()
            );
        }
    }

    #[test]
    fn identity_rules_can_be_forced_case_insensitive_for_tests() {
        let left = Path::new("/workspace/Unit1.pas");
        let right = Path::new("/workspace/UNIT1.PAS");
        with_case_insensitive_volumes(|| {
            assert!(paths_equal(left, right));
            assert_eq!(path_key(left), path_key(right));
        });
        assert_eq!(paths_equal(left, right), is_case_insensitive(left));
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
