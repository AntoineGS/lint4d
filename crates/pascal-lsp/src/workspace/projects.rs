//! Runtime Delphi project selection and project-context protocol data.

use super::{
    ContextKey, OwnerOrigin, ReconciliationBudget, Workspace, absolute_path, canonical_file_uri,
    is_analyzable_source_path, path_stamp,
};
use crate::configuration::{config_directories, resolve_fmt, resolve_lint};
use lsp_types::Url;
use pascal_project::installation_config::ConfigurationSourceStamp;
use pascal_project::rtl_constants::RtlConstantSource;
use pascal_project::{
    BuildChoice, BuildSelection, InstallationSelection, ProjectContext, discover_with_selections,
    discover_with_selections_and_observations_with_work_budget, project_candidates,
    project_candidates_with_work_budget, runtime_project_selection, selected_project_is_current,
};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
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

#[derive(Debug, Clone, Copy)]
pub enum BuildField {
    Config,
    Platform,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildContextInfo {
    pub project_uri: Url,
    pub config: BuildSelectionInfo,
    pub platform: BuildSelectionInfo,
    pub conditionals: ConditionalsInfo,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildSelectionInfo {
    pub selected: Option<String>,
    pub candidates: Vec<String>,
    pub mode: String,
    pub project_default: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConditionalsInfo {
    pub closed: bool,
    pub open_reasons: Vec<String>,
    pub rtl_version_constants: RtlConstantsInfo,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RtlConstantsInfo {
    pub source: String,
    pub names: Vec<String>,
}

/// Admission-time immutable authority for project protocol workers.
pub(crate) struct ProjectOperationSnapshot {
    workspace: Workspace,
    source_generation: u64,
    configuration_generation: u64,
    expected_installation: Option<String>,
    expected_build: Option<BuildChoice>,
}

pub(crate) struct ProjectOperationReadSet {
    candidates: Vec<PathBuf>,
    configuration: Vec<ConfigurationSourceStamp>,
}

impl ProjectOperationReadSet {
    pub(crate) fn candidate_membership_matches(
        &self,
        candidate_uris: &[Url],
        budget: &ReconciliationBudget,
    ) -> Result<bool, String> {
        let mut claimed = Vec::new();
        claimed.try_reserve(candidate_uris.len()).map_err(|error| {
            format!("could not reserve project candidate freshness check: {error}")
        })?;
        for uri in candidate_uris {
            budget.charge_path_visits(1)?;
            if let Ok(path) = uri.to_file_path() {
                claimed.push(path);
            }
        }
        budget.charge_path_visits(super::sort_work_estimate(claimed.len()))?;
        claimed.sort();
        claimed.dedup();
        budget.charge_path_visits(self.candidates.len())?;
        Ok(claimed == self.candidates)
    }
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

impl ProjectContextPreparation {
    pub(crate) fn needs_installation_choice(&self) -> bool {
        matches!(
            self.state.context.installation_selection.as_ref(),
            Some(InstallationSelection::NeedsChoice { .. })
        )
    }
}

#[derive(Clone)]
pub(crate) struct ProjectSelectionPreparation {
    pub(crate) context: ProjectContextPreparation,
    scope: Option<PathBuf>,
    selected: Option<PathBuf>,
    expected_selected: Option<PathBuf>,
    affected_uris: HashSet<Url>,
}

impl ProjectOperationSnapshot {
    pub(crate) fn generations(&self) -> (u64, u64) {
        (self.source_generation, self.configuration_generation)
    }

    pub(crate) fn expected_installation(&self) -> Option<&str> {
        self.expected_installation.as_deref()
    }

    pub(crate) fn expected_build(&self) -> Option<&BuildChoice> {
        self.expected_build.as_ref()
    }

    pub(crate) fn workspace_mut(&mut self) -> &mut Workspace {
        &mut self.workspace
    }

    pub(crate) fn capture_read_set(
        &mut self,
        input_path: &Path,
        budget: &ReconciliationBudget,
    ) -> Result<ProjectOperationReadSet, String> {
        self.workspace
            .project_operation_read_set(input_path, budget)
    }

    pub(crate) fn read_set_is_current(
        &mut self,
        input_path: &Path,
        read_set: &ProjectOperationReadSet,
        budget: &ReconciliationBudget,
    ) -> Result<bool, String> {
        self.workspace
            .project_operation_read_set_is_current(input_path, read_set, budget)
    }
}

impl Workspace {
    fn project_operation_read_set(
        &self,
        input_path: &Path,
        budget: &ReconciliationBudget,
    ) -> Result<ProjectOperationReadSet, String> {
        let roots = self.workspace_root_paths();
        let candidates = project_candidates_with_work_budget(
            input_path,
            &roots,
            None,
            Some(budget as &dyn pascal_project::ProjectWorkBudget),
        )?;
        let mut candidate_paths = candidates.files;
        budget.charge_path_visits(super::sort_work_estimate(candidate_paths.len()))?;
        candidate_paths.sort();
        candidate_paths.dedup();

        // The read set is deliberately bounded by the same operation budget as
        // candidate discovery. Include each discovered project directory, plus
        // the original input directory when no project candidate exists.
        let mut configuration: Vec<ConfigurationSourceStamp> = Vec::new();
        let mut targets = Vec::new();
        targets
            .try_reserve(candidate_paths.len().saturating_add(3).max(1))
            .map_err(|error| {
                format!("could not reserve project configuration read set: {error}")
            })?;
        if candidate_paths.is_empty() {
            targets.push(input_path.to_path_buf());
        } else {
            targets.extend(candidate_paths.iter().cloned());
        }
        if let Some(explicit_project) = self.options.project_file.as_ref() {
            targets.push(explicit_project.clone());
        }
        budget.charge_path_visits(candidate_paths.len())?;
        budget.charge_path_visits(self.project_selections.len())?;
        if let Some((_, selected_project)) = runtime_project_selection(
            input_path,
            &pascal_project::ProjectCandidates {
                directory: candidates.directory.clone(),
                files: candidate_paths.clone(),
            },
            &self.project_selections,
        ) {
            targets.push(selected_project);
        }
        for target in targets {
            budget.charge_path_visits(1)?;
            for stamp in self.project_operation_configuration(&target, Some(budget))? {
                let mut known = false;
                for current in &configuration {
                    budget.charge_path_visits(1)?;
                    if current.path == stamp.path {
                        known = true;
                        break;
                    }
                }
                if !known {
                    configuration.try_reserve(1).map_err(|error| {
                        format!("could not reserve project configuration stamps: {error}")
                    })?;
                    configuration.push(stamp);
                }
            }
        }
        if !self.overrides.captured_sources_match(&configuration)? {
            return Err(
                "captured project configuration changed before request preparation; retry the request"
                    .to_string(),
            );
        }
        Ok(ProjectOperationReadSet {
            candidates: candidate_paths,
            configuration,
        })
    }

    fn project_operation_read_set_is_current(
        &self,
        input_path: &Path,
        read_set: &ProjectOperationReadSet,
        budget: &ReconciliationBudget,
    ) -> Result<bool, String> {
        let roots = self.workspace_root_paths();
        let current_candidates = project_candidates_with_work_budget(
            input_path,
            &roots,
            None,
            Some(budget as &dyn pascal_project::ProjectWorkBudget),
        )?;
        let mut current_candidates = current_candidates.files;
        budget.charge_path_visits(super::sort_work_estimate(current_candidates.len()))?;
        current_candidates.sort();
        current_candidates.dedup();
        budget.charge_path_visits(read_set.candidates.len())?;
        if current_candidates != read_set.candidates {
            return Ok(false);
        }
        self.project_operation_configuration_is_current(&read_set.configuration, Some(budget))
    }

    pub(crate) fn project_operation_configuration(
        &self,
        project_path: &Path,
        budget: Option<&ReconciliationBudget>,
    ) -> Result<Vec<ConfigurationSourceStamp>, String> {
        let roots = self.workspace_root_paths();
        let root = roots
            .iter()
            .find(|root| crate::workspace::path_starts_with_native(project_path, root));
        self.overrides.configuration_source_stamps_for(
            root.map(PathBuf::as_path),
            Some(project_path),
            budget.map(|budget| budget as &dyn pascal_project::ProjectWorkBudget),
        )
    }

    pub(crate) fn project_operation_configuration_is_current(
        &self,
        configuration: &[ConfigurationSourceStamp],
        budget: Option<&ReconciliationBudget>,
    ) -> Result<bool, String> {
        self.overrides.configuration_source_stamps_are_current(
            configuration,
            budget.map(|budget| budget as &dyn pascal_project::ProjectWorkBudget),
        )
    }

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
            build_selections: self.build_selections.clone(),
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
        let expected_build = project_path.and_then(|path| self.build_selections.get(path).cloned());
        ProjectOperationSnapshot {
            workspace,
            source_generation: self.source_generation,
            configuration_generation: self.configuration_generation,
            expected_installation,
            expected_build,
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
        self.prepare_project_context_with_control(uri, cancel, None)
            .map(|prepared| prepared.info)
    }

    pub(crate) fn prepare_project_context_with_control(
        &mut self,
        uri: &Url,
        cancel: Option<&std::sync::atomic::AtomicBool>,
        budget: Option<&ReconciliationBudget>,
    ) -> Result<ProjectContextPreparation, String> {
        let uri = canonical_file_uri(uri);
        let path = document_path(&uri)?;
        let context_key =
            self.context_for_project_protocol_anchor_with_control(&uri, cancel, budget)?;
        let context = self
            .contexts
            .get(&context_key)
            .map(|state| state.context.clone())
            .ok_or_else(|| format!("project context was not retained for {uri}"))?;
        let roots = self.workspace_root_paths();
        let candidates = project_candidates_with_work_budget(
            &path,
            &roots,
            cancel,
            budget.map(|budget| budget as &dyn pascal_project::ProjectWorkBudget),
        )?;
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
                match self.installation_selection_snapshot(project_file, cancel, budget) {
                    Ok((_, candidates, selection)) => (
                        candidates,
                        installation_selection_mode(&selection).to_string(),
                    ),
                    Err(error) if context.override_error.as_deref() == Some(error.as_str()) => {
                        // Project discovery already recorded this exact
                        // malformed-override diagnostic in the context. Keep
                        // the inspection request usable, but do not convert a
                        // cancellation or budget exhaustion into a warning.
                        check_project_operation_cancel(cancel)?;
                        if let Some(budget) = budget {
                            budget.charge_path_visits(0)?;
                        }
                        (Vec::new(), "automatic".to_string())
                    }
                    Err(error) => return Err(error),
                }
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
        self.installation_context_with_budget(project_uri, None)
    }

    pub(crate) fn installation_context_with_budget(
        &mut self,
        project_uri: &Url,
        budget: Option<&ReconciliationBudget>,
    ) -> Result<InstallationContextInfo, String> {
        let project_path = project_path(project_uri)?;
        self.validate_project_scope(&project_path)?;
        let (_, candidates, selection) =
            self.installation_selection_snapshot(&project_path, None, budget)?;
        let selected = match &selection {
            InstallationSelection::Selected { id, .. } | InstallationSelection::Invalid { id } => {
                Some(id.clone())
            }
            InstallationSelection::Legacy | InstallationSelection::NeedsChoice { .. } => None,
        };
        let selection_is_valid = !matches!(selection, InstallationSelection::Invalid { .. })
            && selected.as_ref().is_none_or(|id| {
                candidates
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(id))
            });
        let selection_mode = if !selection_is_valid {
            "invalid"
        } else {
            installation_selection_mode(&selection)
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

    pub fn build_context(&mut self, project_uri: &Url) -> Result<BuildContextInfo, String> {
        self.build_context_with_budget(project_uri, None)
    }

    pub(crate) fn build_context_with_budget(
        &mut self,
        project_uri: &Url,
        budget: Option<&ReconciliationBudget>,
    ) -> Result<BuildContextInfo, String> {
        let project_path = project_path(project_uri)?;
        let context = self.discover_build_context(
            &project_path,
            self.build_selections.clone(),
            None,
            budget,
        )?;
        Ok(build_context_info(project_uri, &context))
    }

    pub fn select_build(
        &mut self,
        project_uri: &Url,
        field: BuildField,
        value: Option<&str>,
    ) -> Result<BuildContextInfo, String> {
        self.select_build_inner(project_uri, field, value, None, None)
    }

    pub(crate) fn select_build_with_control(
        &mut self,
        project_uri: &Url,
        field: BuildField,
        value: Option<&str>,
        cancel: &std::sync::atomic::AtomicBool,
        budget: &ReconciliationBudget,
    ) -> Result<BuildContextInfo, String> {
        self.select_build_inner(project_uri, field, value, Some(cancel), Some(budget))
    }

    fn select_build_inner(
        &mut self,
        project_uri: &Url,
        field: BuildField,
        value: Option<&str>,
        cancel: Option<&std::sync::atomic::AtomicBool>,
        budget: Option<&ReconciliationBudget>,
    ) -> Result<BuildContextInfo, String> {
        let project_path = project_path(project_uri)?;
        self.validate_project_scope(&project_path)?;
        check_project_operation_cancel(cancel)?;

        let mut selections = self.build_selections.clone();
        let mut choice = selections.get(&project_path).cloned().unwrap_or_default();
        match field {
            BuildField::Config => choice.config = value.map(str::to_owned),
            BuildField::Platform => choice.platform = value.map(str::to_owned),
        }
        if choice.config.is_none() && choice.platform.is_none() {
            selections.remove(&project_path);
        } else {
            selections.insert(project_path.clone(), choice);
        }

        // Discover with the tentative selection before mutating live state.
        let validation =
            self.discover_build_context(&project_path, selections.clone(), cancel, budget)?;
        if let Some(value) = value {
            let selection = match field {
                BuildField::Config => &validation.config_selection,
                BuildField::Platform => &validation.platform_selection,
            };
            if !selection
                .candidates
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(value))
            {
                return Err(match field {
                    BuildField::Config => {
                        format!("unknown build configuration `{value}`")
                    }
                    BuildField::Platform => format!("unknown platform `{value}`"),
                });
            }
        }

        let mut keys = HashSet::new();
        keys.try_reserve(self.contexts.len())
            .map_err(|error| format!("could not reserve build selection contexts: {error}"))?;
        for key in self.contexts.keys() {
            if key
                .project_file
                .as_deref()
                .is_some_and(|path| project_paths_equal(path, &project_path))
            {
                keys.insert(key.clone());
            }
        }
        let project_dependency =
            self.preflight_project_selection_dependency(&project_path, budget)?;
        if selections.contains_key(&project_path) {
            self.build_selections
                .try_reserve(1)
                .map_err(|error| format!("could not reserve build selection: {error}"))?;
        }
        self.invalidate_selection_contexts(&keys, cancel, budget)?;
        // Context invalidation is preflighted; the session choice and its
        // freshness changes can now be committed without another fallible step.
        self.build_selections = selections;
        self.bump_source_generation();
        self.bump_configuration_generation();
        self.mark_configuration_change(&project_dependency, false);
        Ok(build_context_info(project_uri, &validation))
    }

    fn discover_build_context(
        &self,
        project_path: &Path,
        build_selections: HashMap<PathBuf, BuildChoice>,
        cancel: Option<&std::sync::atomic::AtomicBool>,
        budget: Option<&ReconciliationBudget>,
    ) -> Result<ProjectContext, String> {
        self.validate_project_scope(project_path)?;
        check_project_operation_cancel(cancel)?;
        let roots = self.workspace_root_paths_with_control(cancel, budget)?;
        let options = self.project_options_with_selections(
            self.installation_selections.clone(),
            build_selections,
        );
        let fallback_cancel = std::sync::atomic::AtomicBool::new(false);
        let context = discover_with_selections_and_observations_with_work_budget(
            project_path,
            &roots,
            &options,
            &self.project_selections,
            &self.overrides,
            &self.options.exclude,
            cancel.unwrap_or(&fallback_cancel),
            budget.map(|budget| budget as &dyn pascal_project::ProjectWorkBudget),
        )?
        .context;
        if context
            .project_file
            .as_deref()
            .is_none_or(|path| !project_paths_equal(path, project_path))
        {
            return Err(format!(
                "project is no longer valid: {}",
                project_path.display()
            ));
        }
        Ok(context)
    }

    pub fn select_installation(
        &mut self,
        project_uri: &Url,
        installation_id: Option<&str>,
    ) -> Result<InstallationContextInfo, String> {
        self.select_installation_inner(project_uri, installation_id, None, None)
    }

    pub(crate) fn select_installation_with_control(
        &mut self,
        project_uri: &Url,
        installation_id: Option<&str>,
        cancel: &std::sync::atomic::AtomicBool,
        budget: &ReconciliationBudget,
    ) -> Result<InstallationContextInfo, String> {
        self.select_installation_inner(project_uri, installation_id, Some(cancel), Some(budget))
    }

    fn select_installation_inner(
        &mut self,
        project_uri: &Url,
        installation_id: Option<&str>,
        cancel: Option<&std::sync::atomic::AtomicBool>,
        budget: Option<&ReconciliationBudget>,
    ) -> Result<InstallationContextInfo, String> {
        let project_path = project_path(project_uri)?;
        self.validate_project_scope(&project_path)?;
        check_project_operation_cancel(cancel)?;
        let (config, candidates, _) =
            self.installation_selection_snapshot(&project_path, cancel, budget)?;
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
                pascal_project::discover_with_selections_and_observations_with_work_budget(
                    &project_path,
                    &roots,
                    &options,
                    &self.project_selections,
                    &self.overrides,
                    &self.options.exclude,
                    cancel,
                    budget.map(|budget| budget as &dyn pascal_project::ProjectWorkBudget),
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
        let project_dependency =
            self.preflight_project_selection_dependency(&project_path, budget)?;
        let selected_after_change = match validation.installation_selection.as_ref() {
            Some(InstallationSelection::Selected { id, .. }) => Some(id.clone()),
            _ => None,
        };
        self.invalidate_selection_contexts(&keys, cancel, budget)?;
        // No fallible operation follows this point: context invalidation has
        // been fully preflighted and applied, so the session map and generation
        // changes become one infallible commit.
        self.installation_selections = selections;
        self.bump_source_generation();
        self.bump_configuration_generation();
        self.mark_configuration_change(&project_dependency, false);
        Ok(InstallationContextInfo {
            project_uri: canonical_file_uri(project_uri),
            candidates,
            selected_installation_id: selected_after_change,
            selection_mode: if installation_id.is_some() {
                "session".to_string()
            } else {
                installation_selection_mode(
                    validation
                        .installation_selection
                        .as_ref()
                        .unwrap_or(&InstallationSelection::Legacy),
                )
                .to_string()
            },
            warnings: Vec::new(),
        })
    }

    /// Commit a worker-validated installation choice. No configuration or
    /// project discovery is performed on the coordinator thread here.
    pub(crate) fn commit_prepared_installation_selection(
        &mut self,
        project_path: &Path,
        installation_id: Option<&str>,
        expected_installation: Option<&str>,
        budget: &ReconciliationBudget,
    ) -> Result<(), String> {
        if self
            .installation_selections
            .get(project_path)
            .map(String::as_str)
            != expected_installation
        {
            return Err("installation selection changed while the request was running".to_string());
        }
        let roots = self.workspace_root_paths();
        let mut in_scope = false;
        for root in &roots {
            budget.charge_path_visits(1)?;
            if crate::workspace::path_starts_with_native(project_path, root) {
                in_scope = true;
            }
        }
        if !in_scope {
            return Err("project is no longer in the configured workspace scope".to_string());
        }
        let mut keys = HashSet::new();
        keys.try_reserve(self.contexts.len()).map_err(|error| {
            format!("could not reserve installation selection contexts: {error}")
        })?;
        for key in self.contexts.keys() {
            budget.charge_path_visits(1)?;
            if key
                .project_file
                .as_deref()
                .is_some_and(|path| project_paths_equal(path, project_path))
            {
                keys.insert(key.clone());
            }
        }
        let project_dependency =
            self.preflight_project_selection_dependency(project_path, Some(budget))?;
        if installation_id.is_some() {
            self.installation_selections
                .try_reserve(1)
                .map_err(|error| format!("could not reserve installation selection: {error}"))?;
        }
        self.invalidate_selection_contexts(&keys, None, Some(budget))?;
        match installation_id {
            Some(id) => {
                self.installation_selections
                    .insert(project_path.to_path_buf(), id.to_string());
            }
            None => {
                self.installation_selections.remove(project_path);
            }
        }
        self.bump_source_generation();
        self.bump_configuration_generation();
        self.mark_configuration_change(&project_dependency, false);
        Ok(())
    }

    /// Commit a worker-validated build choice without rediscovering the
    /// project on the coordinator thread.
    pub(crate) fn commit_prepared_build_selection(
        &mut self,
        project_path: &Path,
        field: BuildField,
        value: Option<&str>,
        expected_build: Option<&BuildChoice>,
        budget: &ReconciliationBudget,
    ) -> Result<(), String> {
        if self.build_selections.get(project_path) != expected_build {
            return Err("build selection changed while the request was running".to_string());
        }
        let roots = self.workspace_root_paths();
        let mut in_scope = false;
        for root in &roots {
            budget.charge_path_visits(1)?;
            if crate::workspace::path_starts_with_native(project_path, root) {
                in_scope = true;
            }
        }
        if !in_scope {
            return Err("project is no longer in the configured workspace scope".to_string());
        }

        let mut choice = self
            .build_selections
            .get(project_path)
            .cloned()
            .unwrap_or_default();
        match field {
            BuildField::Config => choice.config = value.map(str::to_owned),
            BuildField::Platform => choice.platform = value.map(str::to_owned),
        }
        let choice = (choice.config.is_some() || choice.platform.is_some()).then_some(choice);
        let mut keys = HashSet::new();
        keys.try_reserve(self.contexts.len())
            .map_err(|error| format!("could not reserve build selection contexts: {error}"))?;
        for key in self.contexts.keys() {
            budget.charge_path_visits(1)?;
            if key
                .project_file
                .as_deref()
                .is_some_and(|path| project_paths_equal(path, project_path))
            {
                keys.insert(key.clone());
            }
        }
        let project_dependency =
            self.preflight_project_selection_dependency(project_path, Some(budget))?;
        if choice.is_some() {
            self.build_selections
                .try_reserve(1)
                .map_err(|error| format!("could not reserve build selection: {error}"))?;
        }
        self.invalidate_selection_contexts(&keys, None, Some(budget))?;
        match choice {
            Some(choice) => {
                self.build_selections
                    .insert(project_path.to_path_buf(), choice);
            }
            None => {
                self.build_selections.remove(project_path);
            }
        }
        self.bump_source_generation();
        self.bump_configuration_generation();
        self.mark_configuration_change(&project_dependency, false);
        Ok(())
    }

    /// Reserve and account the project-file dependency that fences in-flight
    /// dependency-scoped requests across a selection change. This is required
    /// even when no project context has been cached yet: a worker can already
    /// have read the project descriptor while paused before context creation.
    fn preflight_project_selection_dependency(
        &mut self,
        project_path: &Path,
        budget: Option<&ReconciliationBudget>,
    ) -> Result<Url, String> {
        if let Some(budget) = budget {
            budget.charge_path_visits(1)?;
        }
        let project_uri = Url::from_file_path(absolute_path(project_path.to_path_buf()))
            .map_err(|_| "could not create a URI for the selected project file".to_string())?;
        let project_uri = canonical_file_uri(&project_uri);
        if let Some(budget) = budget {
            budget.charge_project_path_key_bytes(project_uri.as_str().len())?;
        }
        self.configuration_change_generations
            .try_reserve(1)
            .map_err(|error| {
                format!("could not reserve selected project freshness dependency: {error}")
            })?;
        Ok(project_uri)
    }

    fn installation_selection_snapshot(
        &self,
        project_path: &Path,
        cancel: Option<&std::sync::atomic::AtomicBool>,
        budget: Option<&ReconciliationBudget>,
    ) -> Result<
        (
            pascal_project::installation_config::ProjectConfiguration,
            Vec<String>,
            InstallationSelection,
        ),
        String,
    > {
        let roots = self.workspace_root_paths();
        let configuration = self.overrides.configuration_for_with_work_budget(
            roots
                .iter()
                .find(|root| project_path.starts_with(root))
                .map(PathBuf::as_path),
            Some(project_path),
            budget.map(|budget| budget as &dyn pascal_project::ProjectWorkBudget),
        )?;
        let mut candidates = configuration.installation_ids();
        candidates.sort_by_key(|id| id.to_ascii_lowercase());
        // Resolve the selection on this project-operation worker using the
        // same discovery path that prepares ProjectContext. A cached context
        // cannot be the authority here: selection changes intentionally evict
        // it, and metadata-inferred selections survive an automatic reset.
        let options = self.project_options_with_installations(self.installation_selections.clone());
        // Standalone callers may not have a request cancellation token, but
        // their budget remains cancellation-aware and is charged throughout
        // discovery. Protocol workers pass the actual request token.
        let fallback_cancel = std::sync::atomic::AtomicBool::new(false);
        let discovery = discover_with_selections_and_observations_with_work_budget(
            project_path,
            &roots,
            &options,
            &self.project_selections,
            &self.overrides,
            &self.options.exclude,
            cancel.unwrap_or(&fallback_cancel),
            budget.map(|budget| budget as &dyn pascal_project::ProjectWorkBudget),
        )?;
        let selection = discovery
            .context
            .installation_selection
            .unwrap_or(InstallationSelection::Legacy);
        Ok((configuration, candidates, selection))
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

    pub(crate) fn prepare_select_project_with_control(
        &mut self,
        uri: &Url,
        project: Option<&Url>,
        cancel: &std::sync::atomic::AtomicBool,
        budget: &ReconciliationBudget,
    ) -> Result<ProjectSelectionPreparation, String> {
        let uri = canonical_file_uri(uri);
        let path = document_path(&uri)?;
        let roots = self.workspace_root_paths();
        let candidates = project_candidates_with_work_budget(
            &path,
            &roots,
            Some(cancel),
            Some(budget as &dyn pascal_project::ProjectWorkBudget),
        )?;
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
            let context =
                self.prepare_project_context_with_control(&uri, Some(cancel), Some(budget))?;
            return Ok(ProjectSelectionPreparation {
                context,
                scope: None,
                selected: None,
                expected_selected: None,
                affected_uris: HashSet::new(),
            });
        };

        let expected_selected = self.project_selections.get(&scope).cloned();
        let mut tentative = self.project_selections.clone();
        if project.is_some() {
            tentative.insert(scope.clone(), selected.clone());
        } else {
            tentative.remove(&scope);
        }

        let deleted_paths = self.deleted_path_snapshot_with_control(Some(cancel), Some(budget))?;
        let validation = pascal_project::discover_with_selections_and_observations_with_work_budget_and_deleted_paths(
            &path,
            &roots,
            &self.project_options_with_installations(self.installation_selections.clone()),
            &tentative,
            &self.overrides,
            &self.options.exclude,
            cancel,
            Some(budget as &dyn pascal_project::ProjectWorkBudget),
            &deleted_paths,
        )?;
        if validation
            .context
            .project_file
            .as_deref()
            .is_none_or(|found| !project_paths_equal(found, &selected))
            && project.is_some()
        {
            return Err(format!(
                "selected project is no longer valid: {}",
                selected.display()
            ));
        }

        let affected_uris = self.selection_uris_for_scope(&scope, &uri);
        // Force discovery against the tentative selection rather than reusing
        // a context prepared for the previously configured project. This is a
        // disposable worker snapshot, so partial invalidation on failure is
        // never published to the coordinator.
        self.invalidate_project_selection_uris(&affected_uris, Some(cancel), Some(budget))?;
        // Don't restore an old owner from before the selection change; it may
        // retain a configured project from a different candidate directory.
        for affected_uri in &affected_uris {
            self.document_owners.remove(affected_uri);
        }
        self.project_selections = tentative;
        let context =
            self.prepare_project_context_with_control(&uri, Some(cancel), Some(budget))?;
        Ok(ProjectSelectionPreparation {
            context,
            scope: Some(scope),
            selected: project.is_some().then_some(selected),
            expected_selected,
            affected_uris,
        })
    }

    pub(crate) fn commit_prepared_project_selection(
        &mut self,
        prepared: ProjectSelectionPreparation,
        budget: &ReconciliationBudget,
    ) -> Result<ProjectContextInfo, String> {
        let ProjectSelectionPreparation {
            context,
            scope,
            selected,
            expected_selected,
            affected_uris,
        } = prepared;
        if let Some(scope) = scope {
            if self.project_selections.get(&scope).cloned() != expected_selected {
                return Err("project selection changed while the request was running".to_string());
            }
            let roots = self.workspace_root_paths();
            let mut in_scope = false;
            for root in &roots {
                budget.charge_path_visits(1)?;
                if crate::workspace::path_starts_with_native(&scope, root) {
                    in_scope = true;
                }
            }
            if !in_scope {
                return Err("project selection scope is no longer configured".to_string());
            }
            if selected.is_some() {
                self.project_selections
                    .try_reserve(1)
                    .map_err(|error| format!("could not reserve project selection: {error}"))?;
            }
            self.invalidate_project_selection_uris(&affected_uris, None, Some(budget))?;
            match selected {
                Some(selected) => {
                    self.project_selections.insert(scope, selected);
                }
                None => {
                    self.project_selections.remove(&scope);
                }
            }
            self.bump_source_generation();
            self.bump_configuration_generation();
        }
        let info = context.info.clone();
        self.apply_project_context_preparation(context);
        Ok(info)
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

fn build_context_info(project_uri: &Url, context: &ProjectContext) -> BuildContextInfo {
    let rtl_constants = &context.conditional_closure;
    BuildContextInfo {
        project_uri: canonical_file_uri(project_uri),
        config: build_selection_info(&context.config_selection),
        platform: build_selection_info(&context.platform_selection),
        conditionals: ConditionalsInfo {
            closed: rtl_constants.closed,
            open_reasons: rtl_constants
                .open_reasons
                .iter()
                .map(|reason| reason.as_str().to_string())
                .collect(),
            rtl_version_constants: RtlConstantsInfo {
                source: match rtl_constants.rtl_source {
                    RtlConstantSource::Override => "override",
                    RtlConstantSource::SystemPas => "systemPas",
                    RtlConstantSource::Table => "table",
                    RtlConstantSource::Unknown => "unknown",
                }
                .to_string(),
                names: rtl_constants.rtl_constants.clone().unwrap_or_default(),
            },
        },
        warnings: context.warnings.clone(),
    }
}

fn build_selection_info(selection: &BuildSelection) -> BuildSelectionInfo {
    BuildSelectionInfo {
        selected: selection.selected.clone(),
        candidates: selection.candidates.clone(),
        mode: selection.mode.as_str().to_string(),
        project_default: selection.project_default.clone(),
    }
}

fn installation_selection_mode(selection: &InstallationSelection) -> &'static str {
    match selection {
        InstallationSelection::Selected { origin, .. } => match origin {
            pascal_project::InstallationOrigin::Session => "session",
            pascal_project::InstallationOrigin::Configured => "configured",
            pascal_project::InstallationOrigin::Metadata => "metadata",
        },
        InstallationSelection::NeedsChoice { .. } => "needsChoice",
        InstallationSelection::Invalid { .. } => "invalid",
        InstallationSelection::Legacy => "automatic",
    }
}

fn document_path(uri: &Url) -> Result<PathBuf, String> {
    let path = uri
        .to_file_path()
        .map_err(|_| format!("project context requires a file URI: {uri}"))?;
    let path = absolute_path(path);
    let is_project_anchor = path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("dproj"));
    if !is_analyzable_source_path(&path) && !is_project_anchor {
        return Err(format!(
            "unsupported project context anchor: {}",
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

#[cfg(test)]
mod installation_selection_mode_tests {
    use super::{InstallationSelection, Workspace, installation_selection_mode};
    use crate::workspace::{ReconciliationBudget, WorkspaceOptions};
    use pascal_project::InstallationOrigin;
    use std::sync::{Arc, atomic::AtomicBool};

    #[test]
    fn selection_modes_preserve_origin_and_non_selected_states() {
        for (selection, expected) in [
            (
                InstallationSelection::Selected {
                    id: "37.0".to_string(),
                    origin: InstallationOrigin::Session,
                },
                "session",
            ),
            (
                InstallationSelection::Selected {
                    id: "37.0".to_string(),
                    origin: InstallationOrigin::Configured,
                },
                "configured",
            ),
            (
                InstallationSelection::Selected {
                    id: "37.0".to_string(),
                    origin: InstallationOrigin::Metadata,
                },
                "metadata",
            ),
            (
                InstallationSelection::NeedsChoice {
                    candidates: vec!["7.0".to_string(), "37.0".to_string()],
                },
                "needsChoice",
            ),
            (
                InstallationSelection::Invalid {
                    id: "99.0".to_string(),
                },
                "invalid",
            ),
            (InstallationSelection::Legacy, "automatic"),
        ] {
            assert_eq!(installation_selection_mode(&selection), expected);
        }
    }

    #[test]
    fn installation_selection_discovery_uses_the_supplied_work_budget() {
        let directory = tempfile::tempdir().expect("workspace directory");
        let root = directory.path();
        let project = root.join("App.dproj");
        std::fs::write(
            &project,
            "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
        )
        .expect("project file");
        std::fs::write(root.join("App.dpr"), "program App; begin end.").expect("main source");
        std::fs::write(root.join(".delphi-tools.local.toml"), "").expect("configuration file");

        let baseline = Workspace::new(vec![root.to_path_buf()], WorkspaceOptions::default());
        let baseline_budget = ReconciliationBudget::new(Arc::new(AtomicBool::new(false)));
        let roots = baseline.workspace_root_paths();
        baseline
            .overrides
            .configuration_for_with_work_budget(
                roots.first().map(std::path::PathBuf::as_path),
                Some(&project),
                Some(&baseline_budget as &dyn pascal_project::ProjectWorkBudget),
            )
            .expect("configuration snapshot");
        let configuration_visits = baseline_budget.used.get().filesystem_path_visits;
        assert!(
            configuration_visits > 0,
            "fixture must charge configuration work"
        );

        let workspace = Workspace::new(vec![root.to_path_buf()], WorkspaceOptions::default());
        let budget = ReconciliationBudget::new(Arc::new(AtomicBool::new(false)));
        budget.set_path_visit_limit(configuration_visits);
        let error = workspace
            .installation_selection_snapshot(&project, None, Some(&budget))
            .expect_err("discovery must be charged after the configuration snapshot");
        assert!(error.contains("work budget exceeded"), "{error}");
    }
}
