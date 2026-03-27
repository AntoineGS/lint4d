use std::path::{Path, PathBuf};

/// Walk up the directory tree from `start_dir` looking for a file named `filename`.
///
/// Returns `Some((contents, directory))` if found, where `directory` is the
/// directory containing the file. Returns `None` if the file is not found
/// in any ancestor directory.
pub fn find_config_file(start_dir: &Path, filename: &str) -> Option<(String, PathBuf)> {
    let mut dir = start_dir.to_path_buf();
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
