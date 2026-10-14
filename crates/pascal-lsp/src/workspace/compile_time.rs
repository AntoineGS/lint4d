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

struct Control<'a> {
    cancel: &'a AtomicBool,
    budget: Option<&'a ReconciliationBudget>,
}

#[cfg(test)]
thread_local! { static TEST_BODY_ANALYSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
thread_local! { static TEST_HEADER_REVALIDATION_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
pub(super) fn test_header_revalidation_hook(hook: Option<Box<dyn FnMut()>>) {
    TEST_HEADER_REVALIDATION_HOOK.with(|value| *value.borrow_mut() = hook);
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
        &pascal_project::ProjectContext,
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
            max_work: limits.max_compile_time_work.saturating_sub(used.work),
            max_byte_work: limits
                .max_compile_time_byte_work
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
        .min(super::DEFAULT_MAX_FILE_BYTES);
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
    let control = Control { cancel, budget };
    control.check()?;
    if !requires_session(source) {
        if source.len() > limits.max_expanded_bytes
            || source.len() > limits.max_work
            || limits.max_sources == 0
            || limits.max_segments == 0
        {
            return Err("mapped expansion limit reached".into());
        }
        let mut expanded = ExpandedSource::default();
        expanded.push_physical_range(uri.clone(), source, 0);
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
    let input = workspace.analysis_input();
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
    let store =
        LspSourceStore::from_input(context.clone(), &input).with_directory_limit(directory_limit);
    let store = Resources {
        store,
        control: &control,
    };
    let mut resolver = UnitResolver::new(
        context,
        workspace.workspace_root_paths(),
        store,
        resolver_limits,
    )
    .with_context_observations(&state.context_observations);
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
    let analysis = control.finish(resolver.analyze_compile_time(&root, &control))?;
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
            self.append_region(&uri, source, &occurrence.analysis, cursor..range.start)?;
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
        self.append_region(&uri, source, &occurrence.analysis, cursor..source.len())
    }

    fn append_region(
        &mut self,
        uri: &Url,
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
        self.result
            .expanded
            .push_physical_range(uri.clone(), &source[range.clone()], range.start);
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

    #[test]
    fn compatibility_retention_uses_finite_existing_size_domains() {
        let resources = super::super::ResourceLimits {
            max_files: 8,
            max_file_bytes: 128,
            max_total_bytes: 512,
            max_cache_bytes: 256,
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
}
