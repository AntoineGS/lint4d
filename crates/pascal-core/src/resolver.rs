//! Request-scoped Pascal source resolution.
//!
//! The resolver owns lookup policy and bounded observations, while callers own
//! project discovery state and provide a [`SourceStore`] for disk or overlay
//! bytes.  No result is considered precise unless the candidate was loaded and
//! its declaration matched the requested name.

use pascal_project::{
    MetadataObservation, ProjectContext, ProjectPathEntry, ProjectPathProvenance,
    ProjectReadObservation, ProjectReadStamp, ReadPolicy, content_hash_bytes, path_stamp_result,
    read_package_metadata_with_observations,
};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::fs;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A caller-owned cancellation source.
pub trait CancellationToken {
    fn is_cancelled(&self) -> bool;
}

impl CancellationToken for AtomicBool {
    fn is_cancelled(&self) -> bool {
        self.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoCancellation;

impl CancellationToken for NoCancellation {
    fn is_cancelled(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolverLimits {
    pub max_dependency_units: usize,
    pub max_directory_entries: usize,
    pub max_scanned_bytes: usize,
    pub max_source_bytes: usize,
    pub max_include_files: usize,
    pub max_include_bytes: usize,
    pub max_include_directives: usize,
    pub max_include_depth: usize,
    pub max_package_lookups: usize,
    pub max_package_unit_candidates: usize,
    pub max_package_catalogue_entries: usize,
    pub max_package_catalogues: usize,
    pub max_resolution_warnings: usize,
}

impl Default for ResolverLimits {
    fn default() -> Self {
        Self {
            max_dependency_units: 256,
            max_directory_entries: 1_048_576,
            max_scanned_bytes: 8 * 1024 * 1024 * 1024,
            max_source_bytes: 16 * 1024 * 1024,
            max_include_files: 4_096,
            max_include_bytes: 256 * 1024 * 1024,
            max_include_directives: 16_384,
            max_include_depth: 256,
            max_package_lookups: 256,
            max_package_unit_candidates: 1_024,
            max_package_catalogue_entries: 524_288,
            max_package_catalogues: 64,
            max_resolution_warnings: 256,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Unit,
    Include,
    PackageDescriptor,
}

#[derive(Debug, Clone, Copy)]
pub struct DirectoryRequest<'a> {
    pub directory: &'a Path,
    pub entry: &'a ProjectPathEntry,
    pub read_policy: &'a ReadPolicy,
}

#[derive(Debug, Clone, Copy)]
pub struct SourceRequest<'a> {
    pub path: &'a Path,
    pub entry: &'a ProjectPathEntry,
    pub read_policy: &'a ReadPolicy,
    pub legacy_route: Option<&'a LegacyRoute>,
    pub kind: SourceKind,
    pub max_bytes: usize,
}

pub trait SourceStore {
    fn list_directory(
        &mut self,
        request: DirectoryRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<DirectoryListing, SourceStoreError>;

    fn overlay_candidates(&self, roots: &[PathBuf], names: &[String]) -> Vec<PathBuf>;

    fn load(
        &mut self,
        request: SourceRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryListing {
    pub files: Vec<PathBuf>,
    pub directories: Vec<PathBuf>,
    pub stamp: Option<ProjectReadStamp>,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSource {
    pub id: SourceId,
    pub path: PathBuf,
    pub bytes: Arc<[u8]>,
    /// Optional caller-decoded text.  Stateful adapters can provide text in
    /// their coordinate model while retaining raw bytes for revision hashes.
    pub decoded_text: Option<Arc<str>>,
    pub revision: SourceRevision,
}

impl LoadedSource {
    /// Return the UTF-8 bytes used by parser, resolver metadata, and CFG.
    ///
    /// `bytes` remains the exact payload read from disk or supplied by an
    /// overlay.  A source store may attach decoded text when that payload uses
    /// a legacy encoding (or another decoded coordinate model); consumers use
    /// this method so directive/import ranges and parsed bytes share one
    /// validated coordinate space.  The raw payload is still retained for
    /// limits, hashes, and [`SourceRevision`] revalidation.
    pub fn analysis_bytes(&self) -> Cow<'_, [u8]> {
        self.decoded_text.as_deref().map_or_else(
            || Cow::Borrowed(self.bytes.as_ref()),
            |text| Cow::Borrowed(text.as_bytes()),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(String);

impl SourceId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceRevision {
    Disk {
        stamp: ProjectReadStamp,
        content_hash: u64,
        read_policy: ReadPolicy,
        path_entry: ProjectPathEntry,
    },
    Overlay {
        version: i32,
        content_hash: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceStoreError {
    Cancelled,
    NotFound { path: PathBuf },
    Unauthorized { path: PathBuf, reason: String },
    NotRegularFile { path: PathBuf },
    TooLarge { path: PathBuf, maximum: usize },
    Io { path: PathBuf, message: String },
    Incomplete { path: PathBuf, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolverError {
    Cancelled,
    InvalidRequest(String),
    SourceStore(SourceStoreError),
    LimitExceeded {
        limit: &'static str,
        observed: usize,
        maximum: usize,
    },
}

impl fmt::Display for ResolverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => write!(formatter, "request cancelled"),
            Self::InvalidRequest(reason) => write!(formatter, "invalid resolver request: {reason}"),
            Self::SourceStore(error) => write!(formatter, "source store error: {error:?}"),
            Self::LimitExceeded {
                limit,
                observed,
                maximum,
            } => {
                write!(
                    formatter,
                    "resolver limit {limit} exceeded: {observed} > {maximum}"
                )
            }
        }
    }
}

impl std::error::Error for ResolverError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionCandidate {
    pub path: PathBuf,
    pub declared_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution<T> {
    Found(T),
    Unavailable {
        reason: String,
    },
    Ambiguous {
        candidates: Vec<ResolutionCandidate>,
    },
    Incomplete {
        reason: String,
        candidates: Vec<ResolutionCandidate>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionOutcome<T> {
    pub result: Resolution<T>,
    pub observations: Vec<ResolutionObservation>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionObservation {
    Directory {
        path: PathBuf,
        entry: ProjectPathEntry,
        stamp: Option<ProjectReadStamp>,
        complete: bool,
    },
    Candidate {
        path: PathBuf,
        entry: Option<ProjectPathEntry>,
        stamp: Option<ProjectReadStamp>,
        present: bool,
    },
    Payload {
        source_id: SourceId,
        path: PathBuf,
        revision: SourceRevision,
    },
    Metadata(MetadataObservation),
    ProjectRead(ProjectReadObservation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionReport {
    pub observations: Vec<ResolutionObservation>,
    pub warnings: Vec<String>,
    pub complete: bool,
    pub incomplete_reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitResolveRequest<'a> {
    pub requested_name: &'a str,
    pub importer_path: &'a Path,
    pub legacy_route: Option<&'a LegacyRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LegacyRoute {
    pub source_path: PathBuf,
    pub sibling_directory: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSite {
    pub byte_range: Range<usize>,
    pub requested_name: String,
    pub section: ImportSection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludeResolveRequest<'a> {
    pub including_path: &'a Path,
    pub byte_range: Range<usize>,
    pub requested_name: &'a str,
    pub legacy_route: Option<&'a LegacyRoute>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportSection {
    Module,
    Interface,
    Implementation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUnit {
    pub requested_name: String,
    pub declared_name: String,
    pub source: LoadedSource,
}

pub type ResolvedSource = LoadedSource;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionTarget<T> {
    Found(T),
    Unavailable,
    Ambiguous,
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImport {
    pub importer_source_id: SourceId,
    pub site: ImportSite,
    pub target: ResolutionTarget<SourceId>,
    pub authorized_qualifiers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInclude {
    pub including_source_id: SourceId,
    pub byte_range: Range<usize>,
    pub requested_name: String,
    pub target: ResolutionTarget<SourceId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImports {
    pub bindings: Vec<ResolvedImport>,
    pub dependencies: Vec<ResolvedUnit>,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProject {
    pub root: ResolvedUnit,
    pub units: Vec<ResolvedUnit>,
    pub imports: Vec<ResolvedImport>,
    pub includes: Vec<ResolvedInclude>,
    pub include_sources: Vec<LoadedSource>,
    pub complete: bool,
    pub report: ResolutionReport,
}

#[path = "resolver_store.rs"]
mod store;
pub use store::{FilesystemSourceStore, OverlaySource};

/// Source resolver session.  All caches and observations are request-local.
pub struct UnitResolver<S> {
    context: ProjectContext,
    workspace_roots: Vec<PathBuf>,
    store: S,
    limits: ResolverLimits,
    report: ResolutionReport,
    loaded: HashMap<SourceId, LoadedSource>,
    directories: HashMap<DirectoryCacheKey, DirectoryListing>,
    package_catalogues: HashMap<PathBuf, Catalogue>,
    unit_cache: HashMap<UnitCacheKey, ResolvedUnit>,
    legacy_routes: HashMap<SourceId, LegacyRoute>,
    project_unit_ids: Option<HashSet<SourceId>>,
    include_files: usize,
    include_bytes: usize,
    include_directives: usize,
    package_lookups: usize,
    package_catalogue_count: usize,
    scanned_entries: usize,
    scanned_bytes: usize,
    project_includes: Vec<ResolvedInclude>,
    include_seen: HashSet<SourceId>,
}

impl<S: SourceStore> UnitResolver<S> {
    pub fn new(
        context: ProjectContext,
        workspace_roots: Vec<PathBuf>,
        store: S,
        limits: ResolverLimits,
    ) -> Self {
        let mut report = ResolutionReport {
            observations: Vec::new(),
            warnings: Vec::new(),
            complete: context.discovery_complete,
            incomplete_reasons: Vec::new(),
        };
        for observation in &context.metadata_observations {
            push_observation(
                &mut report.observations,
                ResolutionObservation::Metadata(observation.clone()),
            );
        }
        for warning in &context.warnings {
            push_warning(&mut report, warning.clone(), limits.max_resolution_warnings);
        }
        if !context.discovery_complete {
            report
                .incomplete_reasons
                .push("project metadata is incomplete".to_string());
        }
        Self {
            context,
            workspace_roots: workspace_roots
                .into_iter()
                .map(|root| canonical_path(&root))
                .collect(),
            store,
            limits,
            report,
            loaded: HashMap::new(),
            directories: HashMap::new(),
            package_catalogues: HashMap::new(),
            unit_cache: HashMap::new(),
            legacy_routes: HashMap::new(),
            project_unit_ids: None,
            include_files: 0,
            include_bytes: 0,
            include_directives: 0,
            package_lookups: 0,
            package_catalogue_count: 0,
            scanned_entries: 0,
            scanned_bytes: 0,
            project_includes: Vec::new(),
            include_seen: HashSet::new(),
        }
    }

    pub fn resolve_unit(
        &mut self,
        request: UnitResolveRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> ResolutionOutcome<ResolvedUnit> {
        let before_observations = self.report.observations.len();
        let before_warnings = self.report.warnings.len();
        let result = self.resolve_unit_result(request, cancel);
        self.outcome(result, before_observations, before_warnings)
    }

    /// Result-preserving variant for callers that must distinguish
    /// cancellation from an ordinary incomplete lookup.
    pub fn try_resolve_unit(
        &mut self,
        request: UnitResolveRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<ResolvedUnit>, ResolverError> {
        let before_observations = self.report.observations.len();
        let before_warnings = self.report.warnings.len();
        let result = self.resolve_unit_result(request, cancel)?;
        Ok(self.outcome(Ok(result), before_observations, before_warnings))
    }

    /// Load one caller-selected source through the same authorization and
    /// observation path used by unit candidates.
    ///
    /// LSP and other stateful callers may already have selected the document
    /// that owns an import list. They must not resolve that document by name:
    /// doing so could turn an accepted root into an ambiguity merely because a
    /// second same-named file is present. The caller still supplies the exact
    /// legacy route proof, when applicable.
    pub fn load_source(
        &mut self,
        path: &Path,
        legacy_route: Option<&LegacyRoute>,
        kind: SourceKind,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, ResolverError> {
        self.check_cancel(cancel)?;
        let path = canonical_path(path);
        let entry = self
            .context
            .path_entry_for(&path)
            .or_else(|| self.legacy_entry_for_path(&path, legacy_route))
            .ok_or_else(|| {
                ResolverError::InvalidRequest(format!(
                    "source {} is outside the requester-scoped project roots",
                    path.display()
                ))
            })?;
        let source = self
            .load_candidate_with_limit(
                &path,
                &entry,
                legacy_route,
                kind,
                self.limits.max_source_bytes,
                cancel,
            )
            .map_err(|error| match error {
                SourceStoreError::Cancelled => ResolverError::Cancelled,
                error => ResolverError::SourceStore(error),
            })?;
        if let Some(route) = self.legacy_route_for_source(&source, legacy_route) {
            self.legacy_routes.insert(source.id.clone(), route);
        }
        Ok(source)
    }

    pub fn resolve_imports(
        &mut self,
        importer: &ResolvedUnit,
        sites: &[ImportSite],
        cancel: &dyn CancellationToken,
    ) -> Result<ResolvedImports, ResolverError> {
        let (text, require_byte_preserving_coordinates) =
            importer.source.decoded_text.as_deref().map_or_else(
                || {
                    (
                        crate::text::decode_bytes(&importer.source.bytes).into_owned(),
                        true,
                    )
                },
                |text| (text.to_owned(), false),
            );
        self.resolve_imports_inner(
            importer,
            sites,
            &text,
            require_byte_preserving_coordinates,
            cancel,
        )
    }

    /// Resolve imports whose ranges were produced from a caller-owned decoded
    /// source string.  LSP indexes use UTF-8 byte offsets in decoded text even
    /// when the on-disk payload is UTF-16, so those ranges must not be compared
    /// with the raw payload length.  The raw source revision remains attached
    /// to every loaded dependency for stale-result validation.
    pub fn resolve_imports_with_text(
        &mut self,
        importer: &ResolvedUnit,
        sites: &[ImportSite],
        text: &str,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolvedImports, ResolverError> {
        self.resolve_imports_inner(importer, sites, text, false, cancel)
    }

    fn resolve_imports_inner(
        &mut self,
        importer: &ResolvedUnit,
        sites: &[ImportSite],
        text: &str,
        require_byte_preserving_coordinates: bool,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolvedImports, ResolverError> {
        let mut bindings = Vec::with_capacity(sites.len());
        let mut dependencies = Vec::new();
        let mut dependency_ids = HashSet::new();
        let mut complete = true;
        let conditional_context = self.context.effective_conditional_context();
        let analysis =
            crate::conditional::analyze_with_context_and_cancel(text, &conditional_context, cancel);
        self.check_cancel(cancel)?;
        if (require_byte_preserving_coordinates && text.len() != importer.source.bytes.len())
            || !analysis.complete
        {
            complete = false;
            self.mark_incomplete(format!(
                "conditional analysis is incomplete for {}",
                importer.source.path.display()
            ));
        } else if !analysis.unknown_spans.is_empty() {
            complete = false;
            self.mark_incomplete(format!(
                "source {} contains unknown conditional activity",
                importer.source.path.display()
            ));
        }
        for site in sites {
            self.check_cancel(cancel)?;
            let importer_route = self
                .legacy_routes
                .get(&importer.source.id)
                .cloned()
                .or_else(|| self.legacy_route_for_source(&importer.source, None));
            let key = UnitCacheKey::new(
                &importer.source.path,
                &site.requested_name,
                importer_route.as_ref(),
            );
            let inactive = analysis_contains_range(&analysis.inactive_spans, &site.byte_range);
            let unknown = (require_byte_preserving_coordinates
                && text.len() != importer.source.bytes.len())
                || !analysis.complete
                || analysis_contains_range(&analysis.unknown_spans, &site.byte_range);
            let outcome =
                if inactive {
                    Resolution::Unavailable {
                        reason: "import is in a known inactive conditional branch".to_string(),
                    }
                } else if unknown {
                    Resolution::Incomplete {
                        reason: "import conditional activity is unknown".to_string(),
                        candidates: Vec::new(),
                    }
                } else if let Some(unit) = self.unit_cache.get(&key).cloned() {
                    Resolution::Found(unit)
                } else if self.project_unit_ids.as_ref().is_some_and(|ids| {
                    ids.len().saturating_sub(1) >= self.limits.max_dependency_units
                }) {
                    complete = false;
                    self.mark_incomplete(format!(
                        "dependency unit limit ({}) reached",
                        self.limits.max_dependency_units
                    ));
                    Resolution::Incomplete {
                        reason: format!(
                            "dependency unit limit ({}) reached",
                            self.limits.max_dependency_units
                        ),
                        candidates: Vec::new(),
                    }
                } else {
                    self.resolve_unit_result(
                        UnitResolveRequest {
                            requested_name: &site.requested_name,
                            importer_path: &importer.source.path,
                            legacy_route: importer_route.as_ref(),
                        },
                        cancel,
                    )?
                };
            let (target, authorized_qualifiers) = match outcome {
                Resolution::Found(unit) => {
                    let target = unit.source.id.clone();
                    let dependency_allowed = self.project_unit_ids.as_ref().is_none_or(|ids| {
                        ids.contains(&target)
                            || ids.len().saturating_sub(1) < self.limits.max_dependency_units
                    });
                    if !dependency_allowed {
                        complete = false;
                        self.mark_incomplete(format!(
                            "dependency unit limit ({}) reached",
                            self.limits.max_dependency_units
                        ));
                        (
                            ResolutionTarget::Incomplete,
                            qualifiers_for_unresolved(
                                &site.requested_name,
                                &aliased_name(&self.context, &site.requested_name),
                            ),
                        )
                    } else {
                        if self.project_unit_ids.is_none() && dependency_ids.insert(target.clone())
                        {
                            dependencies.push(unit.clone());
                        } else if self
                            .project_unit_ids
                            .as_mut()
                            .is_some_and(|ids| ids.insert(target.clone()))
                        {
                            dependency_ids.insert(target.clone());
                            dependencies.push(unit.clone());
                        }
                        (
                            ResolutionTarget::Found(target),
                            qualifiers_for_found(
                                &site.requested_name,
                                &aliased_name(&self.context, &site.requested_name),
                                &unit.declared_name,
                            ),
                        )
                    }
                }
                Resolution::Unavailable { .. } => {
                    if !inactive {
                        complete = false;
                    }
                    (
                        ResolutionTarget::Unavailable,
                        qualifiers_for_unresolved(
                            &site.requested_name,
                            &aliased_name(&self.context, &site.requested_name),
                        ),
                    )
                }
                Resolution::Ambiguous { .. } => {
                    complete = false;
                    (
                        ResolutionTarget::Ambiguous,
                        qualifiers_for_unresolved(
                            &site.requested_name,
                            &aliased_name(&self.context, &site.requested_name),
                        ),
                    )
                }
                Resolution::Incomplete { .. } => {
                    complete = false;
                    (
                        ResolutionTarget::Incomplete,
                        qualifiers_for_unresolved(
                            &site.requested_name,
                            &aliased_name(&self.context, &site.requested_name),
                        ),
                    )
                }
            };
            bindings.push(ResolvedImport {
                importer_source_id: importer.source.id.clone(),
                site: site.clone(),
                target,
                authorized_qualifiers,
            });
        }
        self.check_cancel(cancel)?;
        if !complete {
            self.mark_incomplete("import resolution was incomplete".to_string());
        }
        Ok(ResolvedImports {
            bindings,
            dependencies,
            complete,
        })
    }

    pub fn resolve_include(
        &mut self,
        request: IncludeResolveRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> ResolutionOutcome<ResolvedSource> {
        let before_observations = self.report.observations.len();
        let before_warnings = self.report.warnings.len();
        let result = self.resolve_include_result(request, cancel);
        self.outcome(result, before_observations, before_warnings)
    }

    /// Result-preserving variant for callers that must distinguish
    /// cancellation from an ordinary incomplete include lookup.
    pub fn try_resolve_include(
        &mut self,
        request: IncludeResolveRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<ResolvedSource>, ResolverError> {
        let before_observations = self.report.observations.len();
        let before_warnings = self.report.warnings.len();
        let result = self.resolve_include_result(request, cancel)?;
        Ok(self.outcome(Ok(result), before_observations, before_warnings))
    }

    pub fn resolve_project(
        &mut self,
        root: UnitResolveRequest<'_>,
        sites: &[ImportSite],
        cancel: &dyn CancellationToken,
    ) -> Result<ResolvedProject, ResolverError> {
        let root = match self.resolve_unit_result(root, cancel)? {
            Resolution::Found(root) => root,
            Resolution::Unavailable { reason } | Resolution::Incomplete { reason, .. } => {
                return Err(ResolverError::InvalidRequest(format!(
                    "root unit unavailable: {reason}"
                )));
            }
            Resolution::Ambiguous { .. } => {
                return Err(ResolverError::InvalidRequest(
                    "root unit is ambiguous".to_string(),
                ));
            }
        };
        self.resolve_project_with_root(root, sites, cancel)
    }

    /// Walk a project from a caller-selected, authorized source path.
    ///
    /// The selected root is loaded through [`Self::load_source`] and is never
    /// sent through unit alias lookup or filename precedence.  Aliases and
    /// normal candidate precedence remain active for imports encountered while
    /// walking the project.  This distinction is required when a project alias
    /// happens to match the selected root's declared name.
    pub fn resolve_project_from_source(
        &mut self,
        path: &Path,
        requested_name: &str,
        sites: &[ImportSite],
        legacy_route: Option<&LegacyRoute>,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolvedProject, ResolverError> {
        self.check_cancel(cancel)?;
        let source = self.load_source(path, legacy_route, SourceKind::Unit, cancel)?;
        let metadata = parse_source_metadata(&source);
        let declared_name = metadata
            .declared_name
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| {
                ResolverError::InvalidRequest(format!(
                    "selected root {} has no unit/program declaration",
                    source.path.display()
                ))
            })?;
        let requested_name = requested_name.trim();
        if requested_name.is_empty() {
            return Err(ResolverError::InvalidRequest(
                "selected root name is empty".to_string(),
            ));
        }
        self.resolve_project_with_root(
            ResolvedUnit {
                requested_name: requested_name.to_string(),
                declared_name,
                source,
            },
            sites,
            cancel,
        )
    }

    fn resolve_project_with_root(
        &mut self,
        root: ResolvedUnit,
        sites: &[ImportSite],
        cancel: &dyn CancellationToken,
    ) -> Result<ResolvedProject, ResolverError> {
        let previous_project_unit_ids = self.project_unit_ids.take();
        let result = self.resolve_project_inner(root, sites, cancel);
        self.project_unit_ids = previous_project_unit_ids;
        result
    }

    fn resolve_project_inner(
        &mut self,
        root: ResolvedUnit,
        sites: &[ImportSite],
        cancel: &dyn CancellationToken,
    ) -> Result<ResolvedProject, ResolverError> {
        self.check_cancel(cancel)?;
        let root_sites = if sites.is_empty() {
            parse_source_metadata(&root.source).imports
        } else {
            sites.to_vec()
        };
        let mut imports = Vec::new();
        let mut units = Vec::new();
        let mut include_sources = Vec::new();
        let mut loaded_ids = HashSet::from([root.source.id.clone()]);
        self.project_unit_ids = Some(loaded_ids.clone());
        let mut queue = VecDeque::from([(root.clone(), root_sites)]);
        let mut complete = self.context.discovery_complete && self.report.complete;
        while let Some((importer, importer_sites)) = queue.pop_front() {
            self.check_cancel(cancel)?;
            let resolved = self.resolve_imports(&importer, &importer_sites, cancel)?;
            complete &= resolved.complete;
            imports.extend(resolved.bindings);
            for dependency in resolved.dependencies {
                if loaded_ids.insert(dependency.source.id.clone()) {
                    let dependency_sites = parse_source_metadata(&dependency.source).imports;
                    units.push(dependency.clone());
                    queue.push_back((dependency, dependency_sites));
                }
            }
            let include_route = self.legacy_routes.get(&importer.source.id).cloned();
            let conditional_context = self.context.effective_conditional_context();
            let mut conditional_environment =
                match crate::conditional::ConditionalEnvironment::try_from_context(
                    &conditional_context,
                ) {
                    Some(environment) => environment,
                    None => {
                        complete = false;
                        crate::conditional::ConditionalEnvironment::default()
                    }
                };
            let include_result = self.resolve_includes_for_source(
                &importer.source,
                &mut conditional_environment,
                include_route.as_ref(),
                cancel,
                0,
                &mut HashSet::new(),
            )?;
            complete &= include_result.complete;
            self.append_include_result(include_result, &mut complete, &mut include_sources);
        }

        let includes = self.take_project_includes();
        include_sources.retain(|source| !loaded_ids.contains(&source.id));
        let mut report = self.finish_report();
        report.complete &= complete;
        if !complete && report.incomplete_reasons.is_empty() {
            report
                .incomplete_reasons
                .push("project resolution was incomplete".to_string());
        }
        Ok(ResolvedProject {
            root,
            units,
            imports,
            includes,
            include_sources,
            complete,
            report,
        })
    }

    pub fn finish(self) -> ResolutionReport {
        self.report
    }

    /// Adjust the request-wide include budget before another include lookup.
    /// The limits are absolute counters, so callers that combine this session
    /// with an outer auditor can account for work performed by other contexts.
    pub fn set_include_limits(
        &mut self,
        max_files: usize,
        max_bytes: usize,
        max_directives: usize,
    ) {
        self.limits.max_include_files = max_files;
        self.limits.max_include_bytes = max_bytes;
        self.limits.max_include_directives = max_directives;
    }

    pub fn include_usage(&self) -> (usize, usize, usize) {
        (
            self.include_files,
            self.include_bytes,
            self.include_directives,
        )
    }

    pub fn report(&self) -> ResolutionReport {
        self.report.clone()
    }

    fn outcome<T>(
        &mut self,
        result: Result<Resolution<T>, ResolverError>,
        before_observations: usize,
        before_warnings: usize,
    ) -> ResolutionOutcome<T> {
        let result = match result {
            Ok(result) => {
                if matches!(&result, Resolution::Incomplete { .. }) {
                    self.mark_incomplete("resolution outcome was incomplete".to_string());
                }
                result
            }
            Err(error) => {
                let reason = error.to_string();
                self.mark_incomplete(reason.clone());
                Resolution::Incomplete {
                    reason,
                    candidates: Vec::new(),
                }
            }
        };
        ResolutionOutcome {
            result,
            observations: self.report.observations[before_observations..].to_vec(),
            warnings: self.report.warnings[before_warnings..].to_vec(),
        }
    }

    fn resolve_unit_result(
        &mut self,
        request: UnitResolveRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<Resolution<ResolvedUnit>, ResolverError> {
        self.check_cancel(cancel)?;
        let key = UnitCacheKey::new(
            request.importer_path,
            request.requested_name,
            request.legacy_route,
        );
        if let Some(unit) = self.unit_cache.get(&key).cloned() {
            return Ok(Resolution::Found(unit));
        }
        let route = request.legacy_route.cloned();
        let result = self.resolve_unit_inner(request, cancel)?;
        if matches!(&result, Resolution::Incomplete { .. }) {
            self.mark_incomplete("unit resolution was incomplete".to_string());
        }
        if let Resolution::Found(unit) = &result {
            self.unit_cache.insert(key, unit.clone());
            if let Some(route) = self.legacy_route_for_source(&unit.source, route.as_ref()) {
                self.legacy_routes.insert(unit.source.id.clone(), route);
            }
        }
        Ok(result)
    }

    fn resolve_include_result(
        &mut self,
        request: IncludeResolveRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<Resolution<ResolvedSource>, ResolverError> {
        let result = self.resolve_include_inner(request, cancel)?;
        if matches!(&result, Resolution::Incomplete { .. }) {
            self.mark_incomplete("include resolution was incomplete".to_string());
        }
        Ok(result)
    }

    fn check_cancel(&mut self, cancel: &dyn CancellationToken) -> Result<(), ResolverError> {
        if cancel.is_cancelled() {
            self.mark_incomplete("request cancelled".to_string());
            Err(ResolverError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn resolve_unit_inner(
        &mut self,
        request: UnitResolveRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<Resolution<ResolvedUnit>, ResolverError> {
        self.check_cancel(cancel)?;
        let requested = request.requested_name.trim();
        if requested.is_empty() {
            return Err(ResolverError::InvalidRequest(
                "unit name is empty".to_string(),
            ));
        }
        if !self.context.discovery_complete {
            self.mark_incomplete("project metadata is incomplete".to_string());
            return Ok(Resolution::Incomplete {
                reason: "project metadata is incomplete".to_string(),
                candidates: Vec::new(),
            });
        }
        let lookup = aliased_name(&self.context, requested);
        if let Some(entries) = self
            .context
            .explicit_unit_entries
            .get(&lookup.to_ascii_lowercase())
        {
            let group = CandidateGroup {
                paths: entries
                    .iter()
                    .map(|entry| (entry.path.clone(), entry.clone()))
                    .collect(),
                legacy_route: request.legacy_route.cloned(),
            };
            if let Some(result) =
                self.resolve_candidate_group(&group, requested, &lookup, cancel)?
            {
                return Ok(result);
            }
        } else if let Some(paths) = self
            .context
            .explicit_units
            .get(&lookup.to_ascii_lowercase())
        {
            let paths = paths
                .iter()
                .filter_map(|path| {
                    self.context
                        .path_entry_for(path)
                        .or_else(|| self.legacy_entry_for_path(path, request.legacy_route))
                        .map(|entry| (path.clone(), entry))
                })
                .collect();
            let group = CandidateGroup {
                paths,
                legacy_route: request.legacy_route.cloned(),
            };
            if let Some(result) =
                self.resolve_candidate_group(&group, requested, &lookup, cancel)?
            {
                return Ok(result);
            }
        }

        if let Some(directory) = request.importer_path.parent() {
            if let Some(result) = self.resolve_directory(
                directory,
                &lookup,
                &self.context.unit_namespaces.clone(),
                request.legacy_route,
                cancel,
                None,
                requested,
            )? {
                return Ok(result);
            }
        }
        let search_entries = if self.context.search_path_entries.is_empty() {
            self.context
                .search_paths
                .iter()
                .filter_map(|path| self.context.path_entry_for(path))
                .collect::<Vec<_>>()
        } else {
            self.context.search_path_entries.clone()
        };
        for entry in search_entries {
            if let Some(result) = self.resolve_directory(
                &entry.path,
                &lookup,
                &self.context.unit_namespaces.clone(),
                request.legacy_route,
                cancel,
                Some(&entry),
                requested,
            )? {
                return Ok(result);
            }
        }

        if self.context.project_file.is_none() {
            let catalogue_groups = self.filename_catalogue_candidates(&lookup, cancel)?;
            for group in catalogue_groups {
                if let Some(result) =
                    self.resolve_candidate_group(&group, requested, &lookup, cancel)?
                {
                    return Ok(result);
                }
            }
        }

        if !self.context.packages.is_empty() {
            self.package_lookup_for_unit(requested, &lookup, request.legacy_route, cancel)
        } else {
            Ok(Resolution::Unavailable {
                reason: format!("no authorized unit candidate for {requested}"),
            })
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_directory(
        &mut self,
        directory: &Path,
        unit_name: &str,
        namespaces: &[String],
        legacy_route: Option<&LegacyRoute>,
        cancel: &dyn CancellationToken,
        provided_entry: Option<&ProjectPathEntry>,
        requested: &str,
    ) -> Result<Option<Resolution<ResolvedUnit>>, ResolverError> {
        let directory = canonical_path(directory);
        let Some(entry) = provided_entry
            .cloned()
            .or_else(|| self.context.path_entry_for(&directory))
            .or_else(|| self.legacy_entry_for_directory(&directory, legacy_route))
        else {
            return Ok(None);
        };
        let entry = ProjectPathEntry {
            path: directory.clone(),
            provenance: entry.provenance,
        };
        let listing = self.list_directory(&directory, &entry, legacy_route, cancel, false)?;
        if !listing.complete {
            return Ok(Some(Resolution::Incomplete {
                reason: format!(
                    "directory scan under {} was incomplete",
                    directory.display()
                ),
                candidates: Vec::new(),
            }));
        }
        let tiers = filename_tiers(unit_name, namespaces);
        for names in tiers {
            let paths = self.paths_for_names(&directory, &entry, &names, &listing);
            let group = CandidateGroup {
                paths,
                legacy_route: legacy_route.cloned(),
            };
            if let Some(result) =
                self.resolve_candidate_group(&group, requested, unit_name, cancel)?
            {
                return Ok(Some(result));
            }
        }
        if unit_name.contains('.') {
            let parts = unit_name
                .split('.')
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>();
            if parts.len() > 1 {
                let nested = parts[..parts.len() - 1]
                    .iter()
                    .fold(directory.clone(), |path, part| path.join(part));
                let nested = match self.resolve_case_insensitive_path(
                    &directory,
                    &nested,
                    &entry,
                    legacy_route,
                    cancel,
                )? {
                    CaseInsensitiveLookup::Found(nested) => Some(nested),
                    CaseInsensitiveLookup::Missing => None,
                    CaseInsensitiveLookup::Ambiguous(paths) => {
                        return Ok(Some(Resolution::Ambiguous {
                            candidates: paths
                                .into_iter()
                                .map(|path| ResolutionCandidate {
                                    path,
                                    declared_name: None,
                                })
                                .collect(),
                        }));
                    }
                    CaseInsensitiveLookup::Incomplete => {
                        return Ok(Some(Resolution::Incomplete {
                            reason: "case-insensitive path lookup was incomplete".to_string(),
                            candidates: Vec::new(),
                        }));
                    }
                };
                if let Some(nested) = nested {
                    let nested_entry = self.candidate_entry_for_path(&nested, &entry, None);
                    let nested_listing =
                        self.list_directory(&nested, &nested_entry, legacy_route, cancel, false)?;
                    if !nested_listing.complete {
                        return Ok(Some(Resolution::Incomplete {
                            reason: format!(
                                "directory scan under {} was incomplete",
                                nested.display()
                            ),
                            candidates: Vec::new(),
                        }));
                    }
                    for names in filename_tiers(parts.last().copied().unwrap_or_default(), &[]) {
                        let group = CandidateGroup {
                            paths: self.paths_for_names(
                                &nested,
                                &nested_entry,
                                &names,
                                &nested_listing,
                            ),
                            legacy_route: legacy_route.cloned(),
                        };
                        if let Some(result) =
                            self.resolve_candidate_group(&group, requested, unit_name, cancel)?
                        {
                            return Ok(Some(result));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    fn legacy_entry_for_path(
        &self,
        path: &Path,
        route: Option<&LegacyRoute>,
    ) -> Option<ProjectPathEntry> {
        route
            .filter(|route| legacy_route_authorizes(path, route))
            .map(|_| ProjectPathEntry::legacy(canonical_path(path)))
    }

    fn legacy_route_for_source(
        &self,
        source: &LoadedSource,
        inherited: Option<&LegacyRoute>,
    ) -> Option<LegacyRoute> {
        let source_is_legacy = match &source.revision {
            SourceRevision::Disk { path_entry, .. } => {
                matches!(path_entry.provenance, ProjectPathProvenance::LegacyNative)
            }
            SourceRevision::Overlay { .. } => {
                self.context
                    .path_entry_for(&source.path)
                    .is_some_and(|entry| {
                        matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
                    })
                    || inherited.is_some_and(|route| legacy_route_authorizes(&source.path, route))
            }
        };
        source_is_legacy.then(|| {
            let sibling_directory = source.path.parent().unwrap_or(&source.path).to_path_buf();
            LegacyRoute {
                source_path: source.path.clone(),
                sibling_directory,
            }
        })
    }

    fn legacy_entry_for_directory(
        &self,
        directory: &Path,
        route: Option<&LegacyRoute>,
    ) -> Option<ProjectPathEntry> {
        route
            .filter(|route| legacy_route_authorizes_directory(directory, route))
            .map(|_| ProjectPathEntry::legacy(canonical_path(directory)))
    }

    fn candidate_entry_for_path(
        &self,
        path: &Path,
        inherited: &ProjectPathEntry,
        preferred_provenance: Option<&ProjectPathProvenance>,
    ) -> ProjectPathEntry {
        let path = canonical_path(path);
        if let Some(entry) = self.exact_context_entry_for_path(&path) {
            return ProjectPathEntry {
                path,
                provenance: entry.provenance,
            };
        }
        ProjectPathEntry {
            path,
            provenance: preferred_provenance
                .cloned()
                .unwrap_or_else(|| inherited.provenance.clone()),
        }
    }

    fn exact_context_entry_for_path(&self, path: &Path) -> Option<ProjectPathEntry> {
        self.context
            .main_source_entry
            .iter()
            .chain(self.context.explicit_unit_entries.values().flatten())
            .find(|entry| path_equivalent(&entry.path, path))
            .cloned()
    }

    fn resolve_candidate_group(
        &mut self,
        group: &CandidateGroup,
        requested: &str,
        lookup: &str,
        cancel: &dyn CancellationToken,
    ) -> Result<Option<Resolution<ResolvedUnit>>, ResolverError> {
        self.check_cancel(cancel)?;
        let mut valid = Vec::new();
        let mut candidates = Vec::new();
        for (path, entry) in dedup_paths(group.paths.clone()) {
            self.check_cancel(cancel)?;
            match self.load_candidate(
                &path,
                &entry,
                group.legacy_route.as_ref(),
                SourceKind::Unit,
                cancel,
            ) {
                Ok(source) => {
                    let metadata = parse_source_metadata(&source);
                    let candidate = ResolutionCandidate {
                        path: source.path.clone(),
                        declared_name: metadata.declared_name.clone(),
                    };
                    if declared_name_matches(
                        metadata.declared_name.as_deref(),
                        requested,
                        lookup,
                        &self.context,
                    ) {
                        valid.push((source, candidate));
                    } else {
                        candidates.push(candidate);
                    }
                }
                Err(SourceStoreError::NotFound { path: missing }) => {
                    self.record_candidate(&missing, Some(entry.clone()), false);
                }
                Err(SourceStoreError::Unauthorized { path, reason }) => {
                    self.warn(format!(
                        "unit candidate {} is unauthorized: {reason}",
                        path.display()
                    ));
                    self.mark_incomplete(format!(
                        "unit candidate {} is unauthorized",
                        path.display()
                    ));
                    return Ok(Some(Resolution::Incomplete {
                        reason: "unit candidate is unauthorized".to_string(),
                        candidates,
                    }));
                }
                Err(SourceStoreError::Cancelled) => return Err(ResolverError::Cancelled),
                Err(SourceStoreError::Incomplete { path, reason }) => {
                    self.mark_incomplete(format!(
                        "unit candidate {} is incomplete: {reason}",
                        path.display()
                    ));
                    return Ok(Some(Resolution::Incomplete { reason, candidates }));
                }
                Err(error) => {
                    let reason = format!("unit candidate could not be loaded: {error:?}");
                    self.mark_incomplete(reason.clone());
                    return Ok(Some(Resolution::Incomplete { reason, candidates }));
                }
            }
        }
        match valid.len() {
            0 => Ok(None),
            1 => {
                let (source, candidate) = valid.pop().expect("one valid candidate");
                Ok(Some(Resolution::Found(ResolvedUnit {
                    requested_name: requested.to_string(),
                    declared_name: candidate
                        .declared_name
                        .unwrap_or_else(|| lookup.to_string()),
                    source,
                })))
            }
            _ => {
                valid.sort_by(|left, right| path_key(&left.0.path).cmp(&path_key(&right.0.path)));
                let candidates = valid.into_iter().map(|(_, candidate)| candidate).collect();
                self.warn(format!(
                    "ambiguous unit {requested}: multiple valid candidates"
                ));
                Ok(Some(Resolution::Ambiguous { candidates }))
            }
        }
    }

    fn paths_for_names(
        &self,
        directory: &Path,
        entry: &ProjectPathEntry,
        names: &[String],
        listing: &DirectoryListing,
    ) -> Vec<(PathBuf, ProjectPathEntry)> {
        let mut paths = listing
            .files
            .iter()
            .filter(|path| {
                path.file_name().is_some_and(|file_name| {
                    names
                        .iter()
                        .any(|name| file_name.to_string_lossy().eq_ignore_ascii_case(name))
                })
            })
            .map(|path| {
                (
                    path.clone(),
                    self.candidate_entry_for_path(path, entry, None),
                )
            })
            .collect::<Vec<_>>();
        let overlays = self
            .store
            .overlay_candidates(&[directory.to_path_buf()], names);
        for path in overlays {
            if path
                .parent()
                .is_none_or(|parent| !path_equivalent(parent, directory))
            {
                continue;
            }
            if !paths
                .iter()
                .any(|(existing, _)| path_equivalent(existing, &path))
            {
                paths.push((
                    path.clone(),
                    self.candidate_entry_for_path(&path, entry, None),
                ));
            }
        }
        paths.sort_by(|left, right| path_key(&left.0).cmp(&path_key(&right.0)));
        paths
    }

    fn list_directory(
        &mut self,
        directory: &Path,
        entry: &ProjectPathEntry,
        _legacy_route: Option<&LegacyRoute>,
        cancel: &dyn CancellationToken,
        missing_is_incomplete: bool,
    ) -> Result<DirectoryListing, ResolverError> {
        self.check_cancel(cancel)?;
        let key = canonical_path(directory);
        let cache_key = DirectoryCacheKey {
            path: key.clone(),
            entry: entry.clone(),
            read_policy: self.context.read_policy.clone(),
            missing_is_incomplete,
        };
        if let Some(listing) = self.directories.get(&cache_key).cloned() {
            return Ok(listing);
        }
        let listing = match self.store.list_directory(
            DirectoryRequest {
                directory: &key,
                entry,
                read_policy: &self.context.read_policy,
            },
            cancel,
        ) {
            Ok(listing) => listing,
            Err(SourceStoreError::NotFound { path: _ }) => DirectoryListing {
                files: Vec::new(),
                directories: Vec::new(),
                stamp: None,
                complete: !missing_is_incomplete,
            },
            Err(SourceStoreError::Cancelled) => return Err(ResolverError::Cancelled),
            Err(error) => return Err(ResolverError::SourceStore(error)),
        };
        self.scanned_entries = self.scanned_entries.saturating_add(
            listing
                .files
                .len()
                .saturating_add(listing.directories.len()),
        );
        self.scanned_bytes = self.scanned_bytes.saturating_add(
            listing
                .files
                .iter()
                .filter_map(|path| fs::symlink_metadata(path).ok())
                .filter(|metadata| !metadata.file_type().is_symlink())
                .map(|metadata| metadata.len() as usize)
                .sum::<usize>(),
        );
        let complete = listing.complete
            && self.scanned_entries <= self.limits.max_directory_entries
            && self.scanned_bytes <= self.limits.max_scanned_bytes;
        let listing = DirectoryListing {
            complete,
            ..listing
        };
        self.record_observation(ResolutionObservation::Directory {
            path: key.clone(),
            entry: entry.clone(),
            stamp: listing.stamp.clone(),
            complete,
        });
        if !complete {
            self.mark_incomplete(format!(
                "directory scan limit or traversal failure under {}",
                key.display()
            ));
        }
        self.directories.insert(cache_key, listing.clone());
        Ok(listing)
    }

    fn load_candidate(
        &mut self,
        path: &Path,
        entry: &ProjectPathEntry,
        legacy_route: Option<&LegacyRoute>,
        kind: SourceKind,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        self.load_candidate_with_limit(
            path,
            entry,
            legacy_route,
            kind,
            self.limits.max_source_bytes,
            cancel,
        )
    }

    fn load_candidate_with_limit(
        &mut self,
        path: &Path,
        entry: &ProjectPathEntry,
        legacy_route: Option<&LegacyRoute>,
        kind: SourceKind,
        max_bytes: usize,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        self.check_cancel(cancel)
            .map_err(|_| SourceStoreError::Cancelled)?;
        let path = canonical_path(path);
        let entry = ProjectPathEntry {
            path: path.clone(),
            provenance: entry.provenance.clone(),
        };
        let stamp = if self.context.read_policy.allows_location(&entry) {
            path_stamp_result(&path).ok().flatten()
        } else {
            None
        };
        self.record_candidate(&path, Some(entry.clone()), stamp.is_some());
        let loaded = self.store.load(
            SourceRequest {
                path: &path,
                entry: &entry,
                read_policy: &self.context.read_policy,
                legacy_route,
                kind,
                max_bytes,
            },
            cancel,
        )?;
        if stamp.is_none() {
            self.record_candidate(&path, Some(entry.clone()), true);
        }
        self.loaded.insert(loaded.id.clone(), loaded.clone());
        self.record_observation(ResolutionObservation::Payload {
            source_id: loaded.id.clone(),
            path: loaded.path.clone(),
            revision: loaded.revision.clone(),
        });
        Ok(loaded)
    }

    fn record_candidate(&mut self, path: &Path, entry: Option<ProjectPathEntry>, present: bool) {
        let stamp = entry
            .as_ref()
            .filter(|entry| self.context.read_policy.allows_location(entry))
            .and_then(|_| path_stamp_result(path).ok().flatten());
        self.record_observation(ResolutionObservation::Candidate {
            path: path.to_path_buf(),
            entry,
            stamp,
            present,
        });
    }

    fn resolve_include_inner(
        &mut self,
        request: IncludeResolveRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<Resolution<ResolvedSource>, ResolverError> {
        self.check_cancel(cancel)?;
        if request.requested_name.trim().is_empty() {
            return Ok(Resolution::Incomplete {
                reason: "include directive has no file name".to_string(),
                candidates: Vec::new(),
            });
        }
        let mut entries = self.context.include_search_entries(request.including_path);
        if let Some(owner_directory) = request.including_path.parent() {
            if let Some(owner_entry) =
                self.legacy_entry_for_directory(owner_directory, request.legacy_route)
            {
                if !entries
                    .iter()
                    .any(|entry| path_equivalent(&entry.path, &owner_entry.path))
                {
                    entries.insert(0, owner_entry);
                }
            }
        }
        let mut candidates = Vec::new();
        let mut saw_incomplete = false;
        for entry in entries {
            self.check_cancel(cancel)?;
            self.record_include_search_directory(&entry);
            let resolved = match self
                .context
                .overrides
                .resolve_path(request.requested_name, &entry.path)
            {
                Ok(resolved) => resolved,
                Err(error) => {
                    self.warn(format!(
                        "include {} could not be mapped: {error}",
                        request.requested_name
                    ));
                    saw_incomplete = true;
                    continue;
                }
            };
            let path = canonical_path(&resolved.path);
            let mapped_provenance =
                resolved
                    .mapping
                    .as_ref()
                    .map(|mapping| ProjectPathProvenance::Mapped {
                        root: canonical_path(&mapping.to),
                    });
            let candidate_entry =
                self.candidate_entry_for_path(&path, &entry, mapped_provenance.as_ref());
            let case_base = resolved
                .mapping
                .as_ref()
                .map(|mapping| canonical_path(&mapping.to))
                .unwrap_or_else(|| canonical_path(&entry.path));
            let case_lookup_base = self.authorized_case_lookup_base(
                &case_base,
                &path,
                &candidate_entry,
                request.legacy_route,
            );
            match self.load_include_candidate(&path, &candidate_entry, request.legacy_route, cancel)
            {
                Ok(source) => candidates.push(source),
                Err(SourceStoreError::NotFound { .. }) => {
                    let Some((case_base, case_base_entry)) = case_lookup_base else {
                        continue;
                    };
                    match self.resolve_case_insensitive_path(
                        &case_base,
                        &path,
                        &case_base_entry,
                        request.legacy_route,
                        cancel,
                    )? {
                        CaseInsensitiveLookup::Found(actual) => {
                            let actual_entry =
                                self.candidate_entry_for_path(&actual, &candidate_entry, None);
                            match self.load_include_candidate(
                                &actual,
                                &actual_entry,
                                request.legacy_route,
                                cancel,
                            ) {
                                Ok(source) => candidates.push(source),
                                Err(SourceStoreError::NotFound { .. }) => {}
                                Err(SourceStoreError::Cancelled) => {
                                    return Err(ResolverError::Cancelled);
                                }
                                Err(error) => {
                                    saw_incomplete = true;
                                    self.warn(format!(
                                        "include candidate could not be loaded: {error:?}"
                                    ));
                                    break;
                                }
                            }
                        }
                        CaseInsensitiveLookup::Missing => {}
                        CaseInsensitiveLookup::Ambiguous(paths) => {
                            saw_incomplete = true;
                            self.warn(format!(
                                "include {} has ambiguous case-insensitive path components: {}",
                                request.requested_name,
                                paths
                                    .iter()
                                    .map(|path| path.display().to_string())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ));
                            break;
                        }
                        CaseInsensitiveLookup::Incomplete => {
                            saw_incomplete = true;
                            break;
                        }
                    }
                }
                Err(SourceStoreError::Cancelled) => return Err(ResolverError::Cancelled),
                Err(error) => {
                    saw_incomplete = true;
                    self.warn(format!("include candidate could not be loaded: {error:?}"));
                    break;
                }
            }
            if !candidates.is_empty() {
                break;
            }
        }
        candidates.sort_by(|left, right| path_key(&left.path).cmp(&path_key(&right.path)));
        candidates.dedup_by(|left, right| left.id == right.id);
        match candidates.len() {
            1 => Ok(Resolution::Found(
                candidates.pop().expect("one include candidate"),
            )),
            0 if saw_incomplete => {
                self.mark_incomplete(format!(
                    "include {} could not be resolved completely",
                    request.requested_name
                ));
                Ok(Resolution::Incomplete {
                    reason: format!("include {} lookup was incomplete", request.requested_name),
                    candidates: Vec::new(),
                })
            }
            0 => Ok(Resolution::Unavailable {
                reason: format!("include {} was not found", request.requested_name),
            }),
            _ => Ok(Resolution::Ambiguous {
                candidates: candidates
                    .into_iter()
                    .map(|source| ResolutionCandidate {
                        path: source.path,
                        declared_name: None,
                    })
                    .collect(),
            }),
        }
    }

    fn record_include_search_directory(&mut self, entry: &ProjectPathEntry) {
        self.record_observation(ResolutionObservation::Directory {
            // Keep the configured spelling instead of canonicalizing through a
            // symlink.  Revalidation must detect changes to the search-path
            // link itself as well as changes to its target.
            path: entry.path.clone(),
            entry: entry.clone(),
            stamp: path_stamp_result(&entry.path).ok().flatten(),
            complete: true,
        });
    }

    fn load_include_candidate(
        &mut self,
        path: &Path,
        entry: &ProjectPathEntry,
        legacy_route: Option<&LegacyRoute>,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        let source_id = source_id_for_path(path);
        let existing = self
            .loaded
            .values()
            .find(|source| {
                path_equivalent(&source.path, path)
                    && self.can_reuse_loaded_source(path, entry, legacy_route)
            })
            .cloned();
        let known_id = existing
            .as_ref()
            .map_or_else(|| source_id.clone(), |source| source.id.clone());
        let is_new = !self.include_seen.contains(&known_id);
        if is_new && self.include_files >= self.limits.max_include_files {
            return Err(SourceStoreError::Incomplete {
                path: canonical_path(path),
                reason: format!(
                    "include file limit ({}) reached",
                    self.limits.max_include_files
                ),
            });
        }
        let remaining_bytes = self
            .limits
            .max_include_bytes
            .saturating_sub(self.include_bytes);
        if is_new && remaining_bytes == 0 {
            return Err(SourceStoreError::Incomplete {
                path: canonical_path(path),
                reason: format!(
                    "include byte limit ({}) reached",
                    self.limits.max_include_bytes
                ),
            });
        }
        if let Some(source) = existing {
            if is_new {
                if source.bytes.len() > remaining_bytes {
                    return Err(SourceStoreError::Incomplete {
                        path: source.path,
                        reason: format!(
                            "include byte limit ({}) exceeded",
                            self.limits.max_include_bytes
                        ),
                    });
                }
                self.include_seen.insert(source.id.clone());
                self.include_files = self.include_files.saturating_add(1);
                self.include_bytes = self.include_bytes.saturating_add(source.bytes.len());
            }
            return Ok(source);
        }
        let max_bytes = if is_new {
            self.limits.max_source_bytes.min(remaining_bytes)
        } else {
            self.limits.max_source_bytes
        };
        let source = self.load_candidate_with_limit(
            path,
            entry,
            legacy_route,
            SourceKind::Include,
            max_bytes,
            cancel,
        )?;
        if is_new {
            self.include_seen.insert(source.id.clone());
            self.include_files = self.include_files.saturating_add(1);
            self.include_bytes = self.include_bytes.saturating_add(source.bytes.len());
            if self.include_bytes > self.limits.max_include_bytes {
                return Err(SourceStoreError::Incomplete {
                    path: source.path,
                    reason: format!(
                        "include byte limit ({}) exceeded",
                        self.limits.max_include_bytes
                    ),
                });
            }
        }
        Ok(source)
    }

    fn can_reuse_loaded_source(
        &self,
        path: &Path,
        entry: &ProjectPathEntry,
        legacy_route: Option<&LegacyRoute>,
    ) -> bool {
        let path = canonical_path(path);
        if !path_equivalent(&path, &entry.path) {
            return false;
        }
        let legacy = legacy_route.is_some_and(|route| legacy_route_authorizes(&path, route))
            && matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
            && self.context.read_policy.allows_legacy_route_entry(entry);
        legacy || self.context.read_policy.allows_entry(entry)
    }

    fn authorized_case_lookup_base(
        &self,
        base: &Path,
        target: &Path,
        fallback_entry: &ProjectPathEntry,
        legacy_route: Option<&LegacyRoute>,
    ) -> Option<(PathBuf, ProjectPathEntry)> {
        let target = canonical_path(target);
        let mut candidate = canonical_path(base);
        loop {
            if path_starts_with(&target, &candidate) {
                let entry = self
                    .context
                    .path_entry_for(&candidate)
                    .or_else(|| self.legacy_entry_for_directory(&candidate, legacy_route))
                    .or_else(|| {
                        path_equivalent(&candidate, base).then(|| ProjectPathEntry {
                            path: candidate.clone(),
                            provenance: fallback_entry.provenance.clone(),
                        })
                    });
                if let Some(entry) = entry {
                    let entry = ProjectPathEntry {
                        path: candidate.clone(),
                        provenance: entry.provenance,
                    };
                    if self.context.read_policy.allows_location(&entry) {
                        return Some((candidate, entry));
                    }
                }
            }
            if !candidate.pop() {
                break;
            }
        }
        None
    }

    fn resolve_case_insensitive_path(
        &mut self,
        base: &Path,
        path: &Path,
        base_entry: &ProjectPathEntry,
        _legacy_route: Option<&LegacyRoute>,
        cancel: &dyn CancellationToken,
    ) -> Result<CaseInsensitiveLookup, ResolverError> {
        let base = canonical_path(base);
        let path = canonical_path(path);
        let Some(relative) = relative_components(&base, &path) else {
            return Ok(CaseInsensitiveLookup::Missing);
        };
        let mut current = base;
        for wanted in relative {
            self.check_cancel(cancel)?;
            let current_entry = ProjectPathEntry {
                path: current.clone(),
                provenance: base_entry.provenance.clone(),
            };
            let listing =
                self.list_directory(&current, &current_entry, _legacy_route, cancel, false)?;
            let mut matches = listing
                .files
                .iter()
                .chain(listing.directories.iter())
                .filter(|candidate| {
                    candidate.file_name().is_some_and(|name| {
                        name.to_string_lossy()
                            .eq_ignore_ascii_case(&wanted.to_string_lossy())
                    })
                })
                .cloned()
                .collect::<Vec<_>>();
            matches.sort_by_key(|left| path_key(left));
            if !listing.complete {
                self.mark_incomplete(format!(
                    "case-insensitive lookup under {} was incomplete",
                    current.display()
                ));
                return Ok(CaseInsensitiveLookup::Incomplete);
            }
            if let Some(exact) = matches
                .iter()
                .find(|candidate| candidate.file_name().is_some_and(|name| name == wanted))
            {
                current = exact.clone();
            } else if matches.len() == 1 {
                current = matches.pop().expect("one path match");
            } else if matches.len() > 1 {
                return Ok(CaseInsensitiveLookup::Ambiguous(matches));
            } else {
                return Ok(CaseInsensitiveLookup::Missing);
            }
        }
        Ok(CaseInsensitiveLookup::Found(current))
    }

    fn filename_catalogue_candidates(
        &mut self,
        unit_name: &str,
        cancel: &dyn CancellationToken,
    ) -> Result<Vec<CandidateGroup>, ResolverError> {
        let tiers = filename_tiers(unit_name, &self.context.unit_namespaces);
        let mut paths_by_tier: Vec<Vec<(PathBuf, ProjectPathEntry)>> =
            vec![Vec::new(); tiers.len()];
        // Projectless lookup is deliberately rooted at the caller-supplied
        // workspace roots.  Reusing the context's ordered unit paths here
        // would turn a configured search path into a recursive project-wide
        // catalogue and would change the precedence boundary.
        let roots = self.workspace_roots.clone();
        for root in roots {
            let catalogue = self.catalogue(&root, cancel)?;
            if !catalogue.complete {
                return Err(ResolverError::LimitExceeded {
                    limit: "filename catalogue",
                    observed: self.scanned_entries,
                    maximum: self.limits.max_package_catalogue_entries,
                });
            }
            for (index, names) in tiers.iter().enumerate() {
                for name in names {
                    if let Some(found) = catalogue.entries.get(&name.to_ascii_lowercase()) {
                        paths_by_tier[index].extend(found.iter().filter_map(|path| {
                            self.context
                                .path_entry_for(path)
                                .map(|entry| (path.clone(), entry))
                        }));
                    }
                }
            }
            for (index, names) in tiers.iter().enumerate() {
                let overlays = self
                    .store
                    .overlay_candidates(std::slice::from_ref(&root), names);
                for path in overlays {
                    let Some(entry) = self.context.path_entry_for(&path) else {
                        continue;
                    };
                    if !paths_by_tier[index]
                        .iter()
                        .any(|(existing, _)| path_equivalent(existing, &path))
                    {
                        paths_by_tier[index].push((path, entry));
                    }
                }
            }
        }
        Ok(paths_by_tier
            .into_iter()
            .map(|paths| CandidateGroup {
                paths,
                legacy_route: None,
            })
            .collect())
    }

    fn catalogue(
        &mut self,
        root: &Path,
        cancel: &dyn CancellationToken,
    ) -> Result<Catalogue, ResolverError> {
        let root = canonical_path(root);
        if let Some(catalogue) = self.package_catalogues.get(&root).cloned() {
            return Ok(catalogue);
        }
        if self.package_catalogue_count >= self.limits.max_package_catalogues {
            self.mark_incomplete(format!(
                "package/source catalogue limit ({}) reached",
                self.limits.max_package_catalogues
            ));
            return Ok(Catalogue {
                entries: HashMap::new(),
                complete: false,
            });
        }
        self.package_catalogue_count += 1;
        let mut queue = VecDeque::from([root.clone()]);
        let mut visited = HashSet::new();
        let mut entries = HashMap::new();
        let mut complete = true;
        let mut catalogue_entries = 0usize;
        while let Some(directory) = queue.pop_front() {
            self.check_cancel(cancel)?;
            if !visited.insert(directory.clone()) {
                continue;
            }
            let Some(entry) = self.context.path_entry_for(&directory) else {
                complete = false;
                self.mark_incomplete(format!(
                    "catalogue directory {} is outside the requester read policy",
                    directory.display()
                ));
                break;
            };
            let listing = self.list_directory(&directory, &entry, None, cancel, true)?;
            complete &= listing.complete;
            catalogue_entries = catalogue_entries.saturating_add(
                listing
                    .directories
                    .len()
                    .saturating_add(listing.files.len()),
            );
            if catalogue_entries > self.limits.max_package_catalogue_entries {
                complete = false;
                break;
            }
            for child in listing.directories {
                let Some(child_entry) = self.context.path_entry_for(&child) else {
                    continue;
                };
                if self.context.read_policy.allows_location(&child_entry) {
                    queue.push_back(child);
                }
            }
            for file in listing.files {
                let Some(file_entry) = self.context.path_entry_for(&file) else {
                    continue;
                };
                if !self.context.read_policy.allows_location(&file_entry) {
                    continue;
                }
                if file.extension().is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("pas")
                        || extension.eq_ignore_ascii_case("dpk")
                        || extension.eq_ignore_ascii_case("dproj")
                }) {
                    let key = file
                        .file_name()
                        .map(|name| name.to_string_lossy().to_ascii_lowercase())
                        .unwrap_or_default();
                    entries.entry(key).or_insert_with(Vec::new).push(file);
                }
            }
            if !complete {
                break;
            }
        }
        let catalogue = Catalogue { entries, complete };
        self.package_catalogues.insert(root, catalogue.clone());
        Ok(catalogue)
    }

    fn package_lookup_for_unit(
        &mut self,
        requested: &str,
        lookup: &str,
        legacy_route: Option<&LegacyRoute>,
        cancel: &dyn CancellationToken,
    ) -> Result<Resolution<ResolvedUnit>, ResolverError> {
        if self.package_lookups >= self.limits.max_package_lookups {
            self.mark_incomplete(format!(
                "package lookup limit ({}) reached",
                self.limits.max_package_lookups
            ));
            return Ok(Resolution::Incomplete {
                reason: "package lookup limit reached".to_string(),
                candidates: Vec::new(),
            });
        }
        if self.context.packages.len() > self.limits.max_package_lookups {
            self.mark_incomplete(format!(
                "named package lookup limit ({}) reached",
                self.limits.max_package_lookups
            ));
            return Ok(Resolution::Incomplete {
                reason: "named package lookup limit reached".to_string(),
                candidates: Vec::new(),
            });
        }
        self.package_lookups += 1;
        let mut package_candidates = Vec::new();
        for package in self.context.packages.clone() {
            self.check_cancel(cancel)?;
            let package_key = package_stem(&package);
            let mut dpk_descriptors = Vec::new();
            let mut dproj_descriptors = Vec::new();
            for root in self.package_roots() {
                let catalogue = self.catalogue(&root, cancel)?;
                let dpk_key = format!("{package_key}.dpk");
                let dproj_key = format!("{package_key}.dproj");
                if let Some(paths) = catalogue.entries.get(&dpk_key) {
                    dpk_descriptors.extend(paths.iter().cloned());
                }
                if let Some(paths) = catalogue.entries.get(&dproj_key) {
                    dproj_descriptors.extend(paths.iter().cloned());
                }
                if !catalogue.complete {
                    return Ok(Resolution::Incomplete {
                        reason: format!("package catalogue for {package} is incomplete"),
                        candidates: Vec::new(),
                    });
                }
            }
            let mut descriptors = if dpk_descriptors.is_empty() {
                dproj_descriptors
            } else {
                dpk_descriptors
            };
            descriptors.sort_by_key(|left| path_key(left));
            descriptors.dedup_by(|left, right| path_equivalent(left, right));
            if descriptors.is_empty() {
                self.warn(format!(
                    "source for package {package} was not found under configured workspace/source roots; compiled-only package skipped"
                ));
                continue;
            }
            if descriptors.len() > 1 {
                self.warn(format!(
                    "ambiguous package {package}; matching source descriptors were found"
                ));
                return Ok(Resolution::Ambiguous {
                    candidates: descriptors
                        .into_iter()
                        .map(|path| ResolutionCandidate {
                            path,
                            declared_name: None,
                        })
                        .collect(),
                });
            }
            let descriptor = descriptors.pop().expect("one descriptor");
            self.check_cancel(cancel)?;
            let Some(entry) = self.context.path_entry_for(&descriptor) else {
                self.mark_incomplete(format!(
                    "package descriptor {} is outside the requester read policy",
                    descriptor.display()
                ));
                return Ok(Resolution::Incomplete {
                    reason: "package descriptor is outside the requester read policy".to_string(),
                    candidates: Vec::new(),
                });
            };
            self.record_candidate(&descriptor, Some(entry.clone()), true);
            let package_read = read_package_metadata_with_observations(
                &descriptor,
                &self.context.selected_project_options(),
                &self.context.overrides,
                &self.context.read_policy,
                &entry,
            );
            let read = match package_read {
                Ok(read) => read,
                Err(error) => {
                    self.warn(format!("source package {package} was skipped: {error}"));
                    self.mark_incomplete(format!(
                        "source package {package} metadata is incomplete: {error}"
                    ));
                    return Ok(Resolution::Incomplete {
                        reason: format!("source package {package} metadata is incomplete"),
                        candidates: Vec::new(),
                    });
                }
            };
            self.check_cancel(cancel)?;
            for observation in read.metadata.metadata_observations.iter().cloned() {
                self.record_observation(ResolutionObservation::Metadata(observation));
            }
            for observation in read.observations {
                self.record_observation(ResolutionObservation::ProjectRead(observation));
            }
            for warning in read.metadata.warnings {
                self.warn(warning);
            }
            if read.metadata.incomplete {
                self.mark_incomplete(format!(
                    "source package {package} metadata omitted an unsafe candidate"
                ));
                return Ok(Resolution::Incomplete {
                    reason: format!("source package {package} metadata is incomplete"),
                    candidates: Vec::new(),
                });
            }
            let mut package_candidate_count = 0usize;
            for (unit_name, entries) in read.metadata.unit_entries {
                if !declared_name_matches(Some(&unit_name), requested, lookup, &self.context) {
                    continue;
                }
                for entry in entries {
                    if package_candidate_count >= self.limits.max_package_unit_candidates {
                        self.mark_incomplete(format!(
                            "package unit candidate limit ({}) reached",
                            self.limits.max_package_unit_candidates
                        ));
                        return Ok(Resolution::Incomplete {
                            reason: "package unit candidate limit reached".to_string(),
                            candidates: Vec::new(),
                        });
                    }
                    package_candidate_count = package_candidate_count.saturating_add(1);
                    match self.load_candidate(
                        &entry.path,
                        &entry,
                        legacy_route,
                        SourceKind::Unit,
                        cancel,
                    ) {
                        Ok(source) => {
                            let metadata = parse_source_metadata(&source);
                            if declared_name_matches(
                                metadata.declared_name.as_deref(),
                                requested,
                                lookup,
                                &self.context,
                            ) {
                                package_candidates.push((
                                    source,
                                    ResolutionCandidate {
                                        path: entry.path.clone(),
                                        declared_name: metadata.declared_name,
                                    },
                                ));
                            }
                        }
                        Err(SourceStoreError::NotFound { .. }) => {}
                        Err(SourceStoreError::Cancelled) => return Err(ResolverError::Cancelled),
                        Err(error) => {
                            self.warn(format!(
                                "package unit candidate could not be loaded: {error:?}"
                            ));
                            self.mark_incomplete(format!(
                                "package unit candidate {} could not be loaded",
                                entry.path.display()
                            ));
                            return Ok(Resolution::Incomplete {
                                reason: "package unit candidate could not be loaded".to_string(),
                                candidates: Vec::new(),
                            });
                        }
                    }
                }
            }
        }
        package_candidates
            .sort_by(|left, right| path_key(&left.0.path).cmp(&path_key(&right.0.path)));
        package_candidates.dedup_by(|left, right| left.0.id == right.0.id);
        match package_candidates.len() {
            0 => Ok(Resolution::Unavailable {
                reason: format!("no package unit candidate for {requested}"),
            }),
            1 => {
                let (source, candidate) = package_candidates.pop().expect("one package candidate");
                Ok(Resolution::Found(ResolvedUnit {
                    requested_name: requested.to_string(),
                    declared_name: candidate
                        .declared_name
                        .unwrap_or_else(|| lookup.to_string()),
                    source,
                }))
            }
            _ => {
                self.warn(format!(
                    "ambiguous package unit {requested}: multiple valid candidates"
                ));
                Ok(Resolution::Ambiguous {
                    candidates: package_candidates
                        .into_iter()
                        .map(|(_, candidate)| candidate)
                        .collect(),
                })
            }
        }
    }

    fn package_roots(&self) -> Vec<PathBuf> {
        let mut roots = self.workspace_roots.clone();
        for configured_root in self.context.overrides.read_roots() {
            let root = resolved_package_root(&self.context, &configured_root);
            if !root.exists() && !context_uses_package_root(&self.context, &root) {
                continue;
            }
            if !roots
                .iter()
                .any(|existing| path_equivalent(existing, &root))
            {
                roots.push(root);
            }
        }
        roots
    }

    fn resolve_includes_for_source(
        &mut self,
        source: &LoadedSource,
        environment: &mut crate::conditional::ConditionalEnvironment,
        legacy_route: Option<&LegacyRoute>,
        cancel: &dyn CancellationToken,
        depth: usize,
        active: &mut HashSet<SourceId>,
    ) -> Result<IncludeWalkResult, ResolverError> {
        self.check_cancel(cancel)?;
        if depth > self.limits.max_include_depth {
            self.mark_incomplete(format!(
                "include depth limit ({}) reached",
                self.limits.max_include_depth
            ));
            return Ok(IncludeWalkResult {
                includes: Vec::new(),
                complete: false,
            });
        }
        if !active.insert(source.id.clone()) {
            self.mark_incomplete(format!("include cycle at {}", source.path.display()));
            return Ok(IncludeWalkResult {
                includes: Vec::new(),
                complete: false,
            });
        }
        let metadata = parse_source_metadata(source);
        let text = metadata.text;
        let lexical_analysis = crate::conditional::analyze_with_cancel(&text, &[], cancel);
        let include_directives = lexical_analysis
            .directives
            .iter()
            .filter(|directive| directive.kind == crate::conditional::DirectiveKind::Include)
            .collect::<Vec<_>>();
        let remaining_directives = self
            .limits
            .max_include_directives
            .saturating_sub(self.include_directives);
        let admitted_directives = include_directives.len().min(remaining_directives);
        self.include_directives = self.include_directives.saturating_add(admitted_directives);
        let directive_budget_exhausted = admitted_directives < include_directives.len();
        self.check_cancel(cancel)?;
        if source.decoded_text.is_none() && text.len() != source.bytes.len() {
            active.remove(&source.id);
            let reason = format!(
                "source encoding is not byte-offset preserving for {}",
                source.path.display()
            );
            self.mark_incomplete(reason);
            let emitted_sites = include_directives
                .iter()
                .take(admitted_directives)
                .filter_map(|directive| {
                    include_name(&directive.body).map(|name| (directive.start..directive.end, name))
                })
                .collect::<Vec<_>>();
            if directive_budget_exhausted {
                self.mark_incomplete(format!(
                    "include directive limit ({}) reached",
                    self.limits.max_include_directives
                ));
            }
            let result = IncludeWalkResult {
                includes: emitted_sites
                    .into_iter()
                    .map(|site| ResolvedInclude {
                        including_source_id: source.id.clone(),
                        byte_range: site.0,
                        requested_name: site.1,
                        target: ResolutionTarget::Incomplete,
                    })
                    .collect(),
                complete: false,
            };
            return Ok(result);
        }
        let mut active_results =
            HashMap::<usize, (ResolutionTarget<SourceId>, Vec<ResolvedInclude>)>::new();
        let mut callback_error = None;
        let analysis = crate::conditional::analyze_with_include_callback(
            &text,
            environment,
            cancel,
            &mut |directive, environment| {
                let mut transition = crate::conditional::IncludeTransition {
                    complete: false,
                    environment_known: false,
                };
                if callback_error.is_some() {
                    return transition;
                }
                let Some(ordinal) = include_directives
                    .iter()
                    .position(|candidate| candidate.start == directive.start)
                else {
                    callback_error = Some(ResolverError::InvalidRequest(
                        "conditional include occurrence was not admitted".to_string(),
                    ));
                    return transition;
                };
                if ordinal >= admitted_directives {
                    return transition;
                }
                let Some(name) = include_name(&directive.body) else {
                    self.mark_incomplete(
                        "active include has no statically known file name".to_string(),
                    );
                    active_results
                        .insert(directive.start, (ResolutionTarget::Incomplete, Vec::new()));
                    return transition;
                };
                let outcome = match self.resolve_include_result(
                    IncludeResolveRequest {
                        including_path: &source.path,
                        byte_range: directive.start..directive.end,
                        requested_name: &name,
                        legacy_route,
                    },
                    cancel,
                ) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        callback_error = Some(error);
                        return transition;
                    }
                };
                match outcome {
                    Resolution::Found(child) => {
                        if active.contains(&child.id) {
                            self.mark_incomplete(format!(
                                "include cycle at {}",
                                child.path.display()
                            ));
                            active_results.insert(
                                directive.start,
                                (ResolutionTarget::Incomplete, Vec::new()),
                            );
                            return transition;
                        }
                        let nested = match self.resolve_includes_for_source(
                            &child,
                            environment,
                            legacy_route,
                            cancel,
                            depth + 1,
                            active,
                        ) {
                            Ok(nested) => nested,
                            Err(error) => {
                                callback_error = Some(error);
                                return transition;
                            }
                        };
                        let complete = nested.complete;
                        active_results.insert(
                            directive.start,
                            (ResolutionTarget::Found(child.id), nested.includes),
                        );
                        transition.complete = complete;
                        transition.environment_known = complete;
                    }
                    Resolution::Unavailable { .. } => {
                        self.mark_incomplete(format!("active include {name} was not found"));
                        active_results
                            .insert(directive.start, (ResolutionTarget::Incomplete, Vec::new()));
                    }
                    Resolution::Ambiguous { .. } => {
                        self.mark_incomplete(format!("active include {name} was ambiguous"));
                        active_results
                            .insert(directive.start, (ResolutionTarget::Ambiguous, Vec::new()));
                    }
                    Resolution::Incomplete { .. } => {
                        active_results
                            .insert(directive.start, (ResolutionTarget::Incomplete, Vec::new()));
                    }
                }
                transition
            },
        );
        if let Some(error) = callback_error {
            active.remove(&source.id);
            return Err(error);
        }
        self.check_cancel(cancel)?;
        let mut result = IncludeWalkResult {
            includes: Vec::new(),
            complete: analysis.complete
                && analysis.unknown_spans.is_empty()
                && !directive_budget_exhausted,
        };
        if !analysis.complete {
            self.mark_incomplete(format!(
                "conditional analysis is incomplete for {}",
                source.path.display()
            ));
        }
        if !analysis.unknown_spans.is_empty() {
            self.mark_incomplete(format!(
                "source {} contains unknown conditional activity",
                source.path.display()
            ));
        }
        if directive_budget_exhausted {
            self.mark_incomplete(format!(
                "include directive limit ({}) reached",
                self.limits.max_include_directives
            ));
        }
        let unsupported_active = analysis.directives.iter().any(|directive| {
            directive.activity != crate::conditional::Truth::False
                && directive.kind == crate::conditional::DirectiveKind::Other
        });
        if unsupported_active {
            result.complete = false;
            self.mark_incomplete(format!(
                "source {} contains an unsupported active directive",
                source.path.display()
            ));
        }
        for directive in analysis
            .directives
            .iter()
            .filter(|directive| directive.kind == crate::conditional::DirectiveKind::Include)
        {
            let Some(ordinal) = include_directives
                .iter()
                .position(|candidate| candidate.start == directive.start)
            else {
                result.complete = false;
                continue;
            };
            if ordinal >= admitted_directives {
                break;
            }
            let Some(name) = include_name(&directive.body) else {
                if directive.potentially_active() {
                    result.complete = false;
                    self.mark_incomplete(
                        "active include has no statically known file name".to_string(),
                    );
                }
                continue;
            };
            if directive.activity == crate::conditional::Truth::False {
                result.includes.push(ResolvedInclude {
                    including_source_id: source.id.clone(),
                    byte_range: directive.start..directive.end,
                    requested_name: name,
                    target: ResolutionTarget::Unavailable,
                });
                continue;
            }
            let Some((mut target, nested)) = active_results.remove(&directive.start) else {
                result.complete = false;
                self.mark_incomplete(format!("include {name} has unknown conditional activity"));
                result.includes.push(ResolvedInclude {
                    including_source_id: source.id.clone(),
                    byte_range: directive.start..directive.end,
                    requested_name: name,
                    target: ResolutionTarget::Incomplete,
                });
                continue;
            };
            if unsupported_active {
                target = ResolutionTarget::Incomplete;
            }
            if !matches!(target, ResolutionTarget::Found(_)) {
                result.complete = false;
            }
            result.includes.extend(nested);
            result.includes.push(ResolvedInclude {
                including_source_id: source.id.clone(),
                byte_range: directive.start..directive.end,
                requested_name: name,
                target,
            });
        }
        active.remove(&source.id);
        Ok(result)
    }

    // Project include bindings are kept in a session-local queue so the public
    // project result can retain source payloads without changing the simple
    // direct include outcome API.
    fn append_include_result(
        &mut self,
        result: IncludeWalkResult,
        complete: &mut bool,
        include_sources: &mut Vec<LoadedSource>,
    ) {
        *complete &= result.complete;
        for include in result.includes {
            if let ResolutionTarget::Found(id) = &include.target {
                if let Some(source) = self.loaded.get(id).cloned() {
                    if !include_sources.iter().any(|existing| existing.id == *id) {
                        include_sources.push(source);
                    }
                }
            }
            self.project_includes.push(include);
        }
    }

    fn record_observation(&mut self, observation: ResolutionObservation) {
        push_observation(&mut self.report.observations, observation);
    }

    fn warn(&mut self, warning: String) {
        push_warning(
            &mut self.report,
            warning,
            self.limits.max_resolution_warnings,
        );
    }

    fn mark_incomplete(&mut self, reason: String) {
        self.report.complete = false;
        if !self
            .report
            .incomplete_reasons
            .iter()
            .any(|existing| existing == &reason)
        {
            self.report.incomplete_reasons.push(reason);
        }
    }

    fn finish_report(&self) -> ResolutionReport {
        self.report.clone()
    }

    fn take_project_includes(&mut self) -> Vec<ResolvedInclude> {
        std::mem::take(&mut self.project_includes)
    }
}

struct CandidateGroup {
    paths: Vec<(PathBuf, ProjectPathEntry)>,
    legacy_route: Option<LegacyRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DirectoryCacheKey {
    path: PathBuf,
    entry: ProjectPathEntry,
    read_policy: ReadPolicy,
    missing_is_incomplete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct UnitCacheKey {
    importer_directory: PathBuf,
    requested_name: String,
    legacy_route: Option<LegacyRoute>,
}

impl UnitCacheKey {
    fn new(path: &Path, requested_name: &str, legacy_route: Option<&LegacyRoute>) -> Self {
        Self {
            importer_directory: canonical_path(path.parent().unwrap_or(path)),
            requested_name: requested_name.to_ascii_lowercase(),
            legacy_route: legacy_route.cloned(),
        }
    }
}

#[derive(Debug, Clone)]
struct Catalogue {
    entries: HashMap<String, Vec<PathBuf>>,
    complete: bool,
}

enum CaseInsensitiveLookup {
    Missing,
    Found(PathBuf),
    Ambiguous(Vec<PathBuf>),
    Incomplete,
}

#[derive(Debug, Clone, Default)]
struct IncludeWalkResult {
    includes: Vec<ResolvedInclude>,
    complete: bool,
}

#[derive(Debug, Clone)]
struct ParsedSourceMetadata {
    text: String,
    declared_name: Option<String>,
    imports: Vec<ImportSite>,
}

fn parse_source_metadata(source: &LoadedSource) -> ParsedSourceMetadata {
    let text = source.decoded_text.as_deref().map_or_else(
        || crate::text::decode_bytes(&source.bytes).into_owned(),
        ToOwned::to_owned,
    );
    let parse_bytes = source
        .decoded_text
        .as_deref()
        .map_or(source.bytes.as_ref(), str::as_bytes);
    let (tree, _) = match crate::parser::parse_file(
        &crate::types::FileInfo::new(source.path.clone()),
        parse_bytes,
    ) {
        Ok(parsed) => parsed,
        Err(_) => {
            return ParsedSourceMetadata {
                text,
                declared_name: None,
                imports: Vec::new(),
            };
        }
    };
    let mut modules = Vec::new();
    collect_nodes(tree.root_node(), &mut modules);
    let unit = modules
        .iter()
        .find(|node| !has_ancestor_kind(**node, "declUses"));
    let declared_name = unit
        .and_then(|node| node.utf8_text(parse_bytes).ok())
        .map(canonical_unit_name)
        .filter(|name| !name.is_empty());
    let mut imports = Vec::new();
    for node in modules
        .iter()
        .filter(|node| has_ancestor_kind(**node, "declUses"))
    {
        let Ok(name) = node.utf8_text(parse_bytes) else {
            continue;
        };
        let name = canonical_unit_name(name);
        if name.is_empty() {
            continue;
        }
        let section = if has_ancestor_kind(*node, "interface") {
            ImportSection::Interface
        } else if has_ancestor_kind(*node, "implementation") {
            ImportSection::Implementation
        } else {
            ImportSection::Module
        };
        imports.push(ImportSite {
            byte_range: node.start_byte()..node.end_byte(),
            requested_name: name,
            section,
        });
    }
    ParsedSourceMetadata {
        text,
        declared_name,
        imports,
    }
}

fn collect_nodes<'a>(node: tree_sitter::Node<'a>, output: &mut Vec<tree_sitter::Node<'a>>) {
    if node.kind() == "moduleName" {
        output.push(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_nodes(child, output);
    }
}

fn has_ancestor_kind(mut node: tree_sitter::Node<'_>, kind: &str) -> bool {
    while let Some(parent) = node.parent() {
        if parent.kind() == kind {
            return true;
        }
        node = parent;
    }
    false
}

fn declared_name_matches(
    actual: Option<&str>,
    requested: &str,
    lookup: &str,
    context: &ProjectContext,
) -> bool {
    let Some(actual) = actual else {
        return false;
    };
    actual.eq_ignore_ascii_case(requested)
        || actual.eq_ignore_ascii_case(lookup)
        || (!lookup.contains('.')
            && context
                .unit_namespaces
                .iter()
                .any(|namespace| format!("{namespace}.{lookup}").eq_ignore_ascii_case(actual)))
}

fn qualifiers_for_found(requested: &str, lookup: &str, declared: &str) -> Vec<String> {
    let mut qualifiers = Vec::new();
    for spelling in [
        Some(requested),
        (!lookup.eq_ignore_ascii_case(requested)).then_some(lookup),
        (!declared.eq_ignore_ascii_case(requested) && !declared.eq_ignore_ascii_case(lookup))
            .then_some(declared),
        short_unit_name(lookup),
        short_unit_name(declared),
    ] {
        if let Some(spelling) = spelling.filter(|spelling| !spelling.trim().is_empty()) {
            if !qualifiers
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(spelling))
            {
                qualifiers.push(spelling.to_string());
            }
        }
    }
    qualifiers
}

fn qualifiers_for_unresolved(requested: &str, lookup: &str) -> Vec<String> {
    let mut qualifiers = Vec::new();
    for spelling in [
        Some(requested),
        (!lookup.eq_ignore_ascii_case(requested)).then_some(lookup),
    ] {
        if let Some(spelling) = spelling.filter(|spelling| !spelling.trim().is_empty()) {
            if !qualifiers
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(spelling))
            {
                qualifiers.push(spelling.to_string());
            }
        }
    }
    qualifiers
}

fn short_unit_name(name: &str) -> Option<&str> {
    name.rsplit('.').next().filter(|short| *short != name)
}

fn analysis_contains_range(spans: &[Range<usize>], range: &Range<usize>) -> bool {
    spans
        .iter()
        .any(|span| span.start <= range.start && range.end <= span.end)
}

fn aliased_name(context: &ProjectContext, requested: &str) -> String {
    context
        .unit_aliases
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(requested))
        .map_or_else(|| requested.to_string(), |(_, target)| target.clone())
}

fn filename_tiers(unit_name: &str, namespaces: &[String]) -> Vec<Vec<String>> {
    let short = unit_name.rsplit('.').next().unwrap_or(unit_name);
    let mut exact = vec![format!("{unit_name}.pas")];
    if !short.eq_ignore_ascii_case(unit_name) {
        exact.push(format!("{short}.pas"));
    }
    let mut tiers = vec![exact];
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

fn include_name(body: &str) -> Option<String> {
    let raw = body
        .trim_start()
        .split_once(|character: char| character.is_ascii_whitespace() || character == ':')
        .map(|(_, rest)| rest.trim())
        .filter(|rest| !rest.is_empty())?;
    if raw.contains("$(") {
        return None;
    }
    let raw = if let Some(quoted) = raw.strip_prefix('\'') {
        quoted.strip_suffix('\'')?
    } else if let Some(quoted) = raw.strip_prefix('"') {
        quoted.strip_suffix('"')?
    } else {
        if raw
            .chars()
            .any(|character| character == '\'' || character == '"')
        {
            return None;
        }
        raw
    };
    (!raw.is_empty()).then(|| raw.replace('\\', "/"))
}

fn canonical_unit_name(name: &str) -> String {
    name.trim()
        .replace(char::is_whitespace, "")
        .trim_matches('.')
        .to_string()
}

fn package_stem(name: &str) -> String {
    let name = name.replace('\\', "/");
    let name = name.rsplit('/').next().unwrap_or(&name);
    name.rsplit_once('.')
        .map_or(name, |(stem, extension)| {
            if matches!(
                extension.to_ascii_lowercase().as_str(),
                "dcp" | "dpk" | "dproj" | "bpl"
            ) {
                stem
            } else {
                name
            }
        })
        .to_ascii_lowercase()
}

fn dedup_paths(paths: Vec<(PathBuf, ProjectPathEntry)>) -> Vec<(PathBuf, ProjectPathEntry)> {
    let mut result: Vec<(PathBuf, ProjectPathEntry)> = Vec::new();
    for (path, entry) in paths {
        if !result
            .iter()
            .any(|(existing, _)| path_equivalent(existing, &path))
        {
            result.push((path, entry));
        }
    }
    result.sort_by(|left, right| path_key(&left.0).cmp(&path_key(&right.0)));
    result
}

fn push_warning(report: &mut ResolutionReport, warning: String, maximum: usize) {
    if report.warnings.iter().any(|existing| existing == &warning) {
        return;
    }
    if maximum == 0 {
        return;
    }
    if report.warnings.len() < maximum {
        report.warnings.push(warning);
    } else if report
        .warnings
        .last()
        .is_none_or(|last| last != "resolution warning limit reached")
    {
        if let Some(last) = report.warnings.last_mut() {
            *last = "resolution warning limit reached".to_string();
        }
    }
}

fn push_observation(
    observations: &mut Vec<ResolutionObservation>,
    observation: ResolutionObservation,
) {
    match &observation {
        ResolutionObservation::Metadata(metadata) => {
            let path = metadata.path();
            if let Some(existing) = observations.iter_mut().find(|existing| matches!(existing, ResolutionObservation::Metadata(current) if current.path() == path)) {
                if matches!(existing, ResolutionObservation::Metadata(MetadataObservation::Stat { .. }))
                    && matches!(metadata, MetadataObservation::Payload { .. })
                { *existing = observation; }
                return;
            }
        }
        ResolutionObservation::ProjectRead(read) => {
            if observations.iter().any(|existing| matches!(existing, ResolutionObservation::ProjectRead(current) if current.path == read.path)) { return; }
        }
        ResolutionObservation::Candidate { path, present, .. } => {
            if let Some(existing) = observations.iter_mut().find(|existing| {
                matches!(existing, ResolutionObservation::Candidate { path: current, .. } if current == path)
            }) {
                if *present {
                    *existing = observation;
                }
                return;
            }
        }
        _ => {
            if observations.iter().any(|existing| existing == &observation) { return; }
        }
    }
    observations.push(observation);
}

fn canonical_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(std::path::MAIN_SEPARATOR.to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    if cfg!(windows) {
        PathBuf::from(normalized.to_string_lossy().to_ascii_lowercase())
    } else {
        normalized
    }
}

fn source_id_for_path(path: &Path) -> SourceId {
    SourceId::new(format!("source:{}", canonical_path(path).to_string_lossy()))
}

fn legacy_route_authorizes(path: &Path, route: &LegacyRoute) -> bool {
    let path = canonical_path(path);
    let source = canonical_path(&route.source_path);
    let sibling_directory = canonical_path(&route.sibling_directory);
    source
        .parent()
        .is_some_and(|parent| path_equivalent(parent, &sibling_directory))
        && path
            .parent()
            .is_some_and(|parent| path_equivalent(parent, &sibling_directory))
}

fn legacy_route_authorizes_directory(directory: &Path, route: &LegacyRoute) -> bool {
    let directory = canonical_path(directory);
    let source = canonical_path(&route.source_path);
    let sibling_directory = canonical_path(&route.sibling_directory);
    source
        .parent()
        .is_some_and(|parent| path_equivalent(parent, &sibling_directory))
        && path_equivalent(&directory, &sibling_directory)
}

fn relative_components(base: &Path, path: &Path) -> Option<Vec<std::ffi::OsString>> {
    let base_components = base.components().collect::<Vec<_>>();
    let path_components = path.components().collect::<Vec<_>>();
    if path_components.len() < base_components.len()
        || !base_components
            .iter()
            .zip(path_components.iter())
            .all(|(base, path)| {
                if cfg!(windows) {
                    base.as_os_str()
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&path.as_os_str().to_string_lossy())
                } else {
                    base == path
                }
            })
    {
        return None;
    }
    Some(
        path_components[base_components.len()..]
            .iter()
            .filter_map(|component| match component {
                Component::Normal(value) => Some(value.to_os_string()),
                _ => None,
            })
            .collect(),
    )
}

fn path_key(path: &Path) -> String {
    let key = canonical_path(path).to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        key.to_ascii_lowercase()
    } else {
        key
    }
}

fn path_equivalent(left: &Path, right: &Path) -> bool {
    path_key(left) == path_key(right)
}

fn path_starts_with(path: &Path, root: &Path) -> bool {
    let path = canonical_path(path);
    let root = canonical_path(root);
    let path_components = path.components().collect::<Vec<_>>();
    let root_components = root.components().collect::<Vec<_>>();
    path_components.len() >= root_components.len()
        && path_components
            .iter()
            .zip(root_components.iter())
            .all(|(left, right)| {
                if cfg!(windows) {
                    left.as_os_str()
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
                } else {
                    left == right
                }
            })
}

fn context_uses_package_root(context: &ProjectContext, root: &Path) -> bool {
    context
        .main_source_entry
        .iter()
        .chain(context.search_path_entries.iter())
        .chain(context.include_path_entries.iter())
        .chain(context.explicit_unit_entries.values().flatten())
        .any(|entry| match &entry.provenance {
            ProjectPathProvenance::Mapped { root: mapped_root } => {
                path_equivalent(mapped_root, root) || path_starts_with(&entry.path, root)
            }
            ProjectPathProvenance::Configured => path_starts_with(&entry.path, root),
            ProjectPathProvenance::LegacyNative => false,
        })
}

fn resolved_package_root(context: &ProjectContext, configured_root: &Path) -> PathBuf {
    let configured_root = canonical_path(configured_root);
    context
        .main_source_entry
        .iter()
        .chain(context.search_path_entries.iter())
        .chain(context.include_path_entries.iter())
        .chain(context.explicit_unit_entries.values().flatten())
        .find_map(|entry| match &entry.provenance {
            ProjectPathProvenance::Mapped { root }
                if path_equivalent_ignore_case(root, &configured_root) =>
            {
                Some(canonical_path(root))
            }
            _ => None,
        })
        .unwrap_or(configured_root)
}

fn path_equivalent_ignore_case(left: &Path, right: &Path) -> bool {
    let left = canonical_path(left);
    let right = canonical_path(right);
    let left_components = left.components().collect::<Vec<_>>();
    let right_components = right.components().collect::<Vec<_>>();
    left_components.len() == right_components.len()
        && left_components
            .iter()
            .zip(right_components.iter())
            .all(|(left, right)| {
                left.as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
            })
}
