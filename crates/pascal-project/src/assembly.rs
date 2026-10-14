use super::authorization::{
    ProvenanceRange, extension_is, is_compiled_reference, lexical_normalize, missing_path_warning,
    project_path_candidate, resolve_existing_path, resolve_existing_path_status_with_provenance,
    resolve_project_path_entry, search_path_missing_warning,
};
#[cfg(any(test, feature = "test-support"))]
use super::discovery_cache::test_record_project_discovery;
use super::discovery_cache::{
    check_project_scan_cancel, project_directory_entries, record_project_discovery_cache_build,
};
use super::msbuild_eval::{
    ConditionEnvironment, ExpandedValue, TruthValue, XmlOperation, condition_matches, expand_value,
    parse_xml_operations,
};
use super::packages::parse_explicit_unit_paths;
use super::selection::{is_deleted_path, relevant_workspace_root, standalone_overrides};
use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn build_standalone_context_with_overrides(
    file: &Path,
    roots: &[PathBuf],
    options: &ProjectOptions,
    mut warnings: Vec<String>,
    mut discovery_complete: bool,
    metadata_files: Vec<PathBuf>,
    metadata_observations: Vec<MetadataObservation>,
    session: &OverrideSession,
    exclusions: &[String],
) -> Result<ProjectContext, String> {
    let (mut effective_overrides, override_error) = match standalone_overrides(file, roots, session)
    {
        Ok(overrides) => (overrides, None),
        Err(error) => {
            warnings.push(error.clone());
            discovery_complete = false;
            (EffectiveOverrides::default(), Some(error))
        }
    };
    let mut options = options.clone();
    if override_error.is_none()
        && let Some(installation) = containing_installation(file, roots, session)
    {
        effective_overrides = merge_effective_overrides(
            installation.overrides,
            &effective_overrides,
            relevant_override_workspace_root(file, roots).as_deref(),
            file.parent().unwrap_or(file),
        );
        if options.conditional_context.compiler_version.is_none() {
            options.conditional_context.compiler_version =
                crate::installations::compiler_version_for_installation(&installation.id);
        }
    }
    let mut context = build_standalone_context(
        file,
        roots,
        &options,
        effective_overrides,
        warnings,
        discovery_complete,
        metadata_files,
        metadata_observations,
        exclusions,
        None,
    )?;
    context.override_error = override_error;
    Ok(context)
}

/// A source inside exactly one configured installation's `BDS` tree (its RTL
/// and VCL sources) is compiled by that installation, so a standalone context
/// for it takes the installation's compiler and profile properties such as
/// `Platform`.
fn containing_installation(
    file: &Path,
    roots: &[PathBuf],
    session: &OverrideSession,
) -> Option<crate::installation_config::ResolvedInstallation> {
    let workspace_root = relevant_override_workspace_root(file, roots);
    let configuration = session
        .configuration_for(workspace_root.as_deref(), None)
        .ok()?;
    let mut containing = configuration
        .installation_ids()
        .into_iter()
        .filter_map(|id| {
            let profile = configuration.profile(&id).ok()?;
            let bds = profile.overrides.properties.get("bds")?;
            let bds = profile.overrides.resolve_path(bds, Path::new("/")).ok()?;
            path_starts_with_ci(file, &bds.path).then_some(profile)
        });
    let installation = containing.next()?;
    containing.next().is_none().then_some(installation)
}

pub(super) fn effective_overrides_for_project(
    project_file: &Path,
    roots: &[PathBuf],
    session: &OverrideSession,
) -> Result<EffectiveOverrides, String> {
    let project_directory = project_file.parent();
    let workspace_root =
        project_directory.and_then(|directory| relevant_override_workspace_root(directory, roots));
    session.effective_for(workspace_root.as_deref(), project_directory)
}

fn merge_effective_overrides(
    mut profile: EffectiveOverrides,
    project: &EffectiveOverrides,
    workspace_root: Option<&Path>,
    project_dir: &Path,
) -> EffectiveOverrides {
    let origin_rank = |path: &Path| {
        if project_paths_equal(path, &project_dir.join(LOCAL_CONFIG_NAME)) {
            2
        } else if workspace_root
            .is_some_and(|root| project_paths_equal(path, &root.join(LOCAL_CONFIG_NAME)))
        {
            1
        } else {
            0
        }
    };
    for (name, value) in &project.properties {
        let project_origin = project.property_origins.get(name);
        let profile_origin = profile.property_origins.get(name);
        let project_is_more_local = profile_origin.is_none_or(|profile_origin| {
            project_origin.is_some_and(|project_origin| {
                origin_rank(project_origin) > origin_rank(profile_origin)
            })
        });
        if project_is_more_local {
            profile.properties.insert(name.clone(), value.clone());
            if let Some(origin) = project_origin {
                profile
                    .property_origins
                    .insert(name.clone(), origin.clone());
            }
        }
    }

    let mut path_mappings = profile
        .path_mappings
        .into_iter()
        .map(|mapping| (mapping.from.clone(), mapping))
        .collect::<BTreeMap<_, _>>();
    for mapping in &project.path_mappings {
        let project_is_more_local = path_mappings.get(&mapping.from).is_none_or(|profile| {
            origin_rank(&mapping.config_file) > origin_rank(&profile.config_file)
        });
        if project_is_more_local {
            path_mappings.insert(mapping.from.clone(), mapping.clone());
        }
    }
    profile.path_mappings = path_mappings.into_values().collect();
    profile
}

pub(super) fn relevant_override_workspace_root(file: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    roots
        .iter()
        .filter(|root| file.starts_with(root))
        .max_by_key(|root| root.components().count())
        .cloned()
}

pub(super) fn path_starts_with_ci(path: &Path, root: &Path) -> bool {
    let mut path_components = path.components();
    for root_component in root.components() {
        let Some(path_component) = path_components.next() else {
            return false;
        };
        if !path_component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&root_component.as_os_str().to_string_lossy())
        {
            return false;
        }
    }
    true
}

fn fallback_main_source(
    project_file: &Path,
    warnings: &mut Vec<String>,
    cancel: Option<&AtomicBool>,
    work_budget: Option<&dyn ProjectWorkBudget>,
    deleted_paths: &[PathBuf],
) -> Result<Option<PathBuf>, String> {
    let (Some(stem), Some(directory)) = (project_file.file_stem(), project_file.parent()) else {
        return Ok(None);
    };
    let entries = match project_directory_entries(directory, cancel, work_budget) {
        Ok(entries) => entries,
        Err(error) => {
            if let Some(work_budget) = work_budget {
                work_budget.check_cancelled()?;
            }
            if error == "request cancelled" {
                return Err(error);
            }
            warnings.push(format!(
                "could not inspect {} for its main source: {error}",
                directory.display()
            ));
            return Ok(None);
        }
    };
    if entries.candidate_overflow {
        return Err(format!(
            "project candidate membership limit ({MAX_OWNERSHIP_CANDIDATES}) reached in {}",
            directory.display()
        ));
    }
    let candidates = entries
        .dpr_or_dpk
        .into_iter()
        .filter(|path| {
            !is_deleted_path(path, deleted_paths)
                && path.file_stem().is_some_and(|candidate| {
                    candidate
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&stem.to_string_lossy())
                })
        })
        .collect::<Vec<_>>();
    match candidates.len() {
        0 => Ok(None),
        1 => Ok(candidates.into_iter().next()),
        _ => {
            warnings.push(format!(
                "multiple main sources match {}; no main source selected",
                project_file.display()
            ));
            Ok(None)
        }
    }
}

// Context construction keeps the selected project, requesting file, roots,
// evaluator options, and immutable effective settings explicit for callers.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_project_context(
    project_file: PathBuf,
    file: &Path,
    roots: &[PathBuf],
    options: &ProjectOptions,
    installation_config: crate::installation_config::ProjectConfiguration,
    mut overrides: EffectiveOverrides,
    warnings: Vec<String>,
    explicit: bool,
    consulted_metadata_files: Vec<PathBuf>,
    consulted_metadata_observations: Vec<MetadataObservation>,
    exclusions: &[String],
    tracker: &mut ProjectReadTracker<'_>,
    cancel: Option<&AtomicBool>,
) -> Result<ProjectContext, String> {
    check_project_scan_cancel(cancel)?;
    #[cfg(any(test, feature = "test-support"))]
    test_record_project_discovery(true);
    record_project_discovery_cache_build();
    let project_dir = project_file
        .parent()
        .map_or_else(PathBuf::new, Path::to_path_buf);
    let mut read_policy = ReadPolicy::new(roots, &options.source_paths, exclusions, &overrides);
    let mut evaluation_options = options.clone();
    let session_build_choice = options
        .build_selections
        .iter()
        .find(|(path, _)| project_paths_equal(path, &project_file))
        .map(|(_, choice)| choice);
    let configured_build_choice = installation_config.configured_build_for(&project_file);
    let config_selected_in_session = session_build_choice
        .and_then(|choice| choice.config.as_ref())
        .is_some();
    let platform_selected_in_session = session_build_choice
        .and_then(|choice| choice.platform.as_ref())
        .is_some();
    let config_selected_in_project_config =
        !config_selected_in_session && configured_build_choice.config.is_some();
    let platform_selected_in_project_config =
        !platform_selected_in_session && configured_build_choice.platform.is_some();
    if let Some(config) = session_build_choice.and_then(|choice| choice.config.as_ref()) {
        evaluation_options.build_config = Some(config.clone());
    } else if let Some(config) = configured_build_choice.config.as_ref() {
        evaluation_options.build_config = Some(config.clone());
    }
    if let Some(platform) = session_build_choice.and_then(|choice| choice.platform.as_ref()) {
        evaluation_options.platform = Some(platform.clone());
    } else if let Some(platform) = configured_build_choice.platform.as_ref() {
        evaluation_options.platform = Some(platform.clone());
    }
    let caller_compiler_version = options.conditional_context.compiler_version;
    let mut bootstrap_config = None;
    let mut bootstrap_platform = None;
    let mut build_candidates = BuildCandidates::default();
    let mut config_is_configured =
        options.build_config.is_some() || overrides.properties.contains_key("config");
    let mut platform_is_configured =
        options.platform.is_some() || overrides.properties.contains_key("platform");
    let mut installation_evidence = InstallationEvidence::default();
    let mut compiler_identity_conflicting = false;
    let mut other_identity_conflicting = false;
    if extension_is(&project_file, "dproj") {
        let bootstrap_entry = ProjectPathEntry::legacy(project_file.clone());
        let (bootstrap_payload, observation) = read_policy
            .read_payload_with_observation(&bootstrap_entry, MAX_PROJECT_BYTES)
            .map_err(|error| format!("could not read project bootstrap metadata: {error}"))?;
        tracker.record_metadata_path(project_file.clone());
        tracker.record_metadata_observation(observation);
        build_candidates = build_selection::parse_build_candidates(&bootstrap_payload);
        let bootstrap = parse_project_bootstrap(
            &bootstrap_payload,
            &project_file,
            &evaluation_options,
            &overrides,
            &read_policy,
        );
        for path in &bootstrap.metadata_files {
            tracker.record_metadata_path(path.clone());
        }
        installation_evidence = bootstrap.evidence;
        compiler_identity_conflicting = bootstrap.compiler_conflicting;
        other_identity_conflicting = bootstrap.other_conflicting;
        bootstrap_config = bootstrap.config;
        bootstrap_platform = bootstrap.platform;
        if evaluation_options
            .conditional_context
            .compiler_version
            .is_none()
        {
            evaluation_options.conditional_context.compiler_version =
                installation_evidence.compiler_version;
        }
    }
    let mut selection_evidence = installation_evidence.clone();
    if let Some(compiler_version) = caller_compiler_version {
        selection_evidence.compiler_version = Some(compiler_version);
        if compiler_identity_conflicting {
            selection_evidence.conflicting = other_identity_conflicting;
        }
    } else if let Some(compiler_version) = installation_evidence.compiler_version {
        selection_evidence.compiler_version = Some(compiler_version);
    }
    let installation_selection = Some(select_installation(
        &installation_config,
        &project_file,
        &selection_evidence,
        evaluation_options
            .installation_selections
            .iter()
            .find(|(path, _)| project_paths_equal(path, &project_file))
            .map(|(_, id)| id.as_str()),
    ));
    let mut installation_environment = None;
    let mut installation_config_files = Vec::new();
    let mut rtl_constants_override = None;
    let mut ide_paths = IdePaths::default();
    let mut installation_warnings = Vec::new();
    let mut ide_path_warnings = Vec::new();
    if let Some(InstallationSelection::Selected { id, .. }) = &installation_selection {
        match installation_config.profile(id) {
            Ok(profile) => {
                rtl_constants_override = profile.rtl_version_constants.clone();
                let effective_profile = crate::installation_config::ResolvedInstallation {
                    id: profile.id.clone(),
                    rtl_version_constants: profile.rtl_version_constants.clone(),
                    overrides: merge_effective_overrides(
                        profile.overrides,
                        &overrides,
                        relevant_override_workspace_root(&project_dir, roots).as_deref(),
                        &project_dir,
                    ),
                };
                overrides = effective_profile.overrides.clone();
                config_is_configured |= overrides.properties.contains_key("config");
                platform_is_configured |= overrides.properties.contains_key("platform");
                if evaluation_options.build_config.is_none() {
                    evaluation_options.build_config = effective_profile
                        .overrides
                        .properties
                        .get("config")
                        .cloned()
                        .or_else(|| bootstrap_config.clone());
                }
                if evaluation_options.platform.is_none() {
                    evaluation_options.platform = effective_profile
                        .overrides
                        .properties
                        .get("platform")
                        .cloned()
                        .or_else(|| bootstrap_platform.clone())
                        .or_else(|| build_candidates.default_platform.clone());
                }
                let installation_roots = ["bds", "appdata", "bdslib", "bdscommondir"]
                    .into_iter()
                    .filter_map(|name| effective_profile.overrides.properties.get(name))
                    .map(PathBuf::from)
                    .collect::<Vec<_>>();
                let explicit_locator_files = ["environmentsettings", "envoptions"]
                    .into_iter()
                    .filter_map(|name| effective_profile.overrides.properties.get(name))
                    .filter_map(|value| {
                        effective_profile
                            .overrides
                            .resolve_path(value, Path::new("/"))
                            .ok()
                            .map(|resolved| resolved.path)
                    })
                    .collect::<Vec<_>>();
                read_policy = ReadPolicy::new_with_installation_roots(
                    roots,
                    &options.source_paths,
                    exclusions,
                    &overrides,
                    &installation_roots,
                )
                .with_authorized_files(&explicit_locator_files);
                match load_installation(
                    &effective_profile,
                    evaluation_options
                        .build_config
                        .as_deref()
                        .unwrap_or("Debug"),
                    evaluation_options.platform.as_deref().unwrap_or(""),
                    tracker,
                    &read_policy,
                ) {
                    Ok(environment) => {
                        installation_config_files = environment.installation_config_files.clone();
                        if let (Some(config), Some(platform)) = (
                            evaluation_options.build_config.as_deref(),
                            evaluation_options.platform.as_deref(),
                        ) {
                            match evaluate_ide_paths(
                                &environment,
                                &effective_profile,
                                config,
                                platform,
                                tracker,
                                &read_policy,
                            ) {
                                Ok(paths) => {
                                    ide_path_warnings = paths.warnings.clone();
                                    ide_paths = paths;
                                }
                                Err(error) => installation_warnings.push(error),
                            }
                        } else {
                            // IDE paths are platform-conditioned; never union unknown platforms.
                        }
                        installation_environment = Some(environment);
                    }
                    Err(error) => {
                        installation_warnings.push(error);
                    }
                }
                if evaluation_options
                    .conditional_context
                    .compiler_version
                    .is_none()
                    && let Some(version) =
                        crate::installations::compiler_version_for_installation(id)
                {
                    evaluation_options.conditional_context.compiler_version = Some(version);
                }
            }
            Err(error) => {
                installation_warnings.push(error);
            }
        }
    }
    if evaluation_options.build_config.is_none() {
        evaluation_options.build_config = bootstrap_config;
    }
    if evaluation_options.platform.is_none() {
        evaluation_options.platform =
            bootstrap_platform.or_else(|| build_candidates.default_platform.clone());
    }
    let mut builder = ProjectBuilder::new(
        &evaluation_options,
        &overrides,
        warnings,
        project_dir.clone(),
        read_policy.clone(),
    );
    if let Some(environment) = &installation_environment {
        builder.seed_installation_properties(&environment.properties);
        builder.warnings.extend(environment.warnings.clone());
        builder
            .metadata_files
            .extend(environment.metadata_files.clone());
        builder
            .metadata_observations
            .extend(environment.metadata_observations.clone());
        builder.path_issues.extend(environment.path_issues.clone());
    }
    builder.warnings.extend(installation_warnings);
    builder.warnings.extend(ide_path_warnings.iter().cloned());
    let project_is_dproj = extension_is(&project_file, "dproj");
    if project_is_dproj {
        builder.process_root_dproj(&project_file, tracker)?;
        check_project_scan_cancel(cancel)?;
    }

    let main_source_entry = if project_is_dproj {
        let main_name = builder.property("mainsource");
        match main_name {
            Some(name) if !name.is_empty() && !name.contains(UNRESOLVED_MARKER) => {
                let configured = builder.property_is_configured("mainsource");
                let provenance = builder.property_provenance("mainsource");
                resolve_project_path_entry(
                    &name,
                    &project_dir,
                    &builder.overrides,
                    &mut builder.warnings,
                    "main source",
                    true,
                    configured,
                )
                .map(|entry| inherit_path_provenance(entry, &provenance))
            }
            Some(_) => {
                builder.warnings.push(format!(
                    "main source contains an unavailable property path in {}",
                    project_file.display()
                ));
                None
            }
            None => fallback_main_source(
                &project_file,
                &mut builder.warnings,
                cancel,
                tracker.work_budget,
                tracker.deleted_paths,
            )?
            .map(ProjectPathEntry::legacy),
        }
    } else {
        Some(ProjectPathEntry::legacy(project_file.clone()))
    };
    let main_source = main_source_entry.as_ref().map(|entry| entry.path.clone());
    if project_is_dproj && main_source.is_none() {
        let main_source_raw = builder.property("mainsource").unwrap_or_default();
        builder.path_issues.push(ProjectPathIssue {
            kind: ProjectPathIssueKind::MissingMainSource,
            source_file: project_file.clone(),
            property: "MainSource".to_owned(),
            raw: main_source_raw,
            path: None,
            unit_name: None,
            provenance: ProjectPathProvenance::LegacyNative,
        });
        builder.warnings.push(format!(
            "project has no resolvable MainSource: {}",
            project_file.display()
        ));
    }

    if let Some(main_source_entry) = &main_source_entry
        && !builder.read_policy.allows_entry(main_source_entry)
    {
        builder.incomplete = true;
        builder.warnings.push(format!(
            "ignored main source outside authorized read roots: {}",
            main_source_entry.path.display()
        ));
    }

    let mut search_path_entries = vec![ProjectPathEntry::legacy(project_dir.clone())];
    let option_base = relevant_workspace_root(file, roots)
        .or_else(|| file.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| project_dir.clone());
    for (item, provenance) in builder.property_list_with_provenance("dcc_unitsearchpath") {
        check_project_scan_cancel(cancel)?;
        add_resolved_search_path_entry(
            &item,
            &project_dir,
            &builder.overrides,
            &mut search_path_entries,
            &mut builder.warnings,
            "DCC_UnitSearchPath",
            provenance,
        );
    }
    for item in &options.source_paths {
        check_project_scan_cancel(cancel)?;
        add_resolved_search_path_entry(
            item,
            &option_base,
            &builder.overrides,
            &mut search_path_entries,
            &mut builder.warnings,
            "configured source path",
            ProjectPathProvenance::Configured,
        );
    }
    let mut project_source_roots = paths_from_entries(&search_path_entries);
    for path in &ide_paths.compiled_library {
        if !project_source_roots
            .iter()
            .any(|root| project_paths_equal(root, path))
        {
            project_source_roots.push(path.clone());
        }
    }
    for entry in &ide_paths.library {
        if !search_path_entries
            .iter()
            .any(|existing| project_paths_equal(&existing.path, &entry.path))
        {
            search_path_entries.push(entry.clone());
        }
    }
    let search_paths = paths_from_entries(&search_path_entries);

    let mut include_path_entries = Vec::new();
    for (item, provenance) in builder.property_list_with_provenance("dcc_includepath") {
        check_project_scan_cancel(cancel)?;
        add_resolved_search_path_entry(
            &item,
            &project_dir,
            &builder.overrides,
            &mut include_path_entries,
            &mut builder.warnings,
            "DCC_IncludePath",
            provenance,
        );
    }
    let include_paths = paths_from_entries(&include_path_entries);

    let mut explicit_units = HashMap::new();
    let mut explicit_unit_entries = HashMap::new();
    if let Some(main_source) = &main_source_entry
        && let Some(observation) = add_explicit_units_from_source(
            main_source,
            &mut explicit_units,
            &mut explicit_unit_entries,
            &mut builder.warnings,
            &builder.overrides,
            &builder.read_policy,
            tracker,
        )
    {
        add_metadata_observation(&mut builder.metadata_observations, observation);
    }
    for reference in &builder.references {
        check_project_scan_cancel(cancel)?;
        if is_compiled_reference(&reference.include) {
            continue;
        }
        let expanded = expand_value(
            &reference.include,
            "",
            &builder.properties,
            &builder.configured_ranges,
            &builder.configured_properties,
            &builder.property_provenance_ranges,
            &builder.property_default_provenances,
            &builder.unknown_properties,
            &mut builder.warnings,
            &reference.source_file,
            &reference.source_provenance,
        );
        if expanded.unknown || expanded.value.contains(UNRESOLVED_MARKER) {
            continue;
        }
        if is_compiled_reference(&expanded.value) {
            continue;
        }
        let provenance = expanded_path_provenance(&expanded, &reference.source_provenance);
        let Some(resolved) = project_path_candidate(
            &expanded.value,
            &builder.project_dir,
            &builder.overrides,
            &mut builder.warnings,
            "DCCReference",
        ) else {
            continue;
        };
        let candidate = lexical_normalize(&resolved.path);
        let entry = match resolve_existing_path_status_with_provenance(
            &candidate,
            &resolved,
            &expanded.value,
            &mut builder.warnings,
            "DCCReference",
        ) {
            ExistingPathStatus::Found(path) => inherit_path_provenance(
                ProjectPathEntry::resolved(path, &resolved, expanded.explicit_dependency),
                &provenance,
            ),
            ExistingPathStatus::Missing => {
                builder.warnings.push(missing_path_warning(
                    "DCCReference",
                    &expanded.value,
                    &candidate,
                    &resolved,
                ));
                let unit_name = candidate
                    .file_stem()
                    .map(|stem| canonical_unit_name(&stem.to_string_lossy()))
                    .filter(|name| !name.is_empty());
                builder.path_issues.push(ProjectPathIssue {
                    kind: ProjectPathIssueKind::MissingReference,
                    source_file: reference.source_file.clone(),
                    property: "DCCReference".to_string(),
                    raw: reference.include.clone(),
                    path: Some(candidate.clone()),
                    unit_name,
                    provenance: inherit_path_provenance(
                        ProjectPathEntry::resolved(
                            candidate.clone(),
                            &resolved,
                            expanded.explicit_dependency,
                        ),
                        &provenance,
                    )
                    .provenance,
                });
                add_metadata_observation(
                    &mut builder.metadata_observations,
                    MetadataObservation::Stat {
                        path: candidate.clone(),
                    },
                );
                continue;
            }
            ExistingPathStatus::Unresolvable => continue,
        };
        let Some(stem) = entry.path.file_stem() else {
            continue;
        };
        let name = canonical_unit_name(&stem.to_string_lossy());
        if !name.is_empty() {
            add_unit_candidate_entry(&mut explicit_units, &mut explicit_unit_entries, name, entry);
        }
    }

    let mut metadata_files = builder.metadata_files.clone();
    if !metadata_files.iter().any(|path| path == &project_file) {
        metadata_files.push(project_file.clone());
    }
    if let Some(main_source) = &main_source
        && !metadata_files.iter().any(|path| path == main_source)
    {
        metadata_files.push(main_source.clone());
    }
    for metadata_file in consulted_metadata_files {
        check_project_scan_cancel(cancel)?;
        if !metadata_files.iter().any(|path| path == &metadata_file) {
            metadata_files.push(metadata_file);
        }
    }
    let project_defines = property_list(&builder, "dcc_define");
    let config = selected_config(&builder, &evaluation_options);
    let platform = selected_platform(&builder, &evaluation_options);
    let console_target = if builder
        .property("dcc_consoletarget")
        .as_deref()
        .and_then(parse_conditional_fact)
        == Some(ConditionalFact::True)
    {
        ConditionalFact::True
    } else if let Some(main_source_entry) = &main_source_entry {
        match builder
            .read_policy
            .read_pascal_payload_with_observation(main_source_entry, MAX_PROJECT_BYTES)
        {
            Ok((source, observation)) => {
                add_metadata_observation(&mut builder.metadata_observations, observation);
                if conditional_closure::source_has_console_apptype(&source) {
                    ConditionalFact::True
                } else {
                    ConditionalFact::False
                }
            }
            Err(error) => {
                builder.warnings.push(format!(
                    "could not read main source to determine CONSOLE in {}: {error}",
                    project_file.display()
                ));
                ConditionalFact::Unknown
            }
        }
    } else {
        ConditionalFact::Unknown
    };
    let mut conditional_warnings = Vec::new();
    let mut conditional_context = merge_project_conditional_context(
        &evaluation_options,
        &builder,
        &project_defines,
        &mut conditional_warnings,
    );
    builder.warnings.extend(conditional_warnings);

    let target_platform = platform.as_deref().and_then(TargetPlatform::parse);
    let predefined = crate::compiler_defines::predefined_defines(
        conditional_context.compiler_version,
        target_platform.as_ref(),
        console_target,
    );
    let version = conditional_context.compiler_version;
    let has_valid_rtl_override = rtl_constants_override.as_deref().is_some_and(|names| {
        names
            .iter()
            .all(|name| ConditionalContext::is_rtl_version_constant(name))
    });
    let system_pas = if !has_valid_rtl_override
        && version.is_some_and(|version| {
            version.cmp_numeric(CompilerVersion::new(34, 0)) != Some(std::cmp::Ordering::Less)
        }) {
        scan_system_pas_from_paths(
            &search_path_entries,
            &ide_paths.browsing,
            &mut builder,
            tracker.work_budget,
            cancel,
            &mut metadata_files,
        )?
    } else {
        None
    };
    let (rtl_source, rtl_constants, rtl_warnings) = crate::rtl_constants::resolve_rtl_constants(
        version,
        rtl_constants_override.as_deref(),
        system_pas,
    );
    builder.warnings.extend(rtl_warnings);

    conditional_closure::merge_predefined_facts(&mut conditional_context, &predefined);
    conditional_closure::merge_rtl_constants(&mut conditional_context, rtl_constants.as_deref());
    conditional_context.absent_define = ConditionalFact::Unknown;

    let mut library_conditional_context = options.conditional_context.clone();
    library_conditional_context.compiler_version = conditional_context.compiler_version;
    conditional_closure::merge_predefined_facts(&mut library_conditional_context, &predefined);
    conditional_closure::merge_rtl_constants(
        &mut library_conditional_context,
        rtl_constants.as_deref(),
    );
    library_conditional_context.absent_define = ConditionalFact::Unknown;

    let metadata_observations = complete_metadata_observations(
        &metadata_files,
        builder
            .metadata_observations
            .clone()
            .into_iter()
            .chain(consulted_metadata_observations)
            .collect(),
    );

    let mut defines = project_defines;
    for (name, value) in &conditional_context.defines {
        if *value == ConditionalFact::True
            && !defines
                .iter()
                .any(|define| define.eq_ignore_ascii_case(name))
        {
            defines.push(name.clone());
        }
    }

    let mut config_selection = build_selection::selection_for_value(
        config.as_deref(),
        &build_candidates.configs,
        build_candidates.default_config.as_deref(),
        config_is_configured,
        false,
    );
    config_selection.mode = if config_selected_in_session {
        BuildSelectionMode::Session
    } else if config_selected_in_project_config || config_is_configured {
        BuildSelectionMode::Configured
    } else {
        BuildSelectionMode::ProjectDefault
    };
    warn_for_unknown_build_selection(
        &mut config_selection,
        config_selected_in_session || config_selected_in_project_config,
        false,
        &project_file,
        &mut builder.warnings,
    );
    let mut platform_selection = build_selection::selection_for_value(
        platform.as_deref(),
        &build_candidates.platforms,
        build_candidates.default_platform.as_deref(),
        platform_is_configured,
        true,
    );
    platform_selection.mode = if platform_selected_in_session {
        BuildSelectionMode::Session
    } else if platform_selected_in_project_config || platform_is_configured {
        BuildSelectionMode::Configured
    } else {
        BuildSelectionMode::ProjectDefault
    };
    warn_for_unknown_build_selection(
        &mut platform_selection,
        platform_selected_in_session || platform_selected_in_project_config,
        true,
        &project_file,
        &mut builder.warnings,
    );

    // Target parsing alone is not build-selection validation: it also accepts
    // undeclared/custom names. Do not change legacy selection/closure semantics.
    let invalid_layout_target = platform_selection.mode == BuildSelectionMode::Invalid
        || platform_selection
            .selected
            .as_ref()
            .is_some_and(|selected| {
                !platform_selection.candidates.is_empty()
                    && !platform_selection
                        .candidates
                        .iter()
                        .any(|candidate| candidate.eq_ignore_ascii_case(selected))
            })
        || builder.unknown_properties.contains("platform");
    let layout_target = (!invalid_layout_target)
        .then_some(target_platform.as_ref())
        .flatten();
    let library_layout = LayoutContext::for_target(version, layout_target);
    let mut project_layout = library_layout.clone();
    apply_project_layout_properties(&mut project_layout.defaults, &builder);
    conditional_context.layout = selected_layout(
        project_layout,
        &options.conditional_context,
        invalid_layout_target,
    );
    library_conditional_context.layout = selected_layout(
        library_layout,
        &options.conditional_context,
        invalid_layout_target,
    );
    conditional_context.layout_explicit = options.conditional_context.layout_explicit;
    library_conditional_context.layout_explicit = options.conditional_context.layout_explicit;

    let discovery_complete = !builder.incomplete
        && ide_path_warnings.is_empty()
        && !project_context_warnings_incomplete(&builder.warnings, explicit);
    let open_reasons = conditional_closure::open_reasons(
        extension_is(&project_file, "dproj"),
        discovery_complete,
        conditional_context.compiler_version.is_some(),
        config.as_deref(),
        platform.as_deref(),
        &config_selection,
        &platform_selection,
    );
    let conditional_closure = ConditionalClosure {
        closed: open_reasons.is_empty(),
        open_reasons,
        rtl_source,
        rtl_constants,
    };

    Ok(ProjectContext {
        discovery_complete,
        binding_metadata_complete: !builder.incomplete
            && !binding_metadata_warnings_incomplete(&builder.warnings, explicit),
        path_issues: builder.path_issues.clone(),
        project_file: Some(project_file.clone()),
        installation_evidence: installation_evidence.clone(),
        installation_selection,
        main_source,
        search_paths,
        project_source_roots,
        search_path_entries,
        browsing_path_entries: ide_paths.browsing,
        debug_dcu_path_entries: ide_paths.debug_dcu,
        main_source_entry,
        explicit_unit_entries,
        include_paths,
        include_path_entries,
        explicit_units,
        unit_namespaces: property_list(&builder, "dcc_namespace")
            .into_iter()
            .chain(ide_paths.namespaces)
            .collect(),
        unit_aliases: parse_aliases(builder.property("dcc_unitalias").as_deref()),
        defines,
        conditional_context,
        conditional_closure,
        library_conditional_context,
        config,
        platform,
        config_selection,
        platform_selection,
        overrides,
        read_policy,
        packages: package_list(&builder),
        metadata_files,
        installation_config_files,
        system_pas_searches: builder.system_pas_searches,
        metadata_observations,
        warnings: builder.warnings,
        override_error: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_standalone_context(
    file: &Path,
    roots: &[PathBuf],
    options: &ProjectOptions,
    overrides: EffectiveOverrides,
    mut warnings: Vec<String>,
    discovery_complete: bool,
    metadata_files: Vec<PathBuf>,
    metadata_observations: Vec<MetadataObservation>,
    exclusions: &[String],
    cancel: Option<&AtomicBool>,
) -> Result<ProjectContext, String> {
    check_project_scan_cancel(cancel)?;
    let read_policy = ReadPolicy::new(roots, &options.source_paths, exclusions, &overrides);
    let mut search_path_entries = Vec::new();
    if let Some(parent) = file.parent()
        && let Some(actual) = resolve_existing_path(parent, &mut warnings, "source directory")
    {
        add_unique_project_path_entry(&mut search_path_entries, ProjectPathEntry::legacy(actual));
    }
    if let Some(root) = relevant_workspace_root(file, roots)
        && root.is_dir()
    {
        add_unique_project_path_entry(&mut search_path_entries, ProjectPathEntry::legacy(root));
    }
    let option_base = relevant_workspace_root(file, roots)
        .or_else(|| file.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    for item in &options.source_paths {
        check_project_scan_cancel(cancel)?;
        add_resolved_search_path_entry(
            item,
            &option_base,
            &overrides,
            &mut search_path_entries,
            &mut warnings,
            "configured source path",
            ProjectPathProvenance::Configured,
        );
    }
    let search_paths = paths_from_entries(&search_path_entries);
    let project_source_roots = paths_from_entries(&search_path_entries);
    let config = selected_standalone_property(&overrides, options.build_config.as_ref(), "config");
    let platform = selected_standalone_property(&overrides, options.platform.as_ref(), "platform");
    // A standalone source is not necessarily a program's main source, so it
    // cannot establish the project-level CONSOLE target.
    let console_target = ConditionalFact::Unknown;
    let mut conditional_context = options.conditional_context.clone();
    let target_platform = platform.as_deref().and_then(TargetPlatform::parse);
    conditional_context.layout = selected_layout(
        LayoutContext::for_target(
            conditional_context.compiler_version,
            target_platform.as_ref(),
        ),
        &options.conditional_context,
        false,
    );
    let predefined = crate::compiler_defines::predefined_defines(
        conditional_context.compiler_version,
        target_platform.as_ref(),
        console_target,
    );
    let (rtl_source, rtl_constants, rtl_warnings) = crate::rtl_constants::resolve_rtl_constants(
        conditional_context.compiler_version,
        None,
        None,
    );
    warnings.extend(rtl_warnings);
    conditional_closure::merge_predefined_facts(&mut conditional_context, &predefined);
    conditional_closure::merge_rtl_constants(&mut conditional_context, rtl_constants.as_deref());
    conditional_context.absent_define = ConditionalFact::Unknown;
    let library_conditional_context = conditional_context.clone();
    let defines = conditional_context
        .defines
        .iter()
        .filter(|(_, value)| **value == ConditionalFact::True)
        .map(|(name, _)| name.clone())
        .collect();
    let config_selection = BuildSelection::default();
    let platform_selection = BuildSelection::default();
    let open_reasons = conditional_closure::open_reasons(
        false,
        discovery_complete,
        conditional_context.compiler_version.is_some(),
        config.as_deref(),
        platform.as_deref(),
        &config_selection,
        &platform_selection,
    );
    let conditional_closure = ConditionalClosure {
        closed: false,
        open_reasons,
        rtl_source,
        rtl_constants,
    };
    let metadata_observations =
        complete_metadata_observations(&metadata_files, metadata_observations);

    Ok(ProjectContext {
        discovery_complete,
        binding_metadata_complete: discovery_complete,
        path_issues: Vec::new(),
        project_file: None,
        installation_evidence: InstallationEvidence::default(),
        installation_selection: None,
        main_source: None,
        search_paths,
        project_source_roots,
        search_path_entries,
        browsing_path_entries: Vec::new(),
        debug_dcu_path_entries: Vec::new(),
        main_source_entry: None,
        explicit_unit_entries: HashMap::new(),
        include_paths: Vec::new(),
        include_path_entries: Vec::new(),
        explicit_units: HashMap::new(),
        unit_namespaces: Vec::new(),
        unit_aliases: HashMap::new(),
        defines,
        conditional_context,
        conditional_closure,
        library_conditional_context,
        config,
        platform,
        config_selection,
        platform_selection,
        overrides,
        read_policy,
        packages: Vec::new(),
        metadata_files,
        installation_config_files: Vec::new(),
        system_pas_searches: Vec::new(),
        metadata_observations,
        warnings,
        override_error: None,
    })
}

fn project_context_warnings_incomplete(warnings: &[String], explicit: bool) -> bool {
    warnings.iter().any(|warning| {
        let warning = warning.to_ascii_lowercase();
        (!explicit && warning.contains("no resolvable"))
            || warning.contains("unresolved property")
            || warning.contains("path does not exist and was omitted")
            || warning.contains("could not read optset")
            || warning.contains("could not read main source")
            || warning.contains("metadata file limit")
            || warning.contains("property expansion exceeds")
            || warning.contains("could not inspect project")
            || warning.contains("could not inspect")
            || warning.contains("invalid xml")
            || warning.contains("windows path")
            || warning.contains("unavailable property path")
            || warning.contains("unknown project condition")
            || warning.contains("unsupported project condition")
            || warning.contains("ambiguous case-insensitive")
            || warning.contains("multiple main sources")
            || warning.contains("property memory budget exceeded")
            || warning.contains("optset import limit")
            || warning.contains("optset import path does not exist")
            || warning.contains("workspace root could not be resolved")
    })
}

fn binding_metadata_warnings_incomplete(warnings: &[String], explicit: bool) -> bool {
    warnings.iter().any(|warning| {
        let warning = warning.to_ascii_lowercase();
        !(warning.starts_with("dccreference path does not exist and was omitted:"))
            && warning_incomplete(&warning, explicit)
    })
}

fn warning_incomplete(warning: &str, explicit: bool) -> bool {
    (!explicit && warning.contains("no resolvable"))
        || warning.contains("unresolved property")
        || warning.contains("path does not exist and was omitted")
        || warning.contains("could not read optset")
        || warning.contains("could not read main source")
        || warning.contains("metadata file limit")
        || warning.contains("property expansion exceeds")
        || warning.contains("could not inspect project")
        || warning.contains("could not inspect")
        || warning.contains("invalid xml")
        || warning.contains("windows path")
        || warning.contains("unavailable property path")
        || warning.contains("unknown project condition")
        || warning.contains("unsupported project condition")
        || warning.contains("ambiguous case-insensitive")
        || warning.contains("multiple main sources")
        || warning.contains("property memory budget exceeded")
        || warning.contains("optset import limit")
        || warning.contains("optset import path does not exist")
        || warning.contains("workspace root could not be resolved")
}

fn selected_config(builder: &ProjectBuilder, options: &ProjectOptions) -> Option<String> {
    builder
        .property("config")
        .filter(|value| !value.is_empty() && !value.contains(UNRESOLVED_MARKER))
        .or_else(|| options.build_config.clone())
}

fn warn_for_unknown_build_selection(
    selection: &mut BuildSelection,
    reject_as_invalid: bool,
    is_platform: bool,
    project_file: &Path,
    warnings: &mut Vec<String>,
) {
    let Some(selected) = selection.selected.as_deref() else {
        return;
    };
    // An empty candidate list provides no declared set against which to
    // validate this field; preserve the value and its selection mode.
    if selection.candidates.is_empty()
        || selection
            .candidates
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(selected))
    {
        return;
    }
    let subject = if is_platform {
        "platform"
    } else {
        "build configuration"
    };
    warnings.push(format!(
        "{subject} `{selected}` is not defined by {}",
        project_file.display()
    ));
    if reject_as_invalid {
        selection.mode = BuildSelectionMode::Invalid;
    }
}

fn selected_standalone_property(
    overrides: &EffectiveOverrides,
    client_value: Option<&String>,
    name: &str,
) -> Option<String> {
    client_value
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .or_else(|| {
            overrides
                .properties
                .get(name)
                .cloned()
                .filter(|value| !value.trim().is_empty())
        })
}

fn selected_platform(builder: &ProjectBuilder, options: &ProjectOptions) -> Option<String> {
    builder
        .property("platform")
        .filter(|value| !value.is_empty() && !value.contains(UNRESOLVED_MARKER))
        .or_else(|| builder.property("dcc_platform"))
        .filter(|value| !value.is_empty() && !value.contains(UNRESOLVED_MARKER))
        .or_else(|| options.platform.clone())
}

fn scan_system_pas_from_paths(
    search_paths: &[ProjectPathEntry],
    browsing_paths: &[ProjectPathEntry],
    builder: &mut ProjectBuilder,
    work_budget: Option<&dyn ProjectWorkBudget>,
    cancel: Option<&AtomicBool>,
    metadata_files: &mut Vec<PathBuf>,
) -> Result<Option<Result<crate::rtl_constants::SystemPasScan, String>>, String> {
    let mut seen_directories = HashSet::new();
    for directory in search_paths.iter().chain(browsing_paths) {
        check_project_scan_cancel(cancel)?;
        if !seen_directories.insert(directory.clone()) {
            continue;
        }
        let candidate = directory.path.join("System.pas");
        add_unique_path(metadata_files, candidate.clone());
        add_metadata_observation(
            &mut builder.metadata_observations,
            MetadataObservation::Stat { path: candidate },
        );
        let path = match find_system_pas_in_directory(&directory.path, cancel, work_budget) {
            Ok(path) => {
                builder
                    .system_pas_searches
                    .push(SystemPasSearchObservation {
                        directory: directory.path.clone(),
                        selected_path: path.clone(),
                        complete: true,
                    });
                path
            }
            Err(error)
                if error == "request cancelled"
                    || work_budget.is_some_and(|budget| budget.is_transient_error(&error)) =>
            {
                return Err(error);
            }
            Err(error) => {
                builder
                    .system_pas_searches
                    .push(SystemPasSearchObservation {
                        directory: directory.path.clone(),
                        selected_path: None,
                        complete: false,
                    });
                builder.warnings.push(format!(
                    "could not inspect {} for System.pas: {error}",
                    directory.path.display()
                ));
                continue;
            }
        };
        let Some(path) = path else {
            continue;
        };
        add_unique_path(metadata_files, path.clone());
        add_metadata_observation(
            &mut builder.metadata_observations,
            MetadataObservation::Stat { path: path.clone() },
        );
        let entry = ProjectPathEntry {
            path,
            provenance: directory.provenance.clone(),
        };
        return Ok(Some(
            match builder
                .read_policy
                .read_pascal_payload_with_observation(&entry, MAX_PROJECT_BYTES)
            {
                Ok((source, observation)) => {
                    add_metadata_observation(&mut builder.metadata_observations, observation);
                    Ok(crate::rtl_constants::scan_system_pas(&source))
                }
                Err(error) => Err(error),
            },
        ));
    }
    Ok(None)
}

fn find_system_pas_in_directory(
    directory: &Path,
    cancel: Option<&AtomicBool>,
    work_budget: Option<&dyn ProjectWorkBudget>,
) -> Result<Option<PathBuf>, String> {
    check_project_scan_cancel(cancel)?;
    if let Some(work_budget) = work_budget {
        work_budget.check_cancelled()?;
        work_budget.charge_path_visits(1)?;
    }
    let exact_path = directory.join("System.pas");
    match fs::symlink_metadata(&exact_path) {
        Ok(metadata) if metadata.file_type().is_file() => return Ok(Some(exact_path)),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "could not inspect System.pas candidate {}: {error}",
                exact_path.display()
            ));
        }
    }

    let mut entries = fs::read_dir(directory).map_err(|error| {
        format!(
            "could not inspect project directory {}: {error}",
            directory.display()
        )
    })?;
    let mut visited_entries = 0usize;
    loop {
        check_project_scan_cancel(cancel)?;
        if let Some(work_budget) = work_budget {
            work_budget.check_cancelled()?;
            work_budget.charge_path_visits(1)?;
        }
        let Some(entry) = entries.next() else {
            return Ok(None);
        };
        visited_entries = visited_entries.saturating_add(1);
        if visited_entries > MAX_PROJECT_DIRECTORY_ENTRIES {
            return Err(format!(
                "project directory entry limit ({MAX_PROJECT_DIRECTORY_ENTRIES}) reached in {}",
                directory.display()
            ));
        }
        let entry = entry.map_err(|error| {
            format!(
                "could not inspect project directory entry under {}: {error}",
                directory.display()
            )
        })?;
        if entry
            .file_type()
            .map_err(|error| {
                format!(
                    "could not inspect project directory entry {}: {error}",
                    entry.path().display()
                )
            })?
            .is_file()
            && entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case("System.pas")
        {
            return Ok(Some(entry.path()));
        }
    }
}

/// Find a `System.pas` candidate using the same bounded, case-insensitive
/// lookup as project discovery. Used to revalidate a cached search result when
/// the directory's entry stamp changes.
pub fn system_pas_candidate_in_directory(
    directory: &Path,
    cancel: Option<&AtomicBool>,
    work_budget: Option<&dyn ProjectWorkBudget>,
) -> Result<Option<PathBuf>, String> {
    find_system_pas_in_directory(directory, cancel, work_budget)
}

fn merge_project_conditional_context(
    options: &ProjectOptions,
    builder: &ProjectBuilder,
    project_defines: &[String],
    warnings: &mut Vec<String>,
) -> ConditionalContext {
    let mut context = options.conditional_context.clone();
    for define in project_defines {
        let name = define.trim().trim_start_matches('&').to_ascii_uppercase();
        if !name.is_empty() && !context.defines.contains_key(&name) {
            context.set_define(name, ConditionalFact::True);
        }
    }

    if context.compiler_version.is_none() {
        let documented = builder.property("compilerversion");
        let dcc = builder.property("dcc_compilerversion");
        if let (Some(documented), Some(dcc)) = (documented.as_deref(), dcc.as_deref()) {
            match (
                CompilerVersion::parse(documented),
                CompilerVersion::parse(dcc),
            ) {
                (Some(left), Some(right)) if left == right => {
                    context.compiler_version = Some(left);
                }
                _ => warnings.push(format!(
                    "conflicting CompilerVersion metadata ({documented:?} vs {dcc:?}); CompilerVersion remains unknown"
                )),
            }
        } else if let Some(raw) = documented.as_deref().or(dcc.as_deref()) {
            if let Some(version) = CompilerVersion::parse(raw) {
                context.compiler_version = Some(version);
            } else {
                warnings.push(format!(
                    "unsupported compiler version metadata {raw:?}; CompilerVersion remains unknown"
                ));
            }
        }
    }

    // These are the bounded project-property spellings with stable Delphi
    // IFOPT aliases.  Unknown properties are intentionally not guessed.
    const OPTION_PROPERTIES: &[(&str, &str)] = &[
        ("dcc_rangechecks", "R"),
        ("dcc_overflowchecks", "Q"),
        ("dcc_optimization", "O"),
        ("dcc_assertions", "C"),
        ("dcc_runtimechecks", "RUNTIME_CHECKS"),
        ("dcc_debuginformation", "D"),
    ];
    for (property, canonical) in OPTION_PROPERTIES {
        let Some(raw) = builder.property(property) else {
            continue;
        };
        let value = parse_conditional_fact(&raw).unwrap_or(ConditionalFact::Unknown);
        if !context.options.contains_key(*canonical) {
            context.set_option(canonical, value);
        }
    }
    context
}

fn parse_conditional_fact(value: &str) -> Option<ConditionalFact> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "+" => Some(ConditionalFact::True),
        "0" | "false" | "no" | "off" | "-" => Some(ConditionalFact::False),
        "" => None,
        _ => Some(ConditionalFact::Unknown),
    }
}

/// Whole-layout overrides retain unknown fields; contradictory build targets
/// cannot establish either profile. Invalid selections cannot be repaired by an
/// override. The compiler version remains solely in ConditionalContext.
fn selected_layout(
    inferred: LayoutContext,
    caller: &ConditionalContext,
    invalid_target: bool,
) -> LayoutContext {
    if !caller.layout_explicit {
        return inferred;
    }
    if invalid_target
        || matches!(
            (&inferred.platform, &caller.layout.platform),
            (Some(build), Some(explicit)) if build != explicit
        )
    {
        return LayoutContext::default();
    }
    caller.layout.clone()
}

fn apply_project_layout_properties(settings: &mut LayoutSettings, builder: &ProjectBuilder) {
    // Reads use the already bounded, ordered metadata evaluator. A tainted or
    // malformed setting invalidates just that setting, not unrelated facts.
    // An unknown import can have supplied an otherwise absent setting.
    let property = |name: &str| -> Option<Option<String>> {
        if builder.unknown_properties.contains(name) {
            return Some(None);
        }
        match builder.property(name) {
            Some(raw) if !raw.contains(UNRESOLVED_MARKER) => Some(Some(raw)),
            Some(_) => Some(None),
            None if builder.unknown_import_taint => Some(None),
            None => None,
        }
    };
    if let Some(raw) = property("dcc_alignment") {
        // ALIGN 16 applicability is not independently verified for our targets.
        settings.record_alignment = raw
            .and_then(|raw| raw.trim().parse::<u8>().ok())
            .filter(|value| matches!(value, 1 | 2 | 4 | 8));
    }
    if let Some(raw) = property("dcc_minimumenumsize") {
        settings.minimum_enum_size = raw
            .and_then(|raw| raw.trim().parse::<u8>().ok())
            .filter(|value| matches!(value, 1 | 2 | 4));
    }
    if let Some(raw) = property("dcc_longstrings") {
        settings.long_strings = raw
            .and_then(|raw| parse_conditional_fact(&raw))
            .unwrap_or(ConditionalFact::Unknown);
    }
}

#[derive(Default)]
struct ProjectBootstrap {
    evidence: InstallationEvidence,
    compiler_conflicting: bool,
    other_conflicting: bool,
    config: Option<String>,
    platform: Option<String>,
    metadata_files: Vec<PathBuf>,
}

/// Extract only properties physically declared in the root project's direct
/// PropertyGroups. Group/property conditions are evaluated against only known
/// caller and local configuration facts; unknown-conditional identity values
/// remain ambiguous. Imports and target-time properties are never inspected.
fn parse_project_bootstrap(
    xml: &str,
    source_file: &Path,
    options: &ProjectOptions,
    overrides: &EffectiveOverrides,
    read_policy: &ReadPolicy,
) -> ProjectBootstrap {
    let operations = match parse_xml_operations(xml, source_file) {
        Ok(operations) => operations,
        Err(_) => {
            return ProjectBootstrap {
                evidence: InstallationEvidence {
                    conflicting: true,
                    ..InstallationEvidence::default()
                },
                other_conflicting: true,
                ..ProjectBootstrap::default()
            };
        }
    };
    let mut properties = HashMap::new();
    let mut unknown_properties = HashSet::new();
    for (key, option_value) in [
        ("config", options.build_config.as_ref()),
        ("platform", options.platform.as_ref()),
    ] {
        if let Some(value) = option_value {
            properties.insert(key.to_owned(), value.clone());
        } else if let Some(value) = overrides.properties.get(key) {
            properties.insert(key.to_owned(), value.clone());
        }
    }
    let mut values = BTreeMap::<String, Vec<String>>::new();
    let mut compiler_uncertain = false;
    let mut other_identity_uncertain = false;
    let mut unknown_import_taint = false;
    let mut warnings = Vec::new();
    let mut metadata_files = Vec::new();
    let base = source_file.parent().unwrap_or_else(|| Path::new("."));
    let config_is_fixed =
        options.build_config.is_some() || overrides.properties.contains_key("config");
    let platform_is_fixed =
        options.platform.is_some() || overrides.properties.contains_key("platform");
    for operation in operations {
        let group = match operation {
            XmlOperation::Import(import) => {
                let import_truth = condition_matches(
                    import.condition.as_deref(),
                    ConditionEnvironment {
                        properties: &properties,
                        unknown_properties: &unknown_properties,
                        unknown_import_taint,
                        overrides,
                        read_policy,
                    },
                    base,
                    &mut metadata_files,
                    &mut warnings,
                    source_file,
                );
                if import_truth == TruthValue::False {
                    continue;
                }
                unknown_import_taint = true;
                if !config_is_fixed {
                    unknown_properties.insert("config".to_owned());
                }
                if !platform_is_fixed {
                    unknown_properties.insert("platform".to_owned());
                }
                continue;
            }
            XmlOperation::PropertyGroup(group) => group,
            XmlOperation::DccReference(_) | XmlOperation::Unsupported(_) => continue,
        };
        let group_truth = condition_matches(
            group.condition.as_deref(),
            ConditionEnvironment {
                properties: &properties,
                unknown_properties: &unknown_properties,
                unknown_import_taint,
                overrides,
                read_policy,
            },
            base,
            &mut metadata_files,
            &mut warnings,
            source_file,
        );
        for entry in group.properties {
            let identity_property = matches!(
                entry.name.to_ascii_lowercase().as_str(),
                "projectversion" | "compilerversion" | "dcc_compilerversion" | "bds"
            );
            if group_truth == TruthValue::False {
                continue;
            }
            let entry_truth = condition_matches(
                entry.condition.as_deref(),
                ConditionEnvironment {
                    properties: &properties,
                    unknown_properties: &unknown_properties,
                    unknown_import_taint,
                    overrides,
                    read_policy,
                },
                base,
                &mut metadata_files,
                &mut warnings,
                source_file,
            );
            let truth = group_truth.and(entry_truth);
            match truth {
                TruthValue::True => {
                    let key = entry.name.to_ascii_lowercase();
                    let value = entry.value.trim();
                    if !value.is_empty() {
                        values
                            .entry(key.clone())
                            .or_default()
                            .push(value.to_owned());
                        if (key == "config" && !config_is_fixed)
                            || (key == "platform" && !platform_is_fixed)
                        {
                            unknown_properties.remove(&key);
                            properties.insert(key, value.to_owned());
                        }
                    }
                }
                TruthValue::False => {}
                TruthValue::Unknown if identity_property => {
                    match entry.name.to_ascii_lowercase().as_str() {
                        "compilerversion" | "dcc_compilerversion" => compiler_uncertain = true,
                        _ => other_identity_uncertain = true,
                    }
                }
                TruthValue::Unknown => {}
            }
        }
    }
    let unique_value = |name: &str| -> Option<String> {
        let candidates = values.get(name)?;
        let first = candidates.first()?;
        candidates
            .iter()
            .all(|candidate| candidate.eq_ignore_ascii_case(first))
            .then(|| first.clone())
    };
    let compiler_raw = ["compilerversion", "dcc_compilerversion"]
        .into_iter()
        .filter_map(&unique_value)
        .collect::<Vec<_>>();
    let compiler_version = if let Some(first) = compiler_raw.first() {
        let parsed = CompilerVersion::parse(first);
        compiler_raw
            .iter()
            .all(|value| CompilerVersion::parse(value) == parsed)
            .then_some(parsed)
            .flatten()
    } else {
        None
    };
    let project_version = unique_value("projectversion");
    let bds_root = unique_value("bds");
    let compiler_conflicting = compiler_uncertain
        || ["compilerversion", "dcc_compilerversion"]
            .into_iter()
            .any(|key| {
                values.get(key).is_some_and(|items| {
                    items
                        .iter()
                        .any(|item| !item.eq_ignore_ascii_case(&items[0]))
                })
            })
        || compiler_raw.len() > 1
            && compiler_raw
                .iter()
                .any(|value| CompilerVersion::parse(value) != compiler_version);
    let other_conflicting = other_identity_uncertain
        || ["projectversion", "bds"].into_iter().any(|key| {
            values.get(key).is_some_and(|items| {
                items
                    .iter()
                    .any(|item| !item.eq_ignore_ascii_case(&items[0]))
            })
        });
    ProjectBootstrap {
        evidence: InstallationEvidence {
            compiler_version,
            project_version,
            bds_root,
            conflicting: compiler_conflicting || other_conflicting,
        },
        compiler_conflicting,
        other_conflicting,
        config: if config_is_fixed {
            properties.get("config").cloned()
        } else {
            unique_value("config")
        },
        platform: if platform_is_fixed {
            properties.get("platform").cloned()
        } else {
            unique_value("platform")
        },
        metadata_files,
    }
}

fn property_list(builder: &ProjectBuilder, name: &str) -> Vec<String> {
    builder
        .property(name)
        .map(|value| {
            value
                .split(';')
                .map(str::trim)
                .filter(|item| !item.is_empty() && !item.contains(UNRESOLVED_MARKER))
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn full_range(value: &str) -> Vec<Range<usize>> {
    (!value.is_empty())
        .then_some(0..value.len())
        .into_iter()
        .collect()
}

pub(super) fn trim_ranges(
    ranges: &[Range<usize>],
    trim_start: usize,
    trimmed_len: usize,
) -> Vec<Range<usize>> {
    let trim_end = trim_start.saturating_add(trimmed_len);
    ranges
        .iter()
        .filter_map(|range| {
            let start = range.start.max(trim_start).min(trim_end);
            let end = range.end.max(trim_start).min(trim_end);
            (start < end).then_some((start - trim_start)..(end - trim_start))
        })
        .collect()
}

pub(super) fn trim_provenance_ranges(
    ranges: &[ProvenanceRange],
    trim_start: usize,
    trimmed_len: usize,
) -> Vec<ProvenanceRange> {
    let trim_end = trim_start.saturating_add(trimmed_len);
    ranges
        .iter()
        .map(|range| {
            if range.range.start == range.range.end {
                let position = range.range.start.max(trim_start).min(trim_end) - trim_start;
                ProvenanceRange {
                    range: position..position,
                    provenance: range.provenance.clone(),
                }
            } else {
                let start = range.range.start.max(trim_start).min(trim_end);
                let end = range.range.end.max(trim_start).min(trim_end);
                ProvenanceRange {
                    range: (start - trim_start)..(end - trim_start),
                    provenance: range.provenance.clone(),
                }
            }
        })
        .collect()
}

pub(super) fn property_list_item(
    value: &str,
    start: usize,
    end: usize,
    provenance_ranges: &[ProvenanceRange],
    default_provenance: &ProjectPathProvenance,
) -> (String, ProjectPathProvenance) {
    let item = &value[start..end];
    let trimmed = item.trim();
    let provenance = provenance_ranges
        .iter()
        .filter(|range| {
            if range.range.start == range.range.end {
                start <= range.range.start && range.range.start <= end
            } else {
                range.range.start < end && start < range.range.end
            }
        })
        .fold(default_provenance.clone(), |current, range| {
            combine_path_provenance(&current, &range.provenance)
        });
    (trimmed.to_string(), provenance)
}

fn package_list(builder: &ProjectBuilder) -> Vec<String> {
    let mut packages = Vec::new();
    for value in property_list(builder, "dcc_usepackage") {
        let package = canonical_package_name(&value);
        if !package.is_empty()
            && !packages
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(&package))
        {
            packages.push(package);
        }
    }
    packages
}

fn parse_aliases(value: Option<&str>) -> HashMap<String, String> {
    let mut aliases: HashMap<String, String> = HashMap::new();
    if let Some(value) = value {
        for item in value.split(';') {
            let Some((alias, target)) = item.split_once('=') else {
                continue;
            };
            let alias = alias.trim();
            let target = target.trim();
            if !alias.is_empty()
                && !target.is_empty()
                && !alias.contains(UNRESOLVED_MARKER)
                && !target.contains(UNRESOLVED_MARKER)
                && !aliases
                    .keys()
                    .any(|existing| existing.eq_ignore_ascii_case(alias))
            {
                aliases.insert(alias.to_string(), target.to_string());
            }
        }
    }
    aliases
}

fn add_resolved_search_path_entry(
    raw: &str,
    base: &Path,
    overrides: &EffectiveOverrides,
    search_paths: &mut Vec<ProjectPathEntry>,
    warnings: &mut Vec<String>,
    kind: &str,
    provenance: ProjectPathProvenance,
) {
    if let Some(entry) =
        resolved_search_path_entry(raw, base, overrides, warnings, kind, provenance)
    {
        add_unique_project_path_entry(search_paths, entry);
    }
}

fn resolved_search_path_entry(
    raw: &str,
    base: &Path,
    overrides: &EffectiveOverrides,
    warnings: &mut Vec<String>,
    kind: &str,
    provenance: ProjectPathProvenance,
) -> Option<ProjectPathEntry> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains(UNRESOLVED_MARKER) {
        return None;
    }
    let resolved = project_path_candidate(raw, base, overrides, warnings, kind)?;
    let candidate = lexical_normalize(&resolved.path);
    Some(
        match resolve_existing_path_status_with_provenance(
            &candidate, &resolved, raw, warnings, kind,
        ) {
            ExistingPathStatus::Found(path) => inherit_path_provenance(
                ProjectPathEntry::resolved(
                    path,
                    &resolved,
                    matches!(&provenance, ProjectPathProvenance::Configured),
                ),
                &provenance,
            ),
            ExistingPathStatus::Missing | ExistingPathStatus::Unresolvable => {
                warnings.push(search_path_missing_warning(
                    kind, raw, &candidate, &resolved,
                ));
                inherit_path_provenance(
                    ProjectPathEntry::resolved(
                        candidate,
                        &resolved,
                        matches!(&provenance, ProjectPathProvenance::Configured),
                    ),
                    &provenance,
                )
            }
        },
    )
}

pub(super) fn add_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

pub(super) fn add_unique_project_path_entry(
    entries: &mut Vec<ProjectPathEntry>,
    entry: ProjectPathEntry,
) {
    if !entries.iter().any(|existing| existing == &entry) {
        entries.push(entry);
    }
}

fn paths_from_entries(entries: &[ProjectPathEntry]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in entries {
        add_unique_path(&mut paths, entry.path.clone());
    }
    paths
}

pub(super) fn add_metadata_file(
    metadata_files: &mut Vec<PathBuf>,
    path: PathBuf,
    warnings: &mut Vec<String>,
    source_file: &Path,
    kind: &str,
) -> bool {
    if metadata_files.iter().any(|existing| existing == &path) {
        return true;
    }
    if metadata_files.len() >= MAX_METADATA_FILES {
        warnings.push(format!(
            "{kind} metadata file limit ({MAX_METADATA_FILES}) reached while reading {}",
            source_file.display()
        ));
        return false;
    }
    metadata_files.push(path);
    true
}

pub fn add_metadata_observation(
    observations: &mut Vec<MetadataObservation>,
    observation: MetadataObservation,
) {
    let Some(existing) = observations
        .iter_mut()
        .find(|existing| existing.path() == observation.path())
    else {
        observations.push(observation);
        return;
    };
    if matches!(existing, MetadataObservation::Stat { .. })
        && matches!(&observation, MetadataObservation::Payload { .. })
    {
        *existing = observation;
    }
}

pub(super) fn complete_metadata_observations(
    paths: &[PathBuf],
    observations: Vec<MetadataObservation>,
) -> Vec<MetadataObservation> {
    let mut merged = Vec::with_capacity(observations.len());
    for observation in observations {
        add_metadata_observation(&mut merged, observation);
    }
    let mut observations = merged;
    for path in paths {
        if !observations
            .iter()
            .any(|observation| observation.path() == path)
        {
            observations.push(MetadataObservation::Stat { path: path.clone() });
        }
    }
    observations
}

pub(super) fn paths_equal_ci(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

pub(super) fn add_unit_candidate(
    units: &mut HashMap<String, Vec<PathBuf>>,
    name: String,
    path: PathBuf,
) {
    let candidates = units.entry(name).or_default();
    if !candidates.iter().any(|existing| existing == &path) {
        candidates.push(path);
    }
}

fn add_unit_candidate_entry(
    units: &mut HashMap<String, Vec<PathBuf>>,
    entries: &mut HashMap<String, Vec<ProjectPathEntry>>,
    name: String,
    entry: ProjectPathEntry,
) {
    add_unit_candidate(units, name.clone(), entry.path.clone());
    add_unique_project_path_entry(entries.entry(name).or_default(), entry);
}

fn add_explicit_units_from_source(
    source_entry: &ProjectPathEntry,
    units: &mut HashMap<String, Vec<PathBuf>>,
    entries: &mut HashMap<String, Vec<ProjectPathEntry>>,
    warnings: &mut Vec<String>,
    overrides: &EffectiveOverrides,
    read_policy: &ReadPolicy,
    tracker: &mut ProjectReadTracker<'_>,
) -> Option<MetadataObservation> {
    let source_path = &source_entry.path;
    if !read_policy.allows_entry(source_entry) {
        warnings.push(format!(
            "ignored main source outside authorized read roots: {}",
            source_path.display()
        ));
        return None;
    }
    let (contents, observation) = match read_payload_with_tracker(
        read_policy,
        source_entry,
        MAX_MAIN_SOURCE_BYTES,
        tracker,
    ) {
        Ok(payload) => payload,
        Err(error) => {
            warnings.push(format!(
                "could not read main source {} for explicit unit paths: {error}",
                source_path.display()
            ));
            return None;
        }
    };
    let Some(base) = source_path.parent() else {
        return Some(observation);
    };
    for (unit_name, raw_path) in parse_explicit_unit_paths(&contents) {
        let Some(entry) = resolve_project_path_entry(
            &raw_path,
            base,
            overrides,
            warnings,
            "explicit DPR/DPK unit path",
            true,
            false,
        ) else {
            continue;
        };
        let entry = inherit_path_provenance(entry, &source_entry.provenance);
        add_unit_candidate_entry(units, entries, canonical_unit_name(&unit_name), entry);
    }
    Some(observation)
}

pub(super) fn inherit_path_provenance(
    mut entry: ProjectPathEntry,
    parent: &ProjectPathProvenance,
) -> ProjectPathEntry {
    if matches!(entry.provenance, ProjectPathProvenance::LegacyNative) {
        entry.provenance = parent.clone();
    }
    entry
}

pub(super) fn combine_path_provenance(
    current: &ProjectPathProvenance,
    next: &ProjectPathProvenance,
) -> ProjectPathProvenance {
    match (current, next) {
        (ProjectPathProvenance::Mapped { root }, _) => {
            ProjectPathProvenance::Mapped { root: root.clone() }
        }
        (_, ProjectPathProvenance::Mapped { root }) => {
            ProjectPathProvenance::Mapped { root: root.clone() }
        }
        (ProjectPathProvenance::Configured, _) | (_, ProjectPathProvenance::Configured) => {
            ProjectPathProvenance::Configured
        }
        _ => ProjectPathProvenance::LegacyNative,
    }
}

pub(super) fn expanded_path_provenance(
    expanded: &ExpandedValue,
    source_provenance: &ProjectPathProvenance,
) -> ProjectPathProvenance {
    expanded
        .provenance_ranges
        .iter()
        .fold(source_provenance.clone(), |current, range| {
            combine_path_provenance(&current, &range.provenance)
        })
}

pub(super) fn canonical_unit_name(name: &str) -> String {
    name.trim().trim_matches('.').to_ascii_lowercase()
}

pub(super) fn canonical_package_name(name: &str) -> String {
    let normalized = name.trim().replace('\\', "/");
    let name = normalized.rsplit('/').next().unwrap_or_default().trim();
    let stem = ["dcp", "dpk", "dproj", "bpl"]
        .iter()
        .find_map(|extension| {
            name.len()
                .checked_sub(extension.len() + 1)
                .filter(|&stem_len| {
                    name.get(stem_len..)
                        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(&format!(".{extension}")))
                })
                .and_then(|stem_len| name.get(..stem_len))
        })
        .unwrap_or(name);
    stem.trim().trim_matches('.').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn imported_mapped_optset_path_properties_retain_mapping_provenance() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path();
        let sdk = root.join("sdk");
        let outside = root.join("outside");
        let main = root.join("App.dpr");
        fs::create_dir_all(&sdk).expect("mapped destination");
        fs::create_dir_all(&outside).expect("relative path directory");
        fs::write(&main, "program App; begin end.").expect("main source");
        fs::write(
            sdk.join("settings.optset"),
            "<Project><PropertyGroup><DCC_UnitSearchPath>outside</DCC_UnitSearchPath></PropertyGroup></Project>",
        )
        .expect("mapped optset");
        fs::write(
            root.join("App.dproj"),
            "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup><Import Project=\"C:\\\\SDK\\\\settings.optset\" /></Project>",
        )
        .expect("project descriptor");
        fs::write(
            root.join(".delphi-tools.local.toml"),
            format!(
                "[[path_mappings]]\nfrom = 'C:\\\\SDK'\nto = '{}'\n",
                sdk.display()
            ),
        )
        .expect("mapping configuration");

        let context = ProjectContext::discover_with_overrides(
            &main,
            &[root.to_path_buf()],
            &ProjectOptions::default(),
            &OverrideSession::new(None),
        )
        .expect("discover project");
        let entry = context
            .search_path_entries
            .iter()
            .find(|entry| entry.path == outside)
            .expect("imported optset search path");

        assert_eq!(
            entry.provenance,
            ProjectPathProvenance::Mapped { root: sdk },
            "path-list values from mapped optsets must not fall back to legacy provenance"
        );
    }

    #[cfg(unix)]
    #[test]
    fn mapped_optset_property_origin_reaches_a_later_root_reference() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path();
        let sdk = root.join("sdk");
        let outside = root.join("outside");
        let main = root.join("App.dpr");
        let provider = outside.join("Provider.pas");
        fs::create_dir_all(&sdk).expect("mapped destination");
        fs::create_dir_all(&outside).expect("reference directory");
        fs::write(&main, "program App; begin end.").expect("main source");
        fs::write(&provider, "unit Provider; interface implementation end.")
            .expect("provider source");
        fs::write(
            sdk.join("settings.optset"),
            "<Project><PropertyGroup><ProviderPath>outside/Provider.pas</ProviderPath></PropertyGroup></Project>",
        )
        .expect("mapped optset");
        fs::write(
            root.join("App.dproj"),
            "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup><Import Project=\"C:\\\\SDK\\\\settings.optset\" /><ItemGroup><DCCReference Include=\"$(ProviderPath)\" /></ItemGroup></Project>",
        )
        .expect("project descriptor");
        fs::write(
            root.join(".delphi-tools.local.toml"),
            format!(
                "[[path_mappings]]\nfrom = 'C:\\\\SDK'\nto = '{}'\n",
                sdk.display()
            ),
        )
        .expect("mapping configuration");

        let context = ProjectContext::discover_with_overrides(
            &main,
            &[root.to_path_buf()],
            &ProjectOptions::default(),
            &OverrideSession::new(None),
        )
        .expect("discover project");
        let entry = context
            .explicit_unit_entries
            .values()
            .flatten()
            .find(|entry| entry.path == provider)
            .expect("expanded root reference");

        assert_eq!(
            entry.provenance,
            ProjectPathProvenance::Mapped { root: sdk },
            "mapped property expansion must not fall back to root legacy provenance"
        );
    }

    #[cfg(unix)]
    #[test]
    fn empty_configured_substitution_reaches_a_search_path_item() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path();
        let outside = root.join("outside");
        let main = root.join("App.dpr");
        fs::create_dir_all(&outside).expect("search path directory");
        fs::write(&main, "program App; begin end.").expect("main source");
        fs::write(
            root.join(".delphi-tools.local.toml"),
            "[properties]\nPrefix = ''\n",
        )
        .expect("override configuration");
        fs::write(
            root.join("App.dproj"),
            "<Project><PropertyGroup><MainSource>App.dpr</MainSource><DCC_UnitSearchPath>$(Prefix)outside</DCC_UnitSearchPath></PropertyGroup></Project>",
        )
        .expect("project descriptor");

        let context = ProjectContext::discover_with_overrides(
            &main,
            &[root.to_path_buf()],
            &ProjectOptions::default(),
            &OverrideSession::new(None),
        )
        .expect("discover project");
        let entry = context
            .search_path_entries
            .iter()
            .find(|entry| entry.path == outside)
            .expect("configured search path");

        assert_eq!(
            entry.provenance,
            ProjectPathProvenance::Configured,
            "an empty configured substitution must retain configured provenance"
        );
    }

    #[cfg(unix)]
    #[test]
    fn trimmed_empty_configured_list_item_keeps_its_boundary_provenance() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path();
        let outside = root.join("outside/sdk");
        let legacy = root.join("legacy");
        let main = root.join("App.dpr");
        fs::create_dir_all(&outside).expect("configured search path");
        fs::create_dir_all(&legacy).expect("legacy search path");
        fs::write(&main, "program App; begin end.").expect("main source");
        fs::write(
            root.join(".delphi-tools.local.toml"),
            "[properties]\nPrefix = ''\n",
        )
        .expect("override configuration");
        fs::write(
            root.join("App.dproj"),
            format!(
                "<Project><PropertyGroup><MainSource>App.dpr</MainSource><DCC_UnitSearchPath>$(Prefix) {outside};legacy</DCC_UnitSearchPath></PropertyGroup></Project>",
                outside = outside.display()
            ),
        )
        .expect("project descriptor");

        let context = ProjectContext::discover_with_overrides(
            &main,
            &[root.to_path_buf()],
            &ProjectOptions::default(),
            &OverrideSession::new(None),
        )
        .expect("discover project");
        let configured = context
            .search_path_entries
            .iter()
            .find(|entry| entry.path == outside)
            .expect("configured item");
        let legacy = context
            .search_path_entries
            .iter()
            .find(|entry| entry.path == legacy)
            .expect("legacy item");

        assert_eq!(
            configured.provenance,
            ProjectPathProvenance::Configured,
            "the configured empty substitution must survive item trimming"
        );
        assert_eq!(
            legacy.provenance,
            ProjectPathProvenance::LegacyNative,
            "the unrelated legacy item must not inherit configured provenance"
        );
    }

    #[cfg(unix)]
    #[test]
    fn trailing_empty_override_markers_keep_unit_and_include_paths_configured() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("project");
        let outside = temp.path().join("outside");
        let unit = outside.join("unit");
        let include = outside.join("include");
        let intermediate = outside.join("intermediate");
        let boundary = outside.join("boundary");
        let boundary_include = outside.join("boundary_include");
        let legacy = root.join("legacy");
        let main = root.join("App.dpr");

        fs::create_dir_all(&root).expect("project directory");
        for path in [
            &unit,
            &include,
            &intermediate,
            &boundary,
            &boundary_include,
            &legacy,
        ] {
            fs::create_dir_all(path).expect("path directory");
            fs::write(path.join("Probe.pas"), b"payload").expect("path payload");
        }
        fs::write(&main, "program App; begin end.").expect("main source");
        fs::write(
            root.join(".delphi-tools.local.toml"),
            "[properties]\nEmpty = ''\n",
        )
        .expect("override configuration");
        fs::write(
            root.join("App.dproj"),
            format!(
                "<Project><PropertyGroup><MainSource>App.dpr</MainSource><Intermediate>{intermediate} $(Empty)</Intermediate><DCC_UnitSearchPath>{unit} $(Empty);{legacy};{boundary}$(Empty);$(Intermediate)</DCC_UnitSearchPath><DCC_IncludePath>{include} $(Empty);{legacy};{boundary_include}$(Empty)</DCC_IncludePath></PropertyGroup></Project>",
                intermediate = intermediate.display(),
                unit = unit.display(),
                legacy = legacy.display(),
                boundary = boundary.display(),
                include = include.display(),
                boundary_include = boundary_include.display(),
            ),
        )
        .expect("project descriptor");

        let context = ProjectContext::discover_with_overrides(
            &main,
            std::slice::from_ref(&root),
            &ProjectOptions::default(),
            &OverrideSession::new(None),
        )
        .expect("discover project");

        let search_entry = |path: &Path| {
            context
                .search_path_entries
                .iter()
                .find(|entry| entry.path == path)
                .unwrap_or_else(|| panic!("missing search path entry {path:?}"))
        };
        let include_entry = |path: &Path| {
            context
                .include_path_entries
                .iter()
                .find(|entry| entry.path == path)
                .unwrap_or_else(|| panic!("missing include path entry {path:?}"))
        };
        let assert_external_is_configured_and_denied =
            |entry: &ProjectPathEntry, path: &Path, label: &str| {
                assert_eq!(
                    entry.provenance,
                    ProjectPathProvenance::Configured,
                    "{label} lost configured provenance"
                );
                let payload = ProjectPathEntry {
                    path: path.join("Probe.pas"),
                    provenance: entry.provenance.clone(),
                };
                assert!(
                    !context.read_policy.allows_entry(&payload),
                    "{label} payload unexpectedly authorized"
                );
                assert!(
                    context
                        .read_policy
                        .read_payload_bytes(&payload, 1024)
                        .is_err(),
                    "{label} payload was not denied before opening"
                );
            };

        assert_external_is_configured_and_denied(search_entry(&unit), &unit, "inline unit path");
        assert_external_is_configured_and_denied(
            search_entry(&intermediate),
            &intermediate,
            "derived intermediate unit path",
        );
        assert_external_is_configured_and_denied(
            search_entry(&boundary),
            &boundary,
            "no-space unit boundary path",
        );
        assert_external_is_configured_and_denied(
            include_entry(&include),
            &include,
            "inline include path",
        );
        assert_external_is_configured_and_denied(
            include_entry(&boundary_include),
            &boundary_include,
            "no-space include boundary path",
        );

        assert_eq!(
            search_entry(&legacy).provenance,
            ProjectPathProvenance::LegacyNative,
            "literal legacy search path inherited configured provenance"
        );
        assert_eq!(
            include_entry(&legacy).provenance,
            ProjectPathProvenance::LegacyNative,
            "literal legacy include path inherited configured provenance"
        );
    }

    #[cfg(unix)]
    #[test]
    fn configured_markers_stay_with_trimmed_search_and_include_items() {
        for (case_name, prefix) in [("empty", ""), ("whitespace", " ")] {
            let temp = tempfile::tempdir().expect("temporary workspace");
            let root = temp.path().join("project");
            let outside = temp.path().join("outside");
            let main = root.join("App.dpr");
            fs::create_dir_all(&root).expect("project directory");
            fs::create_dir_all(&outside).expect("outside directory");
            let configured_names = ["first", "second", "trailing", "intermediate"];
            let legacy_names = ["legacy", "legacy2", "legacy3", "legacy4", "legacy5"];
            for name in configured_names.iter().chain(legacy_names.iter()) {
                fs::create_dir_all(root.join(name)).expect("legacy directory");
                fs::create_dir_all(outside.join(name)).expect("configured directory");
                fs::write(root.join(name).join("Probe.pas"), b"probe").expect("legacy payload");
                fs::write(outside.join(name).join("Probe.pas"), b"probe")
                    .expect("configured payload");
            }
            fs::write(&main, "program App; begin end.").expect("main source");
            fs::write(
                root.join(".delphi-tools.local.toml"),
                format!("[properties]\nPrefix = '{prefix}'\n"),
            )
            .expect("override configuration");
            let search_paths = format!(
                "$(Prefix) {outside}/first;{root}/legacy;{root}/legacy2;$(Prefix) {outside}/second;{root}/legacy3;{outside}/trailing$(Prefix);$(Intermediate) {outside}/intermediate;{root}/legacy4;{root}/legacy5",
                outside = outside.display(),
                root = root.display(),
            );
            let include_paths = search_paths.replace("first", "include_first");
            fs::create_dir_all(outside.join("include_first")).expect("configured include");
            fs::create_dir_all(root.join("include_first")).expect("legacy include");
            fs::write(
                root.join("App.dproj"),
                format!(
                    "<Project><PropertyGroup><MainSource>App.dpr</MainSource><Intermediate>$(Prefix)</Intermediate><DCC_UnitSearchPath>{search_paths}</DCC_UnitSearchPath><DCC_IncludePath>{include_paths}</DCC_IncludePath></PropertyGroup></Project>"
                ),
            )
            .expect("project descriptor");

            let context = ProjectContext::discover_with_overrides(
                &main,
                std::slice::from_ref(&root),
                &ProjectOptions::default(),
                &OverrideSession::new(None),
            )
            .expect("discover project");

            for name in configured_names {
                let path = outside.join(name);
                let search_entry = context
                    .search_path_entries
                    .iter()
                    .find(|entry| entry.path == path)
                    .unwrap_or_else(|| panic!("{case_name}: configured search path {path:?}"));
                assert_eq!(
                    search_entry.provenance,
                    ProjectPathProvenance::Configured,
                    "{case_name}: configured search marker was lost for {name}"
                );
                let search_payload = ProjectPathEntry {
                    path: path.join("Probe.pas"),
                    provenance: search_entry.provenance.clone(),
                };
                assert!(
                    !context.read_policy.allows_entry(&search_payload),
                    "{case_name}: configured outside search payload must remain unauthorized"
                );
                assert!(
                    context
                        .read_policy
                        .read_payload_bytes(&search_payload, 1024)
                        .is_err(),
                    "{case_name}: configured outside search payload must be denied before opening"
                );
                let include_name = if name == "first" {
                    "include_first"
                } else {
                    name
                };
                let include_path = outside.join(include_name);
                let include_entry = context
                    .include_path_entries
                    .iter()
                    .find(|entry| entry.path == include_path)
                    .unwrap_or_else(|| {
                        panic!("{case_name}: configured include path {include_path:?}")
                    });
                assert_eq!(
                    include_entry.provenance,
                    ProjectPathProvenance::Configured,
                    "{case_name}: configured include marker was lost for {include_name}"
                );
                let include_payload = ProjectPathEntry {
                    path: include_path.join("Probe.pas"),
                    provenance: include_entry.provenance.clone(),
                };
                assert!(
                    !context.read_policy.allows_entry(&include_payload),
                    "{case_name}: configured outside include payload must remain unauthorized"
                );
                assert!(
                    context
                        .read_policy
                        .read_payload_bytes(&include_payload, 1024)
                        .is_err(),
                    "{case_name}: configured outside include payload must be denied before opening"
                );
            }
            for name in legacy_names {
                let path = root.join(name);
                let entry = context
                    .search_path_entries
                    .iter()
                    .find(|entry| entry.path == path)
                    .unwrap_or_else(|| panic!("{case_name}: legacy search path {path:?}"));
                assert_eq!(
                    entry.provenance,
                    ProjectPathProvenance::LegacyNative,
                    "{case_name}: configured marker leaked into legacy item {name}"
                );
            }
        }
    }
}
