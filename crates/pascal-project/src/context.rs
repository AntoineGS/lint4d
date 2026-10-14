use super::assembly::path_starts_with_ci;
use super::authorization::lexical_normalize;
use super::selection::{discover_context_with_selections, production_override_session};
use super::*;

/// Options supplied by a caller for project selection and evaluation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectOptions {
    /// An explicit `.dproj`, `.dpr`, or `.dpk` path. Relative paths are
    /// resolved against the supplied workspace roots.
    pub project_file: Option<PathBuf>,
    /// Exact-project session installation choices. An absent entry restores
    /// Automatic selection; project directories are never used as keys.
    pub installation_selections: HashMap<PathBuf, String>,
    /// Exact-project session build choices. Missing fields use the next
    /// configured source independently for the configuration and platform.
    pub build_selections: HashMap<PathBuf, BuildChoice>,
    /// The selected Delphi build configuration, such as `Debug` or `Release`.
    pub build_config: Option<String>,
    /// The selected Delphi platform, such as `Win32` or `Win64`.
    pub platform: Option<String>,
    /// Additional ordered source roots supplied by the caller.
    pub source_paths: Vec<String>,
    /// Explicit conditional-compilation facts supplied by the caller. Missing
    /// facts remain unknown; project metadata is merged without overwriting a
    /// fact explicitly supplied here.
    pub conditional_context: ConditionalContext,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct LocalProjectConfig {
    pub(super) version: u32,
    #[serde(default)]
    pub(super) project: Option<String>,
    #[serde(default)]
    pub(super) source_paths: Vec<String>,
    #[serde(default)]
    pub(super) defines: Vec<String>,
    #[serde(default)]
    pub(super) build_configuration: Option<String>,
}

pub type ProjectSelections = HashMap<PathBuf, PathBuf>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectCandidates {
    pub directory: Option<PathBuf>,
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectCandidateMembership {
    pub paths: Vec<PathBuf>,
    pub readable: bool,
}
/// Metadata used to resolve units without eagerly parsing the repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectContext {
    /// Whether project selection and the metadata needed for binding completed
    /// without an ambiguity or an unresolved project-selection fallback.
    pub discovery_complete: bool,
    pub binding_metadata_complete: bool,
    pub path_issues: Vec<ProjectPathIssue>,
    pub project_file: Option<PathBuf>,
    /// Direct root-project identity facts used before installation-dependent
    /// imports are evaluated.
    pub installation_evidence: InstallationEvidence,
    /// The project-scoped installation result, if profile selection applies.
    pub installation_selection: Option<InstallationSelection>,
    pub main_source: Option<PathBuf>,
    /// Ordered project and caller-provided unit search paths.
    pub search_paths: Vec<PathBuf>,
    /// The same search paths tagged with their source provenance. This keeps
    /// legacy native roots distinct from configured and mapped roots.
    pub search_path_entries: Vec<ProjectPathEntry>,
    /// Source-navigation-only fallbacks from the selected IDE installation.
    pub browsing_path_entries: Vec<ProjectPathEntry>,
    /// Selected IDE debug DCU locations, kept out of source lookup.
    pub debug_dcu_path_entries: Vec<ProjectPathEntry>,
    /// MainSource and explicit references retain the provenance of the path
    /// expression that produced them so membership cannot bypass mapped-root
    /// safety checks.
    pub main_source_entry: Option<ProjectPathEntry>,
    pub explicit_unit_entries: HashMap<String, Vec<ProjectPathEntry>>,
    /// Ordered project-relative include paths from DCC_IncludePath.
    ///
    /// Include paths are kept separate from unit search paths because include
    /// lookup must not make an arbitrary directory a Pascal unit candidate.
    pub include_paths: Vec<PathBuf>,
    /// Include paths retain their source provenance for read authorization.
    pub include_path_entries: Vec<ProjectPathEntry>,
    pub explicit_units: HashMap<String, Vec<PathBuf>>,
    pub unit_namespaces: Vec<String>,
    pub unit_aliases: HashMap<String, String>,
    pub defines: Vec<String>,
    /// Effective compiler/version/options/constants used by source analysis.
    /// `defines` remains as a compatibility projection for existing callers.
    pub conditional_context: ConditionalContext,
    /// Whether project sources can close their define set and resolved RTL constants.
    pub conditional_closure: ConditionalClosure,
    /// Caller facts and compiler/RTL predefined facts for library sources.
    pub library_conditional_context: ConditionalContext,
    /// Roots whose sources the project compiles with its own defines: the
    /// project directory, unit search paths, caller source paths, and IDE
    /// library-path entries outside the selected installation's directories.
    pub project_source_roots: Vec<PathBuf>,
    pub config: Option<String>,
    pub platform: Option<String>,
    /// The selected build configuration and the `.dproj` configuration choices.
    pub config_selection: BuildSelection,
    /// The selected target platform and the `.dproj` platform choices.
    pub platform_selection: BuildSelection,
    /// The immutable Delphi override snapshot used to evaluate this context.
    pub overrides: EffectiveOverrides,
    /// The requester-scoped read policy used while evaluating this context.
    pub read_policy: ReadPolicy,
    /// Ordered, case-insensitively unique package names from DCC_UsePackage.
    /// Package exports are resolved lazily and are not merged into the unit
    /// search paths or the project-wide unit index.
    pub packages: Vec<String>,
    /// Project, main-source, imported option-set, and automatic-selection
    /// candidate files used to build the context. Consumers can revalidate
    /// these paths without rediscovering or reparsing unrelated source files.
    pub metadata_files: Vec<PathBuf>,
    /// Exact selected installation environment and imported IDE XML files
    /// consumed while constructing this context.
    pub installation_config_files: Vec<PathBuf>,
    /// Ordered System.pas directory lookups used to derive RTL constants.
    pub system_pas_searches: Vec<SystemPasSearchObservation>,
    /// The authorization provenance for each metadata observation. A path in
    /// `metadata_files` without a payload observation is stat-only.
    pub metadata_observations: Vec<MetadataObservation>,
    pub warnings: Vec<String>,
    pub override_error: Option<String>,
}

impl ProjectContext {
    /// Equality without the observations that import resolution appends to a
    /// worker's copy of a context as it reads project files
    /// (`metadata_files`, `metadata_observations`, `warnings`). Workers that
    /// resolved different imports under one project then still agree, while
    /// every field that shapes parsing or resolution must match.
    pub fn same_analysis_inputs(&self, other: &Self) -> bool {
        // Destructured so that a new field must be classified here.
        let Self {
            discovery_complete,
            binding_metadata_complete,
            path_issues,
            project_file,
            installation_evidence,
            installation_selection,
            main_source,
            search_paths,
            search_path_entries,
            browsing_path_entries,
            debug_dcu_path_entries,
            main_source_entry,
            explicit_unit_entries,
            include_paths,
            include_path_entries,
            explicit_units,
            unit_namespaces,
            unit_aliases,
            defines,
            conditional_context,
            conditional_closure,
            library_conditional_context,
            project_source_roots,
            config,
            platform,
            config_selection,
            platform_selection,
            overrides,
            read_policy,
            packages,
            metadata_files: _,
            installation_config_files,
            system_pas_searches,
            metadata_observations: _,
            warnings: _,
            override_error,
        } = self;
        *discovery_complete == other.discovery_complete
            && *binding_metadata_complete == other.binding_metadata_complete
            && *path_issues == other.path_issues
            && *project_file == other.project_file
            && *installation_evidence == other.installation_evidence
            && *installation_selection == other.installation_selection
            && *main_source == other.main_source
            && *search_paths == other.search_paths
            && *search_path_entries == other.search_path_entries
            && *browsing_path_entries == other.browsing_path_entries
            && *debug_dcu_path_entries == other.debug_dcu_path_entries
            && *main_source_entry == other.main_source_entry
            && *explicit_unit_entries == other.explicit_unit_entries
            && *include_paths == other.include_paths
            && *include_path_entries == other.include_path_entries
            && *explicit_units == other.explicit_units
            && *unit_namespaces == other.unit_namespaces
            && *unit_aliases == other.unit_aliases
            && *defines == other.defines
            && *conditional_context == other.conditional_context
            && *conditional_closure == other.conditional_closure
            && *library_conditional_context == other.library_conditional_context
            && *project_source_roots == other.project_source_roots
            && *config == other.config
            && *platform == other.platform
            && *config_selection == other.config_selection
            && *platform_selection == other.platform_selection
            && *overrides == other.overrides
            && *read_policy == other.read_policy
            && *packages == other.packages
            && *installation_config_files == other.installation_config_files
            && *system_pas_searches == other.system_pas_searches
            && *override_error == other.override_error
    }

    pub fn can_resolve_units(&self) -> bool {
        self.discovery_complete || self.binding_metadata_complete
    }

    pub fn missing_explicit_unit(&self, canonical_name: &str) -> Option<&ProjectPathIssue> {
        self.path_issues.iter().find(|issue| {
            issue.kind == ProjectPathIssueKind::MissingReference
                && issue
                    .unit_name
                    .as_deref()
                    .is_some_and(|unit| unit.eq_ignore_ascii_case(canonical_name))
        })
    }

    /// Visit each retained child and byte-bearing value that recovery would
    /// release. Callers provide bounded/cancellable admission at every visit.
    pub fn visit_recovery_payload(
        &self,
        visit: &mut dyn FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        if let Some(path) = &self.project_file {
            visit(path.as_os_str().len())?;
        }
        if let Some(path) = &self.main_source {
            visit(path.as_os_str().len())?;
        }
        if let Some(version) = &self.installation_evidence.project_version {
            visit(version.len())?;
        }
        if let Some(root) = &self.installation_evidence.bds_root {
            visit(root.len())?;
        }
        match &self.installation_selection {
            Some(InstallationSelection::Selected { id, .. })
            | Some(InstallationSelection::Invalid { id }) => visit(id.len())?,
            Some(InstallationSelection::NeedsChoice { candidates }) => {
                for id in candidates {
                    visit(id.len())?;
                }
            }
            Some(InstallationSelection::Legacy) | None => {}
        }
        for path in &self.search_paths {
            visit(path.as_os_str().len())?;
        }
        for path in &self.project_source_roots {
            visit(path.as_os_str().len())?;
        }
        for entry in &self.search_path_entries {
            entry.visit_recovery_payload(visit)?;
        }
        for entry in self
            .browsing_path_entries
            .iter()
            .chain(&self.debug_dcu_path_entries)
        {
            entry.visit_recovery_payload(visit)?;
        }
        if let Some(entry) = &self.main_source_entry {
            entry.visit_recovery_payload(visit)?;
        }
        for (name, entries) in &self.explicit_unit_entries {
            visit(name.len())?;
            for entry in entries {
                entry.visit_recovery_payload(visit)?;
            }
        }
        for path in &self.include_paths {
            visit(path.as_os_str().len())?;
        }
        for entry in &self.include_path_entries {
            entry.visit_recovery_payload(visit)?;
        }
        for (name, paths) in &self.explicit_units {
            visit(name.len())?;
            for path in paths {
                visit(path.as_os_str().len())?;
            }
        }
        for namespace in &self.unit_namespaces {
            visit(namespace.len())?;
        }
        for (alias, unit) in &self.unit_aliases {
            visit(alias.len())?;
            visit(unit.len())?;
        }
        for define in &self.defines {
            visit(define.len())?;
        }
        self.conditional_context.visit_recovery_payload(visit)?;
        self.library_conditional_context
            .visit_recovery_payload(visit)?;
        if let Some(names) = &self.conditional_closure.rtl_constants {
            for name in names {
                visit(name.len())?;
            }
        }
        if let Some(config) = &self.config {
            visit(config.len())?;
        }
        if let Some(platform) = &self.platform {
            visit(platform.len())?;
        }
        for selection in [&self.config_selection, &self.platform_selection] {
            if let Some(selected) = &selection.selected {
                visit(selected.len())?;
            }
            for candidate in &selection.candidates {
                visit(candidate.len())?;
            }
            if let Some(project_default) = &selection.project_default {
                visit(project_default.len())?;
            }
        }
        self.overrides.visit_recovery_payload(visit)?;
        self.read_policy.visit_recovery_payload(visit)?;
        for package in &self.packages {
            visit(package.len())?;
        }
        for path in &self.metadata_files {
            visit(path.as_os_str().len())?;
        }
        for search in &self.system_pas_searches {
            visit(search.directory.as_os_str().len())?;
            if let Some(path) = &search.selected_path {
                visit(path.as_os_str().len())?;
            }
        }
        for observation in &self.metadata_observations {
            observation.visit_recovery_payload(visit)?;
        }
        for issue in &self.path_issues {
            visit(issue.source_file.as_os_str().len())?;
            visit(issue.property.len())?;
            visit(issue.raw.len())?;
            if let Some(path) = &issue.path {
                visit(path.as_os_str().len())?;
            }
            if let Some(unit_name) = &issue.unit_name {
                visit(unit_name.len())?;
            }
            if let ProjectPathProvenance::Mapped { root } = &issue.provenance {
                visit(root.as_os_str().len())?;
            }
        }
        for warning in &self.warnings {
            visit(warning.len())?;
        }
        if let Some(error) = &self.override_error {
            visit(error.len())?;
        }
        Ok(())
    }
}
impl ProjectCandidateMembership {
    pub fn visit_recovery_payload(
        &self,
        visit: &mut dyn FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        for path in &self.paths {
            visit(path.as_os_str().len())?;
        }
        Ok(())
    }
}
impl ProjectContext {
    /// Return the requester-scoped provenance for a source path.
    ///
    /// Explicit source references are checked before search roots because a
    /// mapped or configured reference must not be downgraded to the
    /// provenance of a broader native root.  The returned entry always carries
    /// the requested path while retaining the original provenance.
    pub fn path_entry_for(&self, path: &Path) -> Option<ProjectPathEntry> {
        let path = lexical_normalize(path);
        let exact = self
            .main_source_entry
            .iter()
            .chain(self.explicit_unit_entries.values().flatten())
            .find(|entry| project_paths_equal(&entry.path, &path));
        if let Some(entry) = exact {
            return Some(ProjectPathEntry {
                path,
                provenance: entry.provenance.clone(),
            });
        }

        self.search_path_entries
            .iter()
            .chain(&self.browsing_path_entries)
            .chain(&self.debug_dcu_path_entries)
            .filter(|entry| project_path_starts_with(&path, &lexical_normalize(&entry.path)))
            .max_by_key(|entry| entry.path.components().count())
            .map(|entry| ProjectPathEntry {
                path: path.clone(),
                provenance: entry.provenance.clone(),
            })
            .or_else(|| self.read_policy.entry_for_path(&path))
    }

    /// Return include lookup entries in the same precedence order used by the
    /// source resolver: owner directory, include paths, then unit paths.
    pub fn include_search_entries(&self, owner: &Path) -> Vec<ProjectPathEntry> {
        let owner_directory = owner.parent().unwrap_or(owner);
        let owner_directory = lexical_normalize(owner_directory);
        let include_paths = if self.include_path_entries.is_empty() {
            self.include_paths
                .iter()
                .filter_map(|path| self.path_entry_for(path))
                .collect::<Vec<_>>()
        } else {
            self.include_path_entries.clone()
        };
        let search_paths = if self.search_path_entries.is_empty() {
            self.search_paths
                .iter()
                .filter_map(|path| self.path_entry_for(path))
                .collect::<Vec<_>>()
        } else {
            self.search_path_entries.clone()
        };
        let mut entries = Vec::new();
        for entry in self
            .path_entry_for(&owner_directory)
            .into_iter()
            .chain(include_paths)
            .chain(search_paths)
        {
            let normalized = lexical_normalize(&entry.path);
            if entries
                .iter()
                .any(|existing: &ProjectPathEntry| project_paths_equal(&existing.path, &normalized))
            {
                continue;
            }
            entries.push(ProjectPathEntry {
                path: normalized,
                provenance: entry.provenance,
            });
        }
        entries
    }

    /// Project options carrying the selected configuration and platform.
    ///
    /// This is deliberately a pure projection: package metadata parsing owns
    /// all reads and receives the context's immutable overrides and policy
    /// separately.
    pub fn selected_project_options(&self) -> ProjectOptions {
        ProjectOptions {
            build_config: self.config.clone(),
            platform: self.platform.clone(),
            conditional_context: self.conditional_context.clone(),
            ..ProjectOptions::default()
        }
    }

    /// Return the effective conditional context, including compatibility
    /// define values retained in older manually-constructed contexts.
    pub fn effective_conditional_context(&self) -> ConditionalContext {
        let mut context = self.conditional_context.clone();
        context.absent_define = ConditionalFact::Unknown;
        for define in &self.defines {
            let name = define.trim().trim_start_matches('&').to_ascii_uppercase();
            if !name.is_empty() && !context.defines.contains_key(&name) {
                context.set_define(name, ConditionalFact::True);
            }
        }
        context
    }

    /// Classify a source as project-compiled or external library code.
    pub fn source_origin(&self, path: &Path) -> SourceOrigin {
        if self.project_file.is_none() {
            return SourceOrigin::Library;
        }
        let under_project_root = self
            .project_source_roots
            .iter()
            .any(|root| path_starts_with_ci(path, root));
        if under_project_root {
            return SourceOrigin::ProjectCompiled;
        }

        if self
            .main_source
            .as_deref()
            .is_some_and(|main| project_paths_equal(main, path))
        {
            return SourceOrigin::ProjectCompiled;
        }

        // Explicit references are keyed by unit name, which need not match the
        // file name (`uses Foo in 'Bar.pas'`), so match them by path.
        let is_explicit = self
            .explicit_unit_entries
            .values()
            .flatten()
            .any(|entry| project_paths_equal(&entry.path, path))
            || self
                .explicit_units
                .values()
                .flatten()
                .any(|unit| project_paths_equal(unit, path));
        if is_explicit {
            SourceOrigin::ProjectCompiled
        } else {
            SourceOrigin::Library
        }
    }

    /// Return the source-specific context without changing the compatibility
    /// behavior of `effective_conditional_context` for older call sites.
    pub fn conditional_context_for(&self, path: &Path) -> ConditionalContext {
        if self.project_file.is_none() {
            return self.effective_conditional_context();
        }
        match self.source_origin(path) {
            SourceOrigin::ProjectCompiled if self.conditional_closure.closed => self
                .effective_conditional_context()
                .with_absent_define(ConditionalFact::False),
            SourceOrigin::ProjectCompiled => self.effective_conditional_context(),
            SourceOrigin::Library => self.library_conditional_context.clone(),
        }
    }

    /// Discover the nearest safe project context for `file`.
    ///
    /// Discovery only examines directory entries in ancestor directories; it
    /// never recursively scans a workspace. If project selection is
    /// ambiguous, a projectless context with a warning is returned instead of
    /// guessing.
    pub fn discover(
        file: &Path,
        workspace_roots: &[PathBuf],
        options: &ProjectOptions,
    ) -> Result<Self, String> {
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

    /// Discover a project context using an explicitly captured override
    /// session. Injected sessions never consult the process environment.
    pub fn discover_with_overrides(
        file: &Path,
        workspace_roots: &[PathBuf],
        options: &ProjectOptions,
        overrides: &OverrideSession,
    ) -> Result<Self, String> {
        discover_context_with_selections(
            file,
            workspace_roots,
            options,
            &ProjectSelections::new(),
            overrides,
            Vec::new(),
            &[],
            None,
            None,
            &[],
        )
        .map(|discovery| discovery.context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_analysis_inputs_ignores_only_recorded_observations() {
        let base = ProjectContext {
            project_file: Some("A.dproj".into()),
            defines: vec!["DEBUG".to_string()],
            ..ProjectContext::default()
        };
        let observed = ProjectContext {
            metadata_files: vec!["Unit.pas".into()],
            metadata_observations: vec![MetadataObservation::Stat {
                path: "Unit.pas".into(),
            }],
            warnings: vec!["warning".to_string()],
            ..base.clone()
        };
        assert!(base.same_analysis_inputs(&observed));
        assert!(observed.same_analysis_inputs(&base));

        let other_defines = ProjectContext {
            defines: vec!["RELEASE".to_string()],
            ..observed.clone()
        };
        let other_includes = ProjectContext {
            include_paths: vec!["inc".into()],
            ..observed
        };
        assert!(!base.same_analysis_inputs(&other_defines));
        assert!(!base.same_analysis_inputs(&other_includes));
    }
}
