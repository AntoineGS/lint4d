use super::{
    MAX_COMPLETION_RESOLUTION_CONTEXT_BYTES, MAX_COMPLETION_RESOLUTION_RECORDS,
    MAX_DIAGNOSTIC_DEPENDENCY_RECORDS, MAX_DIAGNOSTIC_VALIDATION_BYTES,
};
use crate::workspace::rename::SourceRecord;
use std::mem::size_of;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceRecordCompactionError {
    MissingBoundedContentObservation,
}

fn compact_source_record(
    record: &SourceRecord,
) -> Result<SourceRecord, SourceRecordCompactionError> {
    let content_hash = record.content_hash.or_else(|| {
        if record.open {
            // The compact observation below intentionally drops the full
            // overlay text. Revalidation therefore interprets this as a
            // raw-byte hash, while parsed_text_hash retains the decoded LSP
            // text identity for parser freshness.
            Some(crate::workspace::content_hash_bytes(record.text.as_bytes()))
        } else {
            record
                .content_bytes
                .as_deref()
                .map(crate::workspace::content_hash_bytes)
                .or_else(|| {
                    (!record.text.is_empty())
                        .then(|| crate::workspace::content_hash_bytes(record.text.as_bytes()))
                })
        }
    });
    if !record.open && record.path.is_none() && content_hash.is_none() {
        return Err(SourceRecordCompactionError::MissingBoundedContentObservation);
    }

    Ok(SourceRecord {
        uri: record.uri.clone(),
        text: String::new(),
        version: record.version,
        stamp: record.stamp.clone(),
        open: record.open,
        path: record.path.clone(),
        path_stamp: record.path_stamp.clone(),
        content_hash,
        parsed_text_hash: record.parsed_text_hash.or_else(|| {
            (!record.text.is_empty())
                .then(|| crate::workspace::rename::text_content_hash(&record.text))
        }),
        content_bytes: None,
        candidate_membership: record.candidate_membership.clone(),
        candidate_observations: record.candidate_observations.clone(),
        read_policy: record.read_policy.clone(),
        path_entry: record.path_entry.clone(),
        include_payload: record.include_payload,
        missing_provider_candidate: record.missing_provider_candidate,
        document_link_missing_candidate: record.document_link_missing_candidate,
        document_link_case_variant: record.document_link_case_variant.clone(),
        directory_observation: record.directory_observation,
        document_link_ancestor: record.document_link_ancestor,
        missing_provider_scope: record.missing_provider_scope.clone(),
        auto_import_provider_observation: record.auto_import_provider_observation,
        auto_import_scopes: record.auto_import_scopes.clone(),
    })
}

pub(super) fn compact_completion_records(
    records: &[SourceRecord],
) -> Result<(Vec<SourceRecord>, usize), String> {
    let mut compact = Vec::with_capacity(records.len());
    let mut bytes = 0usize;
    for record in records {
        let observation = compact_source_record(record).map_err(
            |SourceRecordCompactionError::MissingBoundedContentObservation| {
                format!(
                    "completion dependency {} has no bounded content observation",
                    record.uri
                )
            },
        )?;
        let observation_bytes = compact_completion_record_bytes(&observation);
        bytes = bytes.saturating_add(observation_bytes);
        compact.push(observation);
        if compact.len() > MAX_COMPLETION_RESOLUTION_RECORDS
            || bytes > MAX_COMPLETION_RESOLUTION_CONTEXT_BYTES
        {
            return Err("completion dependency observations exceed the bounded limit".to_string());
        }
    }
    Ok((compact, bytes))
}

fn path_storage_bytes(path: &PathBuf) -> usize {
    size_of::<PathBuf>().saturating_add(path.capacity())
}

fn path_entry_storage_bytes(entry: &pascal_project::ProjectPathEntry) -> usize {
    size_of::<pascal_project::ProjectPathEntry>()
        .saturating_add(path_storage_bytes(&entry.path))
        .saturating_add(match &entry.provenance {
            pascal_project::ProjectPathProvenance::Mapped { root } => path_storage_bytes(root),
            pascal_project::ProjectPathProvenance::LegacyNative
            | pascal_project::ProjectPathProvenance::Configured => 0,
        })
}

fn candidate_membership_storage_bytes(
    membership: &pascal_project::ProjectCandidateMembership,
) -> usize {
    size_of::<pascal_project::ProjectCandidateMembership>().saturating_add(
        membership
            .paths
            .iter()
            .map(path_storage_bytes)
            .sum::<usize>(),
    )
}

fn candidate_observations_storage_bytes(
    observations: &[crate::workspace::rename::ResolverCandidateObservation],
) -> usize {
    observations
        .iter()
        .map(|observation| {
            size_of::<crate::workspace::rename::ResolverCandidateObservation>()
                .saturating_add(path_storage_bytes(&observation.path))
        })
        .sum()
}

fn missing_provider_scope_storage_bytes(
    scope: &crate::workspace::rename::MissingProviderScope,
) -> usize {
    size_of::<crate::workspace::rename::MissingProviderScope>()
        .saturating_add(path_storage_bytes(&scope.root))
        .saturating_add(
            scope
                .names
                .iter()
                .map(|name| size_of::<String>().saturating_add(name.capacity()))
                .sum::<usize>(),
        )
        .saturating_add(scope.read_policy.retained_size_hint())
        .saturating_add(path_entry_storage_bytes(&scope.path_entry))
}

fn auto_import_scope_storage_bytes(
    scope: &crate::workspace::rename::AutoImportProviderScope,
) -> usize {
    size_of::<crate::workspace::rename::AutoImportProviderScope>()
        .saturating_add(path_storage_bytes(&scope.root))
        .saturating_add(
            scope
                .provider_units
                .iter()
                .chain(scope.candidate_prefixes.iter())
                .map(|value| size_of::<String>().saturating_add(value.capacity()))
                .sum::<usize>(),
        )
        .saturating_add(scope.read_policy.retained_size_hint())
        .saturating_add(path_entry_storage_bytes(&scope.path_entry))
}

pub(super) fn compact_completion_record_bytes(record: &SourceRecord) -> usize {
    size_of::<SourceRecord>()
        .saturating_add(record.uri.as_str().len())
        .saturating_add(
            record
                .text
                .capacity()
                .saturating_mul(std::mem::size_of::<u8>()),
        )
        .saturating_add(record.path.as_ref().map_or(0, path_storage_bytes))
        .saturating_add(
            record
                .content_bytes
                .as_ref()
                .map_or(0, |bytes| bytes.capacity()),
        )
        .saturating_add(
            record
                .candidate_membership
                .as_ref()
                .map_or(0, candidate_membership_storage_bytes),
        )
        .saturating_add(candidate_observations_storage_bytes(
            &record.candidate_observations,
        ))
        .saturating_add(
            record
                .read_policy
                .as_ref()
                .map_or(0, pascal_project::ReadPolicy::retained_size_hint),
        )
        .saturating_add(
            record
                .path_entry
                .as_ref()
                .map_or(0, path_entry_storage_bytes),
        )
        .saturating_add(
            record
                .missing_provider_scope
                .as_ref()
                .map_or(0, missing_provider_scope_storage_bytes),
        )
        .saturating_add(
            record
                .auto_import_scopes
                .iter()
                .map(auto_import_scope_storage_bytes)
                .sum::<usize>(),
        )
}

pub(super) fn compact_diagnostic_records(
    records: &[SourceRecord],
) -> Result<Vec<SourceRecord>, String> {
    if records.len() > MAX_DIAGNOSTIC_DEPENDENCY_RECORDS {
        return Err(format!(
            "diagnostic validation evidence exceeds the {MAX_DIAGNOSTIC_DEPENDENCY_RECORDS}-record limit"
        ));
    }

    // Preflight the bounded representation before allocating its vector.  The
    // source payloads are deliberately excluded: freshness is proved from the
    // content hashes below, while authorization and resolver observations are
    // retained for dependency validation.
    let mut estimated_bytes = 0usize;
    for record in records {
        let full = compact_completion_record_bytes(record);
        let source_payload = record
            .text
            .capacity()
            .saturating_add(record.content_bytes.as_ref().map_or(0, Vec::capacity));
        estimated_bytes = estimated_bytes.saturating_add(full.saturating_sub(source_payload));
        if estimated_bytes > MAX_DIAGNOSTIC_VALIDATION_BYTES {
            return Err(format!(
                "diagnostic validation evidence exceeds the {MAX_DIAGNOSTIC_VALIDATION_BYTES}-byte limit"
            ));
        }
    }

    let mut compact = Vec::with_capacity(records.len());
    for record in records {
        let compact_record = compact_source_record(record).map_err(
            |SourceRecordCompactionError::MissingBoundedContentObservation| {
                format!(
                    "diagnostic dependency {} has no bounded content observation",
                    record.uri
                )
            },
        )?;
        compact.push(compact_record);
    }
    Ok(compact)
}

pub(super) fn source_records_retained_bytes(records: &[SourceRecord]) -> usize {
    let mut bytes = size_of::<Vec<SourceRecord>>()
        .saturating_add(records.len().saturating_mul(size_of::<SourceRecord>()));
    for record in records {
        bytes = bytes
            .saturating_add(size_of::<SourceRecord>())
            .saturating_add(record.uri.as_str().len())
            .saturating_add(record.text.capacity())
            .saturating_add(record.path.as_ref().map_or(0, path_storage_bytes))
            .saturating_add(record.content_bytes.as_ref().map_or(0, Vec::capacity))
            .saturating_add(
                record
                    .candidate_membership
                    .as_ref()
                    .map_or(0, candidate_membership_storage_bytes),
            )
            .saturating_add(candidate_observations_storage_bytes(
                &record.candidate_observations,
            ))
            .saturating_add(
                record
                    .read_policy
                    .as_ref()
                    .map_or(0, pascal_project::ReadPolicy::retained_size_hint),
            )
            .saturating_add(
                record
                    .path_entry
                    .as_ref()
                    .map_or(0, path_entry_storage_bytes),
            )
            .saturating_add(
                record
                    .missing_provider_scope
                    .as_ref()
                    .map_or(0, missing_provider_scope_storage_bytes),
            )
            .saturating_add(
                record
                    .auto_import_scopes
                    .iter()
                    .map(auto_import_scope_storage_bytes)
                    .sum::<usize>(),
            );
    }
    bytes
}
