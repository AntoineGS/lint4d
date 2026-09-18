use super::KnownDocumentOwner;
use super::rename::{
    BindingClassification, CANCELLATION_MESSAGE, RenameSnapshot, SnapshotMode, SnapshotSeed,
    WorkspaceInput, auto_import_source_is_relevant, build_snapshot,
    input_source_is_readable_with_owner, is_cancelled, owner_for_input,
    project_context_and_metadata_for_input, project_context_and_metadata_for_owner,
    query_binding_info_for_input, reference_binding_info_for_input, revalidate_input,
    snapshot_records, source_for_input_with_cancel, source_for_input_with_owner,
};
use crate::navigation::{
    CompletionMetadata, CompletionOptions, CompletionResult, FoldingRangeOptions,
    SemanticTokenResolutionMode, completion_prefix_at_position,
};
use crate::{NavigationIndex, NavigationTarget};
#[cfg(test)]
use lsp_types::CompletionList;
use lsp_types::{
    DocumentHighlight, DocumentSymbol, FoldingRange, Hover, Location, MarkupKind, Position, Range,
    SelectionRange, SemanticTokens, SignatureHelp, SymbolInformation, Url,
};
use pascal_project::has_invalid_project_selection;
use std::sync::atomic::AtomicBool;

pub(crate) struct NavigationResult {
    pub(crate) locations: Vec<Location>,
    pub(crate) state: super::NavigationState,
}

pub(crate) struct DiagnosticsResult {
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
        .hover_with_cancel(&uri, position, format, cancel);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    with_records(source_generation, configuration_generation, value, records)
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
        .completion_with_cancel_and_options(&uri, position, options, cancel);
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
    let value = snapshot.index.semantic_tokens_with_resolution_mode(
        &uri,
        range.as_ref(),
        cancel,
        resolution_mode,
    );
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
    if let Err(error) =
        index.update_with_defines_with_cancel(uri.clone(), source, &context.defines, cancel)
    {
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
    if has_invalid_project_selection(&owner.state.context) {
        return failed(
            source_generation,
            configuration_generation,
            format!(
                "project selection is invalid; select a current project or Automatic for {uri}"
            ),
        );
    }
    if owner.state.context.override_error.is_some() {
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

    let snapshot = match binding_snapshot(&input, &uri, position, false, classification, cancel) {
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
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }
    let value =
        snapshot
            .index
            .binding_locations_with_cancel(&uri, position, include_declaration, cancel);
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
    if has_invalid_project_selection(&owner.state.context) {
        return failed(
            source_generation,
            configuration_generation,
            format!(
                "project selection is invalid; select a current project or Automatic for {uri}"
            ),
        );
    }
    if owner.state.context.override_error.is_some() {
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
    let locations = match snapshot
        .index
        .binding_locations_in_document_with_cancel(&uri, position, cancel)
    {
        Ok(locations) => locations,
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
    let value = locations
        .into_iter()
        .map(|location| DocumentHighlight {
            range: location.range,
            kind: None,
        })
        .collect();
    with_records(
        source_generation,
        configuration_generation,
        Ok(value),
        records,
    )
}

fn binding_snapshot(
    input: &WorkspaceInput,
    uri: &Url,
    position: Position,
    document_local: bool,
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
    build_binding_snapshot(
        input,
        &uri,
        position,
        target_record,
        binding_info,
        consumed_configuration,
        mode,
        skip_imports_for,
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
                .with_completion_position(completion_position),
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
    let original_name = super::rename::identifier_at_position(&target_record.text, position)
        .ok_or_else(|| format!("no identifier at navigation position in {uri}"))?;
    let mut candidate_names = match binding_info {
        Some(info) => {
            let mut names = info.names;
            if names.is_empty() {
                names.push(original_name.clone());
            }
            names
        }
        None => vec![original_name],
    };
    candidate_names.sort();
    candidate_names.dedup();
    build_snapshot(
        input,
        std::slice::from_ref(uri),
        &candidate_names,
        mode,
        Some(SnapshotSeed::new(target_record).with_consumed_configuration(&consumed_configuration)),
        skip_imports_for,
        cancel,
    )
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
        return Err(format!("reference workspace scan incomplete: {reason}"));
    }
    Ok(())
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
        if let Some(error) = snapshot.include_errors.first() {
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
    if let Some(error) = snapshot.include_errors.first() {
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
    if !snapshot.complete || !snapshot.include_errors.is_empty() {
        return Ok(SemanticTokenResolutionMode::LexicalOnly);
    }
    Ok(SemanticTokenResolutionMode::Full)
}

struct SyntaxDocument {
    source: String,
    context: pascal_project::ProjectContext,
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
    let defines = document.context.defines;
    let mut index = NavigationIndex::new();
    index
        .update_with_defines_and_cached_with_cancel(
            uri.clone(),
            document.source,
            &defines,
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
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    let mut workspace = super::Workspace::from_analysis_input(&input);
    let value = match workspace.navigate_with_cancel(&uri, position, target, cancel) {
        Ok(locations) => Ok(locations),
        Err(error) if error == CANCELLATION_MESSAGE => {
            return cancelled(source_generation, configuration_generation);
        }
        Err(_error) => Ok(Vec::new()),
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
    cancel: &AtomicBool,
) -> super::rename::Computed<Option<lsp_types::TextEdit>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let uri = super::canonical_file_uri(uri);
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    let mut workspace = super::Workspace::from_analysis_input(&input);
    let value = match workspace.formatting_edit_with_cancel(&uri, cancel) {
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
            diagnostics,
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
    with_records(source_generation, configuration_generation, value, records)
}

pub(crate) fn workspace_symbols_from_input(
    input: WorkspaceInput,
    query: &str,
    cancel: &AtomicBool,
) -> super::rename::Computed<Vec<SymbolInformation>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

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
    if !snapshot.complete {
        let reason = snapshot
            .incomplete_reason
            .as_deref()
            .unwrap_or("bounded source discovery did not finish");
        return failed(
            source_generation,
            configuration_generation,
            format!("workspace symbol search incomplete: {reason}"),
        );
    }

    let value = snapshot
        .index
        .workspace_symbols_with_cancel(query, cancel)
        .map(|symbols| {
            symbols
                .into_iter()
                .filter(|symbol| snapshot.readable.contains(&symbol.location.uri))
                .collect()
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
        && left.read_policy == right.read_policy
        && left.path_entry == right.path_entry
        && left.include_payload == right.include_payload
        && left.missing_provider_candidate == right.missing_provider_candidate
        && left.missing_provider_scope == right.missing_provider_scope
        && left.auto_import_provider_observation == right.auto_import_provider_observation
        && left.auto_import_scopes == right.auto_import_scopes
        && match (&left.content_bytes, &right.content_bytes) {
            (Some(left), Some(right)) => left == right,
            _ => true,
        }
}

#[cfg(test)]
mod tests {
    use super::{
        completion_from_input, completion_from_input_with_options, completion_metadata_from_input,
        document_symbols_from_input, highlights_from_input, hover_from_input,
        references_from_input, selection_ranges_from_input, semantic_tokens_from_input,
        signature_help_from_input, type_definitions_from_input,
    };
    use crate::navigation::CompletionOptions;
    use crate::workspace::rename::{
        CANCELLATION_MESSAGE, Computed, WorkspaceInput, binding_info_for_input,
        install_snapshot_priority_barrier, owner_for_input, project_context_and_metadata_for_input,
        revalidate_input,
    };
    use crate::workspace::{Workspace, WorkspaceOptions, content_hash_bytes};
    use lsp_types::{MarkupKind, Position, Url};
    use pascal_project::delphi_overrides::{EffectiveOverrides, OverrideSession};
    use pascal_project::{
        MetadataObservation, ProjectOptions, ProjectPathEntry, ProjectPathProvenance, ReadPolicy,
    };
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

    fn compute_references(fixture: &ReferenceFixture) -> Computed<Vec<lsp_types::Location>> {
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
        assert_eq!(context.project_file, Some(project));
        assert_eq!(context.defines, vec!["CAPTURED"]);
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
        let locations = computed.value.expect("mapped references should complete");
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
        changed_context
            .cached_documents
            .get_mut(&uri)
            .expect("cached document")
            .context
            .defines
            .push("CONTEXT_CHANGED".to_owned());
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
            .text = empty_source.clone();
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
            .replace("OverlayMember", "ChangedMember");
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
        assert!(error.contains("configuration content changed"), "{error}");
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
            .text = empty_source.clone();
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
            completion_error.contains("configuration content changed"),
            "{completion_error}"
        );
        let signature_error = revalidate_input(&input, &signature.records, &cancel)
            .expect_err("null signature must retain consumed project metadata");
        assert!(
            signature_error.contains("configuration content changed"),
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
        assert!(error.contains("configuration content changed"), "{error}");

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
