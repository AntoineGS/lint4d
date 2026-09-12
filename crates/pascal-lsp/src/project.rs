//! Lazy, filesystem-only Delphi project context discovery.
//!
//! This module deliberately does not try to be an MSBuild evaluator. It reads
//! the small amount of project metadata needed by navigation, preserves
//! ambiguity, and reports anything it cannot safely interpret as a warning.

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

const MAX_PROJECT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_IMPORT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_MAIN_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PACKAGE_METADATA_BYTES: u64 = 4 * 1024 * 1024;
const MAX_IMPORT_COUNT: usize = 64;
const MAX_METADATA_FILES: usize = MAX_IMPORT_COUNT + 1;
const MAX_EXPANDED_VALUE_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_PROPERTY_BYTES: usize = 16 * 1024 * 1024;
const MAX_OWNERSHIP_CANDIDATES: usize = 32;
const MAX_OWNERSHIP_METADATA_FILES: usize = 512;
const MAX_OWNERSHIP_SOURCE_FILES: usize = 256;
const MAX_OWNERSHIP_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_OWNERSHIP_WARNING_BYTES: usize = 256 * 1024;
const UNRESOLVED_MARKER: char = '\u{1}';

/// Options supplied by the LSP client for project selection and evaluation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectOptions {
    /// An explicit `.dproj`, `.dpr`, or `.dpk` path. Relative paths are
    /// resolved against the supplied workspace roots.
    pub project_file: Option<PathBuf>,
    /// The selected Delphi build configuration, such as `Debug` or `Release`.
    pub build_config: Option<String>,
    /// The selected Delphi platform, such as `Win32` or `Win64`.
    pub platform: Option<String>,
    /// Additional ordered source roots supplied by the client.
    pub source_paths: Vec<String>,
}

pub(crate) type ProjectSelections = HashMap<PathBuf, PathBuf>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProjectCandidates {
    pub(crate) directory: Option<PathBuf>,
    pub(crate) files: Vec<PathBuf>,
}

/// Metadata used to resolve units without eagerly parsing the repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectContext {
    /// Whether project selection and the metadata needed for binding completed
    /// without an ambiguity or an unresolved project-selection fallback.
    pub discovery_complete: bool,
    pub project_file: Option<PathBuf>,
    pub main_source: Option<PathBuf>,
    /// Ordered project and client-provided unit search paths.
    pub search_paths: Vec<PathBuf>,
    /// Ordered project-relative include paths from DCC_IncludePath.
    ///
    /// Include paths are kept separate from unit search paths because include
    /// lookup must not make an arbitrary directory a Pascal unit candidate.
    pub include_paths: Vec<PathBuf>,
    pub explicit_units: HashMap<String, Vec<PathBuf>>,
    pub unit_namespaces: Vec<String>,
    pub unit_aliases: HashMap<String, String>,
    pub defines: Vec<String>,
    pub config: Option<String>,
    pub platform: Option<String>,
    /// Ordered, case-insensitively unique package names from DCC_UsePackage.
    /// Package exports are resolved lazily and are not merged into the unit
    /// search paths or the project-wide unit index.
    pub packages: Vec<String>,
    /// Project, main-source, imported option-set, and automatic-selection
    /// candidate files used to build the context. Consumers can revalidate
    /// these paths without rediscovering or reparsing unrelated source files.
    pub metadata_files: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

/// Metadata extracted from one source package descriptor. The descriptor is
/// parsed only after a project import has failed the ordinary unit lookup;
/// `metadata_files` contains every project/option-set dependency used to
/// produce the result so callers can detect changes without file events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PackageMetadata {
    pub units: HashMap<String, Vec<PathBuf>>,
    pub warnings: Vec<String>,
    pub metadata_files: Vec<PathBuf>,
}

impl ProjectContext {
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
        discover_context(file, workspace_roots, options)
    }
}

/// Convenience wrapper around [`ProjectContext::discover`].
pub fn discover(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
) -> Result<ProjectContext, String> {
    ProjectContext::discover(file, workspace_roots, options)
}

pub(crate) fn project_candidates(
    file: &Path,
    workspace_roots: &[PathBuf],
) -> Result<ProjectCandidates, String> {
    let mut warnings = Vec::new();
    let absolute_file = absolute_lexical(file)?;
    let file_path = discovery_file_path(&absolute_file, &mut warnings);
    let roots = normalize_workspace_roots(workspace_roots, &mut warnings)?;
    let relevant_root = relevant_workspace_root(&file_path, &roots);
    find_project_candidates(&file_path, relevant_root.as_deref())
}

pub(crate) fn discover_with_selections(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
    selections: &ProjectSelections,
) -> Result<ProjectContext, String> {
    discover_context_with_selections(file, workspace_roots, options, selections)
}

fn discover_context(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
) -> Result<ProjectContext, String> {
    discover_context_with_selections(file, workspace_roots, options, &ProjectSelections::new())
}

fn discover_context_with_selections(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
    selections: &ProjectSelections,
) -> Result<ProjectContext, String> {
    let absolute_file = absolute_lexical(file)?;
    let mut warnings = Vec::new();
    let file_path = discovery_file_path(&absolute_file, &mut warnings);
    let roots = normalize_workspace_roots(workspace_roots, &mut warnings)?;
    let relevant_root = relevant_workspace_root(&file_path, &roots);

    let runtime_selection = if selections.is_empty() {
        None
    } else {
        let candidates = match find_project_candidates(&file_path, relevant_root.as_deref()) {
            Ok(candidates) => candidates,
            Err(error) => {
                warnings.push(error);
                return Ok(build_standalone_context(
                    &file_path,
                    &roots,
                    options,
                    warnings,
                    false,
                    Vec::new(),
                ));
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

    let selected_project = if let Some((scope, requested, selected, candidates)) = runtime_selection
    {
        let Some(project_file) = selected else {
            warnings.push(format!(
                "selected project {} is not a current candidate in {}",
                requested.display(),
                scope.display()
            ));
            return Ok(build_standalone_context(
                &file_path, &roots, options, warnings, false, candidates,
            ));
        };
        ProjectSelection::Selected {
            path: project_file,
            explicit: true,
            metadata_files: candidates,
        }
    } else if let Some(project_file) = &options.project_file {
        explicit_project_file(project_file, &roots, &file_path, &mut warnings)
    } else {
        discover_project_file(
            &file_path,
            relevant_root.as_deref(),
            &roots,
            options,
            &mut warnings,
        )
    };

    match selected_project {
        ProjectSelection::Selected {
            path: project_file,
            explicit,
            metadata_files,
        } => build_project_context(
            project_file,
            &file_path,
            &roots,
            options,
            warnings,
            explicit,
            metadata_files,
        ),
        ProjectSelection::Standalone { metadata_files } => Ok(build_standalone_context(
            &file_path,
            &roots,
            options,
            warnings,
            true,
            metadata_files,
        )),
        ProjectSelection::Incomplete { metadata_files } => Ok(build_standalone_context(
            &file_path,
            &roots,
            options,
            warnings,
            false,
            metadata_files,
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProjectSelection {
    Selected {
        path: PathBuf,
        explicit: bool,
        metadata_files: Vec<PathBuf>,
    },
    Standalone {
        metadata_files: Vec<PathBuf>,
    },
    Incomplete {
        metadata_files: Vec<PathBuf>,
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

pub(crate) fn runtime_project_selection(
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

pub(crate) fn has_invalid_project_selection(context: &ProjectContext) -> bool {
    context.warnings.iter().any(|warning| {
        warning.starts_with("selected project ")
            && warning.contains(" is not a current candidate in ")
    })
}

pub(crate) fn selected_project_is_current(scope: &Path, selected: &Path) -> Result<bool, String> {
    let (dproj, _) = project_directory_entries(scope)?;
    Ok(dproj
        .iter()
        .any(|candidate| project_paths_equal(candidate, selected)))
}

fn find_project_candidates(
    file: &Path,
    workspace_root: Option<&Path>,
) -> Result<ProjectCandidates, String> {
    let mut directory = file.parent().map_or_else(PathBuf::new, Path::to_path_buf);
    loop {
        let (dproj, _) = project_directory_entries(&directory)?;
        if !dproj.is_empty() {
            let mut files = dproj;
            files.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
            return Ok(ProjectCandidates {
                directory: Some(directory),
                files,
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

fn project_path_starts_with(path: &Path, root: &Path) -> bool {
    let path_components = path.components().collect::<Vec<_>>();
    let root_components = root.components().collect::<Vec<_>>();
    path_components.len() >= root_components.len()
        && path_components
            .iter()
            .zip(root_components.iter())
            .all(|(path, root)| project_components_equal(*path, *root))
}

fn project_paths_equal(left: &Path, right: &Path) -> bool {
    let left_components = left.components().collect::<Vec<_>>();
    let right_components = right.components().collect::<Vec<_>>();
    left_components.len() == right_components.len()
        && left_components
            .iter()
            .zip(right_components.iter())
            .all(|(left, right)| project_components_equal(*left, *right))
}

fn project_components_equal(left: Component<'_>, right: Component<'_>) -> bool {
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

fn project_directory_entries(directory: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let entries = fs::read_dir(directory).map_err(|error| {
        format!(
            "could not inspect project directory {}: {error}",
            directory.display()
        )
    })?;
    let mut dproj = Vec::new();
    let mut dpr_or_dpk = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        if extension_is(&path, "dproj") {
            push_bounded_candidate(&mut dproj, path);
        } else if extension_is(&path, "dpr") || extension_is(&path, "dpk") {
            push_bounded_candidate(&mut dpr_or_dpk, path);
        }
    }
    Ok((dproj, dpr_or_dpk))
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
        } else if !is_windows_absolute(root) {
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
    if is_windows_absolute_text(&requested_text) {
        warnings.push(format!(
            "Windows project path is unavailable on Linux and was omitted: {}",
            requested.display()
        ));
        return ProjectSelection::Incomplete {
            metadata_files: Vec::new(),
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
            }
        }
        1 => ProjectSelection::Selected {
            path: candidates.remove(0),
            explicit: true,
            metadata_files: Vec::new(),
        },
        _ => {
            warnings.push(format!(
                "multiple explicit project files matched {}; no project selected",
                requested.display()
            ));
            ProjectSelection::Incomplete {
                metadata_files: Vec::new(),
            }
        }
    }
}

fn discover_project_file(
    file: &Path,
    workspace_root: Option<&Path>,
    roots: &[PathBuf],
    options: &ProjectOptions,
    warnings: &mut Vec<String>,
) -> ProjectSelection {
    let mut directory = file.parent().map_or_else(PathBuf::new, Path::to_path_buf);
    let mut fallback_dpr = None;
    let mut ambiguous_fallback_dpr = None;

    loop {
        let (dproj, dpr_or_dpk) = match project_directory_entries(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                warnings.push(error);
                return ProjectSelection::Incomplete {
                    metadata_files: Vec::new(),
                };
            }
        };

        if !dproj.is_empty() {
            return choose_project_candidate(
                dproj,
                &directory,
                "project files",
                file,
                roots,
                options,
                warnings,
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
            roots,
            options,
            warnings,
        );
    }
    fallback_dpr.map_or(
        ProjectSelection::Standalone {
            metadata_files: Vec::new(),
        },
        |path| ProjectSelection::Selected {
            path,
            explicit: false,
            metadata_files: Vec::new(),
        },
    )
}

fn push_bounded_candidate(candidates: &mut Vec<PathBuf>, path: PathBuf) {
    if candidates.len() <= MAX_OWNERSHIP_CANDIDATES {
        candidates.push(path);
    }
}

fn choose_project_candidate(
    mut candidates: Vec<PathBuf>,
    directory: &Path,
    kind: &str,
    file: &Path,
    roots: &[PathBuf],
    options: &ProjectOptions,
    warnings: &mut Vec<String>,
) -> ProjectSelection {
    if candidates.len() == 1 {
        return ProjectSelection::Selected {
            path: candidates.pop().expect("one candidate"),
            explicit: false,
            metadata_files: Vec::new(),
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
        return ProjectSelection::Incomplete { metadata_files };
    }

    // Ownership checks stay within the already discovered candidate
    // directory. They read each candidate's bounded metadata, but never turn
    // project selection into a recursive workspace scan.
    let mut budget = OwnershipProbeBudget::default();
    let mut consulted_metadata = Vec::new();
    let mut owned = Vec::new();
    let mut incomplete = false;
    let mut candidate_warnings = Vec::new();
    for candidate in &candidates {
        let evaluation = inspect_project_candidate(candidate, file, roots, options, &mut budget);
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
        match evaluation.ownership {
            CandidateOwnership::Owned => owned.push(candidate.clone()),
            CandidateOwnership::NotOwned => {}
            CandidateOwnership::Incomplete => incomplete = true,
        }
        if budget.exhausted || incomplete || owned.len() >= 2 {
            break;
        }
    }

    if owned.len() == 1 && !incomplete && !budget.exhausted {
        return ProjectSelection::Selected {
            path: owned.pop().expect("one owned candidate"),
            explicit: false,
            metadata_files: consulted_metadata,
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
    warnings: Vec<String>,
}

fn inspect_project_candidate(
    project_file: &Path,
    file: &Path,
    roots: &[PathBuf],
    options: &ProjectOptions,
    budget: &mut OwnershipProbeBudget,
) -> CandidateEvaluation {
    let mut metadata_files = vec![project_file.to_path_buf()];
    match build_project_context(
        project_file.to_path_buf(),
        file,
        roots,
        options,
        Vec::new(),
        false,
        Vec::new(),
    ) {
        Ok(context) => {
            for metadata_file in &context.metadata_files {
                add_unique_path(&mut metadata_files, metadata_file.clone());
            }
            if !context.discovery_complete {
                return CandidateEvaluation {
                    ownership: CandidateOwnership::Incomplete,
                    metadata_files,
                    warnings: context.warnings,
                };
            }
            let membership = inspect_source_membership(&context, budget);
            for metadata_file in &membership.metadata_files {
                add_unique_path(&mut metadata_files, metadata_file.clone());
            }
            let mut ownership_paths = membership.source_files;
            if let Some(main_source) = &context.main_source {
                ownership_paths.push(main_source.clone());
            }
            ownership_paths.extend(context.explicit_units.values().flatten().cloned());
            let (owns_source, identity_unverified) = source_ownership(&ownership_paths, file);
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
                warnings,
            }
        }
        Err(error) => CandidateEvaluation {
            ownership: CandidateOwnership::Incomplete,
            metadata_files,
            warnings: vec![format!(
                "could not inspect project candidate {}: {error}",
                project_file.display()
            )],
        },
    }
}

#[derive(Debug, Default)]
struct SourceMembershipInspection {
    complete: bool,
    source_files: Vec<PathBuf>,
    metadata_files: Vec<PathBuf>,
    warnings: Vec<String>,
}

fn inspect_source_membership(
    context: &ProjectContext,
    budget: &mut OwnershipProbeBudget,
) -> SourceMembershipInspection {
    let mut inspection = SourceMembershipInspection {
        complete: true,
        ..SourceMembershipInspection::default()
    };
    let mut pending = Vec::new();
    if let Some(main_source) = &context.main_source {
        pending.push(main_source.clone());
    }
    pending.extend(context.explicit_units.values().flatten().cloned());
    let mut queued = HashSet::new();
    let mut cursor = 0;

    while let Some(source_path) = pending.get(cursor).cloned() {
        cursor += 1;
        if !queued.insert(source_path.clone()) {
            continue;
        }
        add_unique_path(&mut inspection.source_files, source_path.clone());
        add_unique_path(&mut inspection.metadata_files, source_path.clone());

        let size = match fs::metadata(&source_path) {
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
        if size > MAX_MAIN_SOURCE_BYTES {
            inspection.complete = false;
            inspection.warnings.push(format!(
                "source membership file {} exceeds the {} byte safety limit",
                source_path.display(),
                MAX_MAIN_SOURCE_BYTES
            ));
            continue;
        }
        if !budget.reserve_source_file(size) {
            inspection.complete = false;
            inspection.warnings.push(format!(
                "automatic source membership probe limit reached at {}",
                source_path.display()
            ));
            break;
        }
        let contents = match fs::read(&source_path)
            .map_err(|error| format!("could not read file: {error}"))
            .and_then(|bytes| {
                String::from_utf8(bytes).map_err(|error| format!("file is not UTF-8: {error}"))
            }) {
            Ok(contents) => contents,
            Err(error) => {
                inspection.complete = false;
                inspection.warnings.push(format!(
                    "could not read source membership file {}: {error}",
                    source_path.display()
                ));
                continue;
            }
        };
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
            if is_compiled_reference(&raw_path) {
                continue;
            }
            let Some(path) = resolve_project_path(
                &raw_path,
                base,
                &mut inspection.warnings,
                "ownership source membership",
                true,
            ) else {
                inspection.complete = false;
                continue;
            };
            add_unique_path(&mut inspection.metadata_files, path.clone());
            if !queued.contains(&path) {
                pending.push(path);
            }
        }
    }
    inspection
}

fn source_ownership(paths: &[PathBuf], target: &Path) -> (bool, bool) {
    if filesystem_identity_unverified(target)
        || paths
            .iter()
            .any(|path| filesystem_identity_unverified(path))
    {
        return (false, true);
    }
    let Some(target_identity) = fs::canonicalize(target).ok() else {
        return (false, true);
    };
    let mut owns_source = false;
    let mut identity_unverified = false;
    for path in paths {
        let Some(identity) = fs::canonicalize(path).ok() else {
            identity_unverified = true;
            continue;
        };
        let matches = if cfg!(windows) {
            paths_equal_ci(&identity, &target_identity)
        } else {
            identity == target_identity
        };
        owns_source |= matches;
    }
    (owns_source, identity_unverified)
}

fn filesystem_identity_unverified(path: &Path) -> bool {
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

fn relevant_workspace_root(file: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    roots
        .iter()
        .filter(|root| path_starts_with_ci(file, root))
        .max_by_key(|root| root.components().count())
        .cloned()
}

fn path_starts_with_ci(path: &Path, root: &Path) -> bool {
    let path_components: Vec<String> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect();
    let root_components: Vec<String> = root
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect();
    path_components.len() >= root_components.len()
        && path_components
            .iter()
            .zip(root_components.iter())
            .all(|(path, root)| path.eq_ignore_ascii_case(root))
}

fn fallback_main_source(project_file: &Path, warnings: &mut Vec<String>) -> Option<PathBuf> {
    let stem = project_file.file_stem()?;
    let directory = project_file.parent()?;
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            warnings.push(format!(
                "could not inspect {} for its main source: {error}",
                directory.display()
            ));
            return None;
        }
    };
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_file())
            || !(extension_is(&path, "dpr") || extension_is(&path, "dpk"))
            || !path.file_stem().is_some_and(|candidate| {
                candidate
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&stem.to_string_lossy())
            })
        {
            continue;
        }
        candidates.push(path);
    }
    match candidates.len() {
        0 => None,
        1 => candidates.pop(),
        _ => {
            warnings.push(format!(
                "multiple main sources match {}; no main source selected",
                project_file.display()
            ));
            None
        }
    }
}

fn build_project_context(
    project_file: PathBuf,
    file: &Path,
    roots: &[PathBuf],
    options: &ProjectOptions,
    warnings: Vec<String>,
    explicit: bool,
    consulted_metadata_files: Vec<PathBuf>,
) -> Result<ProjectContext, String> {
    let project_dir = project_file
        .parent()
        .map_or_else(PathBuf::new, Path::to_path_buf);
    let mut builder = ProjectBuilder::new(options, warnings, project_dir.clone());
    let project_is_dproj = extension_is(&project_file, "dproj");
    if project_is_dproj {
        builder.process_root_dproj(&project_file)?;
    }

    let main_source = if project_is_dproj {
        let main_name = builder.property("mainsource");
        match main_name {
            Some(name) if !name.is_empty() && !name.contains(UNRESOLVED_MARKER) => {
                resolve_project_path(
                    &name,
                    &project_dir,
                    &mut builder.warnings,
                    "main source",
                    true,
                )
            }
            Some(_) => {
                builder.warnings.push(format!(
                    "main source contains an unavailable property path in {}",
                    project_file.display()
                ));
                None
            }
            None => fallback_main_source(&project_file, &mut builder.warnings),
        }
    } else {
        Some(project_file.clone())
    };
    if project_is_dproj && main_source.is_none() {
        builder.warnings.push(format!(
            "project has no resolvable MainSource: {}",
            project_file.display()
        ));
    }

    let mut search_paths = Vec::new();
    add_unique_path(&mut search_paths, project_dir.clone());
    if let Some(search_path) = builder.property("dcc_unitsearchpath") {
        for item in search_path.split(';') {
            add_resolved_search_path(
                item,
                &project_dir,
                &mut search_paths,
                &mut builder.warnings,
                "DCC_UnitSearchPath",
            );
        }
    }

    let mut include_paths = Vec::new();
    if let Some(include_path) = builder.property("dcc_includepath") {
        for item in include_path.split(';') {
            add_resolved_search_path(
                item,
                &project_dir,
                &mut include_paths,
                &mut builder.warnings,
                "DCC_IncludePath",
            );
        }
    }

    let option_base = relevant_workspace_root(file, roots)
        .or_else(|| file.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| project_dir.clone());
    for item in &options.source_paths {
        add_resolved_search_path(
            item,
            &option_base,
            &mut search_paths,
            &mut builder.warnings,
            "configured source path",
        );
    }

    let mut explicit_units = HashMap::new();
    if let Some(main_source) = &main_source {
        add_explicit_units_from_source(main_source, &mut explicit_units, &mut builder.warnings);
    }
    for reference in &builder.references {
        if is_compiled_reference(&reference.include) {
            continue;
        }
        let expanded = expand_value(
            &reference.include,
            "",
            &builder.properties,
            &builder.unknown_properties,
            &mut builder.warnings,
            &reference.source_file,
        );
        if expanded.unknown || expanded.value.contains(UNRESOLVED_MARKER) {
            continue;
        }
        if is_compiled_reference(&expanded.value) {
            continue;
        }
        let Some(path) = resolve_project_path(
            &expanded.value,
            &builder.project_dir,
            &mut builder.warnings,
            "DCCReference",
            true,
        ) else {
            continue;
        };
        let Some(stem) = path.file_stem() else {
            continue;
        };
        let name = canonical_unit_name(&stem.to_string_lossy());
        if !name.is_empty() {
            add_unit_candidate(&mut explicit_units, name, path);
        }
    }

    let mut metadata_files = builder.metadata_files.clone();
    if !metadata_files.iter().any(|path| path == &project_file) {
        metadata_files.push(project_file.clone());
    }
    if let Some(main_source) = &main_source {
        if !metadata_files.iter().any(|path| path == main_source) {
            metadata_files.push(main_source.clone());
        }
    }
    for metadata_file in consulted_metadata_files {
        if !metadata_files.iter().any(|path| path == &metadata_file) {
            metadata_files.push(metadata_file);
        }
    }

    Ok(ProjectContext {
        discovery_complete: !builder.incomplete
            && !project_context_warnings_incomplete(&builder.warnings, explicit),
        project_file: Some(project_file),
        main_source,
        search_paths,
        include_paths,
        explicit_units,
        unit_namespaces: property_list(&builder, "dcc_namespace"),
        unit_aliases: parse_aliases(builder.property("dcc_unitalias").as_deref()),
        defines: property_list(&builder, "dcc_define"),
        config: selected_config(&builder, options),
        platform: selected_platform(&builder, options),
        packages: package_list(&builder),
        metadata_files,
        warnings: builder.warnings,
    })
}

fn build_standalone_context(
    file: &Path,
    roots: &[PathBuf],
    options: &ProjectOptions,
    mut warnings: Vec<String>,
    discovery_complete: bool,
    metadata_files: Vec<PathBuf>,
) -> ProjectContext {
    let mut search_paths = Vec::new();
    if let Some(parent) = file.parent() {
        if let Some(actual) = resolve_existing_path(parent, &mut warnings, "source directory") {
            add_unique_path(&mut search_paths, actual);
        }
    }
    if let Some(root) = relevant_workspace_root(file, roots) {
        if root.is_dir() {
            add_unique_path(&mut search_paths, root);
        }
    }
    let option_base = relevant_workspace_root(file, roots)
        .or_else(|| file.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    for item in &options.source_paths {
        add_resolved_search_path(
            item,
            &option_base,
            &mut search_paths,
            &mut warnings,
            "configured source path",
        );
    }

    ProjectContext {
        discovery_complete,
        project_file: None,
        main_source: None,
        search_paths,
        include_paths: Vec::new(),
        explicit_units: HashMap::new(),
        unit_namespaces: Vec::new(),
        unit_aliases: HashMap::new(),
        defines: Vec::new(),
        config: options.build_config.clone(),
        platform: options.platform.clone(),
        packages: Vec::new(),
        metadata_files,
        warnings,
    }
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

fn selected_config(builder: &ProjectBuilder, options: &ProjectOptions) -> Option<String> {
    builder
        .property("config")
        .filter(|value| !value.is_empty() && !value.contains(UNRESOLVED_MARKER))
        .or_else(|| options.build_config.clone())
}

fn selected_platform(builder: &ProjectBuilder, options: &ProjectOptions) -> Option<String> {
    builder
        .property("platform")
        .filter(|value| !value.is_empty() && !value.contains(UNRESOLVED_MARKER))
        .or_else(|| builder.property("dcc_platform"))
        .filter(|value| !value.is_empty() && !value.contains(UNRESOLVED_MARKER))
        .or_else(|| options.platform.clone())
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

fn add_resolved_search_path(
    raw: &str,
    base: &Path,
    search_paths: &mut Vec<PathBuf>,
    warnings: &mut Vec<String>,
    kind: &str,
) {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains(UNRESOLVED_MARKER) {
        return;
    }
    if is_windows_absolute_text(raw) {
        warnings.push(format!(
            "Windows path in {kind} is unavailable on Linux and was omitted: {raw}"
        ));
        return;
    }
    if let Some(path) = resolve_project_path(raw, base, warnings, kind, false) {
        add_unique_path(search_paths, path);
        return;
    }
    let normalized = raw.replace('\\', "/");
    let raw_path = Path::new(&normalized);
    let candidate = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        base.join(raw_path)
    };
    let candidate = lexical_normalize(&candidate);
    warnings.push(format!(
        "{kind} path does not exist yet; retaining it for lazy discovery: {}",
        candidate.display()
    ));
    add_unique_path(search_paths, candidate);
}

fn add_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn add_metadata_file(
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

fn paths_equal_ci(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

fn add_unit_candidate(units: &mut HashMap<String, Vec<PathBuf>>, name: String, path: PathBuf) {
    let candidates = units.entry(name).or_default();
    if !candidates.iter().any(|existing| existing == &path) {
        candidates.push(path);
    }
}

fn add_explicit_units_from_source(
    source_path: &Path,
    units: &mut HashMap<String, Vec<PathBuf>>,
    warnings: &mut Vec<String>,
) {
    let contents = match read_bounded(source_path, MAX_MAIN_SOURCE_BYTES) {
        Ok(contents) => contents,
        Err(error) => {
            warnings.push(format!(
                "could not read main source {} for explicit unit paths: {error}",
                source_path.display()
            ));
            return;
        }
    };
    let Some(base) = source_path.parent() else {
        return;
    };
    for (unit_name, raw_path) in parse_explicit_unit_paths(&contents) {
        let Some(path) = resolve_project_path(
            &raw_path,
            base,
            warnings,
            "explicit DPR/DPK unit path",
            true,
        ) else {
            continue;
        };
        add_unit_candidate(units, canonical_unit_name(&unit_name), path);
    }
}

fn canonical_unit_name(name: &str) -> String {
    name.trim().trim_matches('.').to_ascii_lowercase()
}

fn canonical_package_name(name: &str) -> String {
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

fn resolve_project_path(
    raw: &str,
    base: &Path,
    warnings: &mut Vec<String>,
    kind: &str,
    warn_missing: bool,
) -> Option<PathBuf> {
    let candidate = project_path_candidate(raw, base, warnings, kind)?;
    match resolve_existing_path_status(&candidate, warnings, kind) {
        ExistingPathStatus::Found(path) => Some(path),
        ExistingPathStatus::Missing if warn_missing => {
            warnings.push(format!(
                "{kind} path does not exist and was omitted: {}",
                candidate.display()
            ));
            None
        }
        ExistingPathStatus::Missing | ExistingPathStatus::Unresolvable => None,
    }
}

fn project_path_candidate(
    raw: &str,
    base: &Path,
    warnings: &mut Vec<String>,
    kind: &str,
) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains(UNRESOLVED_MARKER) {
        return None;
    }
    if is_windows_absolute_text(raw) {
        warnings.push(format!(
            "Windows path in {kind} is unavailable on Linux and was omitted: {raw}"
        ));
        return None;
    }
    let normalized = raw.replace('\\', "/");
    let path = Path::new(&normalized);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    Some(lexical_normalize(&candidate))
}

#[derive(Debug)]
enum ExistingPathStatus {
    Found(PathBuf),
    Missing,
    Unresolvable,
}

fn absolute_lexical(path: &Path) -> Result<PathBuf, String> {
    if is_windows_absolute(path) || path.is_absolute() {
        Ok(lexical_normalize(path))
    } else {
        let current = std::env::current_dir()
            .map_err(|error| format!("could not determine current directory: {error}"))?;
        Ok(lexical_normalize(&current.join(path)))
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(std::path::MAIN_SEPARATOR.to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() && !path.is_absolute() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    normalized
}

fn resolve_existing_path(path: &Path, warnings: &mut Vec<String>, kind: &str) -> Option<PathBuf> {
    match resolve_existing_path_status(path, warnings, kind) {
        ExistingPathStatus::Found(path) => Some(path),
        ExistingPathStatus::Missing | ExistingPathStatus::Unresolvable => None,
    }
}

fn resolve_existing_path_status(
    path: &Path,
    warnings: &mut Vec<String>,
    kind: &str,
) -> ExistingPathStatus {
    if is_windows_absolute(path) {
        warnings.push(format!(
            "Windows path in {kind} is unavailable on Linux and was omitted: {}",
            path.display()
        ));
        return ExistingPathStatus::Unresolvable;
    }
    let absolute = if path.is_absolute() {
        lexical_normalize(path)
    } else {
        let Ok(current) = std::env::current_dir() else {
            return ExistingPathStatus::Unresolvable;
        };
        lexical_normalize(&current.join(path))
    };
    let mut current = PathBuf::from(std::path::MAIN_SEPARATOR.to_string());
    for component in absolute.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        let wanted = component.to_string_lossy();
        let mut matches = Vec::new();
        let Ok(entries) = fs::read_dir(&current) else {
            return ExistingPathStatus::Missing;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().eq_ignore_ascii_case(&wanted) {
                matches.push(entry.path());
            }
        }
        if let Some(exact) = matches
            .iter()
            .find(|candidate| candidate.file_name().is_some_and(|name| name == component))
        {
            current = exact.clone();
            continue;
        }
        if matches.len() > 1 {
            warnings.push(format!(
                "ambiguous case-insensitive {kind} path component {wanted:?} under {}",
                current.display()
            ));
            return ExistingPathStatus::Unresolvable;
        }
        let Some(next) = matches.into_iter().next() else {
            return ExistingPathStatus::Missing;
        };
        current = next;
    }
    ExistingPathStatus::Found(current)
}

fn is_windows_absolute(path: &Path) -> bool {
    is_windows_absolute_text(&path.to_string_lossy())
}

fn is_windows_absolute_text(path: &str) -> bool {
    let bytes = path.as_bytes();
    (bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic())
        || path.starts_with("\\\\")
}

fn extension_is(path: &Path, extension: &str) -> bool {
    path.extension()
        .is_some_and(|value| value.to_string_lossy().eq_ignore_ascii_case(extension))
}

fn is_compiled_reference(raw: &str) -> bool {
    raw.trim()
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.').map(|(_, extension)| extension))
        .is_some_and(|extension| extension.eq_ignore_ascii_case("dcp"))
}

fn read_bounded(path: &Path, limit: u64) -> Result<String, String> {
    let metadata = fs::metadata(path).map_err(|error| format!("could not stat file: {error}"))?;
    if metadata.len() > limit {
        return Err(format!(
            "file is {} bytes, exceeding the {} byte safety limit",
            metadata.len(),
            limit
        ));
    }
    let bytes = fs::read(path).map_err(|error| format!("could not read file: {error}"))?;
    String::from_utf8(bytes).map_err(|error| format!("file is not UTF-8: {error}"))
}

pub(crate) fn read_package_metadata(path: &Path) -> Result<PackageMetadata, String> {
    let contents = read_bounded(path, MAX_PACKAGE_METADATA_BYTES).map_err(|error| {
        format!(
            "could not read package metadata {}: {error}",
            path.display()
        )
    })?;
    if extension_is(path, "dpk") {
        return parse_dpk_metadata(path, &contents);
    }
    if extension_is(path, "dproj") {
        return parse_dproj_package_metadata(path, &contents);
    }
    Err(format!(
        "unsupported package descriptor extension: {}",
        path.display()
    ))
}

fn parse_dpk_metadata(path: &Path, contents: &str) -> Result<PackageMetadata, String> {
    if declared_package_name(contents).is_none() {
        return Err(format!(
            "package descriptor {} has no package declaration",
            path.display()
        ));
    };
    // The bounded filename catalogue selected this descriptor by the package
    // name requested by the project. Its header is validated as package
    // syntax, but a legacy header spelling does not replace that identity.
    let mut metadata = PackageMetadata::default();
    metadata.metadata_files.push(path.to_path_buf());
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    for (unit_name, raw_path) in parse_explicit_unit_paths(contents) {
        let Some(unit_path) = package_unit_path(
            &raw_path,
            base,
            &mut metadata.warnings,
            "package contains path",
        ) else {
            continue;
        };
        add_unit_candidate(
            &mut metadata.units,
            canonical_unit_name(&unit_name),
            unit_path,
        );
    }
    Ok(metadata)
}

fn parse_dproj_package_metadata(path: &Path, contents: &str) -> Result<PackageMetadata, String> {
    let project_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut builder = ProjectBuilder::new(
        &ProjectOptions::default(),
        Vec::new(),
        project_dir.to_path_buf(),
    );
    let operations = parse_xml_operations(contents, path)?;
    builder.process_operations(operations, path);

    let Some(main_source) = builder.property("mainsource") else {
        return Err(format!(
            "package project {} has no package MainSource",
            path.display()
        ));
    };
    let main_source = main_source.replace('\\', "/");
    let main_path = Path::new(&main_source);
    if !extension_is(main_path, "dpk") {
        return Err(format!(
            "package project {} does not identify a DPK MainSource",
            path.display()
        ));
    }

    let mut metadata = PackageMetadata {
        warnings: builder.warnings,
        metadata_files: vec![path.to_path_buf()],
        ..PackageMetadata::default()
    };
    if let Some(main_source) = package_unit_path(
        &main_source,
        project_dir,
        &mut metadata.warnings,
        "package main source",
    ) {
        metadata.metadata_files.push(main_source);
    }
    metadata.metadata_files.extend(builder.metadata_files);
    for reference in builder.references {
        if is_compiled_reference(&reference.include) {
            continue;
        }
        let expanded = expand_value(
            &reference.include,
            "",
            &builder.properties,
            &builder.unknown_properties,
            &mut metadata.warnings,
            &reference.source_file,
        );
        if expanded.unknown || expanded.value.contains(UNRESOLVED_MARKER) {
            continue;
        }
        if is_compiled_reference(&expanded.value) {
            continue;
        }
        let Some(unit_path) = package_unit_path(
            &expanded.value,
            project_dir,
            &mut metadata.warnings,
            "package project reference",
        ) else {
            continue;
        };
        let Some(stem) = unit_path.file_stem() else {
            continue;
        };
        add_unit_candidate(
            &mut metadata.units,
            canonical_unit_name(&stem.to_string_lossy()),
            unit_path,
        );
    }
    Ok(metadata)
}

fn package_unit_path(
    raw: &str,
    base: &Path,
    warnings: &mut Vec<String>,
    kind: &str,
) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains(UNRESOLVED_MARKER) {
        return None;
    }
    if is_windows_absolute_text(raw) {
        warnings.push(format!(
            "Windows path in {kind} is unavailable on Linux and was omitted: {raw}"
        ));
        return None;
    }
    let normalized = raw.replace('\\', "/");
    let raw_path = Path::new(&normalized);
    let candidate = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        base.join(raw_path)
    };
    let candidate = lexical_normalize(&candidate);
    Some(resolve_existing_path(&candidate, warnings, kind).unwrap_or(candidate))
}

fn declared_package_name(source: &str) -> Option<String> {
    let tokens = lex_pascal(source);
    tokens.windows(2).find_map(|tokens| {
        let [PascalToken::Word(keyword), PascalToken::Word(name)] = tokens else {
            return None;
        };
        keyword
            .eq_ignore_ascii_case("package")
            .then(|| canonical_package_name(name))
            .filter(|name| !name.is_empty())
    })
}

#[derive(Debug)]
struct ProjectBuilder {
    properties: HashMap<String, String>,
    unknown_properties: HashSet<String>,
    unknown_import_taint: bool,
    references: Vec<DccReference>,
    warnings: Vec<String>,
    incomplete: bool,
    active_imports: HashSet<PathBuf>,
    import_count: usize,
    project_dir: PathBuf,
    global_config: Option<String>,
    global_platform: Option<String>,
    property_bytes: usize,
    metadata_files: Vec<PathBuf>,
}

impl ProjectBuilder {
    fn new(options: &ProjectOptions, warnings: Vec<String>, project_dir: PathBuf) -> Self {
        let mut properties = HashMap::new();
        let global_config = options
            .build_config
            .clone()
            .filter(|value| !value.trim().is_empty());
        let global_platform = options
            .platform
            .clone()
            .filter(|value| !value.trim().is_empty());
        properties.insert(
            "config".to_string(),
            global_config.clone().unwrap_or_default(),
        );
        properties.insert(
            "platform".to_string(),
            global_platform.clone().unwrap_or_default(),
        );
        let property_bytes = properties.values().map(String::len).sum();
        Self {
            properties,
            unknown_properties: HashSet::new(),
            unknown_import_taint: false,
            references: Vec::new(),
            warnings,
            incomplete: false,
            active_imports: HashSet::new(),
            import_count: 0,
            project_dir,
            global_config,
            global_platform,
            property_bytes,
            metadata_files: Vec::new(),
        }
    }

    fn property(&self, name: &str) -> Option<String> {
        self.properties.get(&name.to_ascii_lowercase()).cloned()
    }

    fn process_root_dproj(&mut self, path: &Path) -> Result<(), String> {
        self.metadata_files.push(path.to_path_buf());
        let contents = read_bounded(path, MAX_PROJECT_BYTES)
            .map_err(|error| format!("could not read project {}: {error}", path.display()))?;
        let operations = parse_xml_operations(&contents, path)?;
        self.process_operations(operations, path);
        Ok(())
    }

    fn process_operations(&mut self, operations: Vec<XmlOperation>, source_file: &Path) {
        let base = source_file.parent().unwrap_or_else(|| Path::new("."));
        for operation in operations {
            match operation {
                XmlOperation::PropertyGroup(group) => {
                    self.process_property_group(group, source_file, base);
                }
                XmlOperation::DccReference(reference) => {
                    if is_compiled_reference(&reference.include) {
                        continue;
                    }
                    let condition = condition_matches(
                        reference.condition.as_deref(),
                        ConditionEnvironment {
                            properties: &self.properties,
                            unknown_properties: &self.unknown_properties,
                            unknown_import_taint: self.unknown_import_taint,
                        },
                        base,
                        &mut self.metadata_files,
                        &mut self.warnings,
                        source_file,
                    );
                    match condition {
                        TruthValue::True => self.references.push(DccReference {
                            include: reference.include,
                            condition: None,
                            source_file: source_file.to_path_buf(),
                        }),
                        TruthValue::False => {}
                        TruthValue::Unknown => self.incomplete = true,
                    }
                }
                XmlOperation::Import(import) => self.process_import(import, source_file, base),
                XmlOperation::Unsupported(message) => self.warnings.push(message),
            }
        }
    }

    fn process_property_group(&mut self, group: PropertyGroup, source_file: &Path, base: &Path) {
        let group_result = condition_matches(
            group.condition.as_deref(),
            ConditionEnvironment {
                properties: &self.properties,
                unknown_properties: &self.unknown_properties,
                unknown_import_taint: self.unknown_import_taint,
            },
            base,
            &mut self.metadata_files,
            &mut self.warnings,
            source_file,
        );
        match group_result {
            TruthValue::False => return,
            TruthValue::Unknown => {
                self.incomplete = true;
                for property in group.properties {
                    self.mark_property_unknown(&property.name, source_file);
                }
                return;
            }
            TruthValue::True => {}
        }
        for property in group.properties {
            match condition_matches(
                property.condition.as_deref(),
                ConditionEnvironment {
                    properties: &self.properties,
                    unknown_properties: &self.unknown_properties,
                    unknown_import_taint: self.unknown_import_taint,
                },
                base,
                &mut self.metadata_files,
                &mut self.warnings,
                source_file,
            ) {
                TruthValue::False => continue,
                TruthValue::Unknown => {
                    self.incomplete = true;
                    self.mark_property_unknown(&property.name, source_file);
                    continue;
                }
                TruthValue::True => {}
            }
            if (property.name.eq_ignore_ascii_case("config") && self.global_config.is_some())
                || (property.name.eq_ignore_ascii_case("platform")
                    && self.global_platform.is_some())
            {
                continue;
            }
            let value = expand_value(
                &property.value,
                &property.name,
                &self.properties,
                &self.unknown_properties,
                &mut self.warnings,
                source_file,
            );
            self.set_property(
                &property.name,
                value.value.trim().to_string(),
                value.unknown,
                source_file,
            );
        }
    }

    fn set_property(&mut self, name: &str, value: String, unknown: bool, source_file: &Path) {
        let key = name.to_ascii_lowercase();
        let previous_bytes = self.properties.get(&key).map_or(0, String::len);
        let new_bytes = self
            .property_bytes
            .saturating_sub(previous_bytes)
            .saturating_add(value.len());
        if new_bytes > MAX_TOTAL_PROPERTY_BYTES {
            self.incomplete = true;
            self.warnings.push(format!(
                "property memory budget exceeded while evaluating {name} in {}; value ignored",
                source_file.display()
            ));
            return;
        }
        self.property_bytes = new_bytes;
        if unknown || value.contains(UNRESOLVED_MARKER) {
            self.unknown_properties.insert(key.clone());
        } else {
            self.unknown_properties.remove(&key);
        }
        self.properties.insert(key, value);
    }

    fn taint_unknown_import(&mut self) {
        self.unknown_import_taint = true;
        let keys: Vec<String> = self.properties.keys().cloned().collect();
        for key in keys {
            // MainSource identifies the project itself and is consumed after
            // the complete metadata stream has been evaluated. Keep an
            // already-established value available for that identity lookup,
            // but retain its evaluation taint so later conditions cannot use
            // it as definite evidence. A later definite assignment clears the
            // taint normally.
            if key.eq_ignore_ascii_case("mainsource") {
                self.unknown_properties.insert(key);
                continue;
            }
            if (key.eq_ignore_ascii_case("config") && self.global_config.is_some())
                || (key.eq_ignore_ascii_case("platform") && self.global_platform.is_some())
            {
                continue;
            }
            let previous_bytes = self.properties.get(&key).map_or(0, String::len);
            self.property_bytes = self
                .property_bytes
                .saturating_sub(previous_bytes)
                .saturating_add(UNRESOLVED_MARKER.len_utf8());
            self.properties
                .insert(key.clone(), UNRESOLVED_MARKER.to_string());
            self.unknown_properties.insert(key);
        }
    }

    fn mark_property_unknown(&mut self, name: &str, source_file: &Path) {
        if (name.eq_ignore_ascii_case("config") && self.global_config.is_some())
            || (name.eq_ignore_ascii_case("platform") && self.global_platform.is_some())
        {
            return;
        }
        self.set_property(name, UNRESOLVED_MARKER.to_string(), true, source_file);
        self.unknown_properties.insert(name.to_ascii_lowercase());
    }

    fn process_import(&mut self, import: Import, source_file: &Path, base: &Path) {
        if !looks_like_optset(&import.project) {
            self.warnings.push(format!(
                "ignored non-optset project import in {} (targets are not executed): {}",
                source_file.display(),
                import.project
            ));
            return;
        }
        let expanded = expand_value(
            &import.project,
            "",
            &self.properties,
            &self.unknown_properties,
            &mut self.warnings,
            source_file,
        );
        if expanded.unknown || expanded.value.contains(UNRESOLVED_MARKER) {
            self.incomplete = true;
            self.taint_unknown_import();
            return;
        }
        let Some(candidate) =
            project_path_candidate(&expanded.value, base, &mut self.warnings, "optset import")
        else {
            self.incomplete = true;
            self.taint_unknown_import();
            return;
        };
        let path_status =
            resolve_existing_path_status(&candidate, &mut self.warnings, "optset import");
        let path = match &path_status {
            ExistingPathStatus::Found(path) => path,
            ExistingPathStatus::Missing => &candidate,
            ExistingPathStatus::Unresolvable => {
                self.incomplete = true;
                self.taint_unknown_import();
                return;
            }
        };
        if !add_metadata_file(
            &mut self.metadata_files,
            path.clone(),
            &mut self.warnings,
            source_file,
            "optset",
        ) {
            self.incomplete = true;
            self.taint_unknown_import();
            return;
        }
        let condition = condition_matches(
            import.condition.as_deref(),
            ConditionEnvironment {
                properties: &self.properties,
                unknown_properties: &self.unknown_properties,
                unknown_import_taint: self.unknown_import_taint,
            },
            base,
            &mut self.metadata_files,
            &mut self.warnings,
            source_file,
        );
        match condition {
            TruthValue::True => {}
            TruthValue::False => return,
            TruthValue::Unknown => {
                self.incomplete = true;
                self.taint_unknown_import();
                return;
            }
        }
        if self.import_count >= MAX_IMPORT_COUNT {
            self.incomplete = true;
            self.taint_unknown_import();
            self.warnings.push(format!(
                "optset import limit ({MAX_IMPORT_COUNT}) reached while reading {}",
                source_file.display()
            ));
            return;
        }
        let ExistingPathStatus::Found(path) = path_status else {
            self.incomplete = true;
            self.taint_unknown_import();
            self.warnings.push(format!(
                "optset import path does not exist and was omitted: {}",
                path.display()
            ));
            return;
        };
        if !self.active_imports.insert(path.clone()) {
            self.warnings
                .push(format!("optset import cycle ignored at {}", path.display()));
            return;
        }
        self.import_count += 1;
        let result = read_bounded(&path, MAX_IMPORT_BYTES)
            .and_then(|contents| parse_xml_operations(&contents, &path));
        match result {
            Ok(operations) => self.process_operations(operations, &path),
            Err(error) => {
                self.incomplete = true;
                self.taint_unknown_import();
                self.warnings.push(format!(
                    "could not read optset import {}: {error}",
                    path.display()
                ));
            }
        }
        self.active_imports.remove(&path);
    }
}

#[derive(Debug)]
struct DccReference {
    include: String,
    condition: Option<String>,
    source_file: PathBuf,
}

#[derive(Debug)]
struct Import {
    project: String,
    condition: Option<String>,
}

#[derive(Debug)]
struct PropertyGroup {
    condition: Option<String>,
    properties: Vec<PropertyEntry>,
}

#[derive(Debug)]
struct PropertyEntry {
    name: String,
    condition: Option<String>,
    value: String,
}

#[derive(Debug)]
enum XmlOperation {
    PropertyGroup(PropertyGroup),
    DccReference(DccReference),
    Import(Import),
    Unsupported(String),
}

fn looks_like_optset(project: &str) -> bool {
    project
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .is_some_and(|name| name.to_ascii_lowercase().ends_with(".optset"))
}

#[derive(Debug)]
enum FrameKind {
    Other {
        condition: Option<String>,
        in_target: bool,
    },
    PropertyGroup {
        operation: usize,
        condition: Option<String>,
    },
    Property {
        operation: usize,
        condition: Option<String>,
        in_target: bool,
        entry: PropertyEntry,
    },
}

impl FrameKind {
    fn condition(&self) -> Option<&str> {
        match self {
            Self::Other { condition, .. }
            | Self::PropertyGroup { condition, .. }
            | Self::Property { condition, .. } => condition.as_deref(),
        }
    }

    fn in_target(&self) -> bool {
        match self {
            Self::Other { in_target, .. } | Self::Property { in_target, .. } => *in_target,
            Self::PropertyGroup { .. } => false,
        }
    }
}

#[derive(Debug)]
struct XmlFrame {
    kind: FrameKind,
    text: String,
}

fn parse_xml_operations(contents: &str, path: &Path) -> Result<Vec<XmlOperation>, String> {
    let mut reader = Reader::from_str(contents);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut operations = Vec::new();
    let mut frames: Vec<XmlFrame> = Vec::new();

    loop {
        let event = reader
            .read_event_into(&mut buffer)
            .map_err(|error| format!("invalid XML in {}: {error}", path.display()))?;
        match event {
            Event::Start(start) => {
                let name = local_name(start.name().as_ref());
                let attributes = xml_attributes(&start, reader.decoder())?;
                let parent_condition = frames
                    .last()
                    .and_then(|frame| frame.kind.condition().map(ToOwned::to_owned));
                let parent_in_target = frames.last().is_some_and(|frame| frame.kind.in_target());
                let own_condition = attributes.get("condition").cloned();
                let effective_condition =
                    combine_conditions(parent_condition.as_deref(), own_condition.as_deref());
                let in_target = parent_in_target || name.eq_ignore_ascii_case("target");
                let parent_group = frames.last().and_then(|frame| match &frame.kind {
                    FrameKind::PropertyGroup { operation, .. } => Some(*operation),
                    _ => None,
                });
                if name.eq_ignore_ascii_case("target") && !parent_in_target {
                    operations.push(XmlOperation::Unsupported(format!(
                        "ignored Target body in {}; build targets are not executed",
                        path.display()
                    )));
                }
                if in_target {
                    frames.push(XmlFrame {
                        kind: FrameKind::Other {
                            condition: effective_condition,
                            in_target: true,
                        },
                        text: String::new(),
                    });
                } else if name.eq_ignore_ascii_case("propertygroup") {
                    let operation = operations.len();
                    operations.push(XmlOperation::PropertyGroup(PropertyGroup {
                        condition: effective_condition.clone(),
                        properties: Vec::new(),
                    }));
                    frames.push(XmlFrame {
                        kind: FrameKind::PropertyGroup {
                            operation,
                            condition: effective_condition,
                        },
                        text: String::new(),
                    });
                } else if name.eq_ignore_ascii_case("import") {
                    operations.push(XmlOperation::Import(Import {
                        project: attributes.get("project").cloned().unwrap_or_default(),
                        condition: effective_condition,
                    }));
                    frames.push(XmlFrame {
                        kind: FrameKind::Other {
                            condition: None,
                            in_target: false,
                        },
                        text: String::new(),
                    });
                } else if name.eq_ignore_ascii_case("dccreference") {
                    operations.push(XmlOperation::DccReference(DccReference {
                        include: attributes.get("include").cloned().unwrap_or_default(),
                        condition: effective_condition,
                        source_file: path.to_path_buf(),
                    }));
                    frames.push(XmlFrame {
                        kind: FrameKind::Other {
                            condition: None,
                            in_target: false,
                        },
                        text: String::new(),
                    });
                } else if (name.eq_ignore_ascii_case("option")
                    || name.eq_ignore_ascii_case("property"))
                    && attributes.contains_key("name")
                {
                    let operation = operations.len();
                    operations.push(XmlOperation::PropertyGroup(PropertyGroup {
                        condition: effective_condition.clone(),
                        properties: Vec::new(),
                    }));
                    frames.push(XmlFrame {
                        kind: FrameKind::Property {
                            operation,
                            condition: effective_condition,
                            in_target: false,
                            entry: PropertyEntry {
                                name: attributes.get("name").cloned().unwrap_or_default(),
                                condition: None,
                                value: attributes.get("value").cloned().unwrap_or_default(),
                            },
                        },
                        text: String::new(),
                    });
                } else if let Some(operation) = parent_group {
                    frames.push(XmlFrame {
                        kind: FrameKind::Property {
                            operation,
                            condition: frames
                                .last()
                                .and_then(|frame| frame.kind.condition().map(ToOwned::to_owned)),
                            in_target: false,
                            entry: PropertyEntry {
                                name,
                                condition: own_condition,
                                value: String::new(),
                            },
                        },
                        text: String::new(),
                    });
                } else {
                    frames.push(XmlFrame {
                        kind: FrameKind::Other {
                            condition: effective_condition,
                            in_target: false,
                        },
                        text: String::new(),
                    });
                }
            }
            Event::Empty(empty) => {
                let name = local_name(empty.name().as_ref());
                let attributes = xml_attributes(&empty, reader.decoder())?;
                let parent_condition = frames
                    .last()
                    .and_then(|frame| frame.kind.condition().map(ToOwned::to_owned));
                let parent_in_target = frames.last().is_some_and(|frame| frame.kind.in_target());
                let own_condition = attributes.get("condition").cloned();
                let effective_condition =
                    combine_conditions(parent_condition.as_deref(), own_condition.as_deref());
                if parent_in_target || name.eq_ignore_ascii_case("target") {
                    if name.eq_ignore_ascii_case("target") && !parent_in_target {
                        operations.push(XmlOperation::Unsupported(format!(
                            "ignored Target body in {}; build targets are not executed",
                            path.display()
                        )));
                    }
                    buffer.clear();
                    continue;
                }
                let parent_group = frames.last().and_then(|frame| match &frame.kind {
                    FrameKind::PropertyGroup { operation, .. } => Some(*operation),
                    _ => None,
                });
                if name.eq_ignore_ascii_case("import") {
                    operations.push(XmlOperation::Import(Import {
                        project: attributes.get("project").cloned().unwrap_or_default(),
                        condition: effective_condition,
                    }));
                } else if name.eq_ignore_ascii_case("dccreference") {
                    operations.push(XmlOperation::DccReference(DccReference {
                        include: attributes.get("include").cloned().unwrap_or_default(),
                        condition: effective_condition,
                        source_file: path.to_path_buf(),
                    }));
                } else if (name.eq_ignore_ascii_case("option")
                    || name.eq_ignore_ascii_case("property"))
                    && attributes.contains_key("name")
                {
                    operations.push(XmlOperation::PropertyGroup(PropertyGroup {
                        condition: effective_condition,
                        properties: vec![PropertyEntry {
                            name: attributes.get("name").cloned().unwrap_or_default(),
                            condition: None,
                            value: attributes.get("value").cloned().unwrap_or_default(),
                        }],
                    }));
                } else if let Some(operation) = parent_group {
                    push_property(
                        &mut operations,
                        operation,
                        PropertyEntry {
                            name,
                            condition: own_condition,
                            value: String::new(),
                        },
                    );
                }
            }
            Event::Text(text) => {
                if let Some(frame) = frames.last_mut() {
                    if matches!(frame.kind, FrameKind::Property { .. }) {
                        let decoded = text.unescape().map_err(|error| {
                            format!("invalid XML text in {}: {error}", path.display())
                        })?;
                        frame.text.push_str(&decoded);
                    }
                }
            }
            Event::CData(text) => {
                if let Some(frame) = frames.last_mut() {
                    if matches!(frame.kind, FrameKind::Property { .. }) {
                        frame.text.push_str(&String::from_utf8_lossy(text.as_ref()));
                    }
                }
            }
            Event::End(_) => {
                let Some(frame) = frames.pop() else {
                    return Err(format!("unexpected XML closing tag in {}", path.display()));
                };
                if let FrameKind::Property {
                    operation,
                    mut entry,
                    ..
                } = frame.kind
                {
                    entry.value = frame.text.trim().to_string();
                    push_property(&mut operations, operation, entry);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    if !frames.is_empty() {
        return Err(format!("unterminated XML element in {}", path.display()));
    }
    Ok(operations)
}

fn push_property(operations: &mut [XmlOperation], operation: usize, entry: PropertyEntry) {
    if let Some(XmlOperation::PropertyGroup(group)) = operations.get_mut(operation) {
        group.properties.push(entry);
    }
}

fn local_name(name: &[u8]) -> String {
    let name = String::from_utf8_lossy(name);
    name.rsplit(':').next().unwrap_or_default().to_string()
}

fn xml_attributes(
    start: &BytesStart<'_>,
    decoder: quick_xml::encoding::Decoder,
) -> Result<HashMap<String, String>, String> {
    let mut attributes = HashMap::new();
    for attribute in start.attributes().with_checks(false) {
        let attribute = attribute.map_err(|error| format!("invalid XML attribute: {error}"))?;
        let name = local_name(attribute.key.as_ref()).to_ascii_lowercase();
        let value = attribute
            .decode_and_unescape_value(decoder)
            .map_err(|error| format!("invalid XML attribute value: {error}"))?;
        attributes.insert(name, value.into_owned());
    }
    Ok(attributes)
}

#[derive(Debug)]
struct ExpandedValue {
    value: String,
    unknown: bool,
}

fn expand_value(
    value: &str,
    current_property: &str,
    properties: &HashMap<String, String>,
    unknown_properties: &HashSet<String>,
    warnings: &mut Vec<String>,
    source_file: &Path,
) -> ExpandedValue {
    let mut expanded = String::new();
    let mut unknown = false;
    let mut cursor = 0;
    while let Some(relative_start) = value[cursor..].find("$(") {
        let start = cursor + relative_start;
        if !append_expansion(&mut expanded, &value[cursor..start], warnings, source_file) {
            return ExpandedValue {
                value: UNRESOLVED_MARKER.to_string(),
                unknown: true,
            };
        }
        let Some(relative_end) = value[start + 2..].find(')') else {
            warnings.push(format!(
                "unsupported unclosed property expansion in {}",
                source_file.display()
            ));
            return ExpandedValue {
                value: UNRESOLVED_MARKER.to_string(),
                unknown: true,
            };
        };
        let end = start + 2 + relative_end;
        let name = value[start + 2..end].trim();
        let key = name.to_ascii_lowercase();
        if name.eq_ignore_ascii_case("thisfiledirectory")
            || name.eq_ignore_ascii_case("msbuildthisfiledirectory")
        {
            let directory = source_file
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_string_lossy()
                .replace('\\', "/");
            let directory = format!("{}/", directory.trim_end_matches('/'));
            if !append_expansion(&mut expanded, &directory, warnings, source_file) {
                return ExpandedValue {
                    value: UNRESOLVED_MARKER.to_string(),
                    unknown: true,
                };
            }
        } else if let Some(replacement) = properties.get(&key) {
            if !append_expansion(&mut expanded, replacement, warnings, source_file) {
                return ExpandedValue {
                    value: UNRESOLVED_MARKER.to_string(),
                    unknown: true,
                };
            }
            unknown |= replacement.contains(UNRESOLVED_MARKER) || unknown_properties.contains(&key);
        } else if name.eq_ignore_ascii_case(current_property) {
            // Delphi project files commonly terminate list properties with
            // their own unset value. This is an intentional empty default.
            unknown |= unknown_properties.contains(&key);
        } else {
            warnings.push(format!(
                "unresolved property path $({name}) in {}",
                source_file.display()
            ));
            unknown = true;
            if !append_expansion(
                &mut expanded,
                &UNRESOLVED_MARKER.to_string(),
                warnings,
                source_file,
            ) {
                return ExpandedValue {
                    value: UNRESOLVED_MARKER.to_string(),
                    unknown: true,
                };
            }
        }
        cursor = end + 1;
    }
    if !append_expansion(&mut expanded, &value[cursor..], warnings, source_file) {
        return ExpandedValue {
            value: UNRESOLVED_MARKER.to_string(),
            unknown: true,
        };
    }
    ExpandedValue {
        value: expanded,
        unknown,
    }
}

fn append_expansion(
    target: &mut String,
    addition: &str,
    warnings: &mut Vec<String>,
    source_file: &Path,
) -> bool {
    let exceeds_budget = match target.len().checked_add(addition.len()) {
        Some(length) => length > MAX_EXPANDED_VALUE_BYTES,
        None => true,
    };
    if exceeds_budget {
        warnings.push(format!(
            "property expansion exceeds the {} byte per-value budget in {}; value omitted",
            MAX_EXPANDED_VALUE_BYTES,
            source_file.display()
        ));
        return false;
    }
    target.push_str(addition);
    true
}

#[derive(Debug, Clone)]
struct ConditionValue {
    value: String,
    unknown: bool,
}

struct ConditionEnvironment<'a> {
    properties: &'a HashMap<String, String>,
    unknown_properties: &'a HashSet<String>,
    unknown_import_taint: bool,
}

fn expand_condition_value(
    value: &str,
    properties: &HashMap<String, String>,
    unknown_properties: &HashSet<String>,
    unknown_import_taint: bool,
    source_file: &Path,
    warnings: &mut Vec<String>,
) -> ConditionValue {
    let mut expanded = String::new();
    let mut cursor = 0;
    let mut missing = false;
    while let Some(relative_start) = value[cursor..].find("$(") {
        let start = cursor + relative_start;
        if !append_expansion(&mut expanded, &value[cursor..start], warnings, source_file) {
            return ConditionValue {
                value: String::new(),
                unknown: true,
            };
        }
        let Some(relative_end) = value[start + 2..].find(')') else {
            return ConditionValue {
                value: String::new(),
                unknown: true,
            };
        };
        let end = start + 2 + relative_end;
        let name = value[start + 2..end].trim();
        if name.eq_ignore_ascii_case("thisfiledirectory")
            || name.eq_ignore_ascii_case("msbuildthisfiledirectory")
        {
            let directory = source_file
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_string_lossy()
                .replace('\\', "/");
            let directory = format!("{}/", directory.trim_end_matches('/'));
            if !append_expansion(&mut expanded, &directory, warnings, source_file) {
                return ConditionValue {
                    value: String::new(),
                    unknown: true,
                };
            }
        } else if let Some(replacement) = properties.get(&name.to_ascii_lowercase()) {
            if !append_expansion(&mut expanded, replacement, warnings, source_file) {
                return ConditionValue {
                    value: String::new(),
                    unknown: true,
                };
            }
            missing |= replacement.contains(UNRESOLVED_MARKER)
                || unknown_properties.contains(&name.to_ascii_lowercase());
        } else {
            if unknown_import_taint || !is_known_project_local_configuration_property(name) {
                missing = true;
                if !append_expansion(
                    &mut expanded,
                    &UNRESOLVED_MARKER.to_string(),
                    warnings,
                    source_file,
                ) {
                    return ConditionValue {
                        value: String::new(),
                        unknown: true,
                    };
                }
            }
        }
        cursor = end + 1;
    }
    if !append_expansion(&mut expanded, &value[cursor..], warnings, source_file) {
        return ConditionValue {
            value: String::new(),
            unknown: true,
        };
    }
    ConditionValue {
        value: expanded,
        unknown: missing,
    }
}

fn combine_conditions(parent: Option<&str>, own: Option<&str>) -> Option<String> {
    match (
        parent.map(str::trim).filter(|value| !value.is_empty()),
        own.map(str::trim).filter(|value| !value.is_empty()),
    ) {
        (None, None) => None,
        (Some(parent), None) => Some(parent.to_string()),
        (None, Some(own)) => Some(own.to_string()),
        (Some(parent), Some(own)) => Some(format!("({parent}) And ({own})")),
    }
}

fn condition_matches(
    condition: Option<&str>,
    environment: ConditionEnvironment<'_>,
    base: &Path,
    metadata_files: &mut Vec<PathBuf>,
    warnings: &mut Vec<String>,
    source_file: &Path,
) -> TruthValue {
    let Some(condition) = condition.map(str::trim).filter(|value| !value.is_empty()) else {
        return TruthValue::True;
    };
    let tokens = match tokenize_condition(condition) {
        Ok(tokens) => tokens,
        Err(error) => {
            warnings.push(format!(
                "unsupported project condition in {}: {error}: {condition}",
                source_file.display()
            ));
            return TruthValue::Unknown;
        }
    };
    let mut parser = ConditionParser {
        tokens,
        position: 0,
        properties: environment.properties,
        unknown_properties: environment.unknown_properties,
        unknown_import_taint: environment.unknown_import_taint,
        base,
        metadata_files,
        warnings,
        source_file,
        unknown_seen: false,
    };
    let result = parser.parse();
    let unknown = parser.unknown_seen;
    match result {
        Ok(TruthValue::True) => TruthValue::True,
        Ok(TruthValue::False) => {
            if unknown {
                parser.warnings.push(format!(
                    "unknown project condition in {}: {condition}",
                    parser.source_file.display()
                ));
            }
            TruthValue::False
        }
        Ok(TruthValue::Unknown) => {
            parser.warnings.push(format!(
                "unknown project condition in {}: {condition}",
                parser.source_file.display()
            ));
            TruthValue::Unknown
        }
        Err(error) => {
            parser.warnings.push(format!(
                "unsupported project condition in {}: {error}: {condition}",
                parser.source_file.display()
            ));
            TruthValue::Unknown
        }
    }
}

fn is_known_project_local_configuration_property(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "config"
        || name == "platform"
        || name == "base"
        || name.starts_with("base_")
        || name.starts_with("cfg_")
        || name == "cfgparent"
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TruthValue {
    True,
    False,
    Unknown,
}

impl TruthValue {
    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::False, _) | (_, Self::False) => Self::False,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::True, Self::True) => Self::True,
        }
    }

    fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::True, _) | (_, Self::True) => Self::True,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::False, Self::False) => Self::False,
        }
    }
}

#[derive(Debug, Clone)]
enum ConditionToken {
    Word(String),
    Quoted(String),
    Equals,
    NotEquals,
    LeftParen,
    RightParen,
    Comma,
}

fn tokenize_condition(condition: &str) -> Result<Vec<ConditionToken>, String> {
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < condition.len() {
        let rest = &condition[cursor..];
        let Some(first) = rest.chars().next() else {
            break;
        };
        if first.is_whitespace() {
            cursor += first.len_utf8();
            continue;
        }
        if rest.starts_with("==") {
            tokens.push(ConditionToken::Equals);
            cursor += 2;
            continue;
        }
        if rest.starts_with("!=") {
            tokens.push(ConditionToken::NotEquals);
            cursor += 2;
            continue;
        }
        match first {
            '(' => {
                tokens.push(ConditionToken::LeftParen);
                cursor += 1;
            }
            ')' => {
                tokens.push(ConditionToken::RightParen);
                cursor += 1;
            }
            ',' => {
                tokens.push(ConditionToken::Comma);
                cursor += 1;
            }
            '\'' | '"' => {
                let quote = first;
                cursor += quote.len_utf8();
                let start = cursor;
                let mut value = String::new();
                let mut closed = false;
                while cursor < condition.len() {
                    let current = condition[cursor..]
                        .chars()
                        .next()
                        .expect("cursor is inside condition");
                    cursor += current.len_utf8();
                    if current == quote {
                        closed = true;
                        break;
                    }
                    value.push(current);
                }
                if !closed {
                    return Err(format!("unterminated quoted value at byte {start}"));
                }
                tokens.push(ConditionToken::Quoted(value));
            }
            _ => {
                let start = cursor;
                while cursor < condition.len() {
                    let current = condition[cursor..]
                        .chars()
                        .next()
                        .expect("cursor is inside condition");
                    if current.is_whitespace() || matches!(current, '(' | ')' | ',' | '!' | '=') {
                        break;
                    }
                    cursor += current.len_utf8();
                }
                if cursor == start {
                    return Err(format!("unexpected character at byte {cursor}"));
                }
                let word = &condition[start..cursor];
                tokens.push(ConditionToken::Word(word.to_string()));
            }
        }
    }
    Ok(tokens)
}

struct ConditionParser<'a> {
    tokens: Vec<ConditionToken>,
    position: usize,
    properties: &'a HashMap<String, String>,
    unknown_properties: &'a HashSet<String>,
    unknown_import_taint: bool,
    base: &'a Path,
    metadata_files: &'a mut Vec<PathBuf>,
    warnings: &'a mut Vec<String>,
    source_file: &'a Path,
    unknown_seen: bool,
}

impl ConditionParser<'_> {
    fn parse(&mut self) -> Result<TruthValue, String> {
        let value = self.parse_or()?;
        if self.position != self.tokens.len() {
            return Err("trailing tokens".to_string());
        }
        Ok(value)
    }

    fn parse_or(&mut self) -> Result<TruthValue, String> {
        let mut value = self.parse_and()?;
        while self.take_operator("or") {
            value = value.or(self.parse_and()?);
        }
        Ok(value)
    }

    fn parse_and(&mut self) -> Result<TruthValue, String> {
        let mut value = self.parse_primary()?;
        while self.take_operator("and") {
            value = value.and(self.parse_primary()?);
        }
        Ok(value)
    }

    fn parse_primary(&mut self) -> Result<TruthValue, String> {
        if self.take_token(|token| matches!(token, ConditionToken::LeftParen)) {
            let value = self.parse_or()?;
            if !self.take_token(|token| matches!(token, ConditionToken::RightParen)) {
                return Err("missing closing parenthesis".to_string());
            }
            return Ok(value);
        }
        if self.peek_word("exists") {
            self.position += 1;
            if !self.take_token(|token| matches!(token, ConditionToken::LeftParen)) {
                return Err("Exists requires parentheses".to_string());
            }
            let argument = self.parse_value()?;
            if !self.take_token(|token| matches!(token, ConditionToken::RightParen)) {
                return Err("Exists is missing its closing parenthesis".to_string());
            }
            if argument.unknown {
                self.unknown_seen = true;
                return Ok(TruthValue::Unknown);
            }
            let Some(candidate) = project_path_candidate(
                &argument.value,
                self.base,
                self.warnings,
                "Exists condition",
            ) else {
                return Ok(TruthValue::False);
            };
            let path_status =
                resolve_existing_path_status(&candidate, self.warnings, "Exists condition");
            let path = match &path_status {
                ExistingPathStatus::Found(path) => path,
                ExistingPathStatus::Missing | ExistingPathStatus::Unresolvable => &candidate,
            };
            if !add_metadata_file(
                self.metadata_files,
                path.clone(),
                self.warnings,
                self.source_file,
                "Exists condition",
            ) {
                self.unknown_seen = true;
                return Ok(TruthValue::Unknown);
            }
            return Ok(match path_status {
                ExistingPathStatus::Found(_) => TruthValue::True,
                ExistingPathStatus::Missing => TruthValue::False,
                ExistingPathStatus::Unresolvable => TruthValue::Unknown,
            });
        }

        let left = self.parse_value()?;
        let operator = if self.take_token(|token| matches!(token, ConditionToken::Equals)) {
            true
        } else if self.take_token(|token| matches!(token, ConditionToken::NotEquals)) {
            false
        } else {
            return Err("condition requires == or !=".to_string());
        };
        let right = self.parse_value()?;
        if left.unknown || right.unknown {
            self.unknown_seen = true;
            return Ok(TruthValue::Unknown);
        }
        let equal = left.value.eq_ignore_ascii_case(&right.value);
        Ok(if operator {
            if equal {
                TruthValue::True
            } else {
                TruthValue::False
            }
        } else if equal {
            TruthValue::False
        } else {
            TruthValue::True
        })
    }

    fn parse_value(&mut self) -> Result<ConditionValue, String> {
        let token = self
            .tokens
            .get(self.position)
            .ok_or_else(|| "missing condition value".to_string())?
            .clone();
        self.position += 1;
        match token {
            ConditionToken::Word(value) | ConditionToken::Quoted(value) => {
                let expanded = expand_condition_value(
                    &value,
                    self.properties,
                    self.unknown_properties,
                    self.unknown_import_taint,
                    self.source_file,
                    self.warnings,
                );
                self.unknown_seen |= expanded.unknown;
                Ok(expanded)
            }
            _ => Err("expected a condition value".to_string()),
        }
    }

    fn peek_word(&self, expected: &str) -> bool {
        matches!(
            self.tokens.get(self.position),
            Some(ConditionToken::Word(value)) if value.eq_ignore_ascii_case(expected)
        )
    }

    fn take_operator(&mut self, expected: &str) -> bool {
        if self.peek_word(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn take_token(&mut self, predicate: impl FnOnce(&ConditionToken) -> bool) -> bool {
        if self.tokens.get(self.position).is_some_and(predicate) {
            self.position += 1;
            true
        } else {
            false
        }
    }
}

#[derive(Debug)]
enum PascalToken {
    Word(String),
    String(String),
    Comma,
    Semicolon,
    Dot,
    Other,
}

fn parse_explicit_unit_paths(source: &str) -> Vec<(String, String)> {
    parse_unit_membership(source).explicit_paths
}

#[derive(Debug, Default)]
struct UnitMembership {
    explicit_paths: Vec<(String, String)>,
    exhaustive: bool,
}

fn parse_unit_membership(source: &str) -> UnitMembership {
    let tokens = lex_pascal(source);
    let mut result = UnitMembership {
        exhaustive: true,
        ..UnitMembership::default()
    };
    if contains_conditional_compiler_directive(source) {
        // The lexer deliberately removes directives, so a conditional uses or
        // contains clause cannot be proven exhaustive from the remaining
        // tokens. Keep explicit paths for positive lookup, but never use them
        // as negative ownership evidence.
        result.exhaustive = false;
    }
    let mut cursor = 0;
    while cursor < tokens.len() {
        let is_clause = matches!(
            tokens.get(cursor),
            Some(PascalToken::Word(word))
                if word.eq_ignore_ascii_case("uses") || word.eq_ignore_ascii_case("contains")
        );
        if !is_clause {
            cursor += 1;
            continue;
        }
        cursor += 1;
        let clause_start = cursor;
        while cursor < tokens.len() && !matches!(tokens[cursor], PascalToken::Semicolon) {
            cursor += 1;
        }
        parse_unit_clause(
            &tokens[clause_start..cursor],
            &mut result.explicit_paths,
            &mut result.exhaustive,
        );
        cursor = cursor.saturating_add(1);
    }
    result
}

fn contains_conditional_compiler_directive(source: &str) -> bool {
    let lower = source.to_ascii_lowercase();
    ["{$if", "{$else", "{$endif", "(*$if", "(*$else", "(*$endif"]
        .iter()
        .any(|marker| lower.contains(marker))
}

fn parse_unit_clause(
    tokens: &[PascalToken],
    result: &mut Vec<(String, String)>,
    exhaustive: &mut bool,
) {
    let mut segment_start = 0;
    for index in 0..=tokens.len() {
        if index == tokens.len() || matches!(tokens[index], PascalToken::Comma) {
            if !parse_unit_segment(&tokens[segment_start..index], result) {
                *exhaustive = false;
            }
            segment_start = index.saturating_add(1);
        }
    }
}

fn parse_unit_segment(tokens: &[PascalToken], result: &mut Vec<(String, String)>) -> bool {
    if tokens.is_empty() {
        return true;
    }
    let Some(in_index) = tokens.iter().position(
        |token| matches!(token, PascalToken::Word(word) if word.eq_ignore_ascii_case("in")),
    ) else {
        return false;
    };
    let Some(PascalToken::String(path)) = tokens.get(in_index + 1) else {
        return false;
    };
    let mut unit_name = String::new();
    for token in &tokens[..in_index] {
        match token {
            PascalToken::Word(word) => unit_name.push_str(word),
            PascalToken::Dot => unit_name.push('.'),
            PascalToken::Other
            | PascalToken::String(_)
            | PascalToken::Comma
            | PascalToken::Semicolon => {}
        }
    }
    let unit_name = unit_name.trim().to_string();
    if !unit_name.is_empty() && !path.is_empty() {
        result.push((unit_name, path.clone()));
        true
    } else {
        false
    }
}

fn lex_pascal(source: &str) -> Vec<PascalToken> {
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < source.len() {
        let rest = &source[cursor..];
        let Some(first) = rest.chars().next() else {
            break;
        };
        if first.is_whitespace() {
            cursor += first.len_utf8();
            continue;
        }
        if rest.starts_with("//") {
            cursor += 2;
            while cursor < source.len() {
                let current = source[cursor..]
                    .chars()
                    .next()
                    .expect("cursor is inside source");
                cursor += current.len_utf8();
                if current == '\n' {
                    break;
                }
            }
            continue;
        }
        if first == '{' {
            cursor += 1;
            while cursor < source.len() {
                let current = source[cursor..]
                    .chars()
                    .next()
                    .expect("cursor is inside source");
                cursor += current.len_utf8();
                if current == '}' {
                    break;
                }
            }
            continue;
        }
        if rest.starts_with("(*") {
            cursor += 2;
            while cursor < source.len() {
                if source[cursor..].starts_with("*)") {
                    cursor += 2;
                    break;
                }
                let current = source[cursor..]
                    .chars()
                    .next()
                    .expect("cursor is inside source");
                cursor += current.len_utf8();
            }
            continue;
        }
        if first == '\'' {
            cursor += 1;
            let mut value = String::new();
            while cursor < source.len() {
                let current = source[cursor..]
                    .chars()
                    .next()
                    .expect("cursor is inside source");
                cursor += current.len_utf8();
                if current != '\'' {
                    value.push(current);
                    continue;
                }
                if source[cursor..].starts_with('\'') {
                    value.push('\'');
                    cursor += 1;
                } else {
                    break;
                }
            }
            tokens.push(PascalToken::String(value));
            continue;
        }
        if first.is_ascii_alphabetic() || first == '_' {
            let start = cursor;
            cursor += first.len_utf8();
            while cursor < source.len() {
                let current = source[cursor..]
                    .chars()
                    .next()
                    .expect("cursor is inside source");
                if current.is_ascii_alphanumeric() || current == '_' {
                    cursor += current.len_utf8();
                } else {
                    break;
                }
            }
            tokens.push(PascalToken::Word(source[start..cursor].to_string()));
            continue;
        }
        let token = match first {
            ',' => PascalToken::Comma,
            ';' => PascalToken::Semicolon,
            '.' => PascalToken::Dot,
            _ => PascalToken::Other,
        };
        tokens.push(token);
        cursor += first.len_utf8();
    }
    tokens
}
