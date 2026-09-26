//! Runtime Delphi project selection and project-context protocol data.

use super::{
    ContextKey, OwnerOrigin, Workspace, absolute_path, canonical_file_uri,
    is_analyzable_source_path, path_stamp,
};
use crate::configuration::{config_directories, resolve_fmt, resolve_lint};
use lsp_types::Url;
use pascal_project::{
    InstallationSelection, discover_with_selections, project_candidates, runtime_project_selection,
    selected_project_is_current,
};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const MAX_CONFIGURATION_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectContextInfo {
    pub(crate) scope_uri: Option<Url>,
    pub(crate) candidates: Vec<Url>,
    pub(crate) selected_project_uri: Option<Url>,
    pub(crate) selection_mode: String,
    pub(crate) selected_installation_id: Option<String>,
    pub(crate) lint_config_uri: Option<Url>,
    pub(crate) fmt_config_uri: Option<Url>,
    pub(crate) warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallationContextInfo {
    pub project_uri: Url,
    pub candidates: Vec<String>,
    pub selected_installation_id: Option<String>,
    pub selection_mode: String,
    pub warnings: Vec<String>,
}

impl Workspace {
    pub(super) fn selection_uris_for_scope(&self, scope: &Path, requester: &Url) -> HashSet<Url> {
        let mut affected = HashSet::from([requester.clone()]);
        let belongs = |uri: &Url, key: &ContextKey| {
            let is_under_scope = uri
                .to_file_path()
                .ok()
                .map(absolute_path)
                .is_some_and(|path| crate::workspace::path_starts_with_native(&path, scope));
            is_under_scope
                || key
                    .selection_scope
                    .as_deref()
                    .is_some_and(|candidate| project_paths_equal(candidate, scope))
                || key
                    .project_scope
                    .as_deref()
                    .is_some_and(|candidate| project_paths_equal(candidate, scope))
        };
        for (uri, key) in self
            .document_contexts
            .iter()
            .chain(self.open_document_contexts.iter())
        {
            if belongs(uri, key) {
                affected.insert(uri.clone());
            }
        }
        for (uri, owner) in &self.document_owners {
            if belongs(uri, &owner.key) {
                affected.insert(uri.clone());
            }
        }
        affected
    }

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
            selected_installation_id: context.installation_selection.as_ref().and_then(
                |selection| match selection {
                    InstallationSelection::Selected { id, .. } => Some(id.clone()),
                    _ => None,
                },
            ),
            lint_config_uri,
            fmt_config_uri,
            warnings,
        })
    }

    pub fn installation_context(
        &mut self,
        project_uri: &Url,
    ) -> Result<InstallationContextInfo, String> {
        let project_path = project_path(project_uri)?;
        let (project, candidates, selected) =
            self.installation_selection_snapshot(&project_path)?;
        let selection_is_valid = selected.as_ref().is_none_or(|id| {
            candidates
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(id))
        });
        let selection_mode = if !selection_is_valid {
            "invalid"
        } else if self.installation_selections.contains_key(&project_path) {
            "session"
        } else if project.configured_installation_for(&project_path).is_some() {
            "configured"
        } else {
            "automatic"
        };
        Ok(InstallationContextInfo {
            project_uri: canonical_file_uri(project_uri),
            candidates,
            selected_installation_id: selection_is_valid.then(|| selected.clone()).flatten(),
            selection_mode: selection_mode.to_string(),
            warnings: if selection_is_valid {
                Vec::new()
            } else {
                vec![format!(
                    "selected Delphi installation `{}` is not currently configured for this project",
                    selected.as_deref().unwrap_or_default()
                )]
            },
        })
    }

    pub fn select_installation(
        &mut self,
        project_uri: &Url,
        installation_id: Option<&str>,
    ) -> Result<InstallationContextInfo, String> {
        let project_path = project_path(project_uri)?;
        let (config, candidates, _) = self.installation_selection_snapshot(&project_path)?;
        if let Some(id) = installation_id {
            config.profile(id)?;
            if !candidates
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(id))
            {
                return Err(format!("unknown Delphi installation `{id}`"));
            }
        }

        // Validate/evaluate tentatively before changing the live selection.
        let mut selections = self.installation_selections.clone();
        if let Some(id) = installation_id {
            selections.insert(project_path.clone(), id.to_string());
        } else {
            selections.remove(&project_path);
        }
        let roots = self.workspace_root_paths();
        let options = self.project_options_with_installations(selections.clone());
        let validation = discover_with_selections(
            &project_path,
            &roots,
            &options,
            &self.project_selections,
            &self.overrides,
            &self.options.exclude,
        )?;
        if validation
            .project_file
            .as_deref()
            .is_none_or(|path| !project_paths_equal(path, &project_path))
        {
            return Err(format!(
                "project is no longer valid: {}",
                project_path.display()
            ));
        }

        self.installation_selections = selections;
        let keys = self
            .contexts
            .keys()
            .filter(|key| {
                key.project_file
                    .as_deref()
                    .is_some_and(|path| project_paths_equal(path, &project_path))
            })
            .cloned()
            .collect::<HashSet<_>>();
        self.bump_source_generation();
        self.bump_configuration_generation();
        self.invalidate_selection_contexts(&keys, None, None)?;
        self.installation_context(project_uri)
    }

    fn installation_selection_snapshot(
        &self,
        project_path: &Path,
    ) -> Result<
        (
            pascal_project::installation_config::ProjectConfiguration,
            Vec<String>,
            Option<String>,
        ),
        String,
    > {
        let roots = self.workspace_root_paths();
        let configuration = self.overrides.configuration_for(
            roots
                .iter()
                .find(|root| project_path.starts_with(root))
                .map(PathBuf::as_path),
            Some(project_path),
        )?;
        let candidates = configuration.installation_ids();
        let selected = self
            .installation_selections
            .get(project_path)
            .cloned()
            .or_else(|| {
                configuration
                    .configured_installation_for(project_path)
                    .map(str::to_string)
            })
            .or_else(|| {
                self.contexts.iter().find_map(|(key, state)| {
                    key.project_file
                        .as_deref()
                        .filter(|path| project_paths_equal(path, project_path))
                        .and(state.context.installation_selection.as_ref())
                        .and_then(|selection| match selection {
                            InstallationSelection::Selected { id, .. } => Some(id.clone()),
                            _ => None,
                        })
                })
            });
        Ok((configuration, candidates, selected))
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

        let affected_uris = self.selection_uris_for_scope(&scope, &uri);
        self.project_selections = tentative;
        self.bump_source_generation();
        self.bump_configuration_generation();
        self.invalidate_project_selection_uris(&affected_uris, None, None)?;
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
}

fn document_path(uri: &Url) -> Result<PathBuf, String> {
    let path = uri
        .to_file_path()
        .map_err(|_| format!("project context requires a file URI: {uri}"))?;
    let path = absolute_path(path);
    if !is_analyzable_source_path(&path) {
        return Err(format!(
            "unsupported Pascal document path: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn project_path(uri: &Url) -> Result<PathBuf, String> {
    let path = uri
        .to_file_path()
        .map_err(|_| format!("project URI must be a file URI: {uri}"))?;
    let path = absolute_path(path);
    if !path.is_file()
        || !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("dproj"))
    {
        return Err(format!(
            "project is not a current .dproj file: {}",
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
