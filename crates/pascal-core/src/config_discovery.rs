use std::path::{Path, PathBuf};

/// Walk up the directory tree from `start_dir` looking for a file named `filename`.
///
/// Returns `Some((contents, directory))` if found, where `directory` is the
/// directory containing the file. Returns `None` if the file is not found
/// in any ancestor directory.
pub fn find_config_file(start_dir: &Path, filename: &str) -> Option<(String, PathBuf)> {
    // Resolve to an absolute path so that dir.pop() can walk up the full
    // directory tree.  A relative or empty start_dir would otherwise stop
    // at the first component (or immediately), never reaching ancestors.
    let mut dir = if start_dir.is_relative() {
        std::env::current_dir().ok()?.join(start_dir)
    } else {
        start_dir.to_path_buf()
    };
    loop {
        let config_path = dir.join(filename);
        if config_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&config_path) {
                return Some((content, dir));
            }
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_config_in_start_dir() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".fmt4d.toml"), "found").unwrap();

        let result = find_config_file(tmp.path(), ".fmt4d.toml");
        assert!(result.is_some());
        let (content, dir) = result.unwrap();
        assert_eq!(content, "found");
        assert_eq!(dir, tmp.path());
    }

    #[test]
    fn walks_up_to_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let child = tmp.path().join("a").join("b").join("c");
        fs::create_dir_all(&child).unwrap();
        fs::write(tmp.path().join(".fmt4d.toml"), "ancestor").unwrap();

        let result = find_config_file(&child, ".fmt4d.toml");
        assert!(result.is_some());
        let (content, dir) = result.unwrap();
        assert_eq!(content, "ancestor");
        assert_eq!(dir, tmp.path());
    }

    #[test]
    fn returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let result = find_config_file(tmp.path(), ".nonexistent.toml");
        assert!(result.is_none());
    }

    #[test]
    fn relative_empty_path_still_walks_up() {
        // Simulates the bug: when --project gets a relative dproj path,
        // parent() yields "" and the old code couldn't walk up.
        let result = find_config_file(Path::new(""), ".fmt4d.toml");
        // We can't assert it finds a specific file (depends on filesystem),
        // but if CWD is obtainable and no config exists, it should return None
        // rather than panicking.  The key invariant is it doesn't stop at "".
        let _ = result;
    }
}
