use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

#[derive(Debug)]
pub(crate) struct ResolvedConfig<T> {
    pub(crate) value: T,
    pub(crate) path: Option<PathBuf>,
    pub(crate) checked_paths: Vec<PathBuf>,
    pub(crate) checked_contents: Vec<Option<Vec<u8>>>,
    pub(crate) bytes: Option<Vec<u8>>,
}

pub(crate) fn config_directories(
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

pub(crate) fn resolve_lint(
    directories: &[PathBuf],
    max_bytes: usize,
) -> Result<ResolvedConfig<lint4d::config::Config>, String> {
    let read = read_first_config(directories, ".lint4d.toml", max_bytes)?;
    let value = match read.bytes.as_deref() {
        Some(bytes) => {
            let path = read
                .path
                .as_deref()
                .expect("a configuration payload has a path");
            let text = configuration_text(bytes, path)?;
            text.parse::<lint4d::config::Config>()
                .map_err(|error| parse_error(path, error))?
        }
        None => ""
            .parse::<lint4d::config::Config>()
            .map_err(|error| format!("failed to parse default lint4d configuration: {error}"))?,
    };

    Ok(ResolvedConfig {
        value,
        path: read.path,
        checked_paths: read.checked_paths,
        checked_contents: read.checked_contents,
        bytes: read.bytes,
    })
}

pub(crate) fn resolve_fmt(
    directories: &[PathBuf],
    max_bytes: usize,
) -> Result<ResolvedConfig<fmt4d::config::FmtConfig>, String> {
    let read = read_first_config(directories, ".fmt4d.toml", max_bytes)?;
    let project_root = read
        .path
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    let value = match read.bytes.as_deref() {
        Some(bytes) => {
            let path = read
                .path
                .as_deref()
                .expect("a configuration payload has a path");
            let text = configuration_text(bytes, path)?;
            fmt4d::config::FmtConfig::from_toml(text).map_err(|error| parse_error(path, error))?
        }
        None => fmt4d::config::FmtConfig::from_toml("")
            .map_err(|error| format!("failed to parse default fmt4d configuration: {error}"))?,
    };
    let value = if read.path.is_none() {
        fmt4d::config::FmtConfig {
            project_root: directories.first().cloned(),
            ..value
        }
    } else {
        fmt4d::config::FmtConfig {
            project_root,
            ..value
        }
    };

    Ok(ResolvedConfig {
        value,
        path: read.path,
        checked_paths: read.checked_paths,
        checked_contents: read.checked_contents,
        bytes: read.bytes,
    })
}

#[derive(Debug)]
struct ReadConfig {
    path: Option<PathBuf>,
    checked_paths: Vec<PathBuf>,
    checked_contents: Vec<Option<Vec<u8>>>,
    bytes: Option<Vec<u8>>,
}

fn read_first_config(
    directories: &[PathBuf],
    filename: &str,
    max_bytes: usize,
) -> Result<ReadConfig, String> {
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
        return Ok(ReadConfig {
            path: Some(candidate),
            checked_paths,
            checked_contents: {
                checked_contents.push(Some(bytes.clone()));
                checked_contents
            },
            bytes: Some(bytes),
        });
    }

    Ok(ReadConfig {
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

fn configuration_text<'a>(bytes: &'a [u8], path: &Path) -> Result<&'a str, String> {
    std::str::from_utf8(bytes)
        .map_err(|error| format!("invalid UTF-8 in {}: {error}", path.display()))
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

fn parse_error<E: std::fmt::Display>(path: &Path, error: E) -> String {
    format!(
        "could not parse configuration candidate {}: {error}",
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[cfg(unix)]
    use std::os::unix::fs::{FileTypeExt, PermissionsExt, symlink};
    #[cfg(unix)]
    use std::process::Command;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    fn config_text(style: &str) -> String {
        format!("[rules.naming]\nconstant_style = '{style}'\n")
    }

    fn write_file(directory: &Path, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn config_sidecar_wins_independently_for_each_tool() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let app = root.join("app");
        std::fs::create_dir(&app).unwrap();
        std::fs::write(
            root.join(".lint4d.toml"),
            "[rules.naming]\nconstant_style = 'PascalCase'\n",
        )
        .unwrap();
        std::fs::write(
            app.join(".lint4d.toml"),
            "[rules.naming]\nconstant_style = 'UPPER_CASE'\n",
        )
        .unwrap();
        std::fs::write(root.join(".fmt4d.toml"), "[format]\nindent_size = 4\n").unwrap();
        let dirs = vec![app.clone(), root.to_path_buf()];
        let lint = resolve_lint(&dirs, 1024 * 1024).unwrap();
        let fmt = resolve_fmt(&dirs, 1024 * 1024).unwrap();
        assert_eq!(lint.value.constant_style(), "UPPER_CASE");
        assert_eq!(lint.path, Some(app.join(".lint4d.toml")));
        assert_eq!(fmt.value.indent_size, 4);
        assert_eq!(fmt.path, Some(root.join(".fmt4d.toml")));
        assert_eq!(fmt.checked_paths[0], app.join(".fmt4d.toml"));
    }

    #[test]
    fn project_sidecar_precedes_source_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let project = root.join("project");
        let source = project.join("src");
        std::fs::create_dir_all(&source).unwrap();
        write_file(&project, ".lint4d.toml", config_text("UPPER_CASE"));
        write_file(&source, ".lint4d.toml", config_text("PascalCase"));

        let document = source.join("Main.pas");
        let directories = config_directories(&document, Some(&project), &[]).unwrap();
        assert_eq!(directories, vec![project.clone()]);
        let config = resolve_lint(&directories, 1024 * 1024).unwrap();
        assert_eq!(config.value.constant_style(), "UPPER_CASE");
        assert_eq!(config.path, Some(project.join(".lint4d.toml")));
        assert!(!config.checked_paths.contains(&source.join(".lint4d.toml")));
    }

    #[test]
    fn projectless_fallback_ignores_intermediate_source_sidecars() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let source = workspace.join("src");
        std::fs::create_dir_all(&source).unwrap();
        write_file(&workspace, ".fmt4d.toml", "[format]\nindent_size = 4\n");
        write_file(&source, ".fmt4d.toml", "[format]\nindent_size = 8\n");

        let document = source.join("Main.pas");
        let directories =
            config_directories(&document, None, std::slice::from_ref(&workspace)).unwrap();
        assert_eq!(directories, vec![workspace.clone()]);
        let config = resolve_fmt(&directories, 1024 * 1024).unwrap();
        assert_eq!(config.value.indent_size, 4);
        assert!(!config.checked_paths.contains(&source.join(".fmt4d.toml")));
    }

    #[test]
    fn no_project_uses_the_longest_containing_workspace_root() {
        let temp = tempfile::tempdir().unwrap();
        let outer = temp.path().join("outer");
        let inner = outer.join("inner");
        let source = inner.join("src");
        std::fs::create_dir_all(&source).unwrap();
        write_file(&outer, ".lint4d.toml", config_text("PascalCase"));
        write_file(&inner, ".lint4d.toml", config_text("UPPER_CASE"));

        let document = source.join("Main.pas");
        let directories = config_directories(
            &document,
            None,
            &[outer.clone(), inner.clone(), outer.clone()],
        )
        .unwrap();
        assert_eq!(directories, vec![inner.clone()]);
        let config = resolve_lint(&directories, 1024 * 1024).unwrap();
        assert_eq!(config.value.constant_style(), "UPPER_CASE");
    }

    #[test]
    fn project_search_keeps_workspace_and_repository_order() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repository");
        let workspace = repository.join("workspace");
        let project = workspace.join("project");
        let source = project.join("src");
        std::fs::create_dir_all(&source).unwrap();
        write_file(&repository, ".git", "gitdir: ../.git/worktrees/example\n");

        let directories = config_directories(
            &source.join("Main.pas"),
            Some(&project),
            std::slice::from_ref(&workspace),
        )
        .unwrap();
        assert_eq!(directories, vec![project, workspace, repository]);
    }

    #[test]
    fn project_directory_anchors_workspace_and_repository_fallbacks() {
        let temp = tempfile::tempdir().unwrap();
        let project_repository = temp.path().join("project-repository");
        let document_repository = temp.path().join("document-repository");
        let project = project_repository.join("project");
        let document_directory = document_repository.join("shared");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&document_directory).unwrap();
        std::fs::create_dir(project_repository.join(".git")).unwrap();
        std::fs::create_dir(document_repository.join(".git")).unwrap();
        write_file(
            &project_repository,
            ".fmt4d.toml",
            "[format]\nindent_size = 3\n",
        );
        write_file(
            &document_repository,
            ".fmt4d.toml",
            "[format]\nindent_size = 7\n",
        );

        let document = document_directory.join("Shared.pas");
        let directories = config_directories(
            &document,
            Some(&project),
            &[document_repository.clone(), project_repository.clone()],
        )
        .unwrap();
        assert_eq!(directories, vec![project, project_repository.clone()]);
        let config = resolve_fmt(&directories, 1024 * 1024).unwrap();
        assert_eq!(config.value.indent_size, 3);
        assert!(
            !config
                .checked_paths
                .contains(&document_repository.join(".fmt4d.toml"))
        );
    }

    #[test]
    fn git_worktree_marker_file_is_a_repository_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repository");
        let source = repository.join("src").join("nested");
        std::fs::create_dir_all(&source).unwrap();
        write_file(&repository, ".git", "gitdir: /outside/worktree\n");

        let directories = config_directories(&source.join("Main.pas"), None, &[]).unwrap();
        assert_eq!(directories, vec![repository]);
    }

    #[cfg(unix)]
    #[test]
    fn git_marker_inspection_errors_are_not_treated_as_absent() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repository");
        let source = repository.join("src");
        std::fs::create_dir_all(&source).unwrap();
        let marker = repository.join(".git");
        symlink(".git", &marker).unwrap();

        let error = config_directories(&source.join("Main.pas"), None, &[]).unwrap_err();
        assert!(error.contains(&marker.display().to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn git_marker_permission_errors_are_not_treated_as_absent() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repository");
        let source = repository.join("src");
        let blocked = temp.path().join("blocked");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir(&blocked).unwrap();
        let marker = repository.join(".git");
        symlink("../blocked/missing", &marker).unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let result = config_directories(&source.join("Main.pas"), None, &[]);
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
        let error = result.unwrap_err();
        assert!(error.contains(&marker.display().to_string()));
    }

    #[test]
    fn no_project_or_repository_falls_back_to_document_parent() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("src");
        std::fs::create_dir_all(&source).unwrap();
        let document = source.join("Main.pas");

        let directories = config_directories(&document, None, &[]).unwrap();
        assert_eq!(directories, vec![source]);
    }

    #[test]
    fn absent_candidates_return_defaults_and_every_checked_path() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let candidates = vec![first.clone(), second.clone()];

        let lint = resolve_lint(&candidates, 1024 * 1024).unwrap();
        assert_eq!(lint.value.constant_style(), "UPPER_CASE");
        assert_eq!(lint.path, None);
        assert_eq!(lint.bytes, None);
        assert_eq!(
            lint.checked_paths,
            vec![first.join(".lint4d.toml"), second.join(".lint4d.toml")]
        );

        let fmt = resolve_fmt(&candidates, 1024 * 1024).unwrap();
        assert_eq!(
            fmt.value.indent_size,
            fmt4d::config::FmtConfig::default().indent_size
        );
        assert_eq!(fmt.value.project_root, Some(first));
        assert_eq!(fmt.path, None);
        assert_eq!(fmt.bytes, None);
        assert_eq!(
            fmt.checked_paths,
            vec![
                temp.path().join("first/.fmt4d.toml"),
                second.join(".fmt4d.toml")
            ]
        );
    }

    #[test]
    fn malformed_higher_priority_config_is_not_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let first_path = write_file(&first, ".lint4d.toml", "[rules\n");
        write_file(&second, ".lint4d.toml", "version = 1\n");

        let error = resolve_lint(&[first, second], 1024 * 1024).unwrap_err();
        assert!(error.contains(&first_path.display().to_string()));
    }

    #[test]
    fn directory_named_like_fmt_config_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("config");
        std::fs::create_dir(&directory).unwrap();
        let candidate = directory.join(".fmt4d.toml");
        std::fs::create_dir(&candidate).unwrap();

        let error = resolve_fmt(std::slice::from_ref(&directory), 1024 * 1024).unwrap_err();
        assert!(error.contains(&candidate.display().to_string()));
    }

    #[test]
    fn malformed_utf8_is_rejected_with_the_candidate_path() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("config");
        std::fs::create_dir(&directory).unwrap();
        let candidate = write_file(&directory, ".lint4d.toml", [0xff, 0xfe]);

        let error = resolve_lint(std::slice::from_ref(&directory), 1024 * 1024).unwrap_err();
        assert!(error.contains(&candidate.display().to_string()));
    }

    #[test]
    fn configuration_size_limit_is_enforced() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("config");
        std::fs::create_dir(&directory).unwrap();
        let candidate = write_file(&directory, ".lint4d.toml", "version = 1\n");

        let error = resolve_lint(std::slice::from_ref(&directory), 4).unwrap_err();
        assert!(error.contains(&candidate.display().to_string()));
    }

    #[test]
    fn exact_and_zero_byte_limits_are_allowed_for_exactly_sized_files() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("config");
        std::fs::create_dir(&directory).unwrap();
        let empty = write_file(&directory, ".lint4d.toml", []);
        let resolved_empty = resolve_lint(std::slice::from_ref(&directory), 0).unwrap();
        assert_eq!(resolved_empty.path, Some(empty));
        assert_eq!(resolved_empty.bytes, Some(Vec::new()));

        let contents = b"version = 1\n";
        let exact = write_file(&directory, ".lint4d.toml", contents);
        let resolved_exact =
            resolve_lint(std::slice::from_ref(&directory), contents.len()).unwrap();
        assert_eq!(resolved_exact.path, Some(exact));
        assert_eq!(resolved_exact.bytes, Some(contents.to_vec()));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_config_symlinks_are_errors_for_both_tools() {
        let temp = tempfile::tempdir().unwrap();
        let high = temp.path().join("high");
        let low = temp.path().join("low");
        std::fs::create_dir(&high).unwrap();
        std::fs::create_dir(&low).unwrap();
        symlink("missing-lint-config", high.join(".lint4d.toml")).unwrap();
        symlink("missing-fmt-config", high.join(".fmt4d.toml")).unwrap();
        write_file(&low, ".lint4d.toml", "version = 1\n");
        write_file(&low, ".fmt4d.toml", "[format]\nindent_size = 4\n");

        let lint_candidate = high.join(".lint4d.toml");
        let lint_error = resolve_lint(&[high.clone(), low.clone()], 1024 * 1024).unwrap_err();
        assert!(lint_error.contains(&lint_candidate.display().to_string()));

        let fmt_candidate = high.join(".fmt4d.toml");
        let fmt_error = resolve_fmt(&[high, low], 1024 * 1024).unwrap_err();
        assert!(fmt_error.contains(&fmt_candidate.display().to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn fifo_config_candidates_are_rejected_without_blocking() {
        let child_mode = std::env::var_os("LINT4D_FIFO_CHILD").is_some();
        if child_mode {
            let lint_direct = PathBuf::from(std::env::var_os("LINT4D_FIFO_LINT_DIRECT").unwrap());
            let lint_link = PathBuf::from(std::env::var_os("LINT4D_FIFO_LINT_LINK").unwrap());
            let fmt_direct = PathBuf::from(std::env::var_os("LINT4D_FIFO_FMT_DIRECT").unwrap());
            let fmt_link = PathBuf::from(std::env::var_os("LINT4D_FIFO_FMT_LINK").unwrap());
            for directory in [lint_direct, lint_link] {
                let error = resolve_lint(std::slice::from_ref(&directory), 1024)
                    .err()
                    .unwrap();
                assert!(error.contains(".lint4d.toml"));
            }
            for directory in [fmt_direct, fmt_link] {
                let error = resolve_fmt(std::slice::from_ref(&directory), 1024)
                    .err()
                    .unwrap();
                assert!(error.contains(".fmt4d.toml"));
            }
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let lint_direct = temp.path().join("lint-direct");
        let lint_link = temp.path().join("lint-link");
        let fmt_direct = temp.path().join("fmt-direct");
        let fmt_link = temp.path().join("fmt-link");
        for directory in [&lint_direct, &lint_link, &fmt_direct, &fmt_link] {
            std::fs::create_dir(directory).unwrap();
        }

        let lint_fifo = lint_direct.join(".lint4d.toml");
        let fmt_fifo = fmt_direct.join(".fmt4d.toml");
        for fifo in [&lint_fifo, &fmt_fifo] {
            let status = Command::new("mkfifo").arg(fifo).status().unwrap();
            assert!(status.success(), "mkfifo failed for {}", fifo.display());
            assert!(
                std::fs::symlink_metadata(fifo)
                    .unwrap()
                    .file_type()
                    .is_fifo()
            );
        }
        let lint_link_path = lint_link.join(".lint4d.toml");
        let fmt_link_path = fmt_link.join(".fmt4d.toml");
        symlink(&lint_fifo, &lint_link_path).unwrap();
        symlink(&fmt_fifo, &fmt_link_path).unwrap();

        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "configuration::tests::fifo_config_candidates_are_rejected_without_blocking",
                "--nocapture",
            ])
            .env("LINT4D_FIFO_CHILD", "1")
            .env("LINT4D_FIFO_LINT_DIRECT", &lint_direct)
            .env("LINT4D_FIFO_LINT_LINK", &lint_link)
            .env("LINT4D_FIFO_FMT_DIRECT", &fmt_direct)
            .env("LINT4D_FIFO_FMT_LINK", &fmt_link)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "FIFO child exited with {status}");
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("FIFO configuration inspection blocked");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn existing_symlink_components_are_resolved_before_root_selection() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let real_subdirectory = real.join("sub");
        let real_project = real.join("project");
        let lexical_project = temp.path().join("project");
        std::fs::create_dir_all(&real_subdirectory).unwrap();
        std::fs::create_dir_all(real_project.join("src")).unwrap();
        std::fs::create_dir_all(lexical_project.join("src")).unwrap();
        let link = temp.path().join("link");
        symlink(&real_subdirectory, &link).unwrap();

        let document = link.join("..").join("project").join("src").join("Main.pas");
        let directories =
            config_directories(&document, None, &[real.clone(), temp.path().to_path_buf()])
                .unwrap();
        assert_eq!(directories, vec![real]);
    }

    #[test]
    fn fmt_resolution_does_not_merge_lower_priority_values() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        write_file(&root, ".fmt4d.toml", "[format]\nindent_size = 4\n");
        let project_path = write_file(&project, ".fmt4d.toml", "[format]\nmax_line_length = 99\n");

        let config = resolve_fmt(&[project.clone(), root], 1024 * 1024).unwrap();
        assert_eq!(config.value.max_line_length, 99);
        assert_eq!(
            config.value.indent_size,
            fmt4d::config::FmtConfig::default().indent_size
        );
        assert_eq!(config.path, Some(project_path));
        assert_eq!(
            config.bytes,
            Some(b"[format]\nmax_line_length = 99\n".to_vec())
        );
        assert_eq!(config.value.project_root, Some(project));
    }

    #[cfg(not(windows))]
    #[test]
    fn case_distinct_linux_directories_are_not_deduplicated() {
        let temp = tempfile::tempdir().unwrap();
        let upper = temp.path().join("Root");
        let lower = temp.path().join("root");
        std::fs::create_dir(&upper).unwrap();
        std::fs::create_dir(&lower).unwrap();

        let mut directories = Vec::new();
        add_unique_path(&mut directories, upper.clone());
        add_unique_path(&mut directories, lower.clone());
        assert_eq!(directories, vec![upper, lower]);
    }
}
