//! Workspace state, bounded source discovery, overlays, diagnostics, and formatting.

use self::rename::CANCELLATION_MESSAGE;
use crate::configuration::{config_directories, resolve_fmt, resolve_lint};
use crate::project::{
    MetadataObservation, PackageMetadata, ProjectCandidateMembership, ProjectCandidates,
    ProjectContext, ProjectDiscovery, ProjectOptions, ProjectPathEntry, ProjectPathProvenance,
    ProjectReadObservation, ProjectReadStamp, ProjectSelections, discover_with_selections,
    discover_with_selections_and_observations_with_cancel_and_overrides,
    discover_with_selections_and_observations_with_overrides, has_invalid_project_selection,
    project_candidate_membership, project_candidates, project_candidates_with_cancel,
    read_package_metadata_with_observations, runtime_project_selection,
    selected_project_is_current, selected_project_is_current_with_cancel,
};
use crate::{NavigationIndex, NavigationTarget, text};
use globset::{GlobSet, GlobSetBuilder};
use lsp_types::{
    Diagnostic as LspDiagnostic, DiagnosticSeverity, Location, NumberOrString, Position, Range,
    TextEdit, Url,
};
use pascal_core::delphi_overrides::{
    EffectiveOverrides, LOCAL_CONFIG_NAME, OverrideSession, user_config_path,
};
use pascal_core::{FileInfo, Severity, decode_bytes, parser};
use serde::Deserialize;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::hash::Hasher;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};
use walkdir::WalkDir;

pub(crate) mod codeactions;
pub(crate) mod projects;
pub(crate) mod queries;
pub(crate) mod rename;

/// Maximum syntax-tree depth used before invoking the recursive lint/format pipelines.
pub const MAX_TREE_DEPTH: usize = 256;
const DIAGNOSTIC_DEBOUNCE: Duration = Duration::from_millis(250);
const DEFAULT_MAX_FILES: usize = 10_000;
const DEFAULT_MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;
const MAX_DEPENDENCY_WORK: usize = 256;
const MAX_DIRECTORY_CATALOGUES: usize = 1024;
const MAX_FILENAME_CATALOGUE_ENTRIES: usize = 10_000;
const MAX_FILENAME_CATALOGUES: usize = 256;
// The multidev workspace currently contains 436,705 filesystem entries when
// counted without following links. Keep a fixed margin for normal growth, but
// retain a hard stop so a pathological workspace cannot turn package lookup
// into an unbounded traversal.
const MAX_PACKAGE_CATALOGUE_ENTRIES: usize = 524_288;
const MAX_PACKAGE_CATALOGUES: usize = 64;
const MAX_PACKAGE_LOOKUPS: usize = 256;
const MAX_PACKAGE_METADATA_CACHE: usize = 512;
const MAX_PACKAGE_UNIT_CANDIDATES: usize = 1_024;
const MAX_WORKSPACE_WARNINGS: usize = 256;
const MAX_DELETED_OVERRIDES: usize = 256;
const MAX_DOCUMENT_OWNERS: usize = 4_096;
const MAX_FORMAT_EXTERNAL_TRAVERSAL_ENTRIES: usize = 1_048_576;
pub(crate) const MAX_CONFIGURATION_WATCH_PATHS: usize = 256;
const CONFIGURATION_FILENAMES: [&str; 2] = [".lint4d.toml", ".fmt4d.toml"];

fn check_workspace_cancel(cancel: Option<&AtomicBool>) -> Result<(), String> {
    if cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
        Err(CANCELLATION_MESSAGE.to_string())
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ResourceLimits {
    pub max_files: usize,
    pub max_file_bytes: usize,
    pub max_total_bytes: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_files: DEFAULT_MAX_FILES,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct WorkspaceOptions {
    pub source_paths: Vec<String>,
    pub exclude: Vec<String>,
    pub project_file: Option<PathBuf>,
    pub build_config: Option<String>,
    pub platform: Option<String>,
    pub limits: ResourceLimits,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RawWorkspaceOptions {
    #[serde(default)]
    source_paths: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    project_file: Option<PathBuf>,
    build_config: Option<String>,
    platform: Option<String>,
    max_files: Option<usize>,
    max_file_bytes: Option<usize>,
    max_total_bytes: Option<usize>,
}

impl WorkspaceOptions {
    pub fn parse(value: Option<&serde_json::Value>) -> Result<Self, String> {
        let Some(value) = value.filter(|value| !value.is_null()) else {
            return Ok(Self::default());
        };
        let direct: RawWorkspaceOptions = serde_json::from_value(value.clone())
            .map_err(|error| format!("invalid initializationOptions: {error}"))?;
        let nested = value
            .get("pascalLsp")
            .or_else(|| value.get("pascal-lsp"))
            .filter(|nested| nested.is_object())
            .map(|nested| {
                serde_json::from_value::<RawWorkspaceOptions>(nested.clone())
                    .map_err(|error| format!("invalid initializationOptions.pascalLsp: {error}"))
            })
            .transpose()?;

        let raw = nested.unwrap_or(direct);
        let mut limits = ResourceLimits::default();
        if let Some(value) = raw.max_files {
            limits.max_files = bounded_limit(value, 1, DEFAULT_MAX_FILES, "maxFiles")?;
        }
        if let Some(value) = raw.max_file_bytes {
            limits.max_file_bytes =
                bounded_limit(value, 1, DEFAULT_MAX_FILE_BYTES, "maxFileBytes")?;
        }
        if let Some(value) = raw.max_total_bytes {
            limits.max_total_bytes =
                bounded_limit(value, 1, DEFAULT_MAX_TOTAL_BYTES, "maxTotalBytes")?;
        }
        Ok(Self {
            source_paths: raw.source_paths,
            exclude: raw.exclude,
            project_file: raw.project_file,
            build_config: raw.build_config,
            platform: raw.platform,
            limits,
        })
    }
}

fn bounded_limit(
    value: usize,
    minimum: usize,
    maximum: usize,
    name: &str,
) -> Result<usize, String> {
    if value < minimum {
        return Err(format!(
            "initializationOptions.{name} must be at least {minimum}"
        ));
    }
    if value > maximum {
        eprintln!(
            "pascal-lsp: warning: initializationOptions.{name}={value} exceeds the safe limit {maximum}; using {maximum}"
        );
        return Ok(maximum);
    }
    Ok(value)
}

#[derive(Debug, Clone, Copy)]
pub enum FileChange {
    Created,
    Changed,
    Deleted,
}

#[derive(Debug)]
struct OpenDocument {
    text: Option<String>,
    version: i32,
    rejection: Option<String>,
}

struct DiskSource {
    text: String,
    bytes: usize,
    stamp: DiskStamp,
    content_hash: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiskStamp {
    bytes: u64,
    modified: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathStamp {
    bytes: u64,
    modified: Option<SystemTime>,
    is_dir: bool,
    is_symlink: bool,
}

#[derive(Debug, Clone, Default)]
struct DirectoryCatalogue {
    stamp: Option<PathStamp>,
    entries: Vec<PathBuf>,
    last_used: u64,
}

#[derive(Debug, Clone, Default)]
struct FilenameCatalogue {
    entries: HashMap<String, Vec<PathBuf>>,
    complete: bool,
    directories: Vec<(PathBuf, Option<PathStamp>)>,
    last_used: u64,
}

#[derive(Debug, Clone, Default)]
struct PackageCatalogue {
    entries: HashMap<String, Vec<PathBuf>>,
    requested_names: HashSet<String>,
    complete: bool,
    directories: Vec<(PathBuf, Option<PathStamp>)>,
    validated_epoch: u64,
    last_used: u64,
}

#[derive(Debug)]
struct PackageDescriptorMatch {
    path: PathBuf,
    names: Vec<String>,
}

#[derive(Debug, Default)]
struct PackageDescriptorFiles {
    has_dpk: bool,
    dpk: Vec<PackageDescriptorMatch>,
    dproj: Vec<PackageDescriptorMatch>,
}

#[derive(Debug, Default)]
struct PackageCatalogueScan {
    entries: HashMap<String, Vec<PathBuf>>,
    directories: Vec<(PathBuf, Option<PathStamp>)>,
    complete: bool,
}

#[derive(Debug, Clone)]
struct CachedPackageMetadata {
    metadata_stamps: Vec<(PathBuf, Option<PathStamp>)>,
    result: Result<PackageMetadata, String>,
    observations: Vec<ProjectReadObservation>,
    last_used: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PackageMetadataKey {
    descriptor: PathBuf,
    overrides: EffectiveOverrides,
    read_policy: crate::project::ReadPolicy,
    config: Option<String>,
    platform: Option<String>,
}

#[derive(Debug, Default)]
struct PackageLookup {
    candidates: Vec<PathBuf>,
    metadata_paths: Vec<PathBuf>,
    observations: Vec<ProjectReadObservation>,
    metadata_observations: Vec<MetadataObservation>,
    warnings: Vec<String>,
    complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ContextKey {
    project_file: Option<PathBuf>,
    workspace_root: Option<PathBuf>,
    project_scope: Option<PathBuf>,
    selection_scope: Option<PathBuf>,
    selection_project: Option<PathBuf>,
    config: Option<String>,
    platform: Option<String>,
    overrides: EffectiveOverrides,
}

/// A legacy sibling lookup is a per-document fact, not a directory grant.  A
/// route remains usable only for the exact source that was loaded and the
/// project context that established it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LegacyRouteProof {
    source: PathBuf,
    context: ContextKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PackageCatalogueKey {
    context: ContextKey,
    root: PathBuf,
}

#[derive(Debug, Clone, Default)]
struct ContextState {
    context: ProjectContext,
    watched_paths: HashMap<PathBuf, Option<PathStamp>>,
    project_candidate_memberships: HashMap<PathBuf, Result<ProjectCandidateMembership, String>>,
    project_read_observations: Vec<ProjectReadObservation>,
}

impl ContextState {
    fn merge_observations(&mut self, retained: &Self) {
        for (path, stamp) in &retained.watched_paths {
            self.watched_paths
                .entry(path.clone())
                .or_insert_with(|| stamp.clone());
        }
        for (path, membership) in &retained.project_candidate_memberships {
            self.project_candidate_memberships
                .entry(path.clone())
                .or_insert_with(|| membership.clone());
        }
        for path in &retained.context.metadata_files {
            if !self
                .context
                .metadata_files
                .iter()
                .any(|existing| paths_equal_ci(existing, path))
            {
                self.context.metadata_files.push(path.clone());
            }
        }
        for observation in &retained.context.metadata_observations {
            if !self
                .context
                .metadata_observations
                .iter()
                .any(|existing| existing == observation)
            {
                self.context.metadata_observations.push(observation.clone());
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnerOrigin {
    /// The document's own automatic project discovery selected this owner.
    Automatic,
    /// The client selected this owner explicitly, or configured a startup
    /// project for the document.
    Explicit,
    /// The owner was inherited while loading the document as a dependency.
    Inherited,
}

#[derive(Debug, Clone)]
pub(crate) struct KnownDocumentOwner {
    key: ContextKey,
    state: ContextState,
    origin: OwnerOrigin,
    pub(crate) legacy_route: Option<LegacyRouteProof>,
}

impl KnownDocumentOwner {
    pub(crate) fn has_legacy_route(&self, path: &Path) -> bool {
        self.legacy_route.as_ref().is_some_and(|route| {
            route.context == self.key && native_paths_equal(&route.source, path)
        })
    }
}

#[derive(Debug, Clone)]
struct ExcludeMatcher {
    root: PathBuf,
    config_root: PathBuf,
    patterns: Option<GlobSet>,
}

impl ExcludeMatcher {
    fn new(root: &Path, config_root: &Path, patterns: &[String]) -> Self {
        Self {
            root: root.to_path_buf(),
            config_root: config_root.to_path_buf(),
            patterns: compile_exclude_patterns(patterns),
        }
    }

    fn is_excluded(&self, path: &Path, source_root: &Path) -> bool {
        if native_relative_path(path, source_root)
            .is_some_and(|relative| relative.components().any(is_default_excluded_component))
        {
            return true;
        }

        let Some(patterns) = &self.patterns else {
            return false;
        };
        [source_root, self.root.as_path(), self.config_root.as_path()]
            .into_iter()
            .filter_map(|base| native_relative_path(path, base))
            .any(|relative| matches_exclude_patterns(patterns, &relative))
    }
}

#[derive(Debug, Clone)]
struct LintExcludeMatcher {
    root: PathBuf,
    patterns: Option<GlobSet>,
}

impl LintExcludeMatcher {
    fn new(root: &Path, patterns: &[String]) -> Self {
        Self {
            root: root.to_path_buf(),
            patterns: compile_exclude_patterns(patterns),
        }
    }

    fn is_excluded(&self, path: &Path) -> bool {
        let Some(patterns) = &self.patterns else {
            return false;
        };
        relative_path(&self.root, path)
            .is_some_and(|relative| matches_exclude_patterns(patterns, &relative))
    }
}

fn compile_exclude_patterns(patterns: &[String]) -> Option<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    let mut valid_pattern_count = 0;
    for pattern in patterns {
        let normalized = pattern.replace('\\', "/");
        let glob = {
            #[cfg(windows)]
            {
                globset::GlobBuilder::new(&normalized)
                    .case_insensitive(true)
                    .build()
            }
            #[cfg(not(windows))]
            {
                globset::Glob::new(&normalized)
            }
        };
        match glob {
            Ok(glob) => {
                builder.add(glob);
                valid_pattern_count += 1;
            }
            Err(error) => {
                eprintln!("pascal-lsp: warning: ignoring invalid exclude glob {pattern:?}: {error}")
            }
        }
    }
    if valid_pattern_count == 0 {
        return None;
    }
    match builder.build() {
        Ok(set) => Some(set),
        Err(error) => {
            eprintln!("pascal-lsp: warning: failed to build exclude globs: {error}");
            None
        }
    }
}

fn matches_exclude_patterns(patterns: &GlobSet, relative: &Path) -> bool {
    let mut prefix = PathBuf::new();
    for component in relative.components() {
        prefix.push(component.as_os_str());
        if patterns.is_match(path_to_glob(&prefix)) {
            return true;
        }
    }
    false
}

pub(crate) fn is_lint_excluded(
    path: &Path,
    config_path: Option<&Path>,
    patterns: &[String],
) -> bool {
    let Some(root) = config_path.and_then(Path::parent) else {
        return false;
    };
    LintExcludeMatcher::new(root, patterns).is_excluded(path)
}

#[derive(Debug, Clone)]
struct WorkspaceRoot {
    path: PathBuf,
    source_roots: Vec<PathBuf>,
    excludes: ExcludeMatcher,
}

impl WorkspaceRoot {
    fn new(path: PathBuf, options: &WorkspaceOptions) -> Self {
        let path = absolute_path(path);
        // Lint configuration is selected per effective project context at
        // request time. Only explicit client exclusions belong to the root
        // discovery filter; applying one configuration here would hide files
        // owned by another project from navigation and rename completeness.
        let config_root = path.clone();
        let patterns = options.exclude.clone();
        let mut source_roots = vec![path.clone()];
        for source in &options.source_paths {
            let source_path = PathBuf::from(source);
            let source_path = if source_path.is_absolute() {
                source_path
            } else {
                path.join(source_path)
            };
            let source_path = absolute_path(source_path);
            if !source_roots.contains(&source_path) {
                source_roots.push(source_path);
            }
        }
        Self {
            path: path.clone(),
            source_roots,
            excludes: ExcludeMatcher::new(&path, &config_root, &patterns),
        }
    }

    fn accepts(&self, path: &Path) -> bool {
        let path = absolute_path(path.to_path_buf());
        self.source_roots.iter().any(|source_root| {
            (path.as_path() == source_root.as_path() || path.starts_with(source_root))
                && !self.excludes.is_excluded(&path, source_root)
        })
    }
}

fn production_override_session() -> (OverrideSession, Vec<String>) {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match user_config_path(xdg.as_deref(), home.as_deref()) {
        Ok(path) => (OverrideSession::new(Some(path)), Vec::new()),
        Err(error) => (OverrideSession::new(None), vec![error]),
    }
}

#[derive(Default)]
pub struct Workspace {
    options: WorkspaceOptions,
    overrides: OverrideSession,
    roots: Vec<WorkspaceRoot>,
    index: NavigationIndex,
    open_documents: HashMap<Url, OpenDocument>,
    indexed_files: HashSet<Url>,
    indexed_sizes: HashMap<Url, usize>,
    indexed_bytes: usize,
    disk_stamps: HashMap<Url, DiskStamp>,
    last_used: HashMap<Url, u64>,
    use_clock: u64,
    open_text_bytes: usize,
    pending_diagnostics: HashMap<Url, Instant>,
    // Bounded event overrides are needed because some clients report a
    // deletion before the filesystem has caught up. They are cleared by a
    // create/change event or as soon as the observed stamp changes.
    deleted_overrides: HashMap<Url, Option<DiskStamp>>,
    file_cap_warning_sent: bool,
    total_cap_warning_sent: bool,
    contexts: HashMap<ContextKey, ContextState>,
    document_contexts: HashMap<Url, ContextKey>,
    open_document_contexts: HashMap<Url, ContextKey>,
    document_owners: HashMap<Url, KnownDocumentOwner>,
    owner_last_used: HashMap<Url, u64>,
    project_selections: ProjectSelections,
    directory_catalogues: HashMap<PathBuf, DirectoryCatalogue>,
    filename_catalogues: HashMap<PathBuf, FilenameCatalogue>,
    package_catalogues: HashMap<PackageCatalogueKey, PackageCatalogue>,
    package_catalogue_epoch: u64,
    package_metadata_cache: HashMap<PackageMetadataKey, CachedPackageMetadata>,
    warnings: Vec<String>,
    source_generation: u64,
    configuration_generation: u64,
}

impl Workspace {
    pub fn new(roots: Vec<PathBuf>, options: WorkspaceOptions) -> Self {
        let (overrides, warnings) = production_override_session();
        let mut workspace = Self::with_override_session(roots, options, overrides);
        for warning in warnings {
            workspace.warn(warning);
        }
        workspace
    }

    /// Construct a workspace with an explicitly captured Delphi override
    /// session. The supplied session is the only source of override
    /// configuration for this workspace.
    pub fn with_override_session(
        roots: Vec<PathBuf>,
        options: WorkspaceOptions,
        overrides: OverrideSession,
    ) -> Self {
        let roots = roots
            .into_iter()
            .map(|root| WorkspaceRoot::new(root, &options))
            .collect();
        let mut workspace = Self {
            options,
            overrides,
            roots,
            ..Self::default()
        };
        if let Err(error) = workspace.overrides.effective_for(None, None) {
            workspace.warn(error);
        }
        let initial_roots = workspace
            .roots
            .iter()
            .map(|root| root.path.clone())
            .collect::<Vec<_>>();
        for root in initial_roots {
            if let Err(error) = workspace.overrides.capture_workspace(&root) {
                workspace.warn(error);
            }
        }
        workspace
    }

    /// Kept as a compatibility no-op for callers of the original workspace
    /// API. Source discovery is request driven; initialization must not parse
    /// an entire workspace.
    pub fn scan(&mut self) {}

    /// Kept as a compatibility no-op. Navigation revalidates only its source
    /// and the dependencies it actually traverses.
    pub fn refresh_for_navigation(&mut self) {}

    fn project_options(&self) -> ProjectOptions {
        ProjectOptions {
            project_file: self.options.project_file.clone(),
            build_config: self.options.build_config.clone(),
            platform: self.options.platform.clone(),
            source_paths: self.options.source_paths.clone(),
        }
    }

    fn workspace_root_paths(&self) -> Vec<PathBuf> {
        self.roots.iter().map(|root| root.path.clone()).collect()
    }

    /// Return configuration candidates that may affect any known context.
    /// Candidates above a nested workspace root must be registered explicitly
    /// because a workspace-relative glob cannot observe them.
    pub(crate) fn configuration_watch_paths(&self) -> Vec<PathBuf> {
        let roots = self.workspace_root_paths();
        let mut paths = Vec::new();

        for root in &self.roots {
            add_configuration_watch_directory(&mut paths, &root.path);
            if let Ok(directories) = config_directories(
                &root.path.join("__pascal_lsp_configuration_probe__.pas"),
                None,
                &roots,
            ) {
                add_configuration_watch_directories(&mut paths, directories);
            }
        }
        if let Some(project_file) = &self.options.project_file {
            let project_files = if project_file.is_absolute() || roots.is_empty() {
                vec![project_file.clone()]
            } else {
                roots.iter().map(|root| root.join(project_file)).collect()
            };
            for project_file in project_files {
                let project_directory = project_file.parent();
                let probe = project_directory
                    .map(|directory| directory.join("__pascal_lsp_configuration_probe__.pas"))
                    .unwrap_or_else(|| project_file.clone());
                if let Ok(directories) = config_directories(&probe, project_directory, &roots) {
                    add_configuration_watch_directories(&mut paths, directories);
                }
            }
        }
        for scope in self.project_selections.keys() {
            if let Ok(directories) = config_directories(
                &scope.join("__pascal_lsp_configuration_probe__.pas"),
                Some(scope),
                &roots,
            ) {
                add_configuration_watch_directories(&mut paths, directories);
            }
        }
        for state in self.contexts.values() {
            for path in state.watched_paths.keys() {
                if is_live_configuration_file(path) {
                    add_configuration_watch_path(&mut paths, path.clone());
                }
            }
        }
        for owner in self.document_owners.values() {
            for path in owner.state.watched_paths.keys() {
                if is_live_configuration_file(path) {
                    add_configuration_watch_path(&mut paths, path.clone());
                }
            }
        }

        paths.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
        paths
    }

    fn configuration_project_directory(
        &self,
        path: &Path,
        context: &ProjectContext,
        context_key: Option<&ContextKey>,
        candidates: &ProjectCandidates,
    ) -> Option<PathBuf> {
        context
            .project_file
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| context_key.and_then(|key| key.project_scope.clone()))
            .or_else(|| {
                self.runtime_selection_for_path_default(path)
                    .map(|(scope, _)| scope)
            })
            .or_else(|| candidates.directory.clone())
    }

    /// Resolve a navigation request, loading only the requested source and a
    /// bounded dependency closure.
    pub fn navigate(
        &mut self,
        uri: &Url,
        position: Position,
        target: NavigationTarget,
    ) -> Vec<Location> {
        self.package_catalogue_epoch = self.package_catalogue_epoch.wrapping_add(1);
        let context_key = match self.context_for_uri(uri) {
            Ok(context_key) => context_key,
            Err(error) => {
                self.warn(error);
                return Vec::new();
            }
        };
        if self.context_has_invalid_project_selection(&context_key) {
            self.warn(format!(
                "project selection is invalid; navigation is unavailable for {uri}"
            ));
            return Vec::new();
        }
        if self.context_has_override_error(&context_key) {
            self.warn(format!(
                "project override configuration is invalid; navigation is unavailable for {uri}"
            ));
            return Vec::new();
        }
        if !self.ensure_supported_with_context(uri, &context_key) {
            return Vec::new();
        }
        let empty_pins = HashSet::new();
        match self.load_source(uri, &context_key, &empty_pins) {
            Ok(true) => {}
            Ok(false) => return Vec::new(),
            Err(error) => {
                self.warn(error);
                return Vec::new();
            }
        }

        for _attempt in 0..2 {
            let mut pinned = HashSet::new();
            pinned.insert(uri.clone());
            let locations = match self.resolve_navigation_once(
                uri,
                position,
                target,
                &context_key,
                &mut pinned,
            ) {
                Ok(locations) => locations,
                Err(error) => {
                    self.warn(error);
                    return Vec::new();
                }
            };
            let changed = match self.revalidate_pinned(&pinned, &context_key) {
                Ok(changed) => changed,
                Err(error) => {
                    self.warn(error);
                    return Vec::new();
                }
            };
            if !changed {
                return locations;
            }
        }
        self.warn(format!(
            "navigation source changed repeatedly while resolving {}; result is incomplete",
            uri
        ));
        Vec::new()
    }

    pub fn open_document(&mut self, uri: Url, text: String, version: i32) -> Result<(), String> {
        if !self.open_documents.contains_key(&uri) {
            let preserve_mapped_owner =
                uri.to_file_path()
                    .ok()
                    .map(absolute_path)
                    .is_some_and(|path| {
                        self.document_owners.get(&uri).is_some_and(|owner| {
                            owner.origin == OwnerOrigin::Inherited
                                && self.mapped_path_is_readable(&path, &owner.key)
                        })
                    });
            if !preserve_mapped_owner {
                // A legacy disk dependency may have been indexed under another
                // request's project context. Select the editor buffer's own
                // context when it first becomes authoritative instead of
                // inheriting that binding.
                self.document_contexts.remove(&uri);
                self.document_owners.remove(&uri);
                self.owner_last_used.remove(&uri);
            }
        }
        let context_key = self.context_for_uri(&uri)?;
        if !self.ensure_supported_with_context(&uri, &context_key) {
            return Err(format!(
                "document is outside configured source paths: {uri}"
            ));
        }
        if let Some(previous) = self.open_documents.get(&uri) {
            if version <= previous.version {
                eprintln!(
                    "pascal-lsp: warning: ignored non-monotonic version {} for {} (current {})",
                    version, uri, previous.version
                );
                return Ok(());
            }
        }
        if let Err(reason) = self.validate_open_text(&uri, &text) {
            self.reject_open_document(uri, version, reason);
            return Ok(());
        }
        let open_bytes_after = self.open_bytes_after(&uri, text.len());
        let mut pinned = HashSet::new();
        pinned.insert(uri.clone());
        if !self.make_room_for(&uri, text.len(), &pinned, open_bytes_after) {
            let retained_files = self.retained_file_count_after_open(&uri);
            let reason = if retained_files > self.options.limits.max_files {
                format!(
                    "retaining the open document would use {retained_files} files; the configured file limit is {}",
                    self.options.limits.max_files
                )
            } else {
                format!(
                    "retaining the open document would exceed the configured total source limit of {} bytes",
                    self.options.limits.max_total_bytes
                )
            };
            self.reject_open_document(uri, version, reason);
            return Ok(());
        }
        self.accept_open_document(uri, text, version, context_key)
    }

    pub fn change_document(&mut self, uri: Url, text: String, version: i32) -> Result<(), String> {
        let Some(previous) = self.open_documents.get(&uri) else {
            return Err(format!("received didChange for unopened document {uri}"));
        };
        if version <= previous.version {
            eprintln!(
                "pascal-lsp: warning: ignored non-monotonic version {} for {} (current {})",
                version, uri, previous.version
            );
            return Ok(());
        }
        if let Err(reason) = self.validate_open_text(&uri, &text) {
            self.reject_open_document(uri, version, reason);
            return Ok(());
        }
        let context_key = self.context_for_uri(&uri)?;
        let open_bytes_after = self.open_bytes_after(&uri, text.len());
        let mut pinned = HashSet::new();
        pinned.insert(uri.clone());
        if !self.make_room_for(&uri, text.len(), &pinned, open_bytes_after) {
            let retained_files = self.retained_file_count_after_open(&uri);
            let reason = if retained_files > self.options.limits.max_files {
                format!(
                    "retaining the open document would use {retained_files} files; the configured file limit is {}",
                    self.options.limits.max_files
                )
            } else {
                format!(
                    "retaining the open document would exceed the configured total source limit of {} bytes",
                    self.options.limits.max_total_bytes
                )
            };
            self.reject_open_document(uri, version, reason);
            return Ok(());
        }
        self.accept_open_document(uri, text, version, context_key)
    }

    pub fn save_document(&mut self, uri: &Url, saved_text: Option<String>) -> Result<(), String> {
        if let Some(document) = self.open_documents.get(uri) {
            if let Some(reason) = &document.rejection {
                return Err(format!("document rejected: {reason}"));
            }
            if let Some(text) = saved_text {
                if let Err(reason) = self.validate_open_text(uri, &text) {
                    let version = document.version;
                    self.reject_open_document(uri.clone(), version, reason);
                    return Ok(());
                }
                let version = document.version;
                let context_key = self.context_for_uri(uri)?;
                let open_bytes_after = self.open_bytes_after(uri, text.len());
                let mut pinned = HashSet::new();
                pinned.insert(uri.clone());
                if !self.make_room_for(uri, text.len(), &pinned, open_bytes_after) {
                    return Err(format!(
                        "retaining the saved document would exceed the configured source limits for {uri}"
                    ));
                }
                self.accept_open_document(uri.clone(), text, version, context_key)?;
            } else {
                self.refresh_loaded_disk(uri);
                self.schedule_diagnostics(uri.clone());
            }
        } else {
            // A save notification without an open overlay must never make the
            // optional text payload authoritative over the disk file.
            self.invalidate_metadata_for_uri(uri);
            self.refresh_loaded_disk(uri);
        }
        Ok(())
    }

    pub fn close_document(&mut self, uri: &Url) -> bool {
        let was_open = if let Some(document) = self.open_documents.remove(uri) {
            if let Some(text) = document.text {
                self.open_text_bytes = self.open_text_bytes.saturating_sub(text.len());
            }
            true
        } else {
            false
        };
        self.pending_diagnostics.remove(uri);
        if was_open {
            self.bump_source_generation();
            self.open_document_contexts.remove(uri);
            self.disk_stamps.remove(uri);
            self.refresh_loaded_disk(uri);
        }
        was_open
    }

    fn accept_open_document(
        &mut self,
        uri: Url,
        text: String,
        version: i32,
        context_key: ContextKey,
    ) -> Result<(), String> {
        self.bump_source_generation();
        let text_len = text.len();
        let source_for_index = text.clone();
        if let Some(previous) = self.open_documents.get(&uri) {
            if let Some(previous_text) = &previous.text {
                self.open_text_bytes = self.open_text_bytes.saturating_sub(previous_text.len());
            }
        }
        self.open_text_bytes = self.open_text_bytes.saturating_add(text_len);
        self.open_documents.insert(
            uri.clone(),
            OpenDocument {
                text: Some(text),
                version,
                rejection: None,
            },
        );
        self.disk_stamps.remove(&uri);
        self.open_document_contexts
            .insert(uri.clone(), context_key.clone());
        self.set_document_context(&uri, &context_key)?;
        self.index_source(&uri, source_for_index, None, &context_key, &HashSet::new())?;
        self.schedule_diagnostics(uri);
        Ok(())
    }

    fn reject_open_document(&mut self, uri: Url, version: i32, reason: String) {
        self.bump_source_generation();
        if let Some(previous) = self.open_documents.get(&uri) {
            if let Some(previous_text) = &previous.text {
                self.open_text_bytes = self.open_text_bytes.saturating_sub(previous_text.len());
            }
        }
        self.remove_indexed(&uri);
        self.open_document_contexts.remove(&uri);
        self.pending_diagnostics.remove(&uri);
        let message = format!("document rejected: {reason}");
        eprintln!("pascal-lsp: warning: {uri}: {message}");
        self.open_documents.insert(
            uri.clone(),
            OpenDocument {
                text: None,
                version,
                rejection: Some(message),
            },
        );
        self.pending_diagnostics.insert(uri, Instant::now());
    }

    fn validate_open_text(&self, uri: &Url, text: &str) -> Result<(), String> {
        if text.len() > self.options.limits.max_file_bytes {
            return Err(self.file_too_large_message(uri, text.len()));
        }

        let previous_open_bytes = self
            .open_documents
            .get(uri)
            .and_then(|document| document.text.as_ref())
            .map_or(0, String::len);
        let open_bytes_after = self
            .open_text_bytes
            .saturating_sub(previous_open_bytes)
            .saturating_add(text.len());
        if open_bytes_after > self.options.limits.max_total_bytes {
            return Err(format!(
                "open document text uses {open_bytes_after} bytes; the configured total source limit is {}",
                self.options.limits.max_total_bytes
            ));
        }
        Ok(())
    }

    fn open_bytes_after(&self, uri: &Url, incoming_len: usize) -> usize {
        let previous = self
            .open_documents
            .get(uri)
            .and_then(|document| document.text.as_ref())
            .map_or(0, String::len);
        self.open_text_bytes
            .saturating_sub(previous)
            .saturating_add(incoming_len)
    }

    fn retained_file_count_after_open(&self, uri: &Url) -> usize {
        let mut count = self.indexed_files.len();
        for (open_uri, document) in &self.open_documents {
            if open_uri != uri && document.text.is_some() && !self.indexed_files.contains(open_uri)
            {
                count = count.saturating_add(1);
            }
        }
        if !self.indexed_files.contains(uri) {
            count = count.saturating_add(1);
        }
        count
    }

    pub fn file_event(&mut self, uri: &Url, change: FileChange) {
        let override_changed = uri
            .to_file_path()
            .is_ok_and(|path| is_immutable_override_file(&path));
        if !override_changed {
            self.bump_source_generation();
        }
        let configuration_changed = is_configuration_path(uri);
        if configuration_changed && !override_changed {
            self.bump_configuration_generation();
        }
        self.invalidate_metadata_for_uri(uri);
        self.invalidate_directory_for_uri(uri);
        match change {
            FileChange::Deleted => self.remember_deleted(uri),
            FileChange::Created | FileChange::Changed => {
                self.deleted_overrides.remove(uri);
            }
        }
        if self.open_documents.contains_key(uri) {
            // The editor buffer remains authoritative until didClose.
            self.schedule_diagnostics(uri.clone());
            return;
        }
        if configuration_changed {
            let open_documents = self
                .open_documents
                .iter()
                .filter_map(|(open_uri, document)| document.text.as_ref().map(|_| open_uri.clone()))
                .collect::<Vec<_>>();
            for open_uri in open_documents {
                self.schedule_diagnostics(open_uri);
            }
        }
        match change {
            FileChange::Deleted => self.remove_indexed(uri),
            FileChange::Created | FileChange::Changed => self.refresh_loaded_disk(uri),
        }
    }

    pub fn update_workspace_folders(
        &mut self,
        added: impl IntoIterator<Item = PathBuf>,
        removed: impl IntoIterator<Item = PathBuf>,
    ) {
        self.bump_source_generation();
        self.bump_configuration_generation();
        self.clear_legacy_route_proofs();
        for removed in removed {
            let removed = absolute_path(removed);
            self.roots
                .retain(|root| !paths_equal_ci(&root.path, &removed));
            self.document_owners.retain(|uri, _| {
                uri.to_file_path()
                    .ok()
                    .map(absolute_path)
                    .is_none_or(|path| !path_starts_with_ci(&path, &removed))
            });
            self.owner_last_used
                .retain(|uri, _| self.document_owners.contains_key(uri));
            let to_remove: Vec<Url> = self
                .indexed_files
                .iter()
                .filter(|uri| {
                    uri.to_file_path()
                        .is_ok_and(|path| path_starts_with_ci(&path, &removed))
                })
                .cloned()
                .collect();
            for uri in to_remove {
                self.remove_indexed(&uri);
            }
        }
        for added in added {
            let added = absolute_path(added);
            if !self.roots.iter().any(|root| root.path == added) {
                let root = WorkspaceRoot::new(added, &self.options);
                if let Err(error) = self.overrides.capture_workspace(&root.path) {
                    self.warn(error);
                }
                self.roots.push(root);
            }
        }
        self.contexts.clear();
        self.document_contexts.clear();
        self.open_document_contexts.clear();
        self.directory_catalogues.clear();
        self.filename_catalogues.clear();
        self.package_catalogues.clear();
        self.package_metadata_cache.clear();
        self.deleted_overrides.clear();
    }

    pub fn next_diagnostic_timeout(&self) -> Option<Duration> {
        let now = Instant::now();
        self.pending_diagnostics
            .values()
            .map(|deadline| deadline.saturating_duration_since(now))
            .min()
    }

    pub fn take_due_diagnostics(&mut self) -> Vec<(Url, Option<i32>, Vec<LspDiagnostic>)> {
        let now = Instant::now();
        let due: Vec<Url> = self
            .pending_diagnostics
            .iter()
            .filter_map(|(uri, deadline)| (*deadline <= now).then_some(uri.clone()))
            .collect();
        let mut result = Vec::with_capacity(due.len());
        for uri in due {
            self.pending_diagnostics.remove(&uri);
            let Some(document) = self.open_documents.get(&uri) else {
                continue;
            };
            let version = Some(document.version);
            result.push((uri.clone(), version, self.diagnostics_for(&uri)));
        }
        result
    }

    pub fn formatting_edit(&self, uri: &Url) -> Result<Option<TextEdit>, String> {
        let path = uri
            .to_file_path()
            .map(absolute_path)
            .map_err(|_| format!("not a file URI: {uri}"))?;
        if !is_pascal_path(&path) {
            return Err(format!(
                "unsupported Pascal file extension: {}",
                path.display()
            ));
        }
        let roots = self
            .roots
            .iter()
            .map(|root| root.path.clone())
            .collect::<Vec<_>>();
        let project_options = ProjectOptions {
            project_file: self.options.project_file.clone(),
            build_config: self.options.build_config.clone(),
            platform: self.options.platform.clone(),
            source_paths: self.options.source_paths.clone(),
        };
        let (context_key, context) =
            self.readonly_context_for_uri(uri, &path, &roots, &project_options)?;
        if has_invalid_project_selection(&context) {
            return Err(format!(
                "project selection is invalid; select a current project or Automatic for {uri}"
            ));
        }
        if let Some(error) = context.override_error.as_deref() {
            return Err(format!(
                "project override configuration is invalid for {uri}: {error}"
            ));
        }
        let legacy_route = self.legacy_route_is_current(uri, &path, &context_key);
        if !self.ensure_supported_project_context_with_legacy_route(
            &path,
            &context,
            Some(&context_key),
            legacy_route,
        ) {
            return Err(format!(
                "document is outside configured source paths: {uri}"
            ));
        }
        let source = if let Some(document) = self.open_documents.get(uri) {
            if let Some(reason) = &document.rejection {
                return Err(format!("document rejected: {reason}"));
            }
            document
                .text
                .clone()
                .expect("accepted open documents retain their text")
        } else {
            let entry = context_path_entry(&context, &path)
                .or_else(|| {
                    legacy_route.then_some(ProjectPathEntry {
                        path: path.clone(),
                        provenance: ProjectPathProvenance::LegacyNative,
                    })
                })
                .ok_or_else(|| format!("document is outside configured source paths: {uri}"))?;
            let legacy_payload = matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
                && (legacy_route
                    || project_path_entry_for(&context, &path).is_some_and(|entry| {
                        matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
                    }));
            read_disk_source(
                &path,
                self.options.limits.max_file_bytes,
                &context.read_policy,
                &entry,
                legacy_payload,
            )?
            .text
        };
        if source.len() > self.options.limits.max_file_bytes {
            return Err(self.file_too_large_message(uri, source.len()));
        }
        ensure_safe_tree_depth(&path, source.as_bytes())?;
        let candidates = project_candidates(&path, &roots)?;
        let project_directory =
            self.configuration_project_directory(&path, &context, Some(&context_key), &candidates);
        let directories = config_directories(&path, project_directory.as_deref(), &roots)?;
        let config = resolve_fmt(&directories, 4 * 1024 * 1024)?.value;
        let external_units = if config.uses.group {
            let root = config
                .project_root
                .as_deref()
                .ok_or_else(|| "formatting external-unit scan has no project root".to_string())?;
            let mut external_units =
                scan_external_units(root, &config.uses.external_paths, &self.options.limits)?;
            for project_unit in project_unit_stems(&context, &path) {
                external_units.remove(&project_unit);
            }
            external_units
        } else {
            HashSet::new()
        };
        let formatted = fmt4d::format_source(
            source.as_bytes(),
            &FileInfo::new(path.clone()),
            &config,
            &external_units,
        )
        .map_err(|error| error.to_string())?;
        if formatted == source {
            return Ok(None);
        }
        let end = text::offset_to_position(&source, source.len())
            .ok_or_else(|| "could not compute full-document UTF-16 range".to_string())?;
        Ok(Some(TextEdit::new(
            Range::new(Position::new(0, 0), end),
            formatted,
        )))
    }

    fn refresh_loaded_disk(&mut self, uri: &Url) {
        let Some(context_key) = self.document_contexts.get(uri).cloned() else {
            self.remove_indexed(uri);
            return;
        };
        let pins = HashSet::new();
        if let Err(error) = self.load_source(uri, &context_key, &pins) {
            self.warn(error);
        }
    }

    fn resolve_navigation_once(
        &mut self,
        uri: &Url,
        position: Position,
        target: NavigationTarget,
        context_key: &ContextKey,
        pinned: &mut HashSet<Url>,
    ) -> Result<Vec<Location>, String> {
        // Rebuild the request's bindings from the current source graph. This
        // prevents a deleted or changed dependency from surviving in a stale
        // per-document binding map.
        self.index.clear_import_bindings(uri);
        let mut frontier = vec![uri.clone()];
        let mut visited = HashSet::new();
        let mut initialized = HashSet::from([uri.clone()]);
        let mut work = 0;

        loop {
            let locations = self.index.navigate(uri, position, target);
            if !locations.is_empty() {
                return Ok(locations);
            }
            if frontier.is_empty() || work >= MAX_DEPENDENCY_WORK {
                break;
            }

            let mut next = Vec::new();
            for current in frontier.drain(..) {
                if !visited.insert(current.clone()) {
                    continue;
                }
                work += 1;
                let dependencies = self.load_imports(&current, context_key, pinned)?;
                for dependency in dependencies {
                    if initialized.insert(dependency.clone()) {
                        self.index.clear_import_bindings(&dependency);
                    }
                    next.push(dependency);
                }
                if work >= MAX_DEPENDENCY_WORK {
                    break;
                }
            }
            next.retain(|dependency| !visited.contains(dependency));
            next.sort_by(|left, right| left.as_str().cmp(right.as_str()));
            next.dedup();
            frontier = next;
        }

        if work >= MAX_DEPENDENCY_WORK {
            self.warn(format!(
                "dependency work limit ({MAX_DEPENDENCY_WORK}) reached while resolving {uri}; result is incomplete"
            ));
        }

        Ok(self.index.navigate(uri, position, target))
    }

    fn load_imports(
        &mut self,
        uri: &Url,
        context_key: &ContextKey,
        pinned: &mut HashSet<Url>,
    ) -> Result<Vec<Url>, String> {
        self.load_imports_with_cancel(uri, context_key, pinned, None)
    }

    fn load_imports_with_cancel(
        &mut self,
        uri: &Url,
        context_key: &ContextKey,
        pinned: &mut HashSet<Url>,
        cancel: Option<&AtomicBool>,
    ) -> Result<Vec<Url>, String> {
        check_workspace_cancel(cancel)?;
        let imports = self.index.imports(uri);
        let effective_context_key = self
            .document_contexts
            .get(uri)
            .cloned()
            .unwrap_or_else(|| context_key.clone());
        let Some(context) = self
            .contexts
            .get(&effective_context_key)
            .map(|state| state.context.clone())
        else {
            return Ok(Vec::new());
        };
        let mut bindings = HashMap::new();
        let mut dependencies = Vec::new();
        for import in imports {
            check_workspace_cancel(cancel)?;
            if self.index.conditional_unknown_at(uri, import.span.start) {
                continue;
            }
            let lookup_name = aliased_unit_name(&context, &import.name);
            let Some(dependency) = self.resolve_unit_with_cancel(
                uri,
                &import.name,
                &lookup_name,
                &context,
                &effective_context_key,
                pinned,
                cancel,
            )?
            else {
                self.warn(format!(
                    "could not resolve unit {} imported by {} (searched project context)",
                    import.name, uri
                ));
                continue;
            };
            bindings.insert(import.name, dependency.clone());
            pinned.insert(dependency.clone());
            if !dependencies.contains(&dependency) {
                dependencies.push(dependency);
            }
        }
        check_workspace_cancel(cancel)?;
        self.index.bind_imports(uri, bindings);
        Ok(dependencies)
    }

    fn load_source(
        &mut self,
        uri: &Url,
        context_key: &ContextKey,
        pinned: &HashSet<Url>,
    ) -> Result<bool, String> {
        self.load_source_with_cancel(uri, context_key, pinned, None)
    }

    fn load_source_with_cancel(
        &mut self,
        uri: &Url,
        context_key: &ContextKey,
        pinned: &HashSet<Url>,
        cancel: Option<&AtomicBool>,
    ) -> Result<bool, String> {
        self.load_source_with_legacy_sibling_with_cancel(
            uri,
            context_key,
            pinned,
            None,
            false,
            cancel,
        )
    }

    #[allow(dead_code)]
    fn load_source_with_legacy_sibling(
        &mut self,
        uri: &Url,
        context_key: &ContextKey,
        pinned: &HashSet<Url>,
        legacy_sibling_directory: Option<&Path>,
    ) -> Result<bool, String> {
        self.load_source_with_legacy_sibling_with_cancel(
            uri,
            context_key,
            pinned,
            legacy_sibling_directory,
            false,
            None,
        )
    }

    fn load_source_with_legacy_sibling_with_cancel(
        &mut self,
        uri: &Url,
        context_key: &ContextKey,
        pinned: &HashSet<Url>,
        legacy_sibling_directory: Option<&Path>,
        legacy_search_path_route: bool,
        cancel: Option<&AtomicBool>,
    ) -> Result<bool, String> {
        check_workspace_cancel(cancel)?;
        let path = match uri.to_file_path() {
            Ok(path) => absolute_path(path),
            Err(()) => {
                self.warn(format!("cannot load non-file navigation URI: {uri}"));
                return Ok(false);
            }
        };
        if !is_pascal_path(&path) {
            self.warn(format!(
                "unsupported Pascal dependency path: {}",
                path.display()
            ));
            return Ok(false);
        }
        let Some(context) = self
            .contexts
            .get(context_key)
            .map(|state| state.context.clone())
        else {
            return Ok(false);
        };
        let legacy_sibling_authorized = legacy_sibling_directory.is_some_and(|directory| {
            path.parent()
                .is_some_and(|parent| native_paths_equal(parent, directory))
        });
        let legacy_route_authorized =
            legacy_search_path_route || self.legacy_route_is_current(uri, &path, context_key);
        let entry = match context_path_entry(&context, &path) {
            Some(entry) => entry,
            None if legacy_sibling_authorized || legacy_route_authorized => ProjectPathEntry {
                path: path.clone(),
                provenance: ProjectPathProvenance::LegacyNative,
            },
            None => {
                self.warn(format!(
                    "source path is outside the effective project read roots: {}",
                    path.display()
                ));
                return Ok(false);
            }
        };
        let entry_is_explicitly_restricted =
            !matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
                || project_path_entry_for(&context, &path).is_some();
        let legacy_route_granted = (legacy_sibling_authorized || legacy_search_path_route)
            && !entry_is_explicitly_restricted;
        let explicit_legacy_entry = project_path_entry_for(&context, &path)
            .is_some_and(|entry| matches!(entry.provenance, ProjectPathProvenance::LegacyNative));
        let verified_legacy_payload =
            matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
                && (legacy_route_authorized || legacy_route_granted || explicit_legacy_entry);
        let readable = if verified_legacy_payload {
            context.read_policy.allows_legacy_route_entry(&entry)
        } else {
            self.ensure_supported_project_context_with_legacy_route(
                &path,
                &context,
                Some(context_key),
                legacy_route_authorized || legacy_route_granted || explicit_legacy_entry,
            )
        };
        if !readable {
            self.warn(format!(
                "source path is outside the effective project read roots: {}",
                path.display()
            ));
            return Ok(false);
        }

        if let Some(document) = self.open_documents.get(uri) {
            if let Some(reason) = &document.rejection {
                return Err(format!(
                    "document {uri} was rejected and cannot be used for analysis: {reason}"
                ));
            }
            let Some(source) = document.text.clone() else {
                return Ok(false);
            };
            if self.index.contains(uri) {
                check_workspace_cancel(cancel)?;
                self.touch(uri);
                self.set_document_context(uri, context_key)?;
                if legacy_route_granted {
                    self.remember_legacy_route(uri, context_key, &path);
                }
                return Ok(true);
            }
            return self.index_source_with_cancel(uri, source, None, context_key, pinned, cancel);
        }

        let Some(current_stamp) = disk_stamp(&path) else {
            self.remove_indexed(uri);
            return Ok(false);
        };
        if self.deletion_blocks_load(uri, &path) {
            self.remove_indexed(uri);
            return Ok(false);
        }
        if self.index.contains(uri) && self.disk_stamps.get(uri) == Some(&current_stamp) {
            check_workspace_cancel(cancel)?;
            self.touch(uri);
            self.set_document_context(uri, context_key)?;
            return Ok(true);
        }
        let source = match read_disk_source_with_cancel(
            &path,
            self.options.limits.max_file_bytes,
            &context.read_policy,
            &entry,
            verified_legacy_payload,
            cancel,
        ) {
            Ok(source) => source,
            Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
            Err(error) => {
                self.warn(format!("skipping {}: {error}", path.display()));
                self.remove_indexed(uri);
                return Ok(false);
            }
        };
        let stamp = source.stamp;
        let indexed = self.index_source_with_cancel(
            uri,
            source.text,
            Some(source.bytes),
            context_key,
            pinned,
            cancel,
        )?;
        if indexed {
            self.disk_stamps.insert(uri.clone(), stamp);
            if legacy_route_granted {
                self.remember_legacy_route(uri, context_key, &path);
            }
        }
        Ok(indexed)
    }

    fn index_source(
        &mut self,
        uri: &Url,
        source: String,
        disk_size: Option<usize>,
        context_key: &ContextKey,
        pinned: &HashSet<Url>,
    ) -> Result<bool, String> {
        self.index_source_with_cancel(uri, source, disk_size, context_key, pinned, None)
    }

    fn index_source_with_cancel(
        &mut self,
        uri: &Url,
        source: String,
        disk_size: Option<usize>,
        context_key: &ContextKey,
        pinned: &HashSet<Url>,
        cancel: Option<&AtomicBool>,
    ) -> Result<bool, String> {
        check_workspace_cancel(cancel)?;
        let size = disk_size.unwrap_or(source.len());
        if !self.make_room_for(uri, size, pinned, 0) {
            if !self.open_documents.contains_key(uri) {
                self.warn(format!(
                    "source cache limit prevented retaining {}; navigation is incomplete",
                    uri
                ));
            }
            return Ok(false);
        }
        let defines = self
            .contexts
            .get(context_key)
            .map(|state| state.context.defines.clone())
            .unwrap_or_default();
        let update = match cancel {
            Some(cancel) => {
                self.index
                    .update_with_defines_with_cancel(uri.clone(), source, &defines, cancel)
            }
            None => self
                .index
                .update_with_defines(uri.clone(), source, &defines),
        };
        if let Err(error) = update {
            if error == CANCELLATION_MESSAGE {
                return Err(error);
            }
            self.remove_indexed(uri);
            self.warn(format!("cannot index {uri}: {error}"));
            return Ok(false);
        }
        check_workspace_cancel(cancel)?;

        let old_size = self.indexed_sizes.insert(uri.clone(), size);
        if let Some(old_size) = old_size {
            self.indexed_bytes = self.indexed_bytes.saturating_sub(old_size);
        } else {
            self.indexed_files.insert(uri.clone());
        }
        self.indexed_bytes = self.indexed_bytes.saturating_add(size);
        if let Err(error) = self.set_document_context(uri, context_key) {
            self.remove_indexed(uri);
            return Err(error);
        }
        self.index.clear_import_bindings(uri);
        self.touch(uri);
        Ok(true)
    }

    fn make_room_for(
        &mut self,
        uri: &Url,
        size: usize,
        pinned: &HashSet<Url>,
        additional_bytes: usize,
    ) -> bool {
        loop {
            let old_size = self.indexed_sizes.get(uri).copied().unwrap_or(0);
            let indexed_after = self
                .indexed_bytes
                .saturating_sub(old_size)
                .saturating_add(size);
            let new_file = !self.indexed_files.contains(uri);
            let files_after = self.indexed_files.len() + usize::from(new_file);
            let over_files = files_after > self.options.limits.max_files;
            let over_bytes = indexed_after.saturating_add(additional_bytes)
                > self.options.limits.max_total_bytes;
            if !over_files && !over_bytes {
                return true;
            }

            let Some(victim) = self.oldest_evictable(pinned, uri) else {
                if over_files && !self.file_cap_warning_sent {
                    self.file_cap_warning_sent = true;
                    self.warn(format!(
                        "file limit ({}) reached while resolving {uri}",
                        self.options.limits.max_files
                    ));
                }
                if over_bytes && !self.total_cap_warning_sent {
                    self.total_cap_warning_sent = true;
                    self.warn(format!(
                        "total source limit ({}) reached while resolving {uri}",
                        self.options.limits.max_total_bytes
                    ));
                }
                return false;
            };
            self.remove_indexed(&victim);
        }
    }

    fn oldest_evictable(&self, pinned: &HashSet<Url>, requested: &Url) -> Option<Url> {
        self.indexed_files
            .iter()
            .filter(|uri| {
                *uri != requested
                    && !pinned.contains(*uri)
                    && !self.open_documents.contains_key(*uri)
            })
            .min_by_key(|uri| self.last_used.get(*uri).copied().unwrap_or(0))
            .cloned()
    }

    fn touch(&mut self, uri: &Url) {
        self.use_clock = self.use_clock.saturating_add(1);
        self.last_used.insert(uri.clone(), self.use_clock);
    }

    fn revalidate_pinned(
        &mut self,
        pinned: &HashSet<Url>,
        context_key: &ContextKey,
    ) -> Result<bool, String> {
        let mut changed = false;
        let pins = HashSet::new();
        for uri in pinned {
            if self.open_documents.contains_key(uri) {
                continue;
            }
            let Ok(path) = uri.to_file_path() else {
                continue;
            };
            if disk_stamp(&path) != self.disk_stamps.get(uri).cloned() {
                self.load_source(uri, context_key, &pins)?;
                changed = true;
            }
        }
        Ok(changed)
    }

    fn context_for_uri(&mut self, uri: &Url) -> Result<ContextKey, String> {
        self.context_for_uri_with_cancel(uri, None)
    }

    fn context_for_uri_with_cancel(
        &mut self,
        uri: &Url,
        cancel: Option<&AtomicBool>,
    ) -> Result<ContextKey, String> {
        let path = uri
            .to_file_path()
            .map(absolute_path)
            .map_err(|_| format!("project context requires a file URI: {uri}"))?;
        if !is_pascal_path(&path) {
            return Err(format!(
                "unsupported Pascal document path: {}",
                path.display()
            ));
        }

        let mut rediscover_open_context = false;
        if let Some(existing) = self.open_document_contexts.get(uri).cloned() {
            self.extend_context_watch_paths(&existing, &path, cancel)?;
            if self.context_is_fresh_with_cancel(&existing, cancel)?
                && self.context_matches_current_selection_with_cancel(&path, &existing, cancel)?
            {
                self.remember_document_owner(uri, &existing);
                return Ok(existing);
            }
            self.invalidate_context(&existing);
            rediscover_open_context = true;
        }

        if !rediscover_open_context {
            if let Some(existing) = self.document_contexts.get(uri).cloned() {
                self.extend_context_watch_paths(&existing, &path, cancel)?;
                if self.context_is_fresh_with_cancel(&existing, cancel)?
                    && self
                        .context_matches_current_selection_with_cancel(&path, &existing, cancel)?
                {
                    self.remember_document_owner(uri, &existing);
                    return Ok(existing);
                }
                self.invalidate_context(&existing);
            }
        }

        let roots = self
            .roots
            .iter()
            .map(|root| root.path.clone())
            .collect::<Vec<_>>();
        let project_options = ProjectOptions {
            project_file: self.options.project_file.clone(),
            build_config: self.options.build_config.clone(),
            platform: self.options.platform.clone(),
            source_paths: self.options.source_paths.clone(),
        };
        if let Some(owner) = self.document_owners.get(uri).cloned() {
            if owner.origin != OwnerOrigin::Automatic
                && self.known_owner_selection_is_current_with_cancel(&path, &owner, cancel)?
            {
                return self
                    .restore_known_owner(uri, &path, &owner, &roots, &project_options, cancel)
                    .map_err(|error| {
                        if error == CANCELLATION_MESSAGE {
                            error
                        } else {
                            format!("could not rediscover known project owner for {uri}: {error}")
                        }
                    });
            }
        }
        let discovered = match cancel {
            Some(cancel) => discover_with_selections_and_observations_with_cancel_and_overrides(
                &path,
                &roots,
                &project_options,
                &self.project_selections,
                &self.overrides,
                &self.options.exclude,
                cancel,
            ),
            None => discover_with_selections_and_observations_with_overrides(
                &path,
                &roots,
                &project_options,
                &self.project_selections,
                &self.overrides,
                &self.options.exclude,
            ),
        };
        let discovery = match discovered {
            Ok(discovery) => discovery,
            Err(error) => {
                if error == CANCELLATION_MESSAGE {
                    return Err(error);
                }
                return Err(format!(
                    "could not discover project context for {path:?}: {error}"
                ));
            }
        };
        let ProjectDiscovery {
            context,
            observations,
            candidate_memberships,
        } = discovery;
        for warning in context.warnings.iter().cloned() {
            self.warn(warning);
        }
        let key = self.context_key_for_path_with_cancel(&path, Some(&context), cancel)?;
        self.install_context(
            key.clone(),
            context,
            observations,
            candidate_memberships,
            &path,
            cancel,
        )?;
        self.select_document_context(uri, &key, self.owner_origin_for_context_key(&key));
        Ok(key)
    }

    fn restore_known_owner(
        &mut self,
        uri: &Url,
        path: &Path,
        owner: &KnownDocumentOwner,
        roots: &[PathBuf],
        project_options: &ProjectOptions,
        cancel: Option<&AtomicBool>,
    ) -> Result<ContextKey, String> {
        if self.context_state_is_fresh_with_open_documents(&owner.state, cancel)? {
            self.contexts.insert(owner.key.clone(), owner.state.clone());
            self.select_document_context(uri, &owner.key, owner.origin);
            return Ok(owner.key.clone());
        }

        if let Some(current_owner) = self.document_owners.get_mut(uri) {
            current_owner.legacy_route = None;
        }
        let (key, discovery) =
            self.rediscover_known_owner(path, owner, roots, project_options, cancel)?;
        self.install_context(
            key.clone(),
            discovery.context,
            discovery.observations,
            discovery.candidate_memberships,
            path,
            cancel,
        )?;
        self.select_document_context(uri, &key, owner.origin);
        Ok(key)
    }

    fn rediscover_known_owner(
        &self,
        path: &Path,
        owner: &KnownDocumentOwner,
        roots: &[PathBuf],
        project_options: &ProjectOptions,
        cancel: Option<&AtomicBool>,
    ) -> Result<(ContextKey, ProjectDiscovery), String> {
        let mut options = project_options.clone();
        if let (Some(scope), Some(selected)) = (
            owner.key.selection_scope.as_deref(),
            owner.key.selection_project.as_deref(),
        ) {
            let candidate_status = match cancel {
                Some(cancel) => {
                    selected_project_is_current_with_cancel(scope, selected, Some(cancel))
                }
                None => selected_project_is_current(scope, selected),
            };
            if let Err(error) = &candidate_status {
                if error == CANCELLATION_MESSAGE {
                    return Err(error.clone());
                }
            }
            if !matches!(candidate_status, Ok(true)) {
                let mut warnings = vec![format!(
                    "selected project {} is not a current candidate in {}",
                    selected.display(),
                    scope.display()
                )];
                if let Err(error) = candidate_status {
                    warnings.push(error);
                }
                let discovery = ProjectDiscovery {
                    context: ProjectContext {
                        metadata_files: vec![selected.to_path_buf()],
                        warnings,
                        ..ProjectContext::default()
                    },
                    observations: Vec::new(),
                    candidate_memberships: HashMap::new(),
                };
                let mut key = owner.key.clone();
                key.project_file = None;
                key.config = None;
                key.platform = None;
                key.project_scope = Some(scope.to_path_buf());
                return Ok((key, discovery));
            }
            options.project_file = Some(selected.to_path_buf());
            let context = match cancel {
                Some(cancel) => {
                    discover_with_selections_and_observations_with_cancel_and_overrides(
                        path,
                        roots,
                        &options,
                        &ProjectSelections::new(),
                        &self.overrides,
                        &self.options.exclude,
                        cancel,
                    )?
                }
                None => discover_with_selections_and_observations_with_overrides(
                    path,
                    roots,
                    &options,
                    &ProjectSelections::new(),
                    &self.overrides,
                    &self.options.exclude,
                )?,
            };
            return Ok((
                self.key_for_known_owner(path, owner, &context.context),
                context,
            ));
        }

        if let Some(project_file) = &owner.key.project_file {
            options.project_file = Some(project_file.clone());
            let context = match cancel {
                Some(cancel) => {
                    discover_with_selections_and_observations_with_cancel_and_overrides(
                        path,
                        roots,
                        &options,
                        &ProjectSelections::new(),
                        &self.overrides,
                        &self.options.exclude,
                        cancel,
                    )?
                }
                None => discover_with_selections_and_observations_with_overrides(
                    path,
                    roots,
                    &options,
                    &ProjectSelections::new(),
                    &self.overrides,
                    &self.options.exclude,
                )?,
            };
            return Ok((
                self.key_for_known_owner(path, owner, &context.context),
                context,
            ));
        }

        let context = match cancel {
            Some(cancel) => discover_with_selections_and_observations_with_cancel_and_overrides(
                path,
                roots,
                &options,
                &self.project_selections,
                &self.overrides,
                &self.options.exclude,
                cancel,
            )?,
            None => discover_with_selections_and_observations_with_overrides(
                path,
                roots,
                &options,
                &self.project_selections,
                &self.overrides,
                &self.options.exclude,
            )?,
        };
        Ok((
            self.key_for_known_owner(path, owner, &context.context),
            context,
        ))
    }

    fn key_for_known_owner(
        &self,
        path: &Path,
        owner: &KnownDocumentOwner,
        context: &ProjectContext,
    ) -> ContextKey {
        let mut key = self.context_key_for_path(path, Some(context));
        key.workspace_root = owner.key.workspace_root.clone().or(key.workspace_root);
        key.project_scope = owner.key.project_scope.clone().or(key.project_scope);
        key.selection_scope = owner.key.selection_scope.clone();
        key.selection_project = owner.key.selection_project.clone();
        key
    }

    fn readonly_context_for_uri(
        &self,
        uri: &Url,
        path: &Path,
        roots: &[PathBuf],
        project_options: &ProjectOptions,
    ) -> Result<(ContextKey, ProjectContext), String> {
        if let Some(owner) = self.document_owners.get(uri) {
            if owner.origin != OwnerOrigin::Automatic
                && self.known_owner_selection_is_current(path, owner)
            {
                if self.context_state_is_fresh_with_open_documents(&owner.state, None)? {
                    return Ok((owner.key.clone(), owner.state.context.clone()));
                }
                let discovery =
                    self.rediscover_known_owner(path, owner, roots, project_options, None)?;
                return Ok((discovery.0, discovery.1.context));
            }
        }
        let context = discover_with_selections(
            path,
            roots,
            project_options,
            &self.project_selections,
            &self.overrides,
            &self.options.exclude,
        )?;
        let key = self.context_key_for_path(path, Some(&context));
        Ok((key, context))
    }

    fn known_owner_selection_is_current(&self, path: &Path, owner: &KnownDocumentOwner) -> bool {
        self.known_owner_selection_is_current_with_cancel(path, owner, None)
            .unwrap_or(false)
    }

    fn known_owner_selection_is_current_with_cancel(
        &self,
        path: &Path,
        owner: &KnownDocumentOwner,
        cancel: Option<&AtomicBool>,
    ) -> Result<bool, String> {
        if owner.origin == OwnerOrigin::Automatic {
            // Automatic discovery must be rerun after invalidation. Retaining
            // its old project here would turn a discovered owner into an
            // implicit explicit selection.
            return Ok(false);
        }

        let Some(scope) = owner.key.selection_scope.as_deref() else {
            if let (Some(scope), Some(project_file)) = (
                owner.key.project_scope.as_deref(),
                owner.key.project_file.as_deref(),
            ) {
                if let Some(selected) = self.project_selections.get(scope) {
                    return Ok(paths_equal_ci(selected, project_file));
                }
            }
            // A startup-project owner is retained until a runtime directory
            // selection becomes applicable to this document. Inherited owners
            // follow the same precedence: retain their project identity only
            // when no runtime selection applies to the document.
            return Ok(self
                .runtime_selection_for_path_with_cancel(path, &self.workspace_root_paths(), cancel)?
                .is_none());
        };
        let Some(selected) = owner.key.selection_project.as_deref() else {
            return Ok(false);
        };

        // A selection only applies within its nearest candidate directory.
        // A newly-created nearer project scope therefore invalidates the old
        // retained selection even while the session mapping still exists.
        let candidates =
            project_candidates_with_cancel(path, &self.workspace_root_paths(), cancel)?;
        if candidates
            .directory
            .as_deref()
            .is_some_and(|directory| !paths_equal_ci(directory, scope))
        {
            return Ok(false);
        }

        Ok(self
            .project_selections
            .get(scope)
            .is_some_and(|current| paths_equal_ci(current, selected)))
    }

    fn context_matches_current_selection_with_cancel(
        &self,
        path: &Path,
        key: &ContextKey,
        cancel: Option<&AtomicBool>,
    ) -> Result<bool, String> {
        let Some((scope, selected)) = self.runtime_selection_for_path_with_cancel(
            path,
            &self.workspace_root_paths(),
            cancel,
        )?
        else {
            let Some(scope) = key.selection_scope.as_deref() else {
                return Ok(true);
            };
            let Some(selected) = self.project_selections.get(scope) else {
                return Ok(key.selection_project.is_none());
            };
            return Ok(key
                .selection_project
                .as_deref()
                .is_some_and(|key_project| paths_equal_ci(key_project, selected)));
        };
        Ok(self.context_matches_selection(key, &scope, &selected))
    }

    fn context_matches_selection(&self, key: &ContextKey, scope: &Path, selected: &Path) -> bool {
        if !key
            .selection_scope
            .as_deref()
            .is_some_and(|key_scope| paths_equal_ci(key_scope, scope))
            || !key
                .selection_project
                .as_deref()
                .is_some_and(|key_project| paths_equal_ci(key_project, selected))
        {
            return false;
        }
        let Some(state) = self.contexts.get(key) else {
            return false;
        };
        state
            .context
            .project_file
            .as_deref()
            .is_none_or(|project_file| paths_equal_ci(project_file, selected))
    }

    fn owner_origin_for_context_key(&self, key: &ContextKey) -> OwnerOrigin {
        if key.selection_scope.is_some() || self.options.project_file.is_some() {
            OwnerOrigin::Explicit
        } else {
            OwnerOrigin::Automatic
        }
    }

    fn legacy_route_is_current(&self, uri: &Url, path: &Path, key: &ContextKey) -> bool {
        self.document_owners
            .get(uri)
            .is_some_and(|owner| owner.key == *key && owner.has_legacy_route(path))
    }

    fn remember_document_owner(&mut self, uri: &Url, key: &ContextKey) {
        let origin = self
            .document_owners
            .get(uri)
            .map(|owner| owner.origin)
            .or_else(|| {
                if self.open_documents.contains_key(uri) {
                    Some(self.owner_origin_for_context_key(key))
                } else {
                    Some(OwnerOrigin::Inherited)
                }
            })
            .expect("owner origin is always available");
        self.remember_document_owner_with_origin(uri, key, origin);
    }

    fn remember_document_owner_with_origin(
        &mut self,
        uri: &Url,
        key: &ContextKey,
        origin: OwnerOrigin,
    ) {
        let Some(state) = self.contexts.get(key).cloned() else {
            return;
        };
        let legacy_route = self.document_owners.get(uri).and_then(|owner| {
            (owner.key == *key)
                .then(|| owner.legacy_route.clone())
                .flatten()
                .filter(|route| route.context == *key)
        });
        self.use_clock = self.use_clock.saturating_add(1);
        self.owner_last_used.insert(uri.clone(), self.use_clock);
        self.document_owners.insert(
            uri.clone(),
            KnownDocumentOwner {
                key: key.clone(),
                state,
                origin,
                legacy_route,
            },
        );
        self.trim_document_owners();
    }

    fn remember_legacy_route(&mut self, uri: &Url, key: &ContextKey, path: &Path) {
        let Some(owner) = self.document_owners.get_mut(uri) else {
            return;
        };
        if owner.key == *key {
            owner.legacy_route = Some(LegacyRouteProof {
                source: absolute_path(path.to_path_buf()),
                context: key.clone(),
            });
        }
    }

    fn clear_legacy_route_proofs(&mut self) {
        for owner in self.document_owners.values_mut() {
            owner.legacy_route = None;
        }
    }

    fn refresh_known_owners_for_context(&mut self, key: &ContextKey) {
        let Some(state) = self.contexts.get(key).cloned() else {
            return;
        };
        for owner in self.document_owners.values_mut() {
            if owner.key == *key {
                owner.state = state.clone();
            }
        }
    }

    fn trim_document_owners(&mut self) {
        while self.document_owners.len() > MAX_DOCUMENT_OWNERS {
            let Some(victim) = self
                .document_owners
                .keys()
                .filter(|uri| !self.open_documents.contains_key(*uri))
                .min_by_key(|uri| self.owner_last_used.get(*uri).copied().unwrap_or(0))
                .cloned()
            else {
                break;
            };
            self.document_owners.remove(&victim);
            self.owner_last_used.remove(&victim);
        }
    }

    fn context_key_for_path(&self, path: &Path, context: Option<&ProjectContext>) -> ContextKey {
        let selection = self.runtime_selection_for_path_default(path);
        ContextKey {
            project_file: context.and_then(|context| context.project_file.clone()),
            workspace_root: self.root_for_path(path),
            project_scope: self.project_scope_for_path(path, context),
            selection_scope: selection.as_ref().map(|(scope, _)| scope.clone()),
            selection_project: selection.map(|(_, project)| project),
            config: context.and_then(|context| context.config.clone()),
            platform: context.and_then(|context| context.platform.clone()),
            overrides: context
                .map(|context| context.overrides.clone())
                .unwrap_or_default(),
        }
    }

    fn context_key_for_path_with_cancel(
        &self,
        path: &Path,
        context: Option<&ProjectContext>,
        cancel: Option<&AtomicBool>,
    ) -> Result<ContextKey, String> {
        let selection = self.runtime_selection_for_path_with_cancel(
            path,
            &self.workspace_root_paths(),
            cancel,
        )?;
        let project_scope = if let Some(scope) = context
            .and_then(|context| context.project_file.as_deref())
            .and_then(Path::parent)
        {
            Some(scope.to_path_buf())
        } else {
            self.project_scope_for_path_with_cancel(path, context, cancel)?
        };
        Ok(ContextKey {
            project_file: context.and_then(|context| context.project_file.clone()),
            workspace_root: self.root_for_path(path),
            project_scope,
            selection_scope: selection.as_ref().map(|(scope, _)| scope.clone()),
            selection_project: selection.map(|(_, project)| project),
            config: context.and_then(|context| context.config.clone()),
            platform: context.and_then(|context| context.platform.clone()),
            overrides: context
                .map(|context| context.overrides.clone())
                .unwrap_or_default(),
        })
    }

    fn runtime_selection_for_path(
        &self,
        path: &Path,
        roots: &[PathBuf],
    ) -> Option<(PathBuf, PathBuf)> {
        let candidates = project_candidates(path, roots).ok()?;
        runtime_project_selection(path, &candidates, &self.project_selections)
    }

    fn runtime_selection_for_path_with_cancel(
        &self,
        path: &Path,
        roots: &[PathBuf],
        cancel: Option<&AtomicBool>,
    ) -> Result<Option<(PathBuf, PathBuf)>, String> {
        let candidates = project_candidates_with_cancel(path, roots, cancel)?;
        Ok(runtime_project_selection(
            path,
            &candidates,
            &self.project_selections,
        ))
    }

    fn runtime_selection_for_path_default(&self, path: &Path) -> Option<(PathBuf, PathBuf)> {
        let roots = self
            .roots
            .iter()
            .map(|root| root.path.clone())
            .collect::<Vec<_>>();
        self.runtime_selection_for_path(path, &roots)
    }

    fn project_scope_for_path(
        &self,
        path: &Path,
        context: Option<&ProjectContext>,
    ) -> Option<PathBuf> {
        if let Some(scope) = context
            .and_then(|context| context.project_file.as_deref())
            .and_then(Path::parent)
        {
            return Some(scope.to_path_buf());
        }
        let roots = self
            .roots
            .iter()
            .map(|root| root.path.clone())
            .collect::<Vec<_>>();
        let candidates = project_candidates(path, &roots).ok()?;
        runtime_project_selection(path, &candidates, &self.project_selections)
            .map(|(scope, _)| scope)
            .or(candidates.directory)
    }

    fn project_scope_for_path_with_cancel(
        &self,
        path: &Path,
        context: Option<&ProjectContext>,
        cancel: Option<&AtomicBool>,
    ) -> Result<Option<PathBuf>, String> {
        if let Some(scope) = context
            .and_then(|context| context.project_file.as_deref())
            .and_then(Path::parent)
        {
            return Ok(Some(scope.to_path_buf()));
        }
        let roots = self.workspace_root_paths();
        let candidates = project_candidates_with_cancel(path, &roots, cancel)?;
        Ok(
            runtime_project_selection(path, &candidates, &self.project_selections)
                .map(|(scope, _)| scope)
                .or(candidates.directory),
        )
    }

    fn root_for_path(&self, path: &Path) -> Option<PathBuf> {
        self.roots
            .iter()
            .filter(|root| path_starts_with_ci(path, &root.path))
            .max_by_key(|root| root.path.components().count())
            .map(|root| root.path.clone())
    }

    fn install_context(
        &mut self,
        key: ContextKey,
        context: ProjectContext,
        observations: Vec<ProjectReadObservation>,
        mut candidate_memberships: HashMap<PathBuf, Result<ProjectCandidateMembership, String>>,
        file: &Path,
        cancel: Option<&AtomicBool>,
    ) -> Result<(), String> {
        let mut watched_paths = HashMap::new();
        let mut metadata = context.metadata_files.clone();
        if let Some(project_file) = &context.project_file {
            metadata.push(project_file.clone());
        }
        if let Some(main_source) = &context.main_source {
            metadata.push(main_source.clone());
        }
        let mut project_candidate_memberships = HashMap::new();
        for directory in self.discovery_directories(file) {
            let membership = take_candidate_membership(&mut candidate_memberships, &directory)
                .unwrap_or_else(|| project_candidate_membership(&directory, cancel));
            if let Err(error) = &membership {
                if error == CANCELLATION_MESSAGE {
                    return Err(error.clone());
                }
            }
            project_candidate_memberships.insert(directory, membership);
        }
        let project_directory = context
            .project_file
            .as_deref()
            .and_then(Path::parent)
            .or(key.project_scope.as_deref());
        let roots = self
            .roots
            .iter()
            .map(|root| root.path.clone())
            .collect::<Vec<_>>();
        if let Ok(directories) = config_directories(file, project_directory, &roots) {
            for directory in directories {
                metadata.push(directory.join(".lint4d.toml"));
                metadata.push(directory.join(".fmt4d.toml"));
            }
        }
        metadata.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
        metadata.dedup_by(|left, right| package_paths_equal(left, right));
        for path in metadata {
            let stamp = self
                .project_read_observation_for_path(&key, &path, &observations)
                .map(|observation| Some(path_stamp_from_project_read(&observation.stamp)))
                .unwrap_or_else(|| path_stamp(&path));
            watched_paths.insert(path, stamp);
        }
        if let Some(state) = self.contexts.get_mut(&key) {
            state.context = context;
            merge_project_read_observations(&mut state.project_read_observations, observations);
            state.watched_paths.extend(watched_paths);
            merge_candidate_memberships(
                &mut state.project_candidate_memberships,
                project_candidate_memberships,
            );
        } else {
            self.contexts.insert(
                key,
                ContextState {
                    context,
                    watched_paths,
                    project_candidate_memberships,
                    project_read_observations: observations,
                },
            );
        }
        Ok(())
    }

    fn project_read_observation_for_path<'a>(
        &'a self,
        key: &ContextKey,
        path: &Path,
        new_observations: &'a [ProjectReadObservation],
    ) -> Option<&'a ProjectReadObservation> {
        new_observations
            .iter()
            .find(|observation| package_paths_equal(&observation.path, path))
            .or_else(|| {
                self.contexts.get(key).and_then(|state| {
                    state
                        .project_read_observations
                        .iter()
                        .find(|observation| package_paths_equal(&observation.path, path))
                })
            })
    }

    fn extend_context_watch_paths(
        &mut self,
        key: &ContextKey,
        file: &Path,
        cancel: Option<&AtomicBool>,
    ) -> Result<(), String> {
        let paths = self.discovery_directories(file);
        if let Some(state) = self.contexts.get_mut(key) {
            for path in paths {
                let Entry::Vacant(entry) = state.project_candidate_memberships.entry(path.clone())
                else {
                    continue;
                };
                let membership = project_candidate_membership(&path, cancel);
                if let Err(error) = &membership {
                    if error == CANCELLATION_MESSAGE {
                        return Err(error.clone());
                    }
                }
                entry.insert(membership);
            }
        }
        Ok(())
    }

    fn discovery_directories(&self, file: &Path) -> Vec<PathBuf> {
        let Some(mut directory) = file.parent().map(Path::to_path_buf) else {
            return Vec::new();
        };
        let boundary = self.root_for_path(file);
        let mut result = Vec::new();
        loop {
            result.push(directory.clone());
            if boundary
                .as_ref()
                .is_some_and(|root| paths_equal_ci(root, &directory))
            {
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
        result
    }

    fn context_is_fresh_with_cancel(
        &self,
        key: &ContextKey,
        cancel: Option<&AtomicBool>,
    ) -> Result<bool, String> {
        self.contexts.get(key).map_or(Ok(false), |state| {
            self.context_state_is_fresh_with_open_documents(state, cancel)
        })
    }

    fn context_state_is_fresh_with_open_documents(
        &self,
        state: &ContextState,
        cancel: Option<&AtomicBool>,
    ) -> Result<bool, String> {
        let open_overlay_paths = self
            .open_documents
            .iter()
            .filter_map(|(uri, document)| {
                let path = document
                    .text
                    .as_ref()
                    .and_then(|_| uri.to_file_path().ok())
                    .map(absolute_path)?;
                (!path.exists()
                    && context_path_entry(&state.context, &path).is_some_and(|entry| {
                        matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
                    }))
                .then_some(path)
            })
            .collect::<Vec<_>>();
        context_state_is_fresh_with_cancel_ignoring_paths(state, cancel, &open_overlay_paths)
    }

    fn context_has_open_legacy_overlay(&self, state: &ContextState) -> bool {
        self.open_documents.iter().any(|(uri, document)| {
            document.text.is_some()
                && uri.to_file_path().ok().is_some_and(|path| {
                    let path = absolute_path(path);
                    !path.exists()
                        && context_path_entry(&state.context, &path).is_some_and(|entry| {
                            matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
                        })
                })
        })
    }

    fn context_has_invalid_project_selection(&self, key: &ContextKey) -> bool {
        self.contexts
            .get(key)
            .is_some_and(|state| has_invalid_project_selection(&state.context))
    }

    fn context_has_override_error(&self, key: &ContextKey) -> bool {
        self.contexts
            .get(key)
            .is_some_and(|state| state.context.override_error.is_some())
    }

    fn invalidate_context(&mut self, key: &ContextKey) {
        self.contexts.remove(key);
        for owner in self.document_owners.values_mut() {
            if owner.key == *key {
                owner.legacy_route = None;
            }
        }
        let mut affected: HashSet<Url> = self
            .document_contexts
            .iter()
            .filter_map(|(uri, document_key)| (document_key == key).then_some(uri.clone()))
            .collect();
        affected.extend(
            self.open_document_contexts
                .iter()
                .filter_map(|(uri, document_key)| (document_key == key).then_some(uri.clone())),
        );
        for uri in affected {
            // Project defines participate in the parser projection, so a
            // context change invalidates the indexed source itself, not only
            // its import bindings. Open buffers remain authoritative and are
            // re-indexed from `open_documents` on the next request.
            self.remove_indexed(&uri);
        }
    }

    fn invalidate_metadata_for_uri(&mut self, uri: &Url) {
        let Ok(path) = uri.to_file_path() else {
            return;
        };
        let path = absolute_path(path);
        let affected: Vec<ContextKey> = self
            .contexts
            .iter()
            .filter_map(|(key, state)| {
                state
                    .watched_paths
                    .keys()
                    .any(|watched| paths_equal_ci(watched, &path))
                    .then_some(key.clone())
            })
            .collect();
        for key in affected {
            self.invalidate_context(&key);
        }
    }

    fn invalidate_directory_for_uri(&mut self, uri: &Url) {
        if let Ok(path) = uri.to_file_path() {
            let path = absolute_path(path);
            if let Some(parent) = path.parent() {
                self.directory_catalogues.remove(parent);
            }
            self.package_metadata_cache.retain(|_, cached| {
                !cached
                    .metadata_stamps
                    .iter()
                    .any(|(metadata_path, _)| paths_equal_ci(metadata_path, &path))
            });
        }
        // A filename-only catalogue has no source-content dependency and is
        // cheap to rebuild, while clearing it ensures watcher-less creates and
        // renames are visible without a periodic repository scan.
        self.filename_catalogues.clear();
        self.package_catalogues.clear();
    }

    fn ensure_supported_with_context(&self, uri: &Url, key: &ContextKey) -> bool {
        let Ok(path) = uri.to_file_path() else {
            return false;
        };
        let path = absolute_path(path);
        self.contexts.get(key).is_some_and(|state| {
            self.ensure_supported_project_context_with_legacy_route(
                &path,
                &state.context,
                Some(key),
                self.legacy_route_is_current(uri, &path, key),
            )
        })
    }

    fn ensure_supported_project_context(
        &self,
        path: &Path,
        context: &ProjectContext,
        context_key: Option<&ContextKey>,
    ) -> bool {
        self.ensure_supported_project_context_with_legacy_route(path, context, context_key, false)
    }

    fn ensure_supported_project_context_with_legacy_route(
        &self,
        path: &Path,
        context: &ProjectContext,
        context_key: Option<&ContextKey>,
        legacy_route: bool,
    ) -> bool {
        if !is_pascal_path(path) {
            return false;
        }
        let Some(entry) = context_path_entry(context, path) else {
            return legacy_route
                && context
                    .read_policy
                    .allows_legacy_route_entry(&ProjectPathEntry::legacy(path.to_path_buf()));
        };
        let explicit_legacy = project_path_entry_for(context, path)
            .is_some_and(|entry| matches!(entry.provenance, ProjectPathProvenance::LegacyNative));
        if matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
            && (legacy_route || explicit_legacy)
        {
            return context.read_policy.allows_legacy_route_entry(&entry);
        }
        if !self.project_path_entry_is_readable(path, &entry, context) {
            return false;
        }
        let under_workspace = self.roots.iter().any(|root| {
            root.source_roots
                .iter()
                .any(|source_root| path_starts_with_native(path, source_root))
        });
        !under_workspace
            || self.accepts_path(path)
            || context_key.is_some_and(|key| {
                matches!(entry.provenance, ProjectPathProvenance::Mapped { .. })
                    && self.mapped_path_is_readable(path, key)
            })
    }

    fn project_path_entry_is_readable(
        &self,
        path: &Path,
        entry: &ProjectPathEntry,
        context: &ProjectContext,
    ) -> bool {
        context.read_policy.allows_path(path, &entry.provenance)
    }

    fn mapped_path_is_readable(&self, path: &Path, context_key: &ContextKey) -> bool {
        let path = absolute_path(path.to_path_buf());
        context_key
            .overrides
            .read_roots()
            .into_iter()
            .map(|root| native_mapping_root(&root))
            .any(|root| self.mapped_path_is_readable_under_root(&path, &root, context_key))
    }

    fn mapped_path_is_readable_under_root(
        &self,
        path: &Path,
        root: &Path,
        context_key: &ContextKey,
    ) -> bool {
        path_starts_with_native(path, root)
            && !self.mapped_path_is_excluded(path, root, context_key)
            && safe_path_under_root(path, root)
    }

    fn mapped_path_is_excluded(
        &self,
        path: &Path,
        mapped_root: &Path,
        context_key: &ContextKey,
    ) -> bool {
        if native_relative_path(path, mapped_root)
            .is_some_and(|relative| relative.components().any(is_default_excluded_component))
        {
            return true;
        }
        context_key
            .workspace_root
            .as_deref()
            .and_then(|workspace_root| {
                self.roots
                    .iter()
                    .find(|root| native_paths_equal(&root.path, workspace_root))
            })
            .is_some_and(|workspace_root| workspace_root.excludes.is_excluded(path, mapped_root))
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_unit_with_cancel(
        &mut self,
        current_uri: &Url,
        requested_name: &str,
        lookup_name: &str,
        context: &ProjectContext,
        context_key: &ContextKey,
        pinned: &HashSet<Url>,
        cancel: Option<&AtomicBool>,
    ) -> Result<Option<Url>, String> {
        check_workspace_cancel(cancel)?;
        let mut groups: Vec<(Vec<PathBuf>, bool)> = Vec::new();
        if let Some(explicit) = context.explicit_units.get(lookup_name) {
            groups.push((explicit.clone(), false));
        }

        let current_path = current_uri.to_file_path().ok().map(absolute_path);
        let current_directory = current_path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        let legacy_current_source = current_path.as_deref().is_some_and(|path| {
            context.main_source_entry.as_ref().is_some_and(|entry| {
                matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
                    && native_paths_equal(&entry.path, path)
            }) || context
                .explicit_unit_entries
                .values()
                .flatten()
                .any(|entry| {
                    matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
                        && native_paths_equal(&entry.path, path)
                })
                || self.legacy_route_is_current(current_uri, path, context_key)
        });
        let legacy_sibling_directory = legacy_current_source
            .then_some(current_directory.clone())
            .flatten();
        if let Some(directory) = current_directory {
            check_workspace_cancel(cancel)?;
            groups.extend(
                self.directory_unit_candidate_groups(
                    &directory,
                    lookup_name,
                    &context.unit_namespaces,
                    context_key,
                )
                .into_iter()
                .map(|paths| (paths, false)),
            );
        }
        for directory in &context.search_paths {
            check_workspace_cancel(cancel)?;
            let legacy_search_path = context.search_path_entries.iter().any(|entry| {
                matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
                    && native_paths_equal(&entry.path, directory)
            });
            groups.extend(
                self.directory_unit_candidate_groups(
                    directory,
                    lookup_name,
                    &context.unit_namespaces,
                    context_key,
                )
                .into_iter()
                .map(|paths| (paths, legacy_search_path)),
            );
        }
        if context.project_file.is_none() {
            check_workspace_cancel(cancel)?;
            groups.push((
                self.filename_unit_candidates(lookup_name, context, context_key),
                false,
            ));
        }

        for (candidates, legacy_search_path) in groups {
            check_workspace_cancel(cancel)?;
            let mut paths = candidates;
            let mut unique_paths: Vec<PathBuf> = Vec::with_capacity(paths.len());
            for path in paths.drain(..) {
                if !unique_paths
                    .iter()
                    .any(|existing| paths_equal_ci(existing, &path))
                {
                    unique_paths.push(path);
                }
            }
            let mut valid = Vec::new();
            for path in unique_paths {
                check_workspace_cancel(cancel)?;
                let Ok(candidate_uri) = Url::from_file_path(&path) else {
                    continue;
                };
                if candidate_uri == *current_uri {
                    continue;
                }
                let legacy_search_path_route = legacy_search_path
                    && context.search_path_entries.iter().any(|entry| {
                        matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
                            && path_starts_with_native(&path, &entry.path)
                    });
                if !self.load_source_with_legacy_sibling_with_cancel(
                    &candidate_uri,
                    context_key,
                    pinned,
                    legacy_sibling_directory.as_deref(),
                    legacy_search_path_route,
                    cancel,
                )? {
                    continue;
                }
                let Some(unit_name) = self.index.unit_name(&candidate_uri) else {
                    continue;
                };
                if unit_name_matches(&unit_name, requested_name, lookup_name, context) {
                    valid.push(candidate_uri);
                }
            }
            valid.sort_by(|left, right| left.as_str().cmp(right.as_str()));
            valid.dedup();
            match valid.len() {
                0 => {}
                1 => return Ok(valid.pop()),
                _ => {
                    self.warn(format!(
                        "ambiguous unit {requested_name} in the current project context: {}",
                        valid
                            .iter()
                            .map(Url::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                    return Ok(None);
                }
            }
        }
        check_workspace_cancel(cancel)?;
        let package_lookup = self.package_unit_candidates(
            requested_name,
            lookup_name,
            context,
            context_key,
            cancel,
        )?;
        self.retain_package_observations(context_key, package_lookup.observations.clone());
        self.merge_metadata_observations(context_key, &package_lookup.metadata_observations);
        for path in &package_lookup.metadata_paths {
            self.watch_package_path(context_key, path);
        }
        if !package_lookup.complete {
            for warning in package_lookup.warnings {
                self.warn(warning);
            }
            return Ok(None);
        }
        let mut valid = Vec::new();
        for path in package_lookup.candidates {
            check_workspace_cancel(cancel)?;
            let Ok(candidate_uri) = Url::from_file_path(&path) else {
                continue;
            };
            if candidate_uri == *current_uri {
                continue;
            }
            if !self.load_source_with_cancel(&candidate_uri, context_key, pinned, cancel)? {
                continue;
            }
            let Some(unit_name) = self.index.unit_name(&candidate_uri) else {
                continue;
            };
            if unit_name_matches(&unit_name, requested_name, lookup_name, context) {
                valid.push(candidate_uri);
            }
        }
        valid.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        valid.dedup();
        match valid.len() {
            0 => {
                for warning in package_lookup.warnings {
                    self.warn(warning);
                }
            }
            1 => return Ok(valid.pop()),
            _ => {
                self.warn(format!(
                    "ambiguous unit {requested_name} in the named source packages: {}",
                    valid
                        .iter()
                        .map(Url::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                return Ok(None);
            }
        }
        if context.project_file.is_some()
            && !context.explicit_units.is_empty()
            && !context.explicit_units.contains_key(lookup_name)
        {
            self.warn(format!(
                "unsupported unit filename mapping for {requested_name}: no explicit project filename mapping; declaration filename mismatch cannot be inferred safely"
            ));
        }
        Ok(None)
    }

    fn directory_unit_candidate_groups(
        &mut self,
        directory: &Path,
        unit_name: &str,
        namespaces: &[String],
        context_key: &ContextKey,
    ) -> Vec<Vec<PathBuf>> {
        let directory = absolute_path(directory.to_path_buf());
        let mut groups = unit_filename_candidate_tiers(unit_name, namespaces)
            .into_iter()
            .map(|names| self.filename_candidates_for_names(&directory, &names, context_key))
            .collect::<Vec<_>>();
        let parts: Vec<&str> = unit_name
            .split('.')
            .filter(|part| !part.is_empty())
            .collect();
        if parts.len() > 1 {
            let nested = parts[..parts.len() - 1]
                .iter()
                .fold(directory.clone(), |path, part| path.join(part));
            if let Some(nested) = resolve_case_insensitive_path(&nested) {
                groups.extend(
                    unit_filename_candidate_tiers(parts.last().copied().unwrap_or_default(), &[])
                        .into_iter()
                        .map(|names| {
                            self.filename_candidates_for_names(&nested, &names, context_key)
                        }),
                );
            }
        }
        groups
    }

    fn package_unit_candidates(
        &mut self,
        requested_name: &str,
        lookup_name: &str,
        context: &ProjectContext,
        context_key: &ContextKey,
        cancel: Option<&AtomicBool>,
    ) -> Result<PackageLookup, String> {
        check_workspace_cancel(cancel)?;
        let mut lookup = PackageLookup {
            complete: true,
            ..PackageLookup::default()
        };
        if context.packages.is_empty() {
            return Ok(lookup);
        }
        if context.packages.len() > MAX_PACKAGE_LOOKUPS {
            lookup.complete = false;
            lookup.warnings.push(format!(
                "named package lookup limit ({MAX_PACKAGE_LOOKUPS}) reached while resolving {requested_name}"
            ));
            return Ok(lookup);
        }

        let package_names = context.packages.to_vec();
        for package_name in &package_names {
            check_workspace_cancel(cancel)?;
            let (descriptors, catalogue_complete) = self.package_descriptors(
                &package_names,
                package_name,
                context,
                context_key,
                cancel,
            )?;
            if !catalogue_complete {
                lookup.complete = false;
                lookup.warnings.push(format!(
                    "source for package {package_name} was not found because its bounded source catalogue was incomplete; compiled-only package skipped"
                ));
                lookup.candidates.clear();
                return Ok(lookup);
            }
            if descriptors.is_empty() {
                lookup.warnings.push(format!(
                    "source for package {package_name} was not found under the configured workspace/source roots; compiled-only package skipped"
                ));
                continue;
            }
            if descriptors.len() > 1 {
                lookup.warnings.push(format!(
                    "ambiguous package {package_name}; matching source descriptors were found: {}",
                    descriptors
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                continue;
            }

            let descriptor = &descriptors[0];
            let (metadata, observations) =
                match self.cached_package_metadata(descriptor, context, cancel) {
                    Ok(metadata) => metadata,
                    Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
                    Err(error) => {
                        lookup.warnings.push(format!(
                            "source package {package_name} was skipped: {error}"
                        ));
                        continue;
                    }
                };
            merge_project_read_observations(&mut lookup.observations, observations);
            lookup.metadata_paths.push(descriptor.clone());
            lookup
                .metadata_paths
                .extend(metadata.metadata_files.iter().cloned());
            lookup
                .metadata_observations
                .extend(metadata.metadata_observations.iter().cloned());
            let mut matched_mapping = false;
            for (unit_name, entries) in &metadata.unit_entries {
                if !package_unit_name_matches(unit_name, requested_name, lookup_name, context) {
                    continue;
                }
                matched_mapping = true;
                for entry in entries {
                    if !context.read_policy.allows_location(entry) {
                        lookup.warnings.push(format!(
                            "source package {package_name} maps {requested_name} outside configured workspace/source roots; skipped: {}",
                            entry.path.display()
                        ));
                        continue;
                    }
                    let path = &entry.path;
                    if !lookup
                        .metadata_paths
                        .iter()
                        .any(|existing| package_paths_equal(existing, path))
                    {
                        lookup.metadata_paths.push(path.clone());
                    }
                    if lookup
                        .candidates
                        .iter()
                        .any(|existing| package_paths_equal(existing, path))
                    {
                        continue;
                    }
                    if lookup.candidates.len() >= MAX_PACKAGE_UNIT_CANDIDATES {
                        lookup.complete = false;
                        lookup.warnings.push(format!(
                            "package unit candidate limit ({MAX_PACKAGE_UNIT_CANDIDATES}) reached while resolving {requested_name}"
                        ));
                        lookup.candidates.clear();
                        return Ok(lookup);
                    }
                    lookup.candidates.push(path.clone());
                }
            }
            if !matched_mapping {
                lookup.warnings.push(format!(
                    "source package {package_name} has no contains mapping for {requested_name}"
                ));
            }
            lookup.warnings.extend(metadata.warnings.iter().cloned());
        }
        Ok(lookup)
    }

    fn package_descriptors(
        &mut self,
        requested_names: &[String],
        package_name: &str,
        context: &ProjectContext,
        context_key: &ContextKey,
        cancel: Option<&AtomicBool>,
    ) -> Result<(Vec<PathBuf>, bool), String> {
        check_workspace_cancel(cancel)?;
        let requested_names: HashSet<String> = requested_names
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect();
        let key = package_name.to_ascii_lowercase();
        let roots = self.package_catalogue_roots(context, context_key);

        for root in &roots {
            self.package_catalogue(context_key, root, &requested_names, cancel)?;
        }
        check_workspace_cancel(cancel)?;
        let (mut descriptors, complete) =
            self.catalogued_package_descriptors(context_key, &roots, &key);

        descriptors.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
        descriptors.dedup_by(|left, right| package_paths_equal(left, right));
        Ok((descriptors, complete))
    }

    fn catalogued_package_descriptors(
        &self,
        context_key: &ContextKey,
        roots: &[PathBuf],
        package_name: &str,
    ) -> (Vec<PathBuf>, bool) {
        let mut descriptors: Vec<PathBuf> = Vec::new();
        let mut complete = true;
        for root in roots {
            let root = absolute_path(root.clone());
            let key = PackageCatalogueKey {
                context: context_key.clone(),
                root,
            };
            let Some(catalogue) = self.package_catalogues.get(&key) else {
                complete = false;
                continue;
            };
            complete &= catalogue.complete;
            if let Some(paths) = catalogue.entries.get(package_name) {
                for path in paths {
                    if !descriptors
                        .iter()
                        .any(|existing| package_paths_equal(existing, path))
                    {
                        descriptors.push(path.clone());
                    }
                }
            }
        }
        (descriptors, complete)
    }

    fn package_catalogue_roots(
        &self,
        context: &ProjectContext,
        context_key: &ContextKey,
    ) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        for workspace_root in &self.roots {
            for root in &workspace_root.source_roots {
                if !roots.iter().any(|existing| existing == root) {
                    roots.push(root.clone());
                }
            }
        }
        for configured_root in context_key.overrides.read_roots() {
            let configured_root = absolute_path(configured_root);
            let root = native_mapping_root(&configured_root);
            if fs::symlink_metadata(&root).is_err()
                && !context_uses_mapped_root(context, &configured_root, &root)
            {
                continue;
            }
            if !roots
                .iter()
                .any(|existing| package_paths_equal(existing, &root))
            {
                roots.push(root);
            }
        }
        roots
    }

    #[allow(dead_code)]
    fn package_source_is_configured(&self, path: &Path) -> bool {
        self.roots.iter().any(|workspace_root| {
            workspace_root
                .source_roots
                .iter()
                .any(|root| path_starts_with_ci(path, root))
        })
    }

    fn cached_package_metadata(
        &mut self,
        path: &Path,
        context: &ProjectContext,
        cancel: Option<&AtomicBool>,
    ) -> Result<(PackageMetadata, Vec<ProjectReadObservation>), String> {
        check_workspace_cancel(cancel)?;
        let key = PackageMetadataKey {
            descriptor: path.to_path_buf(),
            overrides: context.overrides.clone(),
            read_policy: context.read_policy.clone(),
            config: context.config.clone(),
            platform: context.platform.clone(),
        };
        let stamp = path_stamp(path);
        if let Some(cached) = self.package_metadata_cache.get(&key) {
            if cached
                .metadata_stamps
                .iter()
                .all(|(metadata_path, metadata_stamp)| path_stamp(metadata_path) == *metadata_stamp)
            {
                let result = cached.result.clone();
                let observations = cached.observations.clone();
                self.use_clock = self.use_clock.saturating_add(1);
                if let Some(cached) = self.package_metadata_cache.get_mut(&key) {
                    cached.last_used = self.use_clock;
                }
                check_workspace_cancel(cancel)?;
                return result.map(|metadata| (metadata, observations));
            }
        }
        let previous_metadata_paths = self
            .package_metadata_cache
            .get(&key)
            .map(|cached| {
                cached
                    .metadata_stamps
                    .iter()
                    .map(|(metadata_path, _)| metadata_path.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![path.to_path_buf()]);
        let options = ProjectOptions {
            build_config: key.config.clone(),
            platform: key.platform.clone(),
            ..ProjectOptions::default()
        };
        let entry = match context_path_entry(context, path) {
            Some(entry) => entry,
            None => {
                return Err(format!(
                    "package metadata {} is outside the effective project read roots",
                    path.display()
                ));
            }
        };
        let package_read = read_package_metadata_with_observations(
            path,
            &options,
            &key.overrides,
            &key.read_policy,
            &entry,
        );
        let (result, observations) = match package_read {
            Ok(read) => (Ok(read.metadata), read.observations),
            Err(error) => (Err(error), Vec::new()),
        };
        check_workspace_cancel(cancel)?;
        let metadata_paths = result
            .as_ref()
            .map(|metadata| metadata.metadata_files.clone())
            .unwrap_or(previous_metadata_paths);
        let metadata_stamps = metadata_paths
            .into_iter()
            .map(|metadata_path| {
                let metadata_stamp = if package_paths_equal(&metadata_path, path) {
                    stamp.clone()
                } else if let Some(observation) = observations
                    .iter()
                    .find(|observation| package_paths_equal(&observation.path, &metadata_path))
                {
                    Some(path_stamp_from_project_read(&observation.stamp))
                } else {
                    path_stamp(&metadata_path)
                };
                (metadata_path, metadata_stamp)
            })
            .collect();
        self.use_clock = self.use_clock.saturating_add(1);
        self.package_metadata_cache.insert(
            key,
            CachedPackageMetadata {
                metadata_stamps,
                result: result.clone(),
                observations: observations.clone(),
                last_used: self.use_clock,
            },
        );
        self.trim_package_metadata_cache();
        result.map(|metadata| (metadata, observations))
    }

    fn trim_package_metadata_cache(&mut self) {
        while self.package_metadata_cache.len() > MAX_PACKAGE_METADATA_CACHE {
            let Some(victim) = self
                .package_metadata_cache
                .iter()
                .min_by_key(|(_, metadata)| metadata.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.package_metadata_cache.remove(&victim);
        }
    }

    fn watch_package_path(&mut self, context_key: &ContextKey, path: &Path) {
        if let Some(state) = self.contexts.get_mut(context_key) {
            if (extension_is(path, "dpk") || extension_is(path, "dproj"))
                && !state
                    .context
                    .metadata_files
                    .iter()
                    .any(|existing| paths_equal_ci(existing, path))
            {
                state.context.metadata_files.push(path.to_path_buf());
            }
            state
                .watched_paths
                .entry(path.to_path_buf())
                .or_insert_with(|| path_stamp(path));
        }
    }

    fn retain_package_observations(
        &mut self,
        context_key: &ContextKey,
        observations: Vec<ProjectReadObservation>,
    ) {
        if let Some(state) = self.contexts.get_mut(context_key) {
            merge_project_read_observations(&mut state.project_read_observations, observations);
        }
    }

    fn merge_metadata_observations(
        &mut self,
        context_key: &ContextKey,
        observations: &[MetadataObservation],
    ) {
        let Some(state) = self.contexts.get_mut(context_key) else {
            return;
        };
        for observation in observations {
            crate::project::add_metadata_observation(
                &mut state.context.metadata_observations,
                observation.clone(),
            );
        }
    }

    fn package_catalogue(
        &mut self,
        context_key: &ContextKey,
        root: &Path,
        requested_names: &HashSet<String>,
        cancel: Option<&AtomicBool>,
    ) -> Result<PackageCatalogue, String> {
        check_workspace_cancel(cancel)?;
        let root = absolute_path(root.to_path_buf());
        let key = PackageCatalogueKey {
            context: context_key.clone(),
            root: root.clone(),
        };
        let requested_names: HashSet<String> = requested_names
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect();
        let fresh = self.package_catalogues.get(&key).is_some_and(|catalogue| {
            catalogue.complete
                && package_catalogue_directories_are_readable(&catalogue.directories)
                && (catalogue.validated_epoch == self.package_catalogue_epoch
                    || catalogue
                        .directories
                        .iter()
                        .all(|(path, stamp)| path_stamp(path) == *stamp))
                && requested_names.is_subset(&catalogue.requested_names)
        });
        if fresh {
            self.use_clock = self.use_clock.saturating_add(1);
            if let Some(catalogue) = self.package_catalogues.get_mut(&key) {
                catalogue.validated_epoch = self.package_catalogue_epoch;
                catalogue.last_used = self.use_clock;
                return Ok(catalogue.clone());
            }
        }

        let mut scan_names = requested_names.clone();
        if let Some(catalogue) = self.package_catalogues.get(&key) {
            scan_names.extend(catalogue.requested_names.iter().cloned());
        }
        let scan = self.scan_package_catalogue(context_key, &root, &scan_names, cancel)?;

        self.use_clock = self.use_clock.saturating_add(1);
        let catalogue = PackageCatalogue {
            entries: scan.entries,
            requested_names: scan_names,
            complete: scan.complete,
            directories: scan.directories,
            validated_epoch: self.package_catalogue_epoch,
            last_used: self.use_clock,
        };
        self.package_catalogues.insert(key, catalogue.clone());
        self.trim_package_catalogues();
        Ok(catalogue)
    }

    fn scan_package_catalogue(
        &mut self,
        context_key: &ContextKey,
        root: &Path,
        requested_names: &HashSet<String>,
        cancel: Option<&AtomicBool>,
    ) -> Result<PackageCatalogueScan, String> {
        check_workspace_cancel(cancel)?;
        let root = absolute_path(root.to_path_buf());
        let excludes = self
            .roots
            .iter()
            .find(|workspace_root| {
                context_key
                    .workspace_root
                    .as_deref()
                    .is_some_and(|selected| native_paths_equal(selected, &workspace_root.path))
            })
            .map(|workspace_root| workspace_root.excludes.clone())
            .unwrap_or_else(|| ExcludeMatcher::new(&root, &root, &[]));
        let mut scan = PackageCatalogueScan {
            complete: true,
            directories: vec![(root.clone(), path_stamp(&root))],
            ..PackageCatalogueScan::default()
        };
        if fs::symlink_metadata(&root).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            scan.complete = false;
            return Ok(scan);
        }
        let mut pending = VecDeque::from([root.clone()]);
        let mut visited = 1usize;

        'directories: while let Some(directory) = pending.pop_front() {
            check_workspace_cancel(cancel)?;
            let read_dir = match fs::read_dir(&directory) {
                Ok(read_dir) => read_dir,
                Err(_) => {
                    scan.complete = false;
                    break;
                }
            };
            let mut children = Vec::new();
            for result in read_dir {
                check_workspace_cancel(cancel)?;
                if visited >= MAX_PACKAGE_CATALOGUE_ENTRIES {
                    scan.complete = false;
                    break 'directories;
                }
                let Ok(entry) = result else {
                    scan.complete = false;
                    break 'directories;
                };
                visited += 1;
                children.push(entry);
            }
            children.sort_by(|left, right| {
                left.path()
                    .to_string_lossy()
                    .cmp(&right.path().to_string_lossy())
            });

            let mut descriptors_by_stem: HashMap<String, PackageDescriptorFiles> = HashMap::new();
            for entry in children {
                check_workspace_cancel(cancel)?;
                let path = entry.path();
                let Ok(file_type) = entry.file_type() else {
                    scan.complete = false;
                    continue;
                };
                let excluded = excludes.is_excluded(&path, &root);
                if file_type.is_dir() {
                    scan.directories.push((path.clone(), path_stamp(&path)));
                    if !file_type.is_symlink() && !excluded {
                        pending.push_back(path);
                    }
                    continue;
                }
                if file_type.is_symlink()
                    || excluded
                    || !file_type.is_file()
                    || !(extension_is(&path, "dpk") || extension_is(&path, "dproj"))
                {
                    continue;
                }
                let Some(stem) = path.file_stem() else {
                    continue;
                };
                let stem = stem.to_string_lossy().to_ascii_lowercase();
                let mut matched_names = Vec::new();
                if requested_names.contains(&stem) {
                    matched_names.push(stem.clone());
                }

                let files = descriptors_by_stem.entry(stem).or_default();
                if extension_is(&path, "dpk") {
                    files.has_dpk = true;
                    if !matched_names.is_empty() {
                        files.dpk.push(PackageDescriptorMatch {
                            path,
                            names: matched_names,
                        });
                    }
                } else if !matched_names.is_empty() {
                    files.dproj.push(PackageDescriptorMatch {
                        path,
                        names: matched_names,
                    });
                }
            }

            for files in descriptors_by_stem.into_values() {
                check_workspace_cancel(cancel)?;
                let selected = if files.has_dpk {
                    files.dpk
                } else {
                    files.dproj
                };
                for descriptor in selected {
                    for name in descriptor.names {
                        scan.entries
                            .entry(name)
                            .or_default()
                            .push(descriptor.path.clone());
                    }
                }
            }
        }

        for paths in scan.entries.values_mut() {
            check_workspace_cancel(cancel)?;
            paths.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
            paths.dedup_by(|left, right| package_paths_equal(left, right));
        }
        Ok(scan)
    }

    fn trim_package_catalogues(&mut self) {
        while self.package_catalogues.len() > MAX_PACKAGE_CATALOGUES {
            let Some(victim) = self
                .package_catalogues
                .iter()
                .min_by_key(|(_, catalogue)| catalogue.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.package_catalogues.remove(&victim);
        }
    }

    fn filename_candidates_for_names(
        &mut self,
        directory: &Path,
        names: &[String],
        context_key: &ContextKey,
    ) -> Vec<PathBuf> {
        let mut entries = self.directory_entries(directory);
        entries.extend(self.open_document_entries(directory, context_key));
        let mut candidates: Vec<PathBuf> = Vec::new();
        for name in names {
            for path in &entries {
                if path
                    .file_name()
                    .is_some_and(|file_name| file_name.to_string_lossy().eq_ignore_ascii_case(name))
                    && !candidates
                        .iter()
                        .any(|existing| paths_equal_ci(existing, path))
                {
                    candidates.push(path.clone());
                }
            }
        }
        candidates
    }

    fn open_document_entries(&self, directory: &Path, context_key: &ContextKey) -> Vec<PathBuf> {
        self.open_documents
            .iter()
            .filter_map(|(uri, document)| {
                document.text.as_ref()?;
                let path = absolute_path(uri.to_file_path().ok()?);
                let parent = path.parent()?;
                (paths_equal_ci(parent, directory)
                    && self.ensure_supported_with_context(uri, context_key))
                .then_some(path)
            })
            .collect()
    }

    fn directory_entries(&mut self, directory: &Path) -> Vec<PathBuf> {
        let directory = absolute_path(directory.to_path_buf());
        let stamp = path_stamp(&directory);
        if self
            .directory_catalogues
            .get(&directory)
            .is_some_and(|catalogue| catalogue.stamp == stamp)
        {
            if let Some(catalogue) = self.directory_catalogues.get_mut(&directory) {
                self.use_clock = self.use_clock.saturating_add(1);
                catalogue.last_used = self.use_clock;
                return catalogue.entries.clone();
            }
        }

        let entries = fs::read_dir(&directory)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let file_type = entry.file_type().ok()?;
                (file_type.is_file() && !file_type.is_symlink()).then_some(entry.path())
            })
            .collect::<Vec<_>>();
        self.use_clock = self.use_clock.saturating_add(1);
        self.directory_catalogues.insert(
            directory.clone(),
            DirectoryCatalogue {
                stamp,
                entries: entries.clone(),
                last_used: self.use_clock,
            },
        );
        self.trim_directory_catalogues();
        entries
    }

    fn trim_directory_catalogues(&mut self) {
        while self.directory_catalogues.len() > MAX_DIRECTORY_CATALOGUES {
            let Some(victim) = self
                .directory_catalogues
                .iter()
                .min_by_key(|(_, catalogue)| catalogue.last_used)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            self.directory_catalogues.remove(&victim);
        }
    }

    fn filename_unit_candidates(
        &mut self,
        unit_name: &str,
        context: &ProjectContext,
        context_key: &ContextKey,
    ) -> Vec<PathBuf> {
        let names = unit_filename_candidates(unit_name, &context.unit_namespaces)
            .into_iter()
            .map(|name| name.to_ascii_lowercase())
            .collect::<Vec<_>>();
        let roots = if context.search_paths.is_empty() {
            self.roots
                .iter()
                .map(|root| root.path.clone())
                .collect::<Vec<_>>()
        } else {
            context.search_paths.clone()
        };
        let mut result = Vec::new();
        for root in &roots {
            let catalogue = self.filename_catalogue(root);
            if !catalogue.complete {
                self.warn(format!(
                    "projectless filename catalogue reached its bounded entry limit under {}",
                    root.display()
                ));
            }
            for name in &names {
                if let Some(paths) = catalogue.entries.get(name) {
                    result.extend(paths.iter().cloned());
                }
            }
        }
        for (uri, document) in &self.open_documents {
            if document.text.is_none() || !self.ensure_supported_with_context(uri, context_key) {
                continue;
            }
            let Ok(path) = uri.to_file_path() else {
                continue;
            };
            let path = absolute_path(path);
            let Some(file_name) = path.file_name() else {
                continue;
            };
            if names
                .iter()
                .any(|name| file_name.to_string_lossy().eq_ignore_ascii_case(name))
                && roots.iter().any(|root| path_starts_with_ci(&path, root))
            {
                result.push(path);
            }
        }
        result
    }

    fn filename_catalogue(&mut self, root: &Path) -> FilenameCatalogue {
        let root = absolute_path(root.to_path_buf());
        let fresh = self
            .filename_catalogues
            .get(&root)
            .is_some_and(|catalogue| {
                catalogue
                    .directories
                    .iter()
                    .all(|(path, stamp)| path_stamp(path) == *stamp)
            });
        if fresh {
            self.use_clock = self.use_clock.saturating_add(1);
            if let Some(catalogue) = self.filename_catalogues.get_mut(&root) {
                catalogue.last_used = self.use_clock;
                return catalogue.clone();
            }
        }
        let mut entries: HashMap<String, Vec<PathBuf>> = HashMap::new();
        let mut directories = vec![(root.clone(), path_stamp(&root))];
        let mut visited = 0;
        let mut complete = true;
        let excludes = self
            .roots
            .iter()
            .find(|workspace_root| path_starts_with_ci(&root, &workspace_root.path))
            .map(|workspace_root| workspace_root.excludes.clone());
        for entry in WalkDir::new(&root).follow_links(false).into_iter() {
            if visited >= MAX_FILENAME_CATALOGUE_ENTRIES {
                complete = false;
                break;
            }
            let Ok(entry) = entry else {
                continue;
            };
            visited += 1;
            if entry.file_type().is_dir() && directories.len() < MAX_FILENAME_CATALOGUE_ENTRIES {
                directories.push((entry.path().to_path_buf(), path_stamp(entry.path())));
            }
            if entry.file_type().is_symlink()
                || excludes
                    .as_ref()
                    .is_some_and(|matcher| matcher.is_excluded(entry.path(), &root))
            {
                continue;
            }
            if !entry.file_type().is_file() || !extension_is(entry.path(), "pas") {
                continue;
            }
            if let Some(name) = entry.path().file_name() {
                entries
                    .entry(name.to_string_lossy().to_ascii_lowercase())
                    .or_default()
                    .push(entry.path().to_path_buf());
            }
        }
        self.use_clock = self.use_clock.saturating_add(1);
        let catalogue = FilenameCatalogue {
            entries,
            complete,
            directories,
            last_used: self.use_clock,
        };
        self.filename_catalogues.insert(root, catalogue.clone());
        self.trim_filename_catalogues();
        catalogue
    }

    fn trim_filename_catalogues(&mut self) {
        while self.filename_catalogues.len() > MAX_FILENAME_CATALOGUES {
            let Some(victim) = self
                .filename_catalogues
                .iter()
                .min_by_key(|(_, catalogue)| catalogue.last_used)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            self.filename_catalogues.remove(&victim);
        }
    }

    fn remove_indexed(&mut self, uri: &Url) {
        self.index.remove(uri);
        self.indexed_files.remove(uri);
        self.last_used.remove(uri);
        self.document_contexts.remove(uri);
        self.index.clear_import_bindings(uri);
        self.disk_stamps.remove(uri);
        if let Some(size) = self.indexed_sizes.remove(uri) {
            self.indexed_bytes = self.indexed_bytes.saturating_sub(size);
        }
        self.prune_unused_contexts();
    }

    fn prune_unused_contexts(&mut self) {
        let used = self
            .document_contexts
            .values()
            .cloned()
            .collect::<HashSet<_>>();
        let open_used = self
            .open_document_contexts
            .values()
            .cloned()
            .collect::<HashSet<_>>();
        self.contexts
            .retain(|key, _| used.contains(key) || open_used.contains(key));
    }

    fn set_document_context(&mut self, uri: &Url, context_key: &ContextKey) -> Result<(), String> {
        let path = uri.to_file_path().ok().map(absolute_path);
        let current_selection = path
            .as_deref()
            .and_then(|path| self.runtime_selection_for_path_default(path));
        let known_owner = path.as_deref().and_then(|path| {
            self.document_owners
                .get(uri)
                .filter(|owner| self.known_owner_selection_is_current(path, owner))
                .cloned()
        });
        let effective_context_key = if let Some(owner) = &known_owner {
            if self.context_state_is_fresh_with_open_documents(&owner.state, None)? {
                self.contexts
                    .entry(owner.key.clone())
                    .or_insert_with(|| owner.state.clone());
                owner.key.clone()
            } else {
                if let Some(current_owner) = self.document_owners.get_mut(uri) {
                    current_owner.legacy_route = None;
                }
                let path = absolute_path(
                    uri.to_file_path()
                        .map_err(|_| format!("document context requires a file URI: {uri}"))?,
                );
                let roots = self.workspace_root_paths();
                let (key, discovery) = self
                    .rediscover_known_owner(&path, owner, &roots, &self.project_options(), None)
                    .map_err(|error| {
                        format!("could not rediscover known project owner for {uri}: {error}")
                    })?;
                self.install_context(
                    key.clone(),
                    discovery.context,
                    discovery.observations,
                    discovery.candidate_memberships,
                    &path,
                    None,
                )?;
                key
            }
        } else if let Some((scope, selected)) = current_selection.as_ref() {
            if self.context_matches_selection(context_key, scope, selected) {
                context_key.clone()
            } else {
                self.context_for_uri(uri)?
            }
        } else {
            context_key.clone()
        };
        let owner_is_effective = known_owner
            .as_ref()
            .is_some_and(|owner| owner.key == effective_context_key);
        if self.open_documents.contains_key(uri) {
            if owner_is_effective || !self.open_document_contexts.contains_key(uri) {
                self.open_document_contexts
                    .insert(uri.clone(), effective_context_key.clone());
            }
            let selected = self
                .open_document_contexts
                .get(uri)
                .cloned()
                .expect("open document context is initialized");
            self.document_contexts.insert(uri.clone(), selected);
        } else if owner_is_effective {
            self.document_contexts
                .insert(uri.clone(), effective_context_key.clone());
        } else {
            self.document_contexts
                .entry(uri.clone())
                .or_insert_with(|| effective_context_key.clone());
        }
        let selected = self
            .open_document_contexts
            .get(uri)
            .or_else(|| self.document_contexts.get(uri))
            .cloned();
        if let Some(selected) = selected {
            self.remember_document_owner(uri, &selected);
        }
        self.prune_unused_contexts();
        Ok(())
    }

    fn select_document_context(
        &mut self,
        uri: &Url,
        context_key: &ContextKey,
        origin: OwnerOrigin,
    ) {
        if self.open_documents.contains_key(uri) {
            self.open_document_contexts
                .insert(uri.clone(), context_key.clone());
        }
        self.document_contexts
            .insert(uri.clone(), context_key.clone());
        self.remember_document_owner_with_origin(uri, context_key, origin);
        self.prune_unused_contexts();
    }

    fn remember_deleted(&mut self, uri: &Url) {
        if self.deleted_overrides.len() >= MAX_DELETED_OVERRIDES
            && !self.deleted_overrides.contains_key(uri)
        {
            if let Some(victim) = self.deleted_overrides.keys().next().cloned() {
                self.deleted_overrides.remove(&victim);
            }
        }
        let stamp = uri
            .to_file_path()
            .ok()
            .map(|path| disk_stamp(&absolute_path(path)))
            .unwrap_or(None);
        self.deleted_overrides.insert(uri.clone(), stamp);
    }

    fn deletion_blocks_load(&mut self, uri: &Url, path: &Path) -> bool {
        let Some(deleted_stamp) = self.deleted_overrides.get(uri).cloned() else {
            return false;
        };
        if disk_stamp(path) == deleted_stamp {
            return true;
        }
        self.deleted_overrides.remove(uri);
        false
    }

    /// Number of parsed documents currently retained. This intentionally does
    /// not include filename-only catalogue entries.
    pub fn parsed_document_count(&self) -> usize {
        self.index.document_count()
    }

    /// Warnings emitted by lazy project/unit resolution so embedders can
    /// surface conservative incomplete-resolution status without pretending
    /// to provide compiler diagnostics.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub(crate) fn source_generation(&self) -> u64 {
        self.source_generation
    }

    pub(crate) fn configuration_generation(&self) -> u64 {
        self.configuration_generation
    }

    fn bump_source_generation(&mut self) {
        self.source_generation = self.source_generation.wrapping_add(1);
    }

    fn bump_configuration_generation(&mut self) {
        self.configuration_generation = self.configuration_generation.wrapping_add(1);
    }

    fn warn(&mut self, message: String) {
        if self.warnings.iter().any(|warning| warning == &message) {
            return;
        }
        match self.warnings.len().cmp(&MAX_WORKSPACE_WARNINGS) {
            std::cmp::Ordering::Less => {
                eprintln!("pascal-lsp: warning: {message}");
                self.warnings.push(message);
            }
            std::cmp::Ordering::Equal => {
                eprintln!("pascal-lsp: warning: workspace warning limit reached");
                self.warnings
                    .push("workspace warning limit reached".to_string());
            }
            std::cmp::Ordering::Greater => {}
        }
    }

    fn accepts_path(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| root.accepts(path))
    }

    fn file_too_large_message(&self, uri: &Url, size: usize) -> String {
        format!(
            "document {uri} is {size} bytes; the configured per-file limit is {}",
            self.options.limits.max_file_bytes
        )
    }

    fn schedule_diagnostics(&mut self, uri: Url) {
        self.pending_diagnostics
            .insert(uri, Instant::now() + DIAGNOSTIC_DEBOUNCE);
    }

    fn diagnostics_for(&self, uri: &Url) -> Vec<LspDiagnostic> {
        let Some(document) = self.open_documents.get(uri) else {
            return Vec::new();
        };
        if let Some(rejection) = &document.rejection {
            return vec![server_diagnostic(rejection, DiagnosticSeverity::ERROR)];
        }
        let source = document
            .text
            .as_deref()
            .expect("accepted open documents retain their text");
        let lint_source = normalize_line_endings(source);
        let path = match uri.to_file_path() {
            Ok(path) => absolute_path(path),
            Err(()) => {
                return vec![server_diagnostic(
                    "not a file URI",
                    DiagnosticSeverity::ERROR,
                )];
            }
        };
        if let Err(error) = ensure_safe_tree_depth(&path, lint_source.as_bytes()) {
            return vec![server_diagnostic(&error, DiagnosticSeverity::ERROR)];
        }
        let roots = self
            .roots
            .iter()
            .map(|root| root.path.clone())
            .collect::<Vec<_>>();
        let project_options = self.project_options();
        let (context_key, context) =
            match self.readonly_context_for_uri(uri, &path, &roots, &project_options) {
                Ok(result) => result,
                Err(error) => {
                    return vec![server_diagnostic(&error, DiagnosticSeverity::ERROR)];
                }
            };
        if has_invalid_project_selection(&context) {
            return vec![server_diagnostic(
                "project selection is invalid; select a current project or Automatic",
                DiagnosticSeverity::ERROR,
            )];
        }
        let candidates = match project_candidates(&path, &roots) {
            Ok(candidates) => candidates,
            Err(error) => return vec![server_diagnostic(&error, DiagnosticSeverity::ERROR)],
        };
        let project_directory =
            self.configuration_project_directory(&path, &context, Some(&context_key), &candidates);
        let directories = match config_directories(&path, project_directory.as_deref(), &roots) {
            Ok(directories) => directories,
            Err(error) => return vec![server_diagnostic(&error, DiagnosticSeverity::ERROR)],
        };
        let resolved_config = match resolve_lint(&directories, 4 * 1024 * 1024) {
            Ok(config) => config,
            Err(error) => return vec![server_diagnostic(&error, DiagnosticSeverity::ERROR)],
        };
        if is_lint_excluded(
            &path,
            resolved_config.path.as_deref(),
            &resolved_config.value.exclude,
        ) {
            return Vec::new();
        }
        let config = resolved_config.value;
        let raw = lint4d::engine::run_lint(&FileInfo::new(path), lint_source.as_bytes(), &config);
        let line_index = DiagnosticLineIndex::new(&lint_source);
        raw.into_iter()
            .map(|diagnostic| {
                let range = line_index.range(
                    diagnostic.line,
                    diagnostic.column,
                    diagnostic.end_line,
                    diagnostic.end_column,
                );
                let severity = match diagnostic.severity {
                    Severity::Error => DiagnosticSeverity::ERROR,
                    Severity::Warning => DiagnosticSeverity::WARNING,
                    Severity::Hint => DiagnosticSeverity::HINT,
                };
                LspDiagnostic::new(
                    range,
                    Some(severity),
                    Some(NumberOrString::String(diagnostic.rule_id)),
                    Some("lint4d".to_string()),
                    diagnostic.message,
                    None,
                    None,
                )
            })
            .collect()
    }
}

fn scan_external_units(
    project_root: &Path,
    external_paths: &[String],
    limits: &ResourceLimits,
) -> Result<HashSet<String>, String> {
    let mut units = HashSet::new();
    let mut visited_entries = 0usize;
    let mut scanned_files = 0usize;
    let mut scanned_bytes = 0usize;

    for configured_path in external_paths {
        let configured_path = configured_path.replace('\\', "/");
        let configured_path = PathBuf::from(configured_path);
        let path = if configured_path.is_absolute() {
            configured_path
        } else {
            project_root.join(configured_path)
        };
        let path = absolute_path(path);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "could not inspect formatting external path {}: {error}",
                path.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "formatting external path {} must be a regular directory; symlink roots are not followed",
                path.display()
            ));
        }
        if !metadata.is_dir() {
            return Err(format!(
                "formatting external path {} is not a directory",
                path.display()
            ));
        }

        for entry in WalkDir::new(&path).follow_links(false).into_iter() {
            let entry = entry.map_err(|error| {
                format!(
                    "could not scan formatting external path {}: {error}",
                    path.display()
                )
            })?;
            visited_entries = visited_entries.saturating_add(1);
            if visited_entries > MAX_FORMAT_EXTERNAL_TRAVERSAL_ENTRIES {
                return Err(format!(
                    "formatting external-unit traversal entry limit ({MAX_FORMAT_EXTERNAL_TRAVERSAL_ENTRIES}) reached under {}",
                    path.display()
                ));
            }
            let file_type = entry.file_type();
            if entry.depth() == 0 && file_type.is_symlink() {
                return Err(format!(
                    "formatting external path {} must be a regular directory; symlink roots are not followed",
                    path.display()
                ));
            }
            if file_type.is_symlink() || !file_type.is_file() || !extension_is(entry.path(), "pas")
            {
                continue;
            }

            let file_path = entry.path();
            let metadata = fs::symlink_metadata(file_path).map_err(|error| {
                format!(
                    "could not inspect formatting external unit {}: {error}",
                    file_path.display()
                )
            })?;
            if !metadata.file_type().is_file() {
                return Err(format!(
                    "formatting external unit {} was replaced by a non-regular file",
                    file_path.display()
                ));
            }
            let file_bytes = usize::try_from(metadata.len()).map_err(|_| {
                format!(
                    "formatting external unit {} exceeds the supported size range",
                    file_path.display()
                )
            })?;
            if file_bytes > limits.max_file_bytes {
                return Err(format!(
                    "formatting external unit {} exceeds the configured per-file limit of {} bytes",
                    file_path.display(),
                    limits.max_file_bytes
                ));
            }
            scanned_files = scanned_files.saturating_add(1);
            if scanned_files > limits.max_files {
                return Err(format!(
                    "formatting external-unit file limit ({}) reached under {}",
                    limits.max_files,
                    path.display()
                ));
            }
            scanned_bytes = scanned_bytes.saturating_add(file_bytes);
            if scanned_bytes > limits.max_total_bytes {
                return Err(format!(
                    "formatting external-unit byte limit ({}) reached under {}",
                    limits.max_total_bytes,
                    path.display()
                ));
            }
            if let Some(stem) = file_path.file_stem().and_then(|stem| stem.to_str()) {
                units.insert(stem.to_lowercase());
            }
        }
    }

    Ok(units)
}

fn project_unit_stems(context: &ProjectContext, current_path: &Path) -> HashSet<String> {
    let mut stems = HashSet::new();
    let mut add_stem = |path: &Path| {
        if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
            stems.insert(stem.to_lowercase());
        }
    };
    add_stem(current_path);
    if let Some(main_source) = &context.main_source {
        add_stem(main_source);
    }
    for path in context.explicit_units.values().flatten() {
        add_stem(path);
    }
    // DCC_UnitSearchPath is a compiler lookup path, not a project-file list.
    // Libraries commonly share it with project sources, so only the current
    // input, MainSource, and concrete project references may override an
    // external unit with the same stem. This mirrors the CLI's project file
    // collection instead of inventing ownership from directory membership.
    stems
}

fn path_stamp_from_project_read(stamp: &ProjectReadStamp) -> PathStamp {
    PathStamp {
        bytes: stamp.bytes,
        modified: stamp.modified,
        is_dir: stamp.is_dir,
        is_symlink: stamp.is_symlink,
    }
}

fn take_candidate_membership(
    memberships: &mut HashMap<PathBuf, Result<ProjectCandidateMembership, String>>,
    directory: &Path,
) -> Option<Result<ProjectCandidateMembership, String>> {
    let key = memberships
        .keys()
        .find(|candidate| package_paths_equal(candidate, directory))
        .cloned()?;
    memberships.remove(&key)
}

fn merge_project_read_observations(
    target: &mut Vec<ProjectReadObservation>,
    observations: Vec<ProjectReadObservation>,
) {
    for observation in observations {
        if target
            .iter()
            .any(|existing| package_paths_equal(&existing.path, &observation.path))
        {
            continue;
        }
        target.push(observation);
    }
}

fn merge_candidate_memberships(
    target: &mut HashMap<PathBuf, Result<ProjectCandidateMembership, String>>,
    memberships: HashMap<PathBuf, Result<ProjectCandidateMembership, String>>,
) {
    for (path, membership) in memberships {
        if target
            .keys()
            .any(|existing| package_paths_equal(existing, &path))
        {
            continue;
        }
        target.insert(path, membership);
    }
}

fn context_state_is_fresh_with_cancel(
    state: &ContextState,
    cancel: Option<&AtomicBool>,
) -> Result<bool, String> {
    context_state_is_fresh_with_cancel_ignoring_paths(state, cancel, &[])
}

fn context_state_is_fresh_with_cancel_ignoring_paths(
    state: &ContextState,
    cancel: Option<&AtomicBool>,
    ignored_paths: &[PathBuf],
) -> Result<bool, String> {
    let watched_paths_are_fresh = state.watched_paths.iter().all(|(path, stamp)| {
        if ignored_paths
            .iter()
            .any(|ignored| package_paths_equal(ignored, path))
        {
            true
        } else if is_configuration_file(path) {
            path_stamp_result(path)
                .map(|actual| actual == *stamp)
                .unwrap_or(false)
        } else {
            path_stamp(path) == *stamp
        }
    });
    if !watched_paths_are_fresh {
        return Ok(false);
    }

    for (directory, expected) in &state.project_candidate_memberships {
        match (expected, project_candidate_membership(directory, cancel)) {
            (Ok(expected), Ok(actual)) if actual == *expected => {}
            (_, Err(error)) if error == CANCELLATION_MESSAGE => return Err(error),
            _ => return Ok(false),
        }
    }
    Ok(true)
}

fn server_diagnostic(message: &str, severity: DiagnosticSeverity) -> LspDiagnostic {
    LspDiagnostic::new(
        Range::new(Position::new(0, 0), Position::new(0, 0)),
        Some(severity),
        Some(NumberOrString::String("pascal-lsp".to_string())),
        Some("pascal-lsp".to_string()),
        message.to_string(),
        None,
        None,
    )
}

struct DiagnosticLineIndex<'a> {
    source: &'a str,
    lines: Vec<(usize, usize)>,
    utf16_prefix: Vec<usize>,
}

impl<'a> DiagnosticLineIndex<'a> {
    fn new(source: &'a str) -> Self {
        let mut lines = Vec::new();
        let mut start = 0;
        for (index, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                lines.push((start, index));
                start = index + 1;
            }
        }
        lines.push((start, source.len()));
        let mut utf16_prefix = vec![0; source.len() + 1];
        let mut units = 0;
        for (index, character) in source.char_indices() {
            units += character.len_utf16();
            utf16_prefix[index + character.len_utf8()] = units;
        }
        Self {
            source,
            lines,
            utf16_prefix,
        }
    }

    fn range(&self, line: usize, column: usize, end_line: usize, end_column: usize) -> Range {
        let start = self.position(line, column).unwrap_or(Position::new(0, 0));
        let end = self.position(end_line, end_column).unwrap_or(start);
        Range::new(start, end)
    }

    fn position(&self, line: usize, column: usize) -> Option<Position> {
        let line_number = line.checked_sub(1)?;
        let (start, end) = *self.lines.get(line_number)?;
        let mut byte_offset = column.saturating_sub(1).min(end - start);
        while byte_offset > 0 && !self.source.is_char_boundary(start + byte_offset) {
            byte_offset -= 1;
        }
        let character = self.utf16_prefix[start + byte_offset] - self.utf16_prefix[start];
        Some(Position::new(
            u32::try_from(line_number).ok()?,
            u32::try_from(character).ok()?,
        ))
    }
}

fn normalize_line_endings(source: &str) -> String {
    if !source.as_bytes().contains(&b'\r') {
        return source.to_owned();
    }
    let bytes = source.as_bytes();
    let mut normalized = String::with_capacity(source.len());
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\r' {
            index += 1;
            continue;
        }
        normalized.push_str(&source[start..index]);
        normalized.push('\n');
        index += 1;
        if bytes.get(index) == Some(&b'\n') {
            index += 1;
        }
        start = index;
    }
    normalized.push_str(&source[start..]);
    normalized
}

fn ensure_safe_tree_depth(path: &Path, source: &[u8]) -> Result<(), String> {
    let (tree, _) = parser::parse_file(&FileInfo::new(path.to_path_buf()), source)?;
    let mut pending = vec![(tree.root_node(), 0usize)];
    while let Some((node, depth)) = pending.pop() {
        if depth > MAX_TREE_DEPTH {
            return Err(format!(
                "analysis skipped: syntax tree depth exceeds the safe limit of {MAX_TREE_DEPTH}"
            ));
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            pending.push((child, depth.saturating_add(1)));
        }
    }
    Ok(())
}

fn path_to_glob(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn read_disk_source(
    path: &Path,
    max_bytes: usize,
    read_policy: &crate::project::ReadPolicy,
    entry: &ProjectPathEntry,
    allow_legacy_payload: bool,
) -> Result<DiskSource, String> {
    read_disk_source_with_cancel(
        path,
        max_bytes,
        read_policy,
        entry,
        allow_legacy_payload,
        None,
    )
}

fn read_disk_source_with_cancel(
    path: &Path,
    max_bytes: usize,
    read_policy: &crate::project::ReadPolicy,
    entry: &ProjectPathEntry,
    allow_legacy_payload: bool,
    cancel: Option<&AtomicBool>,
) -> Result<DiskSource, String> {
    check_workspace_cancel(cancel)?;
    let link_metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    let is_symlink = link_metadata.file_type().is_symlink();
    if is_symlink && !allow_legacy_payload {
        return Err(format!(
            "{} is a symlink and not an authorized workspace source",
            path.display()
        ));
    }
    let metadata = if is_symlink {
        fs::metadata(path).map_err(|error| format!("cannot inspect {}: {error}", path.display()))?
    } else {
        link_metadata
    };
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if metadata.len() > max_bytes as u64 {
        return Err(format!(
            "{} is {} bytes; the configured per-file limit is {max_bytes}",
            path.display(),
            metadata.len()
        ));
    }

    let bytes = if allow_legacy_payload {
        read_policy.read_legacy_payload_bytes(entry, max_bytes as u64)
    } else {
        read_policy.read_payload_bytes(entry, max_bytes as u64)
    }
    .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    check_workspace_cancel(cancel)?;
    let text = decode_bytes(&bytes).into_owned();
    if text.len() > max_bytes {
        return Err(format!(
            "decoded contents of {} exceed the configured per-file limit {max_bytes}",
            path.display()
        ));
    }
    check_workspace_cancel(cancel)?;
    Ok(DiskSource {
        text,
        bytes: bytes.len(),
        stamp: DiskStamp {
            bytes: metadata.len(),
            modified: metadata.modified().ok(),
        },
        content_hash: content_hash_bytes(&bytes),
    })
}

pub(crate) fn content_hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write(bytes);
    hasher.write_usize(bytes.len());
    hasher.finish()
}

fn disk_stamp(path: &Path) -> Option<DiskStamp> {
    let metadata = fs::metadata(path).ok()?;
    metadata.is_file().then_some(DiskStamp {
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn absolute_path(path: PathBuf) -> PathBuf {
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir | Component::Normal(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

pub(crate) fn canonical_file_uri(uri: &Url) -> Url {
    uri.to_file_path()
        .ok()
        .and_then(|path| Url::from_file_path(absolute_path(path)).ok())
        .unwrap_or_else(|| uri.clone())
}

pub(crate) fn path_stamp_result(path: &Path) -> io::Result<Option<PathStamp>> {
    let link_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let is_symlink = link_metadata.file_type().is_symlink();
    let metadata = if is_symlink {
        match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => link_metadata,
            Err(error) => return Err(error),
        }
    } else {
        link_metadata
    };
    Ok(Some(PathStamp {
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
        is_dir: metadata.is_dir(),
        is_symlink,
    }))
}

fn path_stamp(path: &Path) -> Option<PathStamp> {
    path_stamp_result(path).ok().flatten()
}

fn add_configuration_watch_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths
        .iter()
        .any(|existing| package_paths_equal(existing, &path))
    {
        paths.push(path);
    }
}

fn add_configuration_watch_directory(paths: &mut Vec<PathBuf>, directory: &Path) {
    for filename in CONFIGURATION_FILENAMES {
        add_configuration_watch_path(paths, directory.join(filename));
    }
}

fn add_configuration_watch_directories(
    paths: &mut Vec<PathBuf>,
    directories: impl IntoIterator<Item = PathBuf>,
) {
    for directory in directories {
        add_configuration_watch_directory(paths, &directory);
    }
}

fn paths_equal_ci(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

fn project_path_entry_for<'a>(
    context: &'a ProjectContext,
    path: &Path,
) -> Option<&'a ProjectPathEntry> {
    context
        .main_source_entry
        .as_ref()
        .filter(|entry| native_paths_equal(&entry.path, path))
        .or_else(|| {
            context
                .explicit_unit_entries
                .values()
                .flatten()
                .find(|entry| native_paths_equal(&entry.path, path))
        })
}

fn context_path_entry(context: &ProjectContext, path: &Path) -> Option<ProjectPathEntry> {
    if let Some(entry) = project_path_entry_for(context, path) {
        return Some(ProjectPathEntry {
            path: path.to_path_buf(),
            provenance: entry.provenance.clone(),
        });
    }
    context
        .search_path_entries
        .iter()
        .filter(|entry| path_starts_with_native(path, &entry.path))
        .max_by_key(|entry| entry.path.components().count())
        .map(|entry| ProjectPathEntry {
            path: path.to_path_buf(),
            provenance: entry.provenance.clone(),
        })
        .or_else(|| context.read_policy.entry_for_path(path))
}

fn native_mapping_root(root: &Path) -> PathBuf {
    resolve_case_insensitive_path(root).unwrap_or_else(|| absolute_path(root.to_path_buf()))
}

fn context_uses_mapped_root(
    context: &ProjectContext,
    configured_root: &Path,
    resolved_root: &Path,
) -> bool {
    context
        .main_source_entry
        .as_ref()
        .into_iter()
        .chain(context.search_path_entries.iter())
        .chain(context.include_path_entries.iter())
        .chain(context.explicit_unit_entries.values().flatten())
        .any(|entry| match &entry.provenance {
            ProjectPathProvenance::Mapped { root: mapped_root } => {
                package_paths_equal(mapped_root, resolved_root)
                    || paths_equal_ci(mapped_root, configured_root)
            }
            ProjectPathProvenance::Configured => {
                path_starts_with_native(&entry.path, resolved_root)
                    || path_starts_with_native(&entry.path, configured_root)
                    || (!resolved_root.exists()
                        && path_starts_with_ci(&entry.path, configured_root))
            }
            ProjectPathProvenance::LegacyNative => false,
        })
}

fn package_catalogue_directories_are_readable(
    directories: &[(PathBuf, Option<PathStamp>)],
) -> bool {
    directories.iter().all(|(path, stamp)| {
        !stamp
            .as_ref()
            .is_some_and(|stamp| stamp.is_dir && !stamp.is_symlink)
            || fs::read_dir(path).is_ok()
    })
}

fn relative_path(base: &Path, path: &Path) -> Option<PathBuf> {
    let base_components = base.components().collect::<Vec<_>>();
    let path_components = path.components().collect::<Vec<_>>();
    let common = base_components
        .iter()
        .zip(path_components.iter())
        .take_while(|(base, path)| path_components_equal(**base, **path))
        .count();
    if common == 0 {
        return None;
    }

    let mut relative = PathBuf::new();
    for _ in common..base_components.len() {
        relative.push("..");
    }
    for component in &path_components[common..] {
        relative.push(component.as_os_str());
    }
    Some(relative)
}

fn path_components_equal(left: Component<'_>, right: Component<'_>) -> bool {
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

#[cfg(windows)]
fn package_paths_equal(left: &Path, right: &Path) -> bool {
    paths_equal_ci(left, right)
}

#[cfg(not(windows))]
fn package_paths_equal(left: &Path, right: &Path) -> bool {
    left == right
}

fn path_starts_with_ci(path: &Path, root: &Path) -> bool {
    let path_components = path.components().collect::<Vec<_>>();
    let root_components = root.components().collect::<Vec<_>>();
    path_components.len() >= root_components.len()
        && path_components
            .iter()
            .zip(root_components.iter())
            .all(|(path, root)| {
                path.as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&root.as_os_str().to_string_lossy())
            })
}

fn path_starts_with_native(path: &Path, root: &Path) -> bool {
    let path_components = path.components().collect::<Vec<_>>();
    let root_components = root.components().collect::<Vec<_>>();
    path_components.len() >= root_components.len()
        && path_components
            .iter()
            .zip(root_components.iter())
            .all(|(path, root)| native_components_equal(*path, *root))
}

fn native_relative_path(path: &Path, root: &Path) -> Option<PathBuf> {
    let path_components = path.components().collect::<Vec<_>>();
    let root_components = root.components().collect::<Vec<_>>();
    if path_components.len() < root_components.len()
        || !path_components
            .iter()
            .zip(root_components.iter())
            .all(|(path, root)| native_components_equal(*path, *root))
    {
        return None;
    }

    let mut relative = PathBuf::new();
    for component in path_components.into_iter().skip(root_components.len()) {
        relative.push(component.as_os_str());
    }
    Some(relative)
}

fn native_paths_equal(left: &Path, right: &Path) -> bool {
    let left_components = left.components().collect::<Vec<_>>();
    let right_components = right.components().collect::<Vec<_>>();
    left_components.len() == right_components.len()
        && left_components
            .iter()
            .zip(right_components.iter())
            .all(|(left, right)| native_components_equal(*left, *right))
}

fn native_components_equal(left: Component<'_>, right: Component<'_>) -> bool {
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

fn safe_path_under_root(path: &Path, root: &Path) -> bool {
    let Some(canonical_root) = fs::canonicalize(root).ok() else {
        return false;
    };
    let Some(canonical_path) = canonical_source_path(path) else {
        return false;
    };
    path_starts_with_native(&canonical_path, &canonical_root) && !has_symlink_component(path, root)
}

fn canonical_source_path(path: &Path) -> Option<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path).ok();
    }
    let parent = path.parent()?;
    let canonical_parent = fs::canonicalize(parent).ok()?;
    Some(canonical_parent.join(path.file_name()?))
}

fn has_symlink_component(path: &Path, root: &Path) -> bool {
    if fs::symlink_metadata(root).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return true;
    }
    let Some(relative) = native_relative_path(path, root) else {
        return true;
    };
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        if fs::symlink_metadata(&current).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return true;
        }
    }
    false
}

fn resolve_case_insensitive_path(path: &Path) -> Option<PathBuf> {
    let absolute = absolute_path(path.to_path_buf());
    let mut current = PathBuf::from(std::path::MAIN_SEPARATOR.to_string());
    for component in absolute.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        let wanted = component.to_string_lossy();
        let mut matches = fs::read_dir(&current)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&wanted)
                    .then_some(entry.path())
            })
            .collect::<Vec<_>>();
        if let Some(exact) = matches
            .iter()
            .find(|candidate| candidate.file_name().is_some_and(|name| name == component))
        {
            current = exact.clone();
            continue;
        }
        if matches.len() != 1 {
            return None;
        }
        current = matches.pop().expect("one path match");
    }
    Some(current)
}

fn aliased_unit_name(context: &ProjectContext, name: &str) -> String {
    context
        .unit_aliases
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
        .map_or_else(
            || name.to_ascii_lowercase(),
            |(_, target)| target.to_ascii_lowercase(),
        )
}

fn unit_filename_candidates(unit_name: &str, namespaces: &[String]) -> Vec<String> {
    unit_filename_candidate_tiers(unit_name, namespaces)
        .into_iter()
        .flatten()
        .collect()
}

fn unit_filename_candidate_tiers(unit_name: &str, namespaces: &[String]) -> Vec<Vec<String>> {
    let mut tiers = Vec::new();
    let short_name = unit_name.rsplit('.').next().unwrap_or(unit_name);
    let mut exact = vec![format!("{unit_name}.pas")];
    if !short_name.eq_ignore_ascii_case(unit_name) {
        exact.push(format!("{short_name}.pas"));
    }
    tiers.push(exact);
    if !unit_name.contains('.') {
        for namespace in namespaces {
            let namespace = namespace.trim().trim_matches('.');
            if !namespace.is_empty() {
                tiers.push(vec![format!("{namespace}.{unit_name}.pas")]);
            }
        }
    }
    for tier in &mut tiers {
        tier.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    }
    tiers
}

fn unit_name_matches(
    actual: &str,
    requested: &str,
    lookup: &str,
    context: &ProjectContext,
) -> bool {
    if actual.eq_ignore_ascii_case(lookup) || actual.eq_ignore_ascii_case(requested) {
        return true;
    }
    if lookup.contains('.') {
        return false;
    }
    context
        .unit_namespaces
        .iter()
        .any(|namespace| format!("{namespace}.{lookup}").eq_ignore_ascii_case(actual))
}

fn package_unit_name_matches(
    actual: &str,
    requested: &str,
    lookup: &str,
    context: &ProjectContext,
) -> bool {
    actual.eq_ignore_ascii_case(requested)
        || actual.eq_ignore_ascii_case(lookup)
        || (!lookup.contains('.')
            && context
                .unit_namespaces
                .iter()
                .any(|namespace| format!("{namespace}.{lookup}").eq_ignore_ascii_case(actual)))
}

fn is_default_excluded_component(component: Component<'_>) -> bool {
    let Component::Normal(name) = component else {
        return false;
    };
    let Some(name) = name.to_str() else {
        return false;
    };
    const DEFAULT_EXCLUDED_COMPONENTS: &[&str] = &[
        ".git",
        ".worktrees",
        ".hg",
        ".svn",
        ".idea",
        ".vscode",
        "target",
        "node_modules",
        "dist",
        "build",
        "coverage",
    ];
    #[cfg(windows)]
    {
        DEFAULT_EXCLUDED_COMPONENTS
            .iter()
            .any(|excluded| name.eq_ignore_ascii_case(excluded))
    }
    #[cfg(not(windows))]
    {
        DEFAULT_EXCLUDED_COMPONENTS.contains(&name)
    }
}

fn is_pascal_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("pas")
                || extension.eq_ignore_ascii_case("dpr")
                || extension.eq_ignore_ascii_case("dpk")
        })
}

fn extension_is(path: &Path, extension: &str) -> bool {
    path.extension()
        .is_some_and(|value| value.to_string_lossy().eq_ignore_ascii_case(extension))
}

fn is_configuration_path(uri: &Url) -> bool {
    let Ok(path) = uri.to_file_path() else {
        return false;
    };
    is_live_configuration_file(&path)
        || path.extension().is_some_and(|extension| {
            matches!(
                extension.to_string_lossy().to_ascii_lowercase().as_str(),
                "dproj" | "dpr" | "dpk" | "optset"
            )
        })
}

pub(crate) fn is_configuration_file(path: &Path) -> bool {
    is_live_configuration_file(path) || is_immutable_override_file(path)
}

fn is_live_configuration_file(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        CONFIGURATION_FILENAMES
            .iter()
            .any(|filename| name.to_string_lossy().eq_ignore_ascii_case(filename))
    })
}

fn is_immutable_override_file(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        name.to_string_lossy()
            .eq_ignore_ascii_case(LOCAL_CONFIG_NAME)
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ContextState, DiagnosticLineIndex, FileChange, ResourceLimits, Workspace, WorkspaceOptions,
        context_state_is_fresh_with_cancel, normalize_line_endings, scan_external_units,
    };
    use crate::NavigationTarget;
    use crate::project::{ProjectContext, ProjectPathEntry, ProjectPathProvenance};
    use lsp_types::{Position, Url};
    use pascal_core::delphi_overrides::{
        EffectiveOverrides, LOCAL_CONFIG_NAME, OverrideSession, PathMapping,
    };
    use std::collections::HashSet;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    fn test_workspace(roots: Vec<std::path::PathBuf>, options: WorkspaceOptions) -> Workspace {
        Workspace::with_override_session(roots, options, OverrideSession::new(None))
    }

    #[test]
    fn legacy_route_proof_requires_the_exact_source_and_owner_context() {
        let source = PathBuf::from("/external/Helper.pas");
        let key = super::ContextKey {
            project_file: Some(PathBuf::from("/workspace/A.dproj")),
            workspace_root: Some(PathBuf::from("/workspace")),
            project_scope: Some(PathBuf::from("/workspace")),
            selection_scope: None,
            selection_project: None,
            config: None,
            platform: None,
            overrides: EffectiveOverrides::default(),
        };
        let mut owner = super::KnownDocumentOwner {
            key: key.clone(),
            state: ContextState::default(),
            origin: super::OwnerOrigin::Inherited,
            legacy_route: Some(super::LegacyRouteProof {
                source: source.clone(),
                context: key.clone(),
            }),
        };

        assert!(owner.has_legacy_route(&source));
        assert!(!owner.has_legacy_route(&PathBuf::from("/external/Other.pas")));

        owner.key.project_file = Some(PathBuf::from("/workspace/B.dproj"));
        assert!(!owner.has_legacy_route(&source));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn legacy_route_proof_uses_native_case_sensitive_source_comparison() {
        let source = PathBuf::from("/external/Helper.pas");
        let key = super::ContextKey {
            project_file: Some(PathBuf::from("/workspace/App.dproj")),
            workspace_root: Some(PathBuf::from("/workspace")),
            project_scope: Some(PathBuf::from("/workspace")),
            selection_scope: None,
            selection_project: None,
            config: None,
            platform: None,
            overrides: EffectiveOverrides::default(),
        };
        let owner = super::KnownDocumentOwner {
            key: key.clone(),
            state: ContextState::default(),
            origin: super::OwnerOrigin::Inherited,
            legacy_route: Some(super::LegacyRouteProof {
                source,
                context: key,
            }),
        };

        assert!(!owner.has_legacy_route(Path::new("/external/helper.pas")));
    }

    #[cfg(unix)]
    fn mapped_package_fixture(temp_root: &Path, mapped_root: &Path) -> (Workspace, Url) {
        let workspace_root = temp_root.join("workspace");
        let main = workspace_root.join("Main.pas");
        let package = workspace_root.join("packages/Shared.dpk");
        let provider = workspace_root.join("packages/src/SharedUnit.pas");
        fs::create_dir_all(provider.parent().expect("package source directory"))
            .expect("package source directory");
        fs::write(
            &main,
            "unit Main;\ninterface\nuses SharedUnit;\nimplementation\nprocedure Run;\nbegin\n  SharedRoutine;\nend;\nend.\n",
        )
        .expect("main source");
        fs::write(
            &provider,
            "unit SharedUnit;\ninterface\nprocedure SharedRoutine;\nimplementation\nprocedure SharedRoutine; begin end;\nend.\n",
        )
        .expect("package provider");
        fs::write(
            &package,
            "package Shared;\ncontains\n  SharedUnit in 'src/SharedUnit.pas';\nend.\n",
        )
        .expect("package descriptor");
        fs::write(
            workspace_root.join("App.dproj"),
            r#"<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>C:\MappedSdk</DCC_UnitSearchPath><DCC_UsePackage>Shared</DCC_UsePackage></PropertyGroup></Project>"#,
        )
        .expect("project descriptor");
        fs::write(
            workspace_root.join(".delphi-tools.local.toml"),
            format!(
                "[[path_mappings]]\nfrom = 'C:\\MappedSdk'\nto = '{}'\n",
                mapped_root.display()
            ),
        )
        .expect("override configuration");

        let main_uri = Url::from_file_path(&main).expect("main URI");
        (
            test_workspace(vec![workspace_root], WorkspaceOptions::default()),
            main_uri,
        )
    }

    #[cfg(unix)]
    fn configured_native_package_fixture(
        temp_root: &Path,
        configured_root: &Path,
        mapped_root: &Path,
    ) -> (Workspace, Url) {
        let workspace_root = temp_root.join("workspace");
        let main = workspace_root.join("Main.pas");
        let package = workspace_root.join("packages/Shared.dpk");
        let provider = workspace_root.join("packages/src/SharedUnit.pas");
        fs::create_dir_all(provider.parent().expect("package source directory"))
            .expect("package source directory");
        fs::write(
            &main,
            "unit Main;\ninterface\nuses SharedUnit;\nimplementation\nprocedure Run;\nbegin\n  SharedRoutine;\nend;\nend.\n",
        )
        .expect("main source");
        fs::write(
            &provider,
            "unit SharedUnit;\ninterface\nprocedure SharedRoutine;\nimplementation\nprocedure SharedRoutine; begin end;\nend.\n",
        )
        .expect("package provider");
        fs::write(
            &package,
            "package Shared;\ncontains\n  SharedUnit in 'src/SharedUnit.pas';\nend.\n",
        )
        .expect("package descriptor");
        fs::write(
            workspace_root.join("App.dproj"),
            r#"<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>$(SDK)</DCC_UnitSearchPath><DCC_UsePackage>Shared</DCC_UsePackage></PropertyGroup></Project>"#,
        )
        .expect("project descriptor");
        fs::write(
            workspace_root.join(".delphi-tools.local.toml"),
            format!(
                "[properties]\nSDK = '{}'\n[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
                configured_root.display(),
                mapped_root.display()
            ),
        )
        .expect("override configuration");

        let main_uri = Url::from_file_path(&main).expect("main URI");
        (
            test_workspace(vec![workspace_root], WorkspaceOptions::default()),
            main_uri,
        )
    }

    #[cfg(unix)]
    struct PermissionsRestore {
        path: PathBuf,
        mode: u32,
    }

    #[cfg(unix)]
    impl Drop for PermissionsRestore {
        fn drop(&mut self) {
            if let Ok(mut permissions) =
                fs::metadata(&self.path).map(|metadata| metadata.permissions())
            {
                permissions.set_mode(self.mode);
                let _ = fs::set_permissions(&self.path, permissions);
            }
        }
    }

    #[test]
    fn diagnostic_line_index_reuses_utf16_prefixes_for_unicode_and_bare_cr() {
        let source = normalize_line_endings("😀abc\rdef\r\nghi");
        let index = DiagnosticLineIndex::new(&source);

        assert_eq!(source, "😀abc\ndef\nghi");
        assert_eq!(index.position(1, 5).expect("after emoji").character, 2);
        assert_eq!(index.position(2, 4).expect("second line end").character, 3);
        assert_eq!(index.position(3, 4).expect("third line end").character, 3);
        assert_eq!(index.utf16_prefix.len(), source.len() + 1);
    }

    #[test]
    fn diagnostic_line_index_builds_a_single_long_line_prefix_table() {
        let source = "x".repeat(46 * 1024);
        let index = DiagnosticLineIndex::new(&source);

        assert_eq!(index.lines.len(), 1);
        assert_eq!(index.utf16_prefix.len(), source.len() + 1);
        assert_eq!(
            index
                .position(1, source.len() + 1)
                .expect("long line end")
                .character as usize,
            source.len()
        );
    }

    #[test]
    fn formatting_edit_keeps_the_read_only_workspace_api() {
        let workspace = test_workspace(Vec::new(), Default::default());
        let uri = Url::parse("file:///tmp/Main.pas").expect("file URI");

        let _ = workspace.formatting_edit(&uri);
    }

    #[test]
    fn immutable_overrides_are_not_live_configuration_watchers() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        fs::create_dir_all(&root).expect("workspace directory");
        let mut workspace = test_workspace(vec![root.clone()], Default::default());

        let paths = workspace.configuration_watch_paths();
        assert!(paths.contains(&root.join(".lint4d.toml")));
        assert!(paths.contains(&root.join(".fmt4d.toml")));
        assert!(!paths.contains(&root.join(LOCAL_CONFIG_NAME)));

        let override_uri = Url::from_file_path(root.join(LOCAL_CONFIG_NAME)).expect("override URI");
        let source_generation = workspace.source_generation();
        let configuration_generation = workspace.configuration_generation();
        workspace.file_event(&override_uri, FileChange::Changed);
        assert_eq!(workspace.source_generation(), source_generation);
        assert_eq!(
            workspace.configuration_generation(),
            configuration_generation
        );

        let live_uri = Url::from_file_path(root.join(".lint4d.toml")).expect("lint URI");
        workspace.file_event(&live_uri, FileChange::Changed);
        assert_ne!(workspace.source_generation(), source_generation);
        assert_ne!(
            workspace.configuration_generation(),
            configuration_generation
        );
    }

    #[test]
    fn malformed_user_override_is_retained_before_project_context_request() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let source = root.join("Main.pas");
        let user_config = temp.path().join("user.toml");
        fs::create_dir_all(&root).expect("workspace directory");
        fs::write(&source, "unit Main; interface implementation end.\n").expect("source");
        fs::write(&user_config, "[properties\ninvalid = 'user'\n").expect("user config");

        let session = OverrideSession::new(Some(user_config.clone()));
        let mut workspace =
            Workspace::with_override_session(vec![root], Default::default(), session);
        assert!(
            workspace
                .warnings()
                .iter()
                .any(|warning| warning.contains(&user_config.display().to_string())),
            "user override error was not retained at construction: {:?}",
            workspace.warnings()
        );

        let uri = Url::from_file_path(source).expect("source URI");
        let context = workspace
            .project_context(&uri)
            .expect("malformed user override must remain inspectable in project context");
        assert!(
            context
                .warnings
                .iter()
                .any(|warning| warning.contains(&user_config.display().to_string())),
            "project context lost malformed user override provenance: {:?}",
            context.warnings
        );
    }

    #[test]
    fn malformed_override_blocks_navigation_after_context_retention() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let consumer = root.join("Consumer.pas");
        let provider = root.join("Provider.pas");
        let user_config = temp.path().join("user.toml");
        let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
        fs::create_dir_all(&root).expect("workspace directory");
        fs::write(&consumer, consumer_source).expect("consumer source");
        fs::write(
            &provider,
            "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n",
        )
        .expect("provider source");
        fs::write(&user_config, "[properties\ninvalid = 'user'\n").expect("user config");

        let session = OverrideSession::new(Some(user_config.clone()));
        let mut workspace =
            Workspace::with_override_session(vec![root], Default::default(), session);
        let uri = Url::from_file_path(&consumer).expect("consumer URI");
        let locations = workspace.navigate(&uri, Position::new(7, 2), NavigationTarget::Definition);

        assert!(
            locations.is_empty(),
            "navigation claimed a result with invalid override configuration: {locations:?}"
        );
        assert!(
            workspace
                .warnings()
                .iter()
                .any(|warning| warning.contains(&user_config.display().to_string())),
            "navigation did not retain override provenance: {:?}",
            workspace.warnings()
        );
    }

    #[test]
    fn malformed_override_blocks_formatting_after_context_retention() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let source = root.join("Main.pas");
        let user_config = temp.path().join("user.toml");
        fs::create_dir_all(&root).expect("workspace directory");
        fs::write(
            &source,
            "unit Main;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nLog(1);\nend;\nend.\n",
        )
        .expect("source");
        fs::write(root.join(".fmt4d.toml"), "[format]\nindent_size = 2\n")
            .expect("formatter configuration");
        fs::write(&user_config, "[properties\ninvalid = 'user'\n").expect("user config");

        let session = OverrideSession::new(Some(user_config.clone()));
        let workspace = Workspace::with_override_session(vec![root], Default::default(), session);
        let uri = Url::from_file_path(&source).expect("source URI");
        let error = workspace
            .formatting_edit(&uri)
            .expect_err("formatting must fail closed for invalid override configuration");

        assert!(
            error.contains(&user_config.display().to_string()),
            "{error}"
        );
    }

    #[test]
    fn malformed_override_blocks_rename_after_context_retention() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let consumer = root.join("Consumer.pas");
        let provider = root.join("Provider.pas");
        let user_config = temp.path().join("user.toml");
        fs::create_dir_all(&root).expect("workspace directory");
        fs::write(
            &consumer,
            "unit Consumer;\ninterface\nuses Provider;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n",
        )
        .expect("consumer source");
        fs::write(
            &provider,
            "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n",
        )
        .expect("provider source");
        fs::write(&user_config, "[properties\ninvalid = 'user'\n").expect("user config");

        let session = OverrideSession::new(Some(user_config.clone()));
        let mut workspace =
            Workspace::with_override_session(vec![root], Default::default(), session);
        let uri = Url::from_file_path(&consumer).expect("consumer URI");
        let error = workspace
            .rename_edits(&uri, Position::new(7, 2), "Renamed", false)
            .expect_err("rename must fail closed for invalid override configuration");

        assert!(
            error.contains("project override configuration is invalid")
                && error.contains(&user_config.display().to_string()),
            "{error}"
        );
    }

    #[test]
    fn workspace_folder_readdition_keeps_captured_override_settings() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let source = root.join("Main.pas");
        let config = root.join(".delphi-tools.local.toml");
        fs::create_dir_all(&root).expect("workspace directory");
        fs::write(&source, "unit Main; interface implementation end.\n").expect("source");
        fs::write(&config, "[properties]\nBDS = 'first'\n").expect("initial override");

        let mut workspace = test_workspace(Vec::new(), Default::default());
        workspace.update_workspace_folders([root.clone()], []);
        let uri = Url::from_file_path(&source).expect("source URI");
        let first_key = workspace
            .context_for_uri(&uri)
            .expect("initial project context");
        let first = &workspace
            .contexts
            .get(&first_key)
            .expect("initial context state")
            .context;
        assert!(
            first
                .warnings
                .iter()
                .all(|warning| !warning.contains(&config.display().to_string())),
            "initial override configuration failed: {:?}",
            first.warnings
        );

        workspace.update_workspace_folders([], [root.clone()]);
        fs::write(&config, "[properties]\nBDS = 'second'\n").expect("replacement override");
        workspace.update_workspace_folders([root], []);

        let second_key = workspace
            .context_for_uri(&uri)
            .expect("re-added project context");
        let second = &workspace
            .contexts
            .get(&second_key)
            .expect("re-added context state")
            .context;
        assert_eq!(
            second.overrides.properties.get("bds").map(String::as_str),
            Some("first")
        );
    }

    #[test]
    fn failed_candidate_observations_are_never_fresh() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let mut state = ContextState::default();
        state.project_candidate_memberships.insert(
            temp.path().to_path_buf(),
            Err("directory observation failed".to_string()),
        );

        assert!(
            !context_state_is_fresh_with_cancel(&state, None).expect("freshness check"),
            "repeated candidate-enumeration errors must not prove freshness"
        );
    }

    #[test]
    fn cancellable_dependency_loading_honors_request_cancellation() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let consumer = temp.path().join("Consumer.pas");
        let provider = temp.path().join("Provider.pas");
        fs::write(
            &consumer,
            "unit Consumer; interface uses Provider; implementation end.\n",
        )
        .expect("consumer source");
        fs::write(&provider, "unit Provider; interface implementation end.\n")
            .expect("provider source");

        let mut workspace =
            Workspace::new(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
        let consumer_uri = Url::from_file_path(&consumer).expect("consumer URI");
        let context_key = workspace
            .context_for_uri(&consumer_uri)
            .expect("consumer context");
        workspace
            .index
            .update(
                consumer_uri.clone(),
                fs::read_to_string(&consumer).expect("consumer text"),
            )
            .expect("consumer index");
        let mut pinned = HashSet::new();
        let cancel = AtomicBool::new(true);

        let error = workspace
            .load_imports_with_cancel(&consumer_uri, &context_key, &mut pinned, Some(&cancel))
            .expect_err("cancelled dependency loading must not read dependencies");
        assert_eq!(error, "request cancelled");
    }

    #[test]
    fn external_context_rediscovery_observes_ancestor_project_candidates() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let external = temp.path().join("library/src");
        let source = external.join("External.pas");
        fs::create_dir_all(&root).expect("workspace directory");
        fs::create_dir_all(&external).expect("external source directory");
        fs::write(&source, "unit External; interface implementation end.\n")
            .expect("external source");

        let mut workspace = test_workspace(
            vec![root],
            WorkspaceOptions {
                source_paths: vec![external.to_string_lossy().into_owned()],
                ..WorkspaceOptions::default()
            },
        );
        let uri = Url::from_file_path(&source).expect("source URI");
        let initial_key = workspace
            .context_for_uri(&uri)
            .expect("initial external context");
        assert!(
            workspace
                .contexts
                .get(&initial_key)
                .expect("initial context state")
                .context
                .project_file
                .is_none()
        );

        let project = temp.path().join("library/App.dproj");
        fs::write(
            &project,
            "<Project><PropertyGroup><MainSource>src/External.pas</MainSource></PropertyGroup></Project>",
        )
        .expect("ancestor project");
        let refreshed_key = workspace
            .context_for_uri(&uri)
            .expect("refreshed external context");
        assert_eq!(
            workspace
                .contexts
                .get(&refreshed_key)
                .expect("refreshed context state")
                .context
                .project_file
                .as_deref(),
            Some(project.as_path())
        );
    }

    #[test]
    fn internal_context_rediscovery_observes_candidates_above_an_overlapping_source_path() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let source_dir = root.join("src");
        let source = source_dir.join("Main.pas");
        fs::create_dir_all(&source_dir).expect("source directory");
        fs::write(&source, "unit Main; interface implementation end.\n").expect("source");

        let mut workspace = test_workspace(
            vec![root.clone()],
            WorkspaceOptions {
                source_paths: vec![source_dir.to_string_lossy().into_owned()],
                ..WorkspaceOptions::default()
            },
        );
        let uri = Url::from_file_path(&source).expect("source URI");
        let initial_key = workspace
            .context_for_uri(&uri)
            .expect("initial internal context");
        assert!(
            workspace
                .contexts
                .get(&initial_key)
                .expect("initial context state")
                .context
                .project_file
                .is_none()
        );

        let project = root.join("App.dproj");
        fs::write(
            &project,
            "<Project><PropertyGroup><MainSource>src/Main.pas</MainSource></PropertyGroup></Project>",
        )
        .expect("workspace project");
        let refreshed_key = workspace
            .context_for_uri(&uri)
            .expect("refreshed internal context");
        assert_eq!(
            workspace
                .contexts
                .get(&refreshed_key)
                .expect("refreshed context state")
                .context
                .project_file
                .as_deref(),
            Some(project.as_path())
        );
    }

    #[cfg(unix)]
    #[test]
    fn external_unit_scanner_does_not_block_on_fifo_entries() {
        if std::env::var_os("LINT4D_EXTERNAL_FIFO_CHILD").is_some() {
            let vendor = std::path::PathBuf::from(
                std::env::var_os("LINT4D_EXTERNAL_FIFO_VENDOR").expect("vendor path"),
            );
            let target = vendor.join("Race.pas");
            let fifo = vendor.join("Race.fifo");
            let held = vendor.join("Race.hold");
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let replacer_stop = stop.clone();
            let replacer = std::thread::spawn(move || {
                while !replacer_stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = fs::rename(&target, &held);
                    let _ = fs::rename(&fifo, &target);
                    let _ = fs::rename(&target, &fifo);
                    let _ = fs::rename(&held, &target);
                }
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while std::time::Instant::now() < deadline {
                let _ = scan_external_units(
                    vendor.parent().expect("vendor parent"),
                    &["vendor".to_string()],
                    &ResourceLimits::default(),
                );
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            replacer.join().expect("join FIFO replacer");
            return;
        }

        use std::os::unix::fs::symlink;
        use std::process::Command;
        use std::time::{Duration, Instant};

        let temp = tempfile::tempdir().expect("temporary workspace");
        let vendor = temp.path().join("vendor");
        fs::create_dir(&vendor).expect("vendor directory");
        fs::write(
            vendor.join("Race.pas"),
            "unit Race; interface implementation end.\n",
        )
        .expect("regular replacement source");
        let fifo = vendor.join("Race.fifo");
        let status = Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo command");
        assert!(status.success(), "mkfifo failed for {}", fifo.display());
        symlink(&fifo, vendor.join("Linked.pas")).expect("FIFO symlink");

        let mut child = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "workspace::tests::external_unit_scanner_does_not_block_on_fifo_entries",
                "--nocapture",
            ])
            .env("LINT4D_EXTERNAL_FIFO_CHILD", "1")
            .env("LINT4D_EXTERNAL_FIFO_VENDOR", &vendor)
            .spawn()
            .expect("spawn FIFO child");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = child.try_wait().expect("poll FIFO child") {
                assert!(status.success(), "FIFO child exited with {status}");
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("external-unit FIFO inspection blocked");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn external_unit_scanner_rejects_symlinked_roots() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temporary workspace");
        let target = temp.path().join("target");
        let link = temp.path().join("linked-vendor");
        fs::create_dir(&target).expect("target directory");
        symlink(&target, &link).expect("symlinked external root");

        let error = scan_external_units(
            temp.path(),
            &["linked-vendor".to_string()],
            &ResourceLimits::default(),
        )
        .expect_err("symlinked external roots must be rejected");
        assert!(
            error.contains("symlink") || error.contains("regular directory"),
            "unexpected symlinked-root error: {error}"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn install_context_keeps_case_distinct_linux_metadata_paths() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let source_dir = root.join("src");
        fs::create_dir_all(&source_dir).expect("source directory");
        let source = source_dir.join("Main.pas");
        let upper = root.join("Debug.optset");
        let lower = root.join("debug.optset");
        fs::write(&source, "unit Main; interface implementation end.\n").expect("source");
        fs::write(&upper, "upper\n").expect("upper metadata");
        fs::write(&lower, "lower\n").expect("lower metadata");

        let mut workspace = test_workspace(vec![root.clone()], Default::default());
        let key = super::ContextKey {
            project_file: None,
            workspace_root: Some(root),
            project_scope: None,
            selection_scope: None,
            selection_project: None,
            config: None,
            platform: None,
            overrides: EffectiveOverrides::default(),
        };
        let context = ProjectContext {
            metadata_files: vec![upper.clone(), lower.clone()],
            ..ProjectContext::default()
        };

        workspace
            .install_context(
                key.clone(),
                context,
                Vec::new(),
                std::collections::HashMap::new(),
                &source,
                None,
            )
            .expect("install context");
        let watched_paths = &workspace
            .contexts
            .get(&key)
            .expect("installed context")
            .watched_paths;
        assert!(watched_paths.contains_key(&upper));
        assert!(watched_paths.contains_key(&lower));
    }

    #[test]
    fn delphi_overrides_package_keys_include_effective_inputs() {
        let base = super::PackageMetadataKey {
            descriptor: std::path::PathBuf::from("/packages/Shared.dproj"),
            overrides: EffectiveOverrides::default(),
            read_policy: crate::project::ReadPolicy::default(),
            config: Some("Debug".into()),
            platform: Some("Win32".into()),
        };
        let mut other = base.clone();
        other
            .overrides
            .properties
            .insert("bds".into(), r"D:\SDK".into());
        assert_ne!(base, other);
        other = base.clone();
        other.platform = Some("Win64".into());
        assert_ne!(base, other);
    }

    #[cfg(unix)]
    #[test]
    fn mapped_package_catalogue_rejects_a_symlinked_destination_root() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temporary directory");
        let target = temp.path().join("sdk");
        let mapped_root = temp.path().join("mapped-sdk");
        fs::create_dir(&target).expect("target directory");
        fs::write(target.join("Shared.dpk"), "package Shared; end.\n").expect("package descriptor");
        symlink(&target, &mapped_root).expect("mapped symlink");

        let mut workspace = test_workspace(Vec::new(), WorkspaceOptions::default());
        let key = super::ContextKey {
            project_file: None,
            workspace_root: None,
            project_scope: None,
            selection_scope: None,
            selection_project: None,
            config: None,
            platform: None,
            overrides: EffectiveOverrides {
                path_mappings: vec![PathMapping {
                    from: "c:/sdk".to_owned(),
                    to: mapped_root.clone(),
                    config_file: temp.path().join("overrides.toml"),
                }],
                ..EffectiveOverrides::default()
            },
        };
        let requested_names = HashSet::from(["shared".to_owned()]);
        let catalogue = workspace
            .package_catalogue(&key, &mapped_root, &requested_names, None)
            .expect("package catalogue");

        assert!(!catalogue.complete, "symlinked roots must be incomplete");
        assert!(
            catalogue.entries.is_empty(),
            "symlinked roots must not be scanned"
        );
    }

    #[cfg(unix)]
    #[test]
    fn relevant_missing_mapped_package_root_keeps_package_lookup_incomplete() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let workspace_root = temp.path().join("workspace");
        let main = workspace_root.join("Main.pas");
        let package = workspace_root.join("packages/Shared.dpk");
        let provider = workspace_root.join("packages/src/SharedUnit.pas");
        let missing_root = temp.path().join("missing-sdk");
        fs::create_dir_all(provider.parent().expect("package source directory"))
            .expect("package source directory");
        fs::write(
            &main,
            "unit Main;\ninterface\nuses SharedUnit;\nimplementation\nprocedure Run;\nbegin\n  SharedRoutine;\nend;\nend.\n",
        )
        .expect("main source");
        fs::write(
            &provider,
            "unit SharedUnit;\ninterface\nprocedure SharedRoutine;\nimplementation\nprocedure SharedRoutine; begin end;\nend.\n",
        )
        .expect("package provider");
        fs::write(
            &package,
            "package Shared;\ncontains\n  SharedUnit in 'src/SharedUnit.pas';\nend.\n",
        )
        .expect("package descriptor");
        fs::write(
            workspace_root.join("App.dproj"),
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>C:\\MissingSdk</DCC_UnitSearchPath><DCC_UsePackage>Shared</DCC_UsePackage></PropertyGroup></Project>",
        )
        .expect("project descriptor");
        fs::write(
            workspace_root.join(".delphi-tools.local.toml"),
            format!(
                "[[path_mappings]]\nfrom = 'C:\\MissingSdk'\nto = '{}'\n",
                missing_root.display()
            ),
        )
        .expect("override configuration");

        let mut workspace = test_workspace(vec![workspace_root], WorkspaceOptions::default());
        let locations = workspace.navigate(
            &Url::from_file_path(&main).expect("main URI"),
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );

        assert!(
            locations.is_empty(),
            "missing mapped package root must not permit a unique package result: {locations:?}"
        );
        assert!(
            workspace.warnings().iter().any(|warning| {
                warning.contains("package shared")
                    && warning.contains("bounded source catalogue")
                    && warning.contains("incomplete")
            }),
            "missing mapped package root did not produce an actionable incomplete warning: {:?}",
            workspace.warnings()
        );
    }

    #[cfg(unix)]
    #[test]
    fn relevant_missing_configured_native_package_root_keeps_package_lookup_incomplete() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let missing_root = temp.path().join("missing-sdk");
        let (mut workspace, main_uri) =
            configured_native_package_fixture(temp.path(), &missing_root, &missing_root);
        let locations = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );

        assert!(
            locations.is_empty(),
            "missing configured native package root must not permit a unique package result: {locations:?}"
        );
        assert!(
            workspace.warnings().iter().any(|warning| {
                warning.contains("package shared")
                    && warning.contains("bounded source catalogue")
                    && warning.contains("incomplete")
            }),
            "missing configured native package root did not produce an actionable incomplete warning: {:?}",
            workspace.warnings()
        );
    }

    #[cfg(unix)]
    #[test]
    fn empty_readable_configured_native_package_root_preserves_a_unique_package_result() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let configured_root = temp.path().join("empty-sdk");
        fs::create_dir(&configured_root).expect("empty configured native root");

        let (mut workspace, main_uri) =
            configured_native_package_fixture(temp.path(), &configured_root, &configured_root);
        let locations = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );

        assert_eq!(locations.len(), 1);
        assert_eq!(
            locations[0].uri,
            Url::from_file_path(temp.path().join("workspace/packages/src/SharedUnit.pas"))
                .expect("provider URI")
        );
        assert!(
            workspace
                .warnings()
                .iter()
                .all(|warning| !warning.contains("bounded source catalogue")),
            "empty readable configured native root made the catalogue incomplete: {:?}",
            workspace.warnings()
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_configured_native_package_root_revalidates_after_create_delete_and_restore() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let configured_root = temp.path().join("restored-sdk");
        let (mut workspace, main_uri) =
            configured_native_package_fixture(temp.path(), &configured_root, &configured_root);

        let initially_missing = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );
        assert!(initially_missing.is_empty());

        fs::create_dir(&configured_root).expect("create configured native root");
        let created = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );
        assert_eq!(created.len(), 1);

        fs::remove_dir(&configured_root).expect("remove configured native root");
        let deleted = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );
        assert!(
            deleted.is_empty(),
            "deleted configured native root reused a stale complete catalogue: {deleted:?}"
        );
        assert!(
            workspace.warnings().iter().any(|warning| {
                warning.contains("package shared")
                    && warning.contains("bounded source catalogue")
                    && warning.contains("incomplete")
            }),
            "deleted configured native root did not remain incomplete: {:?}",
            workspace.warnings()
        );

        fs::create_dir(&configured_root).expect("restore configured native root");
        let restored = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );
        assert_eq!(
            restored.len(),
            1,
            "restored empty configured native root should resolve only the unique workspace package: {restored:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn case_adjusted_configured_native_package_root_stays_incomplete_after_deletion() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let actual_root = temp.path().join("SDK");
        let configured_root = temp.path().join("sdk");
        fs::create_dir(&actual_root).expect("case-adjusted configured native root");

        let (mut workspace, main_uri) =
            configured_native_package_fixture(temp.path(), &configured_root, &configured_root);
        let initial = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );
        assert_eq!(
            initial.len(),
            1,
            "case-adjusted readable root should preserve the unique package: {initial:?}"
        );

        fs::remove_dir(&actual_root).expect("delete case-adjusted configured native root");
        let missing = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );
        assert!(
            missing.is_empty(),
            "deleted case-adjusted root must not reveal a unique workspace package: {missing:?}"
        );
        assert!(
            workspace.warnings().iter().any(|warning| {
                warning.contains("package shared")
                    && warning.contains("bounded source catalogue")
                    && warning.contains("incomplete")
            }),
            "deleted case-adjusted configured root did not remain incomplete: {:?}",
            workspace.warnings()
        );

        fs::create_dir(&actual_root).expect("restore case-adjusted configured native root");
        workspace.warnings.clear();
        let restored = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );
        assert_eq!(
            restored.len(),
            1,
            "restored case-adjusted root should resolve the unique workspace package: {restored:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unrelated_prefix_similar_mapping_does_not_grant_a_configured_native_root() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let mapped_root = temp.path().join("missing-sdk");
        let configured_root = temp.path().join("missing-sdk-extra");
        let (mut workspace, main_uri) =
            configured_native_package_fixture(temp.path(), &configured_root, &mapped_root);
        let locations = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );

        assert_eq!(
            locations.len(),
            1,
            "prefix-similar unrelated mapping must not make the package catalogue incomplete: {locations:?}"
        );
        assert!(
            workspace
                .warnings()
                .iter()
                .all(|warning| !warning.contains("bounded source catalogue")),
            "unrelated missing mapping incorrectly granted the configured root: {:?}",
            workspace.warnings()
        );
    }

    #[cfg(unix)]
    #[test]
    fn configured_native_entries_use_component_containment_for_nested_files() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let configured_root = temp.path().join("missing-sdk");
        let nested_file = configured_root.join("nested/Provider.pas");
        let sibling = temp.path().join("missing-sdk-extra/Provider.pas");
        let context = ProjectContext {
            search_path_entries: vec![ProjectPathEntry {
                path: nested_file,
                provenance: ProjectPathProvenance::Configured,
            }],
            ..ProjectContext::default()
        };

        assert!(super::context_uses_mapped_root(
            &context,
            &configured_root,
            &configured_root
        ));
        let unrelated = ProjectContext {
            search_path_entries: vec![ProjectPathEntry {
                path: sibling,
                provenance: ProjectPathProvenance::Configured,
            }],
            ..ProjectContext::default()
        };
        assert!(!super::context_uses_mapped_root(
            &unrelated,
            &configured_root,
            &configured_root
        ));

        let existing_root = temp.path().join("mapped-sdk");
        let case_distinct_root = temp.path().join("MAPPED-SDK");
        fs::create_dir(&existing_root).expect("existing mapped destination");
        let case_distinct = ProjectContext {
            search_path_entries: vec![ProjectPathEntry {
                path: case_distinct_root.join("Provider.pas"),
                provenance: ProjectPathProvenance::Configured,
            }],
            ..ProjectContext::default()
        };
        assert!(!super::context_uses_mapped_root(
            &case_distinct,
            &existing_root,
            &existing_root
        ));
    }

    #[cfg(unix)]
    #[test]
    fn non_directory_and_inaccessible_mapped_package_roots_keep_lookup_incomplete() {
        for kind in ["non-directory", "inaccessible"] {
            let temp = tempfile::tempdir().expect("temporary workspace");
            let mapped_root = temp.path().join(kind);
            let inaccessible = kind == "inaccessible";
            if inaccessible {
                fs::create_dir(&mapped_root).expect("inaccessible mapped root");
                let original_mode = fs::metadata(&mapped_root)
                    .expect("inaccessible mapped root metadata")
                    .permissions()
                    .mode();
                let _restore = PermissionsRestore {
                    path: mapped_root.clone(),
                    mode: original_mode,
                };
                let mut permissions = fs::metadata(&mapped_root)
                    .expect("inaccessible mapped root metadata")
                    .permissions();
                permissions.set_mode(0o0);
                fs::set_permissions(&mapped_root, permissions)
                    .expect("remove mapped root permissions");
                if fs::read_dir(&mapped_root).is_ok() {
                    eprintln!(
                        "skipping inaccessible mapped-root case: runner can read mode-000 directory"
                    );
                    continue;
                }
                assert!(
                    fs::read_dir(&mapped_root).is_err(),
                    "permission-revocation fixture must fail read_dir before testing the case"
                );

                let (mut workspace, main_uri) = mapped_package_fixture(temp.path(), &mapped_root);
                let locations = workspace.navigate(
                    &main_uri,
                    Position::new(6, 2),
                    NavigationTarget::Declaration,
                );
                let warnings = workspace.warnings().to_vec();

                assert!(
                    locations.is_empty(),
                    "{kind} mapped package root must not permit a unique result: {locations:?}"
                );
                assert!(
                    warnings.iter().any(|warning| {
                        warning.contains("package shared")
                            && warning.contains("bounded source catalogue")
                            && warning.contains("incomplete")
                    }),
                    "{kind} mapped package root did not produce an actionable incomplete warning: {warnings:?}"
                );
            } else {
                fs::write(&mapped_root, "not a directory").expect("non-directory mapped root");
                let (mut workspace, main_uri) = mapped_package_fixture(temp.path(), &mapped_root);
                let locations = workspace.navigate(
                    &main_uri,
                    Position::new(6, 2),
                    NavigationTarget::Declaration,
                );
                let warnings = workspace.warnings().to_vec();

                assert!(
                    locations.is_empty(),
                    "{kind} mapped package root must not permit a unique result: {locations:?}"
                );
                assert!(
                    warnings.iter().any(|warning| {
                        warning.contains("package shared")
                            && warning.contains("bounded source catalogue")
                            && warning.contains("incomplete")
                    }),
                    "{kind} mapped package root did not produce an actionable incomplete warning: {warnings:?}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn empty_readable_mapped_package_root_preserves_a_unique_package_result() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let mapped_root = temp.path().join("empty-sdk");
        fs::create_dir(&mapped_root).expect("empty mapped root");

        let (mut workspace, main_uri) = mapped_package_fixture(temp.path(), &mapped_root);
        let locations = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );

        assert_eq!(locations.len(), 1);
        assert_eq!(
            locations[0].uri,
            Url::from_file_path(temp.path().join("workspace/packages/src/SharedUnit.pas"))
                .expect("provider URI")
        );
        assert!(
            workspace
                .warnings()
                .iter()
                .all(|warning| !warning.contains("bounded source catalogue")),
            "empty readable mapped root made the catalogue incomplete: {:?}",
            workspace.warnings()
        );
    }

    #[cfg(unix)]
    #[test]
    fn case_adjusted_mapped_package_root_contributes_duplicate_descriptors() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let actual_root = temp.path().join("SDK");
        fs::create_dir_all(actual_root.join("src")).expect("case-adjusted mapped root");
        fs::write(
            actual_root.join("Shared.dpk"),
            "package Shared;\ncontains\n  SharedUnit in 'src/SharedUnit.pas';\nend.\n",
        )
        .expect("duplicate package descriptor");
        fs::write(
            actual_root.join("src/SharedUnit.pas"),
            "unit SharedUnit; interface implementation end.\n",
        )
        .expect("duplicate package source");

        let configured_root = temp.path().join("sdk");
        let (mut workspace, main_uri) = mapped_package_fixture(temp.path(), &configured_root);
        let locations = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );

        assert!(
            locations.is_empty(),
            "case-adjusted duplicate descriptor must not produce a unique result: {locations:?}"
        );
        assert!(
            workspace.warnings().iter().any(|warning| {
                warning.contains("ambiguous package shared")
                    && warning.contains(&actual_root.join("Shared.dpk").display().to_string())
            }),
            "case-adjusted duplicate descriptor did not produce an ambiguity warning: {:?}",
            workspace.warnings()
        );
    }

    #[cfg(unix)]
    #[test]
    fn case_adjusted_mapped_package_root_stays_incomplete_after_deletion() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let actual_root = temp.path().join("SDK");
        fs::create_dir_all(actual_root.join("src")).expect("case-adjusted mapped root");
        fs::write(
            actual_root.join("Shared.dpk"),
            "package Shared;\ncontains\n  SharedUnit in 'src/SharedUnit.pas';\nend.\n",
        )
        .expect("duplicate package descriptor");
        fs::write(
            actual_root.join("src/SharedUnit.pas"),
            "unit SharedUnit; interface implementation end.\n",
        )
        .expect("duplicate package source");

        let configured_root = temp.path().join("sdk");
        let (mut workspace, main_uri) = mapped_package_fixture(temp.path(), &configured_root);
        let initial = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );
        assert!(
            initial.is_empty(),
            "the existing case-adjusted duplicate must be ambiguous: {initial:?}"
        );

        fs::remove_dir_all(&actual_root).expect("delete case-adjusted mapped root");
        let missing = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );
        assert!(
            missing.is_empty(),
            "deleted case-adjusted root must not reveal a unique workspace package: {missing:?}"
        );
        assert!(
            workspace.warnings().iter().any(|warning| {
                warning.contains("package shared")
                    && warning.contains("bounded source catalogue")
                    && warning.contains("incomplete")
            }),
            "deleted case-adjusted root did not remain incomplete: {:?}",
            workspace.warnings()
        );

        fs::create_dir_all(actual_root.join("src")).expect("restore case-adjusted mapped root");
        fs::write(
            actual_root.join("Shared.dpk"),
            "package Shared;\ncontains\n  SharedUnit in 'src/SharedUnit.pas';\nend.\n",
        )
        .expect("restored duplicate package descriptor");
        fs::write(
            actual_root.join("src/SharedUnit.pas"),
            "unit SharedUnit; interface implementation end.\n",
        )
        .expect("restored duplicate package source");
        workspace.warnings.clear();
        let restored = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );
        assert!(
            restored.is_empty(),
            "restored case-adjusted root must participate in the duplicate: {restored:?}"
        );
        assert!(
            workspace.warnings().iter().any(|warning| {
                warning.contains("ambiguous package shared")
                    && warning.contains(&actual_root.join("Shared.dpk").display().to_string())
            }),
            "restored case-adjusted root did not participate in ambiguity: {:?}",
            workspace.warnings()
        );
    }

    #[cfg(unix)]
    #[test]
    fn mapped_package_navigation_rejects_a_symlinked_destination_root() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temporary workspace");
        let target = temp.path().join("sdk-target");
        let mapped_root = temp.path().join("mapped-sdk");
        fs::create_dir(&target).expect("symlink target directory");
        symlink(&target, &mapped_root).expect("mapped symlink");

        let (mut workspace, main_uri) = mapped_package_fixture(temp.path(), &mapped_root);
        let locations = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );

        assert!(
            locations.is_empty(),
            "symlinked mapped root must not permit a unique package result: {locations:?}"
        );
        assert!(
            workspace.warnings().iter().any(|warning| {
                warning.contains("package shared")
                    && warning.contains("bounded source catalogue")
                    && warning.contains("incomplete")
            }),
            "symlinked mapped root did not produce an actionable incomplete warning: {:?}",
            workspace.warnings()
        );
    }

    #[cfg(unix)]
    #[test]
    fn package_catalogue_revalidates_directory_permissions_on_revoke_and_restore() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let mapped_root = temp.path().join("mapped-sdk");
        fs::create_dir(&mapped_root).expect("mapped root");
        let (mut workspace, main_uri) = mapped_package_fixture(temp.path(), &mapped_root);

        let initial = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );
        assert_eq!(
            initial.len(),
            1,
            "readable empty root should be cached complete"
        );

        let original_mode = fs::metadata(&mapped_root)
            .expect("mapped root metadata")
            .permissions()
            .mode();
        let _restore = PermissionsRestore {
            path: mapped_root.clone(),
            mode: original_mode,
        };
        let mut inaccessible = fs::metadata(&mapped_root)
            .expect("mapped root metadata")
            .permissions();
        inaccessible.set_mode(0o0);
        fs::set_permissions(&mapped_root, inaccessible).expect("revoke mapped root permissions");

        if fs::read_dir(&mapped_root).is_ok() {
            eprintln!(
                "skipping permission-revocation assertion: runner can read mode-000 directory"
            );
            return;
        }
        assert!(
            fs::read_dir(&mapped_root).is_err(),
            "permission-revocation fixture must fail read_dir before testing the cache"
        );

        let revoked = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );
        assert!(
            revoked.is_empty(),
            "revoked mapped root must not reuse a complete catalogue: {revoked:?}"
        );
        assert!(
            workspace.warnings().iter().any(|warning| {
                warning.contains("package shared")
                    && warning.contains("bounded source catalogue")
                    && warning.contains("incomplete")
            }),
            "revoked mapped root did not produce an actionable incomplete warning: {:?}",
            workspace.warnings()
        );

        let mut restored_permissions = fs::metadata(&mapped_root)
            .expect("mapped root metadata after revoke")
            .permissions();
        restored_permissions.set_mode(original_mode);
        fs::set_permissions(&mapped_root, restored_permissions)
            .expect("restore mapped root permissions");
        let restored = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );
        assert_eq!(
            restored.len(),
            1,
            "restored mapped root must rebuild a complete catalogue"
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_mapped_package_root_cache_revalidates_on_create_delete_and_restore() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let mapped_root = temp.path().join("restored-sdk");
        let (mut workspace, main_uri) = mapped_package_fixture(temp.path(), &mapped_root);

        let initially_missing = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );
        assert!(initially_missing.is_empty());

        fs::create_dir(&mapped_root).expect("create mapped root");
        let created = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );
        assert_eq!(created.len(), 1);

        fs::remove_dir(&mapped_root).expect("remove mapped root");
        let deleted = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );
        assert!(
            deleted.is_empty(),
            "deleted mapped root reused a stale complete catalogue: {deleted:?}"
        );
        assert!(
            workspace.warnings().iter().any(|warning| {
                warning.contains("package shared")
                    && warning.contains("bounded source catalogue")
                    && warning.contains("incomplete")
            }),
            "deleted mapped root did not remain incomplete: {:?}",
            workspace.warnings()
        );

        fs::create_dir(&mapped_root).expect("restore mapped root");
        let restored = workspace.navigate(
            &main_uri,
            Position::new(6, 2),
            NavigationTarget::Declaration,
        );
        assert_eq!(
            restored.len(),
            1,
            "restored empty mapped root should resolve only the unique workspace package: {restored:?}"
        );
    }

    #[test]
    fn mapped_package_catalogue_applies_default_exclusions() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let mapped_root = temp.path().join("mapped-sdk");
        let excluded = mapped_root.join(".git");
        fs::create_dir_all(&excluded).expect("excluded directory");
        fs::write(excluded.join("Shared.dpk"), "package Shared; end.\n")
            .expect("excluded package descriptor");

        let mut workspace = test_workspace(Vec::new(), WorkspaceOptions::default());
        let key = super::ContextKey {
            project_file: None,
            workspace_root: None,
            project_scope: None,
            selection_scope: None,
            selection_project: None,
            config: None,
            platform: None,
            overrides: EffectiveOverrides {
                path_mappings: vec![PathMapping {
                    from: "c:/sdk".to_owned(),
                    to: mapped_root.clone(),
                    config_file: temp.path().join("overrides.toml"),
                }],
                ..EffectiveOverrides::default()
            },
        };
        let requested_names = HashSet::from(["shared".to_owned()]);
        let catalogue = workspace
            .package_catalogue(&key, &mapped_root, &requested_names, None)
            .expect("package catalogue");

        assert!(
            !catalogue.entries.contains_key("shared"),
            "default-excluded directories must not contribute package descriptors"
        );
    }

    #[cfg(unix)]
    #[test]
    fn mapped_read_authorization_is_context_scoped_and_component_safe() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temporary directory");
        let mapped_a = temp.path().join("sdk-a");
        let mapped_b = temp.path().join("sdk-b");
        let sibling = temp.path().join("sdk-a-extra");
        let outside = temp.path().join("outside");
        let source_a = mapped_a.join("source/Provider.pas");
        let source_b = mapped_b.join("source/Provider.pas");
        let sibling_source = sibling.join("Provider.pas");
        let outside_source = outside.join("Provider.pas");
        fs::create_dir_all(source_a.parent().expect("source A directory"))
            .expect("source A directory");
        fs::create_dir_all(source_b.parent().expect("source B directory"))
            .expect("source B directory");
        fs::create_dir_all(&sibling).expect("sibling directory");
        fs::create_dir_all(&outside).expect("outside directory");
        fs::write(&source_a, "unit Provider; end.\n").expect("source A");
        fs::write(&source_b, "unit Provider; end.\n").expect("source B");
        fs::write(&sibling_source, "unit Provider; end.\n").expect("sibling source");
        fs::write(&outside_source, "unit Provider; end.\n").expect("outside source");
        symlink(&outside, mapped_a.join("escape")).expect("source escape symlink");

        let key_for = |mapped_root: PathBuf| super::ContextKey {
            project_file: None,
            workspace_root: None,
            project_scope: None,
            selection_scope: None,
            selection_project: None,
            config: None,
            platform: None,
            overrides: EffectiveOverrides {
                path_mappings: vec![PathMapping {
                    from: "c:/sdk".to_owned(),
                    to: mapped_root,
                    config_file: temp.path().join("overrides.toml"),
                }],
                ..EffectiveOverrides::default()
            },
        };
        let workspace = test_workspace(Vec::new(), WorkspaceOptions::default());
        let key_a = key_for(mapped_a.clone());
        let key_b = key_for(mapped_b);

        assert!(workspace.mapped_path_is_readable(&source_a, &key_a));
        assert!(workspace.mapped_path_is_readable(&source_b, &key_b));
        assert!(
            !workspace.mapped_path_is_readable(&source_b, &key_a),
            "another project's mapping must not authorize this source"
        );
        assert!(
            !workspace.mapped_path_is_readable(&sibling_source, &key_a),
            "prefix-similar sibling roots must not be authorized"
        );
        assert!(
            !workspace.mapped_path_is_readable(&mapped_a.join("escape/Provider.pas"), &key_a),
            "symlink escapes must not be authorized"
        );
        assert!(
            !workspace
                .mapped_path_is_readable(&temp.path().join("SDK-A/source/Provider.pas"), &key_a),
            "Linux containment must remain case-sensitive"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn mapped_root_resolution_prefers_exact_case_before_unique_fallback() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let lower = temp.path().join("sdk");
        let upper = temp.path().join("SDK");
        fs::create_dir(&lower).expect("lowercase directory");
        fs::create_dir(&upper).expect("uppercase directory");

        assert_eq!(super::resolve_case_insensitive_path(&lower), Some(lower));
        assert_eq!(
            super::resolve_case_insensitive_path(&temp.path().join("sDk")),
            None,
            "case-adjusted mapping roots must reject ambiguous fallback"
        );
    }

    #[cfg(windows)]
    #[test]
    fn mapped_read_authorization_is_case_insensitive_and_excludes_case_variants_on_windows() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let mapped_root = temp.path().join("sdk");
        let source = mapped_root.join("Source/Provider.pas");
        let case_variant = temp.path().join("SDK/source/provider.pas");
        let excluded = temp.path().join("SDK/.GIT/Shared.dpk");
        let custom_excluded = temp.path().join("SDK/CACHE/Shared.dpk");
        let workspace_root = temp.path().join("workspace");
        fs::create_dir_all(source.parent().expect("source directory")).expect("source directory");
        fs::create_dir_all(excluded.parent().expect("excluded directory"))
            .expect("excluded directory");
        fs::create_dir_all(custom_excluded.parent().expect("custom excluded directory"))
            .expect("custom excluded directory");
        fs::create_dir_all(&workspace_root).expect("workspace root");
        fs::write(&source, "unit Provider; end.\n").expect("source");
        fs::write(&excluded, "package Shared; end.\n").expect("excluded descriptor");
        fs::write(&custom_excluded, "package Shared; end.\n").expect("custom excluded descriptor");

        let key = super::ContextKey {
            project_file: None,
            workspace_root: Some(workspace_root.clone()),
            project_scope: None,
            selection_scope: None,
            selection_project: None,
            config: None,
            platform: None,
            overrides: EffectiveOverrides {
                path_mappings: vec![PathMapping {
                    from: "c:/sdk".to_owned(),
                    to: mapped_root,
                    config_file: temp.path().join("overrides.toml"),
                }],
                ..EffectiveOverrides::default()
            },
        };
        let workspace = test_workspace(
            vec![workspace_root],
            WorkspaceOptions {
                exclude: vec!["cache".to_owned()],
                ..WorkspaceOptions::default()
            },
        );

        assert!(workspace.mapped_path_is_readable(&case_variant, &key));
        assert!(!workspace.mapped_path_is_readable(&excluded, &key));
        assert!(!workspace.mapped_path_is_readable(&custom_excluded, &key));
    }
}
