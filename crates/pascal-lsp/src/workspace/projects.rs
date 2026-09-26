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
use std::fmt;
use std::path::{Path, PathBuf};

const MAX_CONFIGURATION_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectContextInfo {
    pub(crate) scope_uri: Option<Url>,
    pub(crate) candidates: Vec<Url>,
    pub(crate) selected_project_uri: Option<Url>,
    pub(crate) selection_mode: String,
    pub(crate) selected_installation_id: Option<String>,
    pub(crate) installation_selection_mode: String,
    pub(crate) installation_candidates: Vec<String>,
    pub(crate) main_source_uri: Option<Url>,
    pub(crate) installation_config_uris: Vec<Url>,
    pub(crate) path_issues: Vec<serde_json::Value>,
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

/// Admission-time immutable authority for project protocol workers.
pub(crate) struct ProjectOperationSnapshot {
    workspace: Workspace,
    source_generation: u64,
    configuration_generation: u64,
    expected_installation: Option<String>,
}

impl fmt::Debug for ProjectOperationSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProjectOperationSnapshot")
            .field("source_generation", &self.source_generation)
            .field("configuration_generation", &self.configuration_generation)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub(crate) struct ProjectContextPreparation {
    uri: Url,
    key: ContextKey,
    state: super::ContextState,
    owner: Option<super::KnownDocumentOwner>,
    pub(crate) info: ProjectContextInfo,
}

impl ProjectOperationSnapshot {
    pub(crate) fn generations(&self) -> (u64, u64) {
        (self.source_generation, self.configuration_generation)
    }

    pub(crate) fn expected_installation(&self) -> Option<&str> {
        self.expected_installation.as_deref()
    }

    pub(crate) fn workspace_mut(&mut self) -> &mut Workspace {
        &mut self.workspace
    }
}

impl Workspace {
    pub(crate) fn project_operation_snapshot(
        &self,
        project_path: Option<&Path>,
    ) -> ProjectOperationSnapshot {
        let options = self.options.clone();
        let workspace = Workspace {
            options: options.clone(),
            overrides: self.overrides.clone(),
            roots: self
                .roots
                .iter()
                .map(|root| super::WorkspaceRoot::new(root.path.clone(), &options))
                .collect(),
            project_selections: self.project_selections.clone(),
            installation_selections: self.installation_selections.clone(),
            contexts: self.contexts.clone(),
            document_contexts: self.document_contexts.clone(),
            open_document_contexts: self.open_document_contexts.clone(),
            document_owners: self.document_owners.clone(),
            deleted_overrides: self.deleted_overrides.clone(),
            source_change_generations: self.source_change_generations.clone(),
            configuration_change_generations: self.configuration_change_generations.clone(),
            global_source_change_generation: self.global_source_change_generation,
            global_configuration_change_generation: self.global_configuration_change_generation,
            source_generation: self.source_generation,
            configuration_generation: self.configuration_generation,
            ..Workspace::default()
        };
        let expected_installation =
            project_path.and_then(|path| self.installation_selections.get(path).cloned());
        ProjectOperationSnapshot {
            workspace,
            source_generation: self.source_generation,
            configuration_generation: self.configuration_generation,
            expected_installation,
        }
    }

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
        self.project_context_with_cancel(uri, None)
    }

    pub(crate) fn project_context_with_cancel(
        &mut self,
        uri: &Url,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<ProjectContextInfo, String> {
        self.prepare_project_context_with_cancel(uri, cancel)
            .map(|prepared| prepared.info)
    }

    pub(crate) fn prepare_project_context_with_cancel(
        &mut self,
        uri: &Url,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<ProjectContextPreparation, String> {
        let uri = canonical_file_uri(uri);
        let path = document_path(&uri)?;
        let context_key = self.context_for_uri_with_cancel_and_budget(&uri, cancel, None)?;
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

        let (installation_candidates, installation_selection_mode) =
            if let Some(project_file) = context.project_file.as_deref() {
                self.installation_selection_snapshot(project_file)
                    .map(|(_, candidates, _)| {
                        let mode = if self.installation_selections.contains_key(project_file) {
                            "session"
                        } else {
                            "configured"
                        };
                        (candidates, mode.to_string())
                    })
                    .unwrap_or_else(|_| (Vec::new(), "automatic".to_string()))
            } else {
                (Vec::new(), "automatic".to_string())
            };
        let path_issues = context
            .path_issues
            .iter()
            .map(|issue| {
                serde_json::json!({
                    "kind": format!("{:?}", issue.kind),
                    "sourceUri": file_uri(&issue.source_file),
                    "property": issue.property,
                    "raw": issue.raw,
                    "path": issue.path.as_deref().and_then(file_uri),
                    "unitName": issue.unit_name,
                    "provenance": format!("{:?}", issue.provenance),
                })
            })
            .collect();

        let info = ProjectContextInfo {
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
            installation_selection_mode,
            installation_candidates,
            main_source_uri: context.main_source.as_deref().and_then(file_uri),
            installation_config_uris: context
                .installation_config_files
                .iter()
                .filter_map(|path| file_uri(path))
                .collect(),
            path_issues,
            lint_config_uri,
            fmt_config_uri,
            warnings,
        };
        let state = self
            .contexts
            .get(&context_key)
            .cloned()
            .ok_or_else(|| format!("prepared project context was not retained for {uri}"))?;
        let owner = self.document_owners.get(&uri).cloned();
        Ok(ProjectContextPreparation {
            uri,
            key: context_key,
            state,
            owner,
            info,
        })
    }

    pub(crate) fn apply_project_context_preparation(
        &mut self,
        prepared: ProjectContextPreparation,
    ) {
        let ProjectContextPreparation {
            uri,
            key,
            state,
            owner,
            info: _,
        } = prepared;
        let watched_paths = state.watched_paths.keys().cloned().collect::<Vec<_>>();
        self.contexts.entry(key.clone()).or_insert(state);
        self.document_contexts.insert(uri.clone(), key.clone());
        if let Some(owner) = owner {
            self.document_owners.insert(uri.clone(), owner);
        }
        self.watch_context_paths(&key, watched_paths);
        self.refresh_known_owners_for_context(&key);
    }

    pub fn installation_context(
        &mut self,
        project_uri: &Url,
    ) -> Result<InstallationContextInfo, String> {
        let project_path = project_path(project_uri)?;
        self.validate_project_scope(&project_path)?;
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
        self.select_installation_inner(project_uri, installation_id, None)
    }

    pub(crate) fn select_installation_with_cancel(
        &mut self,
        project_uri: &Url,
        installation_id: Option<&str>,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<InstallationContextInfo, String> {
        self.select_installation_inner(project_uri, installation_id, Some(cancel))
    }

    fn select_installation_inner(
        &mut self,
        project_uri: &Url,
        installation_id: Option<&str>,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<InstallationContextInfo, String> {
        let project_path = project_path(project_uri)?;
        self.validate_project_scope(&project_path)?;
        check_project_operation_cancel(cancel)?;
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
        let validation = match cancel {
            Some(cancel) => {
                pascal_project::discover_with_selections_and_observations_with_cancel_and_overrides(
                    &project_path,
                    &roots,
                    &options,
                    &self.project_selections,
                    &self.overrides,
                    &self.options.exclude,
                    cancel,
                )?
                .context
            }
            None => discover_with_selections(
                &project_path,
                &roots,
                &options,
                &self.project_selections,
                &self.overrides,
                &self.options.exclude,
            )?,
        };
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
        check_project_operation_cancel(cancel)?;
        self.installation_context(project_uri)
    }

    /// Commit a worker-validated installation choice. No configuration or
    /// project discovery is performed on the coordinator thread here.
    pub(crate) fn commit_prepared_installation_selection(
        &mut self,
        project_path: &Path,
        installation_id: Option<&str>,
        expected_installation: Option<&str>,
    ) -> Result<(), String> {
        if self
            .installation_selections
            .get(project_path)
            .map(String::as_str)
            != expected_installation
        {
            return Err("installation selection changed while the request was running".to_string());
        }
        if !self
            .workspace_root_paths()
            .iter()
            .any(|root| crate::workspace::path_starts_with_native(project_path, root))
        {
            return Err("project is no longer in the configured workspace scope".to_string());
        }
        match installation_id {
            Some(id) => {
                self.installation_selections
                    .insert(project_path.to_path_buf(), id.to_string());
            }
            None => {
                self.installation_selections.remove(project_path);
            }
        }
        let keys = self
            .contexts
            .keys()
            .filter(|key| {
                key.project_file
                    .as_deref()
                    .is_some_and(|path| project_paths_equal(path, project_path))
            })
            .cloned()
            .collect::<HashSet<_>>();
        self.bump_source_generation();
        self.bump_configuration_generation();
        self.invalidate_selection_contexts(&keys, None, None)
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
        let mut candidates = configuration.installation_ids();
        candidates.sort_by_key(|id| id.to_ascii_lowercase());
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

    fn validate_project_scope(&self, project_path: &Path) -> Result<(), String> {
        let roots = self.workspace_root_paths();
        if roots
            .iter()
            .any(|root| crate::workspace::path_starts_with_native(project_path, root))
        {
            Ok(())
        } else {
            Err(format!(
                "project is outside the configured workspace scope: {}",
                project_path.display()
            ))
        }
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

fn check_project_operation_cancel(
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<(), String> {
    if cancel.is_some_and(|cancel| cancel.load(std::sync::atomic::Ordering::Acquire)) {
        Err("request cancelled".to_string())
    } else {
        Ok(())
    }
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
