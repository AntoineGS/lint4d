//! Runtime Delphi project selection and project-context protocol data.

use super::{
    ContextKey, OwnerOrigin, Workspace, absolute_path, canonical_file_uri, is_pascal_path,
    path_stamp,
};
use crate::NavigationIndex;
use crate::configuration::{config_directories, resolve_fmt, resolve_lint};
use crate::project::{
    discover_with_selections, project_candidates, runtime_project_selection,
    selected_project_is_current,
};
use lsp_types::Url;
use serde::Serialize;
use std::path::{Path, PathBuf};

const MAX_CONFIGURATION_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectContextInfo {
    pub(crate) scope_uri: Option<Url>,
    pub(crate) candidates: Vec<Url>,
    pub(crate) selected_project_uri: Option<Url>,
    pub(crate) selection_mode: String,
    pub(crate) lint_config_uri: Option<Url>,
    pub(crate) fmt_config_uri: Option<Url>,
    pub(crate) warnings: Vec<String>,
}

impl Workspace {
    pub fn project_context(&mut self, uri: &Url) -> Result<ProjectContextInfo, String> {
        let uri = canonical_file_uri(uri);
        let path = document_path(&uri)?;
        let context_key = self.context_for_uri(&uri)?;
        let context = self
            .contexts
            .get(&context_key)
            .map(|state| state.context.clone())
            .ok_or_else(|| format!("project context was not retained for {uri}"))?;
        let roots = self.workspace_root_paths();
        let candidates = project_candidates(&path, &roots)?;
        let runtime_selection =
            runtime_project_selection(&path, &candidates, &self.project_selections);
        let retained_selection = context_key
            .selection_scope
            .as_ref()
            .zip(context_key.selection_project.as_ref())
            .map(|(scope, selected)| (scope.clone(), selected.clone()));
        let effective_selection = runtime_selection.or(retained_selection);
        let selection_is_valid = effective_selection
            .as_ref()
            .is_none_or(|(scope, selected)| {
                selected_project_is_current(scope, selected).unwrap_or(false)
            });
        let scope =
            self.configuration_project_directory(&path, &context, Some(&context_key), &candidates);

        let mut warnings = context.warnings.clone();
        let (lint_config_uri, fmt_config_uri, checked_paths) =
            self.resolve_configuration(&path, scope.as_deref(), &mut warnings);
        self.watch_context_paths(&context_key, checked_paths);
        self.refresh_known_owners_for_context(&context_key);

        let selection_mode = if !selection_is_valid {
            "invalid"
        } else if effective_selection.is_some() {
            "directory"
        } else if self.options.project_file.is_some() {
            "configured"
        } else if context.project_file.is_some() {
            "automatic"
        } else if !candidates.files.is_empty() {
            "ambiguous"
        } else {
            "standalone"
        };

        Ok(ProjectContextInfo {
            scope_uri: scope.as_deref().and_then(file_uri),
            candidates: candidates
                .files
                .iter()
                .filter_map(|path| file_uri(path))
                .collect(),
            selected_project_uri: selection_is_valid
                .then(|| {
                    context
                        .project_file
                        .as_ref()
                        .and_then(|path| file_uri(path))
                })
                .flatten(),
            selection_mode: selection_mode.to_string(),
            lint_config_uri,
            fmt_config_uri,
            warnings,
        })
    }

    pub fn select_project(
        &mut self,
        uri: &Url,
        project: Option<&Url>,
    ) -> Result<ProjectContextInfo, String> {
        let uri = canonical_file_uri(uri);
        let path = document_path(&uri)?;
        let roots = self.workspace_root_paths();
        let candidates = project_candidates(&path, &roots)?;
        let current_selection =
            runtime_project_selection(&path, &candidates, &self.project_selections);
        let candidate_scope = candidates.directory.clone();
        let retained_owner_scope = self
            .document_owners
            .get(&uri)
            .filter(|owner| owner.origin == OwnerOrigin::Inherited)
            .and_then(|owner| owner.key.selection_scope.clone());
        let reset_scope = current_selection
            .as_ref()
            .map(|(scope, _)| scope.clone())
            .or_else(|| candidate_scope.clone())
            .or(retained_owner_scope);

        let selected = match project {
            Some(project_uri) => {
                let project_path = project_uri
                    .to_file_path()
                    .map_err(|_| format!("project selection must use a file URI: {project_uri}"))?;
                let project_path = absolute_path(project_path);
                let Some(scope) = candidate_scope else {
                    return Err(format!(
                        "project selection is unavailable because no .dproj candidates were found for {uri}"
                    ));
                };
                let Some(candidate) = candidates
                    .files
                    .iter()
                    .find(|candidate| project_paths_equal(candidate, &project_path))
                    .cloned()
                else {
                    return Err(format!(
                        "project selection is not a current candidate: {project_uri}"
                    ));
                };
                Some((scope, candidate))
            }
            None => reset_scope.map(|scope| (scope, PathBuf::new())),
        };

        let Some((scope, selected)) = selected else {
            return self.project_context(&uri);
        };

        let mut tentative = self.project_selections.clone();
        if project.is_some() {
            tentative.insert(scope.clone(), selected);
        } else {
            tentative.remove(&scope);
        }

        // Discover before mutating the live workspace. A malformed or otherwise
        // unavailable candidate must not leave a half-installed override.
        discover_with_selections(
            &path,
            &roots,
            &self.project_options(),
            &tentative,
            &self.overrides,
            &self.options.exclude,
        )?;

        self.project_selections = tentative;
        self.document_owners.remove(&uri);
        self.owner_last_used.remove(&uri);
        self.invalidate_project_selection();
        self.project_context(&uri)
    }

    fn resolve_configuration(
        &self,
        path: &Path,
        project_directory: Option<&Path>,
        warnings: &mut Vec<String>,
    ) -> (Option<Url>, Option<Url>, Vec<PathBuf>) {
        let roots = self.workspace_root_paths();
        let directories = match config_directories(path, project_directory, &roots) {
            Ok(directories) => directories,
            Err(error) => {
                warnings.push(format!(
                    "could not resolve configuration directories: {error}"
                ));
                return (None, None, Vec::new());
            }
        };
        let mut checked_paths = Vec::new();
        let lint = match resolve_lint(&directories, MAX_CONFIGURATION_BYTES) {
            Ok(config) => {
                checked_paths.extend(config.checked_paths.iter().cloned());
                config.path.and_then(|path| file_uri(&path))
            }
            Err(error) => {
                warnings.push(error);
                checked_paths.extend(
                    directories
                        .iter()
                        .map(|directory| directory.join(".lint4d.toml")),
                );
                None
            }
        };
        let fmt = match resolve_fmt(&directories, MAX_CONFIGURATION_BYTES) {
            Ok(config) => {
                checked_paths.extend(config.checked_paths.iter().cloned());
                config.path.and_then(|path| file_uri(&path))
            }
            Err(error) => {
                warnings.push(error);
                checked_paths.extend(
                    directories
                        .iter()
                        .map(|directory| directory.join(".fmt4d.toml")),
                );
                None
            }
        };
        checked_paths.sort();
        checked_paths.dedup();
        (lint, fmt, checked_paths)
    }

    fn watch_context_paths(&mut self, key: &ContextKey, paths: Vec<PathBuf>) {
        let Some(state) = self.contexts.get_mut(key) else {
            return;
        };
        for path in paths {
            state
                .watched_paths
                .entry(path.clone())
                .or_insert_with(|| path_stamp(&path));
        }
    }

    fn invalidate_project_selection(&mut self) {
        self.bump_source_generation();
        self.bump_configuration_generation();
        self.mark_global_change();
        self.index = NavigationIndex::new();
        self.cached_documents.clear();
        self.indexed_files.clear();
        self.indexed_sizes.clear();
        self.indexed_bytes = 0;
        self.disk_stamps.clear();
        self.last_used.clear();
        self.contexts.clear();
        self.document_contexts.clear();
        self.open_document_contexts.clear();
        self.clear_legacy_route_proofs();
        self.directory_catalogues.clear();
        self.filename_catalogues.clear();
        self.package_catalogues.clear();
        self.package_metadata_cache.clear();
        self.file_cap_warning_sent = false;
        self.total_cap_warning_sent = false;
        self.pending_diagnostics.clear();
        let open_documents = self
            .open_documents
            .iter()
            .filter_map(|(uri, document)| document.text.as_ref().map(|_| uri.clone()))
            .collect::<Vec<_>>();
        for uri in open_documents {
            self.schedule_diagnostics(uri);
        }
    }
}

fn document_path(uri: &Url) -> Result<PathBuf, String> {
    let path = uri
        .to_file_path()
        .map_err(|_| format!("project context requires a file URI: {uri}"))?;
    let path = absolute_path(path);
    if !is_pascal_path(&path) {
        return Err(format!(
            "unsupported Pascal document path: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn file_uri(path: &Path) -> Option<Url> {
    Url::from_file_path(path).ok()
}

fn project_paths_equal(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}
