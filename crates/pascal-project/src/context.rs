use super::assembly::{path_starts_with_ci, paths_from_entries};
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

/// The session choices that apply to one project file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub(crate) struct SessionSelections {
    pub(crate) installation: Option<String>,
    pub(crate) build: Option<BuildChoice>,
}

impl ProjectOptions {
    /// Look up this project's session choices. Construction and the context
    /// cache key both use this, so they cannot disagree.
    pub(crate) fn session_selections_for(&self, project_file: &Path) -> SessionSelections {
        SessionSelections {
            installation: selection_for_project(&self.installation_selections, project_file)
                .cloned(),
            build: selection_for_project(&self.build_selections, project_file).cloned(),
        }
    }
}

/// An exact key wins; otherwise the smallest equivalent key by bytes, so the
/// result never depends on `HashMap` iteration order.
fn selection_for_project<'a, T>(
    selections: &'a HashMap<PathBuf, T>,
    project_file: &Path,
) -> Option<&'a T> {
    selections.get(project_file).or_else(|| {
        selections
            .iter()
            .filter(|(path, _)| project_paths_equal(path, project_file))
            .min_by(|left, right| left.0.as_os_str().cmp(right.0.as_os_str()))
            .map(|(_, value)| value)
    })
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
///
/// Other crates read a context through accessors and cannot change it:
///
/// ```compile_fail,E0616
/// let mut context = pascal_project::ProjectContext::default();
/// context.discovery_complete = true;
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectContext {
    /// Whether project selection and the metadata needed for binding completed
    /// without an ambiguity or an unresolved project-selection fallback.
    pub(crate) discovery_complete: bool,
    pub(crate) binding_metadata_complete: bool,
    pub(crate) path_issues: Vec<ProjectPathIssue>,
    pub(crate) project_file: Option<PathBuf>,
    /// Direct root-project identity facts used before installation-dependent
    /// imports are evaluated.
    pub(crate) installation_evidence: InstallationEvidence,
    /// The project-scoped installation result, if profile selection applies.
    pub(crate) installation_selection: Option<InstallationSelection>,
    /// Ordered project and caller-provided unit search paths, tagged with their
    /// source provenance. This keeps legacy native roots distinct from
    /// configured and mapped roots.
    pub(crate) search_path_entries: Vec<ProjectPathEntry>,
    /// Source-navigation-only fallbacks from the selected IDE installation.
    pub(crate) browsing_path_entries: Vec<ProjectPathEntry>,
    /// Selected IDE debug DCU locations, kept out of source lookup.
    pub(crate) debug_dcu_path_entries: Vec<ProjectPathEntry>,
    /// MainSource and explicit references retain the provenance of the path
    /// expression that produced them so membership cannot bypass mapped-root
    /// safety checks.
    pub(crate) main_source_entry: Option<ProjectPathEntry>,
    pub(crate) explicit_unit_entries: HashMap<String, Vec<ProjectPathEntry>>,
    /// Ordered project-relative include paths from DCC_IncludePath.
    ///
    /// Include paths are kept separate from unit search paths because include
    /// lookup must not make an arbitrary directory a Pascal unit candidate.
    /// Include paths retain their source provenance for read authorization.
    pub(crate) include_path_entries: Vec<ProjectPathEntry>,
    pub(crate) unit_namespaces: Vec<String>,
    pub(crate) unit_aliases: HashMap<String, String>,
    /// Effective compiler/version/options/constants used by source analysis.
    pub(crate) conditional_context: ConditionalContext,
    /// Whether project sources can close their define set and resolved RTL constants.
    pub(crate) conditional_closure: ConditionalClosure,
    /// Caller facts and compiler/RTL predefined facts for library sources.
    pub(crate) library_conditional_context: ConditionalContext,
    /// Roots whose sources the project compiles with its own defines: the
    /// project directory, unit search paths, caller source paths, and IDE
    /// library-path entries outside the selected installation's directories.
    pub(crate) project_source_roots: Vec<PathBuf>,
    pub(crate) config: Option<String>,
    pub(crate) platform: Option<String>,
    /// The selected build configuration and the `.dproj` configuration choices.
    pub(crate) config_selection: BuildSelection,
    /// The selected target platform and the `.dproj` platform choices.
    pub(crate) platform_selection: BuildSelection,
    /// The immutable Delphi override snapshot used to evaluate this context.
    pub(crate) overrides: EffectiveOverrides,
    /// The requester-scoped read policy used while evaluating this context.
    pub(crate) read_policy: ReadPolicy,
    /// Ordered, case-insensitively unique package names from DCC_UsePackage.
    /// Package exports are resolved lazily and are not merged into the unit
    /// search paths or the project-wide unit index.
    pub(crate) packages: Vec<String>,
    /// Exact selected installation environment and imported IDE XML files
    /// consumed while constructing this context.
    pub(crate) installation_config_files: Vec<PathBuf>,
    pub(crate) override_error: Option<String>,
}

impl ProjectContext {
    pub fn discovery_complete(&self) -> bool {
        self.discovery_complete
    }

    pub fn binding_metadata_complete(&self) -> bool {
        self.binding_metadata_complete
    }

    pub fn path_issues(&self) -> &[ProjectPathIssue] {
        &self.path_issues
    }

    pub fn project_file(&self) -> Option<&Path> {
        self.project_file.as_deref()
    }

    pub fn installation_evidence(&self) -> &InstallationEvidence {
        &self.installation_evidence
    }

    pub fn installation_selection(&self) -> Option<&InstallationSelection> {
        self.installation_selection.as_ref()
    }

    pub fn search_path_entries(&self) -> &[ProjectPathEntry] {
        &self.search_path_entries
    }

    pub fn browsing_path_entries(&self) -> &[ProjectPathEntry] {
        &self.browsing_path_entries
    }

    pub fn debug_dcu_path_entries(&self) -> &[ProjectPathEntry] {
        &self.debug_dcu_path_entries
    }

    pub fn main_source_entry(&self) -> Option<&ProjectPathEntry> {
        self.main_source_entry.as_ref()
    }

    pub fn explicit_unit_entries(&self) -> &HashMap<String, Vec<ProjectPathEntry>> {
        &self.explicit_unit_entries
    }

    pub fn include_path_entries(&self) -> &[ProjectPathEntry] {
        &self.include_path_entries
    }

    pub fn unit_namespaces(&self) -> &[String] {
        &self.unit_namespaces
    }

    pub fn unit_aliases(&self) -> &HashMap<String, String> {
        &self.unit_aliases
    }

    pub fn conditional_context(&self) -> &ConditionalContext {
        &self.conditional_context
    }

    pub fn conditional_closure(&self) -> &ConditionalClosure {
        &self.conditional_closure
    }

    pub fn library_conditional_context(&self) -> &ConditionalContext {
        &self.library_conditional_context
    }

    pub fn project_source_roots(&self) -> &[PathBuf] {
        &self.project_source_roots
    }

    pub fn config(&self) -> Option<&str> {
        self.config.as_deref()
    }

    pub fn platform(&self) -> Option<&str> {
        self.platform.as_deref()
    }

    pub fn config_selection(&self) -> &BuildSelection {
        &self.config_selection
    }

    pub fn platform_selection(&self) -> &BuildSelection {
        &self.platform_selection
    }

    pub fn overrides(&self) -> &EffectiveOverrides {
        &self.overrides
    }

    pub fn read_policy(&self) -> &ReadPolicy {
        &self.read_policy
    }

    pub fn packages(&self) -> &[String] {
        &self.packages
    }

    pub fn installation_config_files(&self) -> &[PathBuf] {
        &self.installation_config_files
    }

    pub fn override_error(&self) -> Option<&str> {
        self.override_error.as_deref()
    }

    /// Equality that ignores `LegacyNative` search roots, which rename treats
    /// as incidental for contexts without a project file.
    pub fn eq_ignoring_legacy_search_roots(&self, other: &Self) -> bool {
        fn without_legacy_roots(context: &ProjectContext) -> ProjectContext {
            let mut context = context.clone();
            context
                .search_path_entries
                .retain(|entry| !matches!(entry.provenance, ProjectPathProvenance::LegacyNative));
            context
        }
        without_legacy_roots(self) == without_legacy_roots(other)
    }

    /// Ordered unit search paths, without provenance.
    pub fn search_paths(&self) -> Vec<PathBuf> {
        paths_from_entries(&self.search_path_entries)
    }

    /// Ordered include paths, without provenance.
    pub fn include_paths(&self) -> Vec<PathBuf> {
        paths_from_entries(&self.include_path_entries)
    }

    /// Explicit unit references by unit name, without provenance.
    pub fn explicit_units(&self) -> HashMap<String, Vec<PathBuf>> {
        self.explicit_unit_entries
            .iter()
            .map(|(name, entries)| (name.clone(), paths_from_entries(entries)))
            .collect()
    }

    pub fn main_source(&self) -> Option<&Path> {
        self.main_source_entry
            .as_ref()
            .map(|entry| entry.path.as_path())
    }

    /// Defines that are True in the effective conditional context, sorted.
    pub fn defines(&self) -> Vec<String> {
        self.conditional_context
            .defines
            .iter()
            .filter(|(_, fact)| **fact == ConditionalFact::True)
            .map(|(name, _)| name.clone())
            .collect()
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
        for entry in &self.include_path_entries {
            entry.visit_recovery_payload(visit)?;
        }
        for namespace in &self.unit_namespaces {
            visit(namespace.len())?;
        }
        for (alias, unit) in &self.unit_aliases {
            visit(alias.len())?;
            visit(unit.len())?;
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
        let include_paths = self.include_path_entries.clone();
        let search_paths = self.search_path_entries.clone();
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

    /// Return the effective conditional context, with absent defines unknown.
    pub fn effective_conditional_context(&self) -> ConditionalContext {
        let mut context = self.conditional_context.clone();
        context.absent_define = ConditionalFact::Unknown;
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
            .main_source()
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
            .any(|entry| project_paths_equal(&entry.path, path));
        if is_explicit {
            SourceOrigin::ProjectCompiled
        } else {
            SourceOrigin::Library
        }
    }

    /// Return the conditional context for the source at `path`. Library sources
    /// get the library context; project-compiled sources get the effective
    /// context, with absent defines treated as false when the define set is
    /// closed.
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
    ) -> Result<DiscoveredProject, String> {
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
        .map(|discovery| DiscoveredProject {
            context: discovery.context,
            observations: discovery.context_observations,
        })
    }

    /// Discover a project context using an explicitly captured override
    /// session. Injected sessions never consult the process environment.
    pub fn discover_with_overrides(
        file: &Path,
        workspace_roots: &[PathBuf],
        options: &ProjectOptions,
        overrides: &OverrideSession,
    ) -> Result<DiscoveredProject, String> {
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
        .map(|discovery| DiscoveredProject {
            context: discovery.context,
            observations: discovery.context_observations,
        })
    }
}

/// The stored fields of a [`ProjectContext`], for tests that build or adjust
/// one directly. Production contexts come from discovery.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectContextParts {
    pub discovery_complete: bool,
    pub binding_metadata_complete: bool,
    pub path_issues: Vec<ProjectPathIssue>,
    pub project_file: Option<PathBuf>,
    pub installation_evidence: InstallationEvidence,
    pub installation_selection: Option<InstallationSelection>,
    pub search_path_entries: Vec<ProjectPathEntry>,
    pub browsing_path_entries: Vec<ProjectPathEntry>,
    pub debug_dcu_path_entries: Vec<ProjectPathEntry>,
    pub main_source_entry: Option<ProjectPathEntry>,
    pub explicit_unit_entries: HashMap<String, Vec<ProjectPathEntry>>,
    pub include_path_entries: Vec<ProjectPathEntry>,
    pub unit_namespaces: Vec<String>,
    pub unit_aliases: HashMap<String, String>,
    pub conditional_context: ConditionalContext,
    pub conditional_closure: ConditionalClosure,
    pub library_conditional_context: ConditionalContext,
    pub project_source_roots: Vec<PathBuf>,
    pub config: Option<String>,
    pub platform: Option<String>,
    pub config_selection: BuildSelection,
    pub platform_selection: BuildSelection,
    pub overrides: EffectiveOverrides,
    pub read_policy: ReadPolicy,
    pub packages: Vec<String>,
    pub installation_config_files: Vec<PathBuf>,
    pub override_error: Option<String>,
}

#[cfg(any(test, feature = "test-support"))]
impl ProjectContext {
    pub fn from_parts(parts: ProjectContextParts) -> Self {
        let ProjectContextParts {
            discovery_complete,
            binding_metadata_complete,
            path_issues,
            project_file,
            installation_evidence,
            installation_selection,
            search_path_entries,
            browsing_path_entries,
            debug_dcu_path_entries,
            main_source_entry,
            explicit_unit_entries,
            include_path_entries,
            unit_namespaces,
            unit_aliases,
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
            installation_config_files,
            override_error,
        } = parts;
        Self {
            discovery_complete,
            binding_metadata_complete,
            path_issues,
            project_file,
            installation_evidence,
            installation_selection,
            search_path_entries,
            browsing_path_entries,
            debug_dcu_path_entries,
            main_source_entry,
            explicit_unit_entries,
            include_path_entries,
            unit_namespaces,
            unit_aliases,
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
            installation_config_files,
            override_error,
        }
    }

    pub fn into_parts(self) -> ProjectContextParts {
        let Self {
            discovery_complete,
            binding_metadata_complete,
            path_issues,
            project_file,
            installation_evidence,
            installation_selection,
            search_path_entries,
            browsing_path_entries,
            debug_dcu_path_entries,
            main_source_entry,
            explicit_unit_entries,
            include_path_entries,
            unit_namespaces,
            unit_aliases,
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
            installation_config_files,
            override_error,
        } = self;
        ProjectContextParts {
            discovery_complete,
            binding_metadata_complete,
            path_issues,
            project_file,
            installation_evidence,
            installation_selection,
            search_path_entries,
            browsing_path_entries,
            debug_dcu_path_entries,
            main_source_entry,
            explicit_unit_entries,
            include_path_entries,
            unit_namespaces,
            unit_aliases,
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
            installation_config_files,
            override_error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_search_roots_can_be_ignored_in_comparison() {
        let configured = ProjectPathEntry::configured("/w/src".into());
        let with_legacy = ProjectContext {
            search_path_entries: vec![ProjectPathEntry::legacy("/w".into()), configured.clone()],
            ..ProjectContext::default()
        };
        let without_legacy = ProjectContext {
            search_path_entries: vec![configured],
            ..ProjectContext::default()
        };
        assert_ne!(with_legacy, without_legacy);
        assert!(with_legacy.eq_ignoring_legacy_search_roots(&without_legacy));
        let other = ProjectContext {
            search_path_entries: vec![ProjectPathEntry::configured("/w/other".into())],
            ..ProjectContext::default()
        };
        assert!(!with_legacy.eq_ignoring_legacy_search_roots(&other));
    }

    #[test]
    fn session_selections_prefer_the_exact_project_path() {
        let exact = PathBuf::from("/work/App.dproj");
        let dotted = PathBuf::from("/work/./App.dproj");
        let doubly_dotted = PathBuf::from("/work/././App.dproj");
        let debug = BuildChoice {
            config: Some("Debug".into()),
            ..BuildChoice::default()
        };
        let release = BuildChoice {
            config: Some("Release".into()),
            ..BuildChoice::default()
        };
        let options = ProjectOptions {
            installation_selections: [
                (dotted.clone(), "dotted".to_string()),
                (exact.clone(), "exact".to_string()),
            ]
            .into_iter()
            .collect(),
            build_selections: [
                (dotted.clone(), release.clone()),
                (exact.clone(), debug.clone()),
            ]
            .into_iter()
            .collect(),
            ..ProjectOptions::default()
        };
        let selections = options.session_selections_for(&exact);
        assert_eq!(selections.installation.as_deref(), Some("exact"));
        assert_eq!(selections.build, Some(debug));

        let variants_only = ProjectOptions {
            build_selections: [
                (dotted, BuildChoice::default()),
                (doubly_dotted, release.clone()),
            ]
            .into_iter()
            .collect(),
            ..ProjectOptions::default()
        };
        // Neither key is exact; the smaller key by bytes wins in every run.
        // "/work/././App.dproj" < "/work/./App.dproj" because '.' < 'A'.
        for _ in 0..16 {
            assert_eq!(
                variants_only.session_selections_for(&exact).build,
                Some(release.clone())
            );
        }
        assert_eq!(
            ProjectOptions::default().session_selections_for(&exact),
            SessionSelections::default()
        );
    }

    #[test]
    fn equality_is_by_analysis_inputs_and_observations_stay_outside() {
        let base = ProjectContext {
            project_file: Some("A.dproj".into()),
            conditional_context: ConditionalContext::from_defines(&["DEBUG".to_string()]),
            ..ProjectContext::default()
        };
        let observed = DiscoveredProject {
            context: base.clone().into(),
            observations: ProjectObservations {
                metadata_files: vec!["Unit.pas".into()],
                metadata_observations: vec![MetadataObservation::Stat {
                    path: "Unit.pas".into(),
                }],
                warnings: vec!["warning".to_string()],
                system_pas_searches: Vec::new(),
            },
        };
        assert_eq!(base, *observed.context);
        assert_ne!(
            observed,
            DiscoveredProject {
                context: base.clone().into(),
                observations: ProjectObservations::default(),
            }
        );

        let other_defines = ProjectContext {
            conditional_context: ConditionalContext::from_defines(&["RELEASE".to_string()]),
            ..base.clone()
        };
        let other_includes = ProjectContext {
            include_path_entries: vec![ProjectPathEntry::configured("inc".into())],
            ..base.clone()
        };
        assert_ne!(base, other_defines);
        assert_ne!(base, other_includes);
    }

    #[test]
    fn bare_path_views_are_derived_from_entries() {
        let a = PathBuf::from("/w/a");
        let b = PathBuf::from("/w/b");
        let unit = PathBuf::from("/w/Unit1.pas");
        let context = ProjectContext {
            search_path_entries: vec![
                ProjectPathEntry::legacy(a.clone()),
                ProjectPathEntry::configured(a.clone()),
                ProjectPathEntry::configured(b.clone()),
            ],
            include_path_entries: vec![ProjectPathEntry::configured(b.clone())],
            main_source_entry: Some(ProjectPathEntry::configured("/w/App.dpr".into())),
            explicit_unit_entries: [(
                "Unit1".to_string(),
                vec![
                    ProjectPathEntry::legacy(unit.clone()),
                    ProjectPathEntry::configured(unit.clone()),
                ],
            )]
            .into_iter()
            .collect(),
            ..ProjectContext::default()
        };
        assert_eq!(context.search_paths(), vec![a, b.clone()]);
        assert_eq!(context.include_paths(), vec![b]);
        assert_eq!(context.main_source(), Some(Path::new("/w/App.dpr")));
        assert_eq!(context.explicit_units()["Unit1"], vec![unit]);
    }

    #[test]
    fn defines_are_the_true_facts_in_sorted_order() {
        let mut conditional = ConditionalContext::default();
        conditional.set_define("ZED", ConditionalFact::True);
        conditional.set_define("ALPHA", ConditionalFact::True);
        conditional.set_define("OFF", ConditionalFact::False);
        conditional.set_define("MAYBE", ConditionalFact::Unknown);
        let context = ProjectContext {
            conditional_context: conditional,
            ..ProjectContext::default()
        };
        assert_eq!(
            context.defines(),
            vec!["ALPHA".to_string(), "ZED".to_string()]
        );
    }
}
