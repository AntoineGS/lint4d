use super::indexing::TargetPurpose;
use super::rename::{
    BindingClassification, CANCELLATION_MESSAGE, IndexedSources, MAX_SNAPSHOT_MAPPING_WORK,
    RenameSnapshot, SnapshotMode, SnapshotScope, SnapshotSeed, SourceRecord, WorkspaceInput,
    auto_import_source_is_relevant, build_scoped_snapshot, build_snapshot,
    input_source_is_readable_with_owner, is_cancelled, owner_for_input,
    project_context_and_metadata_for_input, project_context_and_metadata_for_owner,
    query_binding_info_for_input, reference_binding_info_for_input, revalidate_input,
    snapshot_records, source_for_input_with_cancel, source_for_input_with_owner,
};
use super::{KnownDocumentOwner, Workspace, conditional_context_for_uri};
use crate::coverage::{Coverage, Partial};
use crate::include_expansion::{MappingBudget, VirtualMapping};
use crate::navigation::{
    CompletionMetadata, CompletionOptions, CompletionResult, FoldingRangeOptions, InlayHintOptions,
    PhysicalProjection, SemanticTokenResolutionMode, completion_prefix_at_position,
};
use crate::workspace_index::CandidateQuery;
use crate::{NavigationIndex, NavigationTarget};
#[cfg(test)]
use lsp_types::CompletionList;
use lsp_types::{
    DocumentHighlight, DocumentSymbol, FoldingRange, Hover, InlayHint, Location, MarkupKind,
    Position, Range, SelectionRange, SemanticTokens, SignatureHelp, SymbolInformation, Url,
};
use pascal_project::{ProjectPathEntry, ProjectPathProvenance, has_invalid_project_selection};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const MAX_DOCUMENT_LINK_DIRECTIVES: usize = 64;
const MAX_DOCUMENT_LINK_FRESHNESS_RECORDS: usize = 4_096;
const MAX_DOCUMENT_LINK_FRESHNESS_BYTES: usize = 32 * 1024 * 1024;
const MAX_DOCUMENT_LINK_RESOURCE_FILE_BYTES: u64 = 256 * 1024;
const MAX_DOCUMENT_LINK_RESOURCE_TOTAL_BYTES: usize = 8 * 1024 * 1024;

pub(crate) struct NavigationResult {
    pub(crate) locations: Vec<Location>,
    pub(crate) state: super::NavigationState,
}

pub(crate) struct DiagnosticsResult {
    pub(crate) uri: Url,
    pub(crate) version: Option<i32>,
    pub(crate) publications: Vec<DiagnosticPublication>,
    pub(crate) publication_dependencies: HashMap<Url, Arc<Vec<SourceRecord>>>,
}

pub(crate) struct WorkspaceDiagnosticsResult {
    pub(crate) publications: Vec<DiagnosticPublication>,
    pub(crate) publication_dependencies: HashMap<Url, Arc<Vec<SourceRecord>>>,
    /// Units the scan did not reach are missing from `publications`.
    pub(crate) coverage: Coverage,
}

#[derive(Debug, Clone)]
pub(crate) struct DiagnosticPublication {
    pub(crate) uri: Url,
    pub(crate) version: Option<i32>,
    pub(crate) diagnostics: Vec<lsp_types::Diagnostic>,
}

pub(crate) fn hover_from_input(
    input: WorkspaceInput,
    uri: &Url,
    position: Position,
    format: MarkupKind,
    cancel: &AtomicBool,
) -> super::rename::Computed<Option<Hover>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let owner = match owner_for_input(&input, &uri, cancel) {
        Ok(owner) => owner,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if !input_source_is_readable_with_owner(&input, &uri, &owner) {
        return failed(
            source_generation,
            configuration_generation,
            format!("document is outside configured workspace roots or source paths: {uri}"),
        );
    }
    let classification = match query_binding_info_for_input(&input, &uri, position, cancel) {
        Ok(result) => result,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if classification.ignored_or_empty {
        let mut records = vec![classification.record];
        records.extend(classification.consumed_configuration);
        return with_records(
            source_generation,
            configuration_generation,
            Ok(None),
            records,
        );
    }

    let snapshot = match binding_snapshot(&input, &uri, position, true, classification, cancel) {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let records = snapshot_records(&snapshot);
    if let Err(error) = ensure_document_ready(&snapshot, &uri) {
        return with_records(
            source_generation,
            configuration_generation,
            Err(error),
            records,
        );
    }
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let value = snapshot
        .index
        .hover_with_cancel(&uri, position, format, cancel)
        .map(|hover| hover.map(|hover| with_coverage_note(hover, &answer_coverage(&snapshot))));
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    with_records(source_generation, configuration_generation, value, records)
}

/// Units named in a hover's incompleteness note.
const MAX_HOVER_NOTE_UNITS: usize = 3;

/// D16: a hover answered over unproven dependencies says so.
fn with_coverage_note(mut hover: Hover, coverage: &Coverage) -> Hover {
    let Some(note) = coverage_note(coverage) else {
        return hover;
    };
    match &mut hover.contents {
        lsp_types::HoverContents::Markup(markup) => {
            let separator = if markup.kind == MarkupKind::Markdown {
                "\n\n---\n"
            } else {
                "\n\n"
            };
            markup.value.push_str(separator);
            markup.value.push_str(&note);
        }
        lsp_types::HoverContents::Scalar(marked) => {
            hover.contents = lsp_types::HoverContents::Array(vec![
                marked.clone(),
                lsp_types::MarkedString::String(note),
            ]);
        }
        lsp_types::HoverContents::Array(items) => {
            items.push(lsp_types::MarkedString::String(note));
        }
    }
    hover
}

fn coverage_note(coverage: &Coverage) -> Option<String> {
    if coverage.is_complete() {
        return None;
    }
    let mut units = Vec::new();
    let mut reasons = Vec::new();
    for gap in &coverage.gaps {
        let Some(name) = gap
            .uri
            .as_ref()
            .and_then(|uri| uri.path_segments()?.next_back().map(str::to_string))
        else {
            let reason = sentence(&gap.reason);
            if !reasons.contains(&reason) {
                reasons.push(reason);
            }
            continue;
        };
        if !units.contains(&name) {
            units.push(name);
        }
    }
    let mut sentences = Vec::new();
    if !units.is_empty() || coverage.omitted > 0 {
        let named = units
            .iter()
            .take(MAX_HOVER_NOTE_UNITS)
            .cloned()
            .collect::<Vec<_>>();
        let mut subject = if named.is_empty() {
            "some dependencies".to_string()
        } else {
            named.join(", ")
        };
        // Gaps past the coverage's retention limit are units too.
        let more = units
            .len()
            .saturating_sub(named.len())
            .saturating_add(coverage.omitted);
        if more > 0 {
            subject.push_str(&format!(" and {more} more"));
        }
        sentences.push(format!(
            "Analysis of {subject} is incomplete; the result may be shadowed."
        ));
    }
    sentences.extend(reasons);
    Some(sentences.join(" "))
}

fn sentence(reason: &str) -> String {
    let mut characters = reason.trim().chars();
    let mut text = characters
        .next()
        .map(|first| first.to_uppercase().chain(characters).collect::<String>())
        .unwrap_or_default();
    if !text.ends_with('.') {
        text.push('.');
    }
    text
}

/// The coverage of a best-effort answer: the dependencies the snapshot
/// could not prove and the bindings the relaxed rules decided.
fn answer_coverage(snapshot: &RenameSnapshot) -> Coverage {
    let mut coverage = snapshot.dependency_coverage.clone();
    for reason in snapshot.index.take_relaxed_bindings() {
        coverage.note(None, reason);
    }
    coverage
}

#[cfg(test)]
pub(crate) fn completion_from_input(
    input: WorkspaceInput,
    uri: &Url,
    position: Position,
    cancel: &AtomicBool,
) -> super::rename::Computed<CompletionList> {
    completion_from_input_with_format(input, uri, position, MarkupKind::Markdown, cancel)
}

#[cfg(test)]
pub(crate) fn completion_from_input_with_format(
    input: WorkspaceInput,
    uri: &Url,
    position: Position,
    format: MarkupKind,
    cancel: &AtomicBool,
) -> super::rename::Computed<CompletionList> {
    let computed = completion_from_input_with_options(
        input,
        uri,
        position,
        CompletionOptions {
            format,
            defer_documentation: false,
            defer_detail: false,
            snippet_support: false,
        },
        cancel,
    );
    super::rename::Computed {
        source_generation: computed.source_generation,
        configuration_generation: computed.configuration_generation,
        value: computed.value.map(|result| result.list),
        records: computed.records,
    }
}

pub(crate) fn completion_from_input_with_options(
    input: WorkspaceInput,
    uri: &Url,
    position: Position,
    options: CompletionOptions,
    cancel: &AtomicBool,
) -> super::rename::Computed<CompletionResult> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let owner = match owner_for_input(&input, &uri, cancel) {
        Ok(owner) => owner,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if !input_source_is_readable_with_owner(&input, &uri, &owner) {
        return failed(
            source_generation,
            configuration_generation,
            format!("document is outside configured workspace roots or source paths: {uri}"),
        );
    }

    let snapshot = match assistance_snapshot(&input, &uri, Some(position), cancel) {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let records = snapshot_records(&snapshot);
    if let Err(error) = ensure_assistance_ready(&snapshot, &uri) {
        return with_records(
            source_generation,
            configuration_generation,
            Err(error),
            records,
        );
    }
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let value = snapshot
        .index
        .completion_with_cancel_and_options(&uri, position, options, cancel)
        .map(|mut result| {
            // D16: candidates from unproven dependencies may be missing.
            result.list.is_incomplete |= !answer_coverage(&snapshot).is_complete();
            result
        });
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    with_records(source_generation, configuration_generation, value, records)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn completion_metadata_from_input(
    input: WorkspaceInput,
    source_uri: &Url,
    position: Position,
    expected_source_generation: u64,
    expected_configuration_generation: u64,
    candidate_uri: &Url,
    candidate_index: usize,
    format: MarkupKind,
    resolve_documentation: bool,
    resolve_detail: bool,
    snippet_support: bool,
    original_records: &[super::rename::SourceRecord],
    cancel: &AtomicBool,
) -> super::rename::Computed<CompletionMetadata> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    if source_generation < expected_source_generation
        || configuration_generation < expected_configuration_generation
    {
        return failed(
            source_generation,
            configuration_generation,
            "completion resolution snapshot predates the retained completion identity; retry the request"
                .to_string(),
        );
    }
    if let Err(error) = revalidate_input(&input, original_records, cancel) {
        return failed(source_generation, configuration_generation, error);
    }
    let source_uri = super::canonical_file_uri(source_uri);
    let candidate_uri = super::canonical_file_uri(candidate_uri);
    let snapshot = match assistance_snapshot(&input, &source_uri, Some(position), cancel) {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let snapshot_records = snapshot_records(&snapshot);
    if let Err(error) = ensure_assistance_ready(&snapshot, &source_uri) {
        return with_records(
            source_generation,
            configuration_generation,
            Err(error),
            snapshot_records,
        );
    }
    if let Err(error) = completion_observations_match(&input, original_records, &snapshot_records) {
        return with_records(
            source_generation,
            configuration_generation,
            Err(error),
            snapshot_records,
        );
    }
    if let Err(error) = revalidate_input(&input, original_records, cancel) {
        return with_records(
            source_generation,
            configuration_generation,
            Err(error),
            snapshot_records,
        );
    }
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let completion = match snapshot.index.completion_with_cancel_and_options(
        &source_uri,
        position,
        CompletionOptions {
            format: format.clone(),
            defer_documentation: true,
            defer_detail: true,
            snippet_support,
        },
        cancel,
    ) {
        Ok(completion) => completion,
        Err(error) => {
            return with_records(
                source_generation,
                configuration_generation,
                Err(error),
                snapshot_records,
            );
        }
    };
    let mut matching = completion.seeds.iter().filter(|seed| {
        seed.candidate_uri() == &candidate_uri && seed.candidate_index() == candidate_index
    });
    let Some(seed) = matching.next() else {
        return with_records(
            source_generation,
            configuration_generation,
            Err(
                "completion declaration is no longer an exact candidate; request completion again"
                    .to_string(),
            ),
            snapshot_records,
        );
    };
    if matching.next().is_some() {
        return with_records(
            source_generation,
            configuration_generation,
            Err("completion declaration became ambiguous; request completion again".to_string()),
            snapshot_records,
        );
    }
    let value = snapshot.index.completion_metadata_for_seed(
        seed,
        format,
        resolve_documentation,
        resolve_detail,
        cancel,
    );
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let mut records = snapshot_records;
    records.extend(original_records.iter().cloned());
    with_records(source_generation, configuration_generation, value, records)
}

#[cfg(test)]
pub(crate) fn signature_help_from_input(
    input: WorkspaceInput,
    uri: &Url,
    position: Position,
    cancel: &AtomicBool,
) -> super::rename::Computed<Option<SignatureHelp>> {
    signature_help_from_input_with_format(input, uri, position, MarkupKind::Markdown, cancel)
}

pub(crate) fn signature_help_from_input_with_format(
    input: WorkspaceInput,
    uri: &Url,
    position: Position,
    format: MarkupKind,
    cancel: &AtomicBool,
) -> super::rename::Computed<Option<SignatureHelp>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let owner = match owner_for_input(&input, &uri, cancel) {
        Ok(owner) => owner,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if !input_source_is_readable_with_owner(&input, &uri, &owner) {
        return failed(
            source_generation,
            configuration_generation,
            format!("document is outside configured workspace roots or source paths: {uri}"),
        );
    }

    let snapshot = match assistance_snapshot(&input, &uri, None, cancel) {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let records = snapshot_records(&snapshot);
    if let Err(error) = ensure_assistance_ready(&snapshot, &uri) {
        return with_records(
            source_generation,
            configuration_generation,
            Err(error),
            records,
        );
    }
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let value = snapshot
        .index
        .signature_help_with_cancel(&uri, position, format, cancel);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    with_records(source_generation, configuration_generation, value, records)
}

pub(crate) fn semantic_tokens_from_input(
    input: WorkspaceInput,
    uri: &Url,
    range: Option<Range>,
    cancel: &AtomicBool,
) -> super::rename::Computed<SemanticTokens> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let owner = match owner_for_input(&input, &uri, cancel) {
        Ok(owner) => owner,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if !input_source_is_readable_with_owner(&input, &uri, &owner) {
        return failed(
            source_generation,
            configuration_generation,
            format!("document is outside configured workspace roots or source paths: {uri}"),
        );
    }

    let snapshot = match assistance_snapshot(&input, &uri, None, cancel) {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let records = snapshot_records(&snapshot);
    let resolution_mode = match ensure_semantic_tokens_ready(&snapshot, &uri) {
        Ok(mode) => mode,
        Err(error) => {
            return with_records(
                source_generation,
                configuration_generation,
                Err(error),
                records,
            );
        }
    };
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    // Include roots are indexed by their expanded text; tokens must be mapped
    // back onto the physical document before encoding positions.
    let value = if let Some(expansion) = snapshot.expansions.get(&uri) {
        let mut budget = MappingBudget::new(cancel, MAX_SNAPSHOT_MAPPING_WORK);
        let mut map = |virtual_range| {
            let spans = match expansion
                .expanded
                .map_range_with_budget(virtual_range, &mut budget)?
            {
                VirtualMapping::Exact(span) => vec![span],
                VirtualMapping::Many(spans) => spans,
                VirtualMapping::Unmapped => Vec::new(),
            };
            Ok(spans
                .into_iter()
                .filter(|span| span.uri == uri)
                .map(|span| span.range)
                .collect())
        };
        snapshot.index.semantic_tokens_with_physical_projection(
            &uri,
            range.as_ref(),
            cancel,
            resolution_mode,
            PhysicalProjection {
                source: &expansion.physical_source,
                map: &mut map,
            },
        )
    } else {
        snapshot.index.semantic_tokens_with_resolution_mode(
            &uri,
            range.as_ref(),
            cancel,
            resolution_mode,
        )
    };
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    with_records(source_generation, configuration_generation, value, records)
}

pub(crate) fn folding_ranges_from_input(
    input: WorkspaceInput,
    uri: &Url,
    options: FoldingRangeOptions,
    cancel: &AtomicBool,
) -> super::rename::Computed<Vec<FoldingRange>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let owner = match owner_for_input(&input, &uri, cancel) {
        Ok(owner) => owner,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if !input_source_is_readable_with_owner(&input, &uri, &owner) {
        return failed(
            source_generation,
            configuration_generation,
            format!("document is outside configured workspace roots or source paths: {uri}"),
        );
    }
    let (source, record) = match source_for_input_with_owner(&input, &uri, &owner, Some(cancel)) {
        Ok(result) => result,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let (context, metadata_records) = match project_context_and_metadata_for_owner(&owner, cancel) {
        Ok(result) => result,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    let mut index = NavigationIndex::new();
    if let Err(error) = index.update_with_context_with_cancel(
        uri.clone(),
        source,
        &conditional_context_for_uri(&context, &uri),
        cancel,
    ) {
        return failed(
            source_generation,
            configuration_generation,
            format!("could not index folding ranges for {uri}: {error}"),
        );
    }
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let value = index.folding_ranges_with_cancel(&uri, options, cancel);
    let mut records = vec![record];
    records.extend(metadata_records);
    with_records(source_generation, configuration_generation, value, records)
}

pub(crate) fn inlay_hints_from_input(
    input: WorkspaceInput,
    uri: &Url,
    range: Range,
    options: InlayHintOptions,
    cancel: &AtomicBool,
) -> super::rename::Computed<Vec<InlayHint>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let owner = match owner_for_input(&input, &uri, cancel) {
        Ok(owner) => owner,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if !input_source_is_readable_with_owner(&input, &uri, &owner) {
        return failed(
            source_generation,
            configuration_generation,
            format!("document is outside configured workspace roots or source paths: {uri}"),
        );
    }
    let snapshot = match assistance_snapshot(&input, &uri, None, cancel) {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let records = snapshot_records(&snapshot);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let value = snapshot
        .index
        .inlay_hints_with_cancel(&uri, range, options, cancel);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    with_records(source_generation, configuration_generation, value, records)
}

pub(crate) fn prepare_call_hierarchy_from_input(
    input: WorkspaceInput,
    uri: &Url,
    position: Position,
    cancel: &AtomicBool,
) -> super::rename::Computed<Option<Vec<lsp_types::CallHierarchyItem>>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let owner = match owner_for_input(&input, &uri, cancel) {
        Ok(owner) => owner,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if !input_source_is_readable_with_owner(&input, &uri, &owner) {
        return failed(
            source_generation,
            configuration_generation,
            format!("document is outside configured workspace roots or source paths: {uri}"),
        );
    }
    let snapshot = match assistance_snapshot(&input, &uri, Some(position), cancel) {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let records = snapshot_records(&snapshot);
    with_records(
        source_generation,
        configuration_generation,
        snapshot
            .index
            .prepare_call_hierarchy_with_cancel(&uri, position, cancel),
        records,
    )
}

pub(crate) fn prepare_type_hierarchy_from_input(
    input: WorkspaceInput,
    uri: &Url,
    position: Position,
    cancel: &AtomicBool,
) -> super::rename::Computed<Option<Vec<lsp_types::TypeHierarchyItem>>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let owner = match owner_for_input(&input, &uri, cancel) {
        Ok(owner) => owner,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if !input_source_is_readable_with_owner(&input, &uri, &owner) {
        return failed(
            source_generation,
            configuration_generation,
            format!("document is outside configured workspace roots or source paths: {uri}"),
        );
    }
    let snapshot = match assistance_snapshot(&input, &uri, Some(position), cancel) {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let records = snapshot_records(&snapshot);
    with_records(
        source_generation,
        configuration_generation,
        snapshot
            .index
            .prepare_type_hierarchy_with_cancel(&uri, position, cancel),
        records,
    )
}

pub(crate) fn type_hierarchy_supertypes_from_input(
    input: WorkspaceInput,
    item: &lsp_types::TypeHierarchyItem,
    cancel: &AtomicBool,
) -> super::rename::Computed<Option<Vec<lsp_types::TypeHierarchyItem>>> {
    type_hierarchy_edges_from_input(input, item, cancel, true)
}

pub(crate) fn type_hierarchy_subtypes_from_input(
    input: WorkspaceInput,
    item: &lsp_types::TypeHierarchyItem,
    cancel: &AtomicBool,
) -> super::rename::Computed<Option<Vec<lsp_types::TypeHierarchyItem>>> {
    type_hierarchy_edges_from_input(input, item, cancel, false)
}

fn type_hierarchy_edges_from_input(
    input: WorkspaceInput,
    item: &lsp_types::TypeHierarchyItem,
    cancel: &AtomicBool,
    supertypes: bool,
) -> super::rename::Computed<Option<Vec<lsp_types::TypeHierarchyItem>>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(&item.uri);
    let owner = match owner_for_input(&input, &uri, cancel) {
        Ok(owner) => owner,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if !input_source_is_readable_with_owner(&input, &uri, &owner) {
        return failed(
            source_generation,
            configuration_generation,
            format!("type hierarchy item is outside the selected project: {uri}"),
        );
    }
    let classification =
        match reference_binding_info_for_input(&input, &uri, item.selection_range.start, cancel) {
            Ok(classification) => classification,
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
    if classification.ignored_or_empty {
        let mut records = vec![classification.record];
        records.extend(classification.consumed_configuration);
        return with_records(
            source_generation,
            configuration_generation,
            Ok(None),
            records,
        );
    }
    let snapshot = match binding_snapshot(
        &input,
        &uri,
        item.selection_range.start,
        false,
        classification,
        cancel,
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let records = snapshot_records(&snapshot);
    if let Err(error) = ensure_reference_ready(&snapshot, &uri) {
        return with_records(
            source_generation,
            configuration_generation,
            Err(error),
            records,
        );
    }
    let value = if supertypes {
        snapshot
            .index
            .type_hierarchy_supertypes_with_cancel(item, cancel)
    } else {
        snapshot
            .index
            .type_hierarchy_subtypes_with_cancel(item, cancel)
    };
    with_records(source_generation, configuration_generation, value, records)
}

pub(crate) fn incoming_calls_from_input(
    input: WorkspaceInput,
    item: &lsp_types::CallHierarchyItem,
    cancel: &AtomicBool,
) -> super::rename::Computed<Vec<lsp_types::CallHierarchyIncomingCall>> {
    call_hierarchy_edges_from_input(input, item, cancel, true)
}

pub(crate) fn outgoing_calls_from_input(
    input: WorkspaceInput,
    item: &lsp_types::CallHierarchyItem,
    cancel: &AtomicBool,
) -> super::rename::Computed<Vec<lsp_types::CallHierarchyOutgoingCall>> {
    call_hierarchy_edges_from_input(input, item, cancel, false)
}

fn call_hierarchy_edges_from_input<T>(
    input: WorkspaceInput,
    item: &lsp_types::CallHierarchyItem,
    cancel: &AtomicBool,
    incoming: bool,
) -> super::rename::Computed<T>
where
    T: FromCallHierarchyEdges,
{
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(&item.uri);
    let owner = match owner_for_input(&input, &uri, cancel) {
        Ok(owner) => owner,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if !input_source_is_readable_with_owner(&input, &uri, &owner) {
        return failed(
            source_generation,
            configuration_generation,
            format!("call hierarchy item is outside the selected project: {uri}"),
        );
    }
    let classification =
        match reference_binding_info_for_input(&input, &uri, item.selection_range.start, cancel) {
            Ok(classification) => classification,
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
    if classification.ignored_or_empty {
        return failed(
            source_generation,
            configuration_generation,
            "call hierarchy target is not an active bound routine".to_string(),
        );
    }
    let snapshot = match binding_snapshot(
        &input,
        &uri,
        item.selection_range.start,
        false,
        classification,
        cancel,
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let records = snapshot_records(&snapshot);
    if let Err(error) = ensure_reference_ready(&snapshot, &uri) {
        return with_records(
            source_generation,
            configuration_generation,
            Err(error),
            records,
        );
    }
    let value = if incoming {
        T::incoming(snapshot.index.incoming_calls_with_cancel(item, cancel))
    } else {
        T::outgoing(snapshot.index.outgoing_calls_with_cancel(item, cancel))
    };
    with_records(source_generation, configuration_generation, value, records)
}

trait FromCallHierarchyEdges: Sized {
    fn incoming(
        value: Result<Vec<lsp_types::CallHierarchyIncomingCall>, String>,
    ) -> Result<Self, String>;
    fn outgoing(
        value: Result<Vec<lsp_types::CallHierarchyOutgoingCall>, String>,
    ) -> Result<Self, String>;
}

impl FromCallHierarchyEdges for Vec<lsp_types::CallHierarchyIncomingCall> {
    fn incoming(
        value: Result<Vec<lsp_types::CallHierarchyIncomingCall>, String>,
    ) -> Result<Self, String> {
        value
    }
    fn outgoing(
        _: Result<Vec<lsp_types::CallHierarchyOutgoingCall>, String>,
    ) -> Result<Self, String> {
        unreachable!()
    }
}

impl FromCallHierarchyEdges for Vec<lsp_types::CallHierarchyOutgoingCall> {
    fn incoming(
        _: Result<Vec<lsp_types::CallHierarchyIncomingCall>, String>,
    ) -> Result<Self, String> {
        unreachable!()
    }
    fn outgoing(
        value: Result<Vec<lsp_types::CallHierarchyOutgoingCall>, String>,
    ) -> Result<Self, String> {
        value
    }
}

pub(crate) fn type_definitions_from_input(
    input: WorkspaceInput,
    uri: &Url,
    position: Position,
    cancel: &AtomicBool,
) -> super::rename::Computed<Vec<Location>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let owner = match owner_for_input(&input, &uri, cancel) {
        Ok(owner) => owner,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if !input_source_is_readable_with_owner(&input, &uri, &owner) {
        return failed(
            source_generation,
            configuration_generation,
            format!("document is outside configured workspace roots or source paths: {uri}"),
        );
    }
    let classification = match query_binding_info_for_input(&input, &uri, position, cancel) {
        Ok(result) => result,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if classification.ignored_or_empty {
        let mut records = vec![classification.record];
        records.extend(classification.consumed_configuration);
        return with_records(
            source_generation,
            configuration_generation,
            Ok(Vec::new()),
            records,
        );
    }

    let snapshot = match type_definition_snapshot(&input, &uri, position, classification, cancel) {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let records = snapshot_records(&snapshot);
    if let Err(error) = ensure_document_ready(&snapshot, &uri) {
        return with_records(
            source_generation,
            configuration_generation,
            Err(error),
            records,
        );
    }
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let value = snapshot
        .index
        .type_definitions_with_cancel(&uri, position, cancel);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    with_records(source_generation, configuration_generation, value, records)
}

pub(crate) fn references_from_input(
    input: WorkspaceInput,
    uri: &Url,
    position: Position,
    include_declaration: bool,
    cancel: &AtomicBool,
) -> super::rename::Computed<Partial<Vec<Location>>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let owner = match owner_for_input(&input, &uri, cancel) {
        Ok(owner) => owner,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if has_invalid_project_selection(&owner.state.context_observations) {
        return failed(
            source_generation,
            configuration_generation,
            format!(
                "project selection is invalid; select a current project or Automatic for {uri}"
            ),
        );
    }
    if owner.state.context.override_error().is_some() {
        return failed(
            source_generation,
            configuration_generation,
            format!(
                "project override configuration is invalid; navigation is unavailable for {uri}"
            ),
        );
    }
    if !input_source_is_readable_with_owner(&input, &uri, &owner) {
        return failed(
            source_generation,
            configuration_generation,
            format!("document is outside configured workspace roots or source paths: {uri}"),
        );
    }
    let classification = match reference_binding_info_for_input(&input, &uri, position, cancel) {
        Ok(result) => result,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if classification.ignored_or_empty {
        let mut records = vec![classification.record];
        records.extend(classification.consumed_configuration);
        return with_records(
            source_generation,
            configuration_generation,
            Ok(Partial::complete(Vec::new())),
            records,
        );
    }

    let local = classification
        .info
        .as_ref()
        .is_some_and(|(info, _)| info.local);
    // Local bindings and include documents keep the walk (A17).
    let pascal_source = uri
        .to_file_path()
        .is_ok_and(|path| super::is_pascal_path(&path));
    let snapshot = if local || !pascal_source {
        binding_snapshot(&input, &uri, position, false, classification, cancel)
    } else {
        indexed_reference_snapshot(&input, &uri, position, &owner, classification, cancel)
    };
    let snapshot = match snapshot {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let records = snapshot_records(&snapshot);
    let (mut coverage, withheld) = match read_only_reference_coverage(&snapshot, &uri) {
        Ok(coverage) => coverage,
        Err(error) => {
            return with_records(
                source_generation,
                configuration_generation,
                Err(error),
                records,
            );
        }
    };
    for gap in &snapshot.index_gaps {
        coverage.note(None, gap.clone());
    }
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let value = snapshot
        .binding_locations(&uri, position, include_declaration, cancel, &mut coverage)
        .map(|mut locations| {
            locations.retain(|location| !withheld.contains(&location.uri));
            Partial {
                value: locations,
                coverage,
            }
        });
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    with_records(source_generation, configuration_generation, value, records)
}

pub(crate) fn highlights_from_input(
    input: WorkspaceInput,
    uri: &Url,
    position: Position,
    cancel: &AtomicBool,
) -> super::rename::Computed<Vec<DocumentHighlight>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let owner = match owner_for_input(&input, &uri, cancel) {
        Ok(owner) => owner,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if has_invalid_project_selection(&owner.state.context_observations) {
        return failed(
            source_generation,
            configuration_generation,
            format!(
                "project selection is invalid; select a current project or Automatic for {uri}"
            ),
        );
    }
    if owner.state.context.override_error().is_some() {
        return failed(
            source_generation,
            configuration_generation,
            format!(
                "project override configuration is invalid; navigation is unavailable for {uri}"
            ),
        );
    }
    if !input_source_is_readable_with_owner(&input, &uri, &owner) {
        return failed(
            source_generation,
            configuration_generation,
            format!("document is outside configured workspace roots or source paths: {uri}"),
        );
    }
    let classification = match reference_binding_info_for_input(&input, &uri, position, cancel) {
        Ok(result) => result,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if classification.ignored_or_empty {
        let mut records = vec![classification.record];
        records.extend(classification.consumed_configuration);
        return with_records(
            source_generation,
            configuration_generation,
            Ok(Vec::new()),
            records,
        );
    }

    let mut snapshot = match binding_snapshot(&input, &uri, position, true, classification, cancel)
    {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    // Highlights mark every occurrence of one binding, like references: an
    // inherited member the index cannot see must still withhold a global.
    snapshot.index.set_best_effort_bindings(false);
    let records = snapshot_records(&snapshot);
    if let Err(error) = ensure_document_ready(&snapshot, &uri) {
        return with_records(
            source_generation,
            configuration_generation,
            Err(error),
            records,
        );
    }
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let value = match snapshot.binding_highlights_in_document(&uri, position, cancel) {
        Ok(highlights) => highlights,
        Err(error) if error == CANCELLATION_MESSAGE => {
            return cancelled(source_generation, configuration_generation);
        }
        Err(error) if error.contains("10000-entry limit") => {
            return with_records(
                source_generation,
                configuration_generation,
                Err(error),
                records,
            );
        }
        Err(_error) => {
            return with_records(
                source_generation,
                configuration_generation,
                Ok(Vec::new()),
                records,
            );
        }
    };
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    with_records(
        source_generation,
        configuration_generation,
        Ok(value),
        records,
    )
}

/// References over index candidates (l4d-33 §6.1). Phase 1 resolves the
/// binding in the bounded snapshot hover uses and decides the candidate
/// rule (A3); phase 2 binds only the candidates the target indexes chose.
/// When the index cannot answer, a small workspace (A16) is walked as
/// before; a larger one is answered from phase 1 with a gap (U4).
fn indexed_reference_snapshot(
    input: &WorkspaceInput,
    uri: &Url,
    position: Position,
    owner: &KnownDocumentOwner,
    classification: BindingClassification,
    cancel: &AtomicBool,
) -> Result<RenameSnapshot, String> {
    let local = binding_snapshot(input, uri, position, true, classification.clone(), cancel);
    let phase_one = match local
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|snapshot| snapshot.reference_targets(uri, position, cancel))
    {
        Ok(targets) => Some(targets),
        Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
        Err(error) => {
            crate::trace::trace!("  references: phase 1 failed, member rule: {error}");
            None
        }
    };
    let scope = match indexed_reference_scope(
        input,
        uri,
        position,
        owner,
        &classification,
        phase_one.as_ref(),
        cancel,
    ) {
        Ok(Some(sources)) => Some(SnapshotScope::Indexed(sources)),
        Ok(None) => None,
        Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
        Err(error) => {
            crate::trace::trace!("  references: the index cannot answer: {error}");
            None
        }
    };
    let small = || match Workspace::from_analysis_input(input).is_small_workspace(cancel) {
        Ok(small) => Ok(small),
        Err(error) if error == CANCELLATION_MESSAGE => Err(error),
        Err(_) => Ok(false),
    };
    let scope = match scope {
        Some(scope) => scope,
        None if small()? => SnapshotScope::Walk,
        None => {
            let mut snapshot = local?;
            snapshot.index_gaps.push(INDEX_UNAVAILABLE_GAP.to_owned());
            return Ok(snapshot);
        }
    };
    binding_snapshot_scoped(input, uri, position, false, classification, scope, cancel)
}

const SYMBOLS_INDEX_UNAVAILABLE_GAP: &str =
    "the workspace index is unavailable; no workspace symbols were searched";

const INDEX_UNAVAILABLE_GAP: &str =
    "the workspace index is unavailable; only the current context's imports were searched";

/// The candidate sources of the target contexts (A8), or `None` when no
/// target context resolves.
fn indexed_reference_scope(
    input: &WorkspaceInput,
    uri: &Url,
    position: Position,
    owner: &KnownDocumentOwner,
    classification: &BindingClassification,
    phase_one: Option<&crate::navigation::ReferenceTargets>,
    cancel: &AtomicBool,
) -> Result<Option<IndexedSources>, String> {
    let started = std::time::Instant::now();
    let mut names = binding_candidate_names(
        &classification.record.text,
        classification
            .info
            .as_ref()
            .map(|(info, _)| info.names.clone()),
        position,
        uri,
    )?;
    if let Some(targets) = phase_one {
        names.extend(targets.names.iter().cloned());
        names.sort();
        names.dedup();
    }
    let top_level_units = phase_one
        .filter(|targets| {
            !targets.members.is_empty() && targets.members.iter().all(|member| member.top_level)
        })
        .map(|targets| {
            let mut units = targets
                .members
                .iter()
                .map(|member| member.unit.clone())
                .collect::<Vec<_>>();
            units.sort();
            units.dedup();
            units
        });

    let mut workspace = Workspace::from_analysis_input(input);
    let targets = workspace.index_targets(input, Some(uri), TargetPurpose::References, cancel)?;
    if targets.contexts.is_empty() {
        return Ok(None);
    }
    // Every target's aliases: a superset of the import names keeps the
    // importer rule sound in each context.
    let mut aliases = HashSet::new();
    let contexts =
        std::iter::once(&*owner.state).chain(targets.contexts.iter().filter_map(|(key, _)| {
            workspace.contexts.get(key).or_else(|| {
                input
                    .document_owners
                    .values()
                    .find(|known| known.key == *key)
                    .map(|known| &*known.state)
            })
        }));
    for state in contexts {
        aliases.extend(state.context.unit_aliases().iter().map(|(alias, target)| {
            (
                crate::navigation::canonical_name(alias),
                crate::navigation::canonical_name(target),
            )
        }));
    }
    let mut aliases = aliases.into_iter().collect::<Vec<_>>();
    aliases.sort();
    let query = CandidateQuery {
        names,
        top_level_units,
        aliases,
    };

    let mut must_include = vec![uri.clone()];
    if let Some(targets) = phase_one {
        must_include.extend(targets.members.iter().map(|member| member.uri.clone()));
    }
    must_include.extend(input.overlays.keys().cloned());
    must_include.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    must_include.dedup();
    let sources = workspace.indexed_sources(
        input,
        &targets,
        Some(&query),
        &must_include,
        Some(targets.requesting.as_ref().unwrap_or(&owner.key)),
        cancel,
    )?;
    crate::trace::trace!(
        "indexed references targets={} sources={} gaps={} elapsed_ms={:.1}",
        targets.contexts.len(),
        sources.sources.len(),
        sources.gaps.len(),
        started.elapsed().as_secs_f64() * 1000.0,
    );
    Ok(Some(sources))
}

fn binding_snapshot(
    input: &WorkspaceInput,
    uri: &Url,
    position: Position,
    document_local: bool,
    classification: BindingClassification,
    cancel: &AtomicBool,
) -> Result<RenameSnapshot, String> {
    binding_snapshot_scoped(
        input,
        uri,
        position,
        document_local,
        classification,
        SnapshotScope::Walk,
        cancel,
    )
}

fn binding_snapshot_scoped(
    input: &WorkspaceInput,
    uri: &Url,
    position: Position,
    document_local: bool,
    classification: BindingClassification,
    scope: SnapshotScope,
    cancel: &AtomicBool,
) -> Result<RenameSnapshot, String> {
    let uri = super::canonical_file_uri(uri);
    let BindingClassification {
        record: target_record,
        info: binding_info,
        consumed_configuration,
        ..
    } = classification;
    let (binding_info, self_contained) = binding_info
        .map_or((None, false), |(info, self_contained)| {
            (Some(info), self_contained)
        });
    let local = binding_info.as_ref().is_some_and(|info| info.local);
    let mode = if document_local {
        if local {
            SnapshotMode::Local
        } else {
            SnapshotMode::LocalWithImports
        }
    } else if local {
        SnapshotMode::Local
    } else {
        SnapshotMode::Workspace
    };
    let skip_imports_for: &[Url] = if self_contained {
        std::slice::from_ref(&uri)
    } else {
        &[]
    };
    build_binding_snapshot_scoped(
        input,
        &uri,
        position,
        target_record,
        binding_info,
        consumed_configuration,
        mode,
        skip_imports_for,
        scope,
        cancel,
    )
}

fn assistance_snapshot(
    input: &WorkspaceInput,
    uri: &Url,
    completion_position: Option<Position>,
    cancel: &AtomicBool,
) -> Result<RenameSnapshot, String> {
    let (source, record) = source_for_input_with_cancel(input, uri, Some(cancel))?;
    let candidate_names = completion_position
        .and_then(|position| completion_prefix_at_position(&source, position))
        .into_iter()
        .collect::<Vec<_>>();
    let (_context, consumed_configuration) =
        project_context_and_metadata_for_input(input, uri, cancel)?;
    build_snapshot(
        input,
        std::slice::from_ref(uri),
        &candidate_names,
        SnapshotMode::Assistance,
        Some(
            SnapshotSeed::new(record)
                .with_consumed_configuration(&consumed_configuration)
                .with_completion_position(completion_position)
                .best_effort(),
        ),
        &[],
        cancel,
    )
}

fn type_definition_snapshot(
    input: &WorkspaceInput,
    uri: &Url,
    position: Position,
    classification: BindingClassification,
    cancel: &AtomicBool,
) -> Result<RenameSnapshot, String> {
    let uri = super::canonical_file_uri(uri);
    let BindingClassification {
        record: target_record,
        info: binding_info,
        consumed_configuration,
        ..
    } = classification;
    build_binding_snapshot(
        input,
        &uri,
        position,
        target_record,
        binding_info.map(|(info, _)| info),
        consumed_configuration,
        SnapshotMode::LocalWithImports,
        &[],
        cancel,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_binding_snapshot(
    input: &WorkspaceInput,
    uri: &Url,
    position: Position,
    target_record: super::rename::SourceRecord,
    binding_info: Option<crate::navigation::RenameBindingInfo>,
    consumed_configuration: Vec<super::rename::SourceRecord>,
    mode: SnapshotMode,
    skip_imports_for: &[Url],
    cancel: &AtomicBool,
) -> Result<RenameSnapshot, String> {
    build_binding_snapshot_scoped(
        input,
        uri,
        position,
        target_record,
        binding_info,
        consumed_configuration,
        mode,
        skip_imports_for,
        SnapshotScope::Walk,
        cancel,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_binding_snapshot_scoped(
    input: &WorkspaceInput,
    uri: &Url,
    position: Position,
    target_record: super::rename::SourceRecord,
    binding_info: Option<crate::navigation::RenameBindingInfo>,
    consumed_configuration: Vec<super::rename::SourceRecord>,
    mode: SnapshotMode,
    skip_imports_for: &[Url],
    scope: SnapshotScope,
    cancel: &AtomicBool,
) -> Result<RenameSnapshot, String> {
    let candidate_names = binding_candidate_names(
        &target_record.text,
        binding_info.map(|info| info.names),
        position,
        uri,
    )?;
    let seed =
        SnapshotSeed::new(target_record).with_consumed_configuration(&consumed_configuration);
    // Only the document-local read-only features (hover, highlights, type
    // definition) bind in `LocalWithImports`; they answer best effort (D16).
    let seed = if mode == SnapshotMode::LocalWithImports {
        seed.best_effort()
    } else {
        seed
    };
    build_scoped_snapshot(
        input,
        std::slice::from_ref(uri),
        &candidate_names,
        mode,
        Some(seed),
        skip_imports_for,
        scope,
        cancel,
    )
}

/// The binding's names, or the identifier at `position` when it has none.
fn binding_candidate_names(
    text: &str,
    names: Option<Vec<String>>,
    position: Position,
    uri: &Url,
) -> Result<Vec<String>, String> {
    let original_name = super::rename::identifier_at_position(text, position)
        .ok_or_else(|| format!("no identifier at navigation position in {uri}"))?;
    let mut candidate_names = match names {
        Some(mut names) => {
            if names.is_empty() {
                names.push(original_name);
            }
            names
        }
        None => vec![original_name],
    };
    candidate_names.sort();
    candidate_names.dedup();
    Ok(candidate_names)
}

/// The include error that blocks a read-only feature on `uri`: the first one
/// recorded for the document itself, or for an owner of it when it is an
/// include file. A failure in a unit the document only imports cannot change
/// how the document parses; a best-effort snapshot reports it as coverage
/// (D16). Rename, references, and code actions stay fail-closed.
fn blocking_include_error<'a>(snapshot: &'a RenameSnapshot, uri: &Url) -> Option<&'a str> {
    let owns_document = |owner: &Url| {
        owner == uri
            || snapshot
                .expansions
                .get(owner)
                .is_some_and(|expansion| expansion.dependencies.contains(uri))
    };
    if let Some((_, error)) = snapshot
        .include_error_owners
        .iter()
        .find(|(owner, _)| owns_document(owner))
    {
        return Some(error.as_str());
    }
    // Past the error limit owners are no longer recorded, but an owner with
    // an unsupported directive still is.
    if snapshot
        .unsupported_directive_owners
        .iter()
        .any(owns_document)
    {
        return snapshot
            .include_errors
            .iter()
            .find(|error| error.contains(uri.as_str()))
            .or(snapshot.include_errors.last())
            .map(String::as_str);
    }
    // Without a best-effort audit an early stop leaves later owners,
    // possibly the document, unaudited.
    if snapshot.other_include_errors && !snapshot.best_effort {
        return snapshot
            .include_errors
            .iter()
            .find(|error| !error.contains("unsupported directive"))
            .or(snapshot.include_errors.first())
            .map(String::as_str);
    }
    None
}

fn ensure_reference_ready(snapshot: &RenameSnapshot, uri: &Url) -> Result<(), String> {
    if !snapshot.records.contains_key(uri) {
        return Err(format!(
            "reference document was not retained in the workspace snapshot: {uri}"
        ));
    }
    if !snapshot.readable.contains(uri) {
        return Err(format!(
            "reference document is outside configured workspace roots: {uri}"
        ));
    }
    if let Some(error) = snapshot.include_errors.first() {
        return Err(format!("reference workspace scan incomplete: {error}"));
    }
    if !snapshot.complete {
        let reason = snapshot
            .incomplete_reason
            .as_deref()
            .unwrap_or("bounded source discovery did not finish");
        if snapshot
            .sources
            .get(uri)
            .is_some_and(|source| super::rename::may_contain_include_directive(source.as_bytes()))
        {
            return Err(format!(
                "reference workspace scan incomplete: include dependency analysis is incomplete: {reason}"
            ));
        }
        return Err(format!("reference workspace scan incomplete: {reason}"));
    }
    Ok(())
}

/// Read-only references over a workspace snapshot. The requested document
/// must be proven, so a failure there still rejects the request. Elsewhere,
/// units whose include audit or project context is unproven are returned
/// for withholding, and they and an incomplete discovery become gaps.
fn read_only_reference_coverage(
    snapshot: &RenameSnapshot,
    uri: &Url,
) -> Result<(Coverage, HashSet<Url>), String> {
    if !snapshot.records.contains_key(uri) {
        return Err(format!(
            "reference document was not retained in the workspace snapshot: {uri}"
        ));
    }
    if !snapshot.readable.contains(uri) {
        return Err(format!(
            "reference document is outside configured workspace roots: {uri}"
        ));
    }
    let requested_has_include = snapshot
        .sources
        .get(uri)
        .is_some_and(|source| super::rename::may_contain_include_directive(source.as_bytes()));
    let other_include_error = snapshot
        .other_include_errors
        .then(|| {
            snapshot
                .include_errors
                .iter()
                .find(|error| !error.contains("unsupported directive"))
                .or(snapshot.include_errors.first())
        })
        .flatten();
    if snapshot.unsupported_directive_owners.contains(uri) {
        let error = snapshot
            .include_errors
            .iter()
            .find(|error| error.contains(uri.as_str()))
            .map_or(
                "the document contains an unsupported directive",
                String::as_str,
            );
        return Err(format!("reference workspace scan incomplete: {error}"));
    }
    if requested_has_include && let Some(error) = other_include_error {
        return Err(format!("reference workspace scan incomplete: {error}"));
    }
    // The requesting document and its imports stay strict: a recorded
    // include failure leaves every unit with an include directive unproven,
    // and an unproven provider may declare a name shadowing the binding.
    if let Some(error) = other_include_error {
        let requesting = std::iter::once(uri).chain(
            snapshot
                .expansions
                .iter()
                .filter(|(_, expansion)| expansion.dependencies.contains(uri))
                .map(|(owner, _)| owner),
        );
        let unproven_provider = requesting.flat_map(|document| {
            snapshot
                .index
                .imports(document)
                .into_iter()
                .filter_map(|import| {
                    snapshot
                        .index
                        .import_provider_uri(document, &import.name)
                        .cloned()
                })
                .collect::<Vec<_>>()
        });
        if let Some(provider) = unproven_provider.into_iter().find(|provider| {
            snapshot.sources.get(provider).is_some_and(|source| {
                super::rename::may_contain_include_directive(source.as_bytes())
            })
        }) {
            return Err(format!(
                "reference workspace scan incomplete: {error} (the requesting document uses {provider})"
            ));
        }
    }
    if snapshot
        .expansions
        .get(uri)
        .is_some_and(|expansion| !expansion.complete)
    {
        return Err(format!(
            "reference workspace scan incomplete: include expansion is incomplete for {uri}"
        ));
    }

    let mut coverage = Coverage::default();
    let mut withheld = HashSet::new();
    for owner in &snapshot.unsupported_directive_owners {
        coverage.note(
            Some(owner),
            "the unit contains an unsupported compiler directive",
        );
        withheld.insert(owner.clone());
    }
    for unit in &snapshot.incomplete_context_sources {
        coverage.note(
            Some(unit),
            "the unit's project context is ambiguous or incomplete",
        );
        withheld.insert(unit.clone());
    }
    if let Some(error) = other_include_error {
        // Which owner failed is not recorded, and an early stop leaves later
        // owners unaudited, so no unit with an include directive is proven.
        coverage.note(None, error.as_str());
        let mut owners = snapshot
            .sources
            .iter()
            .filter(|(_, source)| super::rename::may_contain_include_directive(source.as_bytes()))
            .map(|(owner, _)| owner)
            .collect::<Vec<_>>();
        owners.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        for owner in owners {
            coverage.note(Some(owner), "include analysis is incomplete for the unit");
            withheld.insert(owner.clone());
        }
    }
    let mut incomplete_expansions = snapshot
        .expansions
        .iter()
        .filter(|(_, expansion)| !expansion.complete)
        .map(|(owner, _)| owner)
        .collect::<Vec<_>>();
    incomplete_expansions.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    for owner in incomplete_expansions {
        coverage.note(Some(owner), "include expansion is incomplete for the unit");
        withheld.insert(owner.clone());
    }
    // An indexed snapshot audits its candidates relaxed (l4d-jeh4): a
    // candidate whose include audit failed is withheld. A gap naming no
    // unit withholds every unit with an include directive.
    // Its importers are withheld too: what it declares may shadow theirs.
    // One level is enough: a Pascal unit sees only the interface of the
    // units it uses itself, never what those use in turn, so a failed
    // unit's declarations reach no unit beyond its direct importers.
    let dependency_gaps = &snapshot.dependency_coverage;
    let mut unattributed = dependency_gaps.omitted > 0;
    let mut failed = HashSet::new();
    for gap in &dependency_gaps.gaps {
        coverage.note(gap.uri.as_ref(), gap.reason.as_str());
        match &gap.uri {
            Some(owner) => {
                failed.insert(owner.clone());
            }
            None => unattributed = true,
        }
    }
    if !failed.is_empty() {
        for importer in snapshot.sources.keys() {
            let imports_failed = snapshot.index.imports(importer).iter().any(|import| {
                snapshot
                    .index
                    .import_provider_uri(importer, &import.name)
                    .is_some_and(|provider| failed.contains(provider))
            });
            if imports_failed && !failed.contains(importer) {
                coverage.note(
                    Some(importer),
                    "a unit it uses could not be analyzed, so its bindings are unproven",
                );
                withheld.insert(importer.clone());
            }
        }
        withheld.extend(failed);
    }
    if unattributed {
        withheld.extend(
            snapshot
                .sources
                .iter()
                .filter(|(_, source)| {
                    super::rename::may_contain_include_directive(source.as_bytes())
                })
                .map(|(owner, _)| owner.clone()),
        );
    }
    // Include files are reported under their own URIs; a withheld owner
    // withholds what it includes.
    let included = withheld
        .iter()
        .filter_map(|owner| snapshot.expansions.get(owner))
        .flat_map(|expansion| expansion.dependencies.iter().cloned())
        .collect::<Vec<_>>();
    withheld.extend(included);
    if !snapshot.complete {
        coverage.note(
            None,
            snapshot
                .incomplete_reason
                .as_deref()
                .unwrap_or("bounded source discovery did not finish"),
        );
    }
    Ok((coverage, withheld))
}

fn ensure_document_ready(snapshot: &RenameSnapshot, uri: &Url) -> Result<(), String> {
    if !snapshot.records.contains_key(uri) {
        return Err(format!(
            "highlight document was not retained in the local snapshot: {uri}"
        ));
    }
    if !snapshot.readable.contains(uri) {
        return Err(format!(
            "highlight document is outside configured workspace roots: {uri}"
        ));
    }
    if matches!(
        snapshot.mode,
        SnapshotMode::LocalWithImports | SnapshotMode::Assistance
    ) {
        if let Some(error) = blocking_include_error(snapshot, uri) {
            return Err(format!("highlight dependency scan incomplete: {error}"));
        }
        if !snapshot.complete {
            let reason = snapshot
                .incomplete_reason
                .as_deref()
                .unwrap_or("required import discovery did not finish");
            return Err(format!("highlight dependency scan incomplete: {reason}"));
        }
    }
    Ok(())
}

fn ensure_assistance_ready(snapshot: &RenameSnapshot, uri: &Url) -> Result<(), String> {
    if !snapshot.records.contains_key(uri) {
        return Err(format!(
            "assistance document was not retained in the local snapshot: {uri}"
        ));
    }
    if !snapshot.readable.contains(uri) {
        return Err(format!(
            "assistance document is outside configured workspace roots: {uri}"
        ));
    }
    if let Some(error) = blocking_include_error(snapshot, uri) {
        return Err(format!("assistance dependency scan incomplete: {error}"));
    }
    if !snapshot.complete {
        let reason = snapshot
            .incomplete_reason
            .as_deref()
            .unwrap_or("required import discovery did not finish");
        return Err(format!("assistance dependency scan incomplete: {reason}"));
    }
    Ok(())
}

fn ensure_semantic_tokens_ready(
    snapshot: &RenameSnapshot,
    uri: &Url,
) -> Result<SemanticTokenResolutionMode, String> {
    if !snapshot.records.contains_key(uri) {
        return Err(format!(
            "semantic-token document was not retained in the local snapshot: {uri}"
        ));
    }
    if !snapshot.readable.contains(uri) {
        return Err(format!(
            "semantic-token document is outside configured workspace roots: {uri}"
        ));
    }
    if !snapshot.complete || blocking_include_error(snapshot, uri).is_some() {
        return Ok(SemanticTokenResolutionMode::LexicalOnly);
    }
    Ok(SemanticTokenResolutionMode::Full)
}

struct SyntaxDocument {
    source: String,
    context: std::sync::Arc<pascal_project::ProjectContext>,
    records: Vec<super::rename::SourceRecord>,
}

fn syntax_owner_for_input(
    input: &WorkspaceInput,
    uri: &Url,
    cancel: &AtomicBool,
) -> Result<KnownDocumentOwner, String> {
    let owner = owner_for_input(input, uri, cancel)?;
    if !input_source_is_readable_with_owner(input, uri, &owner) {
        return Err(format!(
            "document is outside configured workspace roots or source paths: {uri}"
        ));
    }
    Ok(owner)
}

fn syntax_document_for_owner(
    input: &WorkspaceInput,
    uri: &Url,
    owner: &KnownDocumentOwner,
    cancel: &AtomicBool,
) -> Result<SyntaxDocument, String> {
    let (source, record) = source_for_input_with_owner(input, uri, owner, Some(cancel))?;
    let (context, metadata_records) = project_context_and_metadata_for_owner(owner, cancel)?;
    let mut records = Vec::with_capacity(metadata_records.len().saturating_add(1));
    records.push(record);
    records.extend(metadata_records);
    Ok(SyntaxDocument {
        source,
        context,
        records,
    })
}

fn syntax_index_for_document(
    input: &WorkspaceInput,
    uri: &Url,
    document: SyntaxDocument,
    cancel: &AtomicBool,
    operation: &str,
) -> Result<(NavigationIndex, Vec<super::rename::SourceRecord>), String> {
    let cached = input
        .cached_documents
        .get(uri)
        .filter(|cached| cached.context == document.context)
        .map(|cached| cached.parsed.clone());
    let conditional_context = conditional_context_for_uri(&document.context, uri);
    let mut index = NavigationIndex::new();
    index
        .update_with_context_and_cached_with_cancel(
            uri.clone(),
            document.source,
            &conditional_context,
            cached,
            cancel,
        )
        .map_err(|error| format!("could not index {operation} for {uri}: {error}"))?;
    Ok((index, document.records))
}

pub(crate) fn document_symbols_from_input(
    input: WorkspaceInput,
    uri: &Url,
    cancel: &AtomicBool,
) -> super::rename::Computed<Vec<DocumentSymbol>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let owner = match syntax_owner_for_input(&input, &uri, cancel) {
        Ok(owner) => owner,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let document = match syntax_document_for_owner(&input, &uri, &owner, cancel) {
        Ok(document) => document,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    let (index, records) =
        match syntax_index_for_document(&input, &uri, document, cancel, "document symbols") {
            Ok(result) => result,
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let value = index.document_symbols_with_cancel(&uri, cancel);
    super::rename::Computed {
        source_generation,
        configuration_generation,
        value,
        records,
    }
}

pub(crate) fn selection_ranges_from_input(
    input: WorkspaceInput,
    uri: &Url,
    positions: Vec<Position>,
    cancel: &AtomicBool,
) -> super::rename::Computed<Vec<SelectionRange>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    let owner = match syntax_owner_for_input(&input, &uri, cancel) {
        Ok(owner) => owner,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    if let Err(error) = crate::navigation::validate_selection_position_count(&positions) {
        return failed(source_generation, configuration_generation, error);
    }
    if positions.is_empty() {
        return with_records(
            source_generation,
            configuration_generation,
            Ok(Vec::new()),
            Vec::new(),
        );
    }
    let document = match syntax_document_for_owner(&input, &uri, &owner, cancel) {
        Ok(document) => document,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    let (index, records) =
        match syntax_index_for_document(&input, &uri, document, cancel, "selection ranges") {
            Ok(result) => result,
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let value = index.selection_ranges_with_cancel(&uri, &positions, cancel);
    with_records(source_generation, configuration_generation, value, records)
}

pub(crate) fn navigation_from_input(
    input: WorkspaceInput,
    uri: &Url,
    position: Position,
    target: NavigationTarget,
    cancel: &AtomicBool,
) -> super::rename::Computed<NavigationResult> {
    navigation_from_input_with_policy(input, uri, position, target, true, cancel)
}

/// Navigation for a caller that must not answer best effort: the
/// implementation code lens shows a bare count, like the references lens.
pub(crate) fn strict_navigation_from_input(
    input: WorkspaceInput,
    uri: &Url,
    position: Position,
    target: NavigationTarget,
    cancel: &AtomicBool,
) -> super::rename::Computed<NavigationResult> {
    navigation_from_input_with_policy(input, uri, position, target, false, cancel)
}

fn navigation_from_input_with_policy(
    input: WorkspaceInput,
    uri: &Url,
    position: Position,
    target: NavigationTarget,
    best_effort: bool,
    cancel: &AtomicBool,
) -> super::rename::Computed<NavigationResult> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    let mut workspace = super::Workspace::from_analysis_input(&input);
    workspace.set_best_effort_navigation(best_effort);
    let value = match workspace.navigate_with_cancel(&uri, position, target, cancel) {
        Ok(locations) => {
            for reason in workspace.index.take_relaxed_bindings() {
                workspace.warn_project(format!("navigation in {uri}: {reason}"));
            }
            Ok(locations)
        }
        Err(error) if error == CANCELLATION_MESSAGE => {
            return cancelled(source_generation, configuration_generation);
        }
        Err(error) => {
            workspace.warn(format!("navigation failed for {uri}: {error}"));
            Ok(Vec::new())
        }
    };
    let state = workspace.navigation_state();
    let records = match workspace.analysis_records(cancel) {
        Ok(records) => records,
        Err(error) if error == CANCELLATION_MESSAGE => {
            return cancelled(source_generation, configuration_generation);
        }
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    with_records(
        source_generation,
        configuration_generation,
        value.map(|locations| NavigationResult { locations, state }),
        records,
    )
}

pub(crate) fn formatting_from_input(
    input: WorkspaceInput,
    uri: &Url,
    tab_size: u32,
    insert_spaces: bool,
    cancel: &AtomicBool,
) -> super::rename::Computed<Option<lsp_types::TextEdit>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    let mut workspace = super::Workspace::from_analysis_input(&input);
    let value = match workspace.formatting_edit_with_cancel(&uri, tab_size, insert_spaces, cancel) {
        Ok(edit) => Ok(edit),
        Err(error) if error == CANCELLATION_MESSAGE => {
            return cancelled(source_generation, configuration_generation);
        }
        Err(error) => Err(error),
    };
    let records = match workspace.analysis_records(cancel) {
        Ok(records) => records,
        Err(error) if error == CANCELLATION_MESSAGE => {
            return cancelled(source_generation, configuration_generation);
        }
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    with_records(source_generation, configuration_generation, value, records)
}

pub(crate) fn document_links_from_input(
    input: WorkspaceInput,
    uri: &Url,
    cancel: &AtomicBool,
) -> super::rename::Computed<Vec<lsp_types::DocumentLink>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let owner = match owner_for_input(&input, &uri, cancel) {
        Ok(owner) => owner,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if !input_source_is_readable_with_owner(&input, &uri, &owner) {
        return failed(
            source_generation,
            configuration_generation,
            "document is outside configured workspace roots or source paths".to_string(),
        );
    }
    let (source, source_record) =
        match source_for_input_with_owner(&input, &uri, &owner, Some(cancel)) {
            Ok(result) => result,
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
    let (context, metadata_records) = match project_context_and_metadata_for_owner(&owner, cancel) {
        Ok(result) => result,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let mut workspace = super::Workspace::from_analysis_input(&input);
    let context_key = match workspace.context_for_uri_with_cancel(&uri, Some(cancel)) {
        Ok(key) => key,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let expansion = match super::compile_time::expand_with_limits(
        &workspace,
        &uri,
        &source,
        &context_key,
        super::compile_time::SessionLimits {
            expansion: workspace.include_expansion_limits(),
            directory_entries: MAX_DOCUMENT_LINK_FRESHNESS_RECORDS,
        },
        cancel,
        None,
    ) {
        Ok(expansion) => expansion,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let Some(session) = expansion.expanded.session.as_ref() else {
        // Directive-free sources use the physical passthrough and have no
        // link-bearing directives or compile-time child decisions to expose.
        let mut records = vec![source_record];
        records.extend(metadata_records);
        return with_records(
            source_generation,
            configuration_generation,
            Ok(Vec::new()),
            records,
        );
    };
    let conditionals = &session.root;
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    if !conditionals.complete || conditionals.directives.len() > MAX_DOCUMENT_LINK_DIRECTIVES {
        let mut records = vec![source_record];
        records.extend(metadata_records);
        return with_records(
            source_generation,
            configuration_generation,
            Ok(Vec::new()),
            records,
        );
    }
    let mut links = Vec::new();
    if let Err(error) = workspace.record_document_link_expansion_sources(
        &expansion,
        &context,
        &context_key,
        cancel,
        MAX_DOCUMENT_LINK_FRESHNESS_RECORDS,
        MAX_DOCUMENT_LINK_FRESHNESS_BYTES,
    ) {
        return failed(source_generation, configuration_generation, error);
    }
    let mut resource_bytes = 0usize;
    let mut resource_records = Vec::new();
    for directive in conditionals.directives.iter().filter(|directive| {
        directive.activity == pascal_core::conditional::Truth::True
            && matches!(
                directive.kind,
                pascal_core::conditional::DirectiveKind::Include
                    | pascal_core::conditional::DirectiveKind::Harmless
            )
    }) {
        if is_cancelled(cancel) {
            return cancelled(source_generation, configuration_generation);
        }
        let Some(body_start) = source
            .get(directive.start..directive.end)
            .and_then(|fragment| fragment.find(&directive.body))
            .map(|offset| directive.start + offset)
        else {
            continue;
        };
        let raw_body = directive.body.as_str();
        let leading_trivia = raw_body.len().saturating_sub(raw_body.trim_start().len());
        let body = raw_body.trim_start();
        let Some((keyword, operand)) = body.split_once(char::is_whitespace) else {
            continue;
        };
        let resource = keyword.eq_ignore_ascii_case("R");
        if !resource && !matches!(keyword.to_ascii_uppercase().as_str(), "I" | "INCLUDE") {
            continue;
        }
        let operand = operand.trim_start();
        let lead = body.len().saturating_sub(operand.len());
        let (path, quote_prefix) = if let Some(rest) = operand.strip_prefix('\'') {
            let Some(end) = rest.find('\'') else { continue };
            (&rest[..end], 1)
        } else if let Some(rest) = operand.strip_prefix('"') {
            let Some(end) = rest.find('"') else { continue };
            (&rest[..end], 1)
        } else {
            // Include resolution treats the whole unquoted remainder as the
            // filename, including spaces. The link range must do the same.
            (operand.trim_end(), 0)
        };
        if path.is_empty()
            || path.len() > 4096
            || path.contains(['*', '?', '$'])
            || path.contains('\\')
            || path.contains(['\'', '"']) && quote_prefix == 0
            || Path::new(path)
                .components()
                .any(|component| component == Component::ParentDir)
        {
            continue;
        }
        let target = if resource {
            let remainder = &operand[quote_prefix + path.len()..];
            let quoted_end = match quote_prefix {
                1 if operand.starts_with('\'') => "'",
                1 => "\"",
                _ => "",
            };
            let components = Path::new(path).components().collect::<Vec<_>>();
            if remainder.trim() != quoted_end
                || components.is_empty()
                || components.len() > 8
                || !components
                    .iter()
                    .all(|component| matches!(component, Component::Normal(_)))
            {
                continue;
            }
            let Ok(source_path) = uri.to_file_path() else {
                continue;
            };
            let Some(directory) = source_path.parent() else {
                continue;
            };
            let target_path = directory.join(path);
            let Some(entry) = context.read_policy().entry_for_path(&target_path) else {
                continue;
            };
            if !context.read_policy().allows_location(&entry) {
                continue;
            }
            let bytes = match context
                .read_policy()
                .read_payload_bytes(&entry, MAX_DOCUMENT_LINK_RESOURCE_FILE_BYTES)
            {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            if is_cancelled(cancel) {
                return cancelled(source_generation, configuration_generation);
            }
            resource_bytes = resource_bytes.saturating_add(bytes.len());
            if resource_bytes > MAX_DOCUMENT_LINK_RESOURCE_TOTAL_BYTES {
                return failed(
                    source_generation,
                    configuration_generation,
                    "document-link resource read limit exceeded".to_string(),
                );
            }
            let Some(target) = Url::from_file_path(&target_path).ok() else {
                continue;
            };
            resource_records.push(SourceRecord {
                uri: target.clone(),
                text: String::new(),
                version: None,
                stamp: None,
                open: false,
                path: Some(target_path.clone()),
                path_stamp: super::path_stamp(&target_path),
                content_hash: Some(super::content_hash_bytes(&bytes)),
                parsed_text_hash: None,
                content_bytes: None,
                candidate_membership: None,
                candidate_observations: Vec::new(),
                read_policy: Some(context.read_policy().clone()),
                path_entry: Some(entry),
                include_payload: false,
                missing_provider_candidate: false,
                document_link_missing_candidate: false,
                document_link_case_variant: None,
                directory_observation: false,
                document_link_ancestor: false,
                missing_provider_scope: None,
                auto_import_provider_observation: false,
                auto_import_scopes: Vec::new(),
            });
            if let Err(error) =
                record_document_link_ancestors(&target_path, &mut resource_records, cancel)
            {
                return failed(source_generation, configuration_generation, error);
            }
            target
        } else {
            let Some(target) = session
                .includes
                .iter()
                .find(|include| {
                    include.owner == session.occurrences[0].id
                        && include.directive.range == (directive.start..directive.end)
                })
                .and_then(|include| match &include.target {
                    pascal_core::resolver::Resolution::Found(source) => {
                        Url::from_file_path(&source.path).ok()
                    }
                    _ => None,
                })
            else {
                continue;
            };
            // A legacy include may be readable through a symlink. A document link
            // must not point at a lexical URI whose actual file escapes the
            // requester's authorized location; refuse symlink components without
            // changing the legacy include expansion policy used by navigation.
            let Ok(target_path) = target.to_file_path() else {
                continue;
            };
            let real_path = if let Ok(real_path) = target_path.canonicalize() {
                pascal_project::path_identity::without_verbatim_prefix(real_path)
            } else if input.overlays.contains_key(&target) {
                let Some((directory, name)) = target_path.parent().zip(target_path.file_name())
                else {
                    continue;
                };
                let Ok(directory) = directory.canonicalize() else {
                    continue;
                };
                pascal_project::path_identity::without_verbatim_prefix(directory).join(name)
            } else {
                continue;
            };
            // The legacy include reader may admit `../` beside the owner.
            // A document link needs a selected project/workspace read root,
            // independent of that legacy navigation fallback.
            if super::context_path_entry(&context, &real_path).is_none()
                || !context.read_policy().allows_location(&ProjectPathEntry {
                    path: target_path,
                    provenance: ProjectPathProvenance::LegacyNative,
                })
            {
                continue;
            }
            if let Err(error) =
                record_document_link_ancestors(&real_path, &mut resource_records, cancel)
            {
                return failed(source_generation, configuration_generation, error);
            }
            target
        };
        let start = body_start + leading_trivia + lead + quote_prefix;
        let end = start + path.len();
        let (Some(start), Some(end)) = (
            crate::text::offset_to_position(&source, start),
            crate::text::offset_to_position(&source, end),
        ) else {
            continue;
        };
        links.push(lsp_types::DocumentLink {
            range: lsp_types::Range::new(start, end),
            target: Some(target),
            tooltip: None,
            data: None,
        });
        if links.len() >= MAX_DOCUMENT_LINK_DIRECTIVES {
            break;
        }
    }
    let records = match workspace.analysis_records(cancel) {
        Ok(mut records) => {
            records.push(source_record);
            records.extend(metadata_records);
            records.extend(resource_records);
            records
        }
        Err(error) if error == CANCELLATION_MESSAGE => {
            return cancelled(source_generation, configuration_generation);
        }
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    with_records(
        source_generation,
        configuration_generation,
        Ok(links),
        records,
    )
}

/// A leaf's stamp and bytes do not witness a change to one of its parent
/// directories. Retain the whole ancestor chain so replacing a parent with a
/// symlink is rejected at delivery even if the new leaf has identical bytes.
fn record_document_link_ancestors(
    target: &Path,
    records: &mut Vec<SourceRecord>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut directory = target.parent();
    let mut depth = 0usize;
    while let Some(path) = directory {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        depth += 1;
        if depth > 32 || records.len() >= MAX_DOCUMENT_LINK_FRESHNESS_RECORDS {
            return Err("document-link ancestor observation limit exceeded".to_string());
        }
        let stamp = super::path_stamp_result(path)
            .map_err(|error| {
                format!(
                    "cannot observe document-link ancestor {}: {error}",
                    path.display()
                )
            })?
            .ok_or_else(|| format!("document-link ancestor disappeared: {}", path.display()))?;
        if stamp.is_symlink {
            return Err(format!(
                "document-link ancestor became a symlink: {}",
                path.display()
            ));
        }
        let uri = Url::from_file_path(path)
            .map_err(|_| format!("invalid document-link ancestor: {}", path.display()))?;
        records.push(SourceRecord {
            uri,
            text: String::new(),
            version: None,
            stamp: None,
            open: false,
            path: Some(path.to_path_buf()),
            path_stamp: Some(stamp),
            content_hash: None,
            parsed_text_hash: None,
            content_bytes: None,
            candidate_membership: None,
            candidate_observations: Vec::new(),
            read_policy: None,
            path_entry: None,
            include_payload: false,
            missing_provider_candidate: false,
            document_link_missing_candidate: false,
            document_link_case_variant: None,
            directory_observation: true,
            document_link_ancestor: true,
            missing_provider_scope: None,
            auto_import_provider_observation: false,
            auto_import_scopes: Vec::new(),
        });
        directory = path.parent();
    }
    Ok(())
}

pub(crate) fn range_formatting_from_input(
    input: WorkspaceInput,
    uri: &Url,
    range: Range,
    on_type_cursor: Option<Position>,
    tab_size: u32,
    insert_spaces: bool,
    cancel: &AtomicBool,
) -> super::rename::Computed<Vec<lsp_types::TextEdit>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let mut workspace = super::Workspace::from_analysis_input(&input);
    let value = match workspace.range_formatting_edits_with_cancel(
        &uri,
        range,
        on_type_cursor,
        tab_size,
        insert_spaces,
        cancel,
    ) {
        Ok(edits) => Ok(edits),
        Err(error) if error == CANCELLATION_MESSAGE => {
            return cancelled(source_generation, configuration_generation);
        }
        Err(error) => Err(error),
    };
    let records = match workspace.analysis_records(cancel) {
        Ok(records) => records,
        Err(error) if error == CANCELLATION_MESSAGE => {
            return cancelled(source_generation, configuration_generation);
        }
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    with_records(source_generation, configuration_generation, value, records)
}

pub(crate) fn diagnostics_from_input(
    input: WorkspaceInput,
    uri: &Url,
    cancel: &AtomicBool,
) -> super::rename::Computed<DiagnosticsResult> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    let version = input.document_versions.get(&uri).copied();
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    let mut workspace = super::Workspace::from_analysis_input(&input);
    let value = match workspace.diagnostics_for_with_cancel(&uri, cancel) {
        Ok(diagnostics) => Ok(DiagnosticsResult {
            uri,
            version,
            publications: diagnostics,
            publication_dependencies: HashMap::new(),
        }),
        Err(error) if error == CANCELLATION_MESSAGE => {
            return cancelled(source_generation, configuration_generation);
        }
        Err(error) => Err(error),
    };
    let records = match workspace.analysis_records(cancel) {
        Ok(records) => records,
        Err(error) if error == CANCELLATION_MESSAGE => {
            return cancelled(source_generation, configuration_generation);
        }
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let value = value.map(|mut result| {
        let shared = Arc::new(records.clone());
        result.publication_dependencies = result
            .publications
            .iter()
            .map(|publication| (publication.uri.clone(), Arc::clone(&shared)))
            .collect();
        result
    });
    with_records(source_generation, configuration_generation, value, records)
}

pub(crate) fn workspace_diagnostics_from_input(
    input: WorkspaceInput,
    cancel: &AtomicBool,
) -> super::rename::Computed<WorkspaceDiagnosticsResult> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    let snapshot = match super::rename::build_rejectable_workspace_snapshot(&input, cancel) {
        Ok(snapshot) => snapshot,
        Err(error) if error == CANCELLATION_MESSAGE => {
            return cancelled(source_generation, configuration_generation);
        }
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    // Each publication is computed for one unit on its own, so units the
    // scan did not reach only make the report shorter.
    let mut coverage = Coverage::default();
    if !snapshot.complete {
        coverage.note(
            None,
            snapshot
                .incomplete_reason
                .as_deref()
                .unwrap_or("bounded source discovery did not finish"),
        );
    }

    for unit in &snapshot.incomplete_context_sources {
        coverage.note(
            Some(unit),
            "the unit's project context is ambiguous or incomplete",
        );
    }
    let mut uris = snapshot
        .sources
        .keys()
        .filter(|uri| {
            uri.to_file_path()
                .ok()
                .is_some_and(|path| super::is_pascal_path(&path))
                && !snapshot.incomplete_context_sources.contains(uri)
        })
        .cloned()
        .collect::<Vec<_>>();
    uris.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    uris.dedup();

    let mut merged = HashMap::<Url, DiagnosticPublication>::new();
    let mut publication_dependencies = HashMap::<Url, Arc<Vec<SourceRecord>>>::new();
    let mut records = super::rename::snapshot_records(&snapshot);
    let mut workspace = super::Workspace::from_analysis_input(&input);
    if let Err(error) = workspace.prepare_diagnostic_root_ownership(&uris, cancel) {
        if error == CANCELLATION_MESSAGE {
            return cancelled(source_generation, configuration_generation);
        }
        return failed(source_generation, configuration_generation, error);
    }
    // Ownership preparation intentionally does not contribute report
    // dependencies.  Each root below gets a fresh bounded evidence set while
    // all prepared expansions remain available to semantic ownership checks.
    workspace.clear_diagnostic_analysis_records();
    for uri in uris {
        if is_cancelled(cancel) {
            return cancelled(source_generation, configuration_generation);
        }
        let context_key = match workspace.context_for_uri_with_cancel(&uri, Some(cancel)) {
            Ok(context_key) => context_key,
            Err(error) if error == CANCELLATION_MESSAGE => {
                return cancelled(source_generation, configuration_generation);
            }
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
        let publications = match workspace.diagnostics_for_with_cancel(&uri, cancel) {
            Ok(publications) => publications,
            Err(error) if error == CANCELLATION_MESSAGE => {
                return cancelled(source_generation, configuration_generation);
            }
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
        let additional = match workspace.diagnostic_analysis_records(&context_key, cancel) {
            Ok(additional) => additional,
            Err(error) if error == CANCELLATION_MESSAGE => {
                return cancelled(source_generation, configuration_generation);
            }
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
        workspace.clear_diagnostic_analysis_records();
        records.extend(additional.iter().cloned());
        let shared_dependencies = Arc::new(additional);
        for publication in publications {
            let publication_uri = publication.uri.clone();
            let entry =
                merged
                    .entry(publication.uri.clone())
                    .or_insert_with(|| DiagnosticPublication {
                        uri: publication.uri.clone(),
                        version: publication.version,
                        diagnostics: Vec::new(),
                    });
            if entry.version.is_none() {
                entry.version = publication.version;
            }
            for diagnostic in publication.diagnostics {
                if !entry.diagnostics.contains(&diagnostic) {
                    entry.diagnostics.push(diagnostic);
                }
            }
            match publication_dependencies.entry(publication_uri) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(Arc::clone(&shared_dependencies));
                }
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    let dependencies = Arc::make_mut(slot.get_mut());
                    dependencies.extend(shared_dependencies.iter().cloned());
                }
            }
        }
    }

    let mut publications = merged.into_values().collect::<Vec<_>>();
    publications.sort_by(|left, right| left.uri.as_str().cmp(right.uri.as_str()));
    records.sort_by(|left, right| {
        left.uri
            .as_str()
            .cmp(right.uri.as_str())
            .then_with(|| left.text.len().cmp(&right.text.len()))
    });
    if records.len() > 32_768 {
        return failed(
            source_generation,
            configuration_generation,
            "workspace diagnostic dependency record limit reached".to_string(),
        );
    }
    with_records(
        source_generation,
        configuration_generation,
        Ok(WorkspaceDiagnosticsResult {
            publications,
            publication_dependencies,
            coverage,
        }),
        records,
    )
}

#[cfg(test)]
thread_local! {
    static TEST_FACT_SYMBOLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static TEST_FACT_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Files read to answer workspace symbols from index facts since the last
/// call.
#[cfg(test)]
pub(crate) fn test_take_fact_reads() -> usize {
    TEST_FACT_READS.with(|count| count.replace(0))
}

/// Workspace symbols answered from index facts since the last call.
#[cfg(test)]
pub(crate) fn test_take_fact_symbols() -> usize {
    TEST_FACT_SYMBOLS.with(|count| count.replace(0))
}

pub(crate) fn workspace_symbols_from_input(
    input: WorkspaceInput,
    query: &str,
    cancel: &AtomicBool,
) -> super::rename::Computed<Partial<Vec<SymbolInformation>>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    let mut workspace = Workspace::from_analysis_input(&input);
    let targets =
        match workspace.index_targets(&input, None, TargetPurpose::WorkspaceSymbols, cancel) {
            Ok(targets) => targets,
            Err(error) if error == CANCELLATION_MESSAGE => {
                return cancelled(source_generation, configuration_generation);
            }
            Err(error) => {
                crate::trace::trace!("  workspace symbols: no index targets: {error}");
                // U4: only a small workspace (A16) is walked without the index.
                return match workspace.is_small_workspace(cancel) {
                    Ok(true) => walked_workspace_symbols(input, query, cancel),
                    Ok(false) => {
                        let mut coverage = Coverage::default();
                        coverage.note(None, SYMBOLS_INDEX_UNAVAILABLE_GAP);
                        super::rename::Computed {
                            source_generation,
                            configuration_generation,
                            value: Ok(Partial {
                                value: Vec::new(),
                                coverage,
                            }),
                            records: Vec::new(),
                        }
                    }
                    Err(error) if error == CANCELLATION_MESSAGE => {
                        cancelled(source_generation, configuration_generation)
                    }
                    Err(error) => failed(source_generation, configuration_generation, error),
                };
            }
        };
    // A16: a small workspace with nothing open or configured keeps the walk.
    if targets.from_open_or_configured == 0 && targets.small_workspace {
        return walked_workspace_symbols(input, query, cancel);
    }
    match indexed_workspace_symbols(&mut workspace, &input, &targets, query, cancel) {
        Ok((value, records)) => super::rename::Computed {
            source_generation,
            configuration_generation,
            value: Ok(value),
            records,
        },
        Err(error) if error == CANCELLATION_MESSAGE => {
            cancelled(source_generation, configuration_generation)
        }
        Err(error) => failed(source_generation, configuration_generation, error),
    }
}

/// Workspace symbols of the target contexts (§6.2): index facts of the
/// members proven current, merged with the parses of unverified members
/// and open documents.
fn indexed_workspace_symbols(
    workspace: &mut Workspace,
    input: &WorkspaceInput,
    targets: &super::indexing::Targets,
    query: &str,
    cancel: &AtomicBool,
) -> Result<(Partial<Vec<SymbolInformation>>, Vec<SourceRecord>), String> {
    let started = std::time::Instant::now();
    let direct = targets.from_open_or_configured > 0;
    let mut must_include = input.overlays.keys().cloned().collect::<Vec<_>>();
    must_include.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    let (sources, facts) =
        workspace.indexed_symbols(input, targets, query, direct, &must_include, cancel)?;
    crate::trace::trace!(
        "indexed symbols targets={} sources={} facts={} gaps={} elapsed_ms={:.1}",
        targets.contexts.len(),
        sources.sources.len(),
        facts.len(),
        sources.gaps.len(),
        started.elapsed().as_secs_f64() * 1000.0,
    );
    // The facts are distinct declarations, so an answer over the cap is
    // known before any indexed file is read.
    if let Some(error) = crate::navigation::too_many_workspace_symbols(facts.len()) {
        return Err(error);
    }
    let mut coverage = Coverage::default();
    if !direct {
        coverage.note(
            None,
            "no project context is open; open a source or select a project",
        );
    }
    let mut found = Vec::new();
    let records = if sources.sources.is_empty() && input.overlays.is_empty() {
        for gap in &sources.gaps {
            coverage.note(None, gap.as_str());
        }
        sources
            .baseline
            .iter()
            .filter_map(|(path, stamp)| {
                super::rename::path_record_at(
                    path.clone(),
                    stamp.clone(),
                    None,
                    None,
                    None,
                    None,
                    None,
                    false,
                )
            })
            .collect()
    } else {
        let snapshot = super::rename::build_indexed_snapshot(
            input,
            &[],
            &[],
            SnapshotMode::WorkspaceSymbols,
            None,
            sources,
            cancel,
        )?;
        if !snapshot.complete {
            coverage.note(
                None,
                snapshot
                    .incomplete_reason
                    .as_deref()
                    .unwrap_or("bounded source discovery did not finish"),
            );
        }
        for gap in &snapshot.index_gaps {
            coverage.note(None, gap.as_str());
        }
        found.extend(
            snapshot
                .index
                .workspace_symbols_with_cancel(query, cancel)?
                .into_iter()
                .filter(|symbol| snapshot.readable.contains(&symbol.location.uri)),
        );
        snapshot_records(&snapshot)
    };
    let fact_files = facts
        .iter()
        .filter_map(|(_, symbol)| Url::from_file_path(&symbol.path).ok())
        .collect::<HashSet<_>>();
    let parsed_elsewhere = found
        .iter()
        .filter(|symbol| !fact_files.contains(&symbol.location.uri))
        .count();
    if let Some(error) =
        crate::navigation::too_many_workspace_symbols(facts.len() + parsed_elsewhere)
    {
        return Err(error);
    }
    found.extend(fact_symbols(workspace, facts, &mut coverage, cancel)?);

    // The parses come first, so a duplicate keeps the live answer.
    let key = |symbol: &SymbolInformation| {
        let range = symbol.location.range;
        (
            symbol.location.uri.to_string(),
            (range.start.line, range.start.character),
            std::cmp::Reverse((range.end.line, range.end.character)),
        )
    };
    found.sort_by_cached_key(key);
    let mut seen = HashSet::new();
    found.retain(|symbol| {
        seen.insert((
            symbol.location.uri.clone(),
            symbol.location.range,
            symbol.name.clone(),
        ))
    });
    if let Some(error) = crate::navigation::too_many_workspace_symbols(found.len()) {
        return Err(error);
    }
    Ok((
        Partial {
            value: found,
            coverage,
        },
        records,
    ))
}

/// Index declarations as symbol information. Each file with a match is read
/// once to convert its byte ranges to UTF-16 positions.
fn fact_symbols(
    workspace: &Workspace,
    facts: Vec<(super::ContextKey, crate::workspace_index::IndexedSymbol)>,
    coverage: &mut Coverage,
    cancel: &AtomicBool,
) -> Result<Vec<SymbolInformation>, String> {
    let mut by_file = std::collections::BTreeMap::<String, Vec<_>>::new();
    for (key, symbol) in facts {
        by_file
            .entry(super::rename::path_key(&symbol.path))
            .or_default()
            .push((key, symbol));
    }
    let mut result = Vec::new();
    for symbols in by_file.into_values() {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let (key, first) = &symbols[0];
        let path = first.path.clone();
        let Ok(uri) = Url::from_file_path(&path) else {
            continue;
        };
        #[cfg(test)]
        TEST_FACT_READS.with(|count| count.set(count.get() + 1));
        let text = match super::rename::read_context_source(workspace, key, &path, cancel) {
            Ok(text) => text,
            Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
            Err(error) => {
                coverage.note(
                    Some(&uri),
                    format!("indexed symbols could not be read: {error}"),
                );
                continue;
            }
        };
        let positions = crate::text::PositionIndex::new_with_cancel(&text, cancel)
            .map_err(|()| CANCELLATION_MESSAGE.to_string())?;
        let mut stale = false;
        for (_, symbol) in symbols {
            let declaration = symbol.declaration;
            let Some((_, range)) = declaration.location else {
                continue;
            };
            let (Some(start), Some(end)) = (
                positions.offset_to_position(&text, range.start as usize),
                positions.offset_to_position(&text, range.end as usize),
            ) else {
                stale = true;
                continue;
            };
            #[cfg(test)]
            TEST_FACT_SYMBOLS.with(|count| count.set(count.get() + 1));
            #[allow(deprecated)]
            result.push(SymbolInformation {
                name: declaration.display.into(),
                kind: crate::navigation::lsp_kind_for_shape(declaration.shape),
                tags: None,
                deprecated: None,
                location: Location {
                    uri: uri.clone(),
                    range: Range { start, end },
                },
                container_name: symbol.container,
            });
        }
        if stale {
            coverage.note(
                Some(&uri),
                "the file changed after it was indexed; some symbols were not listed",
            );
        }
    }
    Ok(result)
}

fn walked_workspace_symbols(
    input: WorkspaceInput,
    query: &str,
    cancel: &AtomicBool,
) -> super::rename::Computed<Partial<Vec<SymbolInformation>>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let snapshot = match build_snapshot(
        &input,
        &[],
        &[],
        SnapshotMode::WorkspaceSymbols,
        None,
        &[],
        cancel,
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    // Every indexed declaration is proven on its own, so units discovery
    // did not reach only make the list shorter.
    let mut coverage = Coverage::default();
    if !snapshot.complete {
        coverage.note(
            None,
            snapshot
                .incomplete_reason
                .as_deref()
                .unwrap_or("bounded source discovery did not finish"),
        );
    }

    let value = snapshot
        .index
        .workspace_symbols_with_cancel(query, cancel)
        .map(|symbols| Partial {
            value: symbols
                .into_iter()
                .filter(|symbol| snapshot.readable.contains(&symbol.location.uri))
                .collect(),
            coverage,
        });
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    super::rename::Computed {
        source_generation,
        configuration_generation,
        value,
        records: snapshot_records(&snapshot),
    }
}

fn cancelled<T>(
    source_generation: u64,
    configuration_generation: u64,
) -> super::rename::Computed<T> {
    super::rename::Computed {
        source_generation,
        configuration_generation,
        value: Err(CANCELLATION_MESSAGE.to_string()),
        records: Vec::new(),
    }
}

fn failed<T>(
    source_generation: u64,
    configuration_generation: u64,
    error: String,
) -> super::rename::Computed<T> {
    super::rename::Computed {
        source_generation,
        configuration_generation,
        value: Err(error),
        records: Vec::new(),
    }
}

fn with_records<T>(
    source_generation: u64,
    configuration_generation: u64,
    value: Result<T, String>,
    records: Vec<super::rename::SourceRecord>,
) -> super::rename::Computed<T> {
    super::rename::Computed {
        source_generation,
        configuration_generation,
        value,
        records,
    }
}

fn completion_observations_match(
    input: &super::rename::WorkspaceInput,
    original: &[super::rename::SourceRecord],
    rebuilt: &[super::rename::SourceRecord],
) -> Result<(), String> {
    let mut matched = vec![false; rebuilt.len()];
    for original_record in original {
        let Some(index) = rebuilt
            .iter()
            .enumerate()
            .position(|(index, rebuilt_record)| {
                !matched[index] && completion_observations_equal(original_record, rebuilt_record)
            })
        else {
            if completion_observation_is_superseded_by_irrelevant_overlay(
                input,
                original_record,
                original,
            ) {
                continue;
            }
            return Err(
                "completion dependency observations changed while resolving; retry the request"
                    .to_string(),
            );
        };
        matched[index] = true;
    }
    Ok(())
}

fn completion_observation_is_superseded_by_irrelevant_overlay(
    input: &super::rename::WorkspaceInput,
    record: &super::rename::SourceRecord,
    original: &[super::rename::SourceRecord],
) -> bool {
    if !record.auto_import_provider_observation {
        return false;
    }
    let Some(path) = record.path.as_deref() else {
        return false;
    };
    let Some(overlay) = input.overlays.get(&super::canonical_file_uri(&record.uri)) else {
        return false;
    };
    let scopes = original
        .iter()
        .flat_map(|record| record.auto_import_scopes.iter())
        .collect::<Vec<_>>();
    scopes.is_empty()
        || scopes.iter().all(|scope| {
            !scope.matches_path(path) || !auto_import_source_is_relevant(&overlay.text, scope)
        })
}

fn completion_observations_equal(
    left: &super::rename::SourceRecord,
    right: &super::rename::SourceRecord,
) -> bool {
    super::canonical_file_uri(&left.uri) == super::canonical_file_uri(&right.uri)
        && (left.text.is_empty() || right.text.is_empty() || left.text == right.text)
        && left.version == right.version
        && left.stamp == right.stamp
        && left.open == right.open
        && left.path == right.path
        && left.path_stamp == right.path_stamp
        && (left.open || left.content_hash == right.content_hash)
        && left.parsed_text_hash == right.parsed_text_hash
        && left.candidate_membership == right.candidate_membership
        && left.candidate_observations == right.candidate_observations
        && left.read_policy == right.read_policy
        && left.path_entry == right.path_entry
        && left.include_payload == right.include_payload
        && left.missing_provider_candidate == right.missing_provider_candidate
        && left.document_link_missing_candidate == right.document_link_missing_candidate
        && left.document_link_case_variant == right.document_link_case_variant
        && left.missing_provider_scope == right.missing_provider_scope
        && left.auto_import_provider_observation == right.auto_import_provider_observation
        && left.auto_import_scopes == right.auto_import_scopes
        && match (&left.content_bytes, &right.content_bytes) {
            (Some(left), Some(right)) => left == right,
            _ => true,
        }
}

#[cfg(test)]
#[path = "queries_best_effort_tests.rs"]
mod best_effort_tests;

#[cfg(test)]
mod tests {
    use super::{
        completion_from_input, completion_from_input_with_options, completion_metadata_from_input,
        document_symbols_from_input, highlights_from_input, hover_from_input,
        references_from_input, selection_ranges_from_input, semantic_tokens_from_input,
        signature_help_from_input, type_definitions_from_input, workspace_symbols_from_input,
    };
    use crate::navigation::CompletionOptions;
    use crate::project_cache::IndexAnchor;
    #[cfg(target_os = "linux")]
    use crate::workspace::content_hash_bytes;
    use crate::workspace::indexing::IndexBound;
    use crate::workspace::rename::{
        CANCELLATION_MESSAGE, Computed, WorkspaceInput, binding_info_for_input,
        install_snapshot_priority_barrier, owner_for_input, project_context_and_metadata_for_input,
        revalidate_input,
    };
    use crate::workspace::{Workspace, WorkspaceOptions};
    use lsp_types::{MarkupKind, Position, Url};
    use pascal_project::ProjectPathProvenance;
    #[cfg(target_os = "linux")]
    use pascal_project::delphi_overrides::EffectiveOverrides;
    use pascal_project::delphi_overrides::OverrideSession;
    #[cfg(target_os = "linux")]
    use pascal_project::{MetadataObservation, ProjectOptions, ProjectPathEntry, ReadPolicy};
    use std::collections::HashSet;
    #[cfg(target_os = "linux")]
    use std::ffi::CString;
    use std::fs;
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::channel;
    use std::thread;
    use tempfile::TempDir;

    #[cfg(unix)]
    #[test]
    fn document_link_ancestor_observation_rejects_symlink_at_capture() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("isolated workspace");
        let root = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).expect("workspace root");
        fs::create_dir_all(&outside).expect("outside root");
        fs::write(outside.join("Selected.inc"), "const Selected = 1;\n").expect("outside source");
        symlink(&outside, root.join("assets")).expect("symlinked ancestor");

        let error = super::record_document_link_ancestors(
            &root.join("assets/Selected.inc"),
            &mut Vec::new(),
            &AtomicBool::new(false),
        )
        .expect_err("a symlinked ancestor must never become the trusted baseline");
        assert!(error.contains("symlink"), "unexpected refusal: {error}");
    }

    // On a case-insensitive volume the requested spelling also opens the
    // file; the link still targets the spelling on disk.
    #[test]
    fn include_document_link_targets_the_on_disk_spelling() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = pascal_project::path_identity::without_verbatim_prefix(
            fs::canonicalize(temp.path()).expect("canonical workspace"),
        );
        let main = root.join("Main.pas");
        fs::write(
            &main,
            "unit Main;\ninterface\nimplementation\n{$I SELECTED.INC}\nend.\n",
        )
        .expect("main source");
        fs::write(root.join("Selected.inc"), "const Selected = 1;\n").expect("include");
        let workspace = test_workspace(vec![root.clone()], WorkspaceOptions::default());

        let computed = super::document_links_from_input(
            workspace.analysis_input(),
            &source_uri(&main),
            &AtomicBool::new(false),
        );

        let links = computed.value.expect("document links");
        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!(
            links[0]
                .target
                .as_ref()
                .and_then(|uri| uri.to_file_path().ok()),
            Some(root.join("Selected.inc"))
        );
    }

    fn nested_resource_links(root: &Path) -> Computed<Vec<lsp_types::DocumentLink>> {
        let main = root.join("Main.pas");
        fs::create_dir_all(root.join("assets")).expect("assets directory");
        fs::write(
            &main,
            "unit Main;\ninterface\nimplementation\n{$R assets/Form.dfm}\nend.\n",
        )
        .expect("main source");
        fs::write(root.join("assets/Form.dfm"), "object Form1: TForm1\nend\n").expect("form");
        let workspace = test_workspace(vec![root.to_path_buf()], WorkspaceOptions::default());
        let computed = super::document_links_from_input(
            workspace.analysis_input(),
            &source_uri(&main),
            &AtomicBool::new(false),
        );
        assert_eq!(
            computed.value.as_ref().expect("document links").len(),
            1,
            "{computed:?}"
        );
        computed
    }

    #[test]
    fn document_link_revalidation_ignores_new_entries_in_ancestor_directories() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let computed = nested_resource_links(&root);
        let input =
            test_workspace(vec![root.clone()], WorkspaceOptions::default()).analysis_input();

        fs::write(temp.path().join("Sibling.txt"), "above the root").expect("parent entry");
        fs::write(root.join(".Main.pas.swp"), "editor swap file").expect("root entry");
        fs::write(root.join("assets/Other.dfm"), "object Other: TForm\nend\n")
            .expect("nested entry");

        revalidate_input(&input, &computed.records, &AtomicBool::new(false))
            .expect("new entries beside a link target's ancestors must not stale the links");
    }

    #[cfg(unix)]
    #[test]
    fn document_link_revalidation_rejects_an_ancestor_replaced_by_a_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let computed = nested_resource_links(&root);
        let input =
            test_workspace(vec![root.clone()], WorkspaceOptions::default()).analysis_input();

        let moved = temp.path().join("moved-assets");
        fs::rename(root.join("assets"), &moved).expect("move assets away");
        symlink(&moved, root.join("assets")).expect("symlink in place of assets");

        revalidate_input(&input, &computed.records, &AtomicBool::new(false))
            .expect_err("an ancestor replaced by a symlink must stale the links");
    }

    fn test_workspace(roots: Vec<PathBuf>, options: WorkspaceOptions) -> Workspace {
        Workspace::with_override_session(roots, options, OverrideSession::new(None))
    }

    struct ReferenceFixture {
        _temp: TempDir,
        root: PathBuf,
        source: PathBuf,
        input: WorkspaceInput,
    }

    fn external_fixture() -> ReferenceFixture {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let external = temp.path().join("library/src");
        let source = external.join("External.pas");
        let source_text =
            "unit External;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
        fs::create_dir_all(&root).expect("workspace directory");
        fs::create_dir_all(&external).expect("external source directory");
        fs::write(&source, source_text).expect("external source");

        let workspace = test_workspace(
            vec![root.clone()],
            WorkspaceOptions {
                source_paths: vec![external.to_string_lossy().into_owned()],
                ..WorkspaceOptions::default()
            },
        );
        ReferenceFixture {
            _temp: temp,
            root,
            source,
            input: workspace.analysis_input(),
        }
    }

    fn source_uri(path: &Path) -> Url {
        Url::from_file_path(path).expect("source URI")
    }

    fn shared_value_position() -> Position {
        Position::new(2, 6)
    }

    fn compute_references(
        fixture: &ReferenceFixture,
    ) -> Computed<crate::coverage::Partial<Vec<lsp_types::Location>>> {
        let cancel = AtomicBool::new(false);
        let computed = references_from_input(
            fixture.input.clone(),
            &source_uri(&fixture.source),
            shared_value_position(),
            true,
            &cancel,
        );
        assert!(
            computed.value.is_ok(),
            "reference computation failed: {computed:?}"
        );
        assert!(
            !computed.records.is_empty(),
            "reference computation must retain observations"
        );
        computed
    }

    #[test]
    fn references_revalidation_observes_an_external_ancestor_project_candidate() {
        let fixture = external_fixture();
        let computed = compute_references(&fixture);
        let ancestor_project = fixture
            .root
            .parent()
            .expect("temporary parent")
            .join("library/App.dproj");
        fs::write(
            &ancestor_project,
            "<Project><PropertyGroup><MainSource>src/External.pas</MainSource></PropertyGroup></Project>",
        )
        .expect("ancestor project");

        let cancel = AtomicBool::new(false);
        let error = revalidate_input(&fixture.input, &computed.records, &cancel).expect_err(
            "adding a project candidate above an external source root must stale references",
        );
        assert!(
            error.contains("membership") || error.contains("changed"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn references_revalidation_ignores_unrelated_external_ancestor_files() {
        let fixture = external_fixture();
        let computed = compute_references(&fixture);
        let unrelated = fixture
            .root
            .parent()
            .expect("temporary parent")
            .join("library/noise.txt");
        fs::write(&unrelated, "not a project candidate").expect("unrelated file");

        let cancel = AtomicBool::new(false);
        revalidate_input(&fixture.input, &computed.records, &cancel)
            .expect("unrelated external ancestor files must not stale references");
    }

    #[test]
    fn owner_preflight_honors_cancellation() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(true);
        let error = owner_for_input(&fixture.input, &source_uri(&fixture.provider), &cancel)
            .expect_err("owner discovery must stop when cancellation is already requested");
        assert_eq!(error, CANCELLATION_MESSAGE);
    }

    #[test]
    fn automatic_context_uses_captured_overrides_after_sidecar_edit() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("project");
        let source = root.join("App.pas");
        let project = root.join("App.dproj");
        let overrides = root.join(".delphi-tools.local.toml");
        fs::create_dir_all(&root).expect("project directory");
        fs::write(&source, "unit App;\ninterface\nimplementation\nend.\n").expect("source");
        fs::write(
            &project,
            "<Project><PropertyGroup><MainSource>App.pas</MainSource></PropertyGroup></Project>",
        )
        .expect("project");
        fs::write(&overrides, "[properties]\nDCC_Define = 'CAPTURED'\n")
            .expect("captured override");

        let workspace = test_workspace(vec![root], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        fs::write(&overrides, "[properties]\nDCC_Define = 'LIVE'\n").expect("edited override");

        let cancel = AtomicBool::new(false);
        let (context, _) =
            project_context_and_metadata_for_input(&input, &source_uri(&source), &cancel)
                .expect("automatic project discovery");
        assert_eq!(context.project_file(), Some(project.as_path()));
        assert_eq!(context.defines(), vec!["CAPTURED"]);
    }

    #[test]
    fn assistance_preflight_propagates_cancellation_during_owner_discovery() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(false);
        let _guard = pascal_project::test_cancel_project_scan_after_checks(0);

        let computed = hover_from_input(
            fixture.input,
            &source_uri(&fixture.provider),
            shared_value_position(),
            MarkupKind::PlainText,
            &cancel,
        );

        assert_eq!(computed.value, Err(CANCELLATION_MESSAGE.to_string()));
        assert!(cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn document_symbols_revalidate_an_external_legacy_optset_payload() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let project_root = temp.path().join("project");
        let shared_root = temp.path().join("shared");
        let source = project_root.join("App.pas");
        let project = project_root.join("App.dproj");
        let settings = shared_root.join("settings.optset");
        fs::create_dir_all(&project_root).expect("project directory");
        fs::create_dir_all(&shared_root).expect("shared directory");
        fs::write(
            &source,
            "unit App;\ninterface\nconst StableValue = 1;\nimplementation\nend.\n",
        )
        .expect("source");
        fs::write(
            &project,
            "<Project><PropertyGroup><MainSource>App.pas</MainSource></PropertyGroup><Import Project=\"../shared/settings.optset\" /></Project>",
        )
        .expect("project");
        fs::write(
            &settings,
            "<Project><PropertyGroup><DCC_Define>EXTERNAL_SETTINGS</DCC_Define></PropertyGroup></Project>",
        )
        .expect("external option set");

        let workspace = test_workspace(vec![project_root], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let computed = document_symbols_from_input(input.clone(), &source_uri(&source), &cancel);
        assert!(
            computed.value.is_ok(),
            "document symbols: {:?}",
            computed.value
        );
        let record = computed
            .records
            .iter()
            .find(|record| record.path.as_deref() == Some(settings.as_path()))
            .expect("external option-set record");
        assert!(
            record.content_bytes.is_some(),
            "payload bytes must be retained"
        );
        assert!(
            record.read_policy.is_some(),
            "payload authorization must be retained"
        );
        assert_eq!(
            record.path_entry.as_ref().map(|entry| &entry.provenance),
            Some(&ProjectPathProvenance::LegacyNative),
        );

        revalidate_input(&input, &computed.records, &cancel)
            .expect("unchanged external option set must revalidate");
        fs::write(
            &settings,
            "<Project><PropertyGroup><DCC_Define>CHANGED_SETTINGS</DCC_Define></PropertyGroup></Project>",
        )
        .expect("changed external option set");
        revalidate_input(&input, &computed.records, &cancel)
            .expect_err("changed external option set must stale document symbols");
    }

    #[cfg(unix)]
    #[test]
    fn references_include_an_unopened_consumer_under_the_requesting_mapping() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("project");
        let sdk = temp.path().join("sdk");
        let main = root.join("Main.pas");
        let provider = sdk.join("Provider.pas");
        let consumer = sdk.join("Consumer.pas");
        fs::create_dir_all(&root).expect("project directory");
        fs::create_dir_all(&sdk).expect("SDK directory");
        fs::write(
            &main,
            "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n",
        )
        .expect("main source");
        fs::write(
            &provider,
            "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n",
        )
        .expect("provider source");
        fs::write(
            &consumer,
            "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n",
        )
        .expect("consumer source");
        fs::write(
            root.join("App.dproj"),
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>C:\\SDK</DCC_UnitSearchPath></PropertyGroup></Project>",
        )
        .expect("project metadata");
        fs::write(
            root.join(".delphi-tools.local.toml"),
            format!(
                "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
                sdk.display()
            ),
        )
        .expect("path mapping");

        let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
        let main_uri = source_uri(&main);
        let provider_uri = source_uri(&provider);
        let declaration = workspace.navigate(
            &main_uri,
            Position::new(6, 6),
            crate::NavigationTarget::Declaration,
        );
        assert_eq!(
            declaration.first().map(|location| &location.uri),
            Some(&provider_uri),
            "the fixture must load the mapped provider before capturing its owner"
        );

        let input = workspace.analysis_input();
        let computed = references_from_input(
            input,
            &provider_uri,
            Position::new(2, 6),
            true,
            &AtomicBool::new(false),
        );
        let locations = computed
            .value
            .expect("mapped references should complete")
            .value;
        assert!(
            locations
                .iter()
                .any(|location| location.uri == source_uri(&consumer)),
            "unopened mapped consumer was not included: {locations:?}"
        );
    }

    struct QueryFixture {
        _temp: TempDir,
        provider: PathBuf,
        consumer: PathBuf,
        provider_source: String,
        consumer_source: String,
        input: WorkspaceInput,
    }

    fn query_fixture() -> QueryFixture {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let provider = temp.path().join("Provider.pas");
        let consumer = temp.path().join("Consumer.pas");
        let provider_source =
            "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n".to_string();
        let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n".to_string();
        fs::create_dir_all(temp.path()).expect("workspace directory");
        fs::write(&provider, &provider_source).expect("provider source");
        fs::write(&consumer, &consumer_source).expect("consumer source");
        let workspace =
            test_workspace(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
        QueryFixture {
            _temp: temp,
            provider,
            consumer,
            provider_source,
            consumer_source,
            input: workspace.analysis_input(),
        }
    }

    /// 500 units: `Shared` declares `SharedValue`; 20 importers use it; the
    /// others declare their own `SharedValue` and never import `Shared`.
    struct ManyUnits {
        _temp: TempDir,
        shared: PathBuf,
        importers: Vec<PathBuf>,
        input: WorkspaceInput,
    }

    fn many_units(importers: usize, others: usize) -> ManyUnits {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let shared = root.join("Shared.pas");
        fs::write(
            &shared,
            "unit Shared;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n",
        )
        .expect("shared");
        let importers = (0..importers)
            .map(|index| {
                let path = root.join(format!("Importer{index:03}.pas"));
                fs::write(
                    &path,
                    format!(
                        "unit Importer{index:03};\ninterface\nuses Shared;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n"
                    ),
                )
                .expect("importer");
                path
            })
            .collect();
        for index in 0..others {
            fs::write(
                root.join(format!("Other{index:03}.pas")),
                format!(
                    "unit Other{index:03};\ninterface\nconst SharedValue = {index};\nimplementation\nend.\n"
                ),
            )
            .expect("other");
        }
        // Settled stamps, so the index proves the excluded units unchanged.
        pascal_project::backdate_tree_for_test(&root);
        let workspace = test_workspace(vec![root], WorkspaceOptions::default());
        ManyUnits {
            _temp: temp,
            shared,
            importers,
            input: workspace.analysis_input(),
        }
    }

    /// Indexes every member of the context that owns `uri`.
    fn build_index_for(input: &WorkspaceInput, uri: &Url) {
        build_index_at(input, &IndexAnchor::Document(uri.clone()));
    }

    fn build_index_at(input: &WorkspaceInput, anchor: &IndexAnchor) {
        let cancel = AtomicBool::new(false);
        let progress = Workspace::from_analysis_input(input)
            .advance_workspace_index(anchor, IndexBound::all(), &mut || Ok(()), &cancel)
            .expect("index batch");
        assert!(progress.complete, "{progress:?}");
    }

    fn located(locations: &[lsp_types::Location]) -> Vec<(Url, u32, u32)> {
        let mut found = locations
            .iter()
            .map(|location| {
                (
                    location.uri.clone(),
                    location.range.start.line,
                    location.range.start.character,
                )
            })
            .collect::<Vec<_>>();
        found.sort();
        found
    }

    fn bound_documents() -> Vec<Url> {
        let mut bound = crate::navigation::test_bound_documents();
        bound.sort();
        bound.dedup();
        bound
    }

    /// l4d-33 acceptance: with a built index, 500 units and one shared symbol
    /// bind only the importers and the declaring unit.
    #[test]
    fn indexed_references_bind_only_the_importing_units() {
        let fixture = many_units(20, 479);
        build_index_for(&fixture.input, &source_uri(&fixture.shared));
        crate::navigation::test_reset_bound_documents();
        let computed = references_from_input(
            fixture.input.clone(),
            &source_uri(&fixture.shared),
            Position::new(2, 6),
            true,
            &AtomicBool::new(false),
        );
        let partial = computed.value.expect("references");
        assert!(partial.coverage.is_complete(), "{:?}", partial.coverage);
        assert_eq!(partial.value.len(), 21);
        let mut bound = crate::navigation::test_bound_documents();
        bound.sort();
        bound.dedup();
        let mut expected = fixture
            .importers
            .iter()
            .map(|p| source_uri(p))
            .collect::<Vec<_>>();
        expected.push(source_uri(&fixture.shared));
        expected.sort();
        assert_eq!(bound, expected);
    }

    #[test]
    fn references_on_an_unindexed_context_search_unverified_members_directly() {
        let fixture = query_fixture();
        crate::workspace::rename::test_reset_workspace_snapshots();
        let computed = references_from_input(
            fixture.input.clone(),
            &source_uri(&fixture.provider),
            shared_value_position(),
            true,
            &AtomicBool::new(false),
        );
        let partial = computed.value.expect("references");
        assert!(partial.coverage.is_complete(), "{:?}", partial.coverage);
        // Recorded on the walk (Task 9 commit e9ae98e).
        assert_eq!(
            located(&partial.value),
            vec![
                (source_uri(&fixture.consumer), 6, 6),
                (source_uri(&fixture.provider), 2, 6),
            ]
        );
        assert_eq!(crate::workspace::rename::test_indexed_snapshots(), 1);
    }

    #[test]
    fn unverified_members_beyond_the_bound_are_a_gap() {
        let fixture = query_fixture();
        let root = fixture.provider.parent().unwrap().to_path_buf();
        fs::write(
            root.join("Another.pas"),
            "unit Another;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n",
        )
        .expect("another consumer");
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::workspace::indexing::test_set_unverified_cap(None);
            }
        }
        let _reset = Reset;
        crate::workspace::indexing::test_set_unverified_cap(Some(1));
        let computed = references_from_input(
            fixture.input.clone(),
            &source_uri(&fixture.provider),
            shared_value_position(),
            true,
            &AtomicBool::new(false),
        );
        let partial = computed.value.expect("a partial result, not a refusal");
        assert!(!partial.coverage.is_complete(), "{:?}", partial.coverage);
        assert!(
            partial
                .coverage
                .gaps
                .iter()
                .any(|gap| gap.reason.contains("not indexed yet")),
            "{:?}",
            partial.coverage
        );
        // Another.pas is first in path order; Provider.pas is the declaring
        // document; Consumer.pas is beyond the bound.
        assert_eq!(
            located(&partial.value),
            vec![
                (source_uri(&root.join("Another.pas")), 6, 6),
                (source_uri(&fixture.provider), 2, 6),
            ]
        );
    }

    /// l4d-jeh4: 300 unverified units that never mention the name sort
    /// before the 20 importers that do. References read at most the
    /// interactive bound directly, the members that spell the name first,
    /// and report the rest as a gap; rename keeps the complete walk.
    #[test]
    fn references_read_a_bounded_set_of_unverified_members_likely_ones_first() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let shared = root.join("Shared.pas");
        fs::write(
            &shared,
            "unit Shared;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n",
        )
        .expect("shared");
        for index in 0..300 {
            fs::write(
                root.join(format!("Alpha{index:03}.pas")),
                format!(
                    "unit Alpha{index:03};\ninterface\nuses Shared;\nconst OtherValue = {index};\nimplementation\nend.\n"
                ),
            )
            .expect("alpha");
        }
        let importers = (0..20)
            .map(|index| {
                let path = root.join(format!("Zed{index:03}.pas"));
                fs::write(
                    &path,
                    format!(
                        "unit Zed{index:03};\ninterface\nuses Shared;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n"
                    ),
                )
                .expect("importer");
                path
            })
            .collect::<Vec<_>>();
        let input =
            test_workspace(vec![root.clone()], WorkspaceOptions::default()).analysis_input();
        crate::workspace::rename::test_reset_workspace_snapshots();
        crate::navigation::test_reset_bound_documents();
        let computed = references_from_input(
            input.clone(),
            &source_uri(&shared),
            Position::new(2, 6),
            true,
            &AtomicBool::new(false),
        );
        let partial = computed.value.expect("a partial result, not a refusal");
        assert_eq!(crate::workspace::rename::test_indexed_snapshots(), 1);
        let bound = crate::workspace::indexing::MAX_INTERACTIVE_UNVERIFIED_CANDIDATES;
        assert!(
            partial.coverage.gaps.iter().any(|gap| gap.reason
                == format!(
                    "{} units of {} are not indexed yet; {bound} were searched directly",
                    321 - bound,
                    shared.display()
                )),
            "{:?}",
            partial.coverage
        );
        let mut expected = importers
            .iter()
            .map(|path| (source_uri(path), 6, 6))
            .chain([(source_uri(&shared), 2, 6)])
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(located(&partial.value), expected);

        crate::workspace::rename::test_reset_workspace_snapshots();
        let rename = crate::workspace::rename::rename_from_input(
            input,
            &source_uri(&shared),
            Position::new(2, 6),
            "Renamed",
            false,
            &AtomicBool::new(false),
        );
        assert_eq!(crate::workspace::rename::test_indexed_snapshots(), 0);
        let edit = rename.value.expect("rename walks the whole workspace");
        let changes = edit.changes.expect("plain changes");
        assert_eq!(changes.len(), 21, "{:?}", changes.keys());
    }

    /// l4d-jeh4: a candidate unit whose include expansion hits a resolver
    /// limit is a coverage gap of the references answer, not its failure;
    /// rename still refuses.
    #[test]
    fn a_candidate_whose_expansion_fails_is_a_reference_gap() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let shared = root.join("Shared.pas");
        fs::write(
            &shared,
            "unit Shared;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n",
        )
        .expect("shared");
        let failing = root.join("Failing.pas");
        fs::write(
            &failing,
            "unit Failing;\ninterface\nuses Shared;\nimplementation\n{$I Extra.inc}\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n",
        )
        .expect("failing");
        fs::write(root.join("Extra.inc"), "const ExtraValue = 2;\n").expect("include");
        let other = root.join("Other.pas");
        fs::write(
            &other,
            "unit Other;\ninterface\nuses Shared;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n",
        )
        .expect("other");
        let downstream = root.join("Downstream.pas");
        fs::write(
            &downstream,
            "unit Downstream;\ninterface\nuses Shared, Failing;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n",
        )
        .expect("downstream");
        let input =
            test_workspace(vec![root.clone()], WorkspaceOptions::default()).analysis_input();
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::workspace::compile_time::test_fail_expansion_of(None);
            }
        }
        let _reset = Reset;
        crate::workspace::compile_time::test_fail_expansion_of(Some("/Failing.pas"));
        crate::workspace::rename::test_reset_workspace_snapshots();
        let computed = references_from_input(
            input.clone(),
            &source_uri(&shared),
            Position::new(2, 6),
            true,
            &AtomicBool::new(false),
        );
        let partial = computed.value.expect("a partial result, not a refusal");
        assert_eq!(crate::workspace::rename::test_indexed_snapshots(), 1);
        assert!(
            partial
                .coverage
                .gaps
                .iter()
                .any(|gap| gap.uri.as_ref() == Some(&source_uri(&failing))
                    && gap.reason.contains("resolver limit")),
            "{:?}",
            partial.coverage
        );
        // Failing.pas may declare what Downstream.pas binds.
        assert!(
            partial
                .coverage
                .gaps
                .iter()
                .any(|gap| gap.uri.as_ref() == Some(&source_uri(&downstream))),
            "{:?}",
            partial.coverage
        );
        assert_eq!(
            located(&partial.value),
            vec![(source_uri(&other), 6, 6), (source_uri(&shared), 2, 6)]
        );
        // A provider of the requesting document keeps the strict proof.
        let refused = references_from_input(
            input.clone(),
            &source_uri(&downstream),
            Position::new(6, 6),
            true,
            &AtomicBool::new(false),
        );
        assert!(refused.value.is_err(), "{:?}", refused.value);
        let rename = crate::workspace::rename::rename_from_input(
            input,
            &source_uri(&shared),
            Position::new(2, 6),
            "Renamed",
            false,
            &AtomicBool::new(false),
        );
        assert!(rename.value.is_err(), "{:?}", rename.value);
    }

    /// l4d-jeh4 fix round 1: without the index, references answer from the
    /// best-effort local snapshot; a provider whose analysis failed there
    /// is a dependency gap that withholds the requesting document too.
    #[test]
    fn the_index_unavailable_fallback_withholds_the_importers_of_a_dependency_gap() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let shared = root.join("Shared.pas");
        fs::write(
            &shared,
            "unit Shared;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n",
        )
        .expect("shared");
        fs::write(
            root.join("Failing.pas"),
            "unit Failing;\ninterface\n{$I Missing.inc}\nimplementation\nend.\n",
        )
        .expect("failing");
        let downstream = root.join("Downstream.pas");
        fs::write(
            &downstream,
            "unit Downstream;\ninterface\nuses Shared, Failing;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n",
        )
        .expect("downstream");
        let input =
            test_workspace(vec![root.clone()], WorkspaceOptions::default()).analysis_input();
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::workspace::indexing::test_set_target_error(false);
                crate::workspace::indexing::test_set_small_workspace_sources(None);
            }
        }
        let _reset = Reset;
        crate::workspace::indexing::test_set_target_error(true);
        crate::workspace::indexing::test_set_small_workspace_sources(Some(0));
        let computed = references_from_input(
            input,
            &source_uri(&downstream),
            Position::new(6, 6),
            true,
            &AtomicBool::new(false),
        );
        let partial = computed.value.expect("a partial result, not a refusal");
        assert!(
            partial
                .coverage
                .gaps
                .iter()
                .any(|gap| gap.reason == super::INDEX_UNAVAILABLE_GAP),
            "{:?}",
            partial.coverage
        );
        assert!(
            partial
                .coverage
                .gaps
                .iter()
                .any(|gap| gap.uri.as_ref() == Some(&source_uri(&downstream))),
            "{:?}",
            partial.coverage
        );
        assert!(
            !located(&partial.value)
                .iter()
                .any(|(uri, _, _)| *uri == source_uri(&downstream)),
            "{:?}",
            partial.value
        );
    }

    /// l4d-jeh4 fix round 1: a provider of the requesting document whose
    /// include is unresolved (a recorded audit failure) may declare a
    /// shadowing name, so references from that document refuse.
    #[test]
    fn a_provider_with_an_unresolved_include_refuses_references() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        fs::write(
            root.join("Shared.pas"),
            "unit Shared;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n",
        )
        .expect("shared");
        fs::write(
            root.join("Failing.pas"),
            "unit Failing;\ninterface\n{$I Missing.inc}\nimplementation\nend.\n",
        )
        .expect("failing");
        let downstream = root.join("Downstream.pas");
        fs::write(
            &downstream,
            "unit Downstream;\ninterface\nuses Shared, Failing;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n",
        )
        .expect("downstream");
        let input =
            test_workspace(vec![root.clone()], WorkspaceOptions::default()).analysis_input();
        let computed = references_from_input(
            input,
            &source_uri(&downstream),
            Position::new(6, 6),
            true,
            &AtomicBool::new(false),
        );
        assert!(computed.value.is_err(), "{:?}", computed.value);
    }

    #[test]
    fn refused_members_are_a_gap_not_a_candidate() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let provider = root.join("Provider.pas");
        fs::write(
            &provider,
            "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n",
        )
        .expect("provider");
        let big = root.join("Big.pas");
        fs::write(
            &big,
            format!(
                "unit Big;\ninterface\nuses Provider;\n{}implementation\nend.\n",
                "{ SharedValue }\n".repeat(60)
            ),
        )
        .expect("big");
        pascal_project::backdate_tree_for_test(&root);
        let mut options = WorkspaceOptions::default();
        options.limits.max_file_bytes = 300;
        let input = test_workspace(vec![root], options).analysis_input();
        build_index_for(&input, &source_uri(&provider));
        crate::navigation::test_reset_bound_documents();
        let computed = references_from_input(
            input,
            &source_uri(&provider),
            shared_value_position(),
            true,
            &AtomicBool::new(false),
        );
        let partial = computed.value.expect("a partial result, not a refusal");
        assert!(
            partial
                .coverage
                .gaps
                .iter()
                .any(|gap| gap.reason.starts_with("1 units could not be indexed: ")),
            "{:?}",
            partial.coverage
        );
        assert_eq!(located(&partial.value), vec![(source_uri(&provider), 2, 6)]);
        assert!(!bound_documents().contains(&source_uri(&big)));
    }

    #[test]
    fn references_search_every_target_context_sharing_the_declaring_unit() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = crate::workspace::absolute_path(temp.path().to_path_buf());
        let shared = root.join("shared/Shared.pas");
        fs::create_dir_all(shared.parent().unwrap()).expect("shared directory");
        fs::write(
            &shared,
            "unit Shared;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n",
        )
        .expect("shared");
        for name in ["P1", "P2"] {
            let directory = root.join(name);
            fs::create_dir_all(&directory).expect("project directory");
            fs::write(
                directory.join(format!("{name}.dproj")),
                format!(
                    "<Project><PropertyGroup><MainSource>{name}.dpr</MainSource><DCC_UnitSearchPath>..\\shared</DCC_UnitSearchPath></PropertyGroup></Project>"
                ),
            )
            .expect("descriptor");
            fs::write(
                directory.join(format!("{name}.dpr")),
                format!("program {name};\nuses C{name};\nbegin\nend.\n"),
            )
            .expect("main source");
            fs::write(
                directory.join(format!("C{name}.pas")),
                format!(
                    "unit C{name};\ninterface\nuses Shared;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n"
                ),
            )
            .expect("consumer");
        }
        pascal_project::backdate_tree_for_test(&root);
        let mut workspace = test_workspace(
            vec![root.clone()],
            WorkspaceOptions {
                project_file: Some(root.join("P2/P2.dproj")),
                ..WorkspaceOptions::default()
            },
        );
        let opened = root.join("P1/CP1.pas");
        workspace
            .open_document(
                source_uri(&opened),
                fs::read_to_string(&opened).expect("consumer text"),
                1,
            )
            .expect("open P1 consumer");
        let input = workspace.analysis_input();
        for name in ["P1", "P2"] {
            build_index_at(
                &input,
                &IndexAnchor::Project(root.join(name).join(format!("{name}.dproj"))),
            );
        }
        crate::workspace::rename::test_reset_workspace_snapshots();
        let computed = references_from_input(
            input,
            &source_uri(&shared),
            shared_value_position(),
            true,
            &AtomicBool::new(false),
        );
        let partial = computed.value.expect("references");
        assert_eq!(
            located(&partial.value),
            vec![
                (source_uri(&root.join("P1/CP1.pas")), 6, 6),
                (source_uri(&root.join("P2/CP2.pas")), 6, 6),
                (source_uri(&shared), 2, 6),
            ],
            "{:?}",
            partial.coverage
        );
        assert_eq!(crate::workspace::rename::test_indexed_snapshots(), 1);
    }

    #[test]
    fn phase_one_failure_falls_back_to_the_member_rule() {
        let fixture = many_units(2, 2);
        let root = fixture.shared.parent().unwrap().to_path_buf();
        let plain = root.join("Plain.pas");
        fs::write(&plain, "unit Plain;\ninterface\nimplementation\nend.\n").expect("plain");
        pascal_project::backdate_tree_for_test(&root);
        build_index_for(&fixture.input, &source_uri(&fixture.shared));
        let references = || {
            references_from_input(
                fixture.input.clone(),
                &source_uri(&fixture.shared),
                Position::new(2, 6),
                true,
                &AtomicBool::new(false),
            )
            .value
            .expect("references")
        };
        let scoped = references();
        crate::navigation::test_reset_bound_documents();
        let failing = crate::workspace::rename::test_fail_reference_targets();
        let fallback = references();
        drop(failing);
        assert_eq!(located(&fallback.value), located(&scoped.value));
        assert!(fallback.coverage.is_complete(), "{:?}", fallback.coverage);
        let mut expected = fixture
            .importers
            .iter()
            .map(|path| source_uri(path))
            .chain([
                source_uri(&fixture.shared),
                source_uri(&root.join("Other000.pas")),
                source_uri(&root.join("Other001.pas")),
            ])
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(bound_documents(), expected);
    }

    /// l4d-vbeh soundness: a unit whose own text never spells the name but
    /// whose include does is still walked, and the skip changes nothing.
    #[test]
    fn an_occurrence_only_through_an_include_survives_the_occurrence_prefilter() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let main = root.join("Main.pas");
        fs::write(
            &main,
            "unit Main;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n",
        )
        .expect("main");
        let include = root.join("Body.inc");
        fs::write(&include, "  Log(SHAREDVALUE);\n").expect("include");
        fs::write(
            root.join("Consumer.pas"),
            "unit Consumer;\ninterface\nuses Main;\nimplementation\nprocedure Run;\nbegin\n{$I Body.inc}\nend;\nend.\n",
        )
        .expect("consumer");
        let input =
            test_workspace(vec![root.clone()], WorkspaceOptions::default()).analysis_input();
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::navigation::test_set_occurrence_prefilter(true);
            }
        }
        let _reset = Reset;
        let mut results = Vec::new();
        for prefilter in [true, false] {
            crate::navigation::test_set_occurrence_prefilter(prefilter);
            crate::navigation::test_reset_bound_documents();
            let computed = references_from_input(
                input.clone(),
                &source_uri(&main),
                Position::new(2, 6),
                true,
                &AtomicBool::new(false),
            );
            let partial = computed.value.expect("references");
            assert!(partial.coverage.is_complete(), "{:?}", partial.coverage);
            results.push((located(&partial.value), bound_documents()));
        }
        assert_eq!(results[0].0, results[1].0);
        assert_eq!(
            results[0].0,
            vec![(source_uri(&include), 0, 6), (source_uri(&main), 2, 6)]
        );
        let consumer = source_uri(&root.join("Consumer.pas"));
        assert!(results[0].1.contains(&consumer), "{:?}", results[0].1);
    }

    #[test]
    fn an_include_document_keeps_the_old_path() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        fs::write(
            root.join("Main.pas"),
            "unit Main;\ninterface\n{$I Values.inc}\nimplementation\nend.\n",
        )
        .expect("main");
        let include = root.join("Values.inc");
        fs::write(&include, "const SharedValue = 1;\n").expect("include");
        fs::write(
            root.join("Consumer.pas"),
            "unit Consumer;\ninterface\nuses Main;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n",
        )
        .expect("consumer");
        let input =
            test_workspace(vec![root.clone()], WorkspaceOptions::default()).analysis_input();
        crate::workspace::rename::test_reset_workspace_snapshots();
        let computed = references_from_input(
            input,
            &source_uri(&include),
            Position::new(0, 6),
            true,
            &AtomicBool::new(false),
        );
        let partial = computed.value.expect("references");
        assert_eq!(crate::workspace::rename::test_workspace_snapshots(), 1);
        assert_eq!(crate::workspace::rename::test_indexed_snapshots(), 0);
        assert!(
            located(&partial.value).contains(&(source_uri(&root.join("Consumer.pas")), 6, 6)),
            "{:?}",
            partial.value
        );
    }

    #[test]
    fn a_local_binding_keeps_the_old_path() {
        let fixture = query_fixture();
        crate::workspace::rename::test_reset_workspace_snapshots();
        let local = fixture.provider.parent().unwrap().join("Local.pas");
        fs::write(
            &local,
            "unit Local;\ninterface\nimplementation\nprocedure Run;\nvar\n  Count: Integer;\nbegin\n  Count := 1;\nend;\nend.\n",
        )
        .expect("local");
        let computed = references_from_input(
            fixture.input.clone(),
            &source_uri(&local),
            Position::new(5, 2),
            true,
            &AtomicBool::new(false),
        );
        let partial = computed.value.expect("references");
        assert_eq!(partial.value.len(), 2, "{:?}", partial.value);
        assert_eq!(crate::workspace::rename::test_indexed_snapshots(), 0);
    }

    #[test]
    fn a_delivered_result_is_rejected_when_an_excluded_member_changes() {
        let fixture = query_fixture();
        let root = fixture.provider.parent().unwrap().to_path_buf();
        let plain = root.join("Plain.pas");
        fs::write(&plain, "unit Plain;\ninterface\nimplementation\nend.\n").expect("plain");
        pascal_project::backdate_tree_for_test(&root);
        build_index_for(&fixture.input, &source_uri(&fixture.provider));
        let cancel = AtomicBool::new(false);
        crate::navigation::test_reset_bound_documents();
        let computed = references_from_input(
            fixture.input.clone(),
            &source_uri(&fixture.provider),
            shared_value_position(),
            true,
            &cancel,
        );
        assert!(computed.value.is_ok(), "{computed:?}");
        assert!(
            !bound_documents().contains(&source_uri(&plain)),
            "Plain.pas is excluded"
        );
        revalidate_input(&fixture.input, &computed.records, &cancel)
            .expect("unchanged reference inputs must remain valid");

        fs::write(
            &plain,
            "unit Plain;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n",
        )
        .expect("changed excluded member");
        revalidate_input(&fixture.input, &computed.records, &cancel)
            .expect_err("a changed excluded member must stale delivered references");
    }

    #[test]
    fn references_do_not_walk_the_workspace_once_indexed() {
        let fixture = query_fixture();
        let root = fixture.provider.parent().unwrap().to_path_buf();
        pascal_project::backdate_tree_for_test(&root);
        build_index_for(&fixture.input, &source_uri(&fixture.provider));
        let cancel = AtomicBool::new(false);
        let references = || {
            references_from_input(
                fixture.input.clone(),
                &source_uri(&fixture.provider),
                shared_value_position(),
                true,
                &cancel,
            )
        };
        let first = references();
        assert!(first.value.is_ok(), "{first:?}");
        crate::catalogue::test_reset_listings();
        crate::workspace::rename::test_reset_workspace_snapshots();
        let second = references();
        let partial = second.value.expect("second references");
        assert_eq!(
            located(&partial.value),
            vec![
                (source_uri(&fixture.consumer), 6, 6),
                (source_uri(&fixture.provider), 2, 6),
            ]
        );
        assert!(
            crate::catalogue::test_listings().is_empty(),
            "{:?}",
            crate::catalogue::test_listings()
        );
        assert_eq!(crate::workspace::rename::test_workspace_snapshots(), 1);
        assert_eq!(crate::workspace::rename::test_indexed_snapshots(), 1);
    }

    /// The indexed scope does not walk mapped roots (l4d-33 §6.1): a mapped
    /// search-path member the index excludes is not bound.
    #[cfg(unix)]
    #[test]
    fn indexed_references_bind_only_index_candidates_under_a_mapped_search_path() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = crate::workspace::absolute_path(temp.path().join("project"));
        let sdk = crate::workspace::absolute_path(temp.path().join("sdk"));
        fs::create_dir_all(&root).expect("project directory");
        fs::create_dir_all(&sdk).expect("SDK directory");
        let main = root.join("Main.pas");
        fs::write(
            &main,
            "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n",
        )
        .expect("main");
        let provider = sdk.join("Provider.pas");
        fs::write(
            &provider,
            "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n",
        )
        .expect("provider");
        let consumer = sdk.join("Consumer.pas");
        fs::write(
            &consumer,
            "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n",
        )
        .expect("consumer");
        // A posting for the name, but no import of Provider: excluded.
        let other = sdk.join("Other.pas");
        fs::write(
            &other,
            "unit Other;\ninterface\nconst SharedValue = 2;\nimplementation\nend.\n",
        )
        .expect("other");
        fs::write(
            root.join("App.dproj"),
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>C:\\SDK</DCC_UnitSearchPath></PropertyGroup></Project>",
        )
        .expect("project metadata");
        fs::write(
            root.join(".delphi-tools.local.toml"),
            format!(
                "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
                sdk.display()
            ),
        )
        .expect("path mapping");
        pascal_project::backdate_tree_for_test(temp.path());
        let input =
            test_workspace(vec![root.clone()], WorkspaceOptions::default()).analysis_input();
        build_index_at(&input, &IndexAnchor::Project(root.join("App.dproj")));
        crate::navigation::test_reset_bound_documents();
        let computed = references_from_input(
            input,
            &source_uri(&main),
            Position::new(6, 6),
            true,
            &AtomicBool::new(false),
        );
        let partial = computed.value.expect("references");
        assert_eq!(
            located(&partial.value),
            vec![
                (source_uri(&root.join("Main.pas")), 6, 6),
                (source_uri(&consumer), 6, 6),
                (source_uri(&provider), 2, 6),
            ],
            "{:?}",
            partial.coverage
        );
        let bound = bound_documents();
        assert!(bound.contains(&source_uri(&consumer)), "{bound:?}");
        assert!(!bound.contains(&source_uri(&other)), "{bound:?}");
    }

    /// Members of another target context outside the workspace roots are a
    /// gap, not candidates (they would not be read by the walk either).
    #[cfg(unix)]
    #[test]
    fn other_contexts_members_outside_the_roots_are_a_gap() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = crate::workspace::absolute_path(temp.path().join("workspace"));
        let sdk = crate::workspace::absolute_path(temp.path().join("sdk"));
        let project_a = root.join("A");
        let project_b = root.join("B");
        for directory in [&project_a, &project_b, &sdk] {
            fs::create_dir_all(directory).expect("directory");
        }
        let provider = project_a.join("Provider.pas");
        fs::write(
            &provider,
            "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n",
        )
        .expect("provider");
        fs::write(
            project_a.join("Main.pas"),
            "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n",
        )
        .expect("main A");
        fs::write(
            project_a.join("App.dproj"),
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
        )
        .expect("project A");
        let consumer = sdk.join("Consumer.pas");
        fs::write(
            &consumer,
            "unit Consumer;\ninterface\nuses Provider;\nimplementation\nprocedure Consume;\nbegin\n  Log(SharedValue);\nend;\nend.\n",
        )
        .expect("consumer");
        fs::write(
            project_b.join("Main.pas"),
            "unit Main; interface implementation end.\n",
        )
        .expect("main B");
        fs::write(
            project_b.join("App.dproj"),
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>C:\\SDK</DCC_UnitSearchPath></PropertyGroup><ItemGroup><DCCReference Include=\"..\\A\\Provider.pas\" /></ItemGroup></Project>",
        )
        .expect("project B");
        fs::write(
            project_b.join(".delphi-tools.local.toml"),
            format!(
                "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
                sdk.display()
            ),
        )
        .expect("path mapping");
        pascal_project::backdate_tree_for_test(temp.path());
        let input =
            test_workspace(vec![root.clone()], WorkspaceOptions::default()).analysis_input();
        crate::navigation::test_reset_bound_documents();
        let computed = references_from_input(
            input,
            &source_uri(&provider),
            shared_value_position(),
            true,
            &AtomicBool::new(false),
        );
        let partial = computed.value.expect("references");
        assert_eq!(
            located(&partial.value),
            vec![
                (source_uri(&project_a.join("Main.pas")), 6, 6),
                (source_uri(&provider), 2, 6),
            ],
            "{:?}",
            partial.coverage
        );
        assert!(
            partial.coverage.gaps.iter().any(|gap| gap.reason
                == "1 units of App.dproj outside the workspace roots were not searched"),
            "{:?}",
            partial.coverage
        );
        assert!(!bound_documents().contains(&source_uri(&consumer)));
    }

    #[test]
    fn member_references_reach_units_that_never_import_the_declaring_unit() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let base = root.join("Base.pas");
        fs::write(
            &base,
            "unit Base;\ninterface\ntype\n  TBase = class\n    Field: Integer;\n  end;\nimplementation\nend.\n",
        )
        .expect("base");
        fs::write(
            root.join("Mid.pas"),
            "unit Mid;\ninterface\nuses Base;\nvar\n  Shared: TBase;\nfunction MakeBase: TBase;\nimplementation\nfunction MakeBase: TBase;\nbegin\n  Result := Shared;\nend;\nend.\n",
        )
        .expect("mid");
        fs::write(
            root.join("User.pas"),
            "unit User;\ninterface\nuses Mid;\nimplementation\nprocedure Run;\nbegin\n  Shared.Field := 1;\n  MakeBase.Field := 2;\nend;\nend.\n",
        )
        .expect("user");
        pascal_project::backdate_tree_for_test(&root);
        let workspace = test_workspace(vec![root], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let walked = references_from_input(
            input.clone(),
            &source_uri(&base),
            Position::new(4, 4),
            true,
            &AtomicBool::new(false),
        )
        .value
        .expect("references before indexing")
        .value;
        build_index_for(&input, &source_uri(&base));
        crate::navigation::test_reset_bound_documents();
        let computed = references_from_input(
            input,
            &source_uri(&base),
            Position::new(4, 4),
            true,
            &AtomicBool::new(false),
        );
        let locations = computed.value.expect("references").value;
        // The member rule took effect: User.pas never imports Base but has
        // a posting for `Field`, so it is bound.
        assert!(
            bound_documents().contains(&source_uri(&temp.path().join("User.pas"))),
            "{:?}",
            bound_documents()
        );
        // Characterization: neither `Shared.Field` nor `MakeBase.Field` in
        // `User.pas` (which never imports `Base`) is found today, nor are
        // inherited or `with` receivers; only the declaration is reported.
        // With a built index the result stays equal to the walk's.
        assert_eq!(
            located(&locations),
            vec![(source_uri(&base), 4, 4)],
            "{locations:?}"
        );
        assert_eq!(located(&locations), located(&walked));
    }

    fn symbols(
        input: &WorkspaceInput,
        query: &str,
    ) -> crate::coverage::Partial<Vec<lsp_types::SymbolInformation>> {
        workspace_symbols_from_input(input.clone(), query, &AtomicBool::new(false))
            .value
            .expect("workspace symbols")
    }

    fn symbol_names(found: &[lsp_types::SymbolInformation]) -> Vec<(String, String)> {
        found
            .iter()
            .map(|symbol| {
                let file = symbol
                    .location
                    .uri
                    .path()
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                (symbol.name.clone(), file)
            })
            .collect()
    }

    /// A workspace of `files` under a fresh root with settled stamps, so the
    /// index proves its members unchanged.
    fn symbol_workspace(files: &[(&str, String)]) -> (TempDir, PathBuf, Workspace) {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        for (name, text) in files {
            fs::write(root.join(name), text).expect("source");
        }
        pascal_project::backdate_tree_for_test(&root);
        let workspace = test_workspace(vec![root.clone()], WorkspaceOptions::default());
        (temp, root, workspace)
    }

    fn open_source(workspace: &mut Workspace, path: &Path) -> Url {
        let uri = source_uri(path);
        workspace
            .open_document(uri.clone(), fs::read_to_string(path).expect("text"), 1)
            .expect("open source");
        uri
    }

    fn reset_symbol_counters() {
        crate::workspace::rename::test_reset_workspace_snapshots();
        super::test_take_fact_symbols();
    }

    /// No whole-workspace walk: every snapshot built read index candidates.
    fn walks() -> usize {
        crate::workspace::rename::test_workspace_snapshots()
            - crate::workspace::rename::test_indexed_snapshots()
    }

    fn unit_text(name: &str, declarations: &str) -> String {
        format!("unit {name};\ninterface\n{declarations}implementation\nend.\n")
    }

    #[test]
    fn workspace_symbols_answer_from_the_index_without_a_snapshot() {
        let fixture = query_fixture();
        let root = fixture.provider.parent().unwrap().to_path_buf();
        pascal_project::backdate_tree_for_test(&root);
        let walked = symbols(&fixture.input, "Shared").value;
        assert!(!walked.is_empty(), "the query must find SharedValue");

        let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
        open_source(&mut workspace, &fixture.consumer);
        let input = workspace.analysis_input();
        build_index_for(&input, &source_uri(&fixture.provider));
        reset_symbol_counters();
        let first = symbols(&input, "Shared");
        let second = symbols(&input, "Shared");
        assert!(first.coverage.is_complete(), "{:?}", first.coverage);
        // No A14 difference applies: SharedValue is a unit-scope constant
        // of slot 0, so the index answer equals the walk's.
        assert_eq!(first.value, walked);
        assert_eq!(first.value, second.value);
        // SharedValue from the facts, once per request.
        assert_eq!(super::test_take_fact_symbols(), 2);
        // The open document is answered from its parse by an indexed
        // snapshot; nothing walks the workspace.
        assert_eq!(walks(), 0);
    }

    #[test]
    fn workspace_symbols_in_an_include_slot_report_the_include_file() {
        let (_temp, root, mut workspace) = symbol_workspace(&[
            (
                "Provider.pas",
                "unit Provider;\ninterface\n{$I Decls.inc}\nimplementation\nend.\n".to_owned(),
            ),
            ("Decls.inc", "const\n  IncludedValue = 2;\n".to_owned()),
            ("Consumer.pas", unit_text("Consumer", "")),
        ]);
        open_source(&mut workspace, &root.join("Consumer.pas"));
        let input = workspace.analysis_input();
        build_index_for(&input, &source_uri(&root.join("Provider.pas")));
        reset_symbol_counters();
        let found = symbols(&input, "IncludedValue");
        assert_eq!(found.value.len(), 1, "{:?}", found.value);
        let symbol = &found.value[0];
        assert_eq!(symbol.location.uri, source_uri(&root.join("Decls.inc")));
        assert_eq!(symbol.location.range.start, Position::new(1, 2));
        assert_eq!(symbol.container_name.as_deref(), Some("Provider"));
        assert_eq!(super::test_take_fact_symbols(), 1);
    }

    #[test]
    fn nested_routines_of_closed_files_are_not_listed_but_open_ones_are() {
        let provider_text = "unit Provider;\ninterface\nimplementation\nprocedure Outer;\n  procedure NestedHelper;\n  begin\n  end;\nbegin\nend;\nend.\n";
        let (_temp, root, mut workspace) = symbol_workspace(&[
            ("Provider.pas", provider_text.to_owned()),
            ("Consumer.pas", unit_text("Consumer", "")),
        ]);
        let provider = root.join("Provider.pas");
        open_source(&mut workspace, &root.join("Consumer.pas"));
        let input = workspace.analysis_input();
        build_index_for(&input, &source_uri(&provider));
        let closed = symbols(&input, "NestedHelper");
        assert!(closed.value.is_empty(), "{:?}", closed.value);

        open_source(&mut workspace, &provider);
        let opened = symbols(&workspace.analysis_input(), "NestedHelper");
        assert_eq!(
            symbol_names(&opened.value),
            [("NestedHelper".to_owned(), "Provider.pas".to_owned())]
        );
    }

    #[test]
    fn workspace_symbols_on_a_partial_index_report_coverage() {
        let mut files = (1..=5)
            .map(|n| {
                (
                    ["A1.pas", "A2.pas", "A3.pas", "A4.pas", "A5.pas"][n - 1],
                    unit_text(&format!("A{n}"), &format!("const PartialValue{n} = {n};\n")),
                )
            })
            .collect::<Vec<_>>();
        files.push(("Opened.pas", unit_text("Opened", "")));
        let (_temp, root, mut workspace) = symbol_workspace(&files);
        open_source(&mut workspace, &root.join("Opened.pas"));
        let input = workspace.analysis_input();
        Workspace::from_analysis_input(&input)
            .advance_workspace_index(
                &IndexAnchor::Document(source_uri(&root.join("A1.pas"))),
                IndexBound::units(2),
                &mut || Ok(()),
                &AtomicBool::new(false),
            )
            .expect("index batch");
        reset_symbol_counters();
        let found = symbols(&input, "PartialValue");
        let expected = (1..=5)
            .map(|n| (format!("PartialValue{n}"), format!("A{n}.pas")))
            .collect::<Vec<_>>();
        assert_eq!(symbol_names(&found.value), expected);
        assert!(found.coverage.is_complete(), "{:?}", found.coverage);
        assert_eq!(super::test_take_fact_symbols(), 2);

        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::workspace::indexing::test_set_unverified_symbol_cap(None);
            }
        }
        let _reset = Reset;
        crate::workspace::indexing::test_set_unverified_symbol_cap(Some(1));
        let bounded = symbols(&input, "PartialValue");
        assert_eq!(symbol_names(&bounded.value), expected[..3]);
        assert!(
            bounded
                .coverage
                .gaps
                .iter()
                .any(|gap| gap.reason.starts_with("2 units of ")
                    && gap
                        .reason
                        .contains("are not indexed yet; 1 were searched directly")),
            "{:?}",
            bounded.coverage
        );
    }

    #[test]
    fn a_small_workspace_without_a_project_keeps_todays_answer() {
        let fixture = query_fixture();
        crate::workspace::rename::test_reset_workspace_snapshots();
        let found = symbols(&fixture.input, "Shared");
        assert!(!found.value.is_empty());
        assert_eq!(crate::workspace::rename::test_workspace_snapshots(), 1);
        assert_eq!(crate::workspace::rename::test_indexed_snapshots(), 0);
    }

    #[test]
    fn a_large_workspace_without_a_project_reports_no_context() {
        let fixture = query_fixture();
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::workspace::indexing::test_set_small_workspace_sources(None);
            }
        }
        let _reset = Reset;
        crate::workspace::indexing::test_set_small_workspace_sources(Some(1));
        crate::workspace::rename::test_reset_workspace_snapshots();
        let found = symbols(&fixture.input, "Shared");
        assert!(found.value.is_empty(), "{:?}", found.value);
        assert!(
            found
                .coverage
                .gaps
                .iter()
                .any(|gap| gap.reason
                    == "no project context is open; open a source or select a project"),
            "{:?}",
            found.coverage
        );
        assert_eq!(crate::workspace::rename::test_workspace_snapshots(), 0);
    }

    /// Index targets fail and the small-workspace source cap is `small`
    /// until dropped.
    struct TargetFailure;

    impl TargetFailure {
        fn new(small: Option<usize>) -> Self {
            crate::workspace::indexing::test_set_target_error(true);
            crate::workspace::indexing::test_set_small_workspace_sources(small);
            Self
        }
    }

    impl Drop for TargetFailure {
        fn drop(&mut self) {
            crate::workspace::indexing::test_set_target_error(false);
            crate::workspace::indexing::test_set_small_workspace_sources(None);
        }
    }

    const INDEX_UNAVAILABLE: &str =
        "the workspace index is unavailable; only the current context's imports were searched";

    #[test]
    fn references_without_the_index_on_a_large_workspace_search_only_the_imports() {
        let fixture = query_fixture();
        let _failure = TargetFailure::new(Some(1));
        crate::workspace::rename::test_reset_workspace_snapshots();
        let computed = references_from_input(
            fixture.input.clone(),
            &source_uri(&fixture.consumer),
            Position::new(6, 6),
            true,
            &AtomicBool::new(false),
        );
        let partial = computed.value.expect("partial references");
        assert_eq!(
            located(&partial.value),
            vec![
                (source_uri(&fixture.consumer), 6, 6),
                (source_uri(&fixture.provider), 2, 6),
            ]
        );
        assert!(
            partial
                .coverage
                .gaps
                .iter()
                .any(|gap| gap.reason == INDEX_UNAVAILABLE),
            "{:?}",
            partial.coverage
        );
        assert_eq!(crate::workspace::rename::test_workspace_snapshots(), 0);
        assert_eq!(crate::workspace::rename::test_indexed_snapshots(), 0);
    }

    #[test]
    fn references_without_the_index_on_a_small_workspace_walk() {
        let fixture = query_fixture();
        let _failure = TargetFailure::new(None);
        crate::workspace::rename::test_reset_workspace_snapshots();
        let computed = references_from_input(
            fixture.input.clone(),
            &source_uri(&fixture.provider),
            shared_value_position(),
            true,
            &AtomicBool::new(false),
        );
        let partial = computed.value.expect("walked references");
        assert!(partial.coverage.is_complete(), "{:?}", partial.coverage);
        assert_eq!(
            located(&partial.value),
            vec![
                (source_uri(&fixture.consumer), 6, 6),
                (source_uri(&fixture.provider), 2, 6),
            ]
        );
        assert_eq!(crate::workspace::rename::test_workspace_snapshots(), 1);
        assert_eq!(crate::workspace::rename::test_indexed_snapshots(), 0);
    }

    #[test]
    fn workspace_symbols_without_the_index_on_a_large_workspace_do_not_walk() {
        let fixture = query_fixture();
        let _failure = TargetFailure::new(Some(1));
        crate::workspace::rename::test_reset_workspace_snapshots();
        let found = symbols(&fixture.input, "Shared");
        assert!(found.value.is_empty(), "{:?}", found.value);
        assert!(
            found.coverage.gaps.iter().any(|gap| gap.reason
                == "the workspace index is unavailable; no workspace symbols were searched"),
            "{:?}",
            found.coverage
        );
        assert_eq!(crate::workspace::rename::test_workspace_snapshots(), 0);
    }

    #[test]
    fn workspace_symbols_without_the_index_on_a_small_workspace_walk() {
        let fixture = query_fixture();
        let _failure = TargetFailure::new(None);
        crate::workspace::rename::test_reset_workspace_snapshots();
        let found = symbols(&fixture.input, "Shared");
        assert!(!found.value.is_empty());
        assert_eq!(crate::workspace::rename::test_workspace_snapshots(), 1);
    }

    #[test]
    fn results_beyond_ten_thousand_keep_todays_error() {
        let constants = |prefix: &str| {
            let mut text = String::from("const\n");
            for index in 0..5_001 {
                text.push_str(&format!("  Val{prefix}{index} = 0;\n"));
            }
            text
        };
        let (_temp, root, mut workspace) = symbol_workspace(&[
            ("Alpha.pas", unit_text("Alpha", &constants("A"))),
            ("Beta.pas", unit_text("Beta", &constants("B"))),
            ("Consumer.pas", unit_text("Consumer", "")),
        ]);
        open_source(&mut workspace, &root.join("Consumer.pas"));
        let input = workspace.analysis_input();
        build_index_for(&input, &source_uri(&root.join("Alpha.pas")));
        reset_symbol_counters();
        let error = workspace_symbols_from_input(input, "Val", &AtomicBool::new(false))
            .value
            .expect_err("more than 10,000 results");
        assert_eq!(
            error,
            "workspace symbol query returned more than 10000 results; narrow the query"
        );
        assert_eq!(walks(), 0);
    }

    #[test]
    fn names_declared_together_are_all_listed_like_the_walk() {
        let pair = "unit Pair;\ninterface\ntype\n  TPair = class\n    FA, FB: Integer;\n  end;\nvar A, B: Integer;\nconst\n  C1 = 1; C2 = 2;\nimplementation\nend.\n";
        let files = [
            ("Pair.pas", pair.to_owned()),
            ("Consumer.pas", unit_text("Consumer", "")),
        ];
        let (_walk_temp, _, walk_workspace) = symbol_workspace(&files);
        let walked = symbols(&walk_workspace.analysis_input(), "").value;
        assert!(
            symbol_names(&walked).contains(&("FB".to_owned(), "Pair.pas".to_owned())),
            "{walked:?}"
        );

        let (_temp, root, mut workspace) = symbol_workspace(&files);
        open_source(&mut workspace, &root.join("Consumer.pas"));
        let input = workspace.analysis_input();
        build_index_for(&input, &source_uri(&root.join("Pair.pas")));
        reset_symbol_counters();
        let indexed = symbols(&input, "").value;
        assert!(super::test_take_fact_symbols() > 0);
        let strip = |found: &[lsp_types::SymbolInformation]| {
            found
                .iter()
                .map(|symbol| {
                    (
                        symbol.name.clone(),
                        symbol.kind,
                        symbol
                            .location
                            .uri
                            .path()
                            .rsplit('/')
                            .next()
                            .unwrap()
                            .to_owned(),
                        symbol.location.range,
                        symbol.container_name.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(strip(&indexed), strip(&walked));
    }

    #[test]
    fn an_answer_over_the_cap_reads_no_indexed_file() {
        let (_temp, root, mut workspace) = symbol_workspace(&[
            ("Alpha.pas", unit_text("Alpha", "const CapValueA = 1;\n")),
            ("Beta.pas", unit_text("Beta", "const CapValueB = 2;\n")),
            ("Consumer.pas", unit_text("Consumer", "")),
        ]);
        open_source(&mut workspace, &root.join("Consumer.pas"));
        let input = workspace.analysis_input();
        build_index_for(&input, &source_uri(&root.join("Alpha.pas")));
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::navigation::test_set_symbol_result_cap(None);
            }
        }
        let _reset = Reset;
        crate::navigation::test_set_symbol_result_cap(Some(1));
        super::test_take_fact_reads();
        let error =
            workspace_symbols_from_input(input.clone(), "CapValue", &AtomicBool::new(false))
                .value
                .expect_err("two results over a cap of one");
        assert_eq!(
            error,
            "workspace symbol query returned more than 1 results; narrow the query"
        );
        assert_eq!(super::test_take_fact_reads(), 0);

        crate::navigation::test_set_symbol_result_cap(Some(2));
        let found = symbols(&input, "CapValue");
        assert_eq!(found.value.len(), 2);
        assert_eq!(super::test_take_fact_reads(), 2);
    }

    #[test]
    fn open_and_deleted_documents_are_not_served_from_disk_facts() {
        let (_temp, root, mut workspace) = symbol_workspace(&[
            (
                "Provider.pas",
                unit_text("Provider", "const SharedValue = 1;\n"),
            ),
            ("Gone.pas", unit_text("Gone", "const SharedGone = 3;\n")),
            ("Kept.pas", unit_text("Kept", "const SharedKept = 4;\n")),
        ]);
        let provider = root.join("Provider.pas");
        let gone = root.join("Gone.pas");
        build_index_for(
            &workspace.analysis_input(),
            &source_uri(&root.join("Kept.pas")),
        );
        workspace
            .open_document(
                source_uri(&provider),
                unit_text("Provider", "const SharedEdited = 1;\n"),
                2,
            )
            .expect("open provider");
        let mut input = workspace.analysis_input();
        input
            .deleted_overrides
            .insert(source_uri(&gone), super::super::disk_stamp(&gone));
        reset_symbol_counters();
        let found = symbols(&input, "Shared");
        assert_eq!(
            symbol_names(&found.value),
            [
                ("SharedKept".to_owned(), "Kept.pas".to_owned()),
                ("SharedEdited".to_owned(), "Provider.pas".to_owned()),
            ]
        );
        assert_eq!(super::test_take_fact_symbols(), 1);
        assert_eq!(walks(), 0);
    }

    #[test]
    fn selection_reuses_context_valid_documents_and_skips_empty_or_oversized_parses() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let source_path = root.join("SelectionCache.pas");
        let source = "unit SelectionCache;\ninterface\nimplementation\nend.\n";
        fs::create_dir_all(&root).expect("workspace directory");
        fs::write(&source_path, source).expect("source");
        let uri = source_uri(&source_path);
        let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
        workspace
            .open_document(uri.clone(), source.to_owned(), 1)
            .expect("open source");
        let input = workspace.analysis_input();
        assert!(
            input.cached_documents.contains_key(&uri),
            "the fixture must provide a reusable parsed document"
        );
        let cancel = AtomicBool::new(false);

        crate::navigation::test_reset_document_parse_count();
        let computed =
            selection_ranges_from_input(input.clone(), &uri, vec![Position::new(0, 0)], &cancel);
        assert!(
            computed.value.is_ok(),
            "cached selection failed: {computed:?}"
        );
        assert_eq!(
            crate::navigation::test_document_parse_count(),
            0,
            "an unchanged cached document must be reused"
        );

        let mut changed_context = input.clone();
        crate::edit_context(
            &mut changed_context
                .cached_documents
                .get_mut(&uri)
                .expect("cached document")
                .context,
            |p| {
                p.conditional_context
                    .set_define("CONTEXT_CHANGED", pascal_project::ConditionalFact::True)
            },
        );
        crate::navigation::test_reset_document_parse_count();
        let computed =
            selection_ranges_from_input(changed_context, &uri, vec![Position::new(0, 0)], &cancel);
        assert!(
            computed.value.is_ok(),
            "changed-context selection failed: {computed:?}"
        );
        assert_eq!(
            crate::navigation::test_document_parse_count(),
            1,
            "a context mismatch must not reuse the cached document"
        );

        crate::navigation::test_reset_document_parse_count();
        let computed = selection_ranges_from_input(input.clone(), &uri, Vec::new(), &cancel);
        assert_eq!(computed.value, Ok(Vec::new()));
        assert_eq!(
            crate::navigation::test_document_parse_count(),
            0,
            "an empty selection request must not parse the source"
        );

        crate::navigation::test_reset_document_parse_count();
        let computed =
            selection_ranges_from_input(input, &uri, vec![Position::new(0, 0); 257], &cancel);
        let error = computed.value.expect_err("oversized selection request");
        assert!(error.contains("more than 256 positions"), "{error}");
        assert_eq!(
            crate::navigation::test_document_parse_count(),
            0,
            "an oversized selection request must not parse the source"
        );
    }

    struct AssistanceFixture {
        _temp: TempDir,
        root: PathBuf,
        project: PathBuf,
        provider: PathBuf,
        main: PathBuf,
        unrelated_closed: PathBuf,
        unrelated_open: PathBuf,
        project_source: String,
        main_source: String,
        input: WorkspaceInput,
    }

    fn assistance_fixture() -> AssistanceFixture {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let project = root.join("Main.dproj");
        let provider = root.join("Provider.pas");
        let main = root.join("Main.pas");
        let unrelated_closed = root.join("UnrelatedClosed.pas");
        let unrelated_open = root.join("UnrelatedOpen.pas");
        let project_source = "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"Provider.pas\" /></ItemGroup></Project>".to_string();
        let provider_source = "unit Provider;\ninterface\ntype\n  TWidget = class\n  public\n    OverlayMember: Integer;\n  end;\nprocedure OverlayRoutine(Value: Integer);\nimplementation\nprocedure OverlayRoutine(Value: Integer);\nbegin\nend;\nend.\n".to_string();
        let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Caller;\nvar\n  Obj: TWidget;\nbegin\n  Obj.Ov;\n  OverlayRoutine(1);\nend;\nend.\n".to_string();
        fs::create_dir_all(&root).expect("workspace directory");
        fs::write(&project, &project_source).expect("project");
        fs::write(&provider, &provider_source).expect("provider source");
        fs::write(&main, &main_source).expect("main source");
        fs::write(
            &unrelated_closed,
            "unit UnrelatedClosed;\ninterface\nconst Sentinel = 1;\nimplementation\nend.\n",
        )
        .expect("unrelated closed source");

        let provider_uri = source_uri(&provider);
        let main_uri = source_uri(&main);
        let mut workspace = Workspace::new(vec![root.clone()], WorkspaceOptions::default());
        workspace
            .open_document(provider_uri, provider_source.clone(), 7)
            .expect("provider overlay");
        workspace
            .open_document(main_uri, main_source.clone(), 11)
            .expect("main overlay");
        workspace
            .open_document(
                source_uri(&unrelated_open),
                "unit UnrelatedOpen;\ninterface\nconst Sentinel = 2;\nimplementation\nend.\n"
                    .to_owned(),
                13,
            )
            .expect("unrelated open overlay");

        AssistanceFixture {
            _temp: temp,
            root,
            project,
            provider,
            main,
            unrelated_closed,
            unrelated_open,
            project_source,
            main_source,
            input: workspace.analysis_input(),
        }
    }

    #[test]
    fn name_free_assistance_rejects_active_declaration_includes_but_keeps_safe_includes() {
        let cases = [
            (
                "ActiveDeclarations",
                "{$I Local.inc}\n",
                "procedure GlobalRun(A: string);\nconst IncludeOnly = 1;\n",
                false,
            ),
            (
                "HarmlessDirectives",
                "{$I Local.inc}\n",
                "{$DEFINE LOCAL_FLAG}\n{$WARN SYMBOL_DEPRECATED OFF}\n",
                true,
            ),
            (
                "InactiveInclude",
                "{$IF False}\n{$I Missing.inc}\n{$ENDIF}\n",
                "",
                true,
            ),
        ];

        for (name, include_directive, include_source, should_succeed) in cases {
            let temp = tempfile::tempdir().expect("temporary workspace");
            let source_path = temp.path().join(format!("{name}.pas"));
            let include_path = temp.path().join("Local.inc");
            let source = format!(
                "unit {name};\ninterface\nprocedure GlobalRun(A: Integer);\n{include_directive}implementation\nprocedure GlobalRun(A: Integer);\nbegin\nend;\nprocedure Caller;\nbegin\n  GlobalRun(1);\n  IncludeOnly;\nend;\nend.\n"
            );
            fs::write(&source_path, &source).expect("source");
            fs::write(&include_path, include_source).expect("include");
            let source_uri = source_uri(&source_path);
            let mut workspace =
                Workspace::new(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
            workspace
                .open_document(source_uri.clone(), source.clone(), 1)
                .expect("open source");
            let input = workspace.analysis_input();
            let cancel = AtomicBool::new(false);

            let completion = completion_from_input(
                input.clone(),
                &source_uri,
                position_after(&source, "IncludeOnly"),
                &cancel,
            );
            let signature_offset = source
                .rfind("GlobalRun(")
                .expect("call site")
                .saturating_add("GlobalRun(".len());
            let signature = signature_help_from_input(
                input,
                &source_uri,
                crate::text::offset_to_position(&source, signature_offset)
                    .expect("signature position"),
                &cancel,
            );

            assert_eq!(
                completion.value.is_ok(),
                should_succeed,
                "{name}: {completion:?}"
            );
            assert_eq!(
                signature.value.is_ok(),
                should_succeed,
                "{name}: {signature:?}"
            );
        }
    }

    /// l4d-20 Decision 5: Delphi's unit search path is not recursive, so
    /// completion never lists a search directory's subdirectories.
    #[test]
    fn completion_providers_come_from_search_directories_without_recursion() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        fs::create_dir_all(root.join("lib/nested")).expect("library directories");
        fs::write(
            root.join("App.dproj"),
            "<Project><PropertyGroup><MainSource>Consumer.pas</MainSource><DCC_UnitSearchPath>lib</DCC_UnitSearchPath></PropertyGroup></Project>",
        )
        .expect("project");
        let consumer_source = "unit Consumer;\ninterface\nimplementation\nprocedure Run;\nvar Value: TDi;\nbegin Value := nil; end;\nend.\n";
        let consumer = root.join("Consumer.pas");
        fs::write(&consumer, consumer_source).expect("consumer");
        fs::write(
            root.join("lib/Direct.pas"),
            "unit Direct;\ninterface\ntype TDirectType = class end;\nimplementation\nend.\n",
        )
        .expect("direct provider");
        fs::write(
            root.join("lib/nested/Deep.pas"),
            "unit Deep;\ninterface\ntype TDiscardedType = class end;\nimplementation\nend.\n",
        )
        .expect("nested provider");
        let input =
            test_workspace(vec![root.clone()], WorkspaceOptions::default()).analysis_input();
        let cancel = AtomicBool::new(false);
        crate::catalogue::test_reset_listings();
        let completion = completion_from_input(
            input,
            &source_uri(&consumer),
            position_after(consumer_source, "TDi"),
            &cancel,
        );
        let labels = completion
            .value
            .expect("completion")
            .items
            .into_iter()
            .map(|item| item.label)
            .collect::<Vec<_>>();
        assert!(
            labels.iter().any(|label| label == "TDirectType"),
            "{labels:?}"
        );
        assert!(
            labels.iter().all(|label| label != "TDiscardedType"),
            "{labels:?}"
        );
        let nested = root.join("lib/nested");
        assert!(
            crate::catalogue::test_listings()
                .iter()
                .all(|listed| !listed.starts_with(&nested)),
            "lib/nested was listed: {:?}",
            crate::catalogue::test_listings()
        );
    }

    /// Projectless contexts keep the shared resolver's recursive workspace-root
    /// tier (guard: green before and after).
    #[test]
    fn projectless_completion_still_offers_providers_in_subdirectories() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        fs::create_dir_all(root.join("sub")).expect("sub");
        let consumer_source = "unit Consumer;\ninterface\nimplementation\nprocedure Run;\nvar Value: TPro;\nbegin Value := nil; end;\nend.\n";
        let consumer = root.join("Consumer.pas");
        fs::write(&consumer, consumer_source).expect("consumer");
        fs::write(
            root.join("sub/Provider.pas"),
            "unit Provider;\ninterface\ntype TProviderType = class end;\nimplementation\nend.\n",
        )
        .expect("provider");
        let input =
            test_workspace(vec![root.clone()], WorkspaceOptions::default()).analysis_input();
        let cancel = AtomicBool::new(false);
        let completion = completion_from_input(
            input,
            &source_uri(&consumer),
            position_after(consumer_source, "TPro"),
            &cancel,
        );
        assert!(
            completion
                .value
                .expect("completion")
                .items
                .iter()
                .any(|item| item.label == "TProviderType")
        );
    }

    fn position_after(source: &str, needle: &str) -> Position {
        let offset = source
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not found in assistance fixture"))
            .saturating_add(needle.len());
        crate::text::offset_to_position(source, offset).expect("assistance position")
    }

    fn assert_assistance_read_set(
        fixture: &AssistanceFixture,
        records: &[super::super::rename::SourceRecord],
    ) {
        let expected_sources =
            HashSet::from([source_uri(&fixture.main), source_uri(&fixture.provider)]);
        let source_records = records
            .iter()
            .filter(|record| record.path.is_none())
            .collect::<Vec<_>>();
        assert_eq!(
            source_records
                .iter()
                .map(|record| record.uri.clone())
                .collect::<HashSet<_>>(),
            expected_sources,
            "assistance read set must contain only the requested source and its import"
        );
        assert!(
            source_records.iter().all(|record| {
                record.uri != source_uri(&fixture.unrelated_closed)
                    && record.uri != source_uri(&fixture.unrelated_open)
            }),
            "unrelated closed/open source sentinel leaked into the assistance read set"
        );
        let expected_paths = HashSet::from([
            fixture.root.clone(),
            fixture.project.clone(),
            fixture.main.clone(),
            fixture.provider.clone(),
            fixture.root.join(".delphilsp.json"),
        ]);
        assert!(
            records
                .iter()
                .filter_map(|record| record.path.as_ref())
                .all(|path| expected_paths.contains(path)),
            "assistance read set retained an unrelated path: {:?}",
            records
                .iter()
                .filter_map(|record| record.path.as_ref())
                .collect::<Vec<_>>()
        );
        let project_records = records
            .iter()
            .filter(|record| record.path.as_deref() == Some(fixture.project.as_path()))
            .collect::<Vec<_>>();
        assert_eq!(
            project_records.len(),
            1,
            "project metadata must be observed once"
        );
        assert_eq!(
            project_records[0].content_bytes.as_deref(),
            Some(fixture.project_source.as_bytes()),
            "project metadata must retain the bytes consumed by discovery"
        );
        let main_overlay = records
            .iter()
            .find(|record| record.uri == source_uri(&fixture.main) && record.path.is_none())
            .expect("main overlay read record");
        assert!(main_overlay.open);
        assert_eq!(main_overlay.version, Some(11));
        let provider_overlay = records
            .iter()
            .find(|record| record.uri == source_uri(&fixture.provider) && record.path.is_none())
            .expect("provider overlay read record");
        assert!(provider_overlay.open);
        assert_eq!(provider_overlay.version, Some(7));
    }

    #[test]
    fn assistance_adapters_return_success_with_the_exact_import_and_metadata_read_set() {
        let fixture = assistance_fixture();
        let cancel = AtomicBool::new(false);
        let completion = completion_from_input(
            fixture.input.clone(),
            &source_uri(&fixture.main),
            position_after(&fixture.main_source, "Obj.Ov"),
            &cancel,
        );
        assert_eq!(
            completion
                .value
                .as_ref()
                .expect("completion worker")
                .items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            ["OverlayMember"]
        );
        assert_assistance_read_set(&fixture, &completion.records);
        revalidate_input(&fixture.input, &completion.records, &cancel)
            .expect("unchanged populated completion read set must revalidate");

        let signature = signature_help_from_input(
            fixture.input.clone(),
            &source_uri(&fixture.main),
            position_after(&fixture.main_source, "OverlayRoutine("),
            &cancel,
        );
        assert!(
            signature.value.as_ref().is_ok_and(Option::is_some),
            "signature worker must return the imported routine: {signature:?}"
        );
        assert_assistance_read_set(&fixture, &signature.records);
        revalidate_input(&fixture.input, &signature.records, &cancel)
            .expect("unchanged populated signature read set must revalidate");
    }

    struct ClosedCompletionFixture {
        _temp: TempDir,
        main: PathBuf,
        provider: PathBuf,
        main_source: String,
        provider_source: String,
        input: WorkspaceInput,
    }

    fn closed_completion_fixture() -> ClosedCompletionFixture {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let main = temp.path().join("Main.pas");
        let provider = temp.path().join("Provider.pas");
        let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Caller;\nbegin\n  Doc\nend;\nend.\n".to_string();
        let provider_source = "unit Provider;\ninterface\n/// <summary>OLD DOCUMENTATION.</summary>\nfunction DocOld: Integer;\nimplementation\nfunction DocOld: Integer;\nbegin\n  Result := 1;\nend;\nend.\n".to_string();
        fs::write(&main, &main_source).expect("main source");
        fs::write(&provider, &provider_source).expect("provider source");
        let workspace =
            test_workspace(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
        ClosedCompletionFixture {
            _temp: temp,
            main,
            provider,
            main_source,
            provider_source,
            input: workspace.analysis_input(),
        }
    }

    #[test]
    fn completion_resolution_rejects_provider_change_after_original_validation() {
        let fixture = closed_completion_fixture();
        let main_uri = source_uri(&fixture.main);
        let provider_uri = source_uri(&fixture.provider);
        let position = position_after(&fixture.main_source, "  Doc");
        let cancel = AtomicBool::new(false);
        let completion = completion_from_input_with_options(
            fixture.input.clone(),
            &main_uri,
            position,
            CompletionOptions {
                format: MarkupKind::Markdown,
                defer_documentation: true,
                defer_detail: true,
                snippet_support: false,
            },
            &cancel,
        );
        let completion_result = completion.value.expect("completion result");
        let seed = completion_result
            .seeds
            .iter()
            .find(|seed| seed.candidate_uri() == &provider_uri)
            .expect("provider completion seed");
        let candidate_index = seed.candidate_index();
        let original_records = completion.records.clone();
        let expected_source_generation = fixture.input.source_generation;
        let expected_configuration_generation = fixture.input.configuration_generation;
        let (ready_sender, ready_receiver) = channel();
        let (release_sender, release_receiver) = channel();
        install_snapshot_priority_barrier(main_uri.clone(), ready_sender, release_receiver);

        let input = fixture.input.clone();
        let worker = thread::spawn(move || {
            let cancel = AtomicBool::new(false);
            completion_metadata_from_input(
                input,
                &main_uri,
                position,
                expected_source_generation,
                expected_configuration_generation,
                &provider_uri,
                candidate_index,
                MarkupKind::Markdown,
                true,
                true,
                false,
                &original_records,
                &cancel,
            )
        });
        ready_receiver
            .recv()
            .expect("resolve snapshot barrier entered");
        let changed_provider = fixture
            .provider_source
            .replace("DocOld", "DocNew")
            .replace("OLD DOCUMENTATION", "NEW DOCUMENTATION");
        fs::write(&fixture.provider, changed_provider).expect("changed provider source");
        release_sender
            .send(())
            .expect("release resolve snapshot barrier");
        let computed = worker.join().expect("resolve worker");
        assert!(
            computed.value.is_err(),
            "provider revision changed after original validation: {computed:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn full_source_revalidation_rejects_inconsistent_parsed_text_and_hash() {
        let fixture = closed_completion_fixture();
        let main_uri = source_uri(&fixture.main);
        let provider_uri = source_uri(&fixture.provider);
        let cancel = AtomicBool::new(false);
        let completion = completion_from_input(
            fixture.input.clone(),
            &main_uri,
            position_after(&fixture.main_source, "  Doc"),
            &cancel,
        );
        assert!(
            completion.value.is_ok(),
            "completion result: {completion:?}"
        );
        let provider_record = completion
            .records
            .iter()
            .find(|record| record.uri == provider_uri && record.path.is_none())
            .cloned()
            .expect("full provider source record");
        let changed_provider = fixture
            .provider_source
            .replace("DocOld", "DocNew")
            .replace("OLD DOCUMENTATION", "NEW DOCUMENTATION");
        let before = fs::metadata(&fixture.provider).expect("provider metadata");
        fs::write(&fixture.provider, &changed_provider).expect("changed provider source");
        restore_mtime(&fixture.provider, &before);

        let mut inconsistent = provider_record;
        inconsistent.content_hash = Some(content_hash_bytes(changed_provider.as_bytes()));
        let workspace = test_workspace(
            vec![
                fixture
                    .provider
                    .parent()
                    .expect("fixture root")
                    .to_path_buf(),
            ],
            WorkspaceOptions::default(),
        );
        workspace
            .revalidate_records(std::slice::from_ref(&inconsistent))
            .expect_err("shared validation must retain parsed-source equality");
        let validation = revalidate_input(&fixture.input, &[inconsistent], &cancel);
        let error =
            validation.expect_err("parsed source equality must not be replaced by a later hash");
        assert!(
            error.contains("changed") || error.contains("resolving"),
            "{error}"
        );
    }

    #[test]
    fn assistance_empty_results_retain_overlay_and_project_read_sets() {
        let fixture = assistance_fixture();
        let main_uri = source_uri(&fixture.main);
        let provider_uri = source_uri(&fixture.provider);
        let mut input = fixture.input.clone();
        let empty_source = fixture
            .main_source
            .replace("Obj.Ov", "// Obj.Ov")
            .replace("OverlayRoutine(1)", "UnknownRoutine(1)");
        input
            .overlays
            .get_mut(&main_uri)
            .expect("main overlay")
            .text = empty_source.as_str().into();
        let cancel = AtomicBool::new(false);

        let completion = completion_from_input(
            input.clone(),
            &main_uri,
            position_after(&empty_source, "// Obj.Ov"),
            &cancel,
        );
        assert_eq!(completion.value, Ok(lsp_types::CompletionList::default()));
        assert_assistance_read_set(&fixture, &completion.records);
        revalidate_input(&input, &completion.records, &cancel)
            .expect("unchanged empty completion read set must revalidate");

        let signature = signature_help_from_input(
            input.clone(),
            &main_uri,
            position_after(&empty_source, "UnknownRoutine("),
            &cancel,
        );
        assert_eq!(signature.value, Ok(None));
        assert_assistance_read_set(&fixture, &signature.records);
        revalidate_input(&input, &signature.records, &cancel)
            .expect("unchanged null signature read set must revalidate");

        let mut changed_input = input;
        let provider_overlay = changed_input
            .overlays
            .get_mut(&provider_uri)
            .expect("provider overlay");
        provider_overlay.text = provider_overlay
            .text
            .replace("OverlayMember", "ChangedMember")
            .into();
        provider_overlay.version = provider_overlay.version.saturating_add(1);
        revalidate_input(&changed_input, &completion.records, &cancel)
            .expect_err("empty completion must retain the provider read set");
        revalidate_input(&changed_input, &signature.records, &cancel)
            .expect_err("null signature help must retain the provider read set");
    }

    #[cfg(target_os = "linux")]
    unsafe extern "C" {
        fn utimensat(
            dirfd: i32,
            pathname: *const std::os::raw::c_char,
            times: *const Timespec,
            flags: i32,
        ) -> i32;
    }

    #[cfg(target_os = "linux")]
    const AT_FDCWD: i32 = -100;

    #[cfg(target_os = "linux")]
    const UTIME_OMIT: i64 = 1_073_741_822;

    #[cfg(target_os = "linux")]
    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i64,
    }

    // The change identity catches a restored-mtime rewrite at the stamp level
    // on Unix, before the content check runs; both reject with a retry error.
    #[cfg(target_os = "linux")]
    fn is_changed_config_rejection(error: &str) -> bool {
        error.contains("configuration content changed")
            || error.contains("metadata or membership changed")
    }

    #[cfg(target_os = "linux")]
    fn restore_mtime(path: &Path, metadata: &fs::Metadata) {
        let pathname = CString::new(path.to_string_lossy().as_bytes()).expect("valid path");
        let times = [
            Timespec {
                tv_sec: 0,
                tv_nsec: UTIME_OMIT,
            },
            Timespec {
                tv_sec: metadata.mtime(),
                tv_nsec: metadata.mtime_nsec(),
            },
        ];
        let result = unsafe { utimensat(AT_FDCWD, pathname.as_ptr(), times.as_ptr(), 0) };
        assert_eq!(result, 0, "utimensat failed for {}", path.display());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn assistance_rejects_same_stamp_project_metadata_mutations() {
        let fixture = assistance_fixture();
        let cancel = AtomicBool::new(false);
        let completion = completion_from_input(
            fixture.input.clone(),
            &source_uri(&fixture.main),
            position_after(&fixture.main_source, "Obj.Ov"),
            &cancel,
        );
        assert!(
            completion.value.is_ok(),
            "completion worker: {completion:?}"
        );
        let project_record = completion
            .records
            .iter()
            .find(|record| record.path.as_deref() == Some(fixture.project.as_path()))
            .expect("project read record");
        let before = fs::metadata(&fixture.project).expect("project metadata");
        let changed = fixture.project_source.replace("Main.pas", "Main.qas");
        assert_ne!(changed, fixture.project_source);
        assert_eq!(changed.len(), fixture.project_source.len());
        fs::write(&fixture.project, changed).expect("mutated project");
        restore_mtime(&fixture.project, &before);

        let error = revalidate_input(
            &fixture.input,
            std::slice::from_ref(project_record),
            &cancel,
        )
        .expect_err("same-stamp metadata content changes must stale assistance");
        assert!(is_changed_config_rejection(&error), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn assistance_empty_and_null_results_reject_same_stamp_project_metadata_mutation() {
        let fixture = assistance_fixture();
        let main_uri = source_uri(&fixture.main);
        let empty_source = fixture
            .main_source
            .replace("Obj.Ov", "// Obj.Ov")
            .replace("OverlayRoutine(1)", "UnknownRoutine(1)");
        let mut input = fixture.input.clone();
        input
            .overlays
            .get_mut(&main_uri)
            .expect("main overlay")
            .text = empty_source.as_str().into();
        let cancel = AtomicBool::new(false);

        let completion = completion_from_input(
            input.clone(),
            &main_uri,
            position_after(&empty_source, "// Obj.Ov"),
            &cancel,
        );
        assert_eq!(completion.value, Ok(lsp_types::CompletionList::default()));
        assert_assistance_read_set(&fixture, &completion.records);
        revalidate_input(&input, &completion.records, &cancel)
            .expect("unchanged empty completion read set must revalidate");

        let signature = signature_help_from_input(
            input.clone(),
            &main_uri,
            position_after(&empty_source, "UnknownRoutine("),
            &cancel,
        );
        assert_eq!(signature.value, Ok(None));
        assert_assistance_read_set(&fixture, &signature.records);
        revalidate_input(&input, &signature.records, &cancel)
            .expect("unchanged null signature read set must revalidate");

        let before = fs::metadata(&fixture.project).expect("project metadata");
        let changed = fixture.project_source.replace("Main.pas", "Main.qas");
        assert_ne!(changed, fixture.project_source);
        assert_eq!(changed.len(), fixture.project_source.len());
        fs::write(&fixture.project, changed).expect("mutated project");
        restore_mtime(&fixture.project, &before);

        let completion_error = revalidate_input(&input, &completion.records, &cancel)
            .expect_err("empty completion must retain consumed project metadata");
        assert!(
            is_changed_config_rejection(&completion_error),
            "{completion_error}"
        );
        let signature_error = revalidate_input(&input, &signature.records, &cancel)
            .expect_err("null signature must retain consumed project metadata");
        assert!(
            is_changed_config_rejection(&signature_error),
            "{signature_error}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn assistance_retains_lazy_package_project_observation_at_the_read_boundary() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let main = root.join("Main.pas");
        let package_project = root.join("packages/Package.dproj");
        let package_source = root.join("packages/PackageMain.dpk");
        let mappings = root.join("packages/Mappings.optset");
        let provider = root.join("packages/PackageUnit.pas");
        let main_source = "unit Main;\ninterface\nuses PackageUnit;\nimplementation\nprocedure Caller;\nbegin\n  PackageRoutine;\nend;\nend.\n";
        let package_project_source = "<Project><PropertyGroup><MainSource>PackageMain.dpk</MainSource></PropertyGroup><Import Project=\"Mappings.optset\" /></Project>";
        let mappings_source = "<Project><ItemGroup><DCCReference Include=\"PackageUnit.pas\" /></ItemGroup></Project>";
        let changed_package_project_source =
            package_project_source.replace("Mappings.optset", "Settings.optset");
        fs::create_dir_all(package_project.parent().expect("package directory"))
            .expect("package directory");
        fs::write(&main, main_source).expect("main source");
        fs::write(&package_source, "package PackageMain;\ncontains\nend.\n")
            .expect("package source");
        fs::write(&provider, "unit PackageUnit;\ninterface\nprocedure PackageRoutine;\nimplementation\nprocedure PackageRoutine; begin end;\nend.\n")
            .expect("package unit");
        fs::write(&mappings, mappings_source).expect("package mappings");
        fs::write(root.join("packages/Settings.optset"), mappings_source)
            .expect("changed package mappings");
        fs::write(&package_project, package_project_source).expect("package project");
        fs::write(
            root.join("App.dproj"),
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UsePackage>Package</DCC_UsePackage></PropertyGroup></Project>",
        )
        .expect("application project");

        let workspace = Workspace::new(vec![root], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let main_uri = source_uri(&main);
        let cancel = AtomicBool::new(false);
        let unchanged = completion_from_input(
            input.clone(),
            &main_uri,
            position_after(main_source, "PackageRoutine"),
            &cancel,
        );
        assert!(
            unchanged.value.is_ok(),
            "unchanged package assistance: {unchanged:?}"
        );
        let unchanged_record = unchanged
            .records
            .iter()
            .find(|record| record.path.as_deref() == Some(package_project.as_path()))
            .expect("unchanged lazy package project record");
        assert_eq!(
            unchanged_record.content_bytes.as_deref(),
            Some(package_project_source.as_bytes()),
            "package project bytes must come from the parser read"
        );
        let unchanged_mapping_record = unchanged
            .records
            .iter()
            .find(|record| record.path.as_deref() == Some(mappings.as_path()))
            .expect("unchanged lazy package mapping record");
        assert_eq!(
            unchanged_mapping_record.content_bytes.as_deref(),
            Some(mappings_source.as_bytes()),
            "package project imports must retain their consumed mapping bytes"
        );
        revalidate_input(&input, &unchanged.records, &cancel)
            .expect("unchanged lazy package project must revalidate");

        let before = fs::metadata(&package_project).expect("package project metadata");
        let package_project_for_hook = package_project.clone();
        let changed_for_hook = changed_package_project_source.to_owned();
        let _hook =
            pascal_project::test_after_project_read_at(package_project.clone(), move |path| {
                assert_eq!(path, package_project_for_hook.as_path());
                fs::write(&package_project_for_hook, changed_for_hook)
                    .expect("mutate package project after read");
            });
        let mutated = completion_from_input(
            input.clone(),
            &main_uri,
            position_after(main_source, "PackageRoutine"),
            &cancel,
        );
        assert!(
            mutated.value.is_ok(),
            "read-boundary package assistance: {mutated:?}"
        );
        let mutated_record = mutated
            .records
            .iter()
            .find(|record| record.path.as_deref() == Some(package_project.as_path()))
            .expect("mutated lazy package project record");
        assert_eq!(
            mutated_record.content_bytes.as_deref(),
            Some(package_project_source.as_bytes()),
            "snapshot must retain bytes consumed before the mutation"
        );
        restore_mtime(&package_project, &before);
        let error = revalidate_input(&input, &mutated.records, &cancel)
            .expect_err("package project mutation at the read boundary must stale assistance");
        assert!(is_changed_config_rejection(&error), "{error}");

        let upper = temp.path().join("UpperPackage.dpk");
        let lower = temp.path().join("lowerpackage.dpk");
        let upper_source = "package UpperPackage;\ncontains\nend.\n";
        let lower_source = "package lowerpackage;\ncontains\nend.\n";
        fs::write(&upper, upper_source).expect("uppercase package descriptor");
        fs::write(&lower, lower_source).expect("lowercase package descriptor");
        let roots = vec![temp.path().to_path_buf()];
        let overrides = EffectiveOverrides::default();
        let read_policy = ReadPolicy::new(&roots, &[], &[], &overrides);
        let options = ProjectOptions::default();
        let upper_entry = ProjectPathEntry {
            path: upper.clone(),
            provenance: ProjectPathProvenance::LegacyNative,
        };
        let lower_entry = ProjectPathEntry {
            path: lower.clone(),
            provenance: ProjectPathProvenance::LegacyNative,
        };
        let upper_read = pascal_project::read_package_metadata(
            &upper,
            &options,
            &overrides,
            &read_policy,
            &upper_entry,
        )
        .expect("upper package read");
        let lower_read = pascal_project::read_package_metadata(
            &lower,
            &options,
            &overrides,
            &read_policy,
            &lower_entry,
        )
        .expect("lower package read");
        let upper_observation = upper_read
            .metadata_observations
            .iter()
            .find_map(|observation| match observation {
                MetadataObservation::Payload {
                    path, content_hash, ..
                } if path == &upper => Some((path, *content_hash)),
                _ => None,
            })
            .expect("upper payload observation");
        let lower_observation = lower_read
            .metadata_observations
            .iter()
            .find_map(|observation| match observation {
                MetadataObservation::Payload {
                    path, content_hash, ..
                } if path == &lower => Some((path, *content_hash)),
                _ => None,
            })
            .expect("lower payload observation");
        assert_eq!(upper_observation.0, &upper);
        assert_eq!(lower_observation.0, &lower);
        assert_eq!(
            upper_observation.1,
            content_hash_bytes(upper_source.as_bytes())
        );
        assert_eq!(
            lower_observation.1,
            content_hash_bytes(lower_source.as_bytes())
        );
    }

    #[test]
    fn references_revalidation_observes_a_new_pascal_consumer() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(false);
        let computed = references_from_input(
            fixture.input.clone(),
            &source_uri(&fixture.provider),
            shared_value_position(),
            true,
            &cancel,
        );
        assert!(
            computed.value.is_ok(),
            "reference computation: {computed:?}"
        );
        revalidate_input(&fixture.input, &computed.records, &cancel)
            .expect("unchanged reference inputs must remain valid");

        let added_consumer = fixture
            .provider
            .parent()
            .expect("workspace directory")
            .join("AddedConsumer.pas");
        fs::write(
            &added_consumer,
            "unit AddedConsumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n",
        )
        .expect("new Pascal consumer");

        let error = revalidate_input(&fixture.input, &computed.records, &cancel)
            .expect_err("a new Pascal consumer must stale completed references");
        assert!(
            error.contains("membership") || error.contains("changed"),
            "unexpected new-consumer revalidation error: {error}"
        );
    }

    /// l4d-20 AC#1: a second references request on the same workspace performs
    /// no directory walk (Task 1 recorded that it did).
    #[test]
    fn a_second_references_request_lists_no_directory() {
        let fixture = query_fixture();
        let root =
            crate::workspace::absolute_path(fixture.provider.parent().unwrap().to_path_buf());
        let nested = root.join("sub").join("Nested.pas");
        fs::create_dir_all(nested.parent().unwrap()).expect("nested directory");
        fs::write(
            &nested,
            "unit Nested;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n",
        )
        .expect("nested consumer");
        // Settled stamps, so the shared catalogue outlives the first request.
        pascal_project::backdate_tree_for_test(&root);
        let cancel = AtomicBool::new(false);
        let references = || {
            references_from_input(
                fixture.input.clone(),
                &source_uri(&fixture.provider),
                shared_value_position(),
                true,
                &cancel,
            )
        };
        crate::catalogue::test_reset_listings();
        let first = references();
        assert!(first.value.is_ok(), "first references: {first:?}");
        let listed = crate::catalogue::test_listings();
        assert!(
            listed.contains(&root) && listed.contains(&root.join("sub")),
            "the first request walks the root and its subdirectory: {listed:?}"
        );
        crate::catalogue::test_reset_listings();
        let second = references();
        assert!(second.value.is_ok(), "second references: {second:?}");
        assert!(
            crate::catalogue::test_listings().is_empty(),
            "the second request listed {:?}",
            crate::catalogue::test_listings()
        );
        let first = first.value.unwrap().value;
        assert!(
            first
                .iter()
                .any(|location| location.uri == source_uri(&nested)),
            "the nested consumer is found: {first:?}"
        );
        assert_eq!(
            first,
            second.value.unwrap().value,
            "both requests find the same references"
        );
    }

    /// Two views of one root sharing one cache, the narrower excluding
    /// `vendor/`, and a references query naming the files each finds. The
    /// tree is settled so catalogues outlive a request.
    fn exclusion_views() -> (
        TempDir,
        WorkspaceInput,
        WorkspaceInput,
        impl Fn(&WorkspaceInput) -> Vec<String>,
    ) {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let provider = root.join("Provider.pas");
        fs::write(
            &provider,
            "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n",
        )
        .expect("provider");
        fs::create_dir_all(root.join("vendor")).expect("vendor");
        fs::write(
            root.join("vendor/VendorConsumer.pas"),
            "unit VendorConsumer;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(SharedValue);\nend;\nend.\n",
        )
        .expect("vendor consumer");
        pascal_project::backdate_tree_for_test(&root);
        let wide = test_workspace(vec![root.clone()], WorkspaceOptions::default()).analysis_input();
        let mut narrow = test_workspace(
            vec![root.clone()],
            WorkspaceOptions {
                exclude: vec!["vendor".to_string()],
                ..WorkspaceOptions::default()
            },
        )
        .analysis_input();
        narrow.project_cache = wide.project_cache.clone();
        let found = move |input: &WorkspaceInput| {
            let computed = references_from_input(
                input.clone(),
                &source_uri(&provider),
                shared_value_position(),
                false,
                &AtomicBool::new(false),
            );
            computed
                .value
                .expect("references")
                .value
                .into_iter()
                .map(|location| location.uri.path().rsplit('/').next().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        (temp, wide, narrow, found)
    }

    #[test]
    fn a_narrower_exclusion_set_never_enumerates_what_a_wider_one_catalogued() {
        let (_temp, wide, narrow, found) = exclusion_views();
        assert_eq!(found(&wide), ["VendorConsumer.pas"]);
        assert!(
            found(&narrow).is_empty(),
            "the narrower view must not see vendor/"
        );
        assert_eq!(
            found(&wide),
            ["VendorConsumer.pas"],
            "the wider view still does"
        );
    }

    #[test]
    fn a_wider_exclusion_set_still_finds_what_a_narrower_one_catalogued_first() {
        let (_temp, wide, narrow, found) = exclusion_views();
        assert!(
            found(&narrow).is_empty(),
            "the narrower view must not see vendor/"
        );
        assert_eq!(
            found(&wide),
            ["VendorConsumer.pas"],
            "the wider view is not served the narrower catalogue"
        );
        assert!(
            found(&narrow).is_empty(),
            "the narrower view still does not"
        );
    }

    #[test]
    fn completed_reference_results_revalidate_changed_sources() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(false);
        let computed = references_from_input(
            fixture.input.clone(),
            &source_uri(&fixture.provider),
            shared_value_position(),
            false,
            &cancel,
        );
        assert!(
            computed.value.is_ok(),
            "reference worker must complete: {computed:?}"
        );
        assert!(
            !computed.records.is_empty(),
            "reference result must carry its read set"
        );

        fs::write(
            &fixture.consumer,
            fixture
                .consumer_source
                .replace("SharedValue", "ChangedValue"),
        )
        .expect("change consumer source");
        let error = revalidate_input(&fixture.input, &computed.records, &cancel)
            .expect_err("completed references must become stale after a source change");
        assert!(
            error.contains("changed") || error.contains("resolving"),
            "{error}"
        );
    }

    #[test]
    fn completed_highlight_results_revalidate_changed_sources() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(false);
        let computed = highlights_from_input(
            fixture.input.clone(),
            &source_uri(&fixture.provider),
            shared_value_position(),
            &cancel,
        );
        assert!(
            computed.value.is_ok(),
            "highlight worker must complete: {computed:?}"
        );
        assert_eq!(computed.value.as_ref().expect("highlights").len(), 1);
        assert!(
            !computed.records.is_empty(),
            "highlight result must carry its read set"
        );
        revalidate_input(&fixture.input, &computed.records, &cancel)
            .expect("unchanged highlight inputs must remain valid");

        fs::write(
            &fixture.provider,
            fixture
                .provider_source
                .replace("SharedValue", "ChangedValue"),
        )
        .expect("change provider source");
        let error = revalidate_input(&fixture.input, &computed.records, &cancel)
            .expect_err("completed highlights must become stale after a source change");
        assert!(
            error.contains("changed") || error.contains("resolving"),
            "{error}"
        );
    }

    #[test]
    fn completed_hover_results_revalidate_imported_source_snapshots() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(false);
        let computed = hover_from_input(
            fixture.input.clone(),
            &source_uri(&fixture.consumer),
            Position::new(6, 6),
            MarkupKind::PlainText,
            &cancel,
        );
        assert!(
            computed.value.as_ref().is_ok_and(Option::is_some),
            "hover worker must return an imported declaration: {computed:?}"
        );
        assert!(
            !computed.records.is_empty(),
            "hover result must carry its bounded source read set"
        );
        revalidate_input(&fixture.input, &computed.records, &cancel)
            .expect("unchanged hover inputs must remain valid");

        fs::write(
            &fixture.provider,
            fixture
                .provider_source
                .replace("SharedValue", "ChangedValue"),
        )
        .expect("change hover provider source");
        let error = revalidate_input(&fixture.input, &computed.records, &cancel)
            .expect_err("completed hover must become stale after a provider change");
        assert!(
            error.contains("changed") || error.contains("resolving"),
            "{error}"
        );
    }

    #[test]
    fn document_symbols_revalidate_project_metadata_used_for_defines() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let source_path = root.join("Main.pas");
        let project_path = root.join("App.dproj");
        let source = "unit Main;\ninterface\n{$IFDEF FEATURE}\nconst Selected = 1;\n{$ELSE}\nconst Other = 2;\n{$ENDIF}\nimplementation\nend.\n";
        fs::create_dir_all(&root).expect("workspace directory");
        fs::write(&source_path, source).expect("source");
        fs::write(
            &project_path,
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Define>FEATURE</DCC_Define></PropertyGroup></Project>",
        )
        .expect("project");
        let workspace = Workspace::new(vec![root], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let computed =
            document_symbols_from_input(input.clone(), &source_uri(&source_path), &cancel);
        assert!(
            computed.value.is_ok(),
            "document symbols must compute: {computed:?}"
        );

        fs::write(
            &project_path,
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Define>OTHER</DCC_Define></PropertyGroup></Project>",
        )
        .expect("changed project");
        let error = revalidate_input(&input, &computed.records, &cancel)
            .expect_err("project defines used by document symbols must be revalidated");
        assert!(
            error.to_ascii_lowercase().contains("configuration")
                || error.to_ascii_lowercase().contains("metadata")
                || error.to_ascii_lowercase().contains("changed"),
            "unexpected project metadata revalidation error: {error}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn document_symbols_retain_case_distinct_consumed_optset_observations() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let source_path = root.join("Main.pas");
        let project_path = root.join("App.dproj");
        let upper_optset = root.join("Flags.optset");
        let lower_optset = root.join("flags.optset");
        let source = "unit Main;\ninterface\nconst Selected = 1;\nimplementation\nend.\n";
        let upper_contents =
            "<Project><PropertyGroup><DCC_Define>FEATURE</DCC_Define></PropertyGroup></Project>";
        let lower_contents =
            "<Project><PropertyGroup><DCC_Define>OTHERXX</DCC_Define></PropertyGroup></Project>";
        let project_contents = "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><Import Project=\"Flags.optset\"/><Import Project=\"flags.optset\"/></Project>";
        fs::create_dir_all(&root).expect("workspace directory");
        fs::write(&source_path, source).expect("source");
        fs::write(&project_path, project_contents).expect("project");
        fs::write(&upper_optset, upper_contents).expect("uppercase optset");
        fs::write(&lower_optset, lower_contents).expect("lowercase optset");

        let workspace = Workspace::new(vec![root], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let computed =
            document_symbols_from_input(input.clone(), &source_uri(&source_path), &cancel);
        assert!(
            computed.value.is_ok(),
            "document symbols must compute: {computed:?}"
        );

        let upper_record = computed
            .records
            .iter()
            .find(|record| record.path.as_deref() == Some(upper_optset.as_path()))
            .expect("uppercase optset read must be retained");
        let lower_record = computed
            .records
            .iter()
            .find(|record| record.path.as_deref() == Some(lower_optset.as_path()))
            .expect("lowercase optset read must be retained");
        assert_eq!(
            upper_record.content_bytes.as_deref(),
            Some(upper_contents.as_bytes())
        );
        assert_eq!(
            lower_record.content_bytes.as_deref(),
            Some(lower_contents.as_bytes())
        );
        revalidate_input(&input, &computed.records, &cancel)
            .expect("unchanged case-distinct optsets must remain valid");

        fs::write(&upper_optset, upper_contents.replace("FEATURE", "FEATURE2"))
            .expect("mutate uppercase optset");
        let error = revalidate_input(&input, std::slice::from_ref(upper_record), &cancel)
            .expect_err("uppercase optset mutation must stale its own record");
        assert!(
            error.contains("Flags.optset"),
            "uppercase optset mutation was attributed incorrectly: {error}"
        );
        revalidate_input(&input, std::slice::from_ref(lower_record), &cancel)
            .expect("uppercase optset mutation must not stale lowercase optset");

        fs::write(&lower_optset, lower_contents.replace("OTHERXX", "OTHERYY"))
            .expect("mutate lowercase optset");
        let error = revalidate_input(&input, std::slice::from_ref(lower_record), &cancel)
            .expect_err("lowercase optset mutation must stale its own record");
        assert!(
            error.contains("flags.optset"),
            "lowercase optset mutation was attributed incorrectly: {error}"
        );
    }

    #[test]
    fn document_symbols_reject_project_mutation_during_discovery_read() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let source_path = root.join("Main.pas");
        let project_path = root.join("App.dproj");
        let source = "unit Main;\ninterface\n{$IFDEF FEATURE}\nconst Selected = 1;\n{$ELSE}\nconst Other = 2;\n{$ENDIF}\nimplementation\nend.\n";
        let initial_project = "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Define>FEATURE</DCC_Define></PropertyGroup></Project>";
        let changed_project = "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Define>OTHERXX</DCC_Define></PropertyGroup></Project>";
        fs::create_dir_all(&root).expect("workspace directory");
        fs::write(&source_path, source).expect("source");
        fs::write(&project_path, initial_project).expect("project");

        let project_for_hook = project_path.clone();
        let changed_for_hook = changed_project.to_string();
        let _hook = pascal_project::test_after_project_read(move |path| {
            if path == project_for_hook {
                fs::write(&project_for_hook, &changed_for_hook)
                    .expect("mutate project after discovery read");
            }
        });
        let workspace = Workspace::new(vec![root], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let computed =
            document_symbols_from_input(input.clone(), &source_uri(&source_path), &cancel);
        assert!(
            computed.value.is_ok(),
            "document symbols must compute: {computed:?}"
        );
        let project_record = computed
            .records
            .iter()
            .find(|record| record.path.as_deref() == Some(project_path.as_path()))
            .expect("project read must be retained in the snapshot");
        assert_eq!(
            project_record.content_bytes.as_deref(),
            Some(initial_project.as_bytes())
        );

        let error = revalidate_input(&input, &computed.records, &cancel)
            .expect_err("a project mutation during discovery must stale document symbols");
        assert!(
            error.to_ascii_lowercase().contains("metadata")
                || error.to_ascii_lowercase().contains("changed"),
            "unexpected project read-boundary error: {error}"
        );
    }

    #[test]
    fn document_symbols_reject_candidate_mutation_during_discovery_read() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let source_path = root.join("Main.pas");
        let project_path = root.join("App.dproj");
        let new_project_path = root.join("Other.dproj");
        let source = "unit Main;\ninterface\nconst Selected = 1;\nimplementation\nend.\n";
        let project = "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Define>FEATURE</DCC_Define></PropertyGroup></Project>";
        fs::create_dir_all(&root).expect("workspace directory");
        fs::write(&source_path, source).expect("source");
        fs::write(&project_path, project).expect("project");

        let project_for_hook = project_path.clone();
        let new_project_for_hook = new_project_path.clone();
        let _hook = pascal_project::test_after_project_read(move |path| {
            if path == project_for_hook {
                fs::write(
                    &new_project_for_hook,
                    "<Project><PropertyGroup><MainSource>Other.pas</MainSource></PropertyGroup></Project>",
                )
                .expect("add project candidate after discovery read");
            }
        });
        let workspace = Workspace::new(vec![root], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let computed =
            document_symbols_from_input(input.clone(), &source_uri(&source_path), &cancel);
        assert!(
            computed.value.is_ok(),
            "document symbols must compute: {computed:?}"
        );

        let error = revalidate_input(&input, &computed.records, &cancel)
            .expect_err("a project candidate mutation during discovery must stale symbols");
        assert!(
            error.to_ascii_lowercase().contains("membership")
                || error.to_ascii_lowercase().contains("changed"),
            "unexpected candidate read-boundary error: {error}"
        );
    }

    #[test]
    fn document_symbols_revalidate_project_candidate_membership() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let source_path = root.join("src/Main.pas");
        let project_path = root.join("App.dproj");
        let source = "unit Main;\ninterface\nconst Selected = 1;\nimplementation\nend.\n";
        fs::create_dir_all(source_path.parent().expect("source directory"))
            .expect("source directory");
        fs::write(&source_path, source).expect("source");
        fs::write(
            &project_path,
            "<Project><PropertyGroup><MainSource>src/Main.pas</MainSource></PropertyGroup></Project>",
        )
        .expect("project");
        let workspace = Workspace::new(vec![root.clone()], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let computed =
            document_symbols_from_input(input.clone(), &source_uri(&source_path), &cancel);
        assert!(
            computed.value.is_ok(),
            "document symbols must compute: {computed:?}"
        );

        fs::write(
            root.join("Another.dproj"),
            "<Project><PropertyGroup><MainSource>src/Other.pas</MainSource></PropertyGroup></Project>",
        )
        .expect("new project candidate");
        let error = revalidate_input(&input, &computed.records, &cancel).expect_err(
            "adding a project candidate in the discovered directory must stale document symbols",
        );
        assert!(
            error.contains("membership") || error.contains("changed"),
            "unexpected candidate membership revalidation error: {error}"
        );
    }

    #[test]
    fn document_symbols_revalidate_a_consumed_project_source() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let consumer_path = root.join("src/Consumer.pas");
        let main_path = root.join("src/Main.pas");
        let project_path = root.join("App.dproj");
        let consumer_source =
            "unit Consumer;\ninterface\nconst Selected = 1;\nimplementation\nend.\n";
        fs::create_dir_all(consumer_path.parent().expect("source directory"))
            .expect("source directory");
        fs::write(&consumer_path, consumer_source).expect("consumer source");
        fs::write(&main_path, "program Main; begin end.\n").expect("main source");
        fs::write(
            &project_path,
            "<Project><PropertyGroup><MainSource>src/Main.pas</MainSource><DCC_Define>FEATURE</DCC_Define></PropertyGroup></Project>",
        )
        .expect("project");
        let workspace = Workspace::new(vec![root], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let computed =
            document_symbols_from_input(input.clone(), &source_uri(&consumer_path), &cancel);
        assert!(
            computed.value.is_ok(),
            "document symbols must compute: {computed:?}"
        );

        fs::write(&main_path, "program Main; const Changed = 1; begin end.\n")
            .expect("changed main source");
        let error = revalidate_input(&input, &computed.records, &cancel)
            .expect_err("a source consumed by project discovery must stale document symbols");
        assert!(
            error.contains("changed") || error.contains("metadata"),
            "unexpected consumed-source revalidation error: {error}"
        );
    }

    #[test]
    fn masked_hover_and_type_queries_revalidate_project_metadata() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let source_path = root.join("Main.pas");
        let project_path = root.join("App.dproj");
        let source = "unit Main;\ninterface\n{$IFDEF FEATURE}\nconst Selected = 1;\n{$ELSE}\nconst Other = 2;\n{$ENDIF}\nimplementation\nend.\n";
        fs::create_dir_all(&root).expect("workspace directory");
        fs::write(&source_path, source).expect("source");
        fs::write(
            &project_path,
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Define>FEATURE</DCC_Define></PropertyGroup></Project>",
        )
        .expect("project");
        let workspace = Workspace::new(vec![root], WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let source_uri = source_uri(&source_path);
        let position = Position::new(5, 6);
        let cancel = AtomicBool::new(false);
        let hover = hover_from_input(
            input.clone(),
            &source_uri,
            position,
            MarkupKind::PlainText,
            &cancel,
        );
        assert_eq!(hover.value, Ok(None));
        let type_definitions =
            type_definitions_from_input(input.clone(), &source_uri, position, &cancel);
        assert_eq!(type_definitions.value, Ok(Vec::new()));

        fs::write(
            &project_path,
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Define>OTHER</DCC_Define></PropertyGroup></Project>",
        )
        .expect("changed project");
        for records in [&hover.records, &type_definitions.records] {
            let error = revalidate_input(&input, records, &cancel)
                .expect_err("masked query metadata must be revalidated");
            assert!(
                error.to_ascii_lowercase().contains("configuration")
                    || error.to_ascii_lowercase().contains("metadata")
                    || error.to_ascii_lowercase().contains("changed"),
                "unexpected project metadata revalidation error: {error}"
            );
        }
    }

    #[test]
    fn references_honor_preset_cancellation() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(true);
        let computed = references_from_input(
            fixture.input,
            &source_uri(&fixture.provider),
            shared_value_position(),
            false,
            &cancel,
        );
        assert_eq!(computed.value, Err("request cancelled".to_string()));
    }

    #[test]
    fn highlights_honor_preset_cancellation() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(true);
        let computed = highlights_from_input(
            fixture.input,
            &source_uri(&fixture.provider),
            shared_value_position(),
            &cancel,
        );
        assert_eq!(computed.value, Err("request cancelled".to_string()));
    }

    #[test]
    fn hover_honors_preset_cancellation() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(true);
        let computed = hover_from_input(
            fixture.input,
            &source_uri(&fixture.consumer),
            Position::new(6, 6),
            MarkupKind::PlainText,
            &cancel,
        );
        assert_eq!(computed.value, Err("request cancelled".to_string()));
    }

    #[test]
    fn references_honor_cancellation_during_occurrence_collection() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(false);
        let _guard = crate::navigation::test_cancel_after_checks(6);
        let computed = references_from_input(
            fixture.input,
            &source_uri(&fixture.provider),
            shared_value_position(),
            false,
            &cancel,
        );
        assert_eq!(computed.value, Err("request cancelled".to_string()));
    }

    #[test]
    fn semantic_tokens_preserve_lexical_tokens_but_skip_resolution_when_an_import_is_missing() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let source_path = root.join("Main.pas");
        let source = "unit Main;\ninterface\nuses MissingUnit;\nimplementation\nprocedure Run;\nvar\n  LocalValue: Integer;\nbegin\n  LocalValue := 1;\n  MissingRoutine;\nend;\nend.\n";
        fs::write(&source_path, source).expect("source");
        let workspace = test_workspace(vec![root], WorkspaceOptions::default());
        let cancel = AtomicBool::new(false);

        let computed = semantic_tokens_from_input(
            workspace.analysis_input(),
            &source_uri(&source_path),
            None,
            &cancel,
        );

        let tokens = computed
            .value
            .expect("missing imports must not fail lexical token generation");
        assert!(
            !tokens.data.iter().any(|token| token.token_type == 8),
            "an incomplete import snapshot must not claim semantic variable bindings"
        );
        assert!(
            tokens.data.iter().any(|token| token.token_type == 17),
            "the lexical number token must be retained"
        );
        assert!(
            !computed.records.is_empty(),
            "the snapshot read set is required"
        );
    }

    #[test]
    fn semantic_tokens_skip_resolution_when_include_audit_is_uncertain() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let provider_path = root.join("Provider.pas");
        let include_path = root.join("IncludeLocal.inc");
        let source_path = root.join("Main.pas");
        fs::write(
            &provider_path,
            "unit Provider;\ninterface\nconst Value = 1;\nimplementation\nend.\n",
        )
        .expect("provider source");
        fs::write(&include_path, "  Value: Integer;\n").expect("include source");
        let source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nvar\n{$I IncludeLocal.inc}\nbegin\n  Value := 1;\nend;\nend.\n";
        fs::write(&source_path, source).expect("main source");
        let workspace = test_workspace(vec![root], WorkspaceOptions::default());
        let cancel = AtomicBool::new(false);

        let computed = semantic_tokens_from_input(
            workspace.analysis_input(),
            &source_uri(&source_path),
            None,
            &cancel,
        );

        let tokens = computed
            .value
            .expect("uncertain includes must not fail lexical token generation");
        assert!(
            !tokens.data.iter().any(|token| {
                token.token_type == 8 && token.token_modifiers_bitset & (1 << 2) != 0
            }),
            "an include audit error must prevent readonly classification from a partial import"
        );
        assert!(
            tokens.data.iter().any(|token| token.token_type == 17),
            "lexical number tokens must survive an uncertain include audit"
        );
    }

    #[test]
    fn initial_context_observations_honor_cancellation() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(false);
        let _guard = pascal_project::test_cancel_project_scan_after_checks(0);
        let computed = references_from_input(
            fixture.input,
            &source_uri(&fixture.provider),
            shared_value_position(),
            false,
            &cancel,
        );
        assert_eq!(computed.value, Err(super::CANCELLATION_MESSAGE.to_string()));
    }

    #[test]
    fn binding_preflight_honors_cancellation() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(false);
        let _guard = pascal_project::test_cancel_project_scan_after_checks(0);
        let result = binding_info_for_input(
            &fixture.input,
            &source_uri(&fixture.provider),
            shared_value_position(),
            &[],
            &cancel,
        );
        assert_eq!(result.err().as_deref(), Some(super::CANCELLATION_MESSAGE));
    }

    #[test]
    fn highlights_honor_cancellation_during_occurrence_collection() {
        let fixture = query_fixture();
        let cancel = AtomicBool::new(false);
        let _guard = crate::navigation::test_cancel_in_phase(
            crate::navigation::TestCancellationPhase::OccurrenceCollection,
        );
        let computed = highlights_from_input(
            fixture.input,
            &source_uri(&fixture.provider),
            shared_value_position(),
            &cancel,
        );
        assert_eq!(computed.value, Err("request cancelled".to_string()));
    }
}
