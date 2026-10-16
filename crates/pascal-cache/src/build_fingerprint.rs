//! The build fingerprint that names the cache namespace: a BLAKE3 digest of
//! every input that can change analysis. Used by `build.rs`; also compiled
//! into unit tests.

use std::fs;
use std::path::{Path, PathBuf};

pub const FINGERPRINT_CONTEXT: &str =
    "lint4d persistent analysis cache 2026-10 build fingerprint v1";

const ROOT_FILES: [&str; 3] = ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"];
const CRATE_FILES: [&str; 2] = ["Cargo.toml", "build.rs"];
const CRATE_DIRECTORIES: [&str; 3] = ["src", "bindings/rust", "queries"];

/// Paths to pass to `cargo:rerun-if-changed`: the root files and every
/// existing per-crate file and directory. A new crate is caught through the
/// root `Cargo.toml` members list; watching `crates/` itself would make cargo
/// rescan every test and fixture file.
pub fn watched_paths(workspace_root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = ROOT_FILES
        .iter()
        .map(|name| workspace_root.join(name))
        .collect::<Vec<_>>();
    for crate_directory in crate_directories(workspace_root)? {
        for name in CRATE_FILES.iter().chain(&CRATE_DIRECTORIES) {
            let path = crate_directory.join(name);
            if path.exists() {
                paths.push(path);
            }
        }
    }
    Ok(paths)
}

/// Digest the toolchain, target and every fingerprinted file (relative name
/// and contents), in name order.
pub fn fingerprint(
    workspace_root: &Path,
    rustc_version: &str,
    target: &str,
) -> Result<[u8; 32], String> {
    let mut files = Vec::new();
    for name in ROOT_FILES {
        let path = workspace_root.join(name);
        if !fs::symlink_metadata(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?
            .is_file()
        {
            return Err(format!("{} is not a regular file", path.display()));
        }
        files.push(path);
    }
    for crate_directory in crate_directories(workspace_root)? {
        for name in CRATE_FILES {
            let path = crate_directory.join(name);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() => files.push(path),
                Ok(_) => return Err(format!("{} is not a regular file", path.display())),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("{}: {error}", path.display())),
            }
        }
        for name in CRATE_DIRECTORIES {
            let path = crate_directory.join(name);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_dir() => collect_files(&path, &mut files)?,
                Ok(_) => return Err(format!("{} is not a directory", path.display())),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("{}: {error}", path.display())),
            }
        }
    }
    let mut named = files
        .into_iter()
        .map(|path| Ok((relative_name(workspace_root, &path)?, path)))
        .collect::<Result<Vec<_>, String>>()?;
    named.sort();

    let mut hasher = blake3::Hasher::new_derive_key(FINGERPRINT_CONTEXT);
    write_bytes(&mut hasher, rustc_version.as_bytes());
    write_bytes(&mut hasher, target.as_bytes());
    hasher.update(&(named.len() as u64).to_le_bytes());
    for (name, path) in named {
        write_bytes(&mut hasher, name.as_bytes());
        let contents = fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        write_bytes(&mut hasher, &contents);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn crate_directories(workspace_root: &Path) -> Result<Vec<PathBuf>, String> {
    let crates = workspace_root.join("crates");
    let mut directories = Vec::new();
    for entry in fs::read_dir(&crates).map_err(|error| format!("{}: {error}", crates.display()))? {
        let entry = entry.map_err(|error| format!("{}: {error}", crates.display()))?;
        if entry
            .file_type()
            .map_err(|error| format!("{}: {error}", entry.path().display()))?
            .is_dir()
        {
            directories.push(entry.path());
        }
    }
    directories.sort();
    Ok(directories)
}

fn collect_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in
        fs::read_dir(directory).map_err(|error| format!("{}: {error}", directory.display()))?
    {
        let entry = entry.map_err(|error| format!("{}: {error}", directory.display()))?;
        // Editor lock files (`.#lib.rs`) and other dot entries are never
        // compiled implicitly.
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let file_type = fs::symlink_metadata(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?
            .file_type();
        if file_type.is_dir() {
            collect_files(&path, files)?;
        } else if file_type.is_file() {
            files.push(path);
        } else {
            return Err(format!(
                "{} is not a regular file or directory",
                path.display()
            ));
        }
    }
    Ok(())
}

fn relative_name(workspace_root: &Path, path: &Path) -> Result<String, String> {
    let relative = path
        .strip_prefix(workspace_root)
        .map_err(|_| format!("{} is outside the workspace", path.display()))?;
    let parts = relative
        .components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .ok_or_else(|| format!("{} is not valid UTF-8", path.display()))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(parts.join("/"))
}

fn write_bytes(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn workspace() -> TempDir {
        let root = tempfile::tempdir().unwrap();
        for (relative, contents) in [
            ("Cargo.toml", "[workspace]"),
            ("Cargo.lock", "version = 4"),
            ("rust-toolchain.toml", "[toolchain]"),
            ("crates/a/Cargo.toml", "[package]"),
            ("crates/a/src/lib.rs", "pub fn a() {}"),
            ("crates/a/tests/t.rs", "#[test] fn t() {}"),
            ("crates/b/Cargo.toml", "[package]"),
            ("crates/b/src/lib.rs", "pub fn b() {}"),
            ("crates/b/queries/q.scm", "(identifier)"),
            ("crates/b/bindings/rust/lib.rs", "pub fn language() {}"),
            ("crates/b/bindings/node/index.js", "module.exports = 1;"),
            ("crates/b/node_modules/x.js", "x"),
            ("target/debug/out", "artifact"),
        ] {
            write(root.path(), relative, contents);
        }
        root
    }

    fn digest(root: &Path) -> [u8; 32] {
        fingerprint(root, "rustc 1.99.0", "x86_64-unknown-linux-gnu").unwrap()
    }

    #[test]
    fn fingerprint_is_stable() {
        let root = workspace();
        assert_eq!(digest(root.path()), digest(root.path()));
    }

    #[test]
    fn analysis_inputs_change_the_fingerprint() {
        let root = workspace();
        let base = digest(root.path());
        for (relative, contents) in [
            ("crates/a/src/lib.rs", "pub fn a() {} // comment"),
            ("crates/a/src/new.rs", ""),
            ("crates/b/queries/q.scm", "(comment)"),
            ("crates/b/bindings/rust/lib.rs", ""),
            ("crates/b/build.rs", "fn main() {}"),
            ("crates/b/Cargo.toml", "[package] # edited"),
            ("Cargo.lock", "version = 4 # edited"),
            ("rust-toolchain.toml", "[toolchain] # edited"),
            ("crates/c/src/lib.rs", ""),
        ] {
            let root = workspace();
            write(root.path(), relative, contents);
            assert_ne!(
                digest(root.path()),
                base,
                "{relative} must change the fingerprint"
            );
        }
        assert_ne!(
            fingerprint(root.path(), "rustc 1.99.1", "x86_64-unknown-linux-gnu").unwrap(),
            base
        );
        assert_ne!(
            fingerprint(root.path(), "rustc 1.99.0", "x86_64-pc-windows-msvc").unwrap(),
            base
        );
    }

    #[test]
    fn renaming_a_source_file_changes_the_fingerprint() {
        let root = workspace();
        let base = digest(root.path());
        fs::rename(
            root.path().join("crates/a/src/lib.rs"),
            root.path().join("crates/a/src/main.rs"),
        )
        .unwrap();
        assert_ne!(digest(root.path()), base);
    }

    #[test]
    fn unrelated_files_do_not_change_the_fingerprint() {
        let root = workspace();
        let base = digest(root.path());
        for (relative, contents) in [
            ("crates/a/tests/t.rs", "// edited"),
            ("crates/b/node_modules/x.js", "y"),
            ("crates/b/bindings/node/index.js", "module.exports = 2;"),
            ("crates/b/build/Release/obj", "node-gyp output"),
            ("target/debug/out", "other"),
            ("docs/notes.md", "notes"),
        ] {
            write(root.path(), relative, contents);
            assert_eq!(
                digest(root.path()),
                base,
                "{relative} must not change the fingerprint"
            );
        }
    }

    #[test]
    fn missing_root_inputs_are_errors() {
        let root = workspace();
        fs::remove_file(root.path().join("rust-toolchain.toml")).unwrap();
        assert!(fingerprint(root.path(), "rustc", "target").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_in_fingerprinted_directories_are_errors() {
        let root = workspace();
        std::os::unix::fs::symlink(
            root.path().join("crates/a/src/lib.rs"),
            root.path().join("crates/a/src/link.rs"),
        )
        .unwrap();
        assert!(digest_result(root.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_editor_lock_symlinks_are_ignored() {
        let root = workspace();
        let base = digest(root.path());
        std::os::unix::fs::symlink("user@host.1234", root.path().join("crates/a/src/.#lib.rs"))
            .unwrap();
        assert_eq!(digest(root.path()), base);
    }

    #[test]
    fn hidden_files_and_directories_are_ignored() {
        let root = workspace();
        let base = digest(root.path());
        write(root.path(), "crates/a/src/.hidden.rs", "fn hidden() {}");
        write(root.path(), "crates/a/src/.cache/x.rs", "fn cached() {}");
        assert_eq!(digest(root.path()), base);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_top_level_directories_are_errors() {
        let root = workspace();
        let source = root.path().join("crates/a/src");
        fs::rename(&source, root.path().join("crates/a/real")).unwrap();
        std::os::unix::fs::symlink(root.path().join("crates/a/real"), &source).unwrap();
        assert!(digest_result(root.path()).is_err());
    }

    fn digest_result(root: &Path) -> Result<[u8; 32], String> {
        fingerprint(root, "rustc 1.99.0", "x86_64-unknown-linux-gnu")
    }

    #[test]
    fn watched_paths_cover_inputs_but_not_unrelated_directories() {
        let root = workspace();
        let watched = watched_paths(root.path()).unwrap();
        let relative = watched
            .iter()
            .map(|path| {
                path.strip_prefix(root.path())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect::<Vec<_>>();
        for expected in [
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            "crates/a/Cargo.toml",
            "crates/a/src",
            "crates/b/Cargo.toml",
            "crates/b/src",
            "crates/b/queries",
            "crates/b/bindings/rust",
        ] {
            assert!(
                relative.iter().any(|path| path == expected),
                "{expected} is not watched"
            );
        }
        assert!(!relative.iter().any(|path| {
            path == "crates" || path.contains("tests") || path.contains("node_modules")
        }));
    }
}
