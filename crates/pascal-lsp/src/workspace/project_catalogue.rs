//! Explicit, filename-only browsing of Delphi project files.

use super::{Workspace, absolute_path, check_workspace_cancel, path_starts_with_native};
use lsp_types::Url;
use serde::Serialize;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

const MAX_CATALOGUE_VISITS: usize = 10_000;
const MAX_CATALOGUE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CATALOGUE_TIME: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCatalogueItem {
    pub project_uri: Url,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCatalogue {
    pub projects: Vec<ProjectCatalogueItem>,
    pub complete: bool,
    pub warnings: Vec<String>,
}

impl Workspace {
    /// Enumerate `.dproj` filenames under the workspace root containing `anchor`.
    /// Project files are not opened; a listed candidate must be revalidated when selected.
    pub fn list_projects(
        &self,
        anchor: &Url,
        cancel: &AtomicBool,
    ) -> Result<ProjectCatalogue, String> {
        check_workspace_cancel(Some(cancel))?;
        let anchor_path = anchor
            .to_file_path()
            .map_err(|_| format!("project catalogue requires a file URI: {anchor}"))?;
        let anchor_path = absolute_path(anchor_path);
        let root = self
            .roots
            .iter()
            .filter(|root| path_starts_with_native(&anchor_path, &root.path))
            .max_by_key(|root| root.path.components().count())
            .ok_or_else(|| {
                format!(
                    "project catalogue anchor is outside configured workspace roots: {}",
                    anchor_path.display()
                )
            })?;

        let started = Instant::now();
        let mut visited = 0usize;
        let mut bytes = 0usize;
        let mut projects = Vec::new();
        let mut warnings = Vec::new();
        let mut complete = true;
        let root_path = &root.path;
        let walker = WalkDir::new(root_path)
            .follow_links(false)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|entry| {
                if entry.depth() == 0 {
                    return true;
                }
                let path = entry.path();
                !entry.file_type().is_symlink() && !root.excludes.is_excluded(path, root_path)
            });

        let mut walker = walker;
        loop {
            check_workspace_cancel(Some(cancel))?;
            if started.elapsed() >= MAX_CATALOGUE_TIME {
                complete = false;
                warnings.push("project catalogue time limit reached".to_owned());
                break;
            }
            if visited >= MAX_CATALOGUE_VISITS {
                complete = false;
                warnings.push("project catalogue entry limit reached".to_owned());
                break;
            }
            let Some(entry) = walker.next() else {
                break;
            };
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    complete = false;
                    if warnings.is_empty() {
                        warnings.push("project catalogue could not visit some entries".to_owned());
                    }
                    continue;
                }
            };
            visited = visited.saturating_add(1);
            if started.elapsed() >= MAX_CATALOGUE_TIME {
                complete = false;
                warnings.push("project catalogue time limit reached".to_owned());
                break;
            }
            if !entry.file_type().is_file()
                || !entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("dproj"))
            {
                continue;
            }
            let relative = entry.path().strip_prefix(root_path).unwrap_or(entry.path());
            let label = relative.to_string_lossy().replace('\\', "/");
            let project_uri = Url::from_file_path(entry.path()).map_err(|_| {
                format!(
                    "could not convert project path to URI: {}",
                    entry.path().display()
                )
            })?;
            let retained = label.len().saturating_add(project_uri.as_str().len());
            if bytes.saturating_add(retained) > MAX_CATALOGUE_BYTES {
                complete = false;
                warnings.push("project catalogue retained-byte limit reached".to_owned());
                break;
            }
            bytes += retained;
            projects.push(ProjectCatalogueItem { project_uri, label });
        }
        check_workspace_cancel(Some(cancel))?;
        projects.sort_by(|left, right| left.label.cmp(&right.label));
        Ok(ProjectCatalogue {
            projects,
            complete,
            warnings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::WorkspaceOptions;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    fn write(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"not read by catalogue").unwrap();
    }

    #[test]
    fn lists_projects_by_distinct_relative_path_and_honors_exclusions() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(&root.join("apps/Two/App.dproj"));
        write(&root.join("apps/One/App.dproj"));
        write(&root.join("ignored/App.dproj"));
        let options = WorkspaceOptions {
            exclude: vec!["ignored/**".to_owned()],
            ..WorkspaceOptions::default()
        };
        let workspace = Workspace::new(vec![root.to_path_buf()], options);
        let catalogue = workspace
            .list_projects(&Url::from_file_path(root).unwrap(), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(
            catalogue
                .projects
                .iter()
                .map(|p| p.label.as_str())
                .collect::<Vec<_>>(),
            vec!["apps/One/App.dproj", "apps/Two/App.dproj"]
        );
        assert!(catalogue.complete);
    }

    #[test]
    fn cancellation_is_an_explicit_error() {
        let temp = tempfile::tempdir().unwrap();
        let workspace =
            Workspace::new(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
        let cancel = AtomicBool::new(true);
        assert!(
            workspace
                .list_projects(&Url::from_file_path(temp.path()).unwrap(), &cancel)
                .unwrap_err()
                .contains("cancel")
        );
    }
}
