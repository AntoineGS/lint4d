use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

/// Result of inspecting one ordered configuration filename.
///
/// The `checked_paths` and `checked_contents` fields are parallel. On a
/// successful selection they contain every candidate through and including
/// the selected candidate; the selected entry has `Some` contents. When no
/// candidate exists, they contain every supplied directory and every content
/// entry is `None`.
///
/// # Security considerations
/// This record describes filesystem observations only. It is not an
/// authorization grant and must not be used to bypass a [`crate::ReadPolicy`].
#[derive(Debug)]
pub struct ConfigRead {
    /// The first existing candidate, or `None` when every candidate was
    /// absent.
    pub path: Option<PathBuf>,
    /// Candidate paths inspected in caller-supplied precedence order.
    pub checked_paths: Vec<PathBuf>,
    /// Contents observed for `checked_paths`; absent candidates are `None`.
    pub checked_contents: Vec<Option<Vec<u8>>>,
    /// Contents of `path`, or `None` when no candidate existed.
    pub bytes: Option<Vec<u8>>,
}

/// Build the ordered precedence roots used for sidecar configuration lookup.
///
/// The order is project directory (when supplied), the longest containing
/// workspace root, the nearest repository root, and finally the document
/// directory. Duplicate roots are removed while preserving that precedence.
/// This selects lookup roots; it is not a general ancestor traversal API and
/// does not read configuration files itself.
///
/// # Security considerations
/// The returned paths are suitable only for the caller's intended lookup
/// scope. This function does not apply a [`crate::ReadPolicy`] or establish that the
/// supplied workspace roots are trusted for arbitrary payload reads.
pub fn config_directories(
    document: &Path,
    project_directory: Option<&Path>,
    roots: &[PathBuf],
) -> Result<Vec<PathBuf>, String> {
    let project_directory = project_directory.map(resolve_path).transpose()?;
    let anchor = if let Some(project_directory) = &project_directory {
        project_directory.clone()
    } else {
        let document = resolve_path(document)?;
        document
            .parent()
            .map_or_else(|| document.clone(), Path::to_path_buf)
    };
    let workspace_root = longest_containing_root(&anchor, roots)?;
    let repository_root = nearest_git_root(&anchor)?;

    let mut directories = Vec::new();
    if let Some(project_directory) = project_directory {
        add_unique_path(&mut directories, project_directory);
    }
    if let Some(workspace_root) = workspace_root {
        add_unique_path(&mut directories, workspace_root);
    }
    if let Some(repository_root) = repository_root {
        add_unique_path(&mut directories, repository_root);
    }
    if directories.is_empty() {
        add_unique_path(&mut directories, anchor);
    }

    Ok(directories)
}

/// Read the first existing configuration candidate from `directories`.
///
/// Candidates are examined in order. Missing candidates are recorded and
/// skipped. The first existing candidate wins; an existing directory, special
/// file, dangling symlink, oversized file, or any open/stat/read error is
/// returned as an error and is never skipped in favor of a later candidate.
/// A symlink to a regular file is accepted for sidecar configuration lookup.
///
/// # Security considerations
/// `directories` and `filename` must be trusted by the caller. This helper
/// validates the candidate file type and bounds the read, but it does not
/// apply [`crate::ReadPolicy`] authorization or constrain the filename. It is for
/// trusted configuration lookup, not arbitrary payload discovery.
pub fn read_config(
    directories: &[PathBuf],
    filename: &str,
    max_bytes: usize,
) -> Result<ConfigRead, String> {
    let mut checked_paths = Vec::with_capacity(directories.len());
    let mut checked_contents = Vec::with_capacity(directories.len());
    for directory in directories {
        let candidate = directory.join(filename);
        checked_paths.push(candidate.clone());
        let Some(metadata) = inspect_candidate(&candidate)? else {
            checked_contents.push(None);
            continue;
        };
        if metadata.len() > max_bytes as u64 {
            return Err(size_error(&candidate, max_bytes));
        }

        let mut file = open_candidate(&candidate).map_err(|error| io_error(&candidate, error))?;
        let opened_metadata = file
            .metadata()
            .map_err(|error| io_error(&candidate, error))?;
        validate_regular_file(&candidate, &opened_metadata)?;
        let bytes = read_bounded(&mut file, max_bytes, &candidate)?;
        return Ok(ConfigRead {
            path: Some(candidate),
            checked_paths,
            checked_contents: {
                checked_contents.push(Some(bytes.clone()));
                checked_contents
            },
            bytes: Some(bytes),
        });
    }

    Ok(ConfigRead {
        path: None,
        checked_paths,
        checked_contents,
        bytes: None,
    })
}

fn inspect_candidate(candidate: &Path) -> Result<Option<fs::Metadata>, String> {
    let metadata = match fs::symlink_metadata(candidate) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(candidate, error)),
    };
    let metadata = if metadata.file_type().is_symlink() {
        fs::metadata(candidate).map_err(|error| io_error(candidate, error))?
    } else {
        metadata
    };
    validate_regular_file(candidate, &metadata)?;
    Ok(Some(metadata))
}

fn validate_regular_file(candidate: &Path, metadata: &fs::Metadata) -> Result<(), String> {
    if metadata.is_dir() {
        return Err(format!(
            "configuration candidate {} is a directory",
            candidate.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!(
            "configuration candidate {} is not a regular file",
            candidate.display()
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_candidate(candidate: &Path) -> io::Result<File> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    // Linux's UAPI O_NONBLOCK value, restricted to this target so a replaced
    // FIFO cannot make the configuration resolver block between inspection and
    // opening. Regular-file reads are unaffected by this flag.
    const O_NONBLOCK: i32 = 0o4000;
    OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(candidate)
}

#[cfg(not(target_os = "linux"))]
fn open_candidate(candidate: &Path) -> io::Result<File> {
    File::open(candidate)
}

fn read_bounded(file: &mut File, max_bytes: usize, path: &Path) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    while bytes.len() < max_bytes {
        let remaining = max_bytes - bytes.len();
        let read_size = remaining.min(buffer.len());
        let count = file
            .read(&mut buffer[..read_size])
            .map_err(|error| io_error(path, error))?;
        if count == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&buffer[..count]);
    }

    let mut extra = [0_u8; 1];
    let count = file
        .read(&mut extra)
        .map_err(|error| io_error(path, error))?;
    if count != 0 {
        return Err(size_error(path, max_bytes));
    }
    Ok(bytes)
}

fn io_error(path: &Path, error: io::Error) -> String {
    format!(
        "could not read configuration candidate {}: {error}",
        path.display()
    )
}

fn size_error(path: &Path, max_bytes: usize) -> String {
    format!(
        "configuration candidate {} exceeds the maximum size of {max_bytes} bytes",
        path.display()
    )
}

fn longest_containing_root(document: &Path, roots: &[PathBuf]) -> Result<Option<PathBuf>, String> {
    let mut longest = None;
    let mut longest_components = 0;
    for root in roots {
        let root = resolve_path(root)?;
        let component_count = root.components().count();
        if path_starts_with(document, &root) && component_count > longest_components {
            longest = Some(root);
            longest_components = component_count;
        }
    }
    Ok(longest)
}

fn nearest_git_root(start: &Path) -> Result<Option<PathBuf>, String> {
    let mut directory = start.to_path_buf();
    loop {
        let marker = directory.join(".git");
        let marker_metadata = match fs::symlink_metadata(&marker) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(parent) = directory.parent() else {
                    return Ok(None);
                };
                if parent == directory {
                    return Ok(None);
                }
                directory = parent.to_path_buf();
                continue;
            }
            Err(error) => return Err(git_marker_error(&marker, error)),
        };
        let marker_metadata = if marker_metadata.file_type().is_symlink() {
            fs::metadata(&marker).map_err(|error| git_marker_error(&marker, error))?
        } else {
            marker_metadata
        };
        if marker_metadata.is_dir() || marker_metadata.is_file() {
            return Ok(Some(directory));
        }
        return Err(format!(
            "Git marker {} is not a directory or regular file",
            marker.display()
        ));
    }
}

fn git_marker_error(path: &Path, error: io::Error) -> String {
    format!("could not inspect Git marker {}: {error}", path.display())
}

fn resolve_path(path: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("could not determine current directory: {error}"))?
            .join(path)
    };

    let mut unresolved = Vec::new();
    let mut current = absolute.as_path();
    loop {
        match fs::canonicalize(current) {
            Ok(mut resolved) => {
                // Canonicalize every existing prefix so `..` follows the
                // filesystem's symlink semantics instead of lexical spelling.
                for component in unresolved.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(name) = current.file_name() else {
                    return Ok(lexical_normalize(&absolute));
                };
                unresolved.push(name.to_os_string());
                let Some(parent) = current.parent() else {
                    return Ok(lexical_normalize(&absolute));
                };
                current = parent;
            }
            Err(error) => {
                return Err(format!(
                    "could not inspect path {}: {error}",
                    current.display()
                ));
            }
        }
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                }
            }
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir | Component::Normal(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn path_starts_with(path: &Path, root: &Path) -> bool {
    let path_components = path.components().collect::<Vec<_>>();
    let root_components = root.components().collect::<Vec<_>>();
    path_components.len() >= root_components.len()
        && path_components
            .iter()
            .zip(root_components.iter())
            .all(|(path, root)| components_equal(*path, *root))
}

fn add_unique_path(paths: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !paths.iter().any(|path| paths_equal(path, &candidate)) {
        paths.push(candidate);
    }
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    let left_components = left.components().collect::<Vec<_>>();
    let right_components = right.components().collect::<Vec<_>>();
    left_components.len() == right_components.len()
        && left_components
            .iter()
            .zip(right_components.iter())
            .all(|(left, right)| components_equal(*left, *right))
}

fn components_equal(left: Component<'_>, right: Component<'_>) -> bool {
    #[cfg(windows)]
    {
        left.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left.as_os_str() == right.as_os_str()
    }
}

#[cfg(not(windows))]
#[cfg(test)]
mod tests {
    use super::add_unique_path;
    use std::path::PathBuf;

    #[test]
    fn case_distinct_linux_directories_are_not_deduplicated() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let upper = temp.path().join("Root");
        let lower = temp.path().join("root");
        std::fs::create_dir(&upper).expect("upper directory");
        std::fs::create_dir(&lower).expect("lower directory");

        let mut directories = Vec::<PathBuf>::new();
        add_unique_path(&mut directories, upper.clone());
        add_unique_path(&mut directories, lower.clone());
        assert_eq!(directories, vec![upper, lower]);
    }
}
