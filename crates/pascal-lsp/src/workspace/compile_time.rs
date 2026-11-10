//! Request-scoped compile-time evaluation and physical expansion projection.
//! The core session owns lexical entry state; this adapter only maps its output.
use super::resolver::LspSourceStore;
use super::{ContextKey, ReconciliationBudget, Workspace};
use crate::include_expansion::{
    ExpandedSource, ExpansionLimits, ExpansionResult, IncludeObservation, ResolvedInclude,
};
use lsp_types::Url;
use pascal_core::compile_time::{AdmittedOwned, CompileTimeBudget};
use pascal_core::compile_time::{CompileTimeAnalysis, OccurrenceId};
use pascal_core::conditional::ConditionalAnalysis;
use pascal_core::resolver::{
    CancellationToken, DirectoryListing, DirectoryRequest, LoadedSource, ResolutionObservation,
    ResolverLimits, SourceRequest, SourceRevision, SourceStore, SourceStoreError, UnitResolver,
};
use pascal_project::ProjectWorkBudget;
use std::ops::Range;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Compiled provider interfaces shared through the project cache (l4d-4nbs
/// D29) by every analysis under one resolver configuration. A reuse replays
/// into this session's capture what the compilation read.
struct CachedProviderGraphs {
    cache: crate::project_cache::ProjectCache,
    configuration: u64,
    overlays: Arc<std::collections::HashMap<Url, super::rename::OverlayInput>>,
    capture: Arc<super::capture::ReadCapture>,
}

impl CachedProviderGraphs {
    /// Core revisions hash exact bytes; overlay probes use the framed text
    /// hash of the buffer that was read. `None` when that buffer is gone.
    fn overlay_probe(
        &self,
        path: &std::path::Path,
        version: i32,
        content_hash: u64,
    ) -> Option<crate::project_cache::Probe> {
        let uri = Url::from_file_path(path).ok()?;
        let overlay = self.overlays.get(&uri)?;
        (overlay.version == version
            && pascal_project::content_hash_bytes(overlay.text.as_bytes()) == content_hash)
            .then(|| crate::project_cache::Probe::Overlay {
                uri,
                version,
                content_hash: crate::project_cache::overlay_content_hash(&overlay.text),
            })
    }
}

impl pascal_core::compile_time::SharedProviderGraphs for CachedProviderGraphs {
    fn get(
        &self,
        key: &pascal_core::compile_time::ProviderGraphKey,
    ) -> Option<Arc<pascal_core::compile_time::SharedProviderGraph>> {
        let hit = self
            .cache
            .provider_graph(self.configuration, key, &self.overlays)?;
        self.capture.replay(&hit.reads);
        Some(hit.graph)
    }

    fn put(
        &self,
        key: pascal_core::compile_time::ProviderGraphKey,
        graph: Arc<pascal_core::compile_time::SharedProviderGraph>,
    ) {
        use crate::project_cache::Probe;
        let mut shareable = true;
        let mut probe = |path: &std::path::Path, hash: u64, disk: Option<&_>, version| {
            let probe = match (disk, version) {
                (Some(stamp), _) => Some(Probe::Content {
                    path: path.to_path_buf(),
                    stamp: Some(Clone::clone(stamp)),
                    content_hash: hash,
                }),
                (None, Some(version)) => self.overlay_probe(path, version, hash),
                (None, None) => None,
            };
            shareable &= probe.is_some();
            probe
        };
        let (mut probes, _) =
            crate::project_cache::observation_probes(&graph.observations, &mut |path, revision| {
                match revision {
                    SourceRevision::Disk {
                        stamp,
                        content_hash,
                        ..
                    } => probe(path, *content_hash, Some(stamp), None),
                    SourceRevision::Overlay {
                        version,
                        content_hash,
                    } => probe(path, *content_hash, None, Some(*version)),
                }
            });
        // Every directory a unit lookup of the interface or of a nested
        // provider consulted, which the session's report may have
        // deduplicated away (l4d-4nbs review C1): a unit created there may
        // shadow what the lookup found.
        let (traced, _) = crate::project_cache::observation_probes(&graph.trace, &mut |_, _| None);
        probes.extend(traced);
        // The provider sources themselves, nested ones included: a session
        // loads a provider before compiling it, so its observations may not
        // name it.
        for dependency in graph.dependencies.iter() {
            probes.extend(probe(
                &dependency.path,
                dependency.content_hash,
                dependency.disk_stamp.as_ref(),
                dependency.overlay_version,
            ));
        }
        if !shareable {
            return;
        }
        let probes = crate::project_cache::dedup_probes(probes);
        // The trace's directories too: a reusing session reports them
        // (`replay_trace`), so its evidence needs their listings even when
        // this session's report had deduplicated them away (rootcause2 U1).
        let reads = self
            .capture
            .restricted_to(graph.observations.iter().chain(graph.trace.iter()));
        self.cache
            .put_provider_graph(self.configuration, key, graph, probes, reads);
    }
}

/// Include lookups that found nothing, shared through the project cache
/// (l4d-ig6b) by every analysis under one resolver configuration. A reuse
/// replays into this session's capture what the lookup listed.
struct CachedIncludeFailures {
    cache: crate::project_cache::ProjectCache,
    configuration: u64,
    context: Arc<pascal_project::ProjectContext>,
    capture: Arc<super::capture::ReadCapture>,
    /// The lookup that missed and is searching, with whether the capture
    /// had seen editor state or overflowed before it began: what it stores
    /// is only its own (l4d-ig6b I2).
    searching: std::sync::Mutex<Option<(pascal_core::IncludeFailureKey, bool)>>,
}

impl pascal_core::SharedIncludeFailures for CachedIncludeFailures {
    fn get(
        &self,
        key: &pascal_core::IncludeFailureKey,
    ) -> Option<Arc<pascal_core::IncludeFailure>> {
        let hit = self
            .cache
            .include_failure(self.configuration, &self.context, key);
        let Some(hit) = hit else {
            let (overflowed, editor_state) = self.capture.flags();
            *self
                .searching
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                (!overflowed).then(|| (key.clone(), editor_state));
            return None;
        };
        // Stored with no overflow, so the replay adds only the reads this
        // session lacks (D29 dedup) and never copies an overflow.
        self.capture.replay(&hit.reads);
        Some(hit.failure)
    }

    fn put(&self, key: pascal_core::IncludeFailureKey, failure: Arc<pascal_core::IncludeFailure>) {
        let Some((searched, editor_state_before)) = self
            .searching
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return;
        };
        if searched != key {
            return;
        }
        let observations = failure
            .observations
            .iter()
            .map(|(_, observation)| observation.clone())
            .collect::<Vec<_>>();
        // A miss reads no payload; one that did is not shared.
        let mut shareable = true;
        let (probes, _) = crate::project_cache::observation_probes(&observations, &mut |_, _| {
            shareable = false;
            None
        });
        let (overflowed, editor_state) = self.capture.flags();
        // An overflowed capture may lack what the lookup listed.
        if !shareable || overflowed {
            return;
        }
        let mut reads = self.capture.restricted_to(&observations);
        // A read the lookup named may be missing.
        if reads.overflowed {
            return;
        }
        // Session-wide flags are not the lookup's: only editor state it
        // met itself carries over.
        reads.editor_state = editor_state && !editor_state_before;
        self.cache.put_include_failure(
            self.configuration,
            Arc::clone(&self.context),
            key,
            failure,
            probes,
            reads,
        );
    }
}

/// What a provider interface compiled for `context` depends on besides its
/// key: the lookup configuration and the editor state that shapes listings.
fn provider_graph_configuration(
    workspace: &Workspace,
    context: &pascal_project::ProjectContext,
    input: &super::rename::WorkspaceInput,
    limits: &ResolverLimits,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    crate::navigation::compiled_dcu::project_context_fingerprint(context).hash(&mut hasher);
    // The fingerprint leaves out what only lookups read (l4d-ig6b C1): the
    // include path, the main source and explicit `in` units (candidate
    // provenance), packages and missing references.
    context.include_path_entries().hash(&mut hasher);
    context.main_source_entry().hash(&mut hasher);
    let mut explicit = context.explicit_unit_entries().iter().collect::<Vec<_>>();
    explicit.sort_by(|left, right| left.0.cmp(right.0));
    explicit.hash(&mut hasher);
    context.packages().hash(&mut hasher);
    format!("{:?}", context.path_issues()).hash(&mut hasher);
    workspace.workspace_root_paths().hash(&mut hasher);
    format!("{limits:?}").hash(&mut hasher);
    // A buffer without a disk file joins its directory's listing, which a
    // directory probe cannot see; deletions and rejections hide files.
    let mut unsaved = input
        .overlays
        .keys()
        .filter(|uri| uri.to_file_path().is_ok_and(|path| !path.exists()))
        .map(Url::as_str)
        .collect::<Vec<_>>();
    unsaved.sort_unstable();
    unsaved.hash(&mut hasher);
    let mut hidden = input
        .deleted_overrides
        .keys()
        .chain(input.rejected_documents.iter())
        .map(Url::as_str)
        .collect::<Vec<_>>();
    hidden.sort_unstable();
    hidden.hash(&mut hasher);
    hasher.finish()
}

struct Control<'a> {
    cancel: &'a AtomicBool,
    budget: Option<&'a ReconciliationBudget>,
    /// An index member's extraction deadline (`Workspace::member_deadline`).
    deadline: Option<std::time::Instant>,
}

#[cfg(test)]
thread_local! { pub(super) static TEST_KEEP_DECLARATIONS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(test)]
thread_local! { static TEST_BODY_ANALYSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
thread_local! { static TEST_HEADER_REVALIDATION_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
pub(super) fn test_header_revalidation_hook(hook: Option<Box<dyn FnMut()>>) {
    TEST_HEADER_REVALIDATION_HOOK.with(|value| *value.borrow_mut() = hook);
}
#[cfg(test)]
thread_local! { static TEST_EXPANSION_FAILURE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) }; }
/// Makes every expansion of a document whose URI ends with `suffix` fail
/// as an exhausted resolver limit does.
#[cfg(test)]
pub(crate) fn test_fail_expansion_of(suffix: Option<&str>) {
    TEST_EXPANSION_FAILURE.with(|value| *value.borrow_mut() = suffix.map(str::to_owned));
}
#[cfg(test)]
pub(super) fn take_body_analyses() -> usize {
    TEST_BODY_ANALYSES.with(|count| count.replace(0))
}
impl Control<'_> {
    fn check(&self) -> Result<(), String> {
        if let Some(budget) = self.budget {
            budget.check_cancelled()?;
        }
        if self.cancel.load(Ordering::Relaxed) {
            Err(super::CANCELLATION_MESSAGE.into())
        } else if self
            .deadline
            .is_some_and(|deadline| std::time::Instant::now() >= deadline)
        {
            Err(super::indexing::MEMBER_DEADLINE_MESSAGE.into())
        } else {
            Ok(())
        }
    }
    fn finish<T>(&self, result: Result<T, impl std::fmt::Display>) -> Result<T, String> {
        self.check()?;
        result.map_err(|e| e.to_string())
    }
}
impl CancellationToken for Control<'_> {
    fn is_cancelled(&self) -> bool {
        self.check().is_err()
    }
}

struct Resources<'a> {
    store: LspSourceStore,
    control: &'a Control<'a>,
}

impl Resources<'_> {
    fn poll(&self) -> Result<(), SourceStoreError> {
        if self.control.is_cancelled() {
            Err(SourceStoreError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn prepare_read(&self, request: SourceRequest<'_>) -> Result<usize, SourceStoreError> {
        self.poll()?;
        let legacy = matches!(
            request.entry.provenance,
            pascal_project::ProjectPathProvenance::LegacyNative
        ) && request.read_policy.allows_legacy_route_entry(request.entry);
        if !legacy && !request.read_policy.allows_location(request.entry) {
            return Err(SourceStoreError::Unauthorized {
                path: request.path.to_path_buf(),
                reason: "include source is outside the owning project's readable roots".into(),
            });
        }
        let size = self
            .store
            .overlays
            .get(request.path)
            .map(|o| o.text.len())
            .or_else(|| {
                std::fs::metadata(request.path)
                    .ok()
                    .and_then(|m| usize::try_from(m.len()).ok())
            });
        if let Some(budget) = self.control.budget {
            budget
                .charge_path_visits(1)
                .map_err(|_| SourceStoreError::Cancelled)?;
            if let Some(size) = size.filter(|size| *size <= request.max_bytes) {
                budget
                    .ensure_file_read_fits(size)
                    .map_err(|_| SourceStoreError::Cancelled)?;
                budget
                    .charge_file_bytes(size)
                    .map_err(|_| SourceStoreError::Cancelled)?;
            }
        }
        self.poll()?;
        Ok(size.map_or(request.max_bytes, |size| size.min(request.max_bytes)))
    }
}

impl SourceStore for Resources<'_> {
    fn load_admitted(
        &mut self,
        request: SourceRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        let max_bytes = self.prepare_read(request)?;
        self.store.load_admitted(
            SourceRequest {
                max_bytes,
                ..request
            },
            budget,
            cancel,
        )
    }
    fn load(
        &mut self,
        request: SourceRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        let max_bytes = self.prepare_read(request)?;
        self.store.load(
            SourceRequest {
                max_bytes,
                ..request
            },
            cancel,
        )
    }
    fn list_directory_admitted(
        &mut self,
        request: DirectoryRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<AdmittedOwned<DirectoryListing>, SourceStoreError> {
        self.poll()?;
        if let Some(work) = self.control.budget {
            work.charge_path_visits(1)
                .map_err(|_| SourceStoreError::Cancelled)?;
        }
        let result = self
            .store
            .list_directory_admitted(request, budget, cancel)?;
        if let Some(work) = self.control.budget {
            work.charge_path_visits(result.files.len() + result.directories.len())
                .map_err(|_| SourceStoreError::Cancelled)?;
        }
        self.poll()?;
        Ok(result)
    }
    fn list_directory(
        &mut self,
        request: DirectoryRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<DirectoryListing, SourceStoreError> {
        self.poll()?;
        self.store.list_directory(request, cancel)
    }
    fn overlay_candidates_admitted(
        &self,
        roots: &[std::path::PathBuf],
        names: &[String],
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<AdmittedOwned<Vec<std::path::PathBuf>>, SourceStoreError> {
        self.poll()?;
        self.store
            .overlay_candidates_admitted(roots, names, budget, cancel)
    }
    fn overlay_candidates(
        &self,
        roots: &[std::path::PathBuf],
        names: &[String],
    ) -> Vec<std::path::PathBuf> {
        if self.control.is_cancelled() {
            vec![]
        } else {
            self.store.overlay_candidates(roots, names)
        }
    }
    fn declared_unit_name(&self, source: &LoadedSource) -> Option<Option<String>> {
        self.store.declared_unit_name(source)
    }
}

pub(super) fn expand(
    workspace: &Workspace,
    uri: &Url,
    source: &str,
    key: &ContextKey,
    limits: ExpansionLimits,
    cancel: &AtomicBool,
    budget: Option<&ReconciliationBudget>,
) -> Result<ExpansionResult, String> {
    expand_with_limits(
        workspace,
        uri,
        source,
        key,
        SessionLimits {
            expansion: limits,
            directory_entries: ResolverLimits::default().max_directory_entries,
        },
        cancel,
        budget,
    )
}

pub(super) struct SessionLimits {
    pub expansion: ExpansionLimits,
    pub directory_entries: usize,
}

// The public prefix verifier returns a bounded compatibility fact, not the
// discarded candidate proof owner. Keep genuine callback backings/reports alive
// until physical mapping and request-local observation validation finish.
struct HeaderCapture {
    source: pascal_core::compile_time::AdmittedSourceBacking,
    _reports: pascal_core::resolver::AdmittedCapabilityReports,
    previous: Option<Box<AdmittedOwned<HeaderCapture>>>,
}

pub(super) const HEADER_CHANGED: &str = "selected import header proof changed before publication";

struct HeaderResolver<'a> {
    inner: &'a mut UnitResolver<LspSourceStore>,
    captures: Option<AdmittedOwned<HeaderCapture>>,
}

impl pascal_core::compile_time::CompileTimeResolver for HeaderResolver<'_> {
    fn scan_credit_floor(&mut self, source: &LoadedSource) -> usize {
        self.inner.scan_credit_floor(source)
    }
    fn note_scanned(&mut self, source: &LoadedSource, end: usize) {
        self.inner.note_scanned(source, end);
    }
    fn fund_validation(&mut self, source: &LoadedSource) -> bool {
        self.inner.fund_validation(source)
    }
    fn include_capability(
        &mut self,
        request: pascal_core::IncludeResolveRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<
        pascal_core::resolver::AdmittedCapabilityOutcome<LoadedSource>,
        pascal_core::ResolverError,
    > {
        let outcome = self.inner.include_capability(request, budget, cancel)?;
        if let pascal_core::Resolution::Found(source) = outcome.result() {
            let backing = source
                .compile_time_backing
                .as_ref()
                .filter(|backing| backing.is_admitted_by(budget))
                .ok_or_else(|| {
                    pascal_core::ResolverError::InvalidRequest(
                        "header include has no admitted coordinates".into(),
                    )
                })?;
            if backing.decoded_text != source.decoded_text
                || backing.id != source.id
                || backing.revision != source.revision
            {
                return Err(pascal_core::ResolverError::InvalidRequest(
                    "header include coordinate owner differs".into(),
                ));
            }
            let reports = pascal_core::resolver::AdmittedCapabilityReports::copy(
                outcome.reports().observations(),
                outcome.reports().warnings(),
                budget,
            )?;
            let retained = std::mem::size_of::<HeaderCapture>()
                + if self.captures.is_some() {
                    std::mem::size_of::<AdmittedOwned<HeaderCapture>>()
                } else {
                    0
                };
            let captured = budget
                .admit_owned(
                    1,
                    std::mem::size_of::<pascal_core::compile_time::AdmittedSourceBacking>(),
                    retained,
                    || HeaderCapture {
                        source: backing.clone(),
                        _reports: reports,
                        previous: self.captures.take().map(Box::new),
                    },
                )
                .map_err(|reason| header_admission_error(reason, budget))?;
            self.captures = Some(captured);
        }
        Ok(outcome)
    }
    fn unit_report_capability(
        &mut self,
        request: pascal_core::UnitResolveRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<
        pascal_core::resolver::AdmittedCapabilityOutcome<
            pascal_core::compile_time::AdmittedProviderSource,
        >,
        pascal_core::ResolverError,
    > {
        self.inner.unit_report_capability(request, budget, cancel)
    }
    fn include(
        &mut self,
        request: pascal_core::IncludeResolveRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<pascal_core::ResolutionOutcome<LoadedSource>, pascal_core::ResolverError> {
        self.inner.include(request, budget, cancel)
    }
    fn unit(
        &mut self,
        request: pascal_core::UnitResolveRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<
        pascal_core::ResolutionOutcome<pascal_core::compile_time::ProviderSource>,
        pascal_core::ResolverError,
    > {
        self.inner.unit(request, budget, cancel)
    }
    fn max_include_depth(&self) -> usize {
        self.inner.max_include_depth()
    }
    fn admit_include(
        &mut self,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<bool, pascal_core::ResolverError> {
        self.inner.admit_include(budget, cancel)
    }
}

fn header_admission_error(
    reason: pascal_core::compile_time::IncompleteReason,
    budget: &CompileTimeBudget<'_>,
) -> pascal_core::ResolverError {
    if matches!(
        reason,
        pascal_core::compile_time::IncompleteReason::Cancelled
    ) {
        return pascal_core::ResolverError::Cancelled;
    }
    pascal_core::ResolverError::InvalidRequest(format!(
        "selected header admission refused: {reason:?}; {:?}",
        budget.snapshot()
    ))
}

fn header_position(
    text: &str,
    offset: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<lsp_types::Position, String> {
    let prefix = text.get(..offset).ok_or("invalid physical header offset")?;
    if prefix.ends_with('\r') && text.as_bytes().get(offset) == Some(&b'\n') {
        return Err("header offset splits CRLF".into());
    }
    let mut line = 0u32;
    let mut character = 0u32;
    let mut cr = false;
    for ch in prefix.chars() {
        budget
            .charge(1, ch.len_utf8(), 0)
            .map_err(|reason| header_admission_error(reason, budget).to_string())?;
        match ch {
            '\r' => {
                line = line.checked_add(1).ok_or("header line overflow")?;
                character = 0;
                cr = true;
            }
            '\n' => {
                if !cr {
                    line = line.checked_add(1).ok_or("header line overflow")?;
                }
                character = 0;
                cr = false;
            }
            _ => {
                character = character
                    .checked_add(ch.len_utf16() as u32)
                    .ok_or("header column overflow")?;
                cr = false;
            }
        }
    }
    Ok(lsp_types::Position::new(line, character))
}

pub(super) fn selected_import_header(
    workspace: &mut Workspace,
    importer: &Url,
    owning_context: (
        &ContextKey,
        &Arc<pascal_project::ProjectContext>,
        &pascal_project::ProjectObservations,
    ),
    selected_site: (&LoadedSource, &pascal_core::ResolvedImport),
    selection_report: &pascal_core::ResolutionReport,
    route: Option<&pascal_core::LegacyRoute>,
    cancel: &AtomicBool,
) -> Result<Option<lsp_types::Location>, String> {
    let (key, context, context_observations) = owning_context;
    let (selected, binding) = selected_site;
    use pascal_core::compile_time::{
        CompileTimeLimits, Fact, HEADER_MAX_BRANCH_STATES, HEADER_MAX_BYTE_WORK,
    };
    let control = Control {
        cancel,
        budget: None,
        deadline: None,
    };
    control.check()?;
    let input = workspace.analysis_input();
    let mut limits = ResolverLimits::default();
    limits.max_source_bytes = input.options.limits.max_file_bytes;
    limits.max_compile_time_byte_work = super::resolver::byte_work_cap(HEADER_MAX_BYTE_WORK);
    limits.max_compile_time_retained_bytes =
        lsp_compatibility_retention(input.options.limits, workspace.include_expansion_limits());
    let mut resolver = super::resolver::resolver_for_context_with_limits(
        context.clone(),
        context_observations,
        input.roots.clone(),
        &input,
        limits,
    );
    // Re-run the actual import request, not path_entry_for(selected.path).
    // Equal-depth overlapping entries can authenticate the same bytes under
    // different authorities. Selection must preserve its ordered entry, alias,
    // namespace, original importer directory and actual route; the resolver
    // reauthorizes/loads all current candidates and registers genuine routes.
    let importer_path = importer
        .to_file_path()
        .map_err(|_| "header importer is not a file URI")?;
    let importer_source = control.finish(resolver.load_source(
        &importer_path,
        route,
        pascal_core::SourceKind::Unit,
        &control,
    ))?;
    if importer_source.id != binding.importer_source_id {
        return Err(HEADER_CHANGED.into());
    }
    let outcome = control.finish(resolver.try_resolve_unit(
        pascal_core::UnitResolveRequest {
            requested_name: &binding.site.requested_name,
            importer_path: &importer_source.path,
            legacy_route: route,
        },
        &control,
    ))?;
    let pascal_core::Resolution::Found(unit) = outcome.result else {
        workspace.merge_resolution_report(key, &resolver.finish());
        return Ok(None);
    };
    let root = unit.source;
    if root != *selected {
        return Err(HEADER_CHANGED.into());
    }
    let conditional_context = context.conditional_context_for(&root.path);
    let used = resolver.compile_time_usage();
    let granted = resolver.compile_time_work_allowance();
    let granted_bytes = resolver.compile_time_byte_work_allowance();
    let mut adapter = HeaderResolver {
        inner: &mut resolver,
        captures: None,
    };
    let proof = control.finish(pascal_core::compile_time::verify_unit_header(
        &root,
        &conditional_context,
        &mut adapter,
        CompileTimeLimits {
            max_branch_states: HEADER_MAX_BRANCH_STATES,
            max_work: limits
                .max_compile_time_work
                .saturating_add(granted)
                .saturating_sub(used.work),
            max_byte_work: limits
                .max_compile_time_byte_work
                .saturating_add(granted_bytes)
                .saturating_sub(used.byte_work),
            max_retained_bytes: limits
                .max_compile_time_retained_bytes
                .saturating_sub(used.retained_bytes),
            ..Default::default()
        },
        &control,
    ))?;
    let Fact::Known(header) = proof else {
        return Ok(None);
    };
    let Some(first) = header.name_parts.first() else {
        return Ok(None);
    };
    let Some(last) = header.name_parts.last() else {
        return Ok(None);
    };
    let mut previous_end = first.range.start;
    for part in &header.name_parts {
        control.check()?;
        if part.source != first.source
            || part.occurrence != first.occurrence
            || part.range.start < previous_end
            || part.range.end < part.range.start
        {
            return Ok(None);
        }
        previous_end = part.range.end;
    }
    let mut physical = if root.id == first.source {
        Some(&root)
    } else {
        None
    };
    let mut capture = adapter.captures.as_deref();
    while let Some(current) = capture {
        control.check()?;
        if current.source.id == first.source {
            if physical.is_some_and(|source| {
                source.revision != current.source.revision
                    || source.decoded_text != current.source.decoded_text
            }) {
                return Ok(None);
            }
            physical = Some(&current.source);
        }
        capture = current.previous.as_deref().map(|owner| &**owner);
    }
    let Some(physical) = physical else {
        return Ok(None);
    };
    let Some(text) = physical.decoded_text.as_deref() else {
        return Ok(None);
    };
    if text.get(first.range.start..last.range.end).is_none() {
        return Ok(None);
    }
    // Separate finite mapping/output admission, not a foreign receipt transfer.
    let mut mapping_budget = CompileTimeBudget::new(
        CompileTimeLimits {
            max_byte_work: limits.max_compile_time_byte_work,
            max_retained_bytes: limits.max_compile_time_retained_bytes,
            ..Default::default()
        },
        &control,
    );
    let start = header_position(text, first.range.start, &mut mapping_budget)?;
    let end = header_position(text, last.range.end, &mut mapping_budget)?;
    let uri_bytes = physical
        .path
        .as_os_str()
        .len()
        .checked_mul(3)
        .and_then(|n| n.checked_add(32))
        .ok_or("header URI size overflow")?;
    let location = mapping_budget
        .admit_owned(
            1,
            uri_bytes,
            uri_bytes + std::mem::size_of::<lsp_types::Location>(),
            || {
                let uri = Url::from_file_path(&physical.path)
                    .map_err(|_| "invalid physical header URI")?;
                Ok::<_, &str>(lsp_types::Location::new(
                    uri,
                    lsp_types::Range::new(start, end),
                ))
            },
        )
        .map_err(|reason| header_admission_error(reason, &mapping_budget).to_string())?;
    let mut records = super::resolver::report_records(context, selection_report);
    for observation in &selection_report.observations {
        if let Some(record) = super::resolver::observation_record(observation) {
            records.push(record);
        }
    }
    records.push(super::resolver::source_record_for_loaded(
        context,
        &root,
        Url::from_file_path(&root.path).map_err(|_| "invalid root URI")?,
        false,
    )?);
    let mut capture = adapter.captures.as_deref();
    while let Some(current) = capture {
        control.check()?;
        let include_uri =
            Url::from_file_path(&current.source.path).map_err(|_| "invalid include URI")?;
        records.push(super::resolver::source_record_for_loaded(
            context,
            &current.source,
            include_uri.clone(),
            true,
        )?);
        workspace.record_resolved_source(context, &current.source, include_uri, true)?;
        capture = current.previous.as_deref().map(|owner| &**owner);
    }
    let captures = adapter.captures.take();
    drop(adapter);
    let report = resolver.finish();
    records.extend(super::resolver::report_records(context, &report));
    for observation in &report.observations {
        if let Some(record) = super::resolver::observation_record(observation) {
            records.push(record);
        }
    }
    if let Some(state) = workspace.contexts.get(key) {
        records.extend(super::rename::consumed_context_records(state, cancel)?);
    }
    if let Some(record) = input.overlays.get(importer)
        && workspace
            .open_documents
            .get(importer)
            .is_none_or(|current| {
                current.version != record.version
                    || current.text.as_deref() != Some(record.text.as_ref())
            })
    {
        return Err(HEADER_CHANGED.into());
    }
    #[cfg(test)]
    TEST_HEADER_REVALIDATION_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook();
        }
    });
    super::rename::revalidate_input(&workspace.analysis_input(), &records, cancel).map_err(
        |error| {
            // Only the existing validator's explicit changed-observation outcome
            // requests a retry. Read/authority/work/cancellation errors stay active.
            if error.ends_with("; retry the request") {
                HEADER_CHANGED.into()
            } else {
                error
            }
        },
    )?;
    workspace.record_resolved_source(
        context,
        &root,
        Url::from_file_path(&root.path).map_err(|_| "invalid root URI")?,
        false,
    )?;
    workspace.merge_resolution_report(key, &report);
    // Preserve the already authenticated owning project for a later request at
    // this physical source (including package-only/mapped providers). This is
    // existing context/read-route bookkeeping, not a parsed provider, semantic
    // import binding or cached header capability. Otherwise a subsequent body
    // request rediscovers the external source under an unrelated fallback scope.
    let root_uri = Url::from_file_path(&root.path).map_err(|_| "invalid root URI")?;
    workspace.remember_document_owner(&root_uri, key);
    let legacy = match &root.revision {
        SourceRevision::Disk { path_entry, .. } => matches!(
            path_entry.provenance,
            pascal_project::ProjectPathProvenance::LegacyNative
        ),
        SourceRevision::Overlay { .. } => context.path_entry_for(&root.path).is_some_and(|entry| {
            matches!(
                entry.provenance,
                pascal_project::ProjectPathProvenance::LegacyNative
            )
        }),
    };
    if route.is_some() && legacy {
        workspace.remember_legacy_route(&root_uri, key, &root.path);
    }
    control.check()?;
    let result = match &*location {
        Ok(location) => {
            let compatibility = mapping_budget
                .admit_owned(
                    2,
                    2 * location.uri.as_str().len(),
                    2 * (location.uri.as_str().len() + std::mem::size_of::<lsp_types::Location>()),
                    || location.clone(),
                )
                .map_err(|reason| header_admission_error(reason, &mapping_budget).to_string())?;
            // Bounded compatibility return, not a transferring receipt.
            Some((*compatibility).clone())
        }
        Err(error) => return Err((*error).into()),
    };
    drop(captures);
    Ok(result)
}

/// R22 compatibility policy, not a total-heap accounting claim. Keep the
/// standalone core fuse out of LSP-owned source/output domains, but retain a
/// finite admission ceiling derived only from existing configured size limits.
/// Runtime maxima also bound programmatic options (including usize::MAX).
pub(crate) fn lsp_compatibility_retention(
    resources: super::ResourceLimits,
    expansion: ExpansionLimits,
) -> usize {
    let source = resources
        .max_total_bytes
        .min(super::DEFAULT_MAX_TOTAL_BYTES);
    let document = resources
        .max_file_bytes
        .min(source)
        .min(super::MAX_FILE_BYTES_CEILING);
    let expanded = expansion
        .max_expanded_bytes
        .min(source)
        .min(ExpansionLimits::default().max_expanded_bytes);
    let cache = resources
        .max_cache_bytes
        .min(crate::project_cache::DEFAULT_MAX_CACHE_BYTES);
    document
        .checked_add(source)
        .and_then(|n| n.checked_add(expanded))
        .and_then(|n| n.checked_add(cache))
        .expect("existing LSP runtime size maxima fit usize")
}

pub(super) fn expand_with_limits(
    workspace: &Workspace,
    uri: &Url,
    source: &str,
    key: &ContextKey,
    limits: SessionLimits,
    cancel: &AtomicBool,
    budget: Option<&ReconciliationBudget>,
) -> Result<ExpansionResult, String> {
    let directory_limit = limits.directory_entries;
    let limits = limits.expansion;
    let control = Control {
        cancel,
        budget,
        deadline: workspace.member_deadline,
    };
    control.check()?;
    #[cfg(test)]
    if TEST_EXPANSION_FAILURE.with(|value| {
        value
            .borrow()
            .as_deref()
            .is_some_and(|suffix| uri.as_str().ends_with(suffix))
    }) {
        return Err("resolver limit max_byte_work exceeded: 2 > 1".into());
    }
    if !requires_session(source) {
        if source.len() > limits.max_expanded_bytes
            || source.len() > limits.max_work
            || limits.max_sources == 0
            || limits.max_segments == 0
        {
            return Err("mapped expansion limit reached".into());
        }
        let mut expanded = ExpandedSource::default();
        expanded.push_physical_range(uri.clone(), source, 0, None);
        expanded.conditionals = Some(ConditionalAnalysis {
            projected_source: source.into(),
            inactive_spans: vec![],
            unknown_spans: vec![],
            directives: vec![],
            complete: true,
        });
        control.check()?;
        return Ok(ExpansionResult {
            expanded,
            dependencies: vec![],
            complete: true,
            errors: vec![],
        });
    }
    let state = workspace
        .contexts
        .get(key)
        .ok_or("project context was not retained")?;
    let context = state.context.clone();
    let input = workspace.session_source_input();
    let source_hash = if input.overlays.contains_key(uri) {
        crate::project_cache::overlay_content_hash(source)
    } else {
        pascal_project::content_hash_bytes(source.as_bytes())
    };
    if let Some(unit) =
        workspace
            .project_cache
            .peek_unit(uri, &context, source_hash, &input.overlays)
        && let Some(expansion) = &unit.expansion
        && unit.disk.as_ref().map_or_else(
            || unit.parsed.source_text().as_ref(),
            |disk| disk.text.as_ref(),
        ) == source
    {
        control.check()?;
        return Ok((**expansion).clone());
    }
    let path = uri
        .to_file_path()
        .map_err(|_| "compile-time root is not a file URI")?;
    let route = workspace.legacy_route_for_resolver(uri, &path, key, &context);
    let mut resolver_limits = ResolverLimits::default();
    resolver_limits.max_compile_time_byte_work =
        super::resolver::byte_work_cap(resolver_limits.max_compile_time_byte_work);
    resolver_limits.max_compile_time_retained_bytes =
        lsp_compatibility_retention(workspace.options.limits, limits);
    resolver_limits.max_source_bytes = workspace.options.limits.max_file_bytes;
    resolver_limits.max_directory_entries =
        resolver_limits.max_directory_entries.min(directory_limit);
    resolver_limits.max_dependency_units = super::MAX_DEPENDENCY_WORK;
    resolver_limits.max_include_files = resolver_limits
        .max_include_files
        .min(limits.max_sources.saturating_sub(1));
    resolver_limits.max_include_bytes = resolver_limits
        .max_include_bytes
        .min(limits.max_expanded_bytes);
    resolver_limits.max_include_directives = resolver_limits
        .max_include_directives
        .min(limits.max_directives);
    resolver_limits.max_include_depth = resolver_limits.max_include_depth.min(limits.max_depth);
    let capture = Arc::new(super::capture::ReadCapture::default());
    let store = LspSourceStore::from_input(context.clone(), &input)
        .with_directory_limit(directory_limit)
        .with_capture(Arc::clone(&capture));
    let store = Resources {
        store,
        control: &control,
    };
    let configuration = provider_graph_configuration(workspace, &context, &input, &resolver_limits);
    let overlays = workspace.overlay_inputs();
    let shared = Arc::new(CachedProviderGraphs {
        cache: workspace.project_cache.clone(),
        configuration,
        overlays,
        capture: Arc::clone(&capture),
    });
    let include_failures = Arc::new(CachedIncludeFailures {
        cache: workspace.project_cache.clone(),
        configuration,
        context: Arc::clone(&context),
        capture: Arc::clone(&capture),
        searching: std::sync::Mutex::new(None),
    });
    let mut resolver = UnitResolver::new(
        context,
        workspace.workspace_root_paths(),
        store,
        resolver_limits,
    )
    .with_context_observations(&state.context_observations)
    .with_shared_provider_graphs(shared)
    .with_shared_include_failures(include_failures);
    let root = control.finish(resolver.load_source(
        &path,
        route.as_ref(),
        pascal_core::resolver::SourceKind::Unit,
        &control,
    ))?;
    if root.decoded_text.as_deref() != Some(source) {
        return Err("compile-time root changed before analysis".into());
    }
    // This existing entrypoint installs the authenticated root's legacy route
    // and derives its source-specific context before invoking analyze_source.
    #[cfg(test)]
    TEST_BODY_ANALYSES.with(|count| count.set(count.get() + 1));
    let mut analysis = control.finish(resolver.analyze_compile_time(&root, &control))?;
    // The expansion is cached and read for its projection only (l4d-4nbs
    // D44): drop the declarations and provider graphs the session holds.
    #[cfg(test)]
    let release = !TEST_KEEP_DECLARATIONS.with(std::cell::Cell::get);
    #[cfg(not(test))]
    let release = true;
    if release {
        control.finish(analysis.release_declarations(&control))?;
    }
    if let Some(b) = budget {
        b.check_cancelled()?;
    }
    let mut result = control.finish(mapped(Arc::new(analysis), limits, &control))?;
    // Core revisions hash exact payload bytes; LSP overlay probes use the
    // existing framed text hash. Convert only against the captured overlay
    // whose version and exact byte hash the session actually observed.
    for observation in &mut result.expanded.observations {
        control.check()?;
        if let Some(version) = observation.overlay_version {
            let uri =
                Url::from_file_path(&observation.path).map_err(|_| "invalid overlay proof path")?;
            let overlay = input
                .overlays
                .get(&uri)
                .ok_or("compile-time overlay proof is no longer current")?;
            if overlay.version != version
                || observation.content_hash
                    != Some(pascal_project::content_hash_bytes(overlay.text.as_bytes()))
            {
                return Err("compile-time overlay proof changed".into());
            }
            observation.content_hash =
                Some(crate::project_cache::overlay_content_hash(&overlay.text));
        }
    }
    result.expanded.reads = Some(Arc::new(capture.snapshot()));
    Ok(result)
}

fn observation(value: &ResolutionObservation) -> Option<IncludeObservation> {
    match value {
        ResolutionObservation::Directory { path, stamp, .. } => Some(IncludeObservation {
            path: path.clone(),
            stamp: stamp.clone(),
            present: stamp.is_some(),
            overlay_version: None,
            content_hash: None,
        }),
        ResolutionObservation::Candidate {
            path,
            stamp,
            present,
            ..
        } => Some(IncludeObservation {
            path: path.clone(),
            stamp: stamp.clone(),
            present: *present,
            overlay_version: None,
            content_hash: None,
        }),
        ResolutionObservation::Payload { path, revision, .. } => Some(match revision {
            SourceRevision::Overlay {
                version,
                content_hash,
            } => IncludeObservation {
                path: path.clone(),
                stamp: None,
                present: true,
                overlay_version: Some(*version),
                content_hash: Some(*content_hash),
            },
            SourceRevision::Disk {
                stamp,
                content_hash,
                ..
            } => IncludeObservation {
                path: path.clone(),
                stamp: Some(stamp.clone()),
                present: true,
                overlay_version: None,
                content_hash: Some(*content_hash),
            },
        }),
        ResolutionObservation::Metadata(pascal_project::MetadataObservation::Stat { path }) => {
            Some(IncludeObservation {
                path: path.clone(),
                stamp: pascal_project::path_stamp_result(path).ok().flatten(),
                present: path.exists(),
                overlay_version: None,
                content_hash: None,
            })
        }
        ResolutionObservation::Metadata(pascal_project::MetadataObservation::Payload {
            path,
            stamp,
            content_hash,
            ..
        }) => Some(IncludeObservation {
            path: path.clone(),
            stamp: stamp.clone(),
            present: stamp.is_some(),
            overlay_version: None,
            content_hash: Some(*content_hash),
        }),
        ResolutionObservation::ProjectRead(read) => Some(IncludeObservation {
            path: read.path.clone(),
            stamp: Some(read.stamp.clone()),
            present: true,
            overlay_version: None,
            content_hash: Some(read.content_hash),
        }),
    }
}

pub(crate) fn mapped(
    analysis: Arc<CompileTimeAnalysis>,
    limits: ExpansionLimits,
    cancel: &dyn CancellationToken,
) -> Result<ExpansionResult, String> {
    let mut result = ExpansionResult {
        expanded: ExpandedSource::default(),
        dependencies: vec![],
        complete: true,
        errors: vec![],
    };
    let mut projection = ConditionalAnalysis {
        projected_source: String::new(),
        inactive_spans: vec![],
        unknown_spans: vec![],
        directives: vec![],
        complete: true,
    };
    let root = analysis
        .occurrences
        .first()
        .ok_or("compile-time session has no root occurrence")?
        .id;
    {
        let mut mapping = Mapping {
            analysis: &analysis,
            limits,
            cancel,
            work: 0,
            sources: 0,
            directives: 0,
            result: &mut result,
            projection: &mut projection,
        };
        mapping.append_occurrence(root, 0)?;
        mapping.charge_work(analysis.observations.len())?;
    }
    result.complete &= projection.complete && !projection.unknown_activity_requires_fail_closed();
    if !result.complete {
        result.errors.extend(analysis.warnings.iter().cloned());
        result
            .errors
            .extend(analysis.reasons.iter().map(|r| format!("{r:?}")));
        result.errors.push(
            if projection.unknown_activity_requires_fail_closed() {
                "shared compile-time analysis has unknown conditional compilation activity"
            } else {
                "shared compile-time analysis is incomplete"
            }
            .into(),
        );
    }
    for value in &analysis.observations {
        if cancel.is_cancelled() {
            return Err(super::CANCELLATION_MESSAGE.into());
        }
        if let Some(observation) = observation(value) {
            result.expanded.observations.push(observation);
        }
    }
    result.expanded.conditionals = Some(projection);
    result.expanded.session = Some(analysis);
    Ok(result)
}

struct Mapping<'a> {
    analysis: &'a CompileTimeAnalysis,
    limits: ExpansionLimits,
    cancel: &'a dyn CancellationToken,
    work: usize,
    sources: usize,
    directives: usize,
    result: &'a mut ExpansionResult,
    projection: &'a mut ConditionalAnalysis,
}

impl Mapping<'_> {
    fn charge_output(&mut self, bytes: usize) -> Result<(), String> {
        if self.cancel.is_cancelled() {
            return Err(super::CANCELLATION_MESSAGE.into());
        }
        self.work = self
            .work
            .checked_add(bytes + 1)
            .ok_or("mapped expansion work overflow")?;
        if self.work > self.limits.max_work {
            return Err("mapped expansion work limit reached".into());
        }
        if self.result.expanded.text().len().saturating_add(bytes) > self.limits.max_expanded_bytes
        {
            return Err("mapped expansion byte limit reached".into());
        }
        if bytes != 0 && self.result.expanded.segment_count() >= self.limits.max_segments {
            return Err("include source-map segment limit reached".into());
        }
        Ok(())
    }

    fn charge_work(&mut self, work: usize) -> Result<(), String> {
        if self.cancel.is_cancelled() {
            return Err(super::CANCELLATION_MESSAGE.into());
        }
        self.work = self
            .work
            .checked_add(work)
            .ok_or("mapped expansion work overflow")?;
        if self.work > self.limits.max_work {
            return Err("mapped expansion work limit reached".into());
        }
        Ok(())
    }

    fn synthetic(&mut self, source: &str) -> Result<(), String> {
        self.charge_output(source.len())?;
        self.result.expanded.push_synthetic(source);
        self.projection.projected_source.push_str(source);
        Ok(())
    }

    fn append_occurrence(&mut self, id: OccurrenceId, depth: usize) -> Result<(), String> {
        self.charge_output(0)?;
        if depth > self.limits.max_depth {
            return Err("include expansion depth limit reached".into());
        }
        if self.sources >= self.limits.max_sources {
            return Err("include expansion source limit reached".into());
        }
        self.sources += 1;
        let analysis = self.analysis;
        self.charge_work(
            analysis
                .occurrences
                .len()
                .saturating_add(analysis.includes.len()),
        )?;
        let occurrence = analysis
            .occurrences
            .iter()
            .find(|o| o.id == id)
            .ok_or("missing compile-time occurrence")?;
        let source = decoded_source(&occurrence.source)?;
        let uri =
            Url::from_file_path(&occurrence.source.path).map_err(|_| "invalid source path")?;
        self.directives = self
            .directives
            .checked_add(occurrence.analysis.directives.len())
            .ok_or("mapped directive count overflow")?;
        if self.directives > self.limits.max_directives {
            return Err("include directive limit reached".into());
        }
        self.projection.complete &= occurrence.analysis.complete;
        let mut cursor = 0;
        for include in analysis.includes.iter().filter(|i| i.owner == id) {
            let range = &include.directive.range;
            if range.start < cursor || range.end > source.len() {
                return Err("non-physical include directive requires a mapped continuation".into());
            }
            self.append_region(&uri, id, source, &occurrence.analysis, cursor..range.start)?;
            let marker = mask(&source[range.clone()]);
            self.synthetic(&marker)?;
            let inactive = occurrence.analysis.directives.iter().any(|d| {
                d.start == range.start
                    && d.end == range.end
                    && d.activity == pascal_core::conditional::Truth::False
            });
            if let Some(child) = include.child {
                self.charge_work(analysis.occurrences.len())?;
                let child_source = analysis
                    .occurrences
                    .iter()
                    .find(|o| o.id == child)
                    .ok_or("missing include child")?;
                if self.sources >= self.limits.max_sources {
                    return Err("include expansion source limit reached".into());
                }
                let child_text = decoded_source(&child_source.source)?;
                if child_text.len() > self.limits.max_work.saturating_sub(self.work) {
                    return Err("mapped expansion work limit reached".into());
                }
                if child_text.len()
                    > self
                        .limits
                        .max_expanded_bytes
                        .saturating_sub(self.result.expanded.text().len())
                {
                    return Err("mapped expansion byte limit reached".into());
                }
                let entry = match &child_source.source.revision {
                    SourceRevision::Disk { path_entry, .. } => Some(path_entry.clone()),
                    _ => None,
                };
                self.result.dependencies.push(ResolvedInclude {
                    uri: Url::from_file_path(&child_source.source.path)
                        .map_err(|_| "invalid include path")?,
                    text: child_text.into(),
                    path_entry: entry,
                    observations: vec![],
                });
                self.append_occurrence(child, depth + 1)?;
            } else if !inactive {
                // A missing/unknown child is a barrier, not a successful EOF.
                self.result.complete = false;
                self.synthetic("\n{ unresolved include }\n")?;
            }
            cursor = range.end;
        }
        self.append_region(&uri, id, source, &occurrence.analysis, cursor..source.len())
    }

    fn append_region(
        &mut self,
        uri: &Url,
        occurrence: OccurrenceId,
        source: &str,
        decisions: &ConditionalAnalysis,
        range: Range<usize>,
    ) -> Result<(), String> {
        self.charge_output(range.len())?;
        self.charge_work(
            decisions
                .inactive_spans
                .len()
                .saturating_add(decisions.unknown_spans.len())
                .saturating_add(decisions.directives.len()),
        )?;
        let start = self.result.expanded.text().len();
        self.result.expanded.push_physical_range(
            uri.clone(),
            &source[range.clone()],
            range.start,
            Some(occurrence),
        );
        self.projection
            .projected_source
            .push_str(&decisions.projected_source[range.clone()]);
        for (spans, output) in [
            (
                &decisions.inactive_spans,
                &mut self.projection.inactive_spans,
            ),
            (&decisions.unknown_spans, &mut self.projection.unknown_spans),
        ] {
            for span in spans {
                let a = span.start.max(range.start);
                let b = span.end.min(range.end);
                if a < b {
                    output.push(start + a - range.start..start + b - range.start);
                }
            }
        }
        for directive in &decisions.directives {
            if directive.start >= range.start && directive.end <= range.end {
                let mut d = directive.clone();
                d.start = start + d.start - range.start;
                d.end = start + d.end - range.start;
                self.projection.directives.push(d);
            }
        }
        Ok(())
    }
}

fn mask(source: &str) -> String {
    source
        .bytes()
        .map(|b| {
            if b == b'\n' || b == b'\r' {
                b as char
            } else {
                ' '
            }
        })
        .collect()
}

fn decoded_source(source: &LoadedSource) -> Result<&str, String> {
    source
        .decoded_text
        .as_deref()
        .or_else(|| std::str::from_utf8(&source.bytes).ok())
        .ok_or_else(|| "source has no decoded coordinate model".into())
}

pub(super) fn requires_session(source: &str) -> bool {
    source.contains("{$") || source.contains("(*$")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NavigationTarget;
    use lsp_types::Position;
    use pascal_project::delphi_overrides::OverrideSession;

    /// 13c-1 review: a read the capture bound dropped and only the graph's
    /// trace names still makes the graph's reads overflowed (`put` stores
    /// the reads of observations and trace), so `put_provider_graph`
    /// refuses it; no later session reuses it and each compiles the
    /// provider itself.
    #[test]
    fn a_read_dropped_by_the_capture_bound_and_named_by_the_trace_refuses_the_graph() {
        use pascal_core::SourceId;
        use pascal_core::compile_time::{ProviderGraphKey, SharedProviderGraph};
        use pascal_core::resolver::{DirectoryListing, ResolutionObservation};
        let directory = |path: &str| ResolutionObservation::Directory {
            path: path.into(),
            entry: pascal_project::ProjectPathEntry::configured(path.into()),
            stamp: None,
            complete: true,
            listed: true,
            resolvable: None,
        };
        let listing = DirectoryListing {
            files: Vec::new(),
            directories: Vec::new(),
            stamp: None,
            complete: true,
        };
        let key = ProviderGraphKey {
            source: SourceId::new("/p/Prov.pas"),
            content_hash: 1,
            disk_stamp: None,
            overlay_version: Some(1),
            context: Default::default(),
            qualifiers: vec![],
        };
        let graph = |trace: Vec<ResolutionObservation>| {
            Arc::new(SharedProviderGraph {
                occurrence: OccurrenceId(1),
                graph: None,
                observations: Arc::new(vec![directory("/w/obs")]),
                warnings: Default::default(),
                dependencies: Default::default(),
                scanned: Default::default(),
                children: Default::default(),
                trace: Arc::new(trace),
                revision: pascal_core::SourceRevision::Overlay {
                    version: 1,
                    content_hash: 1,
                },
                lookup: (0, 0),
                names: None,
                retained_bytes: 100,
            })
        };
        let cache = crate::project_cache::ProjectCache::new(1 << 20);
        let capture = Arc::new(crate::workspace::capture::ReadCapture::default());
        crate::workspace::capture::set_capture_bound_for_test(Some(1));
        // The first read fits; `/w/gen` is dropped.
        capture.listed(std::path::Path::new("/w/obs"), &listing, false);
        capture.listed(std::path::Path::new("/w/gen"), &listing, false);
        crate::workspace::capture::set_capture_bound_for_test(None);
        assert!(capture.snapshot().overflowed);
        let graphs = CachedProviderGraphs {
            cache: cache.clone(),
            configuration: 3,
            overlays: Default::default(),
            capture: Arc::clone(&capture),
        };
        use pascal_core::compile_time::SharedProviderGraphs;
        // Named by observations alone, the dropped read is not missing.
        graphs.put(key.clone(), graph(Vec::new()));
        assert_eq!(cache.provider_graph_count(), 1);
        assert_eq!(cache.index_process_counts().provider_graphs_refused, 0);
        // Named by the trace, it is: the graph is refused and counted.
        let traced = ProviderGraphKey {
            source: SourceId::new("/p/Traced.pas"),
            ..key.clone()
        };
        graphs.put(traced.clone(), graph(vec![directory("/w/gen")]));
        assert_eq!(cache.index_process_counts().provider_graphs_refused, 1);
        assert_eq!(cache.provider_graph_count(), 1);
        assert!(graphs.get(&traced).is_none(), "the consumer compiles it");
        assert!(graphs.get(&key).is_some());
    }

    /// l4d-10: SFClient2's business object interfaces probe the same
    /// candidate paths in their observations, their trace and their nested
    /// providers' lookups; the deduplicated probes kept the capacity of all
    /// of them (BaseREPAIRHEADER: 7.4 MB kept, 0.15 MB charged). The store
    /// charges what it keeps for an interface's probes and reads.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_shared_interface_with_repeated_probes_is_charged_within_a_fifth_of_its_heap() {
        use pascal_core::SourceId;
        use pascal_core::compile_time::{ProviderGraphKey, SharedProviderGraph};
        use pascal_core::resolver::ResolutionObservation;
        let candidate = |n: usize| ResolutionObservation::Candidate {
            path: format!("/w/Common/BOBase/Source{:02}/BaseTABLE{n:04}.pas", n % 30).into(),
            entry: None,
            stamp: None,
            present: false,
        };
        // 40 lookups seen 200 times each.
        let observations = Arc::new((0..8_000).map(|n| candidate(n % 40)).collect::<Vec<_>>());
        let graph = Arc::new(SharedProviderGraph {
            occurrence: OccurrenceId(1),
            graph: None,
            // Kept alive by the test: only what the store adds is measured.
            observations: Arc::clone(&observations),
            warnings: Default::default(),
            dependencies: Default::default(),
            scanned: Default::default(),
            children: Default::default(),
            trace: Arc::clone(&observations),
            revision: pascal_core::SourceRevision::Overlay {
                version: 1,
                content_hash: 1,
            },
            lookup: (0, 0),
            names: None,
            // What the graph's own estimate counts for its struct.
            retained_bytes: std::mem::size_of::<SharedProviderGraph>(),
        });
        let key = ProviderGraphKey {
            source: SourceId::new("/w/Common/BOBase/BaseREPAIRHEADER.pas"),
            content_hash: 1,
            disk_stamp: None,
            overlay_version: Some(1),
            context: Default::default(),
            qualifiers: vec![],
        };
        let cache = crate::project_cache::ProjectCache::new(1 << 30);
        let graphs = CachedProviderGraphs {
            cache: cache.clone(),
            configuration: 3,
            overlays: Default::default(),
            capture: Arc::new(crate::workspace::capture::ReadCapture::default()),
        };
        use pascal_core::compile_time::SharedProviderGraphs;
        graphs.put(key, graph);
        assert_eq!(cache.provider_graph_count(), 1);
        let charged = cache.provider_graph_bytes();
        let (freed, holders) = cache.drop_provider_graphs_for_test();
        assert_eq!(holders, vec![0]);
        let error = charged.abs_diff(freed as usize) as f64 / freed as f64;
        assert!(
            error <= 0.2,
            "charged {charged} bytes for an interface that kept {freed} ({:.0}% off)",
            error * 100.0
        );
    }

    /// l4d-10: an interface compiled through nested providers collects each
    /// provider it read once per nesting; deduplicated, the list kept the
    /// room of every repeat (BaseREPAIRHEADER: 5.3 MB of dependencies).
    #[cfg(target_os = "linux")]
    #[test]
    fn shared_interfaces_keep_no_room_for_repeated_dependencies() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = "unit Main; interface uses A; {$IF A.Flag = 8} const Eight = 8; {$IFEND} implementation end.";
        std::fs::write(root.join("Main.pas"), source).unwrap();
        std::fs::write(
            root.join("C.pas"),
            "unit C; interface const F = 1; implementation end.",
        )
        .unwrap();
        let middles = (0..12).map(|n| format!("B{n}")).collect::<Vec<_>>();
        for name in &middles {
            std::fs::write(
                root.join(format!("{name}.pas")),
                format!("unit {name}; interface uses C; const {{$IF C.F = 1}} F = 1; {{$ELSE}} F = 0; {{$IFEND}} implementation end."),
            )
            .unwrap();
        }
        let checks = middles
            .iter()
            .map(|name| format!("{{$IF {name}.F = 1}} {name}Ok = 1; {{$IFEND}}"))
            .collect::<String>();
        std::fs::write(
            root.join("A.pas"),
            format!(
                "unit A; interface uses C, {}; const {checks} Flag = 8; implementation end.",
                middles.join(", ")
            ),
        )
        .unwrap();
        pascal_project::backdate_tree_for_test(root);
        let mut workspace = Workspace::with_override_session(
            vec![root.into()],
            Default::default(),
            OverrideSession::new(None),
        );
        let uri = Url::from_file_path(root.join("Main.pas")).unwrap();
        let key = workspace.context_for_uri_with_cancel(&uri, None).unwrap();
        drop(expand_main(&workspace, &uri, source, &key));
        let graphs = workspace.project_cache.provider_graphs_for_test();
        assert!(graphs.len() > 2, "{} shared interfaces", graphs.len());
        for graph in graphs {
            assert_eq!(
                graph.dependencies.capacity(),
                graph.dependencies.len(),
                "{} keeps room for {} dependencies, holds {}",
                graph.occurrence.0,
                graph.dependencies.capacity(),
                graph.dependencies.len()
            );
        }
    }

    fn provider_workspace(root: &std::path::Path) -> (Workspace, Url, String, ContextKey) {
        let source = "unit Main; interface uses A; {$IF A.Flag = 8} const Eight = 8; {$ELSE} const Other = 0; {$ENDIF} implementation end.";
        std::fs::write(root.join("Main.pas"), source).unwrap();
        let declarations = (0..2_000)
            .map(|n| format!("C{n} = {n};\n"))
            .collect::<String>();
        std::fs::write(
            root.join("A.pas"),
            format!("unit A; interface const\n{declarations}Flag = 8; implementation end."),
        )
        .unwrap();
        let mut workspace = Workspace::with_override_session(
            vec![root.into()],
            Default::default(),
            OverrideSession::new(None),
        );
        let uri = Url::from_file_path(root.join("Main.pas")).unwrap();
        let key = workspace.context_for_uri_with_cancel(&uri, None).unwrap();
        (workspace, uri, source.into(), key)
    }

    fn expand_main(
        workspace: &Workspace,
        uri: &Url,
        source: &str,
        key: &ContextKey,
    ) -> ExpansionResult {
        let cancel = AtomicBool::new(false);
        let result = expand(
            workspace,
            uri,
            source,
            key,
            workspace.include_expansion_limits(),
            &cancel,
            None,
        )
        .unwrap();
        let session = result.expanded.session.as_ref().unwrap();
        assert!(session.root.complete, "{:?}", session.reasons);
        result
    }

    /// `count` units in `root/lib` that each include `Missing.inc`, which
    /// is on no search path.
    fn missing_include_workspace(
        root: &std::path::Path,
        count: usize,
    ) -> (Workspace, Vec<(Url, String, ContextKey)>) {
        let lib = root.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let mut units = Vec::new();
        for n in 0..count {
            let source = format!(
                "unit U{n}; interface {{$I Missing.inc}} const C{n} = {n}; implementation end."
            );
            std::fs::write(lib.join(format!("U{n}.pas")), &source).unwrap();
            units.push((
                Url::from_file_path(lib.join(format!("U{n}.pas"))).unwrap(),
                source,
            ));
        }
        let mut workspace = Workspace::with_override_session(
            vec![root.into()],
            Default::default(),
            OverrideSession::new(None),
        );
        let units = units
            .into_iter()
            .map(|(uri, source)| {
                let key = workspace.context_for_uri_with_cancel(&uri, None).unwrap();
                (uri, source, key)
            })
            .collect();
        (workspace, units)
    }

    fn expand_unit(
        workspace: &Workspace,
        uri: &Url,
        source: &str,
        key: &ContextKey,
    ) -> ExpansionResult {
        expand(
            workspace,
            uri,
            source,
            key,
            workspace.include_expansion_limits(),
            &AtomicBool::new(false),
            None,
        )
        .unwrap()
    }

    /// What an expansion reports, for comparing two of them.
    fn outcome(result: &ExpansionResult) -> impl PartialEq + std::fmt::Debug {
        let session = result.expanded.session.as_ref().unwrap();
        (
            result.complete,
            result.errors.clone(),
            result.expanded.text().to_string(),
            session.root.complete,
            session.reasons.clone(),
            session.observations.clone(),
            result.expanded.reads.clone(),
        )
    }

    /// l4d-ig6b: units of one directory that include a file missing from
    /// their search path search for it once, not once per unit; the others
    /// replay that failure and report exactly what a search reports.
    #[test]
    fn units_including_one_missing_file_search_for_it_once() {
        let temp = tempfile::tempdir().unwrap();
        let (workspace, units) = missing_include_workspace(temp.path(), 5);
        pascal_project::backdate_tree_for_test(temp.path());
        pascal_core::take_include_searches();
        let replayed = units
            .iter()
            .map(|(uri, source, key)| expand_unit(&workspace, uri, source, key))
            .collect::<Vec<_>>();
        assert_eq!(
            pascal_core::take_include_searches(),
            1,
            "one search for the directory and name"
        );
        assert_eq!(workspace.project_cache.include_failure_count(), 1);
        // A workspace that shares nothing searches for each unit and must
        // report the same.
        for ((uri, source, _), replayed) in units.iter().zip(&replayed) {
            let (mut fresh, _) = missing_include_workspace(temp.path(), 0);
            let key = fresh.context_for_uri_with_cancel(uri, None).unwrap();
            let searched = expand_unit(&fresh, uri, source, &key);
            assert_eq!(outcome(&searched), outcome(replayed), "{uri}");
        }
    }

    /// l4d-ig6b: a remembered failure does not hide an include file created
    /// after it: the directory it would be found in changed.
    #[test]
    fn an_include_created_after_a_failed_lookup_is_found() {
        let temp = tempfile::tempdir().unwrap();
        let (workspace, units) = missing_include_workspace(temp.path(), 2);
        pascal_project::backdate_tree_for_test(temp.path());
        let (uri, source, key) = &units[0];
        let failed = expand_unit(&workspace, uri, source, key);
        assert_eq!(workspace.project_cache.include_failure_count(), 1);
        assert!(!failed.expanded.text().contains("FromInclude"));
        std::fs::write(
            temp.path().join("lib").join("Missing.inc"),
            "const FromInclude = 1;",
        )
        .unwrap();
        pascal_core::take_include_searches();
        let (uri, source, key) = &units[1];
        let found = expand_unit(&workspace, uri, source, key);
        assert_eq!(pascal_core::take_include_searches(), 1);
        assert!(
            found.expanded.text().contains("FromInclude"),
            "{}",
            found.expanded.text()
        );
    }

    /// A workspace over `files` (written under `root`, then made settled),
    /// with the contexts of `units`.
    fn fixture_workspace(
        root: &std::path::Path,
        files: &[(&str, &str)],
        units: &[&str],
    ) -> (Workspace, Vec<(Url, String, ContextKey)>) {
        for (path, text) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        pascal_project::backdate_tree_for_test(root);
        fixture_contexts(root, units)
    }

    fn fixture_contexts(
        root: &std::path::Path,
        units: &[&str],
    ) -> (Workspace, Vec<(Url, String, ContextKey)>) {
        let mut workspace = Workspace::with_override_session(
            vec![root.into()],
            Default::default(),
            OverrideSession::new(None),
        );
        let units = units
            .iter()
            .map(|unit| {
                let path = root.join(unit);
                let uri = Url::from_file_path(&path).unwrap();
                let key = workspace.context_for_uri_with_cancel(&uri, None).unwrap();
                (uri, std::fs::read_to_string(path).unwrap(), key)
            })
            .collect();
        (workspace, units)
    }

    fn project_files<'a>(
        include_path: &str,
        units: &[(&'a str, &'a str)],
    ) -> Vec<(String, String)> {
        let uses = units
            .iter()
            .map(|(path, _)| {
                let name = std::path::Path::new(path)
                    .file_stem()
                    .unwrap()
                    .to_string_lossy();
                format!("{name} in '{}'", path.replace('/', "\\"))
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut files = vec![
            (
                "App.dproj".to_string(),
                format!(
                    "<Project><PropertyGroup><MainSource>App.dpr</MainSource><DCC_IncludePath>{include_path}</DCC_IncludePath></PropertyGroup></Project>"
                ),
            ),
            (
                "App.dpr".to_string(),
                format!("program App; uses {uses}; begin end."),
            ),
        ];
        files.extend(
            units
                .iter()
                .map(|(path, text)| ((*path).to_string(), (*text).to_string())),
        );
        files
    }

    fn as_refs(files: &[(String, String)]) -> Vec<(&str, &str)> {
        files
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect()
    }

    /// l4d-ig6b I1: a failure whose directories are not settled is neither
    /// stored nor reused: an entry created in the stamp's tick could hide.
    #[test]
    fn failures_in_unsettled_directories_are_searched_again() {
        let temp = tempfile::tempdir().unwrap();
        let (workspace, units) = missing_include_workspace(temp.path(), 3);
        pascal_core::take_include_searches();
        for (uri, source, key) in &units {
            expand_unit(&workspace, uri, source, key);
        }
        assert_eq!(pascal_core::take_include_searches(), 3);
        assert_eq!(workspace.project_cache.include_failure_count(), 0);
    }

    /// l4d-ig6b I1: a watcher event under a directory a failure observed,
    /// or a watcher overflow, drops it, whatever the stamps say.
    #[test]
    fn watched_changes_drop_the_failures_they_touch() {
        let temp = tempfile::tempdir().unwrap();
        let (workspace, units) = missing_include_workspace(temp.path(), 3);
        pascal_project::backdate_tree_for_test(temp.path());
        let expand_at = |n: usize| {
            let (uri, source, key) = &units[n];
            expand_unit(&workspace, uri, source, key)
        };
        expand_at(0);
        assert_eq!(workspace.project_cache.include_failure_count(), 1);
        workspace
            .project_cache
            .invalidate_path(&temp.path().join("lib").join("Missing.inc"));
        assert_eq!(workspace.project_cache.include_failure_count(), 0);
        pascal_core::take_include_searches();
        expand_at(1);
        assert_eq!(pascal_core::take_include_searches(), 1);
        assert_eq!(workspace.project_cache.include_failure_count(), 1);
        workspace.project_cache.invalidate_after_overflow();
        assert_eq!(workspace.project_cache.include_failure_count(), 0);
        expand_at(2);
        assert_eq!(pascal_core::take_include_searches(), 1);
    }

    /// l4d-ig6b C1: a failure is not reused once the project's include path
    /// names the directory holding the file; neither is a provider interface
    /// compiled without it.
    #[test]
    fn a_new_include_path_finds_what_a_failure_missed() {
        let temp = tempfile::tempdir().unwrap();
        let main = "unit Main; interface uses A; {$IF A.Flag = 8} const Eight = 8; {$ELSE} const Other = 0; {$ENDIF} implementation end.";
        let units = [
            (
                "lib/U0.pas",
                "unit U0; interface {$I X.inc} implementation end.",
            ),
            (
                "lib/U1.pas",
                "unit U1; interface {$I X.inc} implementation end.",
            ),
            (
                "lib/A.pas",
                "unit A; interface {$I Flag.inc} implementation end.",
            ),
            ("lib/Main.pas", main),
        ];
        let mut files = project_files("", &units);
        files.push(("inc/X.inc".into(), "const FromInclude = 1;".into()));
        files.push(("inc/Flag.inc".into(), "const Flag = 8;".into()));
        let (before, contexts) = fixture_workspace(
            temp.path(),
            &as_refs(&files),
            &["lib/U0.pas", "lib/Main.pas"],
        );
        for (uri, source, key) in &contexts {
            expand_unit(&before, uri, source, key);
        }
        assert!(before.project_cache.include_failure_count() > 0);
        std::fs::write(
            temp.path().join("App.dproj"),
            &project_files("inc", &units)[0].1,
        )
        .unwrap();
        // Only the project file changed: every directory the failures
        // observed keeps its stamp.
        pascal_project::backdate_for_test(&temp.path().join("App.dproj"));
        // The next request: new contexts, the same session cache.
        let (mut after, contexts) = fixture_contexts(temp.path(), &["lib/U1.pas", "lib/Main.pas"]);
        after.project_cache = before.project_cache.clone();
        let (fresh, _) = fixture_contexts(temp.path(), &["lib/U1.pas", "lib/Main.pas"]);
        for (uri, source, key) in &contexts {
            let reused = expand_unit(&after, uri, source, key);
            let searched = expand_unit(&fresh, uri, source, key);
            assert_eq!(outcome(&reused), outcome(&searched), "{uri}");
        }
        let (uri, source, key) = &contexts[0];
        assert!(
            expand_unit(&after, uri, source, key)
                .expanded
                .text()
                .contains("FromInclude")
        );
    }

    /// l4d-ig6b I2, M1: a replayed failure reports, captures and charges what
    /// a search in a session that shares nothing does: over several search
    /// directories, with the storing unit open in the editor, and replayed
    /// twice in one session.
    #[test]
    fn replayed_failures_match_a_search_in_every_respect() {
        let temp = tempfile::tempdir().unwrap();
        // U1's session looks Missing.inc up twice: once for each provider
        // whose interface its conditions read.
        let units = [
            (
                "lib/U0.pas",
                "unit U0; interface {$I Missing.inc} implementation end.",
            ),
            (
                "lib/U1.pas",
                "unit U1; interface uses P0, P1; {$IF P0.X = 1} const A = 1; {$ENDIF} {$IF P1.X = 1} const B = 1; {$ENDIF} implementation end.",
            ),
            (
                "lib/U2.pas",
                "unit U2; interface {$I Missing.inc} implementation end.",
            ),
            (
                "lib/P0.pas",
                "unit P0; interface {$I Missing.inc} const X = 1; implementation end.",
            ),
            (
                "lib/P1.pas",
                "unit P1; interface {$I Missing.inc} const X = 1; implementation end.",
            ),
        ];
        let mut files = project_files("inc1;inc2", &units);
        files.push(("inc1/Other.inc".into(), "".into()));
        files.push(("inc2/Other.inc".into(), "".into()));
        let paths = ["lib/U0.pas", "lib/U1.pas", "lib/U2.pas"];
        let (mut shared, contexts) = fixture_workspace(temp.path(), &as_refs(&files), &paths);
        let (uri, source, _) = &contexts[0];
        shared
            .open_document(uri.clone(), format!("{source} "), 1)
            .unwrap();
        let open_text = format!("{source} ");
        let (first, rest) = contexts.split_first().unwrap();
        expand_unit(&shared, &first.0, &open_text, &first.2);
        assert_eq!(shared.project_cache.include_failure_count(), 1);
        pascal_core::take_include_searches();
        for (uri, source, key) in rest {
            let replayed = expand_unit(&shared, uri, source, key);
            let searches = pascal_core::take_include_searches();
            // The reference: a session that shares nothing; the storing
            // unit's buffer does not shape this unit's lookups.
            let (fresh, _) = fixture_contexts(temp.path(), &paths);
            let searched = expand_unit(&fresh, uri, source, key);
            assert!(pascal_core::take_include_searches() > 0);
            assert_eq!(searches, 0, "{uri} replays");
            // What evidence reads: each distinct listing and payload, and the
            // session flags (a duplicate read means the same).
            let distinct = |result: &ExpansionResult| {
                let reads = result.expanded.reads.as_deref().cloned().unwrap();
                let mut listings = reads
                    .listings
                    .iter()
                    .map(|listing| format!("{listing:?}"))
                    .collect::<Vec<_>>();
                listings.sort();
                listings.dedup();
                let mut payloads = reads
                    .payloads
                    .iter()
                    .map(|payload| format!("{payload:?}"))
                    .collect::<Vec<_>>();
                payloads.sort();
                payloads.dedup();
                (listings, payloads, reads.overflowed, reads.editor_state)
            };
            assert_eq!(
                distinct(&replayed),
                distinct(&searched),
                "{uri} captures alike"
            );
            let duplicates = |result: &ExpansionResult| {
                let all = result.expanded.reads.as_deref().unwrap().listings.len();
                all - distinct(result).0.len()
            };
            assert!(
                duplicates(&replayed) <= duplicates(&searched),
                "{uri}: a replay adds no listing the session holds"
            );
            let session = |result: &ExpansionResult| {
                let session = result.expanded.session.as_ref().unwrap();
                (
                    result.complete,
                    result.errors.clone(),
                    result.expanded.text().to_string(),
                    session.root.complete,
                    session.reasons.clone(),
                    session.observations.clone(),
                )
            };
            assert_eq!(session(&replayed), session(&searched), "{uri}");
            let usage = |result: &ExpansionResult| {
                let snapshot = result
                    .expanded
                    .session
                    .as_ref()
                    .unwrap()
                    .budget
                    .budget_snapshot();
                (snapshot.work, snapshot.byte_work)
            };
            // A search is charged what its session's state makes it cost (a
            // listing the session holds is reused): a replay charges what the
            // storing search was charged, which for one lookup per session is
            // that of a fresh search to within the bytes of its source name.
            if source.contains("{$I ") {
                let (replay, search) = (usage(&replayed), usage(&searched));
                let close = |left: usize, right: usize| left.abs_diff(right) * 100 <= right;
                assert!(
                    close(replay.0, search.0) && close(replay.1, search.1),
                    "{uri} is charged {replay:?}, a search {search:?}"
                );
            }
            let reads = replayed.expanded.reads.as_ref().unwrap();
            assert!(!reads.editor_state && !reads.overflowed, "{reads:?}");
        }
    }

    /// l4d-ig6b M1: a replay that would pass the session's directory entry
    /// limit searches instead, as a fresh search would have stopped there.
    #[test]
    fn a_replay_past_the_entry_limit_searches() {
        let temp = tempfile::tempdir().unwrap();
        let (workspace, units) = missing_include_workspace(temp.path(), 2);
        pascal_project::backdate_tree_for_test(temp.path());
        let (uri, source, key) = &units[0];
        expand_unit(&workspace, uri, source, key);
        assert_eq!(workspace.project_cache.include_failure_count(), 1);
        let charges = workspace.project_cache.include_failure_charges_for_tests();
        assert!(
            charges
                .iter()
                .all(|(scanned, work, _)| *scanned > 0 && *work > 0),
            "the search's entries and work are recorded: {charges:?}"
        );
        workspace
            .project_cache
            .inflate_include_failures_for_tests(usize::MAX / 2);
        pascal_core::take_include_searches();
        let (uri, source, key) = &units[1];
        expand_unit(&workspace, uri, source, key);
        assert_eq!(pascal_core::take_include_searches(), 1);
    }

    fn work(result: &ExpansionResult) -> usize {
        let session = result.expanded.session.as_ref().unwrap();
        session.budget.budget_snapshot().work
    }

    /// l4d-4nbs D29: a later analysis reuses the provider interface an
    /// earlier one compiled, from the project cache, and reports the same
    /// dependencies; the cache charges it to `maxCacheBytes`.
    #[test]
    fn a_later_expansion_reuses_compiled_provider_interfaces() {
        let temp = tempfile::tempdir().unwrap();
        let (workspace, uri, source, key) = provider_workspace(temp.path());
        let first = expand_main(&workspace, &uri, &source, &key);
        assert_eq!(workspace.project_cache.provider_graph_count(), 1);
        let charged = workspace.project_cache.provider_graph_bytes();
        assert!(charged > 0, "the shared interface is charged");
        let second = expand_main(&workspace, &uri, &source, &key);
        assert_eq!(
            first.expanded.session.as_ref().unwrap().observations,
            second.expanded.session.as_ref().unwrap().observations
        );
        assert!(
            work(&second) * 2 < work(&first),
            "{} then {}",
            work(&first),
            work(&second)
        );
        assert_eq!(
            first.expanded.reads, second.expanded.reads,
            "the reuse replays what the compilation read"
        );
    }

    /// l4d-10: a provider interface is charged within a fifth of the heap
    /// it keeps, counted by the test allocator when the store drops it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_provider_interface_is_charged_within_a_fifth_of_its_heap() {
        let temp = tempfile::tempdir().unwrap();
        let (workspace, uri, source, key) = provider_workspace(temp.path());
        drop(expand_main(&workspace, &uri, &source, &key));
        assert_graphs_charged_within_a_fifth(&workspace);
    }

    /// l4d-10: an interface with a branch the session cannot decide keeps
    /// the identifiers its text spells (D46). RAD Studio's `SysUtils` is
    /// one: its graph was charged 30 KB for the 122 KB it kept.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_provider_interface_with_an_unknown_branch_is_charged_within_a_fifth_of_its_heap() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = "unit Main; interface uses A; {$IF Declared(Flag)} const Eight = 8; {$IFEND} implementation end.";
        std::fs::write(root.join("Main.pas"), source).unwrap();
        let declarations = (0..2_000)
            .map(|n| format!("C{n} = Base + Offset + Scale * Count;\n"))
            .collect::<String>();
        std::fs::write(
            root.join("A.pas"),
            format!("unit A; interface {{$IFDEF MSWINDOWS}} {{$ENDIF}} const\n{declarations}Flag = 8; implementation end."),
        )
        .unwrap();
        pascal_project::backdate_tree_for_test(root);
        let mut workspace = Workspace::with_override_session(
            vec![root.into()],
            Default::default(),
            OverrideSession::new(None),
        );
        let uri = Url::from_file_path(root.join("Main.pas")).unwrap();
        let key = workspace.context_for_uri_with_cancel(&uri, None).unwrap();
        drop(expand_main(&workspace, &uri, source, &key));
        assert_graphs_charged_within_a_fifth(&workspace);
    }

    /// l4d-10 smoke probe, not a regression test (fix round 1, M7): it
    /// passed before and after the interface fixes, since `max_work` stops a
    /// single session before the large interfaces production builds over
    /// many member sessions; the evidence for those is the in-process
    /// SFClient2 measurement (report m14/m17). It checks that real providers'
    /// shared interfaces, compiled under the production limits, are charged
    /// within a fifth of the heap they keep. Needs RAD Studio 7.0's sources
    /// (read only), and multidev's business objects when `MULTIDEV` is set:
    /// `PASCAL_RTL=~/.local/share/Headless-Delphi/files/RAD\ Studio/7.0/source/Win32
    ///  MULTIDEV=~/gits/multidev cargo test -p pascal-lsp --lib real_providers_smoke -- --ignored`
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs RAD Studio 7.0 sources"]
    fn real_providers_smoke_probe() {
        let rtl = std::path::PathBuf::from(std::env::var("PASCAL_RTL").expect("PASCAL_RTL"));
        let mut paths = ["rtl/sys", "rtl/common", "rtl/win", "vcl", "db"]
            .iter()
            .map(|dir| rtl.join(dir))
            .collect::<Vec<_>>();
        let mut uses = vec!["SysConst", "Variants", "Types"];
        if let Some(multidev) = std::env::var_os("MULTIDEV").map(std::path::PathBuf::from) {
            for dir in ["Common", "Common/BO", "Common/BOBase", "Common/BORules"] {
                paths.push(multidev.join(dir));
            }
            uses.extend([
                "BusinessObject",
                "BOFields",
                "BOConnection",
                "BaseREPAIRHEADER",
            ]);
        }
        let temp = tempfile::tempdir().unwrap();
        let source = format!(
            "unit Main; interface uses {}; {{$IF Declared(TObject)}} const Eight = 8; {{$IFEND}} implementation end.",
            uses.join(", ")
        );
        std::fs::write(temp.path().join("Main.pas"), &source).unwrap();
        let options = crate::workspace::WorkspaceOptions {
            source_paths: paths
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            ..crate::workspace::WorkspaceOptions::default()
        };
        let mut workspace = Workspace::with_override_session(
            vec![temp.path().into()],
            options,
            OverrideSession::new(None),
        );
        let uri = Url::from_file_path(temp.path().join("Main.pas")).unwrap();
        let key = workspace.context_for_uri_with_cancel(&uri, None).unwrap();
        // A session past max_work still shares what it compiled before.
        drop(expand(
            &workspace,
            &uri,
            &source,
            &key,
            workspace.include_expansion_limits(),
            &AtomicBool::new(false),
            None,
        ));
        assert!(workspace.project_cache.provider_graph_count() > 0);
        let charged = workspace.project_cache.provider_graph_bytes();
        let (freed, holders) = workspace.project_cache.drop_provider_graphs_for_test();
        assert!(holders.iter().all(|holders| *holders == 0), "{holders:?}");
        let error = charged.abs_diff(freed as usize) as f64 / freed as f64;
        assert!(
            error <= 0.2,
            "charged {charged} bytes for interfaces that kept {freed} ({:.0}% off)",
            error * 100.0
        );
    }

    /// Main uses A, whose interface reads B's: both shared.
    #[cfg(target_os = "linux")]
    fn nested_provider_workspace(root: &std::path::Path) -> Workspace {
        let source = "unit Main; interface uses A; {$IF A.Flag = 8} const Eight = 8; {$IFEND} implementation end.";
        std::fs::write(root.join("Main.pas"), source).unwrap();
        let declarations = (0..2_000)
            .map(|n| format!("C{n} = {n};\n"))
            .collect::<String>();
        std::fs::write(
            root.join("B.pas"),
            format!("unit B; interface const\n{declarations}Flag = 8; implementation end."),
        )
        .unwrap();
        std::fs::write(
            root.join("A.pas"),
            "unit A; interface uses B; const {$IF B.Flag = 8} Flag = 8; {$ELSE} Flag = 0; {$IFEND} implementation end.",
        )
        .unwrap();
        pascal_project::backdate_tree_for_test(root);
        let mut workspace = Workspace::with_override_session(
            vec![root.into()],
            Default::default(),
            OverrideSession::new(None),
        );
        let uri = Url::from_file_path(root.join("Main.pas")).unwrap();
        let key = workspace.context_for_uri_with_cancel(&uri, None).unwrap();
        drop(expand_main(&workspace, &uri, source, &key));
        assert_eq!(workspace.project_cache.provider_graph_count(), 2);
        workspace
    }

    /// l4d-10: an interface keeps the interfaces of the providers its own
    /// compilation read. Once the store evicts one, the importer alone keeps
    /// it, and it stays charged until that is dropped: SFClient2's
    /// ZDbcGenericResolver was charged 2.7 MB and kept 168 MB, VCL's Clipbrd
    /// 0.2 MB and 18 MB.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_interface_is_charged_for_the_provider_interfaces_it_keeps() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = nested_provider_workspace(temp.path());
        // The store gives B's interface up; A's still holds it.
        workspace
            .project_cache
            .evict_provider_graphs_for_test("B.pas");
        assert_graphs_charged_within_a_fifth(&workspace);
    }

    /// l4d-10: a nested interface the store holds is charged once, by its
    /// own entry; charging it to every importer too made the share churn
    /// (SFClient2's index extraction ran ten times slower).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_nested_interface_the_store_holds_is_charged_once() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = nested_provider_workspace(temp.path());
        assert_graphs_charged_within_a_fifth(&workspace);
    }

    /// `Main{n}` uses `A{n}`, whose interface reads `B{n}`'s: `units`
    /// such pairs, each B about 1.6 MB of interface. Returns the workspace
    /// and each Main's URI, source and context.
    #[cfg(target_os = "linux")]
    fn importer_pairs(
        root: &std::path::Path,
        units: usize,
    ) -> (Workspace, Vec<(Url, String, ContextKey)>) {
        let declarations = (0..2_000)
            .map(|n| format!("C{n} = {n};\n"))
            .collect::<String>();
        for n in 0..units {
            std::fs::write(
                root.join(format!("B{n}.pas")),
                format!("unit B{n}; interface const\n{declarations}Flag = 8; implementation end."),
            )
            .unwrap();
            std::fs::write(
                root.join(format!("A{n}.pas")),
                format!("unit A{n}; interface uses B{n}; const {{$IF B{n}.Flag = 8}} Flag = 8; {{$ELSE}} Flag = 0; {{$IFEND}} implementation end."),
            )
            .unwrap();
            std::fs::write(
                root.join(format!("Main{n}.pas")),
                format!("unit Main{n}; interface uses A{n}; {{$IF A{n}.Flag = 8}} const Eight = 8; {{$IFEND}} implementation end."),
            )
            .unwrap();
        }
        pascal_project::backdate_tree_for_test(root);
        let mut workspace = Workspace::with_override_session(
            vec![root.into()],
            Default::default(),
            OverrideSession::new(None),
        );
        let mains = (0..units)
            .map(|n| {
                let path = root.join(format!("Main{n}.pas"));
                let source = std::fs::read_to_string(&path).unwrap();
                let uri = Url::from_file_path(path).unwrap();
                let key = workspace.context_for_uri_with_cancel(&uri, None).unwrap();
                (uri, source, key)
            })
            .collect();
        (workspace, mains)
    }

    /// l4d-10 fix round 1 (M9): a kept interface stops being charged once
    /// its last importer is gone.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_kept_interface_is_released_when_its_last_importer_drops() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = nested_provider_workspace(temp.path());
        workspace
            .project_cache
            .evict_provider_graphs_for_test("B.pas");
        assert!(workspace.project_cache.kept_provider_graph_bytes_for_test() > 1_000_000);
        workspace
            .project_cache
            .evict_provider_graphs_for_test("A.pas");
        assert_eq!(
            workspace.project_cache.kept_provider_graph_bytes_for_test(),
            0
        );
        assert_eq!(workspace.project_cache.provider_graph_bytes(), 0);
    }

    /// l4d-10 fix round 1 (I1, l4d-4nbs I2): provider interfaces, kept
    /// states included, give way to every request: a reservation evicts the
    /// importer that keeps a state.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_request_evicts_the_importer_that_keeps_an_interface() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = nested_provider_workspace(temp.path());
        workspace
            .project_cache
            .evict_provider_graphs_for_test("B.pas");
        let kept = workspace.project_cache.kept_provider_graph_bytes_for_test();
        assert!(kept > 1_000_000, "{kept}");
        let cache = &workspace.project_cache;
        cache.set_max_bytes(2 * kept);
        let wanted = 3 * kept / 2;
        let reservation = cache.reserve(crate::project_cache::ReservationKind::Scratch, wanted);
        assert!(
            reservation.is_some(),
            "a {wanted}-byte reservation beside {kept} kept bytes an importer holds"
        );
        assert_eq!(cache.provider_graph_count(), 0, "A gave way");
        assert_eq!(cache.kept_provider_graph_bytes_for_test(), 0);
    }

    /// l4d-10 fix round 1 (I2): interfaces and the states only importers
    /// keep stay within the provider share, however many are compiled.
    #[cfg(target_os = "linux")]
    #[test]
    fn interfaces_and_kept_states_stay_within_the_provider_share() {
        let temp = tempfile::tempdir().unwrap();
        let (workspace, mains) = importer_pairs(temp.path(), 8);
        let cache = &workspace.project_cache;
        // A share of about three pairs.
        let share = 5 << 20;
        cache.set_max_bytes(4 * share);
        for (uri, source, key) in &mains {
            drop(expand_main(&workspace, uri, source, key));
            let charged = cache.provider_graph_bytes();
            assert!(
                charged <= share,
                "{charged} bytes against a {share}-byte share"
            );
        }
        assert!(cache.provider_graph_count() > 0);
    }

    /// l4d-10 fix round 1 (I2): a member that needs an interface the store
    /// evicted while an importer still keeps it takes that one back instead
    /// of compiling a second copy.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_member_needing_a_kept_interface_adopts_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        // Written first: a later file would change the listing B's
        // interface proved, and B would be compiled again for that.
        let source = "unit Other; interface uses B; {$IF B.Flag = 8} const Eight = 8; {$IFEND} implementation end.";
        std::fs::write(root.join("Other.pas"), source).unwrap();
        let mut workspace = nested_provider_workspace(root);
        workspace
            .project_cache
            .evict_provider_graphs_for_test("B.pas");
        assert!(workspace.project_cache.kept_provider_graph_bytes_for_test() > 0);
        let puts = workspace.project_cache.provider_graph_puts_for_test();
        let uri = Url::from_file_path(root.join("Other.pas")).unwrap();
        let key = workspace.context_for_uri_with_cancel(&uri, None).unwrap();
        drop(expand_main(&workspace, &uri, source, &key));
        assert_eq!(
            workspace.project_cache.provider_graph_puts_for_test(),
            puts,
            "B was not compiled again"
        );
        assert_eq!(workspace.project_cache.provider_graph_count(), 2);
        assert_eq!(
            workspace.project_cache.kept_provider_graph_bytes_for_test(),
            0
        );
    }

    /// l4d-10 fix round 1 (I3): the store sizes each state once, at its put,
    /// outside its lock: evicting walks no state, and an importer of a
    /// stored interface walks only its own.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_store_sizes_each_interface_state_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = "unit Other; interface uses A2; {$IF A2.Flag = 8} const Eight = 8; {$IFEND} implementation end.";
        std::fs::write(
            root.join("A2.pas"),
            "unit A2; interface uses B; const {$IF B.Flag = 8} Flag = 8; {$ELSE} Flag = 0; {$IFEND} implementation end.",
        )
        .unwrap();
        std::fs::write(root.join("Other.pas"), source).unwrap();
        let mut workspace = nested_provider_workspace(root);
        let walks = pascal_core::compile_time::interface_state_walks;
        let before = walks();
        workspace
            .project_cache
            .evict_provider_graphs_for_test("B.pas");
        workspace
            .project_cache
            .evict_provider_graphs_for_test("A.pas");
        assert_eq!(walks(), before, "evictions walk no state");
        // Another importer of B, with B stored again: only its own state.
        nested_provider_workspace_again(&mut workspace, root);
        let uri = Url::from_file_path(root.join("Other.pas")).unwrap();
        let key = workspace.context_for_uri_with_cancel(&uri, None).unwrap();
        let before = walks();
        drop(expand_main(&workspace, &uri, source, &key));
        assert_eq!(
            walks() - before,
            0,
            "B's state is charged already: not walked again"
        );
    }

    #[cfg(target_os = "linux")]
    fn nested_provider_workspace_again(workspace: &mut Workspace, root: &std::path::Path) {
        let source = "unit Main; interface uses A; {$IF A.Flag = 8} const Eight = 8; {$IFEND} implementation end.";
        let uri = Url::from_file_path(root.join("Main.pas")).unwrap();
        let key = workspace.context_for_uri_with_cancel(&uri, None).unwrap();
        drop(expand_main(workspace, &uri, source, &key));
        assert_eq!(workspace.project_cache.provider_graph_count(), 2);
    }

    #[cfg(target_os = "linux")]
    fn assert_graphs_charged_within_a_fifth(workspace: &Workspace) {
        assert!(workspace.project_cache.provider_graph_count() > 0);
        let charged = workspace.project_cache.provider_graph_bytes();
        let (freed, holders) = workspace.project_cache.drop_provider_graphs_for_test();
        assert!(
            holders.iter().all(|holders| *holders == 0),
            "only the store holds the interfaces: {holders:?}"
        );
        let error = charged.abs_diff(freed as usize) as f64 / freed as f64;
        assert!(
            error <= 0.2,
            "charged {charged} bytes for interfaces that kept {freed} ({:.0}% off)",
            error * 100.0
        );
    }

    /// Rootcause A1: a provider whose interface looks up many units over a
    /// search path is replayed with one listing per directory, so a session
    /// that reuses it captures every directory once.
    #[test]
    fn a_reused_provider_replays_one_listing_per_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(
            root.join("App.dproj"),
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>s1;s2;s3</DCC_UnitSearchPath></PropertyGroup></Project>",
        )
        .unwrap();
        for directory in ["s1", "s2", "s3"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        let units: Vec<String> = (0..12).map(|n| format!("B{n}")).collect();
        for unit in &units {
            std::fs::write(
                root.join("s3").join(format!("{unit}.pas")),
                format!("unit {unit}; interface const {unit}Value = 1; implementation end."),
            )
            .unwrap();
        }
        std::fs::write(
            root.join("s3/A.pas"),
            format!(
                "unit A; interface uses {}; const Flag = 8; implementation end.",
                units.join(", ")
            ),
        )
        .unwrap();
        let source = "unit Main; interface uses A; {$IF A.Flag = 8} const Eight = 8; {$ENDIF} implementation end.";
        std::fs::write(root.join("Main.pas"), source).unwrap();
        let mut workspace = Workspace::with_override_session(
            vec![root.into()],
            Default::default(),
            OverrideSession::new(None),
        );
        let uri = Url::from_file_path(root.join("Main.pas")).unwrap();
        let key = workspace.context_for_uri_with_cancel(&uri, None).unwrap();
        let first = expand_main(&workspace, &uri, source, &key);
        assert_eq!(workspace.project_cache.provider_graph_count(), 1);
        let second = expand_main(&workspace, &uri, source, &key);
        for result in [&first, &second] {
            let reads = result.expanded.reads.as_ref().unwrap();
            assert!(!reads.overflowed);
            for (n, listing) in reads.listings.iter().enumerate() {
                assert!(
                    !reads.listings[..n].contains(listing),
                    "{listing:?} twice in {:?}",
                    reads.listings
                );
            }
            for (n, payload) in reads.payloads.iter().enumerate() {
                assert!(!reads.payloads[..n].contains(payload), "{payload:?} twice");
            }
        }
        let directories = |result: &ExpansionResult| {
            let mut listed: Vec<_> = result
                .expanded
                .reads
                .as_ref()
                .unwrap()
                .listings
                .iter()
                .map(|listing| listing.directory.clone())
                .collect();
            listed.sort();
            listed
        };
        assert_eq!(directories(&first), directories(&second));
        assert!(
            directories(&first)
                .iter()
                .any(|directory| directory.ends_with("s1")),
            "{:?}",
            directories(&first)
        );
    }

    /// l4d-4nbs D44: a cached expansion keeps a compact projection of its
    /// session, not the declaration state and provider graphs it compiled, so
    /// what it retains is a small multiple of the source it covers.
    #[test]
    fn a_cached_expansion_retains_a_small_multiple_of_its_source() {
        let temp = tempfile::tempdir().unwrap();
        let (workspace, uri, source, key) = provider_workspace(temp.path());
        let result = expand_main(&workspace, &uri, &source, &key);
        let session = result.expanded.session.as_ref().unwrap();
        let mut declarations = 0usize;
        session
            .facts
            .visit_recovery_payload(&mut |bytes| {
                declarations += bytes;
                Ok(())
            })
            .unwrap();
        assert!(
            declarations < 8 * 1024,
            "the session keeps {declarations} bytes of declarations and provider graphs"
        );
        let retained = crate::project_cache::expansion_bytes(&result);
        let provider = std::fs::metadata(temp.path().join("A.pas")).unwrap().len() as usize;
        assert!(
            retained < 16 * source.len() + 16 * 1024,
            "{retained} bytes for a {} byte root (its provider is {provider} bytes)",
            source.len()
        );
    }

    /// A changed provider file is not reused, whether or not a watcher saw
    /// the change; a watched change drops the entry and its charge.
    #[test]
    fn a_changed_provider_is_compiled_again() {
        let temp = tempfile::tempdir().unwrap();
        let (workspace, uri, source, key) = provider_workspace(temp.path());
        let first = expand_main(&workspace, &uri, &source, &key);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let path = temp.path().join("A.pas");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace("Flag = 8", "Flag = 4")).unwrap();
        let second = expand_main(&workspace, &uri, &source, &key);
        assert!(
            work(&second) * 2 > work(&first),
            "{} then {}",
            work(&first),
            work(&second)
        );
        let inactive = |result: &ExpansionResult| {
            let conditionals = result.expanded.conditionals.as_ref().unwrap();
            conditionals
                .inactive_spans
                .iter()
                .map(|span| source[span.clone()].to_string())
                .collect::<Vec<_>>()
                .concat()
        };
        assert!(
            inactive(&first).contains("Other"),
            "{:?}",
            first.expanded.conditionals
        );
        assert!(
            inactive(&second).contains("Eight"),
            "A.Flag is 4 now: {}",
            inactive(&second)
        );
        assert_eq!(
            workspace.project_cache.provider_graph_count(),
            2,
            "both revisions are kept"
        );
        workspace
            .project_cache
            .invalidate_file_contents(&temp.path().join("A.pas"));
        assert_eq!(workspace.project_cache.provider_graph_count(), 0);
        assert_eq!(workspace.project_cache.provider_graph_bytes(), 0);
    }

    /// l4d-4nbs review C1 (the reviewer's probe): P in c/ uses W in b/;
    /// creating a/W.pas, earlier in the search path, shadows it. The stored
    /// P interface carries the directories its lookup of W consulted, so the
    /// same workspace computes P.Flag from the new W, as a fresh one does.
    #[test]
    fn a_unit_shadowing_a_nested_lookup_invalidates_the_shared_provider() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        let c = temp.path().join("c");
        std::fs::create_dir_all(&c).unwrap();
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let source = "unit Main; interface uses W, P; {$IF W.Flag = 1} const WOne = 1; {$ENDIF} {$IF P.Flag = 8} const Eight = 8; {$ELSE} const Other = 0; {$ENDIF} implementation end.";
        std::fs::write(a.join("Main.pas"), source).unwrap();
        std::fs::write(
            b.join("W.pas"),
            "unit W; interface const Flag = 1; implementation end.",
        )
        .unwrap();
        std::fs::write(
            c.join("P.pas"),
            "unit P; interface uses W; const Flag = W.Flag * 8; implementation end.",
        )
        .unwrap();
        let make = || {
            let mut ws = Workspace::with_override_session(
                vec![a.clone(), b.clone(), c.clone()],
                Default::default(),
                OverrideSession::new(None),
            );
            let uri = Url::from_file_path(a.join("Main.pas")).unwrap();
            let key = ws.context_for_uri_with_cancel(&uri, None).unwrap();
            let _ = &mut ws;
            (ws, uri, key)
        };
        let inactive = |result: &ExpansionResult| {
            let c = result.expanded.conditionals.as_ref().unwrap();
            c.inactive_spans
                .iter()
                .map(|s| source[s.clone()].to_string())
                .collect::<Vec<_>>()
                .concat()
        };
        let (ws, uri, key) = make();
        let first = expand(
            &ws,
            &uri,
            source,
            &key,
            ws.include_expansion_limits(),
            &AtomicBool::new(false),
            None,
        )
        .unwrap();
        assert!(inactive(&first).contains("Other"), "{}", inactive(&first));
        assert_eq!(ws.project_cache.provider_graph_count(), 2);
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(
            a.join("W.pas"),
            "unit W; interface const Flag = 2; implementation end.",
        )
        .unwrap();
        let second = expand(
            &ws,
            &uri,
            source,
            &key,
            ws.include_expansion_limits(),
            &AtomicBool::new(false),
            None,
        )
        .unwrap();
        let (fresh_ws, fresh_uri, fresh_key) = make();
        let fresh = expand(
            &fresh_ws,
            &fresh_uri,
            source,
            &fresh_key,
            fresh_ws.include_expansion_limits(),
            &AtomicBool::new(false),
            None,
        )
        .unwrap();
        assert_eq!(
            inactive(&second),
            inactive(&fresh),
            "reused workspace disagrees with a fresh one"
        );
        // A watched creation in a directory P's lookup consulted drops P,
        // not W, which no lookup of its own depends on.
        assert_eq!(fresh_ws.project_cache.provider_graph_count(), 2);
        fresh_ws.project_cache.invalidate_path(&a.join("X.pas"));
        assert_eq!(fresh_ws.project_cache.provider_graph_count(), 1);
    }

    /// l4d-4nbs review I4: nested providers up to the session's stack limit,
    /// each resolving a 250-level type alias chain, run on an analysis thread
    /// without overflowing it.
    fn deep_nesting_on(builder: std::thread::Builder, levels: usize) -> bool {
        builder
            .spawn(move || {
                let temp = tempfile::tempdir().unwrap();
                let root = temp.path();
                for n in 0..levels {
                    let mut text = format!("unit U{n};\ninterface\n");
                    let leaf = n + 1 == levels;
                    let base = if leaf {
                        "Int64".to_string()
                    } else {
                        text.push_str(&format!("uses U{};\n", n + 1));
                        format!("U{}.T{}", n + 1, n + 1)
                    };
                    // The innermost provider prepares a chain at max_type_depth.
                    let chain = if leaf { 250 } else { 1 };
                    text.push_str(&format!("type\n  T{n}_0 = {base};\n"));
                    for k in 1..chain {
                        text.push_str(&format!("  T{n}_{k} = T{n}_{};\n", k - 1));
                    }
                    text.push_str(&format!("  T{n} = T{n}_{};\nimplementation\nend.\n", chain - 1));
                    std::fs::write(root.join(format!("U{n}.pas")), text).unwrap();
                }
                let source = "unit Main; interface uses U0; {$IF SizeOf(U0.T0)=8}{$DEFINE DEEP}{$ENDIF} implementation end.";
                std::fs::write(root.join("Main.pas"), source).unwrap();
                let mut workspace = Workspace::with_override_session(
                    vec![root.into()],
                    Default::default(),
                    OverrideSession::new(None),
                );
                let uri = Url::from_file_path(root.join("Main.pas")).unwrap();
                let key = workspace.context_for_uri_with_cancel(&uri, None).unwrap();
                let cancel = AtomicBool::new(false);
                let _ = expand(
                    &workspace,
                    &uri,
                    source,
                    &key,
                    workspace.include_expansion_limits(),
                    &cancel,
                    None,
                );
            })
            .unwrap()
            .join()
            .is_ok()
    }

    #[test]
    fn deep_provider_and_type_nesting_fit_an_analysis_thread() {
        // A debug build stops nesting at about 17 such providers; release
        // goes past 40. Measured peaks: 1.24 MiB debug, 0.71 MiB release.
        for levels in [16, 40] {
            assert!(deep_nesting_on(
                crate::server::analysis_thread("PascalLspTestAnalysis"),
                levels
            ));
        }
    }

    /// An interface larger than its share of `maxCacheBytes` is not kept.
    #[test]
    fn a_provider_interface_past_its_share_is_not_kept() {
        let temp = tempfile::tempdir().unwrap();
        let (workspace, uri, source, key) = provider_workspace(temp.path());
        expand_main(&workspace, &uri, &source, &key);
        let charged = workspace.project_cache.provider_graph_bytes();
        workspace.project_cache.set_max_bytes(charged * 2);
        workspace
            .project_cache
            .invalidate_file_contents(&temp.path().join("A.pas"));
        expand_main(&workspace, &uri, &source, &key);
        assert_eq!(workspace.project_cache.provider_graph_count(), 0);
        assert_eq!(workspace.project_cache.provider_graph_bytes(), 0);
    }

    #[test]
    fn compatibility_retention_uses_finite_existing_size_domains() {
        let resources = super::super::ResourceLimits {
            max_files: 8,
            max_file_bytes: 128,
            max_total_bytes: 512,
            max_cache_bytes: 256,
            max_workspace_units: super::super::DEFAULT_MAX_WORKSPACE_UNITS,
        };
        let expansion = ExpansionLimits {
            max_expanded_bytes: 384,
            ..ExpansionLimits::default()
        };
        assert_eq!(
            lsp_compatibility_retention(resources, expansion),
            128 + 512 + 384 + 256
        );
        assert!(
            lsp_compatibility_retention(
                super::super::ResourceLimits::default(),
                ExpansionLimits::default()
            ) > 1_048_576
        );
        let unbounded = super::super::ResourceLimits {
            max_file_bytes: usize::MAX,
            max_total_bytes: usize::MAX,
            max_cache_bytes: usize::MAX,
            ..resources
        };
        assert!(lsp_compatibility_retention(unbounded, expansion) < usize::MAX);
    }

    #[test]
    fn compatibility_retention_preserves_small_file_total_and_expanded_refusals() {
        let dir = tempfile::tempdir().unwrap();
        let source = "unit Main; interface {$I Body.inc} implementation end.";
        std::fs::write(dir.path().join("Main.pas"), source).unwrap();
        std::fs::write(
            dir.path().join("Body.inc"),
            format!("const Value=1; {{{}}}", "x".repeat(512)),
        )
        .unwrap();
        for (file, total, expanded) in [(128, 2048, 2048), (2048, 128, 2048), (2048, 2048, 128)] {
            let options = super::super::WorkspaceOptions {
                limits: super::super::ResourceLimits {
                    max_file_bytes: file,
                    max_total_bytes: total,
                    max_cache_bytes: 2048,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut workspace = Workspace::with_override_session(
                vec![dir.path().into()],
                options,
                OverrideSession::new(None),
            );
            let uri = Url::from_file_path(dir.path().join("Main.pas")).unwrap();
            let key = workspace.context_for_uri_with_cancel(&uri, None).unwrap();
            let limits = ExpansionLimits {
                max_expanded_bytes: expanded.min(total),
                ..workspace.include_expansion_limits()
            };
            let cancel = AtomicBool::new(false);
            let result = expand(&workspace, &uri, source, &key, limits, &cancel, None);
            assert!(
                !matches!(result, Ok(result) if result.complete),
                "accepted file={file} total={total} expanded={expanded}"
            );
        }
    }

    #[test]
    fn verified_prefix_session_and_cancelled_navigation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = "{$I flags.inc}\n{$IF Defined(READY)}\nunit Known;\n{$ELSE}\nunit Other;\n{$ENDIF}\ninterface {$IF UnsupportedFunction(1)} var Value: Integer; {$ENDIF} implementation end.";
        std::fs::write(root.join("Known.pas"), source).unwrap();
        std::fs::write(root.join("flags.inc"), "{$DEFINE READY}").unwrap();
        std::fs::write(
            root.join("Main.pas"),
            "unit Main; interface uses Known; implementation end.",
        )
        .unwrap();
        let mut workspace = Workspace::with_override_session(
            vec![root.into()],
            Default::default(),
            OverrideSession::new(None),
        );
        let uri = Url::from_file_path(root.join("Known.pas")).unwrap();
        let key = workspace.context_for_uri_with_cancel(&uri, None).unwrap();
        let cancel = AtomicBool::new(false);
        let result = expand(
            &workspace,
            &uri,
            source,
            &key,
            workspace.include_expansion_limits(),
            &cancel,
            None,
        )
        .unwrap();
        let session = result.expanded.session.as_ref().unwrap();
        assert!(
            matches!(&session.header, pascal_core::compile_time::Fact::Known(h) if h.name == "Known"),
            "{:?} {:?}",
            session.header,
            session.root
        );
        let main = Url::from_file_path(root.join("Main.pas")).unwrap();
        assert_eq!(
            workspace
                .navigate(&main, Position::new(0, 26), NavigationTarget::Definition)
                .len(),
            1,
            "{:?} {:?}",
            session.header,
            result.expanded.conditionals
        );
        cancel.store(true, Ordering::Relaxed);
        super::super::resolver::reset_test_source_loads();
        assert!(
            workspace
                .navigate_with_cancel(
                    &main,
                    Position::new(0, 26),
                    NavigationTarget::Definition,
                    &cancel
                )
                .unwrap_err()
                .contains("cancelled")
        );
        assert_eq!(
            super::super::resolver::test_source_load_count(&root.join("Known.pas")),
            0
        );
    }

    #[test]
    fn include_spelling_session_remains_complete() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = "unit Main;\ninterface\nimplementation\n{$I SELECTED.INC}\nend.\n";
        std::fs::write(root.join("Main.pas"), source).unwrap();
        std::fs::write(root.join("Selected.inc"), "const Selected = 1;\n").unwrap();
        let mut workspace = Workspace::with_override_session(
            vec![root.into()],
            Default::default(),
            OverrideSession::new(None),
        );
        let uri = Url::from_file_path(root.join("Main.pas")).unwrap();
        let key = workspace.context_for_uri_with_cancel(&uri, None).unwrap();
        let cancel = AtomicBool::new(false);
        let result = expand(
            &workspace,
            &uri,
            source,
            &key,
            workspace.include_expansion_limits(),
            &cancel,
            None,
        )
        .unwrap();
        let session = result.expanded.session.as_ref().unwrap();
        assert!(
            session.root.complete,
            "{:?}; {:?}; {:?}",
            session.reasons, session.root, session.warnings
        );
    }

    #[test]
    fn resource_budget_stops_header_callbacks_before_payload_reads() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = "unit Main; interface {$IF SizeOf(Extended)>=10} {$ENDIF} implementation end.";
        std::fs::write(root.join("Main.pas"), source).unwrap();
        let mut workspace = Workspace::with_override_session(
            vec![root.into()],
            Default::default(),
            OverrideSession::new(None),
        );
        let uri = Url::from_file_path(root.join("Main.pas")).unwrap();
        let key = workspace.context_for_uri_with_cancel(&uri, None).unwrap();
        let cancel = AtomicBool::new(false);
        let budget = ReconciliationBudget::new(Arc::new(AtomicBool::new(false)));
        budget.path_visit_limit.set(Some(0));
        super::super::resolver::reset_test_source_loads();
        let error = expand(
            &workspace,
            &uri,
            source,
            &key,
            workspace.include_expansion_limits(),
            &cancel,
            Some(&budget),
        )
        .unwrap_err();
        assert!(error.contains("budget"), "{error}");
        assert_eq!(
            super::super::resolver::test_source_load_count(&root.join("Main.pas")),
            0
        );
    }

    #[test]
    fn cross_physical_header_references_remain_incomplete() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(
            root.join("Main.pas"),
            "unit Main; interface uses Prefix.Known; implementation end.",
        )
        .unwrap();
        std::fs::write(root.join("Prefix.Known.pas"), "unit Prefix.{$I suffix.inc}; interface {$IF UnsupportedFunction(1)} var Value: Integer; {$ENDIF} implementation end.").unwrap();
        std::fs::write(root.join("suffix.inc"), "Known").unwrap();
        let workspace = Workspace::with_override_session(
            vec![root.into()],
            Default::default(),
            OverrideSession::new(None),
        );
        let main = Url::from_file_path(root.join("Main.pas")).unwrap();
        let references = super::super::queries::references_from_input(
            workspace.analysis_input(),
            &main,
            Position::new(0, 33),
            true,
            &AtomicBool::new(false),
        );
        // References are a read-only partial-answer API. Keep its established
        // incomplete coverage (rather than inventing an edit/completeness
        // proof or turning this into a different public error contract).
        let partial = references.value.unwrap();
        assert!(!partial.coverage.is_complete());
        assert!(
            partial
                .coverage
                .gaps
                .iter()
                .any(|gap| gap.reason.contains("include expansion is incomplete"))
        );
        assert!(partial.value.iter().all(|location| location.uri == main));
    }

    /// 13b-5 (A4): a passed member deadline stops the session as
    /// cancellation does, under its own message; cancellation comes first,
    /// so a cancelled member is never taken for one past its deadline.
    #[test]
    fn a_passed_member_deadline_stops_the_session_after_cancellation() {
        let cancel = AtomicBool::new(false);
        let passed = Control {
            cancel: &cancel,
            budget: None,
            deadline: Some(std::time::Instant::now()),
        };
        assert_eq!(
            passed.check().unwrap_err(),
            super::super::indexing::MEMBER_DEADLINE_MESSAGE
        );
        assert!(passed.is_cancelled());
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            passed.check().unwrap_err(),
            super::super::CANCELLATION_MESSAGE
        );
        let running = AtomicBool::new(false);
        let ahead = Control {
            cancel: &running,
            budget: None,
            deadline: Some(std::time::Instant::now() + std::time::Duration::from_secs(3_600)),
        };
        assert!(ahead.check().is_ok());
        assert!(!ahead.is_cancelled());
    }

    /// 13b-5 (A4): an expansion in a view with a passed member deadline
    /// fails with the deadline's message; without a deadline it succeeds.
    #[test]
    fn an_expansion_past_the_member_deadline_fails_with_its_message() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("x.inc"), "const X = 1;\n").unwrap();
        let source = "unit Main; interface {$I x.inc} implementation end.";
        std::fs::write(root.join("Main.pas"), source).unwrap();
        let mut workspace = Workspace::with_override_session(
            vec![root.into()],
            Default::default(),
            OverrideSession::new(None),
        );
        let uri = Url::from_file_path(root.join("Main.pas")).unwrap();
        let key = workspace.context_for_uri_with_cancel(&uri, None).unwrap();
        let limits = workspace.include_expansion_limits();
        let cancel = AtomicBool::new(false);
        workspace.member_deadline = Some(std::time::Instant::now());
        let error = expand(&workspace, &uri, source, &key, limits, &cancel, None).unwrap_err();
        assert_eq!(error, super::super::indexing::MEMBER_DEADLINE_MESSAGE);
        workspace.member_deadline = None;
        assert!(expand(&workspace, &uri, source, &key, limits, &cancel, None).is_ok());
    }
}
