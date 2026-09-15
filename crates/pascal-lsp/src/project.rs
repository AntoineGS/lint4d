//! Lazy, filesystem-only Delphi project context discovery.
//!
//! This module deliberately does not try to be an MSBuild evaluator. It reads
//! the small amount of project metadata needed by navigation, preserves
//! ambiguity, and reports anything it cannot safely interpret as a warning.

use pascal_core::delphi_overrides::{
    EffectiveOverrides, OverrideSession, PathMapping, ResolvedPath, user_config_path,
};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

const MAX_PROJECT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_IMPORT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_MAIN_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PACKAGE_METADATA_BYTES: u64 = 4 * 1024 * 1024;
const MAX_IMPORT_COUNT: usize = 64;
const MAX_METADATA_FILES: usize = MAX_IMPORT_COUNT + 1;
const MAX_EXPANDED_VALUE_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_PROPERTY_BYTES: usize = 16 * 1024 * 1024;
const MAX_OWNERSHIP_CANDIDATES: usize = 32;
const MAX_PROJECT_DIRECTORY_ENTRIES: usize = 10_000;
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProjectCandidateMembership {
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) readable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ProjectPathProvenance {
    LegacyNative,
    Configured,
    Mapped { root: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ProjectPathEntry {
    pub(crate) path: PathBuf,
    pub(crate) provenance: ProjectPathProvenance,
}

#[derive(Debug, Clone)]
struct ProvenanceRange {
    range: Range<usize>,
    provenance: ProjectPathProvenance,
}

impl ProjectPathEntry {
    pub(crate) fn legacy(path: PathBuf) -> Self {
        Self {
            path,
            provenance: ProjectPathProvenance::LegacyNative,
        }
    }

    fn resolved(path: PathBuf, resolved: &ResolvedPath, configured: bool) -> Self {
        let provenance = resolved.mapping.as_ref().map_or_else(
            || {
                if configured {
                    ProjectPathProvenance::Configured
                } else {
                    ProjectPathProvenance::LegacyNative
                }
            },
            |mapping| ProjectPathProvenance::Mapped {
                root: resolved_mapping_root(mapping),
            },
        );
        Self { path, provenance }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AuthorizedReadRoot {
    path: PathBuf,
    pattern_exclusion_bases: Vec<PathBuf>,
}

/// Immutable, requester-scoped authorization for project metadata and source
/// payloads.  The policy deliberately keeps configured native roots separate
/// from mapping provenance: a mapped entry may only use its selected mapping
/// root, while a configured native entry may use one of the requester's
/// workspace, source-path, or effective mapping destinations.
#[derive(Debug, Clone, Default)]
pub(crate) struct ReadPolicy {
    configured_roots: Vec<AuthorizedReadRoot>,
    mapped_roots: Vec<AuthorizedReadRoot>,
    exclusions: Vec<String>,
    exclusion_bases: Vec<PathBuf>,
    compiled_exclusions: Arc<Option<globset::GlobSet>>,
}

impl PartialEq for ReadPolicy {
    fn eq(&self, other: &Self) -> bool {
        self.configured_roots == other.configured_roots
            && self.mapped_roots == other.mapped_roots
            && self.exclusions == other.exclusions
            && self.exclusion_bases == other.exclusion_bases
    }
}

impl Eq for ReadPolicy {}

impl Hash for ReadPolicy {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.configured_roots.hash(state);
        self.mapped_roots.hash(state);
        self.exclusions.hash(state);
        self.exclusion_bases.hash(state);
    }
}

impl ReadPolicy {
    pub(crate) fn new(
        roots: &[PathBuf],
        source_paths: &[String],
        exclusions: &[String],
        overrides: &EffectiveOverrides,
    ) -> Self {
        let mut configured_roots = Vec::new();
        for root in roots {
            let root = absolute_lexical(root).unwrap_or_else(|_| root.to_path_buf());
            add_unique_path(&mut configured_roots, root.clone());
            for source in source_paths {
                let source = PathBuf::from(source);
                let source = if source.is_absolute() {
                    source
                } else {
                    root.join(source)
                };
                let source = absolute_lexical(&source).unwrap_or(source);
                add_unique_path(&mut configured_roots, source);
            }
        }
        for source in source_paths {
            let source = PathBuf::from(source);
            if source.is_absolute() {
                let source = absolute_lexical(&source).unwrap_or(source);
                add_unique_path(&mut configured_roots, source);
            }
        }

        let mut mapped_roots = Vec::new();
        for mapping in &overrides.path_mappings {
            add_unique_path(&mut mapped_roots, resolved_mapping_root(mapping));
        }

        let mut exclusion_bases = configured_roots.clone();
        exclusion_bases.extend(mapped_roots.iter().cloned());
        let mut unique_exclusion_bases = Vec::new();
        for base in exclusion_bases {
            add_unique_path(&mut unique_exclusion_bases, base);
        }
        let configured_roots = configured_roots
            .into_iter()
            .map(|path| AuthorizedReadRoot {
                path,
                pattern_exclusion_bases: unique_exclusion_bases.clone(),
            })
            .collect();
        let mapped_roots = mapped_roots
            .into_iter()
            .map(|path| AuthorizedReadRoot {
                path,
                pattern_exclusion_bases: unique_exclusion_bases.clone(),
            })
            .collect();

        Self {
            configured_roots,
            mapped_roots,
            exclusions: exclusions.to_vec(),
            exclusion_bases: unique_exclusion_bases,
            compiled_exclusions: Arc::new(compile_exclude_patterns(exclusions)),
        }
    }

    pub(crate) fn allows_entry(&self, entry: &ProjectPathEntry) -> bool {
        match &entry.provenance {
            ProjectPathProvenance::LegacyNative => {
                safe_regular_file(&entry.path) && !self.is_excluded(&entry.path)
            }
            ProjectPathProvenance::Configured => self
                .configured_roots
                .iter()
                .chain(self.mapped_roots.iter())
                .any(|root| {
                    self.allows_regular_file_under_root(&entry.path, &root.path)
                        && !self.is_excluded_for_root(&entry.path, root)
                }),
            ProjectPathProvenance::Mapped { root } => {
                !self.is_excluded_for_mapped_root(&entry.path, root)
                    && self.allows_regular_file_under_root(&entry.path, root)
            }
        }
    }

    pub(crate) fn allows_location(&self, entry: &ProjectPathEntry) -> bool {
        match &entry.provenance {
            ProjectPathProvenance::LegacyNative => {
                path_has_no_symlink_component(&entry.path) && !self.is_excluded(&entry.path)
            }
            ProjectPathProvenance::Configured => self
                .configured_roots
                .iter()
                .chain(self.mapped_roots.iter())
                .any(|root| {
                    self.allows_location_under_root(&entry.path, &root.path)
                        && !self.is_excluded_for_root(&entry.path, root)
                }),
            ProjectPathProvenance::Mapped { root } => {
                !self.is_excluded_for_mapped_root(&entry.path, root)
                    && self.allows_location_under_root(&entry.path, root)
            }
        }
    }

    pub(crate) fn entry_for_path(&self, path: &Path) -> Option<ProjectPathEntry> {
        self.mapped_roots
            .iter()
            .filter(|root| project_path_starts_with(path, &root.path))
            .max_by_key(|root| root.path.components().count())
            .map(|root| ProjectPathEntry {
                path: path.to_path_buf(),
                provenance: ProjectPathProvenance::Mapped {
                    root: root.path.clone(),
                },
            })
            .or_else(|| {
                self.configured_roots
                    .iter()
                    .filter(|root| project_path_starts_with(path, &root.path))
                    .max_by_key(|root| root.path.components().count())
                    .map(|_| ProjectPathEntry {
                        path: path.to_path_buf(),
                        provenance: ProjectPathProvenance::Configured,
                    })
            })
    }

    #[allow(dead_code)]
    pub(crate) fn allows_path(&self, path: &Path, provenance: &ProjectPathProvenance) -> bool {
        self.allows_location(&ProjectPathEntry {
            path: path.to_path_buf(),
            provenance: provenance.clone(),
        })
    }

    #[allow(dead_code)]
    pub(crate) fn identity(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }

    fn read_payload_with_observation(
        &self,
        entry: &ProjectPathEntry,
        limit: u64,
    ) -> Result<(String, MetadataObservation), String> {
        let stamp = crate::workspace::path_stamp_result(&entry.path)
            .ok()
            .flatten();
        let bytes = self.read_payload_bytes(entry, limit)?;
        let content_hash = crate::workspace::content_hash_bytes(&bytes);
        let observation = MetadataObservation::Payload {
            path: entry.path.clone(),
            read_policy: self.clone(),
            path_entry: entry.clone(),
            stamp,
            content_hash,
        };
        let contents =
            String::from_utf8(bytes).map_err(|error| format!("file is not UTF-8: {error}"))?;
        Ok((contents, observation))
    }

    pub(crate) fn read_payload_bytes(
        &self,
        entry: &ProjectPathEntry,
        limit: u64,
    ) -> Result<Vec<u8>, String> {
        if !self.allows_entry(entry) {
            return Err("payload path is not authorized".to_string());
        }
        self.read_payload_bytes_after_authorization(entry, limit, false)
    }

    pub(crate) fn allows_legacy_payload_entry(&self, entry: &ProjectPathEntry) -> bool {
        self.allows_legacy_route_entry(entry)
            && fs::metadata(&entry.path).is_ok_and(|metadata| metadata.is_file())
    }

    pub(crate) fn allows_legacy_route_entry(&self, entry: &ProjectPathEntry) -> bool {
        matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
            && !self.is_excluded(&entry.path)
    }

    pub(crate) fn read_legacy_payload_bytes(
        &self,
        entry: &ProjectPathEntry,
        limit: u64,
    ) -> Result<Vec<u8>, String> {
        if !self.allows_legacy_payload_entry(entry) {
            return Err("payload path is not authorized".to_string());
        }
        self.read_payload_bytes_after_authorization(entry, limit, true)
    }

    fn read_payload_bytes_after_authorization(
        &self,
        entry: &ProjectPathEntry,
        limit: u64,
        allow_legacy_symlink: bool,
    ) -> Result<Vec<u8>, String> {
        let metadata = fs::symlink_metadata(&entry.path)
            .map_err(|error| format!("could not inspect file: {error}"))?;
        if metadata.file_type().is_symlink() && !allow_legacy_symlink {
            return Err("path is not a regular file".to_string());
        }
        if !metadata.file_type().is_symlink() && !metadata.is_file() {
            return Err("path is not a regular file".to_string());
        }
        let file = open_payload_file(&entry.path)
            .map_err(|error| format!("could not open file: {error}"))?;
        let opened_metadata = file
            .metadata()
            .map_err(|error| format!("could not stat opened file: {error}"))?;
        if !opened_metadata.is_file() {
            return Err("opened path is not a regular file".to_string());
        }
        let mut bytes = Vec::new();
        file.take(limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| format!("could not read file: {error}"))?;
        if bytes.len() as u64 > limit {
            return Err(format!("file exceeds the {limit} byte safety limit"));
        }
        Ok(bytes)
    }

    fn allows_regular_file_under_root(&self, path: &Path, root: &Path) -> bool {
        self.allows_location_under_root(path, root) && safe_regular_file(path)
    }

    fn allows_location_under_root(&self, path: &Path, root: &Path) -> bool {
        project_path_starts_with(path, root) && path_has_no_symlink_component(path)
    }

    fn is_excluded(&self, path: &Path) -> bool {
        self.is_excluded_under_bases(path, &self.exclusion_bases)
    }

    fn is_excluded_for_root(&self, path: &Path, root: &AuthorizedReadRoot) -> bool {
        project_relative_path(path, &root.path)
            .is_some_and(|relative| relative.components().any(is_default_excluded_component))
            || self.matches_exclusion_patterns(path, &root.pattern_exclusion_bases)
    }

    fn is_excluded_for_mapped_root(&self, path: &Path, root: &Path) -> bool {
        project_relative_path(path, root)
            .is_some_and(|relative| relative.components().any(is_default_excluded_component))
            || self.matches_exclusion_patterns(path, &self.exclusion_bases)
    }

    fn is_excluded_under_bases(&self, path: &Path, bases: &[PathBuf]) -> bool {
        bases.iter().any(|base| {
            let Some(relative) = project_relative_path(path, base) else {
                return false;
            };
            if relative.components().any(is_default_excluded_component) {
                return true;
            }
            self.matches_exclusion_patterns_for_relative(&relative)
        })
    }

    fn matches_exclusion_patterns(&self, path: &Path, bases: &[PathBuf]) -> bool {
        bases.iter().any(|base| {
            project_relative_path(path, base)
                .is_some_and(|relative| self.matches_exclusion_patterns_for_relative(&relative))
        })
    }

    fn matches_exclusion_patterns_for_relative(&self, relative: &Path) -> bool {
        let Some(patterns) = self.compiled_exclusions.as_ref() else {
            return false;
        };
        let mut prefix = PathBuf::new();
        relative.components().any(|component| {
            prefix.push(component.as_os_str());
            patterns.is_match(prefix.to_string_lossy().replace('\\', "/"))
        })
    }
}

/// A metadata path observed during evaluation.  Stat-only observations are
/// retained for freshness and precedence, but never authorize a later
/// payload read.  Payload observations carry the exact policy and path entry
/// that authorized the original read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MetadataObservation {
    Stat {
        path: PathBuf,
    },
    Payload {
        path: PathBuf,
        read_policy: ReadPolicy,
        path_entry: ProjectPathEntry,
        stamp: Option<crate::workspace::PathStamp>,
        content_hash: u64,
    },
}

impl MetadataObservation {
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::Stat { path } | Self::Payload { path, .. } => path,
        }
    }
}

fn compile_exclude_patterns(patterns: &[String]) -> Option<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
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
        if let Ok(glob) = glob {
            builder.add(glob);
            valid_pattern_count += 1;
        }
    }
    (valid_pattern_count > 0)
        .then(|| builder.build().ok())
        .flatten()
}

fn project_relative_path(path: &Path, root: &Path) -> Option<PathBuf> {
    let path_components = path.components().collect::<Vec<_>>();
    let root_components = root.components().collect::<Vec<_>>();
    if path_components.len() < root_components.len()
        || !path_components
            .iter()
            .zip(root_components.iter())
            .all(|(path, root)| project_components_equal(*path, *root))
    {
        return None;
    }
    let mut relative = PathBuf::new();
    for component in &path_components[root_components.len()..] {
        relative.push(component.as_os_str());
    }
    Some(relative)
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

#[cfg(target_os = "linux")]
fn open_payload_file(path: &Path) -> io::Result<fs::File> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    // Linux's UAPI O_NONBLOCK prevents a replaced FIFO from blocking between
    // the stat-only check and the actual open.
    const O_NONBLOCK: i32 = 0o4000;
    OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(path)
}

#[cfg(not(target_os = "linux"))]
fn open_payload_file(path: &Path) -> io::Result<fs::File> {
    fs::File::open(path)
}

#[derive(Debug, Default)]
struct ProjectDirectoryEntries {
    dproj: Vec<PathBuf>,
    dpr_or_dpk: Vec<PathBuf>,
    candidate_overflow: bool,
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
    /// The same search paths tagged with their source provenance. This keeps
    /// legacy native roots distinct from configured and mapped roots.
    pub(crate) search_path_entries: Vec<ProjectPathEntry>,
    /// MainSource and explicit references retain the provenance of the path
    /// expression that produced them so membership cannot bypass mapped-root
    /// safety checks.
    pub(crate) main_source_entry: Option<ProjectPathEntry>,
    pub(crate) explicit_unit_entries: HashMap<String, Vec<ProjectPathEntry>>,
    /// Ordered project-relative include paths from DCC_IncludePath.
    ///
    /// Include paths are kept separate from unit search paths because include
    /// lookup must not make an arbitrary directory a Pascal unit candidate.
    pub include_paths: Vec<PathBuf>,
    /// Include paths retain their source provenance for read authorization.
    pub(crate) include_path_entries: Vec<ProjectPathEntry>,
    pub explicit_units: HashMap<String, Vec<PathBuf>>,
    pub unit_namespaces: Vec<String>,
    pub unit_aliases: HashMap<String, String>,
    pub defines: Vec<String>,
    pub config: Option<String>,
    pub platform: Option<String>,
    /// The immutable Delphi override snapshot used to evaluate this context.
    pub overrides: EffectiveOverrides,
    /// The requester-scoped read policy used while evaluating this context.
    pub(crate) read_policy: ReadPolicy,
    /// Ordered, case-insensitively unique package names from DCC_UsePackage.
    /// Package exports are resolved lazily and are not merged into the unit
    /// search paths or the project-wide unit index.
    pub packages: Vec<String>,
    /// Project, main-source, imported option-set, and automatic-selection
    /// candidate files used to build the context. Consumers can revalidate
    /// these paths without rediscovering or reparsing unrelated source files.
    pub metadata_files: Vec<PathBuf>,
    /// The authorization provenance for each metadata observation. A path in
    /// `metadata_files` without a payload observation is stat-only.
    pub(crate) metadata_observations: Vec<MetadataObservation>,
    pub warnings: Vec<String>,
    pub(crate) override_error: Option<String>,
}

/// One file observation captured at the read which supplied bytes to project
/// discovery. Consumers use this instead of reopening the file after parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectReadObservation {
    pub(crate) path: PathBuf,
    pub(crate) stamp: ProjectReadStamp,
    pub(crate) content_hash: u64,
    pub(crate) content_bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectReadStamp {
    pub(crate) bytes: u64,
    pub(crate) modified: Option<SystemTime>,
    pub(crate) is_dir: bool,
    pub(crate) is_symlink: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ProjectDiscovery {
    pub(crate) context: ProjectContext,
    pub(crate) observations: Vec<ProjectReadObservation>,
    pub(crate) candidate_memberships: HashMap<PathBuf, Result<ProjectCandidateMembership, String>>,
}

#[derive(Debug, Default)]
struct ProjectReadTracker {
    observations: Vec<ProjectReadObservation>,
    candidate_memberships: HashMap<PathBuf, Result<ProjectCandidateMembership, String>>,
}

impl ProjectReadTracker {
    fn record(&mut self, path: &Path, stamp: ProjectReadStamp, bytes: &[u8]) {
        if self
            .observations
            .iter()
            .any(|observation| project_paths_equal(&observation.path, path))
        {
            return;
        }
        self.observations.push(ProjectReadObservation {
            path: path.to_path_buf(),
            stamp,
            content_hash: project_content_hash(bytes),
            content_bytes: (!is_pascal_source_path(path)).then(|| bytes.to_vec()),
        });
    }

    fn record_candidate_membership(
        &mut self,
        path: PathBuf,
        membership: Result<ProjectCandidateMembership, String>,
    ) {
        if self
            .candidate_memberships
            .keys()
            .any(|existing| project_paths_equal(existing, &path))
        {
            return;
        }
        self.candidate_memberships.insert(path, membership);
    }

    fn into_discovery(self, context: ProjectContext) -> ProjectDiscovery {
        ProjectDiscovery {
            context,
            observations: self.observations,
            candidate_memberships: self.candidate_memberships,
        }
    }
}

/// Metadata extracted from one source package descriptor. The descriptor is
/// parsed only after a project import has failed the ordinary unit lookup;
/// `metadata_files` contains every project/option-set dependency used to
/// produce the result so callers can detect changes without file events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PackageMetadata {
    pub units: HashMap<String, Vec<PathBuf>>,
    pub unit_entries: HashMap<String, Vec<ProjectPathEntry>>,
    pub warnings: Vec<String>,
    pub metadata_files: Vec<PathBuf>,
    pub metadata_observations: Vec<MetadataObservation>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct PackageMetadataRead {
    pub(crate) metadata: PackageMetadata,
    pub(crate) observations: Vec<ProjectReadObservation>,
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
        )
        .map(|discovery| discovery.context)
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

fn production_override_session() -> (OverrideSession, Vec<String>) {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match user_config_path(xdg.as_deref(), home.as_deref()) {
        Ok(path) => (OverrideSession::new(Some(path)), Vec::new()),
        Err(error) => (OverrideSession::new(None), vec![error]),
    }
}

pub(crate) fn project_candidates(
    file: &Path,
    workspace_roots: &[PathBuf],
) -> Result<ProjectCandidates, String> {
    project_candidates_with_cancel(file, workspace_roots, None)
}

pub(crate) fn project_candidates_with_cancel(
    file: &Path,
    workspace_roots: &[PathBuf],
    cancel: Option<&AtomicBool>,
) -> Result<ProjectCandidates, String> {
    check_project_scan_cancel(cancel)?;
    let mut warnings = Vec::new();
    let absolute_file = absolute_lexical(file)?;
    let file_path = discovery_file_path(&absolute_file, &mut warnings);
    let roots = normalize_workspace_roots(workspace_roots, &mut warnings)?;
    let relevant_root = relevant_workspace_root(&file_path, &roots);
    find_project_candidates(&file_path, relevant_root.as_deref(), cancel)
}

pub(crate) fn discover_with_selections(
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
    )
}

pub(crate) fn discover_with_selections_and_observations_with_overrides(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
    selections: &ProjectSelections,
    overrides: &OverrideSession,
    exclusions: &[String],
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
    )
}

pub(crate) fn discover_with_selections_and_observations_with_cancel_and_overrides(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
    selections: &ProjectSelections,
    overrides: &OverrideSession,
    exclusions: &[String],
    cancel: &AtomicBool,
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
    )
    .map(|discovery| discovery.context)
}

// Discovery orchestration keeps the immutable request inputs, observation
// tracker, and cancellation token explicit at this boundary.
#[allow(clippy::too_many_arguments)]
fn discover_context_with_selections(
    file: &Path,
    workspace_roots: &[PathBuf],
    options: &ProjectOptions,
    selections: &ProjectSelections,
    overrides: &OverrideSession,
    mut warnings: Vec<String>,
    exclusions: &[String],
    cancel: Option<&AtomicBool>,
) -> Result<ProjectDiscovery, String> {
    check_project_scan_cancel(cancel)?;
    let mut read_tracker = ProjectReadTracker::default();
    let absolute_file = absolute_lexical(file)?;
    let file_path = discovery_file_path(&absolute_file, &mut warnings);
    let roots = normalize_workspace_roots(workspace_roots, &mut warnings)?;
    let relevant_root = relevant_workspace_root(&file_path, &roots);
    record_candidate_memberships(
        &file_path,
        relevant_root.as_deref(),
        &mut read_tracker,
        cancel,
    )?;

    let runtime_selection = if selections.is_empty() {
        None
    } else {
        let candidates = match find_project_candidates(&file_path, relevant_root.as_deref(), cancel)
        {
            Ok(candidates) => candidates,
            Err(error) => {
                warnings.push(error);
                let context = build_standalone_context_with_overrides(
                    &file_path,
                    &roots,
                    options,
                    warnings,
                    false,
                    Vec::new(),
                    Vec::new(),
                    overrides,
                    exclusions,
                )?;
                return Ok(read_tracker.into_discovery(context));
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
            let context = build_standalone_context_with_overrides(
                &file_path,
                &roots,
                options,
                warnings,
                false,
                candidates,
                Vec::new(),
                overrides,
                exclusions,
            )?;
            return Ok(read_tracker.into_discovery(context));
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
            options,
            overrides,
            &mut warnings,
            &mut read_tracker,
            cancel,
            exclusions,
        )
    };
    check_project_scan_cancel(cancel)?;

    match selected_project {
        ProjectSelection::Selected {
            path: project_file,
            explicit,
            metadata_files,
            metadata_observations,
        } => {
            let effective_overrides =
                match effective_overrides_for_project(&project_file, &roots, overrides) {
                    Ok(overrides) => overrides,
                    Err(error) => {
                        warnings.push(error.clone());
                        let mut context = build_project_context(
                            project_file,
                            &file_path,
                            &roots,
                            options,
                            EffectiveOverrides::default(),
                            warnings,
                            explicit,
                            metadata_files,
                            metadata_observations.clone(),
                            exclusions,
                            &mut read_tracker,
                            cancel,
                        )?;
                        context.discovery_complete = false;
                        context.override_error = Some(error);
                        return Ok(read_tracker.into_discovery(context));
                    }
                };
            let context = build_project_context(
                project_file,
                &file_path,
                &roots,
                options,
                effective_overrides,
                warnings,
                explicit,
                metadata_files,
                metadata_observations,
                exclusions,
                &mut read_tracker,
                cancel,
            )?;
            Ok(read_tracker.into_discovery(context))
        }
        ProjectSelection::Standalone {
            metadata_files,
            metadata_observations,
        } => {
            let context = build_standalone_context_with_overrides(
                &file_path,
                &roots,
                options,
                warnings,
                true,
                metadata_files,
                metadata_observations,
                overrides,
                exclusions,
            )?;
            Ok(read_tracker.into_discovery(context))
        }
        ProjectSelection::Incomplete {
            metadata_files,
            metadata_observations,
            override_error,
        } => {
            let mut context = build_standalone_context_with_overrides(
                &file_path,
                &roots,
                options,
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
            Ok(read_tracker.into_discovery(context))
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
    selected_project_is_current_with_cancel(scope, selected, None)
}

pub(crate) fn selected_project_is_current_with_cancel(
    scope: &Path,
    selected: &Path,
    cancel: Option<&AtomicBool>,
) -> Result<bool, String> {
    let entries = project_directory_entries(scope, cancel)?;
    Ok(entries
        .dproj
        .iter()
        .any(|candidate| project_paths_equal(candidate, selected)))
}

fn find_project_candidates(
    file: &Path,
    workspace_root: Option<&Path>,
    cancel: Option<&AtomicBool>,
) -> Result<ProjectCandidates, String> {
    let mut directory = file.parent().map_or_else(PathBuf::new, Path::to_path_buf);
    loop {
        check_project_scan_cancel(cancel)?;
        let entries = project_directory_entries(&directory, cancel)?;
        if !entries.dproj.is_empty() {
            let mut files = entries.dproj;
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

fn record_candidate_memberships(
    file: &Path,
    boundary: Option<&Path>,
    tracker: &mut ProjectReadTracker,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    let mut directory = file.parent().map_or_else(PathBuf::new, Path::to_path_buf);
    loop {
        check_project_scan_cancel(cancel)?;
        let membership = project_candidate_membership(&directory, cancel);
        if matches!(&membership, Err(error) if error == "request cancelled") {
            return Err("request cancelled".to_string());
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

pub(crate) fn project_candidate_membership(
    directory: &Path,
    cancel: Option<&AtomicBool>,
) -> Result<ProjectCandidateMembership, String> {
    let entries = project_directory_entries(directory, cancel)?;
    if entries.candidate_overflow {
        return Err(format!(
            "project candidate membership limit ({MAX_OWNERSHIP_CANDIDATES}) reached in {}",
            directory.display()
        ));
    }
    let mut dproj = entries.dproj;
    let mut dpr_or_dpk = entries.dpr_or_dpk;
    dproj.append(&mut dpr_or_dpk);
    dproj.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
    Ok(ProjectCandidateMembership {
        paths: dproj,
        readable: true,
    })
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

fn project_directory_entries(
    directory: &Path,
    cancel: Option<&AtomicBool>,
) -> Result<ProjectDirectoryEntries, String> {
    check_project_scan_cancel(cancel)?;
    let mut entries = fs::read_dir(directory).map_err(|error| {
        format!(
            "could not inspect project directory {}: {error}",
            directory.display()
        )
    })?;
    let mut result = ProjectDirectoryEntries::default();
    let mut visited_entries = 0usize;
    loop {
        check_project_scan_cancel(cancel)?;
        let Some(entry) = entries.next() else {
            break;
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
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "could not inspect project directory entry {}: {error}",
                path.display()
            )
        })?;
        if !file_type.is_file() {
            continue;
        }
        if extension_is(&path, "dproj") {
            push_bounded_candidate(&mut result.dproj, path);
            result.candidate_overflow |= result.dproj.len() > MAX_OWNERSHIP_CANDIDATES;
        } else if extension_is(&path, "dpr") || extension_is(&path, "dpk") {
            push_bounded_candidate(&mut result.dpr_or_dpk, path);
            result.candidate_overflow |= result.dpr_or_dpk.len() > MAX_OWNERSHIP_CANDIDATES;
        }
    }
    Ok(result)
}

fn check_project_scan_cancel(cancel: Option<&AtomicBool>) -> Result<(), String> {
    #[cfg(test)]
    let force_cancel = cancel.is_some()
        && TEST_PROJECT_SCAN_CANCEL_AFTER_CHECKS.with(|budget| match budget.get() {
            Some(0) => {
                budget.set(None);
                true
            }
            Some(remaining) => {
                budget.set(Some(remaining.saturating_sub(1)));
                false
            }
            None => false,
        });
    #[cfg(test)]
    if force_cancel {
        if let Some(cancel) = cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }
    if cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
        Err("request cancelled".to_string())
    } else {
        Ok(())
    }
}

#[cfg(test)]
type ProjectReadHook = Box<dyn FnOnce(&Path)>;

#[cfg(test)]
thread_local! {
    static TEST_PROJECT_SCAN_CANCEL_AFTER_CHECKS: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
    static TEST_AFTER_PROJECT_READ:
        std::cell::RefCell<Option<ProjectReadHook>> = std::cell::RefCell::new(None);
    static TEST_AFTER_PROJECT_READ_AT:
        std::cell::RefCell<Option<(PathBuf, ProjectReadHook)>> = std::cell::RefCell::new(None);
}

#[cfg(test)]
pub(crate) struct TestProjectScanCancellationGuard(Option<usize>);

#[cfg(test)]
pub(crate) fn test_cancel_project_scan_after_checks(
    checks: usize,
) -> TestProjectScanCancellationGuard {
    let previous = TEST_PROJECT_SCAN_CANCEL_AFTER_CHECKS.with(|budget| {
        let previous = budget.get();
        budget.set(Some(checks));
        previous
    });
    TestProjectScanCancellationGuard(previous)
}

#[cfg(test)]
impl Drop for TestProjectScanCancellationGuard {
    fn drop(&mut self) {
        TEST_PROJECT_SCAN_CANCEL_AFTER_CHECKS.with(|budget| budget.set(self.0.take()));
    }
}

#[cfg(test)]
pub(crate) struct TestAfterProjectReadGuard(Option<ProjectReadHook>);

#[cfg(test)]
pub(crate) fn test_after_project_read(
    hook: impl FnOnce(&Path) + 'static,
) -> TestAfterProjectReadGuard {
    let previous = TEST_AFTER_PROJECT_READ.with(|slot| slot.borrow_mut().replace(Box::new(hook)));
    TestAfterProjectReadGuard(previous)
}

#[cfg(test)]
impl Drop for TestAfterProjectReadGuard {
    fn drop(&mut self) {
        TEST_AFTER_PROJECT_READ.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

#[cfg(test)]
pub(crate) struct TestAfterProjectReadAtGuard(Option<(PathBuf, ProjectReadHook)>);

#[cfg(test)]
pub(crate) fn test_after_project_read_at(
    path: PathBuf,
    hook: impl FnOnce(&Path) + 'static,
) -> TestAfterProjectReadAtGuard {
    let previous =
        TEST_AFTER_PROJECT_READ_AT.with(|slot| slot.borrow_mut().replace((path, Box::new(hook))));
    TestAfterProjectReadAtGuard(previous)
}

#[cfg(test)]
impl Drop for TestAfterProjectReadAtGuard {
    fn drop(&mut self) {
        TEST_AFTER_PROJECT_READ_AT.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

#[cfg(test)]
fn run_after_project_read(path: &Path) {
    let hook = TEST_AFTER_PROJECT_READ.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook(path);
    }
    let targeted_hook = TEST_AFTER_PROJECT_READ_AT.with(|slot| {
        let matches = slot
            .borrow()
            .as_ref()
            .is_some_and(|(target, _)| target == path);
        matches.then(|| slot.borrow_mut().take()).flatten()
    });
    if let Some((_, hook)) = targeted_hook {
        hook(path);
    }
}

#[cfg(not(test))]
fn run_after_project_read(_path: &Path) {}

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
    tracker: &mut ProjectReadTracker,
    cancel: Option<&AtomicBool>,
    exclusions: &[String],
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
        let entries = match project_directory_entries(&directory, cancel) {
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
        let dproj = entries.dproj;
        let dpr_or_dpk = entries.dpr_or_dpk;

        if !dproj.is_empty() {
            return choose_project_candidate(
                dproj,
                &directory,
                "project files",
                file,
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

fn push_bounded_candidate(candidates: &mut Vec<PathBuf>, path: PathBuf) {
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
    roots: &[PathBuf],
    options: &ProjectOptions,
    overrides: &OverrideSession,
    warnings: &mut Vec<String>,
    tracker: &mut ProjectReadTracker,
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

    if owned.len() == 1 && !incomplete && !budget.exhausted {
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
    tracker: &mut ProjectReadTracker,
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
    match build_project_context(
        project_file.to_path_buf(),
        file,
        roots,
        options,
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
    tracker: &mut ProjectReadTracker,
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
    mut tracker: Option<&mut ProjectReadTracker>,
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
        if let Some(tracker) = tracker.as_deref_mut() {
            if let Ok(stamp) = project_read_stamp(source_path) {
                tracker.record(source_path, stamp, contents.as_bytes());
            }
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

fn standalone_overrides(
    file: &Path,
    roots: &[PathBuf],
    session: &OverrideSession,
) -> Result<EffectiveOverrides, String> {
    let workspace_root = relevant_override_workspace_root(file, roots);
    session.effective_for(workspace_root.as_deref(), None)
}

#[allow(clippy::too_many_arguments)]
fn build_standalone_context_with_overrides(
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
    let (effective_overrides, override_error) = match standalone_overrides(file, roots, session) {
        Ok(overrides) => (overrides, None),
        Err(error) => {
            warnings.push(error.clone());
            discovery_complete = false;
            (EffectiveOverrides::default(), Some(error))
        }
    };
    let mut context = build_standalone_context(
        file,
        roots,
        options,
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

fn effective_overrides_for_project(
    project_file: &Path,
    roots: &[PathBuf],
    session: &OverrideSession,
) -> Result<EffectiveOverrides, String> {
    let project_directory = project_file.parent();
    let workspace_root =
        project_directory.and_then(|directory| relevant_override_workspace_root(directory, roots));
    session.effective_for(workspace_root.as_deref(), project_directory)
}

fn relevant_override_workspace_root(file: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    roots
        .iter()
        .filter(|root| file.starts_with(root))
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

// Context construction keeps the selected project, requesting file, roots,
// evaluator options, and immutable effective settings explicit for callers.
#[allow(clippy::too_many_arguments)]
fn build_project_context(
    project_file: PathBuf,
    file: &Path,
    roots: &[PathBuf],
    options: &ProjectOptions,
    overrides: EffectiveOverrides,
    warnings: Vec<String>,
    explicit: bool,
    consulted_metadata_files: Vec<PathBuf>,
    consulted_metadata_observations: Vec<MetadataObservation>,
    exclusions: &[String],
    tracker: &mut ProjectReadTracker,
    cancel: Option<&AtomicBool>,
) -> Result<ProjectContext, String> {
    check_project_scan_cancel(cancel)?;
    let project_dir = project_file
        .parent()
        .map_or_else(PathBuf::new, Path::to_path_buf);
    let read_policy = ReadPolicy::new(roots, &options.source_paths, exclusions, &overrides);
    let mut builder = ProjectBuilder::new(
        options,
        &overrides,
        warnings,
        project_dir.clone(),
        read_policy.clone(),
    );
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
            None => fallback_main_source(&project_file, &mut builder.warnings)
                .map(ProjectPathEntry::legacy),
        }
    } else {
        Some(ProjectPathEntry::legacy(project_file.clone()))
    };
    let main_source = main_source_entry.as_ref().map(|entry| entry.path.clone());
    if project_is_dproj && main_source.is_none() {
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
    if let Some(main_source) = &main_source_entry {
        if let Some(observation) = add_explicit_units_from_source(
            main_source,
            &mut explicit_units,
            &mut explicit_unit_entries,
            &mut builder.warnings,
            &builder.overrides,
            &builder.read_policy,
            tracker,
        ) {
            add_metadata_observation(&mut builder.metadata_observations, observation);
        }
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
        let Some(entry) = resolve_project_path_entry(
            &expanded.value,
            &builder.project_dir,
            &builder.overrides,
            &mut builder.warnings,
            "DCCReference",
            true,
            expanded.explicit_dependency,
        ) else {
            continue;
        };
        let provenance = expanded_path_provenance(&expanded, &reference.source_provenance);
        let entry = inherit_path_provenance(entry, &provenance);
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
    if let Some(main_source) = &main_source {
        if !metadata_files.iter().any(|path| path == main_source) {
            metadata_files.push(main_source.clone());
        }
    }
    for metadata_file in consulted_metadata_files {
        check_project_scan_cancel(cancel)?;
        if !metadata_files.iter().any(|path| path == &metadata_file) {
            metadata_files.push(metadata_file);
        }
    }
    let metadata_observations = complete_metadata_observations(
        &metadata_files,
        builder
            .metadata_observations
            .clone()
            .into_iter()
            .chain(consulted_metadata_observations)
            .collect(),
    );

    Ok(ProjectContext {
        discovery_complete: !builder.incomplete
            && !project_context_warnings_incomplete(&builder.warnings, explicit),
        project_file: Some(project_file),
        main_source,
        search_paths,
        search_path_entries,
        main_source_entry,
        explicit_unit_entries,
        include_paths,
        include_path_entries,
        explicit_units,
        unit_namespaces: property_list(&builder, "dcc_namespace"),
        unit_aliases: parse_aliases(builder.property("dcc_unitalias").as_deref()),
        defines: property_list(&builder, "dcc_define"),
        config: selected_config(&builder, options),
        platform: selected_platform(&builder, options),
        overrides,
        read_policy,
        packages: package_list(&builder),
        metadata_files,
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
    if let Some(parent) = file.parent() {
        if let Some(actual) = resolve_existing_path(parent, &mut warnings, "source directory") {
            add_unique_project_path_entry(
                &mut search_path_entries,
                ProjectPathEntry::legacy(actual),
            );
        }
    }
    if let Some(root) = relevant_workspace_root(file, roots) {
        if root.is_dir() {
            add_unique_project_path_entry(&mut search_path_entries, ProjectPathEntry::legacy(root));
        }
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
    let metadata_observations =
        complete_metadata_observations(&metadata_files, metadata_observations);

    Ok(ProjectContext {
        discovery_complete,
        project_file: None,
        main_source: None,
        search_paths,
        search_path_entries,
        main_source_entry: None,
        explicit_unit_entries: HashMap::new(),
        include_paths: Vec::new(),
        include_path_entries: Vec::new(),
        explicit_units: HashMap::new(),
        unit_namespaces: Vec::new(),
        unit_aliases: HashMap::new(),
        defines: Vec::new(),
        config: selected_standalone_property(&overrides, options.build_config.as_ref(), "config"),
        platform: selected_standalone_property(&overrides, options.platform.as_ref(), "platform"),
        overrides,
        read_policy,
        packages: Vec::new(),
        metadata_files,
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

fn selected_config(builder: &ProjectBuilder, options: &ProjectOptions) -> Option<String> {
    builder
        .property("config")
        .filter(|value| !value.is_empty() && !value.contains(UNRESOLVED_MARKER))
        .or_else(|| options.build_config.clone())
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

fn full_range(value: &str) -> Vec<Range<usize>> {
    (!value.is_empty())
        .then_some(0..value.len())
        .into_iter()
        .collect()
}

fn trim_ranges(
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

fn trim_provenance_ranges(
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

fn property_list_item(
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

fn add_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn add_unique_project_path_entry(entries: &mut Vec<ProjectPathEntry>, entry: ProjectPathEntry) {
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

pub(crate) fn add_metadata_observation(
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

fn complete_metadata_observations(
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
    tracker: &mut ProjectReadTracker,
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

fn inherit_path_provenance(
    mut entry: ProjectPathEntry,
    parent: &ProjectPathProvenance,
) -> ProjectPathEntry {
    if matches!(entry.provenance, ProjectPathProvenance::LegacyNative) {
        entry.provenance = parent.clone();
    }
    entry
}

fn combine_path_provenance(
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

fn expanded_path_provenance(
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

fn resolve_project_path_entry(
    raw: &str,
    base: &Path,
    overrides: &EffectiveOverrides,
    warnings: &mut Vec<String>,
    kind: &str,
    warn_missing: bool,
    configured: bool,
) -> Option<ProjectPathEntry> {
    let resolved = project_path_candidate(raw, base, overrides, warnings, kind)?;
    let candidate = lexical_normalize(&resolved.path);
    match resolve_existing_path_status_with_provenance(&candidate, &resolved, raw, warnings, kind) {
        ExistingPathStatus::Found(path) => {
            Some(ProjectPathEntry::resolved(path, &resolved, configured))
        }
        ExistingPathStatus::Missing if warn_missing => {
            warnings.push(missing_path_warning(kind, raw, &candidate, &resolved));
            None
        }
        ExistingPathStatus::Missing | ExistingPathStatus::Unresolvable => None,
    }
}

fn safe_regular_file(path: &Path) -> bool {
    if filesystem_identity_unverified(path) {
        return false;
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    metadata.is_file() && !metadata.file_type().is_symlink()
}

fn path_has_no_symlink_component(path: &Path) -> bool {
    path.ancestors().all(|ancestor| {
        fs::symlink_metadata(ancestor).map_or(true, |metadata| !metadata.file_type().is_symlink())
    })
}

fn project_path_candidate(
    raw: &str,
    base: &Path,
    overrides: &EffectiveOverrides,
    warnings: &mut Vec<String>,
    kind: &str,
) -> Option<ResolvedPath> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains(UNRESOLVED_MARKER) {
        return None;
    }
    let normalized = normalize_delphi_separators(raw);
    match overrides.resolve_path(&normalized, base) {
        Ok(resolved) => Some(resolved),
        Err(error) if error == format!("Windows path is unavailable on Linux: {normalized}") => {
            warnings.push(format!(
                "Windows path in {kind} is unavailable on Linux and was omitted: {raw}"
            ));
            None
        }
        Err(error) => {
            let error = restore_original_path_in_error(&error, &normalized, raw);
            let provenance = matching_path_mapping(raw, overrides)
                .map(|mapping| mapping_provenance_suffix(raw, mapping))
                .unwrap_or_default();
            warnings.push(format!("{kind}: {error}{provenance}"));
            None
        }
    }
}

fn normalize_delphi_separators(path: &str) -> String {
    path.replace('\\', "/")
}

fn restore_original_path_in_error(error: &str, normalized: &str, raw: &str) -> String {
    error
        .strip_suffix(normalized)
        .map_or_else(|| error.to_owned(), |prefix| format!("{prefix}{raw}"))
}

fn matching_path_mapping<'a>(
    raw: &str,
    overrides: &'a EffectiveOverrides,
) -> Option<&'a PathMapping> {
    let normalized = normalize_delphi_separators(raw).to_ascii_lowercase();
    overrides
        .path_mappings
        .iter()
        .filter(|mapping| {
            normalized == mapping.from || normalized.starts_with(&format!("{}/", mapping.from))
        })
        .max_by_key(|mapping| mapping.from.len())
}

fn mapping_provenance_suffix(raw: &str, mapping: &PathMapping) -> String {
    format!(
        " (from {raw}; mapping {} -> {})",
        mapping.config_file.display(),
        mapping.to.display(),
    )
}

fn missing_path_warning(
    kind: &str,
    raw: &str,
    candidate: &Path,
    resolved: &ResolvedPath,
) -> String {
    match resolved.mapping.as_ref() {
        Some(mapping) => format!(
            "{kind} path does not exist and was omitted: {} (from {raw}; mapping {} -> {})",
            candidate.display(),
            mapping.config_file.display(),
            mapping.to.display(),
        ),
        None => format!(
            "{kind} path does not exist and was omitted: {}",
            candidate.display()
        ),
    }
}

fn search_path_missing_warning(
    kind: &str,
    raw: &str,
    candidate: &Path,
    resolved: &ResolvedPath,
) -> String {
    match resolved.mapping.as_ref() {
        Some(mapping) => format!(
            "{kind} path does not exist yet; retaining it for lazy discovery: {} (from {raw}; mapping {} -> {})",
            candidate.display(),
            mapping.config_file.display(),
            mapping.to.display(),
        ),
        None => format!(
            "{kind} path does not exist yet; retaining it for lazy discovery: {}",
            candidate.display()
        ),
    }
}

fn resolve_existing_path_status_with_provenance(
    path: &Path,
    resolved: &ResolvedPath,
    raw: &str,
    warnings: &mut Vec<String>,
    kind: &str,
) -> ExistingPathStatus {
    let warning_start = warnings.len();
    let status = resolve_existing_path_status(path, warnings, kind);
    if matches!(status, ExistingPathStatus::Unresolvable) {
        if let Some(mapping) = resolved.mapping.as_ref() {
            let provenance = mapping_provenance_suffix(raw, mapping);
            for warning in warnings.iter_mut().skip(warning_start) {
                if !warning.ends_with(&provenance) {
                    warning.push_str(&provenance);
                }
            }
        }
    }
    status
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

fn resolved_mapping_root(mapping: &PathMapping) -> PathBuf {
    let candidate = lexical_normalize(&mapping.to);
    let mut warnings = Vec::new();
    match resolve_existing_path_status(&candidate, &mut warnings, "mapped path root") {
        ExistingPathStatus::Found(path) => path,
        ExistingPathStatus::Missing | ExistingPathStatus::Unresolvable => candidate,
    }
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

#[derive(Debug)]
#[allow(dead_code)]
struct BoundedRead {
    stamp: ProjectReadStamp,
    bytes: Vec<u8>,
}

#[allow(dead_code)]
fn read_bounded_with_tracker(
    path: &Path,
    limit: u64,
    tracker: &mut ProjectReadTracker,
) -> Result<String, String> {
    let read = read_bounded_bytes(path, limit)?;
    let text =
        String::from_utf8(read.bytes).map_err(|error| format!("file is not UTF-8: {error}"))?;
    tracker.record(path, read.stamp, text.as_bytes());
    Ok(text)
}

#[allow(dead_code)]
fn read_bounded_bytes(path: &Path, limit: u64) -> Result<BoundedRead, String> {
    let stamp = project_read_stamp(path)?;
    if stamp.bytes > limit {
        return Err(format!(
            "file is {} bytes, exceeding the {} byte safety limit",
            stamp.bytes, limit
        ));
    }
    let bytes = fs::read(path).map_err(|error| format!("could not read file: {error}"))?;
    run_after_project_read(path);
    Ok(BoundedRead { stamp, bytes })
}

fn read_payload_with_tracker(
    read_policy: &ReadPolicy,
    entry: &ProjectPathEntry,
    limit: u64,
    tracker: &mut ProjectReadTracker,
) -> Result<(String, MetadataObservation), String> {
    if !read_policy.allows_entry(entry) {
        return Err("payload path is not authorized".to_string());
    }
    let stamp = project_read_stamp(&entry.path)?;
    let bytes = read_policy.read_payload_bytes(entry, limit)?;
    run_after_project_read(&entry.path);
    tracker.record(&entry.path, stamp, &bytes);
    let observation = MetadataObservation::Payload {
        path: entry.path.clone(),
        read_policy: read_policy.clone(),
        path_entry: entry.clone(),
        stamp: crate::workspace::path_stamp_result(&entry.path)
            .ok()
            .flatten(),
        content_hash: project_content_hash(&bytes),
    };
    let contents =
        String::from_utf8(bytes).map_err(|error| format!("file is not UTF-8: {error}"))?;
    Ok((contents, observation))
}

fn project_read_stamp(path: &Path) -> Result<ProjectReadStamp, String> {
    let link_metadata =
        fs::symlink_metadata(path).map_err(|error| format!("could not stat file: {error}"))?;
    let is_symlink = link_metadata.file_type().is_symlink();
    let metadata = if is_symlink {
        fs::metadata(path).map_err(|error| format!("could not stat file: {error}"))?
    } else {
        link_metadata
    };
    Ok(ProjectReadStamp {
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
        is_dir: metadata.is_dir(),
        is_symlink,
    })
}

fn project_content_hash(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write(bytes);
    hasher.write_usize(bytes.len());
    hasher.finish()
}

fn is_pascal_source_path(path: &Path) -> bool {
    path.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("pas")
            || extension.eq_ignore_ascii_case("dpr")
            || extension.eq_ignore_ascii_case("dpk")
    })
}

#[allow(dead_code)]
pub(crate) fn read_package_metadata(
    path: &Path,
    options: &ProjectOptions,
    overrides: &EffectiveOverrides,
    read_policy: &ReadPolicy,
    entry: &ProjectPathEntry,
) -> Result<PackageMetadata, String> {
    read_package_metadata_with_observations(path, options, overrides, read_policy, entry)
        .map(|read| read.metadata)
}

pub(crate) fn read_package_metadata_with_observations(
    path: &Path,
    options: &ProjectOptions,
    overrides: &EffectiveOverrides,
    read_policy: &ReadPolicy,
    entry: &ProjectPathEntry,
) -> Result<PackageMetadataRead, String> {
    if entry.path != path {
        return Err(format!(
            "package metadata entry does not match descriptor {}",
            path.display()
        ));
    }
    let mut tracker = ProjectReadTracker::default();
    let (contents, descriptor_observation) =
        read_payload_with_tracker(read_policy, entry, MAX_PACKAGE_METADATA_BYTES, &mut tracker)
            .map_err(|error| {
                format!(
                    "could not read package metadata {}: {error}",
                    path.display()
                )
            })?;
    let mut metadata = if extension_is(path, "dpk") {
        parse_dpk_metadata(path, &contents, options, overrides, read_policy, entry)
    } else if extension_is(path, "dproj") {
        parse_dproj_package_metadata(
            path,
            &contents,
            options,
            overrides,
            read_policy,
            entry,
            &mut tracker,
        )
    } else {
        Err(format!(
            "unsupported package descriptor extension: {}",
            path.display()
        ))
    }?;
    add_metadata_observation(&mut metadata.metadata_observations, descriptor_observation);
    metadata.metadata_observations =
        complete_metadata_observations(&metadata.metadata_files, metadata.metadata_observations);
    Ok(PackageMetadataRead {
        metadata,
        observations: tracker.observations,
    })
}

fn parse_dpk_metadata(
    path: &Path,
    contents: &str,
    _options: &ProjectOptions,
    overrides: &EffectiveOverrides,
    read_policy: &ReadPolicy,
    entry: &ProjectPathEntry,
) -> Result<PackageMetadata, String> {
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
        let Some(unit_entry) = package_unit_entry(
            &raw_path,
            base,
            overrides,
            &mut metadata.warnings,
            "package contains path",
            &entry.provenance,
            read_policy,
        ) else {
            continue;
        };
        add_package_unit_candidate(&mut metadata, canonical_unit_name(&unit_name), unit_entry);
    }
    Ok(metadata)
}

fn parse_dproj_package_metadata(
    path: &Path,
    contents: &str,
    options: &ProjectOptions,
    overrides: &EffectiveOverrides,
    read_policy: &ReadPolicy,
    entry: &ProjectPathEntry,
    tracker: &mut ProjectReadTracker,
) -> Result<PackageMetadata, String> {
    let project_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut builder = ProjectBuilder::new(
        options,
        overrides,
        Vec::new(),
        project_dir.to_path_buf(),
        read_policy.clone(),
    );
    let operations = parse_xml_operations(contents, path)?;
    builder.process_operations(operations, path, tracker, &entry.provenance);

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
    if let Some(main_source) = package_unit_entry(
        &main_source,
        project_dir,
        overrides,
        &mut metadata.warnings,
        "package main source",
        &entry.provenance,
        read_policy,
    ) {
        metadata.metadata_files.push(main_source.path);
    }
    metadata.metadata_files.extend(builder.metadata_files);
    metadata
        .metadata_observations
        .extend(builder.metadata_observations);
    for reference in builder.references {
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
            &mut metadata.warnings,
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
        let Some(unit_entry) = package_unit_entry(
            &expanded.value,
            project_dir,
            overrides,
            &mut metadata.warnings,
            "package project reference",
            &provenance,
            read_policy,
        ) else {
            continue;
        };
        let Some(stem) = unit_entry.path.file_stem() else {
            continue;
        };
        add_package_unit_candidate(
            &mut metadata,
            canonical_unit_name(&stem.to_string_lossy()),
            unit_entry,
        );
    }
    Ok(metadata)
}

fn package_unit_entry(
    raw: &str,
    base: &Path,
    overrides: &EffectiveOverrides,
    warnings: &mut Vec<String>,
    kind: &str,
    parent_provenance: &ProjectPathProvenance,
    read_policy: &ReadPolicy,
) -> Option<ProjectPathEntry> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains(UNRESOLVED_MARKER) {
        return None;
    }
    let resolved = project_path_candidate(raw, base, overrides, warnings, kind)?;
    let candidate = lexical_normalize(&resolved.path);
    let path = match resolve_existing_path_status_with_provenance(
        &candidate, &resolved, raw, warnings, kind,
    ) {
        ExistingPathStatus::Found(path) => path,
        ExistingPathStatus::Missing | ExistingPathStatus::Unresolvable => candidate,
    };
    let entry = inherit_path_provenance(
        ProjectPathEntry::resolved(path, &resolved, false),
        parent_provenance,
    );
    if !read_policy.allows_location(&entry) {
        warnings.push(format!(
            "ignored package path outside authorized read roots: {}",
            entry.path.display()
        ));
        return None;
    }
    Some(entry)
}

fn add_package_unit_candidate(
    metadata: &mut PackageMetadata,
    name: String,
    entry: ProjectPathEntry,
) {
    add_unit_candidate(&mut metadata.units, name.clone(), entry.path.clone());
    add_unique_project_path_entry(metadata.unit_entries.entry(name).or_default(), entry);
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
    configured_ranges: HashMap<String, Vec<Range<usize>>>,
    property_provenance_ranges: HashMap<String, Vec<ProvenanceRange>>,
    property_default_provenances: HashMap<String, ProjectPathProvenance>,
    configured_properties: HashSet<String>,
    global_properties: HashSet<String>,
    overrides: EffectiveOverrides,
    unknown_properties: HashSet<String>,
    unknown_import_taint: bool,
    references: Vec<DccReference>,
    warnings: Vec<String>,
    incomplete: bool,
    active_imports: HashSet<PathBuf>,
    import_count: usize,
    project_dir: PathBuf,
    property_bytes: usize,
    metadata_files: Vec<PathBuf>,
    metadata_observations: Vec<MetadataObservation>,
    read_policy: ReadPolicy,
}

impl ProjectBuilder {
    fn new(
        options: &ProjectOptions,
        overrides: &EffectiveOverrides,
        mut warnings: Vec<String>,
        project_dir: PathBuf,
        read_policy: ReadPolicy,
    ) -> Self {
        let mut properties: HashMap<String, String> =
            overrides.properties.clone().into_iter().collect();
        let mut configured_ranges = properties
            .iter()
            .map(|(name, value)| (name.clone(), full_range(value)))
            .collect::<HashMap<_, _>>();
        let mut configured_properties: HashSet<String> =
            overrides.properties.keys().cloned().collect();
        let mut client_properties = HashSet::new();
        for (name, value) in [
            ("config", options.build_config.as_ref()),
            ("platform", options.platform.as_ref()),
        ] {
            if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
                properties.insert(name.to_string(), value.clone());
                // Client selections retain the legacy project-evaluation
                // source policy even when they replace an override-file
                // value; they still remain immutable global properties.
                configured_properties.remove(name);
                configured_ranges.remove(name);
                client_properties.insert(name.to_string());
            }
        }
        let global_properties: HashSet<String> = properties.keys().cloned().collect();
        properties.entry("config".to_string()).or_default();
        properties.entry("platform".to_string()).or_default();
        let mut property_provenance_ranges = HashMap::new();
        let mut property_default_provenances = HashMap::new();
        for (name, value) in &properties {
            let key = name.to_ascii_lowercase();
            let provenance = if client_properties.contains(&key) {
                ProjectPathProvenance::LegacyNative
            } else if overrides.properties.contains_key(name) {
                ProjectPathProvenance::Configured
            } else {
                ProjectPathProvenance::LegacyNative
            };
            property_default_provenances.insert(key.clone(), provenance.clone());
            if !value.is_empty() {
                property_provenance_ranges.insert(
                    key,
                    vec![ProvenanceRange {
                        range: 0..value.len(),
                        provenance,
                    }],
                );
            }
        }
        let property_bytes = properties.values().map(String::len).sum();
        let mut incomplete = false;
        if property_bytes > MAX_TOTAL_PROPERTY_BYTES {
            let mut provenances = Vec::new();
            for name in &global_properties {
                let provenance = if client_properties.contains(name) {
                    "client initialization options".to_string()
                } else {
                    overrides.property_origins.get(name).map_or_else(
                        || "explicit override session".to_string(),
                        |path| path.display().to_string(),
                    )
                };
                if !provenances.iter().any(|existing| existing == &provenance) {
                    provenances.push(provenance);
                }
            }
            provenances.sort_unstable();
            incomplete = true;
            warnings.push(format!(
                "configured Delphi override properties exceed the {MAX_TOTAL_PROPERTY_BYTES} byte evaluator budget (sources: {})",
                provenances.join(", ")
            ));
        }
        for (name, value) in &properties {
            if value.len() <= MAX_EXPANDED_VALUE_BYTES {
                continue;
            }
            incomplete = true;
            let provenance = if client_properties.contains(name) {
                "client initialization options".to_string()
            } else {
                overrides.property_origins.get(name).map_or_else(
                    || "explicit override session".to_string(),
                    |path| path.display().to_string(),
                )
            };
            warnings.push(format!(
                "configured Delphi property {name} from {provenance} exceeds the {MAX_EXPANDED_VALUE_BYTES} byte per-value evaluator budget"
            ));
        }
        Self {
            properties,
            configured_ranges,
            property_provenance_ranges,
            property_default_provenances,
            configured_properties,
            global_properties,
            overrides: overrides.clone(),
            unknown_properties: HashSet::new(),
            unknown_import_taint: false,
            references: Vec::new(),
            warnings,
            incomplete,
            active_imports: HashSet::new(),
            import_count: 0,
            project_dir,
            property_bytes,
            metadata_files: Vec::new(),
            metadata_observations: Vec::new(),
            read_policy,
        }
    }

    fn record_payload_observation(&mut self, observation: MetadataObservation) {
        add_metadata_observation(&mut self.metadata_observations, observation);
    }

    fn property(&self, name: &str) -> Option<String> {
        self.properties.get(&name.to_ascii_lowercase()).cloned()
    }

    fn property_is_configured(&self, name: &str) -> bool {
        self.configured_properties
            .contains(&name.to_ascii_lowercase())
    }

    fn property_provenance(&self, name: &str) -> ProjectPathProvenance {
        let key = name.to_ascii_lowercase();
        self.property_provenance_ranges
            .get(&key)
            .into_iter()
            .flatten()
            .fold(
                self.property_default_provenances
                    .get(&key)
                    .cloned()
                    .unwrap_or(ProjectPathProvenance::LegacyNative),
                |current, range| combine_path_provenance(&current, &range.provenance),
            )
    }

    fn property_list_with_provenance(&self, name: &str) -> Vec<(String, ProjectPathProvenance)> {
        let Some(value) = self.property(name) else {
            return Vec::new();
        };
        let provenance_ranges = self
            .property_provenance_ranges
            .get(&name.to_ascii_lowercase())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let default_provenance = self
            .property_default_provenances
            .get(&name.to_ascii_lowercase())
            .cloned()
            .unwrap_or(ProjectPathProvenance::LegacyNative);
        let mut result = Vec::new();
        let mut start = 0;
        for separator in value.match_indices(';').map(|(index, _)| index) {
            result.push(property_list_item(
                &value,
                start,
                separator,
                provenance_ranges,
                &default_provenance,
            ));
            start = separator + 1;
        }
        result.push(property_list_item(
            &value,
            start,
            value.len(),
            provenance_ranges,
            &default_provenance,
        ));
        result
    }

    fn process_root_dproj(
        &mut self,
        path: &Path,
        tracker: &mut ProjectReadTracker,
    ) -> Result<(), String> {
        self.metadata_files.push(path.to_path_buf());
        let entry = ProjectPathEntry::legacy(path.to_path_buf());
        let (contents, observation) =
            read_payload_with_tracker(&self.read_policy, &entry, MAX_PROJECT_BYTES, tracker)
                .map_err(|error| format!("could not read project {}: {error}", path.display()))?;
        self.record_payload_observation(observation);
        let operations = parse_xml_operations(&contents, path)?;
        self.process_operations(operations, path, tracker, &entry.provenance);
        Ok(())
    }

    fn process_operations(
        &mut self,
        operations: Vec<XmlOperation>,
        source_file: &Path,
        tracker: &mut ProjectReadTracker,
        source_provenance: &ProjectPathProvenance,
    ) {
        let base = source_file.parent().unwrap_or_else(|| Path::new("."));
        for operation in operations {
            match operation {
                XmlOperation::PropertyGroup(group) => {
                    self.process_property_group(group, source_file, base, source_provenance);
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
                            overrides: &self.overrides,
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
                            source_provenance: source_provenance.clone(),
                        }),
                        TruthValue::False => {}
                        TruthValue::Unknown => self.incomplete = true,
                    }
                }
                XmlOperation::Import(import) => {
                    self.process_import(import, source_file, base, tracker, source_provenance);
                }
                XmlOperation::Unsupported(message) => self.warnings.push(message),
            }
        }
    }

    fn process_property_group(
        &mut self,
        group: PropertyGroup,
        source_file: &Path,
        base: &Path,
        source_provenance: &ProjectPathProvenance,
    ) {
        let group_result = condition_matches(
            group.condition.as_deref(),
            ConditionEnvironment {
                properties: &self.properties,
                unknown_properties: &self.unknown_properties,
                unknown_import_taint: self.unknown_import_taint,
                overrides: &self.overrides,
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
                    self.mark_property_unknown(&property.name, source_file, source_provenance);
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
                    overrides: &self.overrides,
                },
                base,
                &mut self.metadata_files,
                &mut self.warnings,
                source_file,
            ) {
                TruthValue::False => continue,
                TruthValue::Unknown => {
                    self.incomplete = true;
                    self.mark_property_unknown(&property.name, source_file, source_provenance);
                    continue;
                }
                TruthValue::True => {}
            }
            if self
                .global_properties
                .contains(&property.name.to_ascii_lowercase())
            {
                continue;
            }
            let value = expand_value(
                &property.value,
                &property.name,
                &self.properties,
                &self.configured_ranges,
                &self.configured_properties,
                &self.property_provenance_ranges,
                &self.property_default_provenances,
                &self.unknown_properties,
                &mut self.warnings,
                source_file,
                source_provenance,
            );
            let trimmed = value.value.trim();
            let trim_start = value.value.len() - value.value.trim_start().len();
            let configured_ranges =
                trim_ranges(&value.configured_ranges, trim_start, trimmed.len());
            let provenance_ranges =
                trim_provenance_ranges(&value.provenance_ranges, trim_start, trimmed.len());
            self.set_property(
                &property.name,
                trimmed.to_string(),
                value.unknown,
                configured_ranges,
                value.explicit_dependency,
                provenance_ranges,
                source_provenance,
                source_file,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn set_property(
        &mut self,
        name: &str,
        value: String,
        unknown: bool,
        configured_ranges: Vec<Range<usize>>,
        explicit_dependency: bool,
        provenance_ranges: Vec<ProvenanceRange>,
        source_provenance: &ProjectPathProvenance,
        source_file: &Path,
    ) {
        let key = name.to_ascii_lowercase();
        if self.global_properties.contains(&key) {
            return;
        }
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
        if explicit_dependency {
            self.configured_properties.insert(key.clone());
        } else {
            self.configured_properties.remove(&key);
        }
        if configured_ranges.is_empty() {
            self.configured_ranges.remove(&key);
        } else {
            self.configured_ranges
                .insert(key.clone(), configured_ranges);
        }
        if provenance_ranges.is_empty() {
            self.property_provenance_ranges.remove(&key);
        } else {
            self.property_provenance_ranges
                .insert(key.clone(), provenance_ranges);
        }
        self.property_default_provenances
            .insert(key.clone(), source_provenance.clone());
        self.properties.insert(key, value);
    }

    fn taint_unknown_import(&mut self) {
        self.unknown_import_taint = true;
        let keys: Vec<String> = self.properties.keys().cloned().collect();
        for key in keys {
            if self.global_properties.contains(&key) {
                continue;
            }
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
            let previous_bytes = self.properties.get(&key).map_or(0, String::len);
            self.property_bytes = self
                .property_bytes
                .saturating_sub(previous_bytes)
                .saturating_add(UNRESOLVED_MARKER.len_utf8());
            self.properties
                .insert(key.clone(), UNRESOLVED_MARKER.to_string());
            self.configured_ranges.remove(&key);
            self.property_provenance_ranges.remove(&key);
            self.unknown_properties.insert(key);
        }
    }

    fn mark_property_unknown(
        &mut self,
        name: &str,
        source_file: &Path,
        source_provenance: &ProjectPathProvenance,
    ) {
        if self.global_properties.contains(&name.to_ascii_lowercase()) {
            return;
        }
        self.set_property(
            name,
            UNRESOLVED_MARKER.to_string(),
            true,
            Vec::new(),
            false,
            Vec::new(),
            source_provenance,
            source_file,
        );
        self.unknown_properties.insert(name.to_ascii_lowercase());
    }

    fn process_import(
        &mut self,
        import: Import,
        source_file: &Path,
        base: &Path,
        tracker: &mut ProjectReadTracker,
        source_provenance: &ProjectPathProvenance,
    ) {
        if !import_may_be_optset(&import.project) {
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
            &self.configured_ranges,
            &self.configured_properties,
            &self.property_provenance_ranges,
            &self.property_default_provenances,
            &self.unknown_properties,
            &mut self.warnings,
            source_file,
            source_provenance,
        );
        if expanded.unknown || expanded.value.contains(UNRESOLVED_MARKER) {
            self.incomplete = true;
            self.taint_unknown_import();
            return;
        }
        let Some(resolved) = project_path_candidate(
            &expanded.value,
            base,
            &self.overrides,
            &mut self.warnings,
            "optset import",
        ) else {
            self.incomplete = true;
            self.taint_unknown_import();
            return;
        };
        let candidate = lexical_normalize(&resolved.path);
        if !extension_is(&candidate, "optset") {
            self.warnings.push(format!(
                "ignored non-optset project import in {} (targets are not executed): {}",
                source_file.display(),
                import.project
            ));
            return;
        }
        let path_status = resolve_existing_path_status_with_provenance(
            &candidate,
            &resolved,
            &expanded.value,
            &mut self.warnings,
            "optset import",
        );
        let path = match &path_status {
            ExistingPathStatus::Found(path) => path,
            ExistingPathStatus::Missing => &candidate,
            ExistingPathStatus::Unresolvable => {
                self.incomplete = true;
                self.taint_unknown_import();
                return;
            }
        };
        let entry = inherit_path_provenance(
            ProjectPathEntry::resolved(path.clone(), &resolved, expanded.explicit_dependency),
            &expanded_path_provenance(&expanded, source_provenance),
        );
        if matches!(path_status, ExistingPathStatus::Found(_))
            && !self.read_policy.allows_entry(&entry)
        {
            self.incomplete = true;
            self.taint_unknown_import();
            self.warnings.push(format!(
                "ignored optset import outside authorized read roots: {}",
                path.display()
            ));
            return;
        }
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
                overrides: &self.overrides,
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
            self.warnings.push(missing_path_warning(
                "optset import",
                &expanded.value,
                &candidate,
                &resolved,
            ));
            return;
        };
        if !self.active_imports.insert(path.clone()) {
            self.warnings
                .push(format!("optset import cycle ignored at {}", path.display()));
            return;
        }
        self.import_count += 1;
        let result =
            read_payload_with_tracker(&self.read_policy, &entry, MAX_IMPORT_BYTES, tracker)
                .and_then(|(contents, observation)| {
                    self.record_payload_observation(observation);
                    parse_xml_operations(&contents, &path)
                });
        match result {
            Ok(operations) => {
                self.process_operations(operations, &path, tracker, &entry.provenance)
            }
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
    source_provenance: ProjectPathProvenance,
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

fn import_may_be_optset(project: &str) -> bool {
    let project = project.trim();
    if looks_like_optset(project) {
        return true;
    }
    let normalized = project.replace('\\', "/");
    let Some(name) = normalized.rsplit('/').next() else {
        return false;
    };
    if !name.contains("$(") {
        return false;
    }
    match name.rfind(')') {
        Some(end) => name[end + 1..].is_empty(),
        None => true,
    }
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
                        source_provenance: ProjectPathProvenance::LegacyNative,
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
                        source_provenance: ProjectPathProvenance::LegacyNative,
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
    explicit_dependency: bool,
    configured_ranges: Vec<Range<usize>>,
    provenance_ranges: Vec<ProvenanceRange>,
}

#[allow(clippy::too_many_arguments)]
fn expand_value(
    value: &str,
    current_property: &str,
    properties: &HashMap<String, String>,
    property_ranges: &HashMap<String, Vec<Range<usize>>>,
    configured_properties: &HashSet<String>,
    property_provenance_ranges: &HashMap<String, Vec<ProvenanceRange>>,
    property_default_provenances: &HashMap<String, ProjectPathProvenance>,
    unknown_properties: &HashSet<String>,
    warnings: &mut Vec<String>,
    source_file: &Path,
    source_provenance: &ProjectPathProvenance,
) -> ExpandedValue {
    let mut expanded = String::new();
    let mut unknown = false;
    let mut explicit_dependency = false;
    let mut configured_ranges = Vec::new();
    let mut provenance_ranges = Vec::new();
    let mut cursor = 0;
    while let Some(relative_start) = value[cursor..].find("$(") {
        let start = cursor + relative_start;
        if !append_expansion_with_provenance(
            &mut expanded,
            &value[cursor..start],
            warnings,
            source_file,
            &mut provenance_ranges,
            source_provenance,
        ) {
            return ExpandedValue {
                value: UNRESOLVED_MARKER.to_string(),
                unknown: true,
                explicit_dependency,
                configured_ranges: Vec::new(),
                provenance_ranges: Vec::new(),
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
                explicit_dependency,
                configured_ranges: Vec::new(),
                provenance_ranges: Vec::new(),
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
            if !append_expansion_with_provenance(
                &mut expanded,
                &directory,
                warnings,
                source_file,
                &mut provenance_ranges,
                source_provenance,
            ) {
                return ExpandedValue {
                    value: UNRESOLVED_MARKER.to_string(),
                    unknown: true,
                    explicit_dependency,
                    configured_ranges: Vec::new(),
                    provenance_ranges: Vec::new(),
                };
            }
        } else if let Some(replacement) = properties.get(&key) {
            let replacement_start = expanded.len();
            if !append_expansion(&mut expanded, replacement, warnings, source_file) {
                return ExpandedValue {
                    value: UNRESOLVED_MARKER.to_string(),
                    unknown: true,
                    explicit_dependency,
                    configured_ranges: Vec::new(),
                    provenance_ranges: Vec::new(),
                };
            }
            let replacement_end = expanded.len();
            if let Some(ranges) = property_provenance_ranges.get(&key) {
                for range in ranges {
                    if range.range.start <= range.range.end && range.range.end <= replacement.len()
                    {
                        provenance_ranges.push(ProvenanceRange {
                            range: (replacement_start + range.range.start)
                                ..(replacement_start + range.range.end),
                            provenance: range.provenance.clone(),
                        });
                    }
                }
            } else if replacement_start < replacement_end {
                provenance_ranges.push(ProvenanceRange {
                    range: replacement_start..replacement_end,
                    provenance: property_default_provenances
                        .get(&key)
                        .cloned()
                        .unwrap_or_else(|| source_provenance.clone()),
                });
            } else if let Some(provenance) = property_default_provenances.get(&key) {
                provenance_ranges.push(ProvenanceRange {
                    range: replacement_start..replacement_end,
                    provenance: provenance.clone(),
                });
            }
            if let Some(ranges) = property_ranges.get(&key) {
                for range in ranges {
                    if range.start < range.end && range.end <= replacement.len() {
                        configured_ranges.push(
                            (replacement_start + range.start)..(replacement_start + range.end),
                        );
                    }
                }
            }
            explicit_dependency |= configured_properties.contains(&key)
                || property_ranges
                    .get(&key)
                    .is_some_and(|ranges| !ranges.is_empty());
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
                    explicit_dependency,
                    configured_ranges: Vec::new(),
                    provenance_ranges: Vec::new(),
                };
            }
        }
        cursor = end + 1;
    }
    if !append_expansion_with_provenance(
        &mut expanded,
        &value[cursor..],
        warnings,
        source_file,
        &mut provenance_ranges,
        source_provenance,
    ) {
        return ExpandedValue {
            value: UNRESOLVED_MARKER.to_string(),
            unknown: true,
            explicit_dependency,
            configured_ranges: Vec::new(),
            provenance_ranges: Vec::new(),
        };
    }
    ExpandedValue {
        value: expanded,
        unknown,
        explicit_dependency,
        configured_ranges,
        provenance_ranges,
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

fn append_expansion_with_provenance(
    target: &mut String,
    addition: &str,
    warnings: &mut Vec<String>,
    source_file: &Path,
    provenance_ranges: &mut Vec<ProvenanceRange>,
    provenance: &ProjectPathProvenance,
) -> bool {
    let start = target.len();
    if !append_expansion(target, addition, warnings, source_file) {
        return false;
    }
    if start < target.len() {
        provenance_ranges.push(ProvenanceRange {
            range: start..target.len(),
            provenance: provenance.clone(),
        });
    }
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
    overrides: &'a EffectiveOverrides,
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
        overrides: environment.overrides,
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
    overrides: &'a EffectiveOverrides,
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
            let Some(resolved) = project_path_candidate(
                &argument.value,
                self.base,
                self.overrides,
                self.warnings,
                "Exists condition",
            ) else {
                return Ok(TruthValue::False);
            };
            let candidate = lexical_normalize(&resolved.path);
            let path_status = resolve_existing_path_status_with_provenance(
                &candidate,
                &resolved,
                &argument.value,
                self.warnings,
                "Exists condition",
            );
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

#[cfg(test)]
mod tests {
    use super::{
        EffectiveOverrides, MAX_OWNERSHIP_CANDIDATES, MAX_OWNERSHIP_SOURCE_BYTES,
        MAX_OWNERSHIP_SOURCE_FILES, MAX_PROJECT_DIRECTORY_ENTRIES, MetadataObservation,
        ProjectContext, ProjectOptions, ProjectPathEntry, ProjectPathProvenance, ProjectReadStamp,
        ProjectReadTracker, ReadPolicy, project_candidate_membership, read_package_metadata,
        test_cancel_project_scan_after_checks,
    };
    use pascal_core::delphi_overrides::OverrideSession;
    use std::fs;
    use std::io::Write;
    #[cfg(unix)]
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

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
    fn project_read_tracker_preserves_the_first_observation() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join("App.dproj");
        let first_stamp = ProjectReadStamp {
            bytes: 5,
            modified: None,
            is_dir: false,
            is_symlink: false,
        };
        let second_stamp = ProjectReadStamp {
            bytes: 6,
            modified: None,
            is_dir: false,
            is_symlink: false,
        };
        let mut tracker = ProjectReadTracker::default();
        tracker.record(&path, first_stamp.clone(), b"first");
        tracker.record(&path, second_stamp, b"second");

        let discovery = tracker.into_discovery(ProjectContext::default());
        assert_eq!(discovery.observations.len(), 1);
        assert_eq!(discovery.observations[0].stamp, first_stamp);
        assert_eq!(
            discovery.observations[0].content_bytes,
            Some(b"first".to_vec())
        );
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

        assert_eq!(context.project_file, Some(root.join("A.dproj")));
        for path in [&main, &member] {
            assert!(
                context.metadata_observations.iter().any(|observation| {
                    let MetadataObservation::Payload {
                        path: observed,
                        read_policy,
                        path_entry,
                        stamp,
                        content_hash,
                    } = observation
                    else {
                        return false;
                    };
                    observed == path
                        && read_policy == &context.read_policy
                        && path_entry.path == *path
                        && stamp
                            == &crate::workspace::path_stamp_result(path)
                                .expect("payload path stamp")
                        && *content_hash
                            == crate::workspace::content_hash_bytes(
                                &fs::read(path).expect("payload bytes"),
                            )
                }),
                "evaluated payload {path:?} was downgraded to stat-only: {context:?}"
            );
        }
    }

    #[test]
    fn metadata_observation_keeps_the_first_payload_for_each_path() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let metadata = root.join("App.dproj");
        fs::write(&metadata, "<Project />").expect("metadata");
        let policy = ReadPolicy::new(
            std::slice::from_ref(&root),
            &[],
            &[],
            &EffectiveOverrides::default(),
        );
        let entry = ProjectPathEntry {
            path: metadata.clone(),
            provenance: ProjectPathProvenance::Configured,
        };
        let first_stamp =
            crate::workspace::path_stamp_result(&metadata).expect("first metadata stamp");
        let first = MetadataObservation::Payload {
            path: metadata.clone(),
            read_policy: policy.clone(),
            path_entry: entry.clone(),
            stamp: first_stamp.clone(),
            content_hash: 1,
        };
        let replacement = MetadataObservation::Payload {
            path: metadata.clone(),
            read_policy: policy.clone(),
            path_entry: entry.clone(),
            stamp: first_stamp.clone(),
            content_hash: 2,
        };
        let mut observations = vec![MetadataObservation::Stat {
            path: metadata.clone(),
        }];
        super::add_metadata_observation(&mut observations, first.clone());
        super::add_metadata_observation(&mut observations, replacement);

        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0], first);
    }

    #[cfg(unix)]
    #[test]
    fn package_metadata_rejects_a_symlinked_descriptor() {
        let temp = tempfile::tempdir().expect("temporary package directory");
        let outside = temp.path().join("outside.dpk");
        let descriptor = temp.path().join("Package.dpk");
        fs::write(&outside, "package Package; end.\n").expect("package descriptor");
        symlink(&outside, &descriptor).expect("descriptor symlink");
        let root = temp.path().to_path_buf();
        let read_policy = ReadPolicy::new(
            std::slice::from_ref(&root),
            &[],
            &[],
            &EffectiveOverrides::default(),
        );
        let entry = ProjectPathEntry {
            path: descriptor.clone(),
            provenance: ProjectPathProvenance::Configured,
        };

        let error = read_package_metadata(
            &descriptor,
            &ProjectOptions::default(),
            &EffectiveOverrides::default(),
            &read_policy,
            &entry,
        )
        .expect_err("symlinked package descriptors must not be opened");
        assert!(
            error.contains("regular file")
                || error.contains("symlink")
                || error.contains("authorized"),
            "unexpected package descriptor error: {error}"
        );
    }

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
    fn package_metadata_rejects_relative_units_outside_a_mapped_descriptor_root() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path();
        let sdk = root.join("sdk");
        let outside = root.join("outside");
        let descriptor = sdk.join("Package.dpk");
        fs::create_dir_all(&sdk).expect("mapped package directory");
        fs::create_dir_all(&outside).expect("outside directory");
        fs::write(
            &descriptor,
            "package Package; contains Provider in '../outside/Provider.pas'; end.",
        )
        .expect("package descriptor");
        fs::write(
            outside.join("Provider.pas"),
            "unit Provider; interface implementation end.",
        )
        .expect("outside package unit");
        let overrides = EffectiveOverrides {
            path_mappings: vec![pascal_core::delphi_overrides::PathMapping {
                from: "c:/sdk".to_string(),
                to: sdk.clone(),
                config_file: root.join(".delphi-tools.local.toml"),
            }],
            ..EffectiveOverrides::default()
        };
        let read_policy = ReadPolicy::new(&[root.to_path_buf()], &[], &[], &overrides);
        let entry = ProjectPathEntry {
            path: descriptor.clone(),
            provenance: ProjectPathProvenance::Mapped { root: sdk },
        };

        let metadata = read_package_metadata(
            &descriptor,
            &ProjectOptions::default(),
            &overrides,
            &read_policy,
            &entry,
        )
        .expect("read package metadata");

        assert!(
            !metadata
                .units
                .values()
                .flatten()
                .any(|path| path == &outside.join("Provider.pas")),
            "package metadata exposed a unit outside its mapped descriptor root: {metadata:?}"
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

    #[test]
    fn configured_exclusions_are_not_weakened_by_overlapping_source_roots() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let workspace = temp.path().join("ws");
        let excluded = workspace.join("vendor/private/settings.optset");
        fs::create_dir_all(excluded.parent().expect("excluded parent")).expect("directories");
        fs::write(&excluded, b"settings").expect("excluded metadata");

        let policy = ReadPolicy::new(
            std::slice::from_ref(&workspace),
            &["vendor".to_string()],
            &["vendor/private".to_string()],
            &EffectiveOverrides::default(),
        );
        let entry = ProjectPathEntry {
            path: excluded,
            provenance: ProjectPathProvenance::Configured,
        };

        assert!(
            !policy.allows_entry(&entry),
            "an overlapping source root must not bypass the workspace-relative exclusion"
        );
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

    #[test]
    fn legacy_payload_entries_respect_configured_exclusions() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let path = root.join("private/Hidden.pas");
        fs::create_dir_all(path.parent().expect("private directory")).expect("directories");
        fs::write(&path, b"unit Hidden; interface implementation end.").expect("source");
        let policy = ReadPolicy::new(
            std::slice::from_ref(&root),
            &[],
            &["private".to_string()],
            &EffectiveOverrides::default(),
        );
        let entry = ProjectPathEntry {
            path,
            provenance: ProjectPathProvenance::LegacyNative,
        };

        assert!(
            !policy.allows_legacy_payload_entry(&entry),
            "legacy payload compatibility must not bypass configured exclusions"
        );
    }
}
