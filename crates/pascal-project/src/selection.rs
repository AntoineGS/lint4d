use super::assembly::{
    build_project_context, build_standalone_context_with_overrides,
    effective_overrides_for_project, inherit_path_provenance, path_starts_with_ci, paths_equal_ci,
    relevant_override_workspace_root,
};
use super::authorization::{
    absolute_lexical, is_compiled_reference, is_foreign_windows_path, is_windows_absolute_text,
    resolve_existing_path, resolve_project_path_entry,
};
use super::context::LocalProjectConfig;
#[cfg(any(test, feature = "test-support"))]
use super::discovery_cache::test_record_project_discovery;
use super::discovery_cache::{
    CachedProjectContext, PathResolutionCacheScope, ProjectContextBuildCacheKey,
    ProjectOptionsCacheKey, cache_path_list, check_project_scan_cancel, project_context_cache_get,
    project_context_cache_insert, project_directory_entries,
    record_project_discovery_cache_discovery, with_path_resolution_cancel,
};
use super::membership::parse_unit_membership;
use super::reads::project_read_stamp;
use super::*;

/// Convenience wrapper around [`ProjectContext::discover`].
pub fn discover(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
) -> Result<ProjectContext, String> {
    ProjectContext::discover(file, workspace_roots, options)
}

pub(super) fn production_override_session() -> (OverrideSession, Vec<String>) {
    match delphi_overrides::user_config_path_from_env() {
        Ok(path) => (OverrideSession::new(Some(path)), Vec::new()),
        Err(error) => (OverrideSession::new(None), vec![error]),
    }
}

pub fn project_candidates(
    file: &Path,
    workspace_roots: &[PathBuf],
) -> Result<ProjectCandidates, String> {
    project_candidates_with_cancel(file, workspace_roots, None)
}

pub fn project_candidates_with_cancel(
    file: &Path,
    workspace_roots: &[PathBuf],
    cancel: Option<&AtomicBool>,
) -> Result<ProjectCandidates, String> {
    project_candidates_with_work_budget(file, workspace_roots, cancel, None)
}

pub fn project_candidates_with_work_budget(
    file: &Path,
    workspace_roots: &[PathBuf],
    cancel: Option<&AtomicBool>,
    work_budget: Option<&dyn ProjectWorkBudget>,
) -> Result<ProjectCandidates, String> {
    project_candidates_with_work_budget_and_deleted_paths(
        file,
        workspace_roots,
        cancel,
        work_budget,
        &[],
    )
}

pub fn project_candidates_with_work_budget_and_deleted_paths(
    file: &Path,
    workspace_roots: &[PathBuf],
    cancel: Option<&AtomicBool>,
    work_budget: Option<&dyn ProjectWorkBudget>,
    deleted_paths: &[PathBuf],
) -> Result<ProjectCandidates, String> {
    check_project_scan_cancel(cancel)?;
    if let Some(work_budget) = work_budget {
        work_budget.check_cancelled()?;
    }
    let mut warnings = Vec::new();
    let absolute_file = absolute_lexical(file)?;
    let file_path = discovery_file_path(&absolute_file, &mut warnings);
    let roots = normalize_workspace_roots(workspace_roots, &mut warnings)?;
    let relevant_root = relevant_workspace_root(&file_path, &roots);
    find_project_candidates(
        &file_path,
        relevant_root.as_deref(),
        cancel,
        work_budget,
        deleted_paths,
    )
}

pub fn discover_with_selections(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
    selections: &ProjectSelections,
    overrides: &OverrideSession,
    exclusions: &[String],
) -> Result<ProjectContext, String> {
    discover_context_with_selections(
        file,
        workspace_roots,
        options,
        selections,
        overrides,
        Vec::new(),
        exclusions,
        None,
        None,
        &[],
    )
    .map(|discovery| discovery.context)
}

#[allow(dead_code)]
pub(crate) fn discover_with_selections_and_observations(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
    selections: &ProjectSelections,
) -> Result<ProjectDiscovery, String> {
    let (overrides, warnings) = production_override_session();
    discover_context_with_selections(
        file,
        workspace_roots,
        options,
        selections,
        &overrides,
        warnings,
        &[],
        None,
        None,
        &[],
    )
}

#[allow(dead_code)]
pub(crate) fn discover_with_selections_and_observations_with_cancel(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
    selections: &ProjectSelections,
    cancel: &AtomicBool,
) -> Result<ProjectDiscovery, String> {
    let (overrides, warnings) = production_override_session();
    discover_context_with_selections(
        file,
        workspace_roots,
        options,
        selections,
        &overrides,
        warnings,
        &[],
        Some(cancel),
        None,
        &[],
    )
}

pub fn discover_with_selections_and_observations_with_overrides(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
    selections: &ProjectSelections,
    overrides: &OverrideSession,
    exclusions: &[String],
) -> Result<ProjectDiscovery, String> {
    discover_with_selections_and_observations_with_overrides_and_deleted_paths(
        file,
        workspace_roots,
        options,
        selections,
        overrides,
        exclusions,
        &[],
    )
}

pub fn discover_with_selections_and_observations_with_overrides_and_deleted_paths(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
    selections: &ProjectSelections,
    overrides: &OverrideSession,
    exclusions: &[String],
    deleted_paths: &[PathBuf],
) -> Result<ProjectDiscovery, String> {
    discover_context_with_selections(
        file,
        workspace_roots,
        options,
        selections,
        overrides,
        Vec::new(),
        exclusions,
        None,
        None,
        deleted_paths,
    )
}

pub fn discover_with_selections_and_observations_with_cancel_and_overrides(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
    selections: &ProjectSelections,
    overrides: &OverrideSession,
    exclusions: &[String],
    cancel: &AtomicBool,
) -> Result<ProjectDiscovery, String> {
    discover_with_selections_and_observations_with_work_budget(
        file,
        workspace_roots,
        options,
        selections,
        overrides,
        exclusions,
        cancel,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn discover_with_selections_and_observations_with_work_budget(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
    selections: &ProjectSelections,
    overrides: &OverrideSession,
    exclusions: &[String],
    cancel: &AtomicBool,
    work_budget: Option<&dyn ProjectWorkBudget>,
) -> Result<ProjectDiscovery, String> {
    discover_with_selections_and_observations_with_work_budget_and_deleted_paths(
        file,
        workspace_roots,
        options,
        selections,
        overrides,
        exclusions,
        cancel,
        work_budget,
        &[],
    )
}

#[allow(clippy::too_many_arguments)]
pub fn discover_with_selections_and_observations_with_work_budget_and_deleted_paths(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
    selections: &ProjectSelections,
    overrides: &OverrideSession,
    exclusions: &[String],
    cancel: &AtomicBool,
    work_budget: Option<&dyn ProjectWorkBudget>,
    deleted_paths: &[PathBuf],
) -> Result<ProjectDiscovery, String> {
    discover_context_with_selections(
        file,
        workspace_roots,
        options,
        selections,
        overrides,
        Vec::new(),
        exclusions,
        Some(cancel),
        work_budget,
        deleted_paths,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn discover_with_selections_and_observations_with_work_budget_and_optional_cancel_and_deleted_paths(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
    selections: &ProjectSelections,
    overrides: &OverrideSession,
    exclusions: &[String],
    cancel: Option<&AtomicBool>,
    work_budget: Option<&dyn ProjectWorkBudget>,
    deleted_paths: &[PathBuf],
) -> Result<ProjectDiscovery, String> {
    discover_context_with_selections(
        file,
        workspace_roots,
        options,
        selections,
        overrides,
        Vec::new(),
        exclusions,
        cancel,
        work_budget,
        deleted_paths,
    )
}

#[allow(dead_code)]
fn discover_context(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
) -> Result<ProjectContext, String> {
    let (overrides, warnings) = production_override_session();
    discover_context_with_selections(
        file,
        workspace_roots,
        options,
        &ProjectSelections::new(),
        &overrides,
        warnings,
        &[],
        None,
        None,
        &[],
    )
    .map(|discovery| discovery.context)
}

#[allow(clippy::too_many_arguments)]
fn load_local_project_config(
    file: &Path,
    workspace_root: Option<&Path>,
    roots: &[PathBuf],
    options: &ProjectOptions,
    overrides: &OverrideSession,
    exclusions: &[String],
    tracker: &mut ProjectReadTracker<'_>,
    cancel: Option<&AtomicBool>,
) -> Result<Option<(PathBuf, LocalProjectConfig)>, String> {
    let Some(workspace_root) = workspace_root else {
        return Ok(None);
    };
    let project_directory = file.parent();
    // Override loading has its own established diagnostic path in project
    // discovery. A local config can still be bounded/read safely under the
    // workspace roots without mappings when that independent layer is invalid.
    let effective_overrides = overrides
        .effective_for(Some(workspace_root), project_directory)
        .unwrap_or_default();
    let policy = ReadPolicy::new(
        roots,
        &options.source_paths,
        exclusions,
        &effective_overrides,
    );
    let mut current = project_directory;
    let mut candidates = 0;
    while let Some(directory) = current {
        check_project_scan_cancel(cancel)?;
        if let Some(budget) = tracker.work_budget {
            budget.charge_path_visits(1)?;
        }
        candidates += 1;
        if candidates > MAX_LOCAL_PROJECT_CONFIG_CANDIDATES {
            return Err(format!(
                ".delphilsp.json ancestor candidate limit ({MAX_LOCAL_PROJECT_CONFIG_CANDIDATES}) exceeded"
            ));
        }
        if !project_path_starts_with(directory, workspace_root) {
            break;
        }
        let candidate = directory.join(".delphilsp.json");
        tracker.record_metadata_path(candidate.clone());
        let candidate_exists = if is_deleted_path(&candidate, tracker.deleted_paths) {
            false
        } else {
            match fs::symlink_metadata(&candidate) {
                Ok(_) => true,
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(error) => {
                    return Err(format!(
                        "could not inspect .delphilsp.json candidate {}: {error}",
                        candidate.display()
                    ));
                }
            }
        };
        if candidate_exists {
            let entry = ProjectPathEntry {
                path: candidate.clone(),
                provenance: ProjectPathProvenance::Configured,
            };
            if !policy.allows_entry(&entry) {
                return Err(format!(
                    ".delphilsp.json is outside authorized read roots or uses a symlink: {}",
                    candidate.display()
                ));
            }
            let (contents, observation) =
                read_payload_with_tracker(&policy, &entry, MAX_LOCAL_PROJECT_CONFIG_BYTES, tracker)
                    .map_err(|error| format!("could not read {}: {error}", candidate.display()))?;
            tracker.record_metadata_observation(observation);
            let config: LocalProjectConfig = serde_json::from_str(&contents).map_err(|error| {
                format!(
                    "invalid .delphilsp.json at {}: {error}",
                    candidate.display()
                )
            })?;
            if config.version != 1 {
                return Err(format!(
                    "unsupported .delphilsp.json version {} at {}",
                    config.version,
                    candidate.display()
                ));
            }
            if config
                .build_configuration
                .as_ref()
                .is_some_and(|value| value.trim().is_empty() || value.len() > 128)
            {
                return Err(".delphilsp.json buildConfiguration must be 1..=128 bytes".to_string());
            }
            return Ok(Some((candidate, config)));
        }
        if directory == workspace_root {
            break;
        }
        current = directory.parent();
    }
    Ok(None)
}

fn validate_local_config_path(value: &str) -> Result<(), String> {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > 4096
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            ".delphilsp.json path must be a relative path without traversal: {value:?}"
        ));
    }
    Ok(())
}

// Discovery orchestration keeps the immutable request inputs, observation
// tracker, and cancellation token explicit at this boundary.
#[allow(clippy::too_many_arguments)]
pub(super) fn discover_context_with_selections(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
    selections: &ProjectSelections,
    overrides: &OverrideSession,
    warnings: Vec<String>,
    exclusions: &[String],
    cancel: Option<&AtomicBool>,
    work_budget: Option<&dyn ProjectWorkBudget>,
    deleted_paths: &[PathBuf],
) -> Result<ProjectDiscovery, String> {
    with_path_resolution_cancel(cancel, || {
        let discovery = discover_context_with_selections_inner(
            file,
            workspace_roots,
            options,
            selections,
            overrides,
            warnings,
            exclusions,
            cancel,
            work_budget,
            deleted_paths,
        )?;
        // Work built with no token (standalone contexts) can drop paths
        // silently once the token is set, so a result is only returned if the
        // request was still live.
        if let Some(work_budget) = work_budget {
            work_budget.check_cancelled()?;
        }
        if cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
            return Err("request cancelled".to_string());
        }
        Ok(discovery)
    })
}

#[allow(clippy::too_many_arguments)]
fn discover_context_with_selections_inner(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
    selections: &ProjectSelections,
    overrides: &OverrideSession,
    mut warnings: Vec<String>,
    exclusions: &[String],
    cancel: Option<&AtomicBool>,
    work_budget: Option<&dyn ProjectWorkBudget>,
    deleted_paths: &[PathBuf],
) -> Result<ProjectDiscovery, String> {
    #[cfg(any(test, feature = "test-support"))]
    test_record_project_discovery(false);
    record_project_discovery_cache_discovery();
    let _path_resolution_cache_scope = PathResolutionCacheScope::new();
    check_project_scan_cancel(cancel)?;
    if let Some(work_budget) = work_budget {
        work_budget.check_cancelled()?;
    }
    let mut read_tracker = ProjectReadTracker::with_budget(work_budget, deleted_paths);
    let absolute_file = absolute_lexical(file)?;
    let file_path = discovery_file_path(&absolute_file, &mut warnings);
    let roots = normalize_workspace_roots(workspace_roots, &mut warnings)?;
    let relevant_root = relevant_workspace_root(&file_path, &roots);
    let local_config_overrides = overrides
        .effective_for(relevant_root.as_deref(), file_path.parent())
        .unwrap_or_default();
    let original_read_policy = ReadPolicy::new(
        &roots,
        &options.source_paths,
        exclusions,
        &local_config_overrides,
    );
    let config_path = load_local_project_config(
        &file_path,
        relevant_root.as_deref(),
        &roots,
        options,
        overrides,
        exclusions,
        &mut read_tracker,
        cancel,
    )?;
    let mut options = options.clone();
    if let Some((config_file, config)) = config_path {
        let config_directory = config_file.parent().unwrap_or_else(|| Path::new("."));
        let configured_project = if let Some(project) = config.project.as_deref() {
            validate_local_config_path(project)?;
            let project_path = absolute_lexical(&config_directory.join(project))?;
            let project_entry = ProjectPathEntry {
                path: project_path.clone(),
                provenance: ProjectPathProvenance::Configured,
            };
            if !original_read_policy.allows_location(&project_entry) {
                return Err(format!(
                    ".delphilsp.json project path escapes its original authorized roots or uses a symlink: {}",
                    project_path.display()
                ));
            }
            Some(project_path)
        } else {
            None
        };
        let requester_has_project_selection = selections
            .keys()
            .any(|scope| project_path_starts_with(&file_path, scope));
        if options.project_file.is_none()
            && !requester_has_project_selection
            && let Some(project) = configured_project
        {
            options.project_file = Some(project);
        }
        for source_path in config.source_paths {
            validate_local_config_path(&source_path)?;
            let source_path = absolute_lexical(&config_directory.join(source_path))?;
            let source_entry = ProjectPathEntry {
                path: source_path.clone(),
                provenance: ProjectPathProvenance::Configured,
            };
            if !original_read_policy.allows_location(&source_entry)
                || !project_path_starts_with(
                    &source_path,
                    relevant_root.as_deref().unwrap_or(config_directory),
                )
            {
                return Err(format!(
                    ".delphilsp.json source path escapes its original authorized roots or uses a symlink: {}",
                    source_path.display()
                ));
            }
            let source_path = source_path.to_string_lossy().into_owned();
            if !options.source_paths.contains(&source_path) {
                options.source_paths.push(source_path);
            }
        }
        for define in config.defines {
            if define.trim().is_empty() || define.len() > 256 || define.contains(';') {
                return Err(
                    ".delphilsp.json defines must be non-empty symbols of at most 256 bytes"
                        .to_string(),
                );
            }
            if options.conditional_context.define(&define) == ConditionalFact::Unknown {
                options
                    .conditional_context
                    .set_define(&define, ConditionalFact::True);
            }
        }
        if options.build_config.is_none() {
            options.build_config = config.build_configuration;
        }
    }
    record_candidate_memberships(
        &file_path,
        relevant_root.as_deref(),
        &mut read_tracker,
        cancel,
        deleted_paths,
    )?;
    if let Some(work_budget) = work_budget {
        work_budget.check_cancelled()?;
    }

    let runtime_selection = if selections.is_empty() {
        None
    } else {
        let candidates = match find_project_candidates(
            &file_path,
            relevant_root.as_deref(),
            cancel,
            work_budget,
            deleted_paths,
        ) {
            Ok(candidates) => candidates,
            Err(error) => {
                if let Some(work_budget) = work_budget {
                    work_budget.check_cancelled()?;
                }
                warnings.push(error);
                let context = build_standalone_context_with_overrides(
                    &file_path,
                    &roots,
                    &options,
                    warnings,
                    false,
                    Vec::new(),
                    Vec::new(),
                    overrides,
                    exclusions,
                )?;
                if let Some(work_budget) = work_budget {
                    work_budget.check_cancelled()?;
                }
                return Ok(read_tracker.into_discovery(context, None));
            }
        };
        runtime_project_selection(&file_path, &candidates, selections).map(|(scope, requested)| {
            let selected = candidates
                .files
                .iter()
                .find(|candidate| project_paths_equal(candidate, &requested))
                .cloned();
            (scope, requested, selected, candidates.files)
        })
    };
    let discovery_selection = runtime_selection
        .as_ref()
        .map(|(scope, requested, _, _)| (scope.clone(), requested.clone()));

    let mut selected_project =
        if let Some((scope, requested, selected, candidates)) = runtime_selection {
            let Some(project_file) = selected else {
                warnings.push(format!(
                    "selected project {} is not a current candidate in {}",
                    requested.display(),
                    scope.display()
                ));
                let context = build_standalone_context_with_overrides(
                    &file_path,
                    &roots,
                    &options,
                    warnings,
                    false,
                    candidates,
                    Vec::new(),
                    overrides,
                    exclusions,
                )?;
                if let Some(work_budget) = work_budget {
                    work_budget.check_cancelled()?;
                }
                return Ok(read_tracker.into_discovery(context, discovery_selection.clone()));
            };
            ProjectSelection::Selected {
                path: project_file,
                explicit: true,
                metadata_files: candidates,
                metadata_observations: Vec::new(),
            }
        } else if let Some(project_file) = &options.project_file {
            explicit_project_file(project_file, &roots, &file_path, &mut warnings)
        } else {
            discover_project_file(
                &file_path,
                relevant_root.as_deref(),
                &roots,
                &options,
                overrides,
                &mut warnings,
                &mut read_tracker,
                cancel,
                exclusions,
                deleted_paths,
            )
        };
    if let ProjectSelection::Selected { path, .. } = &selected_project
        && is_deleted_path(path, deleted_paths)
    {
        warnings.push(format!(
            "selected project {} is tombstoned by a delete notification",
            path.display()
        ));
        selected_project = ProjectSelection::Incomplete {
            metadata_files: vec![path.clone()],
            metadata_observations: Vec::new(),
            override_error: None,
        };
    }
    check_project_scan_cancel(cancel)?;
    if let Some(work_budget) = work_budget {
        work_budget.check_cancelled()?;
    }

    match selected_project {
        ProjectSelection::Selected {
            path: project_file,
            explicit,
            metadata_files,
            metadata_observations,
        } => {
            let installation_workspace = project_file
                .parent()
                .and_then(|directory| relevant_override_workspace_root(directory, &roots));
            let installation_config = match overrides
                .configuration_for(installation_workspace.as_deref(), Some(&project_file))
            {
                Ok(config) => config,
                Err(error) => {
                    warnings.push(error);
                    crate::installation_config::ProjectConfiguration::from_layers(Vec::new())
                }
            };
            let effective_overrides =
                match effective_overrides_for_project(&project_file, &roots, overrides) {
                    Ok(overrides) => overrides,
                    Err(error) => {
                        warnings.push(error.clone());
                        for path in &metadata_files {
                            read_tracker.record_metadata_path(path.clone());
                        }
                        for observation in &metadata_observations {
                            read_tracker.record_metadata_observation(observation.clone());
                        }
                        let mut build_tracker =
                            ProjectReadTracker::with_budget(work_budget, deleted_paths);
                        let mut context = build_project_context(
                            project_file,
                            &file_path,
                            &roots,
                            &options,
                            installation_config.clone(),
                            EffectiveOverrides::default(),
                            warnings,
                            explicit,
                            Vec::new(),
                            Vec::new(),
                            exclusions,
                            &mut build_tracker,
                            cancel,
                        )?;
                        read_tracker.merge_project_context_reads(
                            &CachedProjectContext {
                                context: context.clone(),
                                observations: build_tracker.observations,
                                metadata_paths: build_tracker.metadata_paths,
                                metadata_observations: build_tracker.metadata_observations,
                            },
                            cancel,
                            work_budget,
                        )?;
                        if let Some(work_budget) = work_budget {
                            work_budget.check_cancelled()?;
                        }
                        context.discovery_complete = false;
                        context.override_error = Some(error);
                        return Ok(
                            read_tracker.into_discovery(context, discovery_selection.clone())
                        );
                    }
                };
            for path in &metadata_files {
                read_tracker.record_metadata_path(path.clone());
            }
            for observation in &metadata_observations {
                read_tracker.record_metadata_observation(observation.clone());
            }
            let option_base = relevant_workspace_root(&file_path, &roots)
                .or_else(|| file_path.parent().map(Path::to_path_buf))
                .unwrap_or_else(|| {
                    project_file
                        .parent()
                        .map_or_else(PathBuf::new, Path::to_path_buf)
                });
            let cache_key = ProjectContextBuildCacheKey {
                project_file: project_file.clone(),
                roots: roots.clone(),
                deleted_paths: cache_path_list(deleted_paths),
                option_base,
                options: ProjectOptionsCacheKey::from(&options),
                overrides: effective_overrides.clone(),
                exclusions: exclusions.to_vec(),
                warnings: warnings.clone(),
                explicit,
            };
            let cached = project_context_cache_get(&cache_key, cancel, work_budget)?;
            let context = if let Some(cached) = cached {
                read_tracker.merge_project_context_reads(&cached, cancel, work_budget)?;
                cached.context
            } else {
                let mut build_tracker = ProjectReadTracker::with_budget(work_budget, deleted_paths);
                let context = build_project_context(
                    project_file,
                    &file_path,
                    &roots,
                    &options,
                    installation_config,
                    effective_overrides,
                    warnings,
                    explicit,
                    Vec::new(),
                    Vec::new(),
                    exclusions,
                    &mut build_tracker,
                    cancel,
                )?;
                let cached = CachedProjectContext {
                    context: context.clone(),
                    observations: build_tracker.observations,
                    metadata_paths: build_tracker.metadata_paths,
                    metadata_observations: build_tracker.metadata_observations,
                };
                read_tracker.merge_project_context_reads(&cached, cancel, work_budget)?;
                project_context_cache_insert(cache_key, cached);
                context
            };
            if let Some(work_budget) = work_budget {
                work_budget.check_cancelled()?;
            }
            Ok(read_tracker.into_discovery(context, discovery_selection.clone()))
        }
        ProjectSelection::Standalone {
            metadata_files,
            metadata_observations,
        } => {
            let context = build_standalone_context_with_overrides(
                &file_path,
                &roots,
                &options,
                warnings,
                true,
                metadata_files,
                metadata_observations,
                overrides,
                exclusions,
            )?;
            if let Some(work_budget) = work_budget {
                work_budget.check_cancelled()?;
            }
            Ok(read_tracker.into_discovery(context, discovery_selection.clone()))
        }
        ProjectSelection::Incomplete {
            metadata_files,
            metadata_observations,
            override_error,
        } => {
            let mut context = build_standalone_context_with_overrides(
                &file_path,
                &roots,
                &options,
                warnings,
                false,
                metadata_files,
                metadata_observations,
                overrides,
                exclusions,
            )?;
            if override_error.is_some() {
                context.override_error = override_error;
            }
            Ok(read_tracker.into_discovery(context, discovery_selection.clone()))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProjectSelection {
    Selected {
        path: PathBuf,
        explicit: bool,
        metadata_files: Vec<PathBuf>,
        metadata_observations: Vec<MetadataObservation>,
    },
    Standalone {
        metadata_files: Vec<PathBuf>,
        metadata_observations: Vec<MetadataObservation>,
    },
    Incomplete {
        metadata_files: Vec<PathBuf>,
        metadata_observations: Vec<MetadataObservation>,
        override_error: Option<String>,
    },
}

fn discovery_file_path(absolute_file: &Path, warnings: &mut Vec<String>) -> PathBuf {
    resolve_existing_path(absolute_file, warnings, "source file").unwrap_or_else(|| {
        let parent = absolute_file
            .parent()
            .and_then(|path| resolve_existing_path(path, warnings, "source directory"))
            .unwrap_or_else(|| {
                absolute_file
                    .parent()
                    .map_or_else(PathBuf::new, Path::to_path_buf)
            });
        parent.join(absolute_file.file_name().unwrap_or_default())
    })
}

pub fn runtime_project_selection(
    file: &Path,
    candidates: &ProjectCandidates,
    selections: &ProjectSelections,
) -> Option<(PathBuf, PathBuf)> {
    selections
        .iter()
        .filter(|(scope, _)| project_path_starts_with(file, scope))
        .filter(|(scope, _)| {
            candidates.directory.as_deref().is_none_or(|directory| {
                project_paths_equal(scope, directory) || project_path_starts_with(scope, directory)
            })
        })
        .max_by_key(|(scope, _)| scope.components().count())
        .map(|(scope, project)| (scope.clone(), project.clone()))
}

pub fn has_invalid_project_selection(context: &ProjectContext) -> bool {
    context.warnings.iter().any(|warning| {
        warning.starts_with("selected project ")
            && warning.contains(" is not a current candidate in ")
    })
}

pub fn selected_project_is_current(scope: &Path, selected: &Path) -> Result<bool, String> {
    selected_project_is_current_with_cancel(scope, selected, None)
}

pub fn selected_project_is_current_with_cancel(
    scope: &Path,
    selected: &Path,
    cancel: Option<&AtomicBool>,
) -> Result<bool, String> {
    selected_project_is_current_with_work_budget(scope, selected, cancel, None)
}

pub fn selected_project_is_current_with_work_budget(
    scope: &Path,
    selected: &Path,
    cancel: Option<&AtomicBool>,
    work_budget: Option<&dyn ProjectWorkBudget>,
) -> Result<bool, String> {
    selected_project_is_current_with_budget_and_deleted_paths(
        scope,
        selected,
        cancel,
        work_budget,
        &[],
    )
}

pub fn selected_project_is_current_with_budget_and_deleted_paths(
    scope: &Path,
    selected: &Path,
    cancel: Option<&AtomicBool>,
    work_budget: Option<&dyn ProjectWorkBudget>,
    deleted_paths: &[PathBuf],
) -> Result<bool, String> {
    let entries = project_directory_entries(scope, cancel, work_budget)?;
    Ok(entries.dproj.iter().any(|candidate| {
        !is_deleted_path(candidate, deleted_paths) && project_paths_equal(candidate, selected)
    }))
}

fn find_project_candidates(
    file: &Path,
    workspace_root: Option<&Path>,
    cancel: Option<&AtomicBool>,
    work_budget: Option<&dyn ProjectWorkBudget>,
    deleted_paths: &[PathBuf],
) -> Result<ProjectCandidates, String> {
    let mut directory = file.parent().map_or_else(PathBuf::new, Path::to_path_buf);
    loop {
        check_project_scan_cancel(cancel)?;
        let entries = project_directory_entries(&directory, cancel, work_budget)?;
        let mut project_files =
            filter_deleted_candidates(entries.dproj, deleted_paths, cancel, work_budget)?;
        if !project_files.is_empty() {
            sort_project_candidates(&mut project_files, cancel, work_budget)?;
            return Ok(ProjectCandidates {
                directory: Some(directory),
                files: project_files,
            });
        }
        if workspace_root.is_some_and(|root| project_paths_equal(&directory, root)) {
            break;
        }
        let Some(parent) = directory.parent() else {
            break;
        };
        if parent == directory {
            break;
        }
        directory = parent.to_path_buf();
    }
    Ok(ProjectCandidates::default())
}

fn record_candidate_memberships(
    file: &Path,
    boundary: Option<&Path>,
    tracker: &mut ProjectReadTracker<'_>,
    cancel: Option<&AtomicBool>,
    deleted_paths: &[PathBuf],
) -> Result<(), String> {
    let mut directory = file.parent().map_or_else(PathBuf::new, Path::to_path_buf);
    loop {
        check_project_scan_cancel(cancel)?;
        let membership = project_directory_entries(&directory, cancel, tracker.work_budget)
            .and_then(|entries| {
                if entries.candidate_overflow {
                    Err(format!(
                        "project candidate membership limit ({MAX_OWNERSHIP_CANDIDATES}) reached in {}",
                        directory.display()
                    ))
                } else {
                    let mut paths = entries.dproj;
                    paths.extend(entries.dpr_or_dpk);
                    let mut paths = filter_deleted_candidates(
                        paths,
                        deleted_paths,
                        cancel,
                        tracker.work_budget,
                    )?;
                    sort_project_candidates(&mut paths, cancel, tracker.work_budget)?;
                    Ok(ProjectCandidateMembership {
                        paths,
                        readable: true,
                    })
                }
            });
        if let Err(error) = &membership
            && (error == "request cancelled"
                || tracker
                    .work_budget
                    .is_some_and(|budget| budget.is_transient_error(error)))
        {
            return Err(error.clone());
        }
        tracker.record_candidate_membership(directory.clone(), membership);
        if boundary.is_some_and(|root| paths_equal_ci(&directory, root)) {
            break;
        }
        let Some(parent) = directory.parent() else {
            break;
        };
        if parent == directory {
            break;
        }
        directory = parent.to_path_buf();
    }
    Ok(())
}

pub fn project_candidate_membership(
    directory: &Path,
    cancel: Option<&AtomicBool>,
) -> Result<ProjectCandidateMembership, String> {
    project_candidate_membership_with_deleted_paths(directory, cancel, &[])
}

pub fn project_candidate_membership_with_deleted_paths(
    directory: &Path,
    cancel: Option<&AtomicBool>,
    deleted_paths: &[PathBuf],
) -> Result<ProjectCandidateMembership, String> {
    project_candidate_membership_with_deleted_paths_and_budget(
        directory,
        cancel,
        deleted_paths,
        None,
    )
}

pub fn project_candidate_membership_with_deleted_paths_and_budget(
    directory: &Path,
    cancel: Option<&AtomicBool>,
    deleted_paths: &[PathBuf],
    work_budget: Option<&dyn ProjectWorkBudget>,
) -> Result<ProjectCandidateMembership, String> {
    let mut entries = project_directory_entries(directory, cancel, work_budget)?;
    if entries.candidate_overflow {
        return Err(format!(
            "project candidate membership limit ({MAX_OWNERSHIP_CANDIDATES}) reached in {}",
            directory.display()
        ));
    }
    let mut dproj = entries.dproj;
    dproj.append(&mut entries.dpr_or_dpk);
    let mut dproj = filter_deleted_candidates(dproj, deleted_paths, cancel, work_budget)?;
    sort_project_candidates(&mut dproj, cancel, work_budget)?;
    Ok(ProjectCandidateMembership {
        paths: dproj,
        readable: true,
    })
}

type ProjectPathIdentity = Vec<std::ffi::OsString>;

fn project_path_identity(
    path: &Path,
    cancel: Option<&AtomicBool>,
    work_budget: Option<&dyn ProjectWorkBudget>,
) -> Result<ProjectPathIdentity, String> {
    let case_insensitive = path_identity::is_case_insensitive(path);
    let mut identity = Vec::new();
    for component in path.components() {
        check_project_scan_cancel(cancel)?;
        if let Some(budget) = work_budget {
            budget.check_cancelled()?;
            budget.charge_path_visits(1)?;
            budget.charge_path_bytes(project_path_materialization_bytes(component.as_os_str())?)?;
        }
        if case_insensitive {
            let mut key = component.as_os_str().to_string_lossy().into_owned();
            key.make_ascii_lowercase();
            identity.push(key.into());
        } else {
            identity.push(component.as_os_str().to_os_string());
        }
    }
    Ok(identity)
}

fn project_path_materialization_bytes(path: &std::ffi::OsStr) -> Result<usize, String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(path.as_bytes().len())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        path.encode_wide()
            .count()
            .checked_mul(3)
            .ok_or_else(|| "project path key size overflow".to_string())
    }
    #[cfg(not(any(unix, windows)))]
    {
        Ok(path.to_string_lossy().len())
    }
}

fn filter_deleted_candidates(
    paths: Vec<PathBuf>,
    deleted_paths: &[PathBuf],
    cancel: Option<&AtomicBool>,
    work_budget: Option<&dyn ProjectWorkBudget>,
) -> Result<Vec<PathBuf>, String> {
    let mut deleted = HashSet::new();
    for path in deleted_paths {
        check_project_scan_cancel(cancel)?;
        if let Some(budget) = work_budget {
            budget.check_cancelled()?;
            budget.charge_path_visits(1)?;
        }
        deleted.insert(project_path_identity(path, cancel, work_budget)?);
    }

    let mut retained = Vec::new();
    for path in paths {
        check_project_scan_cancel(cancel)?;
        if let Some(budget) = work_budget {
            budget.check_cancelled()?;
            budget.charge_path_visits(1)?;
        }
        if !deleted.contains(&project_path_identity(&path, cancel, work_budget)?) {
            retained.push(path);
        }
    }
    Ok(retained)
}

fn sort_project_candidates(
    paths: &mut Vec<PathBuf>,
    cancel: Option<&AtomicBool>,
    work_budget: Option<&dyn ProjectWorkBudget>,
) -> Result<(), String> {
    check_project_scan_cancel(cancel)?;
    if let Some(budget) = work_budget {
        budget.check_cancelled()?;
        let slots = paths
            .len()
            .checked_mul(4)
            .ok_or_else(|| "project candidate sort storage overflow".to_string())?;
        budget.charge_path_visits(slots)?;
    }
    let mut keyed_paths = Vec::with_capacity(paths.len());
    for path in paths.iter() {
        check_project_scan_cancel(cancel)?;
        if let Some(budget) = work_budget {
            budget.check_cancelled()?;
            budget.charge_path_bytes(project_path_materialization_bytes(path.as_os_str())?)?;
        }
        let key = path.to_string_lossy();
        keyed_paths.push(key.into_owned());
    }

    let mut source = std::mem::take(paths)
        .into_iter()
        .zip(keyed_paths)
        .map(Some)
        .collect::<Vec<_>>();
    let mut destination = (0..source.len()).map(|_| None).collect::<Vec<_>>();
    let mut width = 1usize;
    while width < source.len() {
        let run_width = width.saturating_mul(2);
        for start in (0..source.len()).step_by(run_width) {
            let middle = start.saturating_add(width).min(source.len());
            let end = start.saturating_add(run_width).min(source.len());
            let (mut left, mut right, mut output) = (start, middle, start);
            while left < middle && right < end {
                check_project_scan_cancel(cancel)?;
                if let Some(budget) = work_budget {
                    budget.check_cancelled()?;
                    budget.charge_path_visits(1)?;
                }
                let left_key = &source[left].as_ref().expect("unconsumed left merge item").1;
                let right_key = &source[right]
                    .as_ref()
                    .expect("unconsumed right merge item")
                    .1;
                if left_key <= right_key {
                    destination[output] = source[left].take();
                    left += 1;
                } else {
                    destination[output] = source[right].take();
                    right += 1;
                }
                output += 1;
            }
            while left < middle {
                check_project_scan_cancel(cancel)?;
                if let Some(budget) = work_budget {
                    budget.check_cancelled()?;
                    budget.charge_path_visits(1)?;
                }
                destination[output] = source[left].take();
                left += 1;
                output += 1;
            }
            while right < end {
                check_project_scan_cancel(cancel)?;
                if let Some(budget) = work_budget {
                    budget.check_cancelled()?;
                    budget.charge_path_visits(1)?;
                }
                destination[output] = source[right].take();
                right += 1;
                output += 1;
            }
        }
        std::mem::swap(&mut source, &mut destination);
        width = width.saturating_mul(2);
    }
    for item in source {
        check_project_scan_cancel(cancel)?;
        if let Some(budget) = work_budget {
            budget.check_cancelled()?;
            budget.charge_path_visits(1)?;
        }
        let (path, _) = item.expect("sorted candidate item remains present");
        paths.push(path);
    }
    check_project_scan_cancel(cancel)?;
    if let Some(budget) = work_budget {
        budget.check_cancelled()?;
    }
    Ok(())
}

pub(super) fn project_path_starts_with(path: &Path, root: &Path) -> bool {
    path_identity::path_starts_with(path, root)
}

pub(super) fn project_paths_equal(left: &Path, right: &Path) -> bool {
    path_identity::paths_equal(left, right)
}

pub(super) fn is_deleted_path(path: &Path, deleted_paths: &[PathBuf]) -> bool {
    deleted_paths
        .iter()
        .any(|deleted| project_paths_equal(path, deleted))
}
fn normalize_workspace_roots(
    roots: &[PathBuf],
    warnings: &mut Vec<String>,
) -> Result<Vec<PathBuf>, String> {
    let mut normalized = Vec::new();
    for root in roots {
        let absolute = absolute_lexical(root)?;
        if let Some(actual) = resolve_existing_path(&absolute, warnings, "workspace root") {
            add_unique_path(&mut normalized, actual);
        } else if !is_foreign_windows_path(root) {
            warnings.push(format!(
                "workspace root could not be resolved; preserving its lexical boundary: {}",
                root.display()
            ));
            add_unique_path(&mut normalized, absolute);
        }
    }
    Ok(normalized)
}

fn explicit_project_file(
    requested: &Path,
    roots: &[PathBuf],
    file: &Path,
    warnings: &mut Vec<String>,
) -> ProjectSelection {
    let requested_text = requested.to_string_lossy();
    if cfg!(not(windows)) && is_windows_absolute_text(&requested_text) {
        warnings.push(format!(
            "Windows project path is unavailable on Linux and was omitted: {}",
            requested.display()
        ));
        return ProjectSelection::Incomplete {
            metadata_files: Vec::new(),
            metadata_observations: Vec::new(),
            override_error: None,
        };
    }
    let requested = PathBuf::from(requested_text.replace('\\', "/"));

    let mut candidates = Vec::new();
    if requested.is_absolute() {
        if let Some(path) = resolve_existing_path(&requested, warnings, "project file") {
            candidates.push(path);
        }
    } else {
        let bases: Vec<PathBuf> = if roots.is_empty() {
            file.parent()
                .map_or_else(Vec::new, |parent| vec![parent.to_path_buf()])
        } else {
            roots.to_vec()
        };
        for root in bases {
            if let Some(path) =
                resolve_existing_path(&root.join(&requested), warnings, "project file")
            {
                add_unique_path(&mut candidates, path);
            }
        }
    }

    match candidates.len() {
        0 => {
            warnings.push(format!(
                "explicit project file was not found: {}",
                requested.display()
            ));
            ProjectSelection::Incomplete {
                metadata_files: Vec::new(),
                metadata_observations: Vec::new(),
                override_error: None,
            }
        }
        1 => ProjectSelection::Selected {
            path: candidates.remove(0),
            explicit: true,
            metadata_files: Vec::new(),
            metadata_observations: Vec::new(),
        },
        _ => {
            warnings.push(format!(
                "multiple explicit project files matched {}; no project selected",
                requested.display()
            ));
            ProjectSelection::Incomplete {
                metadata_files: Vec::new(),
                metadata_observations: Vec::new(),
                override_error: None,
            }
        }
    }
}

// Automatic project selection threads the discovery policy and observation
// state through candidate probing without hiding runtime inputs in globals.
#[allow(clippy::too_many_arguments)]
fn discover_project_file(
    file: &Path,
    workspace_root: Option<&Path>,
    roots: &[PathBuf],
    options: &ProjectOptions,
    overrides: &OverrideSession,
    warnings: &mut Vec<String>,
    tracker: &mut ProjectReadTracker<'_>,
    cancel: Option<&AtomicBool>,
    exclusions: &[String],
    deleted_paths: &[PathBuf],
) -> ProjectSelection {
    let mut directory = file.parent().map_or_else(PathBuf::new, Path::to_path_buf);
    let mut fallback_dpr = None;
    let mut ambiguous_fallback_dpr = None;

    loop {
        if let Err(error) = check_project_scan_cancel(cancel) {
            warnings.push(error);
            return ProjectSelection::Incomplete {
                metadata_files: Vec::new(),
                metadata_observations: Vec::new(),
                override_error: None,
            };
        }
        let entries = match project_directory_entries(&directory, cancel, tracker.work_budget) {
            Ok(entries) => entries,
            Err(error) => {
                warnings.push(error);
                return ProjectSelection::Incomplete {
                    metadata_files: Vec::new(),
                    metadata_observations: Vec::new(),
                    override_error: None,
                };
            }
        };
        let mut dproj = entries.dproj;
        dproj.retain(|path| !is_deleted_path(path, deleted_paths));
        let mut dpr_or_dpk = entries.dpr_or_dpk;
        dpr_or_dpk.retain(|path| !is_deleted_path(path, deleted_paths));

        if !dproj.is_empty() {
            return choose_project_candidate(
                dproj,
                &directory,
                "project files",
                file,
                false,
                roots,
                options,
                overrides,
                warnings,
                tracker,
                cancel,
                exclusions,
            );
        }
        if fallback_dpr.is_none() && ambiguous_fallback_dpr.is_none() && !dpr_or_dpk.is_empty() {
            if dpr_or_dpk.len() == 1 {
                fallback_dpr = dpr_or_dpk.into_iter().next();
            } else {
                ambiguous_fallback_dpr = Some((dpr_or_dpk, directory.clone()));
            }
        }

        if workspace_root.is_some_and(|root| paths_equal_ci(&directory, root)) {
            break;
        }
        let Some(parent) = directory.parent() else {
            break;
        };
        if parent == directory {
            break;
        }
        directory = parent.to_path_buf();
    }
    if let Some((candidates, directory)) = ambiguous_fallback_dpr {
        return choose_project_candidate(
            candidates,
            &directory,
            "DPR/DPK files",
            file,
            true,
            roots,
            options,
            overrides,
            warnings,
            tracker,
            cancel,
            exclusions,
        );
    }
    fallback_dpr.map_or(
        ProjectSelection::Standalone {
            metadata_files: Vec::new(),
            metadata_observations: Vec::new(),
        },
        |path| ProjectSelection::Selected {
            path,
            explicit: false,
            metadata_files: Vec::new(),
            metadata_observations: Vec::new(),
        },
    )
}

pub(super) fn push_bounded_candidate(candidates: &mut Vec<PathBuf>, path: PathBuf) {
    if candidates.len() <= MAX_OWNERSHIP_CANDIDATES {
        candidates.push(path);
    }
}

// Candidate probing keeps the filesystem inputs, evaluator options, and
// immutable override session explicit at this boundary.
#[allow(clippy::too_many_arguments)]
fn choose_project_candidate(
    mut candidates: Vec<PathBuf>,
    directory: &Path,
    kind: &str,
    file: &Path,
    allow_membership_selection: bool,
    roots: &[PathBuf],
    options: &ProjectOptions,
    overrides: &OverrideSession,
    warnings: &mut Vec<String>,
    tracker: &mut ProjectReadTracker<'_>,
    cancel: Option<&AtomicBool>,
    exclusions: &[String],
) -> ProjectSelection {
    if candidates.len() == 1 {
        return ProjectSelection::Selected {
            path: candidates.pop().expect("one candidate"),
            explicit: false,
            metadata_files: Vec::new(),
            metadata_observations: Vec::new(),
        };
    }
    candidates.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));

    if candidates.len() > MAX_OWNERSHIP_CANDIDATES {
        warnings.push(format!(
            "automatic project ownership candidate limit ({MAX_OWNERSHIP_CANDIDATES}) reached in {}; no project selected",
            directory.display()
        ));
        let metadata_files = candidates
            .into_iter()
            .take(MAX_OWNERSHIP_METADATA_FILES)
            .collect();
        return ProjectSelection::Incomplete {
            metadata_files,
            metadata_observations: Vec::new(),
            override_error: None,
        };
    }

    // Ownership checks stay within the already discovered candidate
    // directory. They read each candidate's bounded metadata, but never turn
    // project selection into a recursive workspace scan.
    let mut budget = OwnershipProbeBudget::default();
    let mut consulted_metadata = Vec::new();
    let mut consulted_observations = Vec::new();
    let mut owned = Vec::new();
    let mut incomplete = false;
    let mut candidate_warnings = Vec::new();
    let mut candidate_override_error = None;
    for candidate in &candidates {
        if check_project_scan_cancel(cancel).is_err() {
            return ProjectSelection::Incomplete {
                metadata_files: consulted_metadata,
                metadata_observations: consulted_observations,
                override_error: None,
            };
        }
        let evaluation = inspect_project_candidate(
            candidate,
            file,
            roots,
            options,
            overrides,
            exclusions,
            &mut budget,
            tracker,
            cancel,
        );
        for observation in &evaluation.metadata_observations {
            if consulted_observations.len() >= MAX_OWNERSHIP_METADATA_FILES {
                budget.exhaust("automatic ownership metadata read-set");
                incomplete = true;
                break;
            }
            add_metadata_observation(&mut consulted_observations, observation.clone());
        }
        if !budget.reserve_metadata_files(evaluation.metadata_files.len()) {
            incomplete = true;
        }
        for metadata_file in evaluation.metadata_files {
            if consulted_metadata
                .iter()
                .any(|existing| existing == &metadata_file)
            {
                continue;
            }
            if consulted_metadata.len() >= MAX_OWNERSHIP_METADATA_FILES {
                budget.exhaust("automatic ownership metadata read-set");
                incomplete = true;
                break;
            }
            consulted_metadata.push(metadata_file);
        }
        append_bounded_warnings(&mut candidate_warnings, evaluation.warnings, &mut budget);
        if let Some(error) = evaluation.override_error {
            candidate_override_error.get_or_insert(error);
        }
        match evaluation.ownership {
            CandidateOwnership::Owned => owned.push(candidate.clone()),
            CandidateOwnership::NotOwned => {}
            CandidateOwnership::Incomplete => incomplete = true,
        }
        if budget.exhausted || incomplete || owned.len() >= 2 {
            break;
        }
    }

    if allow_membership_selection && owned.len() == 1 && !incomplete && !budget.exhausted {
        return ProjectSelection::Selected {
            path: owned.pop().expect("one owned candidate"),
            explicit: false,
            metadata_files: consulted_metadata,
            metadata_observations: consulted_observations,
        };
    }

    warnings.push(format!(
        "multiple {kind} in {}; no project selected: {}",
        directory.display(),
        candidates
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    warnings.extend(candidate_warnings);
    if budget.exhausted {
        warnings.push(format!(
            "automatic project ownership probe limit reached; no project selected: {}",
            budget.exhaustion_reason.unwrap_or("aggregate budget")
        ));
    }
    ProjectSelection::Incomplete {
        metadata_files: consulted_metadata,
        metadata_observations: consulted_observations,
        override_error: candidate_override_error,
    }
}

#[derive(Debug, Default)]
struct OwnershipProbeBudget {
    metadata_files: usize,
    source_files: usize,
    source_bytes: u64,
    warning_bytes: usize,
    exhausted: bool,
    exhaustion_reason: Option<&'static str>,
}

impl OwnershipProbeBudget {
    fn reserve_metadata_files(&mut self, count: usize) -> bool {
        let Some(total) = self.metadata_files.checked_add(count) else {
            self.exhaust("aggregate ownership metadata read-set");
            return false;
        };
        if total > MAX_OWNERSHIP_METADATA_FILES {
            self.exhaust("aggregate ownership metadata read-set");
            return false;
        }
        self.metadata_files = total;
        true
    }

    fn reserve_source_file(&mut self, bytes: u64) -> bool {
        let Some(file_count) = self.source_files.checked_add(1) else {
            self.exhaust("aggregate ownership source-file work");
            return false;
        };
        let Some(total_bytes) = self.source_bytes.checked_add(bytes) else {
            self.exhaust("aggregate ownership source-byte work");
            return false;
        };
        if file_count > MAX_OWNERSHIP_SOURCE_FILES || total_bytes > MAX_OWNERSHIP_SOURCE_BYTES {
            self.exhaust("aggregate ownership source-file work");
            return false;
        }
        self.source_files = file_count;
        self.source_bytes = total_bytes;
        true
    }

    fn reserve_warning_bytes(&mut self, bytes: usize) -> bool {
        let Some(total) = self.warning_bytes.checked_add(bytes) else {
            self.exhaust("aggregate ownership diagnostics");
            return false;
        };
        if total > MAX_OWNERSHIP_WARNING_BYTES {
            self.exhaust("aggregate ownership diagnostics");
            return false;
        }
        self.warning_bytes = total;
        true
    }

    fn exhaust(&mut self, reason: &'static str) {
        self.exhausted = true;
        self.exhaustion_reason.get_or_insert(reason);
    }
}

fn append_bounded_warnings(
    target: &mut Vec<String>,
    warnings: Vec<String>,
    budget: &mut OwnershipProbeBudget,
) {
    for warning in warnings {
        if !budget.reserve_warning_bytes(warning.len()) {
            break;
        }
        target.push(warning);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateOwnership {
    Owned,
    NotOwned,
    Incomplete,
}

#[derive(Debug)]
struct CandidateEvaluation {
    ownership: CandidateOwnership,
    metadata_files: Vec<PathBuf>,
    metadata_observations: Vec<MetadataObservation>,
    warnings: Vec<String>,
    override_error: Option<String>,
}

// Candidate inspection keeps project evaluation, bounded probing, observation
// tracking, and cancellation explicit so each read remains attributable.
#[allow(clippy::too_many_arguments)]
fn inspect_project_candidate(
    project_file: &Path,
    file: &Path,
    roots: &[PathBuf],
    options: &ProjectOptions,
    overrides: &OverrideSession,
    exclusions: &[String],
    budget: &mut OwnershipProbeBudget,
    tracker: &mut ProjectReadTracker<'_>,
    cancel: Option<&AtomicBool>,
) -> CandidateEvaluation {
    let mut metadata_files = vec![project_file.to_path_buf()];
    let mut metadata_observations = Vec::new();
    let effective_overrides = match effective_overrides_for_project(project_file, roots, overrides)
    {
        Ok(overrides) => overrides,
        Err(error) => {
            return CandidateEvaluation {
                ownership: CandidateOwnership::Incomplete,
                metadata_files,
                metadata_observations,
                warnings: vec![format!(
                    "could not evaluate project candidate {}: {error}",
                    project_file.display()
                )],
                override_error: Some(error),
            };
        }
    };
    let installation_workspace = project_file
        .parent()
        .and_then(|directory| relevant_override_workspace_root(directory, roots));
    let installation_config = overrides
        .configuration_for(installation_workspace.as_deref(), Some(project_file))
        .unwrap_or_else(|_| {
            crate::installation_config::ProjectConfiguration::from_layers(Vec::new())
        });
    match build_project_context(
        project_file.to_path_buf(),
        file,
        roots,
        options,
        installation_config,
        effective_overrides,
        Vec::new(),
        false,
        Vec::new(),
        Vec::new(),
        exclusions,
        tracker,
        cancel,
    ) {
        Ok(context) => {
            for metadata_file in &context.metadata_files {
                add_unique_path(&mut metadata_files, metadata_file.clone());
            }
            metadata_observations.extend(context.metadata_observations.iter().cloned());
            if !context.discovery_complete {
                return CandidateEvaluation {
                    ownership: CandidateOwnership::Incomplete,
                    metadata_files,
                    metadata_observations,
                    warnings: context.warnings,
                    override_error: None,
                };
            }
            let membership = inspect_source_membership(&context, budget, tracker, cancel);
            for metadata_file in &membership.metadata_files {
                add_unique_path(&mut metadata_files, metadata_file.clone());
            }
            metadata_observations.extend(membership.metadata_observations);
            let (owns_source, identity_unverified) =
                source_ownership(&membership.source_entries, file, &context.read_policy);
            let ownership = if budget.exhausted || !membership.complete || identity_unverified {
                CandidateOwnership::Incomplete
            } else if owns_source {
                CandidateOwnership::Owned
            } else {
                CandidateOwnership::NotOwned
            };
            let mut warnings = context.warnings;
            warnings.extend(membership.warnings);
            CandidateEvaluation {
                ownership,
                metadata_files,
                metadata_observations,
                warnings,
                override_error: None,
            }
        }
        Err(error) => CandidateEvaluation {
            ownership: CandidateOwnership::Incomplete,
            metadata_files,
            metadata_observations,
            warnings: vec![format!(
                "could not inspect project candidate {}: {error}",
                project_file.display()
            )],
            override_error: None,
        },
    }
}

#[derive(Debug, Default)]
struct SourceMembershipInspection {
    complete: bool,
    source_entries: Vec<ProjectPathEntry>,
    metadata_files: Vec<PathBuf>,
    metadata_observations: Vec<MetadataObservation>,
    warnings: Vec<String>,
}

fn inspect_source_membership(
    context: &ProjectContext,
    budget: &mut OwnershipProbeBudget,
    tracker: &mut ProjectReadTracker<'_>,
    cancel: Option<&AtomicBool>,
) -> SourceMembershipInspection {
    inspect_source_membership_impl(context, budget, Some(tracker), cancel, &mut |_| {})
}

#[cfg(test)]
fn inspect_source_membership_with_hook(
    context: &ProjectContext,
    budget: &mut OwnershipProbeBudget,
    before_read: &mut dyn FnMut(&Path),
) -> SourceMembershipInspection {
    inspect_source_membership_impl(context, budget, None, None, before_read)
}

fn inspect_source_membership_impl(
    context: &ProjectContext,
    budget: &mut OwnershipProbeBudget,
    mut tracker: Option<&mut ProjectReadTracker<'_>>,
    cancel: Option<&AtomicBool>,
    before_read: &mut dyn FnMut(&Path),
) -> SourceMembershipInspection {
    let mut inspection = SourceMembershipInspection {
        complete: true,
        ..SourceMembershipInspection::default()
    };
    let mut pending = Vec::new();
    if let Some(main_source) = &context.main_source_entry {
        pending.push(main_source.clone());
    }
    pending.extend(context.explicit_unit_entries.values().flatten().cloned());
    let mut queued = HashSet::new();
    let mut cursor = 0;

    while let Some(source_entry) = pending.get(cursor).cloned() {
        if check_project_scan_cancel(cancel).is_err() {
            inspection.complete = false;
            inspection.warnings.push("request cancelled".to_string());
            break;
        }
        cursor += 1;
        if !queued.insert((source_entry.path.clone(), source_entry.provenance.clone())) {
            continue;
        }
        let source_path = &source_entry.path;
        inspection.source_entries.push(source_entry.clone());
        add_unique_path(&mut inspection.metadata_files, source_path.clone());

        let size = match fs::symlink_metadata(source_path) {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                inspection.complete = false;
                inspection.warnings.push(format!(
                    "could not inspect source membership file {}: {error}",
                    source_path.display()
                ));
                continue;
            }
        };
        if !context.read_policy.allows_entry(&source_entry) {
            inspection.complete = false;
            inspection.warnings.push(format!(
                "ignored source membership file outside authorized read roots: {}",
                source_path.display()
            ));
            continue;
        }
        if size > MAX_MAIN_SOURCE_BYTES {
            inspection.complete = false;
            inspection.warnings.push(format!(
                "source membership file {} exceeds the {} byte safety limit",
                source_path.display(),
                MAX_MAIN_SOURCE_BYTES
            ));
            continue;
        }
        // Reserve the attempt before opening the payload.  Include the
        // reader's one-byte overflow probe in the reservation so a file that
        // grows after this stat cannot consume bytes outside the aggregate
        // ownership budget.  Keeping the reservation on read/decode errors
        // charges failed attempts conservatively as well.
        if !budget.reserve_source_file(size.saturating_add(1)) {
            inspection.complete = false;
            inspection.warnings.push(format!(
                "automatic source membership probe limit reached at {}",
                source_path.display()
            ));
            break;
        }
        before_read(source_path);
        let (contents, observation) = match context
            .read_policy
            .read_payload_with_observation(&source_entry, size)
        {
            Ok(payload) => payload,
            Err(error) => {
                inspection.complete = false;
                inspection.warnings.push(format!(
                    "could not read source membership file {}: {error}",
                    source_path.display()
                ));
                continue;
            }
        };
        add_metadata_observation(&mut inspection.metadata_observations, observation);
        if let Some(tracker) = tracker.as_deref_mut()
            && let Ok(stamp) = project_read_stamp(source_path)
        {
            tracker.record(source_path, stamp, contents.as_bytes());
        }
        let parsed = parse_unit_membership(&contents);
        if !parsed.exhaustive {
            inspection.complete = false;
            inspection.warnings.push(format!(
                "source membership is incomplete for {}; bare uses/contains clauses require dependency resolution",
                source_path.display()
            ));
        }
        if contains_compiler_include_directive(&contents) {
            inspection.complete = false;
            inspection.warnings.push(format!(
                "source membership is incomplete for {}; compiler include directives require dependency resolution",
                source_path.display()
            ));
        }
        let Some(base) = source_path.parent() else {
            continue;
        };
        for (_, raw_path) in parsed.explicit_paths {
            if check_project_scan_cancel(cancel).is_err() {
                inspection.complete = false;
                inspection.warnings.push("request cancelled".to_string());
                break;
            }
            if is_compiled_reference(&raw_path) {
                continue;
            }
            let Some(entry) = resolve_project_path_entry(
                &raw_path,
                base,
                &context.overrides,
                &mut inspection.warnings,
                "ownership source membership",
                true,
                false,
            ) else {
                inspection.complete = false;
                continue;
            };
            let entry = inherit_path_provenance(entry, &source_entry.provenance);
            add_unique_path(&mut inspection.metadata_files, entry.path.clone());
            if !queued.contains(&(entry.path.clone(), entry.provenance.clone())) {
                pending.push(entry);
            }
        }
    }
    inspection
}

fn source_ownership(
    entries: &[ProjectPathEntry],
    target: &Path,
    read_policy: &ReadPolicy,
) -> (bool, bool) {
    if filesystem_identity_unverified(target)
        || entries.iter().any(|entry| {
            !read_policy.allows_entry(entry) || filesystem_identity_unverified(&entry.path)
        })
    {
        return (false, true);
    }
    let Some(target_identity) = fs::canonicalize(target).ok() else {
        return (false, true);
    };
    let mut owns_source = false;
    let mut identity_unverified = false;
    for entry in entries {
        let Some(identity) = fs::canonicalize(&entry.path).ok() else {
            identity_unverified = true;
            continue;
        };
        owns_source |= path_identity::paths_equal(&identity, &target_identity);
    }
    (owns_source, identity_unverified)
}

pub(super) fn filesystem_identity_unverified(path: &Path) -> bool {
    for ancestor in path.ancestors() {
        if ancestor.as_os_str().is_empty() {
            break;
        }
        let Ok(metadata) = fs::symlink_metadata(ancestor) else {
            return true;
        };
        if metadata.file_type().is_symlink() {
            return true;
        }
    }
    false
}

fn contains_compiler_include_directive(source: &str) -> bool {
    let lower = source.to_ascii_lowercase();
    let mut cursor = 0;
    while let Some(relative_start) = lower[cursor..].find("{$") {
        let start = cursor + relative_start + 2;
        let rest = lower[start..].trim_start();
        if is_include_directive_name(rest) {
            return true;
        }
        cursor = start;
    }
    let mut cursor = 0;
    while let Some(relative_start) = lower[cursor..].find("(*$") {
        let start = cursor + relative_start + 3;
        let rest = lower[start..].trim_start();
        if is_include_directive_name(rest) {
            return true;
        }
        cursor = start;
    }
    false
}

fn is_include_directive_name(rest: &str) -> bool {
    let is_boundary = |value: &str| match value.chars().next() {
        None => true,
        Some(character) => character.is_whitespace() || character == '\'' || character == '"',
    };
    rest.strip_prefix("include").is_some_and(is_boundary)
        || rest.strip_prefix('i').is_some_and(is_boundary)
}

pub(super) fn relevant_workspace_root(file: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    roots
        .iter()
        .filter(|root| path_starts_with_ci(file, root))
        .max_by_key(|root| root.components().count())
        .cloned()
}

pub(super) fn standalone_overrides(
    file: &Path,
    roots: &[PathBuf],
    session: &OverrideSession,
) -> Result<EffectiveOverrides, String> {
    let workspace_root = relevant_override_workspace_root(file, roots);
    session.effective_for(workspace_root.as_deref(), None)
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    #[cfg(unix)]
    use std::io::Write;
    struct TestProjectWorkBudget {
        remaining_visits: Cell<usize>,
        path_visits: Cell<usize>,
        cancel_after_visits: Option<usize>,
        cancelled: AtomicBool,
    }

    impl TestProjectWorkBudget {
        fn limited_path_visits(limit: usize) -> Self {
            Self {
                remaining_visits: Cell::new(limit),
                path_visits: Cell::new(0),
                cancel_after_visits: None,
                cancelled: AtomicBool::new(false),
            }
        }

        fn cancel_after_path_visits(limit: usize) -> Self {
            Self {
                remaining_visits: Cell::new(usize::MAX),
                path_visits: Cell::new(0),
                cancel_after_visits: Some(limit),
                cancelled: AtomicBool::new(false),
            }
        }
    }

    impl crate::ProjectWorkBudget for TestProjectWorkBudget {
        fn check_cancelled(&self) -> Result<(), String> {
            if self.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                Err("request cancelled".to_string())
            } else {
                Ok(())
            }
        }

        fn charge_path_visits(&self, amount: usize) -> Result<(), String> {
            self.check_cancelled()?;
            if amount > self.remaining_visits.get() {
                return Err("test work budget exceeded".to_string());
            }
            self.remaining_visits
                .set(self.remaining_visits.get().saturating_sub(amount));
            let next = self.path_visits.get().saturating_add(amount);
            self.path_visits.set(next);
            if self.cancel_after_visits.is_some_and(|limit| next >= limit) {
                self.cancelled
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(())
        }

        fn ensure_file_read_fits(&self, _max_bytes: usize) -> Result<(), String> {
            self.check_cancelled()
        }

        fn charge_file_bytes(&self, _amount: usize) -> Result<(), String> {
            self.check_cancelled()
        }

        fn is_transient_error(&self, error: &str) -> bool {
            error == "test work budget exceeded"
        }
    }
    struct FlagSettingBudget<'a> {
        calls: Cell<usize>,
        set_on_call: usize,
        cancel: &'a AtomicBool,
    }

    impl FlagSettingBudget<'_> {
        fn tick(&self) -> Result<(), String> {
            let call = self.calls.get() + 1;
            self.calls.set(call);
            if call == self.set_on_call {
                self.cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(())
        }
    }

    impl super::ProjectWorkBudget for FlagSettingBudget<'_> {
        fn check_cancelled(&self) -> Result<(), String> {
            self.tick()
        }
        fn charge_path_visits(&self, _amount: usize) -> Result<(), String> {
            self.tick()
        }
        fn ensure_file_read_fits(&self, _max_bytes: usize) -> Result<(), String> {
            self.tick()
        }
        fn charge_file_bytes(&self, _amount: usize) -> Result<(), String> {
            self.tick()
        }
    }

    #[test]
    fn discovery_never_returns_ok_after_the_token_was_set() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let root = temp.path().to_path_buf();
        let file = root.join("Unit1.pas");
        fs::write(&file, b"unit Unit1;\ninterface\nimplementation\nend.\n").expect("unit");
        let options = ProjectOptions {
            source_paths: vec!["src".to_string()],
            ..ProjectOptions::default()
        };
        fs::create_dir(root.join("src")).expect("source directory");
        let overrides = OverrideSession::default();
        let selections = super::ProjectSelections::new();

        for set_on_call in 1..=120 {
            let cancel = AtomicBool::new(false);
            let budget = FlagSettingBudget {
                calls: Cell::new(0),
                set_on_call,
                cancel: &cancel,
            };
            let result = super::discover_with_selections_and_observations_with_work_budget_and_optional_cancel_and_deleted_paths(
                &file,
                std::slice::from_ref(&root),
                &options,
                &selections,
                &overrides,
                &[],
                Some(&cancel),
                Some(&budget),
                &[],
            );
            assert!(
                result.is_err() || !cancel.load(std::sync::atomic::Ordering::Relaxed),
                "discovery returned Ok although the token was set on budget call {set_on_call}"
            );
        }
    }

    #[test]
    fn candidate_membership_reports_directory_read_errors() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let file = temp.path().join("not-a-directory");
        fs::write(&file, b"source").expect("regular file");
        let cancel = AtomicBool::new(false);

        let error = project_candidate_membership(&file, Some(&cancel))
            .expect_err("a non-directory cannot produce a membership observation");
        assert!(
            error.contains("project directory"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn candidate_membership_reports_candidate_overflow() {
        let temp = tempfile::tempdir().expect("temporary directory");
        for index in 0..=MAX_OWNERSHIP_CANDIDATES {
            fs::write(temp.path().join(format!("Project{index:02}.dproj")), b"")
                .expect("project candidate");
        }
        let cancel = AtomicBool::new(false);

        let error = project_candidate_membership(temp.path(), Some(&cancel))
            .expect_err("truncated candidate membership is not a valid observation");
        assert!(error.contains("candidate"), "unexpected error: {error}");
    }

    #[test]
    fn candidate_membership_reports_directory_entry_budget_exhaustion() {
        let temp = tempfile::tempdir().expect("temporary directory");
        for index in 0..=MAX_PROJECT_DIRECTORY_ENTRIES {
            fs::write(temp.path().join(format!("Noise{index:04}.txt")), b"").expect("noise entry");
        }
        let cancel = AtomicBool::new(false);

        let error = project_candidate_membership(temp.path(), Some(&cancel))
            .expect_err("an incomplete directory scan is not a valid observation");
        assert!(error.contains("entry limit"), "unexpected error: {error}");
    }

    #[test]
    fn candidate_membership_honors_cancellation_during_entry_scan() {
        let temp = tempfile::tempdir().expect("temporary directory");
        for index in 0..4 {
            fs::write(temp.path().join(format!("Noise{index}.txt")), b"").expect("noise entry");
        }
        let cancel = AtomicBool::new(false);
        let _guard = test_cancel_project_scan_after_checks(2);

        let error = project_candidate_membership(temp.path(), Some(&cancel))
            .expect_err("cancellation during enumeration must abort the observation");
        assert_eq!(error, "request cancelled");
    }

    #[test]
    fn project_candidates_refuse_unbudgeted_tombstone_postprocessing() {
        const PROJECTS: usize = 512;
        let temp = tempfile::tempdir().expect("temporary directory");
        let root = temp.path();
        let source = root.join("Consumer.pas");
        fs::write(&source, "unit Consumer; interface implementation end.").expect("source file");
        for index in 0..PROJECTS {
            fs::write(root.join(format!("Project{index:03}.dproj")), b"<Project/>")
                .expect("project candidate");
        }
        let deleted_paths = (0..PROJECTS)
            .map(|index| root.join(format!("Deleted{index:03}.dproj")))
            .collect::<Vec<_>>();
        // Enumeration charges one directory probe and one visit for each entry
        // plus the terminal iterator advance. No budget remains for filtering,
        // key materialization, or sort admission.
        let enumeration_visits = 1 + PROJECTS + 1 + 1; // probe, entries, terminal advance
        let budget = TestProjectWorkBudget::limited_path_visits(enumeration_visits);

        let error = super::find_project_candidates(
            &source,
            Some(root),
            None,
            Some(&budget),
            &deleted_paths,
        )
        .expect_err("candidate list post-processing must share the enumeration account");
        assert_eq!(error, "test work budget exceeded");
        assert_eq!(budget.path_visits.get(), enumeration_visits);
    }

    #[test]
    fn project_candidate_sort_charges_storage_keys_and_each_comparison() {
        const CANDIDATES: usize = 512;
        let mut paths = (0..CANDIDATES)
            .map(|index| std::path::PathBuf::from(format!("/workspace/Project{index:03}.dproj")))
            .collect::<Vec<_>>();
        let key_bytes = paths
            .iter()
            .map(|path| super::project_path_materialization_bytes(path.as_os_str()).unwrap())
            .sum::<usize>();
        let admitted_pre_sort_work = CANDIDATES * 4 + key_bytes;
        let budget = TestProjectWorkBudget::limited_path_visits(admitted_pre_sort_work);

        let error = super::sort_project_candidates(&mut paths, None, Some(&budget))
            .expect_err("sorting must charge comparison work after storage and keys");
        assert_eq!(error, "test work budget exceeded");
        assert_eq!(budget.path_visits.get(), admitted_pre_sort_work);
    }

    #[test]
    fn candidate_membership_cancellation_after_enumeration_is_not_published() {
        const PROJECTS: usize = MAX_OWNERSHIP_CANDIDATES;
        const TOMBSTONES: usize = 512;
        let temp = tempfile::tempdir().expect("temporary directory");
        let root = temp.path();
        let source = root.join("Consumer.pas");
        fs::write(&source, "unit Consumer; interface implementation end.").expect("source file");
        for index in 0..PROJECTS {
            fs::write(root.join(format!("Project{index:03}.dproj")), b"<Project/>")
                .expect("project candidate");
        }
        let deleted_paths = (0..TOMBSTONES)
            .map(|index| root.join(format!("Deleted{index:03}.dproj")))
            .collect::<Vec<_>>();
        let enumeration_visits = 1 + PROJECTS + 1 + 1; // probe, entries, terminal advance
        let budget = TestProjectWorkBudget::cancel_after_path_visits(enumeration_visits + 20);
        let mut tracker = ProjectReadTracker::with_budget(Some(&budget), &[]);

        let error = super::record_candidate_memberships(
            &source,
            Some(root),
            &mut tracker,
            None,
            &deleted_paths,
        )
        .expect_err("cancelled candidate membership must not be returned as successful");
        assert_eq!(error, "request cancelled");
        assert!(
            tracker.candidate_memberships.is_empty(),
            "no partial/stale candidate membership may be published"
        );
        assert!(budget.path_visits.get() > enumeration_visits);
    }

    #[test]
    fn candidate_membership_budget_exhaustion_after_enumeration_is_not_published() {
        const PROJECTS: usize = MAX_OWNERSHIP_CANDIDATES;
        const TOMBSTONES: usize = 512;
        let temp = tempfile::tempdir().expect("temporary directory");
        let root = temp.path();
        let source = root.join("Consumer.pas");
        fs::write(&source, "unit Consumer; interface implementation end.").expect("source file");
        for index in 0..PROJECTS {
            fs::write(root.join(format!("Project{index:03}.dproj")), b"<Project/>")
                .expect("project candidate");
        }
        let deleted_paths = (0..TOMBSTONES)
            .map(|index| root.join(format!("Deleted{index:03}.dproj")))
            .collect::<Vec<_>>();
        let enumeration_visits = 1 + PROJECTS + 1 + 1; // probe, entries, terminal advance
        let budget = TestProjectWorkBudget::limited_path_visits(enumeration_visits + 20);
        let mut tracker = ProjectReadTracker::with_budget(Some(&budget), &[]);

        let error = super::record_candidate_memberships(
            &source,
            Some(root),
            &mut tracker,
            None,
            &deleted_paths,
        )
        .expect_err("transient post-enumeration exhaustion must fail the membership refresh");
        assert_eq!(error, "test work budget exceeded");
        assert!(
            tracker.candidate_memberships.is_empty(),
            "no partial/stale candidate membership may be published"
        );
        assert!(budget.path_visits.get() > enumeration_visits);
        assert!(budget.path_visits.get() <= enumeration_visits + 20);
    }

    #[test]
    fn candidate_selection_retains_payload_observations_for_main_and_recursive_members() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path();
        let main = root.join("A.dpr");
        let member = root.join("Child.pas");
        fs::write(&main, "program A; uses Child in 'Child.pas'; begin end.\n")
            .expect("main source");
        fs::write(&member, "unit Child; interface implementation end.\n")
            .expect("recursive member");
        fs::write(root.join("B.dpr"), "program B; begin end.\n").expect("other main source");
        fs::write(
            root.join("A.dproj"),
            "<Project><PropertyGroup><MainSource>A.dpr</MainSource></PropertyGroup></Project>",
        )
        .expect("owning project");
        fs::write(
            root.join("B.dproj"),
            "<Project><PropertyGroup><MainSource>B.dpr</MainSource></PropertyGroup></Project>",
        )
        .expect("competing project");

        let context =
            ProjectContext::discover(&member, &[root.to_path_buf()], &ProjectOptions::default())
                .expect("discover project context");

        assert!(context.project_file.is_none(), "{context:?}");
        assert!(!context.discovery_complete);
        for path in [&main, &member] {
            assert!(
                context.metadata_observations.iter().any(|observation| {
                    let MetadataObservation::Payload {
                        path: observed,
                        path_entry,
                        stamp,
                        content_hash,
                        ..
                    } = observation
                    else {
                        return false;
                    };
                    observed == path
                        && path_entry.path == *path
                        && stamp == &path_stamp_result(path).expect("payload path stamp")
                        && *content_hash
                            == content_hash_bytes(&fs::read(path).expect("payload bytes"))
                }),
                "evaluated payload {path:?} was downgraded to stat-only: {context:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn ownership_probe_charges_bytes_read_after_a_stale_stat() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let target_index = 8;
        let initial_size = (super::MAX_OWNERSHIP_SOURCE_BYTES / 9).saturating_sub(1) as usize;
        let total_initial = initial_size * 9;
        let growth = (super::MAX_OWNERSHIP_SOURCE_BYTES as usize - total_initial) + 1;
        let mut paths = Vec::new();

        for index in 0..9 {
            let path = root.join(format!("Unit{index}.pas"));
            let next = if index < target_index {
                format!("contains U{} in 'Unit{}.pas';", index + 1, index + 1)
            } else {
                String::new()
            };
            let mut source =
                format!("unit U{index}; interface {next} implementation end.\n").into_bytes();
            source.resize(initial_size, b' ');
            fs::write(&path, source).expect("membership source");
            paths.push(path);
        }

        let read_policy = ReadPolicy::new(
            std::slice::from_ref(&root),
            &[],
            &[],
            &EffectiveOverrides::default(),
        );
        let context = ProjectContext {
            main_source_entry: Some(ProjectPathEntry {
                path: paths[0].clone(),
                provenance: ProjectPathProvenance::Configured,
            }),
            read_policy,
            ..ProjectContext::default()
        };
        let mut budget = super::OwnershipProbeBudget::default();
        let target = paths[target_index].clone();
        let mut grew = false;
        let inspection =
            super::inspect_source_membership_with_hook(&context, &mut budget, &mut |path| {
                if path == target && !grew {
                    let mut file = fs::OpenOptions::new()
                        .append(true)
                        .open(path)
                        .expect("target source");
                    file.write_all(&vec![b'x'; growth])
                        .expect("grow target source");
                    grew = true;
                }
            });

        assert!(grew, "the stale-stat growth hook must run");
        assert!(
            !inspection.complete,
            "source membership must become incomplete when actual bytes exceed the aggregate budget"
        );
        assert!(
            inspection.warnings.iter().any(|warning| {
                warning.contains("probe limit reached") || warning.contains("exceeds")
            }),
            "expected bounded growth warning: {:?}",
            inspection.warnings
        );
        assert!(
            budget.source_bytes <= MAX_OWNERSHIP_SOURCE_BYTES,
            "stale-stat growth must not make the aggregate byte reservation exceed its cap"
        );
        assert!(
            budget.source_files <= MAX_OWNERSHIP_SOURCE_FILES,
            "stale-stat growth must not make the aggregate file reservation exceed its cap"
        );
        assert!(
            !inspection
                .metadata_observations
                .iter()
                .any(|observation| observation.path() == target),
            "a source that grew after its reservation must not be accepted as a payload"
        );
    }

    #[test]
    fn ownership_probe_caps_invalid_utf8_attempts_before_opening() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let main = root.join("Main.pas");
        fs::write(&main, "unit Main; interface implementation end.\n").expect("main source");

        let mut explicit_unit_entries = std::collections::HashMap::new();
        for index in 0..(MAX_OWNERSHIP_SOURCE_FILES + 32) {
            let path = root.join(format!("Invalid{index}.pas"));
            fs::write(&path, [0xff]).expect("invalid UTF-8 source");
            explicit_unit_entries.insert(
                format!("Invalid{index}"),
                vec![ProjectPathEntry {
                    path,
                    provenance: ProjectPathProvenance::Configured,
                }],
            );
        }

        let read_policy = ReadPolicy::new(
            std::slice::from_ref(&root),
            &[],
            &[],
            &EffectiveOverrides::default(),
        );
        let context = ProjectContext {
            main_source_entry: Some(ProjectPathEntry {
                path: main,
                provenance: ProjectPathProvenance::Configured,
            }),
            explicit_unit_entries,
            read_policy,
            ..ProjectContext::default()
        };
        let mut budget = super::OwnershipProbeBudget::default();
        let mut read_attempts = 0;
        let inspection =
            super::inspect_source_membership_with_hook(&context, &mut budget, &mut |_| {
                read_attempts += 1
            });

        assert!(
            !inspection.complete,
            "invalid UTF-8 must make probing incomplete"
        );
        assert!(
            budget.exhausted,
            "failed payload reads must consume the ownership budget"
        );
        assert!(
            read_attempts <= MAX_OWNERSHIP_SOURCE_FILES,
            "invalid UTF-8 sources must be capped before payload opening"
        );
        assert!(
            budget.source_files <= MAX_OWNERSHIP_SOURCE_FILES,
            "failed payload reads must not exceed the file-attempt cap"
        );
        assert!(
            budget.source_bytes <= MAX_OWNERSHIP_SOURCE_BYTES,
            "failed payload reads must not exceed the byte cap"
        );
    }
}
